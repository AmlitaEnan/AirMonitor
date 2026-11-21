use std::cell::UnsafeCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, SetupDiGetDevicePropertyW, DIGCF_DEVICEINTERFACE,
    DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    SP_DEVINFO_DATA,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetManufacturerString, HidD_GetPreparsedData,
    HidD_GetProductString, HidD_GetSerialNumberString, HidP_GetCaps, GUID_DEVINTERFACE_HID,
    HIDD_ATTRIBUTES, HIDP_CAPS,
};
use windows::Win32::Devices::Properties::{DEVPKEY_Device_ContainerId, DEVPROP_TYPE_GUID};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};

use super::protocols::{
    format_hex, hidpp, probe_protocol, rapoo, BatteryReading, DeviceRegistry, HidInterfaceInfo,
    HidIo, ProtocolFamily, ReadOnlyHidIo,
};
use crate::state::{DeviceKind, Transport, WirelessDevice};

const HID_CACHE_GRACE_SECS: u64 = 1800;
const HID_OFFLINE_FAIL_THRESHOLD: u32 = 3;
const HID_POLL_BUDGET: Duration = Duration::from_millis(4500);
const HID_IOCTL_TIMEOUT_MS: u32 = 300;
const HID_WAIT_SLICE_MS: u32 = 50;
const HID_CANCEL_WAIT_MS: u32 = 80;
const HID_RECLAIM_QUEUE_CAP: usize = 64;
const HID_QUARANTINE_SECS: u64 = 120;
const HID_MAX_BACKOFF_SECS: u64 = 600;
const HID_DEFAULT_POLL_BASE_SECS: u64 = 45;
const HID_PASSIVE_MAX_BACKOFF_SECS: u64 = 45;
const HID_PASSIVE_INITIAL_RETRY_SECS: u64 = 4;
const HID_PASSIVE_INITIAL_FAST_RETRIES: u32 = 2;
const HID_PASSIVE_REOPEN_FAIL_INTERVAL: u32 = 20;

// Win32 WDK CTL_CODE definitions for hidclass.sys (hidclass.h):
// CTL_CODE(FILE_DEVICE_KEYBOARD=0x0B, Function, Method, FILE_ANY_ACCESS=0)
const FILE_DEVICE_KEYBOARD: u32 = 0x0000_000B;
const METHOD_IN_DIRECT: u32 = 1;
const METHOD_OUT_DIRECT: u32 = 2;

const fn hid_ctl_code(function: u32, method: u32) -> u32 {
    (FILE_DEVICE_KEYBOARD << 16) | (function << 2) | method
}

const IOCTL_HID_SET_FEATURE: u32 = hid_ctl_code(100, METHOD_IN_DIRECT);
const IOCTL_HID_GET_FEATURE: u32 = hid_ctl_code(100, METHOD_OUT_DIRECT);
const IOCTL_HID_GET_INPUT_REPORT: u32 = hid_ctl_code(104, METHOD_OUT_DIRECT);

const _: () = assert!(IOCTL_HID_SET_FEATURE == 0x000B_0191);
const _: () = assert!(IOCTL_HID_GET_FEATURE == 0x000B_0192);
const _: () = assert!(IOCTL_HID_GET_INPUT_REPORT == 0x000B_01A2);

struct DevInfoGuard(HDEVINFO);

impl Drop for DevInfoGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = SetupDiDestroyDeviceInfoList(self.0);
            }
        }
    }
}

struct HandleGuard(HANDLE);

unsafe impl Send for HandleGuard {}
unsafe impl Sync for HandleGuard {}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0 .0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

trait ReclaimableIo: Send {
    fn is_completed(&mut self) -> bool;
}

struct Win32PendingIo {
    handle: Arc<HandleGuard>,
    event: HandleGuard,
    overlapped: Box<UnsafeCell<OVERLAPPED>>,
    buffer: Box<[UnsafeCell<u8>]>,
    completed: bool,
}

unsafe impl Send for Win32PendingIo {}

impl Win32PendingIo {
    fn new(handle: Arc<HandleGuard>, event: HandleGuard, buf_len: usize) -> Self {
        let overlapped = Box::new(UnsafeCell::new(OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        }));
        let buffer: Box<[UnsafeCell<u8>]> = (0..buf_len)
            .map(|_| UnsafeCell::new(0u8))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            handle,
            event,
            overlapped,
            buffer,
            completed: false,
        }
    }

    fn overlapped_ptr(&self) -> *mut OVERLAPPED {
        self.overlapped.get()
    }

    fn buffer_ptr(&self) -> *mut u8 {
        self.buffer.as_ptr() as *mut u8
    }

    fn buffer_len(&self) -> usize {
        self.buffer.len()
    }

    /// Safety: must only be called before the kernel owns the buffer (prior to I/O submission)
    /// or after `self.completed == true`.
    unsafe fn buffer_mut_slice(&mut self) -> &mut [u8] {
        std::slice::from_raw_parts_mut(self.buffer_ptr(), self.buffer.len())
    }

    /// Safety: must only be called after `self.completed == true`.
    unsafe fn copy_out(&self, len: usize) -> Vec<u8> {
        let take = len.min(self.buffer.len());
        let slice = std::slice::from_raw_parts(self.buffer.as_ptr() as *const u8, take);
        slice.to_vec()
    }

    fn poll_completed(&mut self) -> bool {
        if self.completed {
            return true;
        }
        unsafe {
            let mut transferred = 0u32;
            match GetOverlappedResult(
                self.handle.0,
                self.overlapped.get(),
                &mut transferred,
                false,
            ) {
                Ok(()) => {
                    self.completed = true;
                    true
                }
                Err(e) if e.code() == ERROR_IO_INCOMPLETE.into() => false,
                Err(_) => {
                    self.completed = true;
                    true
                }
            }
        }
    }
}

impl ReclaimableIo for Win32PendingIo {
    fn is_completed(&mut self) -> bool {
        self.poll_completed()
    }
}

impl Drop for Win32PendingIo {
    fn drop(&mut self) {
        if !self.completed && !self.poll_completed() {
            // Kernel IRP still holds raw pointers to OVERLAPPED, buffer, and event;
            // retain those allocations rather than triggering a UAF on late driver completion.
            let ov = std::mem::replace(
                &mut self.overlapped,
                Box::new(UnsafeCell::new(OVERLAPPED::default())),
            );
            let buf = std::mem::replace(&mut self.buffer, Vec::new().into_boxed_slice());
            let ev = std::mem::replace(&mut self.event, HandleGuard(HANDLE::default()));
            std::mem::forget(ov);
            std::mem::forget(buf);
            std::mem::forget(ev);
        }
    }
}

#[derive(Default)]
struct ReclaimerInner {
    pending: VecDeque<Box<dyn ReclaimableIo>>,
    max_pending: usize,
    quarantined_until: HashMap<String, Instant>,
    reaper_running: bool,
    evicted_uncompleted: u64,
    reclaimed_total: u64,
}

struct IoReclaimer {
    inner: Mutex<ReclaimerInner>,
    wake_cvar: Condvar,
    shutdown: AtomicBool,
}

impl IoReclaimer {
    fn new(max_pending: usize) -> Self {
        Self {
            inner: Mutex::new(ReclaimerInner {
                pending: VecDeque::new(),
                max_pending: max_pending.max(1),
                quarantined_until: HashMap::new(),
                reaper_running: false,
                evicted_uncompleted: 0,
                reclaimed_total: 0,
            }),
            wake_cvar: Condvar::new(),
            shutdown: AtomicBool::new(false),
        }
    }

    fn reap_locked(inner: &mut ReclaimerInner) -> usize {
        let before = inner.pending.len();
        let mut idx = 0usize;
        while idx < inner.pending.len() {
            let done = inner
                .pending
                .get_mut(idx)
                .is_some_and(|item| item.is_completed());
            if done {
                let _ = inner.pending.remove(idx);
                inner.reclaimed_total = inner.reclaimed_total.saturating_add(1);
            } else {
                idx += 1;
            }
        }
        before.saturating_sub(inner.pending.len())
    }

    fn reap_completed(&self) -> usize {
        let Ok(mut inner) = self.inner.lock() else {
            return 0;
        };
        Self::reap_locked(&mut inner)
    }

    fn is_saturated(&self) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        Self::reap_locked(&mut inner);
        inner.pending.len() >= inner.max_pending
    }

    fn is_quarantined(&self, norm_path: &str, now: Instant) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        inner.quarantined_until.retain(|_, until| *until > now);
        inner
            .quarantined_until
            .get(norm_path)
            .is_some_and(|until| *until > now)
    }

    fn quarantine_path(&self, norm_path: &str, now: Instant, duration: Duration) {
        if norm_path.is_empty() {
            return;
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner
                .quarantined_until
                .insert(norm_path.to_string(), now + duration);
        }
    }

    fn handoff(self: &Arc<Self>, item: Box<dyn ReclaimableIo>, spawn_reaper: bool) {
        let should_spawn = {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            Self::reap_locked(&mut inner);
            while inner.pending.len() >= inner.max_pending {
                let _ = inner.pending.pop_front();
                inner.evicted_uncompleted = inner.evicted_uncompleted.saturating_add(1);
            }
            inner.pending.push_back(item);
            if spawn_reaper && !inner.reaper_running && !self.shutdown.load(Ordering::Acquire) {
                inner.reaper_running = true;
                true
            } else {
                false
            }
        };

        if should_spawn {
            let reclaimer = Arc::clone(self);
            if std::thread::Builder::new()
                .name("hid-io-reclaim".to_string())
                .spawn(move || {
                    while !reclaimer.shutdown.load(Ordering::Acquire) {
                        let Ok(mut inner) = reclaimer.inner.lock() else {
                            break;
                        };
                        Self::reap_locked(&mut inner);
                        if inner.pending.is_empty() {
                            inner.reaper_running = false;
                            return;
                        }
                        let (guard, _) = reclaimer
                            .wake_cvar
                            .wait_timeout(inner, Duration::from_millis(40))
                            .unwrap_or_else(|e| e.into_inner());
                        drop(guard);
                    }
                    if let Ok(mut inner) = reclaimer.inner.lock() {
                        Self::reap_locked(&mut inner);
                        inner.reaper_running = false;
                    }
                })
                .is_err()
            {
                if let Ok(mut inner) = self.inner.lock() {
                    inner.reaper_running = false;
                }
            }
        }
    }

    fn shutdown_and_reap(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.wake_cvar.notify_all();
        let _ = self.reap_completed();
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.inner.lock().map(|g| g.pending.len()).unwrap_or(0)
    }

    #[cfg(test)]
    fn stats(&self) -> (usize, u64, u64) {
        self.inner
            .lock()
            .map(|g| (g.pending.len(), g.reclaimed_total, g.evicted_uncompleted))
            .unwrap_or((0, 0, 0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitSliceResult {
    Signaled,
    Timeout,
    Failed,
}

#[derive(Debug)]
enum OverlappedDriveError {
    CompletedErr(String),
    DeferredPending(String),
}

trait OverlappedOps {
    fn wait_slice(&mut self, timeout_ms: u32) -> WaitSliceResult;
    fn cancel_io(&mut self);
    fn poll_completed(&mut self) -> bool;
    fn read_result(&mut self) -> Result<u32, String>;
}

fn drive_overlapped_wait(
    stop_flag: &AtomicBool,
    timeout_ms: u32,
    cancel_wait_ms: u32,
    op_name: &str,
    ops: &mut impl OverlappedOps,
) -> Result<u32, OverlappedDriveError> {
    let mut remaining = timeout_ms;
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            ops.cancel_io();
            let _ = ops.wait_slice(cancel_wait_ms);
            if ops.poll_completed() {
                return Err(OverlappedDriveError::CompletedErr(format!(
                    "{} cancelled",
                    op_name
                )));
            }
            return Err(OverlappedDriveError::DeferredPending(format!(
                "{} cancelled (deferred reclaim)",
                op_name
            )));
        }

        let slice = remaining.min(HID_WAIT_SLICE_MS);
        match ops.wait_slice(slice) {
            WaitSliceResult::Signaled => {
                let _ = ops.poll_completed();
                return ops
                    .read_result()
                    .map_err(OverlappedDriveError::CompletedErr);
            }
            WaitSliceResult::Failed => {
                ops.cancel_io();
                let _ = ops.wait_slice(cancel_wait_ms);
                if ops.poll_completed() {
                    return Err(OverlappedDriveError::CompletedErr(format!(
                        "{} wait failed",
                        op_name
                    )));
                }
                return Err(OverlappedDriveError::DeferredPending(format!(
                    "{} wait failed (deferred reclaim)",
                    op_name
                )));
            }
            WaitSliceResult::Timeout => {
                if remaining <= slice {
                    ops.cancel_io();
                    let _ = ops.wait_slice(cancel_wait_ms);
                    if ops.poll_completed() {
                        return Err(OverlappedDriveError::CompletedErr(format!(
                            "{} timeout",
                            op_name
                        )));
                    }
                    return Err(OverlappedDriveError::DeferredPending(format!(
                        "{} timeout (deferred reclaim)",
                        op_name
                    )));
                }
                remaining -= slice;
            }
        }
    }
}

struct OpenHidDevice {
    handle: Arc<HandleGuard>,
    norm_path: String,
    info: HidInterfaceInfo,
    can_rw: bool,
    stop_flag: Arc<AtomicBool>,
    reclaimer: Arc<IoReclaimer>,
}

// Exclusive ownership via &mut HidMonitor; Win32 overlapped HANDLE may move across threads.
unsafe impl Send for OpenHidDevice {}

impl OpenHidDevice {
    fn open_handle(
        path: &str,
        read_write: bool,
        stop_flag: &AtomicBool,
    ) -> Option<(Arc<HandleGuard>, bool)> {
        if stop_flag.load(Ordering::Relaxed) {
            return None;
        }
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let (handle, can_rw) = unsafe {
            if read_write {
                match CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                ) {
                    Ok(h) if !h.is_invalid() => (h, true),
                    _ => match CreateFileW(
                        PCWSTR(wide.as_ptr()),
                        GENERIC_READ.0,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAG_OVERLAPPED,
                        None,
                    ) {
                        Ok(h) if !h.is_invalid() => (h, true),
                        _ => match CreateFileW(
                            PCWSTR(wide.as_ptr()),
                            0,
                            FILE_SHARE_READ | FILE_SHARE_WRITE,
                            None,
                            OPEN_EXISTING,
                            FILE_FLAG_OVERLAPPED,
                            None,
                        ) {
                            Ok(h) if !h.is_invalid() => (h, false),
                            _ => return None,
                        },
                    },
                }
            } else {
                match CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                ) {
                    Ok(h) if !h.is_invalid() => (h, false),
                    _ => return None,
                }
            }
        };
        Some((Arc::new(HandleGuard(handle)), can_rw))
    }

    fn open(
        path: &str,
        read_write: bool,
        stop_flag: Arc<AtomicBool>,
        reclaimer: Arc<IoReclaimer>,
    ) -> Option<Self> {
        let (guard, can_rw) = Self::open_handle(path, read_write, &stop_flag)?;
        let info = unsafe { read_hid_info(guard.0, path)? };
        Some(Self {
            handle: guard,
            norm_path: normalize_hid_path(path),
            info,
            can_rw,
            stop_flag,
            reclaimer,
        })
    }

    fn open_with_info(
        info: HidInterfaceInfo,
        read_write: bool,
        stop_flag: Arc<AtomicBool>,
        reclaimer: Arc<IoReclaimer>,
    ) -> Option<Self> {
        let (guard, can_rw) = Self::open_handle(&info.path, read_write, &stop_flag)?;
        let norm_path = normalize_hid_path(&info.path);
        Some(Self {
            handle: guard,
            norm_path,
            info,
            can_rw,
            stop_flag,
            reclaimer,
        })
    }

    fn run_overlapped<I, S>(
        &self,
        timeout_ms: u32,
        op_name: &str,
        buf_len: usize,
        init_buf: I,
        submit: S,
    ) -> Result<Vec<u8>, String>
    where
        I: FnOnce(&mut [u8]),
        S: FnOnce(*mut u8, usize, *mut OVERLAPPED) -> windows::core::Result<()>,
    {
        if self.stop_flag.load(Ordering::Relaxed) {
            return Err(format!("{} cancelled", op_name));
        }
        let now = Instant::now();
        if self.reclaimer.is_quarantined(&self.norm_path, now) {
            return Err(format!("{} skipped: device quarantined", op_name));
        }
        if self.reclaimer.is_saturated() {
            return Err(format!("{} skipped: reclaim queue full", op_name));
        }

        unsafe {
            let event = CreateEventW(None, true, false, None)
                .map_err(|e| format!("CreateEventW failed: {}", e))?;
            let mut ctx = Box::new(Win32PendingIo::new(
                Arc::clone(&self.handle),
                HandleGuard(event),
                buf_len,
            ));
            init_buf(ctx.buffer_mut_slice());

            let ov_ptr = ctx.overlapped_ptr();
            let buf_ptr = ctx.buffer_ptr();
            let raw_len = ctx.buffer_len();

            match submit(buf_ptr, raw_len, ov_ptr) {
                Ok(()) => {
                    let mut transferred = 0u32;
                    let res = GetOverlappedResult(self.handle.0, ov_ptr, &mut transferred, false);
                    ctx.completed = true;
                    res.map_err(|e| format!("{} GetOverlappedResult failed: {}", op_name, e))?;
                    Ok(ctx.copy_out(transferred as usize))
                }
                Err(e) if e.code() == ERROR_IO_PENDING.into() => {
                    struct Win32WaitOps<'a> {
                        file_handle: HANDLE,
                        event_handle: HANDLE,
                        ov_ptr: *mut OVERLAPPED,
                        ctx: &'a mut Win32PendingIo,
                        op_name: &'a str,
                    }
                    impl<'a> OverlappedOps for Win32WaitOps<'a> {
                        fn wait_slice(&mut self, slice_ms: u32) -> WaitSliceResult {
                            match unsafe { WaitForSingleObject(self.event_handle, slice_ms) } {
                                WAIT_OBJECT_0 => WaitSliceResult::Signaled,
                                WAIT_TIMEOUT => WaitSliceResult::Timeout,
                                _ => WaitSliceResult::Failed,
                            }
                        }
                        fn cancel_io(&mut self) {
                            unsafe {
                                let _ = CancelIoEx(self.file_handle, Some(self.ov_ptr));
                            }
                        }
                        fn poll_completed(&mut self) -> bool {
                            self.ctx.poll_completed()
                        }
                        fn read_result(&mut self) -> Result<u32, String> {
                            let mut transferred = 0u32;
                            unsafe {
                                GetOverlappedResult(
                                    self.file_handle,
                                    self.ov_ptr,
                                    &mut transferred,
                                    false,
                                )
                                .map(|()| transferred)
                                .map_err(|err| {
                                    format!("{} overlapped failed: {}", self.op_name, err)
                                })
                            }
                        }
                    }

                    let event_handle = ctx.event.0;
                    let file_handle = self.handle.0;
                    let mut ops = Win32WaitOps {
                        file_handle,
                        event_handle,
                        ov_ptr,
                        ctx: &mut ctx,
                        op_name,
                    };
                    let drive_res = drive_overlapped_wait(
                        &self.stop_flag,
                        timeout_ms,
                        HID_CANCEL_WAIT_MS,
                        op_name,
                        &mut ops,
                    );
                    match drive_res {
                        Ok(transferred) => {
                            ctx.completed = true;
                            Ok(ctx.copy_out(transferred as usize))
                        }
                        Err(OverlappedDriveError::CompletedErr(msg)) => {
                            ctx.completed = true;
                            Err(msg)
                        }
                        Err(OverlappedDriveError::DeferredPending(msg)) => {
                            self.reclaimer.quarantine_path(
                                &self.norm_path,
                                Instant::now(),
                                Duration::from_secs(HID_QUARANTINE_SECS),
                            );
                            self.reclaimer.handoff(ctx, true);
                            Err(msg)
                        }
                    }
                }
                Err(e) => {
                    ctx.completed = true;
                    Err(format!("{} error: {}", op_name, e))
                }
            }
        }
    }
}

impl HidIo for OpenHidDevice {
    fn info(&self) -> &HidInterfaceInfo {
        &self.info
    }

    fn can_rw(&self) -> bool {
        self.can_rw
    }

    fn write_output(&self, data: &[u8]) -> Result<(), String> {
        if !self.can_rw || self.info.output_len == 0 {
            return Err("HID interface not writable".into());
        }
        let len = self.info.output_len as usize;
        let _ = self.run_overlapped(
            300,
            "WriteFile",
            len,
            |buf| {
                let copy_len = data.len().min(buf.len());
                buf[..copy_len].copy_from_slice(&data[..copy_len]);
            },
            |buf_ptr, buf_len, ov_ptr| unsafe {
                let slice = std::slice::from_raw_parts(buf_ptr, buf_len);
                WriteFile(self.handle.0, Some(slice), None, Some(ov_ptr))
            },
        )?;
        Ok(())
    }

    fn read_input(&self, timeout_ms: u32) -> Result<Vec<u8>, String> {
        if !self.can_rw || self.info.input_len == 0 {
            return Err("HID interface not readable".into());
        }
        let len = self.info.input_len as usize;
        self.run_overlapped(
            timeout_ms.clamp(20, 500),
            "ReadFile",
            len,
            |_| {},
            |buf_ptr, buf_len, ov_ptr| unsafe {
                let slice = std::slice::from_raw_parts_mut(buf_ptr, buf_len);
                ReadFile(self.handle.0, Some(slice), None, Some(ov_ptr))
            },
        )
    }

    fn get_feature(&self, report_id: u8) -> Result<Vec<u8>, String> {
        if self.info.feature_len == 0 {
            return Err("No feature report support".into());
        }
        let len = self.info.feature_len as usize;
        // IOCTL_HID_GET_FEATURE uses METHOD_OUT_DIRECT: Report ID is placed at output buffer byte 0.
        let mut buf = self.run_overlapped(
            HID_IOCTL_TIMEOUT_MS,
            "IOCTL_HID_GET_FEATURE",
            len,
            |b| {
                if !b.is_empty() {
                    b[0] = report_id;
                }
            },
            |buf_ptr, buf_len, ov_ptr| unsafe {
                DeviceIoControl(
                    self.handle.0,
                    IOCTL_HID_GET_FEATURE,
                    None,
                    0,
                    Some(buf_ptr as *mut c_void),
                    buf_len as u32,
                    None,
                    Some(ov_ptr),
                )
            },
        )?;
        if buf.is_empty() {
            buf.resize(len, 0);
        }
        Ok(buf)
    }

    fn get_input_report(&self, report_id: u8) -> Result<Vec<u8>, String> {
        if self.info.input_len == 0 {
            return Err("No input report support".into());
        }
        let len = self.info.input_len as usize;
        // IOCTL_HID_GET_INPUT_REPORT uses METHOD_OUT_DIRECT: Report ID is placed at output buffer byte 0.
        let mut buf = self.run_overlapped(
            HID_IOCTL_TIMEOUT_MS,
            "IOCTL_HID_GET_INPUT_REPORT",
            len,
            |b| {
                if !b.is_empty() {
                    b[0] = report_id;
                }
            },
            |buf_ptr, buf_len, ov_ptr| unsafe {
                DeviceIoControl(
                    self.handle.0,
                    IOCTL_HID_GET_INPUT_REPORT,
                    None,
                    0,
                    Some(buf_ptr as *mut c_void),
                    buf_len as u32,
                    None,
                    Some(ov_ptr),
                )
            },
        )?;
        if buf.is_empty() {
            buf.resize(len, 0);
        }
        Ok(buf)
    }

    fn set_feature(&self, data: &[u8]) -> Result<(), String> {
        if self.info.feature_len == 0 {
            return Err("No feature report support".into());
        }
        let len = self.info.feature_len as usize;
        // IOCTL_HID_SET_FEATURE uses METHOD_IN_DIRECT: hidclass reads report payload from lpInBuffer.
        let _ = self.run_overlapped(
            HID_IOCTL_TIMEOUT_MS,
            "IOCTL_HID_SET_FEATURE",
            len,
            |b| {
                let copy_len = data.len().min(b.len());
                b[..copy_len].copy_from_slice(&data[..copy_len]);
            },
            |buf_ptr, buf_len, ov_ptr| unsafe {
                DeviceIoControl(
                    self.handle.0,
                    IOCTL_HID_SET_FEATURE,
                    Some(buf_ptr as *const c_void),
                    buf_len as u32,
                    None,
                    0,
                    None,
                    Some(ov_ptr),
                )
            },
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct DevicePollState {
    last_attempt: Instant,
    next_probe_after: Instant,
    fail_streak: u32,
    deferred: bool,
    ever_had_battery: bool,
}

#[derive(Debug, Clone)]
struct EnumeratedHidPath {
    path: String,
    instance_tag: String,
}

#[derive(Clone)]
struct PollCandidate {
    info: HidInterfaceInfo,
    meta: super::protocols::DeviceMeta,
    group_key: String,
    is_custom_match: bool,
    deferred_prio: u8,
    last_attempt_rank: Option<Instant>,
    custom_prio: u8,
    vendor_prio: u8,
    usage_prio: u8,
}

fn sort_poll_candidates(candidates: &mut [PollCandidate]) {
    candidates.sort_by(|a, b| {
        a.deferred_prio
            .cmp(&b.deferred_prio)
            .then_with(|| a.last_attempt_rank.cmp(&b.last_attempt_rank))
            .then_with(|| a.group_key.cmp(&b.group_key))
            .then_with(|| a.custom_prio.cmp(&b.custom_prio))
            .then_with(|| a.vendor_prio.cmp(&b.vendor_prio))
            .then_with(|| a.usage_prio.cmp(&b.usage_prio))
    });
}

fn normalize_hid_path(path: &str) -> String {
    path.trim().to_ascii_lowercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HidPollMode {
    Normal,
    DeviceChange,
    Force,
}

#[derive(Debug, Clone)]
struct HidppSlotCache {
    group_key: String,
    dev_idx: u8,
    device: WirelessDevice,
    base_detail_note: Option<String>,
    last_battery_seen: Instant,
    fail_streak: u32,
}

pub struct HidMonitor {
    registry: DeviceRegistry,
    interface_cache: HashMap<String, HidInterfaceInfo>,
    persistent_handles: HashMap<String, OpenHidDevice>,
    cached_readings: HashMap<String, (BatteryReading, Instant)>,
    cached_group_devices: HashMap<String, (Vec<WirelessDevice>, Instant)>,
    hidpp_slots: HashMap<String, HidppSlotCache>,
    known_group_paths: HashMap<String, HashSet<String>>,
    poll_states: HashMap<String, DevicePollState>,
    stop_flag: Arc<AtomicBool>,
    reclaimer: Arc<IoReclaimer>,
}

impl Default for HidMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HidMonitor {
    fn drop(&mut self) {
        self.persistent_handles.clear();
        self.interface_cache.clear();
        self.cached_readings.clear();
        self.cached_group_devices.clear();
        self.hidpp_slots.clear();
        self.known_group_paths.clear();
        self.poll_states.clear();
        self.reclaimer.shutdown_and_reap();
    }
}

impl HidMonitor {
    pub fn new() -> Self {
        Self::with_stop_flag(Arc::new(AtomicBool::new(false)))
    }

    pub fn with_stop_flag(stop_flag: Arc<AtomicBool>) -> Self {
        let mut monitor = Self {
            registry: DeviceRegistry::load(),
            interface_cache: HashMap::new(),
            persistent_handles: HashMap::new(),
            cached_readings: HashMap::new(),
            cached_group_devices: HashMap::new(),
            hidpp_slots: HashMap::new(),
            known_group_paths: HashMap::new(),
            poll_states: HashMap::new(),
            stop_flag,
            reclaimer: Arc::new(IoReclaimer::new(HID_RECLAIM_QUEUE_CAP)),
        };
        // Pre-open passive telemetry endpoints (e.g., Rapoo 54L 0xFF00:0x0002) so Windows HID
        // buffers incoming unsolicited 0x07 heartbeats between polls.
        monitor.warm_passive_handles();
        monitor
    }

    fn is_stopped(&self) -> bool {
        self.stop_flag.load(Ordering::Relaxed)
    }

    fn get_or_read_interface_info(&mut self, path: &str) -> Option<HidInterfaceInfo> {
        let key = normalize_hid_path(path);
        if let Some(cached) = self.interface_cache.get(&key) {
            return Some(cached.clone());
        }
        let dev = OpenHidDevice::open(
            path,
            false,
            Arc::clone(&self.stop_flag),
            Arc::clone(&self.reclaimer),
        )?;
        self.interface_cache.insert(key, dev.info.clone());
        Some(dev.info)
    }

    fn warm_passive_handles(&mut self) {
        let (enumerated, _) = enumerate_hid_interfaces_until(None);
        for item in enumerated {
            if self.is_stopped() {
                break;
            }
            let Some(info) = self.get_or_read_interface_info(&item.path) else {
                continue;
            };
            let Some(meta) = self.registry.lookup(&info) else {
                continue;
            };
            if meta.protocol == ProtocolFamily::Rapoo
                && info.usage_page >= 0xFF00
                && info.usage == 0x0002
            {
                let norm_key = normalize_hid_path(&info.path);
                if let Some(rw) = OpenHidDevice::open_with_info(
                    info,
                    true,
                    Arc::clone(&self.stop_flag),
                    Arc::clone(&self.reclaimer),
                ) {
                    self.persistent_handles.insert(norm_key, rw);
                }
            }
        }
    }

    pub fn poll(&mut self) -> Vec<WirelessDevice> {
        self.poll_internal(HidPollMode::Normal)
    }

    pub fn poll_device_change(&mut self) -> Vec<WirelessDevice> {
        self.poll_internal(HidPollMode::DeviceChange)
    }

    pub fn poll_force(&mut self) -> Vec<WirelessDevice> {
        self.poll_internal(HidPollMode::Force)
    }

    fn poll_internal(&mut self, mode: HidPollMode) -> Vec<WirelessDevice> {
        if self.is_stopped() {
            return Vec::new();
        }
        let _ = self.reclaimer.reap_completed();
        let poll_start = Instant::now();
        let deadline = poll_start + HID_POLL_BUDGET;

        let (enumerated, mut enum_complete) = enumerate_hid_interfaces_until(Some(deadline));
        let present_norm_paths: HashSet<String> = enumerated
            .iter()
            .map(|e| normalize_hid_path(&e.path))
            .collect();
        if enum_complete {
            self.persistent_handles
                .retain(|p, _| present_norm_paths.contains(p));
            self.interface_cache
                .retain(|p, _| present_norm_paths.contains(p));
        }

        // Collect candidate interfaces and map each path to its physical device group key.
        let mut candidates: Vec<PollCandidate> = Vec::new();
        let mut present_groups: HashSet<String> = HashSet::new();
        for item in enumerated {
            if self.is_stopped() {
                return Vec::new();
            }
            if Instant::now() >= deadline {
                enum_complete = false;
                break;
            }
            let Some(info) = self.get_or_read_interface_info(&item.path) else {
                continue;
            };
            let Some(meta) = self.registry.lookup(&info) else {
                continue;
            };
            let group_key = make_device_group_key_with_instance(&info, Some(&item.instance_tag));
            present_groups.insert(group_key.clone());

            let (deferred_prio, last_attempt_rank) = match self.poll_states.get(&group_key) {
                Some(st) => (if st.deferred { 0u8 } else { 1u8 }, Some(st.last_attempt)),
                None => (0u8, None),
            };
            let is_custom_match = meta.protocol == ProtocolFamily::CustomHid
                && !self.registry.matching_custom_rules(&info).is_empty();
            let custom_prio = if is_custom_match { 0u8 } else { 1u8 };
            let vendor_prio = if info.usage_page >= 0xFF00 { 0u8 } else { 1u8 };
            let usage_prio = if info.usage == 0x0002 || info.usage == 0x0061 {
                0u8
            } else {
                1u8
            };
            candidates.push(PollCandidate {
                info,
                meta,
                group_key,
                is_custom_match,
                deferred_prio,
                last_attempt_rank,
                custom_prio,
                vendor_prio,
                usage_prio,
            });
        }

        let mut prober = RealCandidateProber {
            deadline,
            force_refresh: mode == HidPollMode::Force,
        };
        self.poll_candidates_with_mode(
            candidates,
            &present_groups,
            enum_complete,
            Instant::now(),
            mode,
            &mut prober,
        )
    }

    #[cfg(test)]
    fn poll_candidates_with(
        &mut self,
        candidates: Vec<PollCandidate>,
        present_groups: &HashSet<String>,
        now: Instant,
        prober: &mut impl CandidateProber,
    ) -> Vec<WirelessDevice> {
        self.poll_candidates_with_mode(
            candidates,
            present_groups,
            true,
            now,
            HidPollMode::Normal,
            prober,
        )
    }

    #[cfg(test)]
    fn poll_candidates_with_force(
        &mut self,
        candidates: Vec<PollCandidate>,
        present_groups: &HashSet<String>,
        now: Instant,
        force_refresh: bool,
        prober: &mut impl CandidateProber,
    ) -> Vec<WirelessDevice> {
        let mode = if force_refresh {
            HidPollMode::Force
        } else {
            HidPollMode::Normal
        };
        self.poll_candidates_with_mode(candidates, present_groups, true, now, mode, prober)
    }

    fn poll_candidates_with_mode(
        &mut self,
        mut candidates: Vec<PollCandidate>,
        present_groups: &HashSet<String>,
        enum_complete: bool,
        now: Instant,
        mode: HidPollMode,
        prober: &mut impl CandidateProber,
    ) -> Vec<WirelessDevice> {
        let mut grouped: HashMap<String, WirelessDevice> = HashMap::new();

        // Only prune missing groups when full enumeration completed within budget.
        if enum_complete {
            self.poll_states.retain(|k, _| present_groups.contains(k));
            self.known_group_paths
                .retain(|k, _| present_groups.contains(k));
        }
        self.cached_readings.retain(|k, (_, ts)| {
            let group_present = !enum_complete
                || present_groups.contains(k)
                || k.rsplit_once(":slot")
                    .is_some_and(|(gk, _)| present_groups.contains(gk));
            group_present && is_within_cache_grace(*ts, now)
        });
        self.cached_group_devices.retain(|k, (_, ts)| {
            (!enum_complete || present_groups.contains(k)) && is_within_cache_grace(*ts, now)
        });
        self.hidpp_slots.retain(|_, slot| {
            (!enum_complete || present_groups.contains(&slot.group_key))
                && is_within_cache_grace(slot.last_battery_seen, now)
        });

        // Detect newly discovered groups or groups whose HID interface path set changed
        // (for gentle DeviceChange refresh without waking unrelated sleeping devices).
        let mut round_group_paths: HashMap<String, HashSet<String>> = HashMap::new();
        for cand in &candidates {
            round_group_paths
                .entry(cand.group_key.clone())
                .or_default()
                .insert(normalize_hid_path(&cand.info.path));
        }
        let mut changed_groups: HashSet<String> = HashSet::new();
        for (gk, paths) in &round_group_paths {
            let changed = match self.known_group_paths.get(gk) {
                Some(prev_paths) => prev_paths != paths,
                None => true,
            };
            if changed {
                changed_groups.insert(gk.clone());
            }
            self.known_group_paths.insert(gk.clone(), paths.clone());
        }

        // Order deferred/least-recently-polled device groups first, then keep interface priorities
        // grouped by device key with zero allocations during sort.
        sort_poll_candidates(&mut candidates);

        let mut probed_groups_this_round: HashSet<String> = HashSet::new();
        let mut skipped_groups: HashSet<String> = HashSet::new();
        let mut deferred_groups: HashSet<String> = HashSet::new();
        let mut group_had_success: HashMap<String, bool> = HashMap::new();
        let mut group_min_poll: HashMap<String, Option<u64>> = HashMap::new();
        let mut passive_push_groups: HashSet<String> = HashSet::new();
        let mut hidpp_probed_groups: HashSet<String> = HashSet::new();
        let mut hidpp_responded_slots: HashMap<String, HashSet<u8>> = HashMap::new();
        let mut hidpp_hit_deadline_groups: HashSet<String> = HashSet::new();

        for cand in candidates {
            let PollCandidate {
                info,
                meta,
                group_key,
                is_custom_match,
                ..
            } = cand;
            if self.is_stopped() {
                break;
            }
            if skipped_groups.contains(&group_key) {
                continue;
            }
            if meta.protocol != ProtocolFamily::Hidpp {
                if let Some(existing) = grouped.get(&group_key) {
                    if existing.battery_pct.is_some()
                        && group_had_success.get(&group_key).copied().unwrap_or(false)
                    {
                        continue;
                    }
                }
            }

            let is_dareu_kbd = is_dareu_kbd_feature(&info);
            let need_rw = is_custom_match
                || is_dareu_kbd
                || info.usage_page >= 0xFF00
                || info.usage_page == 0x0085
                || (info.usage_page == 0x0006 && info.usage == 0x0020);
            if !need_rw {
                continue;
            }

            let min_poll_secs = self.registry.min_poll_secs(info.vid, info.pid);
            group_min_poll
                .entry(group_key.clone())
                .or_insert(min_poll_secs);
            if meta.protocol == ProtocolFamily::Rapoo {
                passive_push_groups.insert(group_key.clone());
            }

            // Enforce per-device min_poll_secs / exponential backoff on group entry.
            if !probed_groups_this_round.contains(&group_key) {
                if let Some(st) = self.poll_states.get(&group_key) {
                    if now < st.next_probe_after {
                        let bypass = match mode {
                            HidPollMode::Force => true,
                            HidPollMode::DeviceChange => {
                                let group_changed = changed_groups.contains(&group_key);
                                let within_min_poll = min_poll_secs.is_some_and(|secs| {
                                    secs > 0
                                        && now.saturating_duration_since(st.last_attempt).as_secs()
                                            < secs
                                });
                                group_changed && !within_min_poll
                            }
                            HidPollMode::Normal => false,
                        };
                        if !bypass {
                            let cached_online = self.resolve_skipped_group_online(&group_key, now);
                            self.emit_cached_group_or_standby(
                                &group_key,
                                &info,
                                &meta,
                                cached_online,
                                now,
                                &mut grouped,
                            );
                            skipped_groups.insert(group_key);
                            continue;
                        }
                    }
                }
            }

            // Check overall poll budget on EVERY candidate interface (including subsequent
            // interfaces within the same device group) so budget cannot be exceeded mid-group.
            if prober.is_budget_exhausted() {
                deferred_groups.insert(group_key.clone());
                let cached_online = self.resolve_skipped_group_online(&group_key, now);
                self.emit_cached_group_or_standby(
                    &group_key,
                    &info,
                    &meta,
                    cached_online,
                    now,
                    &mut grouped,
                );
                skipped_groups.insert(group_key);
                continue;
            }

            probed_groups_this_round.insert(group_key.clone());
            group_had_success.entry(group_key.clone()).or_insert(false);

            let prev_state = self.poll_states.get(&group_key);
            let prev_streak = prev_state.map(|s| s.fail_streak).unwrap_or(0);
            let ever_had_battery = prev_state.map(|s| s.ever_had_battery).unwrap_or(false);
            let tentative_fail_streak = prev_streak.saturating_add(1);

            if meta.protocol == ProtocolFamily::Hidpp {
                hidpp_probed_groups.insert(group_key.clone());
                let outcome = prober.probe_hidpp(self, &info, &meta).unwrap_or_default();
                if outcome.hit_deadline {
                    hidpp_hit_deadline_groups.insert(group_key.clone());
                    deferred_groups.insert(group_key.clone());
                }
                if !outcome.slots.is_empty() {
                    group_had_success.insert(group_key.clone(), true);
                    let responded_set = hidpp_responded_slots.entry(group_key.clone()).or_default();
                    for (dev_idx, slot_name, slot_kind, reading) in outcome.slots {
                        responded_set.insert(dev_idx);
                        let slot_key = if dev_idx == 0xFF {
                            group_key.clone()
                        } else {
                            format!("{}:slot{}", group_key, dev_idx)
                        };
                        self.cached_readings
                            .insert(slot_key.clone(), (reading.clone(), now));
                        let base_note = reading.detail_note.clone();
                        let entry = WirelessDevice {
                            id: slot_key.clone(),
                            name: slot_name,
                            kind: slot_kind,
                            transport: Transport::Wireless24G,
                            battery_pct: reading.battery_pct,
                            charging: reading.charging,
                            sub_batteries: None,
                            online: reading.online,
                            last_seen: now,
                            detail_note: reading.detail_note,
                        };
                        self.hidpp_slots.insert(
                            slot_key.clone(),
                            HidppSlotCache {
                                group_key: group_key.clone(),
                                dev_idx,
                                device: entry.clone(),
                                base_detail_note: base_note,
                                last_battery_seen: now,
                                fail_streak: 0,
                            },
                        );
                        grouped.insert(slot_key, entry);
                    }
                }
                continue;
            }

            let reading = prober.probe_standard_or_vendor(self, &info, &meta, &group_key);

            let (effective_reading, effective_seen, succeeded) =
                resolve_effective_reading_with_streak(
                    reading,
                    &group_key,
                    now,
                    tentative_fail_streak,
                    ever_had_battery,
                    &mut self.cached_readings,
                );

            let already_succeeded = group_had_success.get(&group_key).copied().unwrap_or(false);
            if succeeded {
                group_had_success.insert(group_key.clone(), true);
            }

            let name = resolve_hid_display_name(&info, &meta);
            if let Some(reading) = effective_reading {
                let entry = WirelessDevice {
                    id: group_key.clone(),
                    name,
                    kind: meta.kind,
                    transport: Transport::Wireless24G,
                    battery_pct: reading.battery_pct,
                    charging: reading.charging,
                    sub_batteries: None,
                    online: reading.online,
                    last_seen: effective_seen,
                    detail_note: reading.detail_note,
                };

                if succeeded {
                    self.cached_group_devices
                        .insert(group_key.clone(), (vec![entry.clone()], now));
                } else if !already_succeeded && (entry.battery_pct.is_some() || !ever_had_battery) {
                    let cache_ts = if entry.battery_pct.is_some() {
                        effective_seen
                    } else {
                        now
                    };
                    self.cached_group_devices
                        .insert(group_key.clone(), (vec![entry.clone()], cache_ts));
                }

                match grouped.get_mut(&group_key) {
                    Some(prev) => {
                        if (succeeded && !already_succeeded)
                            || (prev.battery_pct.is_none()
                                && (entry.battery_pct.is_some() || entry.detail_note.is_some()))
                        {
                            *prev = entry;
                        }
                    }
                    None => {
                        grouped.insert(group_key, entry);
                    }
                }
            } else if info.usage_page >= 0xFF00 && !grouped.contains_key(&group_key) {
                // Present Vendor-Defined HID interface without a battery reading:
                // only treat as sleeping/offline if it previously reported a battery reading
                // and its cache has expired; otherwise the USB/2.4G interface is online without battery.
                let online = !ever_had_battery;
                let note = if online && meta.protocol == ProtocolFamily::Rapoo {
                    "Rapoo (awaiting telemetry frame)"
                } else if online {
                    "2.4G Receiver (interface present, battery unavailable)"
                } else {
                    "2.4G Receiver (standby / awaiting telemetry)"
                };
                let entry = WirelessDevice {
                    id: group_key.clone(),
                    name,
                    kind: meta.kind,
                    transport: Transport::Wireless24G,
                    battery_pct: None,
                    charging: false,
                    sub_batteries: None,
                    online,
                    last_seen: now,
                    detail_note: Some(note.into()),
                };
                if !already_succeeded {
                    self.cached_group_devices
                        .insert(group_key.clone(), (vec![entry.clone()], now));
                }
                grouped.insert(group_key, entry);
            }
        }

        // Reconcile per-slot HID++ caches once per probed HID++ group so one awake slot never
        // drops a sleeping sibling slot on the same receiver.
        let mut expired_hidpp_keys = Vec::new();
        for group_key in &hidpp_probed_groups {
            let responded = hidpp_responded_slots.get(group_key);
            let hit_deadline = hidpp_hit_deadline_groups.contains(group_key);
            for (slot_key, slot) in self.hidpp_slots.iter_mut() {
                if &slot.group_key != group_key {
                    continue;
                }
                if responded.is_some_and(|set| set.contains(&slot.dev_idx)) {
                    continue;
                }
                if !hit_deadline {
                    slot.fail_streak = slot.fail_streak.saturating_add(1);
                }
                if !is_within_cache_grace(slot.last_battery_seen, now) {
                    expired_hidpp_keys.push(slot_key.clone());
                    continue;
                }
                let debounced_online = slot.fail_streak < HID_OFFLINE_FAIL_THRESHOLD;
                let mut d = slot.device.clone();
                d.online = debounced_online;
                d.detail_note = slot.base_detail_note.clone();
                if !debounced_online {
                    mark_device_cached_offline(&mut d);
                }
                slot.device.online = debounced_online;
                grouped.entry(slot_key.clone()).or_insert(d);
            }
        }
        for key in expired_hidpp_keys {
            self.hidpp_slots.remove(&key);
        }

        // If enumeration terminated early due to poll budget, emit cached devices for any
        // unreached present groups and mark them deferred for priority next round.
        if !enum_complete {
            let mut deferred_cached_groups: HashSet<String> = HashSet::new();
            for (gk, (devs, _)) in &self.cached_group_devices {
                if !probed_groups_this_round.contains(gk) && !skipped_groups.contains(gk) {
                    deferred_cached_groups.insert(gk.clone());
                    for dev in devs {
                        grouped.entry(dev.id.clone()).or_insert_with(|| dev.clone());
                    }
                }
            }
            for (slot_key, slot) in &self.hidpp_slots {
                if !probed_groups_this_round.contains(&slot.group_key)
                    && !skipped_groups.contains(&slot.group_key)
                {
                    deferred_cached_groups.insert(slot.group_key.clone());
                    grouped
                        .entry(slot_key.clone())
                        .or_insert_with(|| slot.device.clone());
                }
            }
            deferred_groups.extend(deferred_cached_groups);
        }

        // Update poll scheduling & exponential backoff for every group probed this round.
        for group_key in probed_groups_this_round {
            let success = group_had_success.get(&group_key).copied().unwrap_or(false);
            let min_poll = group_min_poll.get(&group_key).copied().flatten();
            let is_passive_push = passive_push_groups.contains(&group_key);
            let prev = self.poll_states.get(&group_key);
            let prev_streak = prev.map(|s| s.fail_streak).unwrap_or(0);
            let prev_ever = prev.map(|s| s.ever_had_battery).unwrap_or(false);
            let mut next_state = update_poll_state(now, success, prev_streak, min_poll);
            if is_passive_push && !success {
                if !prev_ever && next_state.fail_streak <= HID_PASSIVE_INITIAL_FAST_RETRIES {
                    // Passive-push devices (e.g. Rapoo Gen2 54L) emit periodic 0x07 frames
                    // without active TX; allow 2 short passive buffer re-reads on startup.
                    next_state.next_probe_after =
                        now + Duration::from_secs(HID_PASSIVE_INITIAL_RETRY_SECS);
                } else {
                    // Passive-push reads only drain the local Windows HID ring buffer (zero
                    // USB TX, never wakes sleeping mice). Cap cooldown at 45s (or min_poll_secs)
                    // instead of exponential 600s backoff so woken mice recover promptly.
                    let cap_secs = min_poll
                        .filter(|&s| s > 0)
                        .unwrap_or(HID_PASSIVE_MAX_BACKOFF_SECS);
                    next_state.next_probe_after = now + Duration::from_secs(cap_secs);
                }
            }
            next_state.ever_had_battery = prev_ever || success;
            if deferred_groups.contains(&group_key) {
                next_state.deferred = true;
                // Keep next_probe_after <= now when deferred mid-group/slot due to budget
                // so the next round prioritizes finishing deferred work.
                if !success {
                    next_state.fail_streak = prev_streak;
                    next_state.next_probe_after = now;
                }
            }
            self.poll_states.insert(group_key, next_state);
        }

        for group_key in deferred_groups {
            let st = self
                .poll_states
                .entry(group_key)
                .or_insert_with(|| DevicePollState {
                    last_attempt: now,
                    next_probe_after: now,
                    fail_streak: 0,
                    deferred: true,
                    ever_had_battery: false,
                });
            st.deferred = true;
        }

        let mut list: Vec<WirelessDevice> = grouped.into_values().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        list
    }

    fn resolve_skipped_group_online(&self, group_key: &str, now: Instant) -> bool {
        let hidpp_slots: Vec<&HidppSlotCache> = self
            .hidpp_slots
            .values()
            .filter(|s| s.group_key == group_key)
            .collect();
        if !hidpp_slots.is_empty() {
            return hidpp_slots.iter().any(|s| {
                s.fail_streak < HID_OFFLINE_FAIL_THRESHOLD
                    && is_within_cache_grace(s.last_battery_seen, now)
            });
        }
        let Some(st) = self.poll_states.get(group_key) else {
            // Never probed yet (e.g. deferred on first poll by time budget): keep online.
            return true;
        };
        if !st.ever_had_battery {
            // Device interface present without battery capability: keep its last evaluated
            // online state (true unless the protocol explicitly marked it offline).
            return self
                .cached_group_devices
                .get(group_key)
                .map(|(devs, _)| devs.iter().any(|d| d.online))
                .unwrap_or(true);
        }
        st.fail_streak < HID_OFFLINE_FAIL_THRESHOLD
            && self
                .cached_group_devices
                .get(group_key)
                .is_some_and(|(_, ts)| is_within_cache_grace(*ts, now))
    }

    fn emit_cached_group_or_standby(
        &self,
        group_key: &str,
        info: &HidInterfaceInfo,
        meta: &super::protocols::DeviceMeta,
        online: bool,
        now: Instant,
        grouped: &mut HashMap<String, WirelessDevice>,
    ) {
        let mut emitted_hidpp = false;
        for (slot_key, slot) in &self.hidpp_slots {
            if slot.group_key == group_key {
                emitted_hidpp = true;
                let slot_online = online
                    && slot.fail_streak < HID_OFFLINE_FAIL_THRESHOLD
                    && is_within_cache_grace(slot.last_battery_seen, now);
                let mut d = slot.device.clone();
                d.online = slot_online;
                d.detail_note = slot.base_detail_note.clone();
                if !slot_online {
                    mark_device_cached_offline(&mut d);
                }
                grouped.entry(slot_key.clone()).or_insert(d);
            }
        }
        if emitted_hidpp {
            return;
        }
        if let Some((cached_devs, _)) = self.cached_group_devices.get(group_key) {
            for dev in cached_devs {
                let mut d = dev.clone();
                if !online {
                    mark_device_cached_offline(&mut d);
                }
                grouped.entry(d.id.clone()).or_insert(d);
            }
            return;
        }
        if info.usage_page >= 0xFF00 && !grouped.contains_key(group_key) {
            let note = if online {
                "2.4G Receiver (interface present, awaiting telemetry)"
            } else {
                "2.4G Receiver (standby / awaiting telemetry)"
            };
            grouped.insert(
                group_key.to_string(),
                WirelessDevice {
                    id: group_key.to_string(),
                    name: resolve_hid_display_name(info, meta),
                    kind: meta.kind,
                    transport: Transport::Wireless24G,
                    battery_pct: None,
                    charging: false,
                    sub_batteries: None,
                    online,
                    last_seen: now,
                    detail_note: Some(note.into()),
                },
            );
        }
    }

    pub fn diagnostics(&mut self) -> String {
        self.diagnostics_with_options(false)
    }

    pub fn diagnostics_with_options(&mut self, verbose: bool) -> String {
        let mut out = String::new();
        let reg_diags = self.registry.diagnostics();
        if !reg_diags.is_empty() {
            out.push_str("[External devices.toml Diagnostics]\n");
            for line in reg_diags {
                out.push_str("  - ");
                out.push_str(line);
                out.push('\n');
            }
            out.push('\n');
        }

        let (enumerated, _) = enumerate_hid_interfaces_until(None);
        let present_norm_paths: HashSet<String> = enumerated
            .iter()
            .map(|e| normalize_hid_path(&e.path))
            .collect();
        self.persistent_handles
            .retain(|p, _| present_norm_paths.contains(p));
        self.interface_cache
            .retain(|p, _| present_norm_paths.contains(p));
        out.push_str(&format!(
            "[USB HID Devices] Enumerated {} interfaces\n",
            enumerated.len()
        ));

        for item in enumerated {
            if self.is_stopped() {
                out.push_str("  [Aborted due to stop request]\n");
                break;
            }
            let Some(info) = self.get_or_read_interface_info(&item.path) else {
                continue;
            };
            let matched = self.registry.lookup(&info);
            let proto_label = matched
                .as_ref()
                .map(|m| m.protocol.label())
                .unwrap_or("Unmatched");
            let reg_name = matched.as_ref().map(|m| m.name.as_str()).unwrap_or("-");

            out.push_str(&format_hid_diag_line(&info, reg_name, proto_label, verbose));

            let is_custom_match = matched
                .as_ref()
                .map(|m| {
                    m.protocol == ProtocolFamily::CustomHid
                        && !self.registry.matching_custom_rules(&info).is_empty()
                })
                .unwrap_or(false);
            let should_probe = matched.is_some()
                && (is_custom_match
                    || is_dareu_kbd_feature(&info)
                    || info.usage_page >= 0xFF00
                    || info.usage_page == 0x0085
                    || (info.usage_page == 0x0006 && info.usage == 0x0020));

            if should_probe {
                let family = matched
                    .as_ref()
                    .map(|m| m.protocol)
                    .unwrap_or(ProtocolFamily::Unknown);

                if family == ProtocolFamily::Hidpp {
                    if !self.registry.allows_active_tx(&info, ProtocolFamily::Hidpp) {
                        out.push_str(
                            "      -> Probe: skipped active HID++ write (read-only guard)\n",
                        );
                        continue;
                    }
                    let base_name = matched
                        .as_ref()
                        .map(|m| resolve_hid_display_name(&info, m))
                        .unwrap_or_else(|| "Logitech HID++".into());
                    let kind = matched
                        .as_ref()
                        .map(|m| m.kind)
                        .unwrap_or(DeviceKind::Other);
                    if let Some(rw_dev) = OpenHidDevice::open_with_info(
                        info,
                        true,
                        Arc::clone(&self.stop_flag),
                        Arc::clone(&self.reclaimer),
                    ) {
                        let outcome = probe_hidpp_guarded(
                            &self.registry,
                            &rw_dev,
                            &base_name,
                            kind,
                            &self.stop_flag,
                            None,
                        );
                        if outcome.blocked_by_ro {
                            out.push_str(
                                "      -> Probe: skipped active HID++ write (read-only guard)\n",
                            );
                        } else if outcome.slots.is_empty() {
                            out.push_str("      -> Probe: no response\n");
                        } else {
                            for (dev_idx, slot_name, _, reading) in outcome.slots {
                                out.push_str(&format!(
                                    "      -> Probe[slot {} {}]: battery={:?}%, charging={}, online={}, note={:?}\n",
                                    dev_idx,
                                    slot_name,
                                    reading.battery_pct,
                                    reading.charging,
                                    reading.online,
                                    reading.detail_note
                                ));
                                for pkt in &reading.raw_packets {
                                    out.push_str(&format!("         raw: {}\n", pkt));
                                }
                            }
                        }
                    }
                    continue;
                }

                let probe_res = if family == ProtocolFamily::Rapoo && info.usage == 0x0002 {
                    let norm_key = normalize_hid_path(&info.path);
                    if !self.persistent_handles.contains_key(&norm_key) {
                        if let Some(rw) = OpenHidDevice::open_with_info(
                            info.clone(),
                            true,
                            Arc::clone(&self.stop_flag),
                            Arc::clone(&self.reclaimer),
                        ) {
                            self.persistent_handles.insert(norm_key.clone(), rw);
                        }
                    }
                    let allow_tx = self.registry.allows_active_tx(&info, ProtocolFamily::Rapoo);
                    let first = self.persistent_handles.get(&norm_key).map(|io| {
                        let ro = ReadOnlyHidIo(io);
                        let eff: &dyn HidIo = if allow_tx { io } else { &ro };
                        rapoo::probe_detailed(eff, 3200)
                    });
                    match first {
                        Some(res) if res.io_error => {
                            self.persistent_handles.remove(&norm_key);
                            if let Some(rw) = OpenHidDevice::open_with_info(
                                info,
                                true,
                                Arc::clone(&self.stop_flag),
                                Arc::clone(&self.reclaimer),
                            ) {
                                self.persistent_handles.insert(norm_key.clone(), rw);
                                self.persistent_handles.get(&norm_key).and_then(|io| {
                                    let ro = ReadOnlyHidIo(io);
                                    let eff: &dyn HidIo = if allow_tx { io } else { &ro };
                                    rapoo::probe_diagnostics_listen(eff)
                                })
                            } else {
                                res.reading
                            }
                        }
                        Some(res) => res.reading,
                        None => None,
                    }
                } else {
                    OpenHidDevice::open_with_info(
                        info,
                        true,
                        Arc::clone(&self.stop_flag),
                        Arc::clone(&self.reclaimer),
                    )
                    .and_then(|rw_dev| probe_protocol(&self.registry, family, &rw_dev))
                };

                match probe_res {
                    Some(reading) => {
                        out.push_str(&format!(
                            "      -> Probe: battery={:?}%, charging={}, online={}, note={:?}\n",
                            reading.battery_pct,
                            reading.charging,
                            reading.online,
                            reading.detail_note
                        ));
                        for pkt in &reading.raw_packets {
                            out.push_str(&format!("         raw: {}\n", pkt));
                        }
                    }
                    None => {
                        out.push_str("      -> Probe: no response\n");
                    }
                }
            }
        }

        out
    }
}

fn mask_hid_serial(serial: &str, verbose: bool) -> String {
    if verbose || serial.is_empty() {
        serial.to_string()
    } else {
        format!("<redacted:{}>", serial.len())
    }
}

fn format_hid_diag_line(
    info: &HidInterfaceInfo,
    reg_name: &str,
    proto_label: &str,
    verbose: bool,
) -> String {
    let serial_disp = mask_hid_serial(&info.serial, verbose);
    format!(
        "  - {:04X}:{:04X} [{}] UP=0x{:04X} U=0x{:04X} len(I/O/F)=({}/{}/{}) | mfg='{}' prod='{}' sn='{}' | reg='{}' ({})\n",
        info.vid,
        info.pid,
        if info.interface_tag.is_empty() { "-" } else { &info.interface_tag },
        info.usage_page,
        info.usage,
        info.input_len,
        info.output_len,
        info.feature_len,
        info.manufacturer,
        info.product,
        serial_disp,
        reg_name,
        proto_label,
    )
}

type HidppSlotReading = (u8, String, DeviceKind, BatteryReading);

#[derive(Default, Clone)]
struct HidppProbeOutcome {
    slots: Vec<HidppSlotReading>,
    hit_deadline: bool,
    blocked_by_ro: bool,
}

trait CandidateProber {
    fn is_budget_exhausted(&self) -> bool;
    fn probe_hidpp(
        &mut self,
        monitor: &mut HidMonitor,
        info: &HidInterfaceInfo,
        meta: &super::protocols::DeviceMeta,
    ) -> Option<HidppProbeOutcome>;
    fn probe_standard_or_vendor(
        &mut self,
        monitor: &mut HidMonitor,
        info: &HidInterfaceInfo,
        meta: &super::protocols::DeviceMeta,
        group_key: &str,
    ) -> Option<BatteryReading>;
}

struct RealCandidateProber {
    deadline: Instant,
    force_refresh: bool,
}

impl RealCandidateProber {
    fn remaining_ms(&self) -> u64 {
        self.deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as u64
    }
}

impl CandidateProber for RealCandidateProber {
    fn is_budget_exhausted(&self) -> bool {
        Instant::now() >= self.deadline
    }

    fn probe_hidpp(
        &mut self,
        monitor: &mut HidMonitor,
        info: &HidInterfaceInfo,
        meta: &super::protocols::DeviceMeta,
    ) -> Option<HidppProbeOutcome> {
        if !monitor
            .registry
            .allows_active_tx(info, ProtocolFamily::Hidpp)
        {
            return Some(HidppProbeOutcome {
                slots: Vec::new(),
                hit_deadline: false,
                blocked_by_ro: true,
            });
        }
        let norm_key = normalize_hid_path(&info.path);
        if monitor.reclaimer.is_quarantined(&norm_key, Instant::now()) {
            return Some(HidppProbeOutcome::default());
        }
        let base_name = resolve_hid_display_name(info, meta);
        let io = OpenHidDevice::open_with_info(
            info.clone(),
            true,
            Arc::clone(&monitor.stop_flag),
            Arc::clone(&monitor.reclaimer),
        )?;
        Some(probe_hidpp_guarded(
            &monitor.registry,
            &io,
            &base_name,
            meta.kind,
            &monitor.stop_flag,
            Some(self.deadline),
        ))
    }

    fn probe_standard_or_vendor(
        &mut self,
        monitor: &mut HidMonitor,
        info: &HidInterfaceInfo,
        meta: &super::protocols::DeviceMeta,
        group_key: &str,
    ) -> Option<BatteryReading> {
        let norm_key = normalize_hid_path(&info.path);
        if monitor.reclaimer.is_quarantined(&norm_key, Instant::now()) {
            return None;
        }
        if meta.protocol == ProtocolFamily::Rapoo
            && info.usage_page >= 0xFF00
            && info.usage == 0x0002
        {
            let had_existing_handle = monitor.persistent_handles.contains_key(&norm_key);
            if !had_existing_handle {
                if let Some(rw) = OpenHidDevice::open_with_info(
                    info.clone(),
                    true,
                    Arc::clone(&monitor.stop_flag),
                    Arc::clone(&monitor.reclaimer),
                ) {
                    monitor.persistent_handles.insert(norm_key.clone(), rw);
                }
            }
            let has_cached_pct = monitor
                .cached_readings
                .get(group_key)
                .and_then(|(r, _)| r.battery_pct)
                .is_some();
            let prev_state = monitor.poll_states.get(group_key);
            let prev_streak = prev_state.map(|s| s.fail_streak).unwrap_or(0);
            let ever_had_battery = prev_state.map(|s| s.ever_had_battery).unwrap_or(false);
            let is_sleeping_offline = ever_had_battery && prev_streak >= HID_OFFLINE_FAIL_THRESHOLD;

            // Always read/drain the existing handle first so any already-buffered wake frame
            // is never discarded before deciding whether the handle needs to be reopened.
            let desired_listen_ms: u64 = if self.force_refresh {
                if has_cached_pct {
                    400
                } else if had_existing_handle {
                    1600
                } else {
                    3600
                }
            } else if is_sleeping_offline {
                // Sleeping passive mouse: drain already-buffered wake frames only (~25ms, zero TX).
                0
            } else if has_cached_pct {
                80
            } else {
                3600
            };
            let first_listen_ms = desired_listen_ms.min(self.remaining_ms());
            let allow_tx = monitor
                .registry
                .allows_active_tx(info, ProtocolFamily::Rapoo);

            let first_res = monitor.persistent_handles.get(&norm_key).map(|io| {
                let ro = ReadOnlyHidIo(io);
                let eff: &dyn HidIo = if allow_tx { io } else { &ro };
                if first_listen_ms == 0 {
                    rapoo::probe_drain_only(eff)
                } else {
                    rapoo::probe_detailed(eff, first_listen_ms)
                }
            });

            let first_has_pct = first_res
                .as_ref()
                .and_then(|r| r.reading.as_ref())
                .and_then(|r| r.battery_pct)
                .is_some();
            let first_io_err = first_res.as_ref().is_some_and(|r| r.io_error);

            if first_io_err && self.remaining_ms() > 0 {
                // Handle returned a non-timeout Win32 I/O error: close and reopen immediately.
                monitor.persistent_handles.remove(&norm_key);
                if let Some(rw) = OpenHidDevice::open_with_info(
                    info.clone(),
                    true,
                    Arc::clone(&monitor.stop_flag),
                    Arc::clone(&monitor.reclaimer),
                ) {
                    monitor.persistent_handles.insert(norm_key.clone(), rw);
                    if !first_has_pct {
                        let retry_ms = if first_listen_ms == 0 {
                            250
                        } else if self.force_refresh {
                            2000
                        } else {
                            first_listen_ms
                        }
                        .min(self.remaining_ms());
                        return monitor.persistent_handles.get(&norm_key).and_then(|io| {
                            let ro = ReadOnlyHidIo(io);
                            let eff: &dyn HidIo = if allow_tx { io } else { &ro };
                            rapoo::probe_detailed(eff, retry_ms).reading
                        });
                    }
                }
                return first_res.and_then(|r| r.reading);
            }

            if !first_has_pct && had_existing_handle && self.remaining_ms() > 0 {
                // On manual force refresh while offline/no-battery, reopen once to recover from
                // silent stale handles (USB suspend/driver quirk) and listen with remaining budget.
                if self.force_refresh && (is_sleeping_offline || !has_cached_pct) {
                    monitor.persistent_handles.remove(&norm_key);
                    if let Some(rw) = OpenHidDevice::open_with_info(
                        info.clone(),
                        true,
                        Arc::clone(&monitor.stop_flag),
                        Arc::clone(&monitor.reclaimer),
                    ) {
                        monitor.persistent_handles.insert(norm_key.clone(), rw);
                        let retry_ms = 2000u64.min(self.remaining_ms());
                        return monitor.persistent_handles.get(&norm_key).and_then(|io| {
                            let ro = ReadOnlyHidIo(io);
                            let eff: &dyn HidIo = if allow_tx { io } else { &ro };
                            rapoo::probe_detailed(eff, retry_ms).reading
                        });
                    }
                } else if is_sleeping_offline
                    && prev_streak > 0
                    && prev_streak.is_multiple_of(HID_PASSIVE_REOPEN_FAIL_INTERVAL)
                {
                    // Periodic silent-stale-handle self-heal during long offline periods (~15m).
                    // Reopening a local HID file handle sends zero USB TX to the sleeping mouse.
                    monitor.persistent_handles.remove(&norm_key);
                    if let Some(rw) = OpenHidDevice::open_with_info(
                        info.clone(),
                        true,
                        Arc::clone(&monitor.stop_flag),
                        Arc::clone(&monitor.reclaimer),
                    ) {
                        monitor.persistent_handles.insert(norm_key.clone(), rw);
                    }
                }
            }

            first_res.and_then(|r| r.reading)
        } else {
            let io = OpenHidDevice::open_with_info(
                info.clone(),
                true,
                Arc::clone(&monitor.stop_flag),
                Arc::clone(&monitor.reclaimer),
            )?;
            probe_protocol(&monitor.registry, meta.protocol, &io)
        }
    }
}

fn is_within_cache_grace(cached_ts: Instant, now: Instant) -> bool {
    now.saturating_duration_since(cached_ts).as_secs() <= HID_CACHE_GRACE_SECS
}

fn mark_device_cached_offline(dev: &mut WirelessDevice) {
    dev.online = false;
    if dev.battery_pct.is_none() {
        return;
    }
    if let Some(note) = &dev.detail_note {
        if !note.contains("(cached)") {
            dev.detail_note = Some(format!("{} (cached)", note));
        }
    } else {
        dev.detail_note = Some("2.4G (cached)".into());
    }
}

fn resolve_effective_reading_with_streak(
    reading: Option<BatteryReading>,
    group_key: &str,
    now: Instant,
    tentative_fail_streak: u32,
    ever_had_battery: bool,
    cached_readings: &mut HashMap<String, (BatteryReading, Instant)>,
) -> (Option<BatteryReading>, Instant, bool) {
    match reading {
        Some(mut r) if r.battery_pct.is_some() => {
            r.online = true;
            cached_readings.insert(group_key.to_string(), (r.clone(), now));
            (Some(r), now, true)
        }
        other => {
            if let Some((cached, cached_ts)) = cached_readings.get(group_key) {
                if cached.battery_pct.is_some() {
                    let debounced_online = tentative_fail_streak < HID_OFFLINE_FAIL_THRESHOLD
                        && is_within_cache_grace(*cached_ts, now);
                    let mut c = cached.clone();
                    c.online = debounced_online;
                    if !debounced_online {
                        let base_note = c.detail_note.as_deref().unwrap_or("2.4G");
                        if !base_note.contains("(cached)") {
                            c.detail_note = Some(format!("{} (cached)", base_note));
                        }
                    }
                    return (Some(c), *cached_ts, false);
                }
            }
            let adjusted = other.map(|mut r| {
                if ever_had_battery && tentative_fail_streak >= HID_OFFLINE_FAIL_THRESHOLD {
                    r.online = false;
                }
                r
            });
            (adjusted, now, false)
        }
    }
}

fn compute_backoff_secs(base_secs: u64, fail_streak: u32) -> u64 {
    let shift = fail_streak.saturating_sub(1).min(4);
    base_secs
        .max(15)
        .saturating_mul(1u64 << shift)
        .min(HID_MAX_BACKOFF_SECS)
}

fn update_poll_state(
    now: Instant,
    success: bool,
    prev_fail_streak: u32,
    min_poll_secs: Option<u64>,
) -> DevicePollState {
    if success {
        let next_probe_after = min_poll_secs
            .filter(|&s| s > 0)
            .map(|s| now + Duration::from_secs(s))
            .unwrap_or(now);
        DevicePollState {
            last_attempt: now,
            next_probe_after,
            fail_streak: 0,
            deferred: false,
            ever_had_battery: true,
        }
    } else {
        let fail_streak = prev_fail_streak.saturating_add(1);
        let base = min_poll_secs.unwrap_or(HID_DEFAULT_POLL_BASE_SECS);
        let backoff_secs = compute_backoff_secs(base, fail_streak);
        DevicePollState {
            last_attempt: now,
            next_probe_after: now + Duration::from_secs(backoff_secs),
            fail_streak,
            deferred: false,
            ever_had_battery: false,
        }
    }
}

fn probe_hidpp_guarded(
    registry: &DeviceRegistry,
    io: &dyn HidIo,
    base_name: &str,
    default_kind: DeviceKind,
    stop_flag: &AtomicBool,
    deadline: Option<Instant>,
) -> HidppProbeOutcome {
    if !registry.allows_active_tx(io.info(), ProtocolFamily::Hidpp) {
        return HidppProbeOutcome {
            slots: Vec::new(),
            hit_deadline: false,
            blocked_by_ro: true,
        };
    }
    let ro = ReadOnlyHidIo(io);
    let effective_io: &dyn HidIo = if registry.allows_active_tx(io.info(), ProtocolFamily::Hidpp) {
        io
    } else {
        &ro
    };
    let (slots, hit_deadline) = probe_hidpp_multi_slot_with_deadline(
        effective_io,
        base_name,
        default_kind,
        stop_flag,
        deadline,
    );
    HidppProbeOutcome {
        slots,
        hit_deadline,
        blocked_by_ro: false,
    }
}

/// Probe Logitech HID++ 2.0 across direct device (`0xFF`) and receiver slots (`1..=6`),
/// returning one entry per responsive wireless device rather than stopping at the first slot.
#[cfg(test)]
fn probe_hidpp_multi_slot(
    io: &dyn HidIo,
    base_name: &str,
    default_kind: DeviceKind,
    stop_flag: &AtomicBool,
) -> Vec<(u8, String, DeviceKind, BatteryReading)> {
    probe_hidpp_multi_slot_with_deadline(io, base_name, default_kind, stop_flag, None).0
}

fn probe_hidpp_multi_slot_with_deadline(
    io: &dyn HidIo,
    base_name: &str,
    default_kind: DeviceKind,
    stop_flag: &AtomicBool,
    deadline: Option<Instant>,
) -> (Vec<(u8, String, DeviceKind, BatteryReading)>, bool) {
    const HIDPP_SHORT: u8 = 0x10;
    const HIDPP_LONG: u8 = 0x11;
    const SW_ID: u8 = 0x0A;
    const FEAT_DEVICE_NAME: u16 = 0x0005;
    const FEAT_BATTERY_STATUS: u16 = 0x1000;
    const FEAT_UNIFIED_BATTERY: u16 = 0x1004;

    let info = io.info();
    if info.usage_page != 0xFF00 || !io.can_rw() || info.output_len < 7 || info.input_len < 7 {
        return (Vec::new(), false);
    }

    let report_id = if info.output_len >= 20 && info.usage == 0x0002 {
        HIDPP_LONG
    } else if info.usage == 0x0001 || info.output_len == 7 {
        HIDPP_SHORT
    } else {
        return (Vec::new(), false);
    };

    struct HidppCtx<'a> {
        io: &'a dyn HidIo,
        report_id: u8,
        raw_packets: Vec<String>,
        deadline: Option<Instant>,
        stop_flag: &'a AtomicBool,
    }

    impl<'a> HidppCtx<'a> {
        fn budget_expired(&self) -> bool {
            self.stop_flag.load(Ordering::Relaxed)
                || self.deadline.is_some_and(|d| Instant::now() >= d)
        }

        fn req(
            &mut self,
            dev_idx: u8,
            feat_idx: u8,
            func_idx: u8,
            params: &[u8],
        ) -> Option<Vec<u8>> {
            if self.budget_expired() {
                return None;
            }
            let len = if self.report_id == HIDPP_LONG { 20 } else { 7 };
            let mut pkt = vec![0u8; len];
            pkt[0] = self.report_id;
            pkt[1] = dev_idx;
            pkt[2] = feat_idx;
            pkt[3] = ((func_idx & 0x0F) << 4) | (SW_ID & 0x0F);
            for (i, &b) in params.iter().take(len - 4).enumerate() {
                pkt[4 + i] = b;
            }
            self.io.write_output(&pkt).ok()?;

            for _ in 0..3 {
                if self.budget_expired() {
                    break;
                }
                let read_ms = self
                    .deadline
                    .map(|d| {
                        d.saturating_duration_since(Instant::now())
                            .as_millis()
                            .clamp(20, 120) as u32
                    })
                    .unwrap_or(120);
                let Ok(resp) = self.io.read_input(read_ms) else {
                    break;
                };
                if resp.len() < 7 || resp[1] != dev_idx {
                    continue;
                }
                if resp[2] == 0x8F || resp[2] == 0xFF {
                    self.raw_packets
                        .push(format!("HID++ ERR: {}", format_hex(&resp)));
                    return None;
                }
                if resp[2] == feat_idx && (resp[3] & 0x0F) == SW_ID {
                    self.raw_packets
                        .push(format!("HID++ RX: {}", format_hex(&resp)));
                    return Some(resp);
                }
            }
            None
        }

        fn feat_idx(&mut self, dev_idx: u8, feature_id: u16) -> Option<u8> {
            let resp = self.req(
                dev_idx,
                0x00,
                0,
                &[(feature_id >> 8) as u8, (feature_id & 0xFF) as u8, 0x00],
            )?;
            let idx = resp.get(4).copied().unwrap_or(0);
            if idx == 0 {
                None
            } else {
                Some(idx)
            }
        }

        fn query_slot_identity(
            &mut self,
            dev_idx: u8,
            base_name: &str,
            default_kind: DeviceKind,
        ) -> (String, DeviceKind) {
            let mut name_out = if dev_idx == 0xFF {
                base_name.to_string()
            } else {
                format!("{} (Slot {})", base_name, dev_idx)
            };
            let mut kind_out = default_kind;

            let Some(feat_idx) = self.feat_idx(dev_idx, FEAT_DEVICE_NAME) else {
                return (name_out, kind_out);
            };

            if let Some(count_resp) = self.req(dev_idx, feat_idx, 0, &[]) {
                let name_len = (count_resp.get(4).copied().unwrap_or(0) as usize).min(48);
                if name_len > 0 {
                    let mut bytes = Vec::with_capacity(name_len);
                    let mut offset = 0usize;
                    while offset < name_len {
                        let Some(chunk) = self.req(dev_idx, feat_idx, 1, &[offset as u8]) else {
                            break;
                        };
                        let avail = chunk.len().saturating_sub(4);
                        if avail == 0 {
                            break;
                        }
                        let take = (name_len - offset).min(avail);
                        bytes.extend_from_slice(&chunk[4..4 + take]);
                        offset += take;
                    }
                    if let Ok(s) = std::str::from_utf8(&bytes) {
                        let trimmed = s.trim_matches('\0').trim();
                        if !trimmed.is_empty() {
                            name_out = trimmed.to_string();
                        }
                    }
                }
            }

            if let Some(type_resp) = self.req(dev_idx, feat_idx, 2, &[]) {
                kind_out = match type_resp.get(4).copied() {
                    Some(0..=2) => DeviceKind::Keyboard,
                    Some(3..=5) => DeviceKind::Mouse,
                    Some(8) => DeviceKind::Headset,
                    _ => default_kind,
                };
            }

            (name_out, kind_out)
        }
    }

    let mut results = Vec::new();
    let mut hit_deadline = false;
    for &dev_idx in &[0xFFu8, 1, 2, 3, 4, 5, 6] {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            hit_deadline = true;
            break;
        }
        let mut ctx = HidppCtx {
            io,
            report_id,
            raw_packets: Vec::new(),
            deadline,
            stop_flag,
        };
        let battery = if let Some(feat_idx) = ctx.feat_idx(dev_idx, FEAT_UNIFIED_BATTERY) {
            let caps = ctx
                .req(dev_idx, feat_idx, 0, &[0, 0, 0])
                .and_then(|r| hidpp::parse_unified_battery_capabilities(&r));
            let supports_soc = caps.map(|c| c.supports_soc);
            ctx.req(dev_idx, feat_idx, 1, &[0, 0, 0])
                .and_then(|resp| hidpp::parse_unified_battery(&resp, supports_soc))
                .map(|decoded| (decoded, "UnifiedBattery"))
        } else {
            None
        }
        .or_else(|| {
            if ctx.budget_expired() {
                return None;
            }
            let feat_idx = ctx.feat_idx(dev_idx, FEAT_BATTERY_STATUS)?;
            let resp = ctx.req(dev_idx, feat_idx, 0, &[0, 0, 0])?;
            let decoded = hidpp::parse_battery_status(&resp)?;
            Some((decoded, "BatteryStatus"))
        });

        if deadline.is_some_and(|d| Instant::now() >= d) && battery.is_none() {
            hit_deadline = true;
            break;
        }

        if let Some((decoded, label)) = battery {
            let (slot_name, slot_kind) = ctx.query_slot_identity(dev_idx, base_name, default_kind);
            let detail_note = hidpp::format_hidpp_detail_note(label, dev_idx, &decoded);
            results.push((
                dev_idx,
                slot_name,
                slot_kind,
                BatteryReading {
                    battery_pct: decoded.battery_pct,
                    charging: decoded.charging,
                    online: true,
                    detail_note: Some(detail_note),
                    raw_packets: ctx.raw_packets,
                },
            ));
            if dev_idx == 0xFF {
                break;
            }
        }
    }

    (results, hit_deadline)
}

fn is_dareu_kbd_feature(info: &HidInterfaceInfo) -> bool {
    info.usage_page == 0x0001
        && info.usage == 0x0006
        && info.feature_len == 65
        && (info.vid == 0x05AC || info.vid == 0x260D)
}

fn resolve_hid_display_name(
    info: &HidInterfaceInfo,
    meta: &super::protocols::DeviceMeta,
) -> String {
    if info.vid == 0x05AC && info.pid == 0x024F && info.manufacturer.eq_ignore_ascii_case("Smart") {
        return "Dareu A87".to_string();
    }
    if !meta.name.is_empty() {
        meta.name.clone()
    } else if !info.product.is_empty() {
        info.product.clone()
    } else {
        format!("HID {:04X}:{:04X}", info.vid, info.pid)
    }
}

fn make_device_group_key_with_instance(
    info: &HidInterfaceInfo,
    instance_tag: Option<&str>,
) -> String {
    if !info.serial.is_empty() {
        format!("hid:{:04X}:{:04X}:{}", info.vid, info.pid, info.serial)
    } else if let Some(tag) = instance_tag.filter(|t| !t.is_empty()) {
        format!("hid:{:04X}:{:04X}:{}", info.vid, info.pid, tag)
    } else {
        format!("hid:{:04X}:{:04X}", info.vid, info.pid)
    }
}

fn extract_path_instance_fallback(path: &str) -> String {
    if let Some(part) = path.split('#').nth(2) {
        part.to_ascii_uppercase()
    } else {
        path.to_ascii_uppercase()
    }
}

unsafe fn read_container_id_tag(dev_info: HDEVINFO, dev_data: &SP_DEVINFO_DATA) -> Option<String> {
    let mut prop_type = Default::default();
    let mut buf = [0u8; 16];
    let mut req = 0u32;
    if SetupDiGetDevicePropertyW(
        dev_info,
        dev_data,
        &DEVPKEY_Device_ContainerId,
        &mut prop_type,
        Some(&mut buf),
        Some(&mut req),
        0,
    )
    .is_ok()
        && prop_type == DEVPROP_TYPE_GUID
        && req >= 16
        && buf.iter().any(|&b| b != 0)
    {
        let mut s = String::with_capacity(34);
        s.push_str("c:");
        for b in buf {
            use std::fmt::Write;
            let _ = write!(&mut s, "{:02X}", b);
        }
        Some(s)
    } else {
        None
    }
}

fn enumerate_hid_interfaces_until(deadline: Option<Instant>) -> (Vec<EnumeratedHidPath>, bool) {
    let mut out = Vec::new();
    let mut complete = true;
    unsafe {
        let Ok(dev_info) = SetupDiGetClassDevsW(
            Some(&GUID_DEVINTERFACE_HID),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        ) else {
            return (out, true);
        };
        if dev_info.is_invalid() {
            return (out, true);
        }
        let _guard = DevInfoGuard(dev_info);

        let mut idx = 0u32;
        loop {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                complete = false;
                break;
            }
            let mut iface_data = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(
                dev_info,
                None,
                &GUID_DEVINTERFACE_HID,
                idx,
                &mut iface_data,
            )
            .is_err()
            {
                break;
            }
            idx += 1;

            let mut req_size = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(
                dev_info,
                &iface_data,
                None,
                0,
                Some(&mut req_size),
                None,
            );
            if req_size < std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 {
                continue;
            }

            // Use Vec<u32> so the buffer satisfies SP_DEVICE_INTERFACE_DETAIL_DATA_W alignment.
            let word_count = (req_size as usize).div_ceil(std::mem::size_of::<u32>());
            let mut detail_buf = vec![0u32; word_count];
            let detail_ptr = detail_buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail_ptr).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

            let mut dev_data = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };

            if SetupDiGetDeviceInterfaceDetailW(
                dev_info,
                &iface_data,
                Some(detail_ptr),
                req_size,
                None,
                Some(&mut dev_data),
            )
            .is_ok()
            {
                let offset = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
                let byte_len =
                    (req_size as usize).min(detail_buf.len() * std::mem::size_of::<u32>());
                if offset < byte_len {
                    let bytes =
                        std::slice::from_raw_parts(detail_buf.as_ptr() as *const u8, byte_len);
                    let (chunks, _) = bytes[offset..].as_chunks::<2>();
                    let u16s: Vec<u16> = chunks
                        .iter()
                        .map(|&c| u16::from_le_bytes(c))
                        .take_while(|&w| w != 0)
                        .collect();
                    let path = String::from_utf16_lossy(&u16s);
                    if !path.is_empty() {
                        let instance_tag = read_container_id_tag(dev_info, &dev_data)
                            .unwrap_or_else(|| extract_path_instance_fallback(&path));
                        out.push(EnumeratedHidPath { path, instance_tag });
                    }
                }
            }
        }
    }
    (out, complete)
}

unsafe fn read_hid_info(handle: HANDLE, path: &str) -> Option<HidInterfaceInfo> {
    let mut attrs = HIDD_ATTRIBUTES {
        Size: std::mem::size_of::<HIDD_ATTRIBUTES>() as u32,
        ..Default::default()
    };
    if !HidD_GetAttributes(handle, &mut attrs).as_bool() {
        return None;
    }

    let mut caps = HIDP_CAPS::default();
    let mut preparsed = Default::default();
    if HidD_GetPreparsedData(handle, &mut preparsed).as_bool() {
        let _ = HidP_GetCaps(preparsed, &mut caps);
        let _ = HidD_FreePreparsedData(preparsed);
    }

    let manufacturer =
        read_hid_string(|buf, bytes| HidD_GetManufacturerString(handle, buf, bytes).as_bool());
    let product = read_hid_string(|buf, bytes| HidD_GetProductString(handle, buf, bytes).as_bool());
    let serial =
        read_hid_string(|buf, bytes| HidD_GetSerialNumberString(handle, buf, bytes).as_bool());

    let interface_tag = extract_mi_tag(path);

    Some(HidInterfaceInfo {
        path: path.to_string(),
        vid: attrs.VendorID,
        pid: attrs.ProductID,
        usage_page: caps.UsagePage,
        usage: caps.Usage,
        input_len: caps.InputReportByteLength,
        output_len: caps.OutputReportByteLength,
        feature_len: caps.FeatureReportByteLength,
        manufacturer,
        product,
        serial,
        interface_tag,
    })
}

unsafe fn read_hid_string<F>(f: F) -> String
where
    F: FnOnce(*mut c_void, u32) -> bool,
{
    let mut buf = [0u16; 128];
    if f(buf.as_mut_ptr() as *mut c_void, (buf.len() * 2) as u32) {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len]).trim().to_string()
    } else {
        String::new()
    }
}

fn extract_mi_tag(path: &str) -> String {
    let upper = path.to_ascii_uppercase();
    if let Some(pos) = upper.find("MI_") {
        let rest = &upper[pos..];
        let tag: String = rest
            .chars()
            .take_while(|&c| c != '#' && c != '\\' && c != '&')
            .collect();
        return tag;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_info(vid: u16, pid: u16, serial: &str) -> HidInterfaceInfo {
        HidInterfaceInfo {
            path: String::new(),
            vid,
            pid,
            usage_page: 0xFF00,
            usage: 0x0002,
            input_len: 20,
            output_len: 20,
            feature_len: 0,
            manufacturer: String::new(),
            product: String::new(),
            serial: serial.to_string(),
            interface_tag: "MI_02".to_string(),
        }
    }

    #[test]
    fn test_group_key_distinguishes_serialless_dongles_and_merges_same_container() {
        let info_a = mk_info(0x24AE, 0x1412, "");
        let key_a_mi0 = make_device_group_key_with_instance(&info_a, Some("c:AABB"));
        let key_a_mi1 = make_device_group_key_with_instance(&info_a, Some("c:AABB"));
        let key_b = make_device_group_key_with_instance(&info_a, Some("c:CCDD"));
        assert_eq!(key_a_mi0, key_a_mi1);
        assert_ne!(key_a_mi0, key_b);

        let info_sn1 = mk_info(0x24AE, 0x1412, "SN001");
        let info_sn2 = mk_info(0x24AE, 0x1412, "SN002");
        assert_ne!(
            make_device_group_key_with_instance(&info_sn1, Some("c:AABB")),
            make_device_group_key_with_instance(&info_sn2, Some("c:AABB"))
        );
    }

    #[test]
    fn test_exponential_backoff_and_min_poll_secs() {
        let t0 = Instant::now();
        let ok = update_poll_state(t0, true, 3, Some(600));
        assert_eq!(ok.fail_streak, 0);
        assert_eq!(ok.next_probe_after.duration_since(t0).as_secs(), 600);

        assert_eq!(compute_backoff_secs(45, 1), 45);
        assert_eq!(compute_backoff_secs(45, 2), 90);
        assert_eq!(compute_backoff_secs(45, 3), 180);
        assert_eq!(compute_backoff_secs(45, 4), 360);
        assert_eq!(compute_backoff_secs(45, 5), 600);
        assert_eq!(compute_backoff_secs(45, 8), 600);

        let fail1 = update_poll_state(t0, false, 0, None);
        assert_eq!(fail1.fail_streak, 1);
        assert_eq!(fail1.next_probe_after.duration_since(t0).as_secs(), 45);
        let fail2 = update_poll_state(t0, false, fail1.fail_streak, None);
        assert_eq!(fail2.fail_streak, 2);
        assert_eq!(fail2.next_probe_after.duration_since(t0).as_secs(), 90);
    }

    struct MockHidppIo {
        info: HidInterfaceInfo,
    }

    impl HidIo for MockHidppIo {
        fn info(&self) -> &HidInterfaceInfo {
            &self.info
        }
        fn can_rw(&self) -> bool {
            true
        }
        fn write_output(&self, _data: &[u8]) -> Result<(), String> {
            Ok(())
        }
        fn read_input(&self, _timeout_ms: u32) -> Result<Vec<u8>, String> {
            Err("unused".into())
        }
        fn get_feature(&self, _report_id: u8) -> Result<Vec<u8>, String> {
            Err("unused".into())
        }
        fn get_input_report(&self, _report_id: u8) -> Result<Vec<u8>, String> {
            Err("unused".into())
        }
        fn set_feature(&self, _data: &[u8]) -> Result<(), String> {
            Err("unused".into())
        }
    }

    #[test]
    fn test_hidpp_multi_slot_probe_handles_stop_flag() {
        let stop = AtomicBool::new(true);
        let io = MockHidppIo {
            info: mk_info(0x046D, 0xC52B, "ABC"),
        };
        let res = probe_hidpp_multi_slot(&io, "Logitech Unifying", DeviceKind::Mouse, &stop);
        assert!(res.is_empty());
    }

    #[test]
    fn test_cache_debounce_preserves_online_for_transient_failures_and_marks_offline_after_threshold(
    ) {
        let mut cache = HashMap::new();
        let t0 = Instant::now();
        let fresh = BatteryReading {
            battery_pct: Some(74),
            charging: false,
            online: true,
            detail_note: Some("2.4G".into()),
            raw_packets: vec![],
        };
        let (r1, _, ok1) = resolve_effective_reading_with_streak(
            Some(fresh),
            "hid:1234:5678",
            t0,
            0,
            false,
            &mut cache,
        );
        assert!(ok1);
        assert!(r1.unwrap().online);

        // 1st and 2nd transient failures within cache grace stay online = true
        let t1 = t0 + Duration::from_secs(60);
        let (r2, seen2, ok2) =
            resolve_effective_reading_with_streak(None, "hid:1234:5678", t1, 1, true, &mut cache);
        assert!(!ok2);
        assert_eq!(seen2, t0);
        let debounced1 = r2.expect("cached reading should be returned");
        assert_eq!(debounced1.battery_pct, Some(74));
        assert!(debounced1.online);

        let t2 = t0 + Duration::from_secs(120);
        let (r3, seen3, ok3) =
            resolve_effective_reading_with_streak(None, "hid:1234:5678", t2, 2, true, &mut cache);
        assert!(!ok3);
        assert_eq!(seen3, t0);
        let debounced2 = r3.expect("cached reading should be returned");
        assert_eq!(debounced2.battery_pct, Some(74));
        assert!(debounced2.online);

        // 3rd consecutive failure reaches threshold -> online = false & "(cached)"
        let t3 = t0 + Duration::from_secs(210);
        let (r4, seen4, ok4) =
            resolve_effective_reading_with_streak(None, "hid:1234:5678", t3, 3, true, &mut cache);
        assert!(!ok4);
        assert_eq!(seen4, t0);
        let offline = r4.expect("cached reading should be returned");
        assert_eq!(offline.battery_pct, Some(74));
        assert!(!offline.online);
        assert!(offline.detail_note.unwrap_or_default().contains("(cached)"));
    }

    struct MultiSlotMockIo {
        info: HidInterfaceInfo,
        rx_queue: std::sync::Mutex<std::collections::VecDeque<Vec<u8>>>,
    }

    impl HidIo for MultiSlotMockIo {
        fn info(&self) -> &HidInterfaceInfo {
            &self.info
        }
        fn can_rw(&self) -> bool {
            true
        }
        fn write_output(&self, data: &[u8]) -> Result<(), String> {
            let dev_idx = data[1];
            let feat_idx = data[2];
            let func = (data[3] >> 4) & 0x0F;
            let sw = data[3] & 0x0F;
            let mut resp = vec![0u8; 20];
            resp[0] = data[0];
            resp[1] = dev_idx;
            if dev_idx == 1 || dev_idx == 2 {
                if feat_idx == 0x00 && func == 0 {
                    let fid = u16::from_be_bytes([data[4], data[5]]);
                    resp[2] = 0x00;
                    resp[3] = sw;
                    resp[4] = match fid {
                        0x1004 => 0x05,
                        0x0005 => 0x03,
                        _ => 0x00,
                    };
                } else if feat_idx == 0x05 && func == 1 {
                    resp[2] = 0x05;
                    resp[3] = sw;
                    resp[4] = if dev_idx == 1 { 82 } else { 45 };
                    resp[6] = if dev_idx == 1 { 0 } else { 1 };
                } else if feat_idx == 0x03 && func == 0 {
                    resp[2] = 0x03;
                    resp[3] = sw;
                    resp[4] = if dev_idx == 1 { 5 } else { 7 };
                } else if feat_idx == 0x03 && func == 1 {
                    resp[2] = 0x03;
                    resp[3] = sw;
                    let name = if dev_idx == 1 {
                        b"Mouse".as_slice()
                    } else {
                        b"Keybd-X".as_slice()
                    };
                    for (i, &b) in name.iter().enumerate() {
                        resp[4 + i] = b;
                    }
                } else if feat_idx == 0x03 && func == 2 {
                    resp[2] = 0x03;
                    resp[3] = sw;
                    resp[4] = if dev_idx == 1 { 3 } else { 0 };
                } else {
                    resp[2] = 0xFF;
                }
            } else {
                resp[2] = 0xFF;
            }
            self.rx_queue.lock().unwrap().push_back(resp);
            Ok(())
        }
        fn read_input(&self, _timeout_ms: u32) -> Result<Vec<u8>, String> {
            self.rx_queue
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "empty".into())
        }
        fn get_feature(&self, _report_id: u8) -> Result<Vec<u8>, String> {
            Err("unused".into())
        }
        fn get_input_report(&self, _report_id: u8) -> Result<Vec<u8>, String> {
            Err("unused".into())
        }
        fn set_feature(&self, _data: &[u8]) -> Result<(), String> {
            Err("unused".into())
        }
    }

    #[test]
    fn test_hidpp_multi_slot_discovers_multiple_devices_on_one_receiver() {
        let stop = AtomicBool::new(false);
        let io = MultiSlotMockIo {
            info: mk_info(0x046D, 0xC52B, "RX01"),
            rx_queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
        };
        let res = probe_hidpp_multi_slot(&io, "Logitech Unifying", DeviceKind::Other, &stop);
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].0, 1);
        assert_eq!(res[0].1, "Mouse");
        assert_eq!(res[0].2, DeviceKind::Mouse);
        assert_eq!(res[0].3.battery_pct, Some(82));
        assert!(!res[0].3.charging);

        assert_eq!(res[1].0, 2);
        assert_eq!(res[1].1, "Keybd-X");
        assert_eq!(res[1].2, DeviceKind::Keyboard);
        assert_eq!(res[1].3.battery_pct, Some(45));
        assert!(res[1].3.charging);
    }

    #[test]
    fn test_normalize_hid_path_and_instance_fallback() {
        assert_eq!(
            normalize_hid_path("  \\\\?\\HID#VID_24AE&PID_1412#7&1234ABCD&0#  "),
            "\\\\?\\hid#vid_24ae&pid_1412#7&1234abcd&0#"
        );
        assert_eq!(
            extract_path_instance_fallback("\\\\?\\hid#vid_24ae&pid_1412#7&1234abcd&0#{guid}"),
            "7&1234ABCD&0"
        );
        assert_eq!(extract_path_instance_fallback("short"), "SHORT");
    }

    #[test]
    fn test_poll_candidate_sort_order_is_deterministic() {
        let now = Instant::now();
        let make_cand =
            |group: &str, deferred: u8, last: Option<Instant>, usage_prio: u8| PollCandidate {
                info: mk_info(0x24AE, 0x1412, ""),
                meta: super::super::protocols::DeviceMeta {
                    name: "Test".into(),
                    kind: DeviceKind::Mouse,
                    protocol: ProtocolFamily::Rapoo,
                    min_poll_secs: None,
                    allow_active_tx: false,
                },
                group_key: group.to_string(),
                is_custom_match: false,
                deferred_prio: deferred,
                last_attempt_rank: last,
                custom_prio: 1,
                vendor_prio: 0,
                usage_prio,
            };
        let mut cands = vec![
            make_cand("g_recent", 1, Some(now), 0),
            make_cand("g_deferred", 0, Some(now), 1),
            make_cand("g_never", 0, None, 1),
            make_cand("g_deferred", 0, Some(now), 0),
        ];
        sort_poll_candidates(&mut cands);
        assert_eq!(cands[0].group_key, "g_never");
        assert_eq!(cands[1].group_key, "g_deferred");
        assert_eq!(cands[1].usage_prio, 0);
        assert_eq!(cands[2].group_key, "g_deferred");
        assert_eq!(cands[2].usage_prio, 1);
        assert_eq!(cands[3].group_key, "g_recent");
    }

    struct StubProber {
        budget_exhausted: bool,
        budget_exhaust_after_calls: Option<usize>,
        responses: HashMap<String, Option<BatteryReading>>,
        hidpp_responses: HashMap<String, Option<Vec<HidppSlotReading>>>,
        hidpp_hit_deadline: bool,
        probe_calls: usize,
    }

    impl StubProber {
        fn new() -> Self {
            Self {
                budget_exhausted: false,
                budget_exhaust_after_calls: None,
                responses: HashMap::new(),
                hidpp_responses: HashMap::new(),
                hidpp_hit_deadline: false,
                probe_calls: 0,
            }
        }
    }

    impl CandidateProber for StubProber {
        fn is_budget_exhausted(&self) -> bool {
            self.budget_exhausted
                || self
                    .budget_exhaust_after_calls
                    .is_some_and(|limit| self.probe_calls >= limit)
        }

        fn probe_hidpp(
            &mut self,
            _monitor: &mut HidMonitor,
            info: &HidInterfaceInfo,
            _meta: &super::super::protocols::DeviceMeta,
        ) -> Option<HidppProbeOutcome> {
            self.probe_calls += 1;
            let key = format!("{:04X}:{:04X}", info.vid, info.pid);
            let slots = self
                .hidpp_responses
                .get(&key)
                .cloned()
                .unwrap_or(Some(vec![]))?;
            Some(HidppProbeOutcome {
                slots,
                hit_deadline: self.hidpp_hit_deadline,
                blocked_by_ro: false,
            })
        }

        fn probe_standard_or_vendor(
            &mut self,
            _monitor: &mut HidMonitor,
            _info: &HidInterfaceInfo,
            _meta: &super::super::protocols::DeviceMeta,
            group_key: &str,
        ) -> Option<BatteryReading> {
            self.probe_calls += 1;
            self.responses.get(group_key).cloned().unwrap_or(None)
        }
    }

    fn mk_candidate(
        vid: u16,
        pid: u16,
        name: &str,
        kind: DeviceKind,
        protocol: ProtocolFamily,
        group_key: &str,
        min_poll_secs: Option<u64>,
    ) -> PollCandidate {
        let mut info = mk_info(vid, pid, "SN1");
        info.usage_page = 0xFF00;
        info.usage = 0x0002;
        PollCandidate {
            info,
            meta: super::super::protocols::DeviceMeta {
                name: name.to_string(),
                kind,
                protocol,
                min_poll_secs,
                allow_active_tx: true,
            },
            group_key: group_key.to_string(),
            is_custom_match: false,
            deferred_prio: 0,
            last_attempt_rank: None,
            custom_prio: 1,
            vendor_prio: 0,
            usage_prio: 0,
        }
    }

    fn mk_monitor_for_test() -> HidMonitor {
        HidMonitor {
            registry: DeviceRegistry::from_toml_parts(include_str!("../../devices.toml"), &[]),
            interface_cache: HashMap::new(),
            persistent_handles: HashMap::new(),
            cached_readings: HashMap::new(),
            cached_group_devices: HashMap::new(),
            hidpp_slots: HashMap::new(),
            known_group_paths: HashMap::new(),
            poll_states: HashMap::new(),
            stop_flag: Arc::new(AtomicBool::new(false)),
            reclaimer: Arc::new(IoReclaimer::new(HID_RECLAIM_QUEUE_CAP)),
        }
    }

    #[test]
    fn test_hid_debounce_recovery_and_unplug_lifecycle() {
        let mut mon = mk_monitor_for_test();
        let gk = "hid:24AE:1412:SN1";
        let present: HashSet<String> = [gk.to_string()].into_iter().collect();
        let cand = || {
            vec![mk_candidate(
                0x24AE,
                0x1412,
                "Rapoo VT3",
                DeviceKind::Mouse,
                ProtocolFamily::Rapoo,
                gk,
                None,
            )]
        };

        let t0 = Instant::now();
        let mut prober = StubProber::new();
        prober.responses.insert(
            gk.to_string(),
            Some(BatteryReading {
                battery_pct: Some(67),
                charging: false,
                online: true,
                detail_note: Some("Rapoo Gen2 (54L)".into()),
                raw_packets: vec![],
            }),
        );
        let d0 = mon.poll_candidates_with(cand(), &present, t0, &mut prober);
        assert_eq!(d0.len(), 1);
        assert!(d0[0].online);
        assert_eq!(d0[0].battery_pct, Some(67));
        assert_eq!(mon.poll_states[gk].fail_streak, 0);

        // Fail 1 (at +50s): stays online = true (debounced)
        prober.responses.insert(gk.to_string(), None);
        let t1 = t0 + Duration::from_secs(50);
        let d1 = mon.poll_candidates_with(cand(), &present, t1, &mut prober);
        assert_eq!(d1.len(), 1);
        assert!(d1[0].online);
        assert_eq!(d1[0].battery_pct, Some(67));
        assert_eq!(mon.poll_states[gk].fail_streak, 1);

        // Throttled poll during 45s backoff (at +60s): skipped, must keep online = true!
        let t_skip = t1 + Duration::from_secs(10);
        let calls_before = prober.probe_calls;
        let d_skip = mon.poll_candidates_with(cand(), &present, t_skip, &mut prober);
        assert_eq!(prober.probe_calls, calls_before);
        assert!(d_skip[0].online);
        assert_eq!(d_skip[0].battery_pct, Some(67));

        // Fail 2 (after 45s backoff): still online = true (debounced)
        let t2 = t1 + Duration::from_secs(50);
        let d2 = mon.poll_candidates_with(cand(), &present, t2, &mut prober);
        assert!(d2[0].online);
        assert_eq!(mon.poll_states[gk].fail_streak, 2);

        // Recovery on next probe: fail_streak resets to 0, online = true, updated pct
        let t3 = t2 + Duration::from_secs(95);
        prober.responses.insert(
            gk.to_string(),
            Some(BatteryReading {
                battery_pct: Some(66),
                charging: false,
                online: true,
                detail_note: Some("Rapoo Gen2 (54L)".into()),
                raw_packets: vec![],
            }),
        );
        let d3 = mon.poll_candidates_with(cand(), &present, t3, &mut prober);
        assert!(d3[0].online);
        assert_eq!(d3[0].battery_pct, Some(66));
        assert_eq!(mon.poll_states[gk].fail_streak, 0);

        // 3 consecutive failures -> accurately transitions to online = false
        prober.responses.insert(gk.to_string(), None);
        let t_f1 = t3 + Duration::from_secs(50);
        let _ = mon.poll_candidates_with(cand(), &present, t_f1, &mut prober);
        let t_f2 = t_f1 + Duration::from_secs(50);
        let _ = mon.poll_candidates_with(cand(), &present, t_f2, &mut prober);
        let t_f3 = t_f2 + Duration::from_secs(95);
        let d_off = mon.poll_candidates_with(cand(), &present, t_f3, &mut prober);
        assert_eq!(mon.poll_states[gk].fail_streak, 3);
        assert!(!d_off[0].online);
        assert_eq!(d_off[0].battery_pct, Some(66));

        // Throttled poll while offline stays offline
        let d_off_skip = mon.poll_candidates_with(
            cand(),
            &present,
            t_f3 + Duration::from_secs(10),
            &mut prober,
        );
        assert!(!d_off_skip[0].online);

        // Physically unplugged -> disappears cleanly
        let empty_present: HashSet<String> = HashSet::new();
        let d_unplugged = mon.poll_candidates_with(
            vec![],
            &empty_present,
            t_f3 + Duration::from_secs(20),
            &mut prober,
        );
        assert!(d_unplugged.is_empty());
        assert!(!mon.poll_states.contains_key(gk));
    }

    #[test]
    fn test_budget_deferred_and_min_poll_preserve_online() {
        let mut mon = mk_monitor_for_test();
        // 046A:01A1 has min_poll_secs = 600 in devices.toml
        let gk_throttled = "hid:046A:01A1:SN1";
        let gk_deferred = "hid:24AE:1412:SN2";
        let present: HashSet<String> = [gk_throttled.to_string(), gk_deferred.to_string()]
            .into_iter()
            .collect();

        let make_cands = || {
            vec![
                mk_candidate(
                    0x046A,
                    0x01A1,
                    "Cherry Mouse",
                    DeviceKind::Mouse,
                    ProtocolFamily::CustomHid,
                    gk_throttled,
                    Some(600),
                ),
                mk_candidate(
                    0x24AE,
                    0x1412,
                    "Rapoo VT3",
                    DeviceKind::Mouse,
                    ProtocolFamily::Rapoo,
                    gk_deferred,
                    None,
                ),
            ]
        };

        let t0 = Instant::now();
        let mut prober = StubProber::new();
        prober.responses.insert(
            gk_throttled.to_string(),
            Some(BatteryReading {
                battery_pct: Some(80),
                charging: false,
                online: true,
                detail_note: Some("Custom HID".into()),
                raw_packets: vec![],
            }),
        );
        // Budget exhausted before probing gk_deferred on first poll
        prober.budget_exhausted = true;
        let d0 = mon.poll_candidates_with(make_cands(), &present, t0, &mut prober);
        assert_eq!(d0.len(), 2);
        // Both deferred devices (even never-probed present interface) must stay online = true
        assert!(
            d0.iter().all(|d| d.online),
            "expected all deferred devices online: {:?}",
            d0
        );

        // Next round: budget available, probe both
        prober.budget_exhausted = false;
        prober.responses.insert(
            gk_deferred.to_string(),
            Some(BatteryReading {
                battery_pct: Some(72),
                charging: false,
                online: true,
                detail_note: Some("Rapoo Gen2 (54L)".into()),
                raw_packets: vec![],
            }),
        );
        let t1 = t0 + Duration::from_secs(5);
        let d1 = mon.poll_candidates_with(make_cands(), &present, t1, &mut prober);
        assert_eq!(d1.len(), 2);
        assert!(d1.iter().all(|d| d.online));

        // Now throttle Cherry (min_poll_secs=600) AND exhaust budget on Rapoo: both must remain online = true
        prober.budget_exhausted = true;
        let t2 = t1 + Duration::from_secs(10);
        let d2 = mon.poll_candidates_with(make_cands(), &present, t2, &mut prober);
        assert_eq!(d2.len(), 2);
        assert!(
            d2.iter().all(|d| d.online),
            "throttled/deferred devices must stay online: {:?}",
            d2
        );
        assert_eq!(mon.poll_states[gk_throttled].fail_streak, 0);
        assert_eq!(mon.poll_states[gk_deferred].fail_streak, 0);
    }

    #[test]
    fn test_present_no_battery_device_stays_online_across_polls() {
        let mut mon = mk_monitor_for_test();
        let gk = "hid:362D:D101:c:SKN";
        let present: HashSet<String> = [gk.to_string()].into_iter().collect();
        let mut info = mk_info(0x362D, 0xD101, "");
        info.usage_page = 0xFF60;
        info.usage = 0x0061;
        info.input_len = 33;
        info.output_len = 33;

        let make_cands = || {
            vec![PollCandidate {
                info: info.clone(),
                meta: super::super::protocols::DeviceMeta {
                    name: "SKN 青龙 4.0".into(),
                    kind: DeviceKind::Keyboard,
                    protocol: ProtocolFamily::Sonix,
                    min_poll_secs: None,
                    allow_active_tx: true,
                },
                group_key: gk.to_string(),
                is_custom_match: false,
                deferred_prio: 0,
                last_attempt_rank: None,
                custom_prio: 1,
                vendor_prio: 0,
                usage_prio: 0,
            }]
        };

        let mut prober = StubProber::new();
        // Simulates SKN 青龙 4.0: raw HID responded 0xFF (unhandled), battery_pct = None, online = true
        prober.responses.insert(
            gk.to_string(),
            Some(BatteryReading {
                battery_pct: None,
                charging: false,
                online: true,
                detail_note: Some("SONiX/QMK Raw HID (raw captured)".into()),
                raw_packets: vec![],
            }),
        );

        let t0 = Instant::now();
        let d0 = mon.poll_candidates_with(make_cands(), &present, t0, &mut prober);
        assert_eq!(d0.len(), 1);
        assert_eq!(d0[0].name, "SKN 青龙 4.0");
        assert_eq!(d0[0].battery_pct, None);
        assert!(
            d0[0].online,
            "SKN Qinglong 4.0 must not be marked offline on poll 1"
        );

        // Second poll within backoff window (previously triggered the standby online=false bug!)
        let t1 = t0 + Duration::from_secs(2);
        let d1 = mon.poll_candidates_with(make_cands(), &present, t1, &mut prober);
        assert_eq!(d1.len(), 1);
        assert_eq!(d1[0].battery_pct, None);
        assert!(
            d1[0].online,
            "SKN Qinglong 4.0 must stay online during backoff"
        );

        // Even after >=3 poll attempts returning None (unsupported battery), present interface stays online
        prober.responses.insert(gk.to_string(), None);
        let t2 = t1 + Duration::from_secs(50);
        let d2 = mon.poll_candidates_with(make_cands(), &present, t2, &mut prober);
        let t3 = t2 + Duration::from_secs(95);
        let d3 = mon.poll_candidates_with(make_cands(), &present, t3, &mut prober);
        let t4 = t3 + Duration::from_secs(185);
        let d4 = mon.poll_candidates_with(make_cands(), &present, t4, &mut prober);
        assert!(d2[0].online && d3[0].online && d4[0].online);
    }

    #[test]
    fn test_hidpp_multi_slot_debounces_transient_failures() {
        let mut mon = mk_monitor_for_test();
        let gk = "hid:046D:C52B:RX01";
        let present: HashSet<String> = [gk.to_string()].into_iter().collect();
        let make_cands = || {
            vec![mk_candidate(
                0x046D,
                0xC52B,
                "Logitech Unifying",
                DeviceKind::Mouse,
                ProtocolFamily::Hidpp,
                gk,
                None,
            )]
        };

        let mut prober = StubProber::new();
        prober.hidpp_responses.insert(
            "046D:C52B".into(),
            Some(vec![(
                1,
                "MX Master 3".into(),
                DeviceKind::Mouse,
                BatteryReading {
                    battery_pct: Some(85),
                    charging: false,
                    online: true,
                    detail_note: Some("HID++ 2.0 UnifiedBattery (slot 1)".into()),
                    raw_packets: vec![],
                },
            )]),
        );

        let t0 = Instant::now();
        let d0 = mon.poll_candidates_with(make_cands(), &present, t0, &mut prober);
        assert_eq!(d0.len(), 1);
        assert!(d0[0].online);
        assert_eq!(d0[0].battery_pct, Some(85));

        // 2 transient empty responses -> stays online
        prober
            .hidpp_responses
            .insert("046D:C52B".into(), Some(vec![]));
        let t1 = t0 + Duration::from_secs(50);
        let d1 = mon.poll_candidates_with(make_cands(), &present, t1, &mut prober);
        assert!(d1[0].online);
        let t2 = t1 + Duration::from_secs(50);
        let d2 = mon.poll_candidates_with(make_cands(), &present, t2, &mut prober);
        assert!(d2[0].online);

        // 3rd consecutive failure -> transitions to offline
        let t3 = t2 + Duration::from_secs(95);
        let d3 = mon.poll_candidates_with(make_cands(), &present, t3, &mut prober);
        assert!(!d3[0].online);
        assert_eq!(d3[0].battery_pct, Some(85));
    }

    #[test]
    fn test_passive_rapoo_sleep_caps_backoff_and_force_refresh_bypasses_cooldown() {
        let mut mon = mk_monitor_for_test();
        let gk = "hid:24AE:1412:2607-54L";
        let present: HashSet<String> = [gk.to_string()].into_iter().collect();
        let make_cands = || {
            vec![mk_candidate(
                0x24AE,
                0x1412,
                "Rapoo VT3",
                DeviceKind::Mouse,
                ProtocolFamily::Rapoo,
                gk,
                None,
            )]
        };

        let mut prober = StubProber::new();
        prober.responses.insert(
            gk.to_string(),
            Some(BatteryReading {
                battery_pct: Some(65),
                charging: false,
                online: true,
                detail_note: Some("Rapoo Gen2 (54L)".into()),
                raw_packets: vec![],
            }),
        );

        let t0 = Instant::now();
        let d0 = mon.poll_candidates_with(make_cands(), &present, t0, &mut prober);
        assert_eq!(d0.len(), 1);
        assert!(d0[0].online);
        assert_eq!(d0[0].battery_pct, Some(65));

        // Overnight sleep: > 30 min cache expiry and 6 consecutive failures.
        prober.responses.insert(gk.to_string(), None);
        let mut t = t0 + Duration::from_secs(HID_CACHE_GRACE_SECS + 60);
        for _ in 0..6 {
            let d = mon.poll_candidates_with(make_cands(), &present, t, &mut prober);
            assert_eq!(d.len(), 1);
            let cooldown = mon.poll_states[gk]
                .next_probe_after
                .duration_since(t)
                .as_secs();
            assert_eq!(
                cooldown, HID_PASSIVE_MAX_BACKOFF_SECS,
                "passive Rapoo cooldown must stay capped at 45s instead of 600s"
            );
            t += Duration::from_secs(HID_PASSIVE_MAX_BACKOFF_SECS + 1);
        }

        // During cooldown (+5s), normal poll skips probing and reports offline standby without redundant "(cached)".
        let t_cool = t - Duration::from_secs(HID_PASSIVE_MAX_BACKOFF_SECS) + Duration::from_secs(5);
        let calls_before = prober.probe_calls;
        let d_skip = mon.poll_candidates_with(make_cands(), &present, t_cool, &mut prober);
        assert_eq!(prober.probe_calls, calls_before);
        assert!(!d_skip[0].online);
        assert_eq!(d_skip[0].battery_pct, None);
        assert!(
            !d_skip[0]
                .detail_note
                .as_deref()
                .unwrap_or("")
                .contains("(cached)"),
            "standby entry without cached percentage should not append (cached): {:?}",
            d_skip[0].detail_note
        );

        // Mouse wakes up; explicit force refresh bypasses cooldown and recovers immediately.
        prober.responses.insert(
            gk.to_string(),
            Some(BatteryReading {
                battery_pct: Some(63),
                charging: false,
                online: true,
                detail_note: Some("Rapoo Gen2 (54L)".into()),
                raw_packets: vec![],
            }),
        );
        let d_force = mon.poll_candidates_with_force(
            make_cands(),
            &present,
            t_cool + Duration::from_secs(1),
            true,
            &mut prober,
        );
        assert_eq!(prober.probe_calls, calls_before + 1);
        assert!(d_force[0].online);
        assert_eq!(d_force[0].battery_pct, Some(63));
        assert_eq!(mon.poll_states[gk].fail_streak, 0);
    }

    #[test]
    fn test_active_device_force_refresh_bypasses_long_backoff() {
        let mut mon = mk_monitor_for_test();
        let gk = "hid:260D:1175:SN9";
        let present: HashSet<String> = [gk.to_string()].into_iter().collect();
        let make_cands = || {
            vec![mk_candidate(
                0x260D,
                0x1175,
                "Dareu A950 Air",
                DeviceKind::Mouse,
                ProtocolFamily::Compx,
                gk,
                None,
            )]
        };

        let mut prober = StubProber::new();
        prober.responses.insert(
            gk.to_string(),
            Some(BatteryReading {
                battery_pct: Some(80),
                charging: false,
                online: true,
                detail_note: Some("CompX".into()),
                raw_packets: vec![],
            }),
        );

        let t0 = Instant::now();
        let _ = mon.poll_candidates_with(make_cands(), &present, t0, &mut prober);

        // 3 consecutive failures push active Compx device into 180s backoff.
        prober.responses.insert(gk.to_string(), None);
        let t1 = t0 + Duration::from_secs(50);
        let _ = mon.poll_candidates_with(make_cands(), &present, t1, &mut prober);
        let t2 = t1 + Duration::from_secs(50);
        let _ = mon.poll_candidates_with(make_cands(), &present, t2, &mut prober);
        let t3 = t2 + Duration::from_secs(95);
        let d_off = mon.poll_candidates_with(make_cands(), &present, t3, &mut prober);
        assert!(!d_off[0].online);
        assert_eq!(
            mon.poll_states[gk]
                .next_probe_after
                .duration_since(t3)
                .as_secs(),
            180
        );

        // Woken 10s later: manual force refresh bypasses the 180s backoff.
        prober.responses.insert(
            gk.to_string(),
            Some(BatteryReading {
                battery_pct: Some(79),
                charging: false,
                online: true,
                detail_note: Some("CompX".into()),
                raw_packets: vec![],
            }),
        );
        let d_rec = mon.poll_candidates_with_force(
            make_cands(),
            &present,
            t3 + Duration::from_secs(10),
            true,
            &mut prober,
        );
        assert!(d_rec[0].online);
        assert_eq!(d_rec[0].battery_pct, Some(79));
        assert_eq!(mon.poll_states[gk].fail_streak, 0);
    }

    #[test]
    fn test_hidpp_guard_blocks_external_device_override_and_unknown_pid() {
        struct RecordingHidppIo {
            info: HidInterfaceInfo,
            writes: std::cell::RefCell<Vec<Vec<u8>>>,
        }
        impl HidIo for RecordingHidppIo {
            fn info(&self) -> &HidInterfaceInfo {
                &self.info
            }
            fn can_rw(&self) -> bool {
                true
            }
            fn write_output(&self, data: &[u8]) -> Result<(), String> {
                self.writes.borrow_mut().push(data.to_vec());
                Ok(())
            }
            fn read_input(&self, _timeout_ms: u32) -> Result<Vec<u8>, String> {
                Err("timeout".into())
            }
            fn get_feature(&self, _report_id: u8) -> Result<Vec<u8>, String> {
                Err("unsupported".into())
            }
            fn get_input_report(&self, _report_id: u8) -> Result<Vec<u8>, String> {
                Err("unsupported".into())
            }
            fn set_feature(&self, _data: &[u8]) -> Result<(), String> {
                Ok(())
            }
        }

        let builtin = include_str!("../../devices.toml");
        let ext = r#"
[[device]]
vid = "046D"
pid = "9999"
name = "External Unverified Logitech"
kind = "mouse"
protocol = "hidpp"
"#;
        let reg = DeviceRegistry::from_toml_parts(builtin, &[ext]);
        let stop = AtomicBool::new(false);

        let io_ext = RecordingHidppIo {
            info: mk_info(0x046D, 0x9999, "EXT"),
            writes: std::cell::RefCell::new(Vec::new()),
        };
        let outcome =
            probe_hidpp_guarded(&reg, &io_ext, "External", DeviceKind::Mouse, &stop, None);
        assert!(outcome.blocked_by_ro);
        assert!(outcome.slots.is_empty());
        assert!(io_ext.writes.borrow().is_empty());

        let io_unknown = RecordingHidppIo {
            info: mk_info(0x9999, 0x8888, "UNK"),
            writes: std::cell::RefCell::new(Vec::new()),
        };
        let outcome_unk =
            probe_hidpp_guarded(&reg, &io_unknown, "Unknown", DeviceKind::Mouse, &stop, None);
        assert!(outcome_unk.blocked_by_ro);
        assert!(io_unknown.writes.borrow().is_empty());
    }

    #[test]
    fn test_overlapped_cancel_delayed_completion_reclaim_cap_stop_race_and_quarantine() {
        struct MockPending {
            completed: Arc<AtomicBool>,
            dropped: Arc<AtomicBool>,
        }
        impl ReclaimableIo for MockPending {
            fn is_completed(&mut self) -> bool {
                self.completed.load(Ordering::Acquire)
            }
        }
        impl Drop for MockPending {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::Release);
            }
        }

        struct FakeWaitOps {
            slice_res: WaitSliceResult,
            cancel_called: bool,
            completed: bool,
        }
        impl OverlappedOps for FakeWaitOps {
            fn wait_slice(&mut self, _timeout_ms: u32) -> WaitSliceResult {
                self.slice_res
            }
            fn cancel_io(&mut self) {
                self.cancel_called = true;
            }
            fn poll_completed(&mut self) -> bool {
                self.completed
            }
            fn read_result(&mut self) -> Result<u32, String> {
                Ok(0)
            }
        }

        let reclaimer = Arc::new(IoReclaimer::new(3));
        let stop = AtomicBool::new(false);

        // 1. Timeout + driver fails to cancel within 80ms -> returns DeferredPending immediately
        let mut ops1 = FakeWaitOps {
            slice_res: WaitSliceResult::Timeout,
            cancel_called: false,
            completed: false,
        };
        let err = drive_overlapped_wait(&stop, 100, 80, "ReadFile", &mut ops1).unwrap_err();
        assert!(ops1.cancel_called);
        assert!(matches!(err, OverlappedDriveError::DeferredPending(_)));

        // Hand off delayed request & quarantine faulty device path while healthy path stays active
        let t0 = Instant::now();
        let faulty_path = normalize_hid_path(r"\\?\hid#vid_1111&pid_2222");
        let healthy_path = normalize_hid_path(r"\\?\hid#vid_3333&pid_4444");
        reclaimer.quarantine_path(&faulty_path, t0, Duration::from_secs(120));
        assert!(reclaimer.is_quarantined(&faulty_path, t0 + Duration::from_secs(30)));
        assert!(!reclaimer.is_quarantined(&healthy_path, t0 + Duration::from_secs(30)));
        assert!(!reclaimer.is_quarantined(&faulty_path, t0 + Duration::from_secs(121)));

        let c1 = Arc::new(AtomicBool::new(false));
        let d1 = Arc::new(AtomicBool::new(false));
        reclaimer.handoff(
            Box::new(MockPending {
                completed: Arc::clone(&c1),
                dropped: Arc::clone(&d1),
            }),
            false,
        );
        assert_eq!(reclaimer.pending_len(), 1);
        assert!(!d1.load(Ordering::Acquire));

        // Delayed completion arrives -> reaped and freed cleanly
        c1.store(true, Ordering::Release);
        assert_eq!(reclaimer.reap_completed(), 1);
        assert_eq!(reclaimer.pending_len(), 0);
        assert!(d1.load(Ordering::Acquire));

        // 2. Bounded reclaim queue cap (cap = 3)
        let mut flags = Vec::new();
        for _ in 0..5 {
            let c = Arc::new(AtomicBool::new(false));
            let d = Arc::new(AtomicBool::new(false));
            reclaimer.handoff(
                Box::new(MockPending {
                    completed: Arc::clone(&c),
                    dropped: Arc::clone(&d),
                }),
                false,
            );
            flags.push((c, d));
        }
        assert!(reclaimer.pending_len() <= 3);
        assert!(reclaimer.is_saturated());
        let (_, reclaimed, evicted) = reclaimer.stats();
        assert_eq!(reclaimed, 1);
        assert_eq!(evicted, 2);

        // Complete remaining -> saturation clears
        for (c, _) in &flags {
            c.store(true, Ordering::Release);
        }
        assert!(!reclaimer.is_saturated());
        assert_eq!(reclaimer.pending_len(), 0);

        // 3. Stop-flag race where driver completes right during cancel wait -> CompletedErr (no handoff)
        stop.store(true, Ordering::Relaxed);
        let mut ops_race = FakeWaitOps {
            slice_res: WaitSliceResult::Signaled,
            cancel_called: false,
            completed: true,
        };
        let race_err =
            drive_overlapped_wait(&stop, 300, 80, "ReadFile", &mut ops_race).unwrap_err();
        assert!(ops_race.cancel_called);
        assert!(matches!(race_err, OverlappedDriveError::CompletedErr(_)));
    }

    #[test]
    fn test_hidpp_per_slot_cache_keeps_sleeping_sibling_when_other_slot_online() {
        let mut mon = mk_monitor_for_test();
        let gk = "hid:046D:C52B:RX_TWO";
        let present: HashSet<String> = [gk.to_string()].into_iter().collect();
        let make_cands = || {
            vec![mk_candidate(
                0x046D,
                0xC52B,
                "Logitech Unifying",
                DeviceKind::Other,
                ProtocolFamily::Hidpp,
                gk,
                None,
            )]
        };

        let mut prober = StubProber::new();
        prober.hidpp_responses.insert(
            "046D:C52B".into(),
            Some(vec![
                (
                    1,
                    "MX Master 3".into(),
                    DeviceKind::Mouse,
                    BatteryReading {
                        battery_pct: Some(85),
                        charging: false,
                        online: true,
                        detail_note: Some("HID++ 2.0 UnifiedBattery (slot 1)".into()),
                        raw_packets: vec![],
                    },
                ),
                (
                    2,
                    "MX Keys".into(),
                    DeviceKind::Keyboard,
                    BatteryReading {
                        battery_pct: Some(60),
                        charging: false,
                        online: true,
                        detail_note: Some("HID++ 2.0 UnifiedBattery (slot 2)".into()),
                        raw_packets: vec![],
                    },
                ),
            ]),
        );

        let t0 = Instant::now();
        let d0 = mon.poll_candidates_with(make_cands(), &present, t0, &mut prober);
        assert_eq!(d0.len(), 2);

        // Next round: Slot 1 responds (84%), Slot 2 is asleep (absent from slot_results).
        // Slot 2 MUST NOT disappear! It stays online during debounce (<3 misses) with cached 60%.
        prober.hidpp_responses.insert(
            "046D:C52B".into(),
            Some(vec![(
                1,
                "MX Master 3".into(),
                DeviceKind::Mouse,
                BatteryReading {
                    battery_pct: Some(84),
                    charging: false,
                    online: true,
                    detail_note: Some("HID++ 2.0 UnifiedBattery (slot 1)".into()),
                    raw_packets: vec![],
                },
            )]),
        );
        let t1 = t0 + Duration::from_secs(50);
        let d1 = mon.poll_candidates_with(make_cands(), &present, t1, &mut prober);
        assert_eq!(d1.len(), 2);
        let slot1 = d1.iter().find(|d| d.name == "MX Master 3").unwrap();
        let slot2 = d1.iter().find(|d| d.name == "MX Keys").unwrap();
        assert!(slot1.online && slot1.battery_pct == Some(84));
        assert!(slot2.online && slot2.battery_pct == Some(60));

        // 2 more misses on Slot 2 -> Slot 2 transitions to offline (cached) while Slot 1 stays online
        let t2 = t1 + Duration::from_secs(50);
        let _ = mon.poll_candidates_with(make_cands(), &present, t2, &mut prober);
        let t3 = t2 + Duration::from_secs(50);
        let d3 = mon.poll_candidates_with(make_cands(), &present, t3, &mut prober);
        assert_eq!(d3.len(), 2);
        let slot1_t3 = d3.iter().find(|d| d.name == "MX Master 3").unwrap();
        let slot2_t3 = d3.iter().find(|d| d.name == "MX Keys").unwrap();
        assert!(slot1_t3.online);
        assert!(!slot2_t3.online);
        assert_eq!(slot2_t3.battery_pct, Some(60));
        assert!(slot2_t3
            .detail_note
            .as_deref()
            .unwrap_or("")
            .contains("(cached)"));
    }

    #[test]
    fn test_device_change_poll_protects_min_poll_sleeping_devices_and_mid_group_budget() {
        let mut mon = mk_monitor_for_test();
        let gk_cherry = "hid:046A:01A1:CHERRY";
        let gk_new = "hid:24AE:1412:RAPOO_NEW";
        let gk_multi = "hid:260D:1175:MULTI_IF";

        // 1. Mid-group budget exhaustion stops before 2nd interface of same group and marks deferred
        let mut c1 = mk_candidate(
            0x260D,
            0x1175,
            "Multi",
            DeviceKind::Mouse,
            ProtocolFamily::Compx,
            gk_multi,
            None,
        );
        c1.info.path = "p1".into();
        let mut c2 = c1.clone();
        c2.info.path = "p2".into();
        c2.usage_prio = 1;
        let present1: HashSet<String> = [gk_multi.to_string()].into_iter().collect();
        let mut prober = StubProber::new();
        prober.budget_exhaust_after_calls = Some(1);
        let t0 = Instant::now();
        let _ = mon.poll_candidates_with(vec![c1, c2], &present1, t0, &mut prober);
        assert_eq!(
            prober.probe_calls, 1,
            "budget must stop 2nd interface in same group"
        );
        assert!(mon.poll_states[gk_multi].deferred);

        // 2. DeviceChange poll probes newly added device while respecting unchanged min_poll_secs=600 device
        let mut cherry_cand = mk_candidate(
            0x046A,
            0x01A1,
            "Cherry Protected",
            DeviceKind::Keyboard,
            ProtocolFamily::CustomHid,
            gk_cherry,
            Some(600),
        );
        cherry_cand.info.path = r"\\?\hid#cherry#mi_00".into();

        let mut new_cand = mk_candidate(
            0x24AE,
            0x1412,
            "New Mouse",
            DeviceKind::Mouse,
            ProtocolFamily::Rapoo,
            gk_new,
            None,
        );
        new_cand.info.path = r"\\?\hid#rapoo#mi_01".into();

        let mut prober2 = StubProber::new();
        prober2.responses.insert(
            gk_cherry.to_string(),
            Some(BatteryReading {
                battery_pct: Some(90),
                charging: false,
                online: true,
                detail_note: Some("Cherry".into()),
                raw_packets: vec![],
            }),
        );
        let present_cherry: HashSet<String> = [gk_cherry.to_string()].into_iter().collect();
        let _ = mon.poll_candidates_with_mode(
            vec![cherry_cand.clone()],
            &present_cherry,
            true,
            t0,
            HidPollMode::Normal,
            &mut prober2,
        );
        assert_eq!(prober2.probe_calls, 1);

        // 10s later: new_cand arrives and triggers DeviceChange poll -> ONLY new_cand is probed,
        // Cherry (min_poll_secs=600, unchanged path) is NOT disturbed!
        prober2.responses.insert(
            gk_new.to_string(),
            Some(BatteryReading {
                battery_pct: Some(77),
                charging: false,
                online: true,
                detail_note: Some("Rapoo".into()),
                raw_packets: vec![],
            }),
        );
        let present_both: HashSet<String> = [gk_cherry.to_string(), gk_new.to_string()]
            .into_iter()
            .collect();
        let d_dc = mon.poll_candidates_with_mode(
            vec![cherry_cand, new_cand],
            &present_both,
            true,
            t0 + Duration::from_secs(10),
            HidPollMode::DeviceChange,
            &mut prober2,
        );
        assert_eq!(
            prober2.probe_calls, 2,
            "only new device should be probed on DeviceChange"
        );
        assert_eq!(d_dc.len(), 2);
    }

    #[test]
    fn test_hidpp_multi_slot_stops_at_deadline_and_marks_hit_deadline() {
        let stop = AtomicBool::new(false);
        let io = MultiSlotMockIo {
            info: mk_info(0x046D, 0xC52B, "RX01"),
            rx_queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
        };
        let expired = Instant::now() - Duration::from_millis(1);
        let (res, hit_deadline) = probe_hidpp_multi_slot_with_deadline(
            &io,
            "Logitech Unifying",
            DeviceKind::Other,
            &stop,
            Some(expired),
        );
        assert!(res.is_empty());
        assert!(hit_deadline);
    }

    #[test]
    fn test_hid_diagnostics_redacts_serial_by_default_and_shows_when_verbose() {
        let mut info = mk_info(0x24AE, 0x1412, "SECRET_SN_987654");
        info.manufacturer = "Rapoo".into();
        info.product = "VT3".into();

        let redacted = format_hid_diag_line(&info, "Rapoo VT3", "Rapoo", false);
        assert!(!redacted.contains("SECRET_SN_987654"));
        assert!(redacted.contains("<redacted:16>"));

        let verbose = format_hid_diag_line(&info, "Rapoo VT3", "Rapoo", true);
        assert!(verbose.contains("SECRET_SN_987654"));
    }

    #[test]
    fn test_hid_monitor_drop_does_not_mutate_caller_stop_flag_and_shuts_down_reclaimer() {
        let stop = Arc::new(AtomicBool::new(false));
        let reclaimer_ref;
        {
            let mut mon = mk_monitor_for_test();
            mon.stop_flag = Arc::clone(&stop);
            reclaimer_ref = Arc::clone(&mon.reclaimer);
            assert!(!reclaimer_ref.shutdown.load(Ordering::Acquire));
        }
        assert!(reclaimer_ref.shutdown.load(Ordering::Acquire));
        assert!(
            !stop.load(Ordering::Relaxed),
            "HidMonitor::drop must not mutate caller stop_flag"
        );
    }
}
