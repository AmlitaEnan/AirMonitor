pub mod popup;
pub mod renderer;
pub mod taskbar;
pub mod text;
pub mod theme;
pub mod tray;

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::size_of;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, FreeLibrary, GetLastError, ERROR_ALREADY_EXISTS, FILETIME, HANDLE, HINSTANCE,
    HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetProcessTimes, SetProcessWorkingSetSize,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIM_DELETE, NIN_SELECT, NOTIFYICONDATAW};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    GetWindowLongPtrW, IsWindow, KillTimer, LoadCursorW, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterDeviceNotificationW, RegisterWindowMessageW, SetTimer,
    SetWindowLongPtrW, TranslateMessage, UnregisterDeviceNotification, CS_HREDRAW, CS_VREDRAW,
    DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVNODES_CHANGED, DBT_DEVTYP_DEVICEINTERFACE,
    DEVICE_NOTIFY_ALL_INTERFACE_CLASSES, DEVICE_NOTIFY_WINDOW_HANDLE,
    DEV_BROADCAST_DEVICEINTERFACE_W, GWLP_USERDATA, HDEVNOTIFY, IDC_ARROW, MA_NOACTIVATE, MSG,
    WM_APP, WM_CAPTURECHANGED, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_DEVICECHANGE,
    WM_DPICHANGED, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_RBUTTONUP,
    WM_SETTINGCHANGE, WM_TIMER, WM_USER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::config::{AnchorEdge, AppConfig, LayoutMode};
use crate::hw::{HardwareCollector, HardwareDemand};
use crate::state::{HardwareMetrics, MonitorSnapshot, WirelessDevice};
use crate::wireless::WirelessManager;
use popup::layout::PopupTab;
use popup::{
    register_popup_class, PopupManager, WM_APP_CONFIG_CHANGED, WM_APP_REFRESH_WIRELESS,
    WM_APP_UPDATE_RESTART, WM_APP_UPDATE_STATUS,
};
use taskbar::{find_taskbar_geometry, DragState, TaskbarManager};
use tray::{MenuAction, TrayManager, NIN_KEYSELECT, WM_APP_TRAY_CALLBACK};

const WM_APP_HW_UPDATE: u32 = WM_APP + 1;
const WM_APP_WIRELESS_UPDATE: u32 = WM_APP + 2;
const WM_APP_DEFERRED_SETTINGCHANGE: u32 = WM_APP + 10;
const WM_APP_DEFERRED_REFRESH_BARS: u32 = WM_APP + 11;
const WM_APP_DEFERRED_ZORDER_CHECK: u32 = WM_APP + 12;
const WM_APP_DEFERRED_SMOKE_TEST: u32 = WM_APP + 13;
const WM_APP_DEFERRED_QUIT: u32 = WM_APP + 14;

// Win32 MK_CONTROL modifier flag in mouse message wParam.
const MK_CONTROL: usize = 0x0008;

static WM_TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

const TIMER_ZORDER_CHECK: usize = 1;
const TIMER_DEVICE_DEBOUNCE: usize = 2;
const TIMER_SMOKE_TEST: usize = 3;
const TIMER_STARTUP_TRIM: usize = 4;
const TIMER_GEOM_SETTLE: usize = 5;

const ZORDER_TICK_MS: u32 = 500;
const GEOM_SETTLE_TICK_MS: u32 = 180;
const DEVICE_DEBOUNCE_MS: u32 = 800;
const STARTUP_TRIM_MS: u32 = 1500;
const SMOKE_STEADY_MS: u32 = 4000;

/// Immutable snapshot paired with per-subsystem revision numbers so UI views
/// only redraw when their own inputs actually change.
#[derive(Clone)]
pub(crate) struct VersionedSnapshot {
    pub snapshot: Arc<MonitorSnapshot>,
    pub hw_rev: u64,
    pub wireless_rev: u64,
    pub config_rev: u64,
}

/// Lock-light shared snapshot store: writers construct a new `Arc<MonitorSnapshot>`
/// under a brief write lock and never perform IO or rendering inside any lock;
/// readers clone the `Arc` and immediately release the read lock.
#[derive(Clone)]
pub(crate) struct SnapshotStore {
    inner: Arc<RwLock<VersionedSnapshot>>,
}

impl SnapshotStore {
    pub fn new(config: AppConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(VersionedSnapshot {
                snapshot: Arc::new(MonitorSnapshot::new(config)),
                hw_rev: 0,
                wireless_rev: 0,
                config_rev: 0,
            })),
        }
    }

    pub fn read_versioned(&self) -> Option<VersionedSnapshot> {
        self.inner.read().ok().map(|guard| guard.clone())
    }

    pub fn read_snapshot(&self) -> Option<Arc<MonitorSnapshot>> {
        self.inner
            .read()
            .ok()
            .map(|guard| Arc::clone(&guard.snapshot))
    }

    pub fn update_hw(&self, hw: HardwareMetrics) {
        if let Ok(mut guard) = self.inner.write() {
            let cur = &guard.snapshot;
            guard.snapshot = Arc::new(MonitorSnapshot {
                hw,
                devices: cur.devices.clone(),
                config: cur.config.clone(),
            });
            guard.hw_rev = guard.hw_rev.wrapping_add(1);
        }
    }

    pub fn update_devices(&self, devices: Vec<WirelessDevice>) {
        if let Ok(mut guard) = self.inner.write() {
            let cur = &guard.snapshot;
            guard.snapshot = Arc::new(MonitorSnapshot {
                hw: cur.hw.clone(),
                devices,
                config: cur.config.clone(),
            });
            guard.wireless_rev = guard.wireless_rev.wrapping_add(1);
        }
    }

    /// Updates `AppConfig` purely in memory under the write lock and returns the
    /// resulting config so callers can persist it outside any lock.
    pub fn update_config<F>(&self, f: F) -> Option<AppConfig>
    where
        F: FnOnce(&mut AppConfig) -> bool,
    {
        let mut updated = None;
        if let Ok(mut guard) = self.inner.write() {
            let cur = &guard.snapshot;
            let mut next_cfg = cur.config.clone();
            if f(&mut next_cfg) {
                guard.snapshot = Arc::new(MonitorSnapshot {
                    hw: cur.hw.clone(),
                    devices: cur.devices.clone(),
                    config: next_cfg.clone(),
                });
                guard.config_rev = guard.config_rev.wrapping_add(1);
                updated = Some(next_cfg);
            }
        }
        updated
    }

    /// Updates `AppConfig` under the write lock, drops the lock, and then saves
    /// `config.toml` to disk outside any lock.
    pub fn update_and_save_config<F>(&self, f: F) -> (bool, Option<String>)
    where
        F: FnOnce(&mut AppConfig) -> bool,
    {
        if let Some(cfg) = self.update_config(f) {
            (true, cfg.save().err())
        } else {
            (false, None)
        }
    }
}

pub(crate) fn trim_working_set() {
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

#[derive(Default)]
struct HwWorkerSignal {
    stop: bool,
    wake: bool,
}

#[derive(Clone)]
pub(crate) struct HwWakeSignal(Arc<(Mutex<HwWorkerSignal>, Condvar)>);

impl HwWakeSignal {
    pub fn new() -> Self {
        Self(Arc::new((
            Mutex::new(HwWorkerSignal::default()),
            Condvar::new(),
        )))
    }

    pub fn wake(&self) {
        let (lock, cvar) = &*self.0;
        if let Ok(mut sig) = lock.lock() {
            sig.wake = true;
            cvar.notify_all();
        }
    }

    pub fn stop(&self) {
        let (lock, cvar) = &*self.0;
        if let Ok(mut sig) = lock.lock() {
            sig.stop = true;
            cvar.notify_all();
        }
    }

    /// Waits up to `dur` for a wake or stop signal.
    /// Returns `true` if the worker should continue running, or `false` if stopped.
    pub fn wait_timeout(&self, dur: Duration) -> bool {
        let (lock, cvar) = &*self.0;
        let mut sig = lock.lock().unwrap_or_else(|e| e.into_inner());
        if sig.stop {
            return false;
        }
        if !sig.wake && !dur.is_zero() {
            let deadline = Instant::now() + dur;
            while !sig.stop && !sig.wake {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let (guard, wait_res) = cvar
                    .wait_timeout(sig, deadline.saturating_duration_since(now))
                    .unwrap_or_else(|e| e.into_inner());
                sig = guard;
                if wait_res.timed_out() {
                    break;
                }
            }
        }
        if sig.stop {
            return false;
        }
        sig.wake = false;
        true
    }
}

struct SingleInstanceGuard(HANDLE);

impl SingleInstanceGuard {
    fn acquire() -> Option<Self> {
        unsafe {
            let handle =
                CreateMutexW(None, false, w!("Local\\AirMonitor_SingleInstance_v1")).ok()?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                return None;
            }
            Some(Self(handle))
        }
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_invalid() {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct ComApartmentGuard;

impl ComApartmentGuard {
    fn init() -> Self {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        }
        Self
    }
}

impl Drop for ComApartmentGuard {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

struct UiApp {
    state: SnapshotStore,
    hinstance: HINSTANCE,
    host_hwnd: HWND,
    self_cell_ptr: isize,
    taskbar: TaskbarManager,
    tray: TrayManager,
    popup: Rc<RefCell<PopupManager>>,
    hw_signal: HwWakeSignal,
    wireless_wake: Arc<(Mutex<bool>, Condvar)>,
    stop_flag: Arc<AtomicBool>,
    hdevnotify: HDEVNOTIFY,
    smoke_test: bool,
    smoke_steady_start: Option<(Instant, u64)>,
    smoke_report: Arc<Mutex<Option<String>>>,
    geom_settle_timer_active: bool,
}

impl UiApp {
    fn ensure_bars_alive(&mut self) {
        if self.taskbar.bars_alive() {
            return;
        }
        let self_ptr = self.self_cell_ptr;
        unsafe {
            if self.taskbar.primary_hwnd.0.is_null()
                || !IsWindow(self.taskbar.primary_hwnd).as_bool()
            {
                if let Ok(h) = create_bar_window(self.hinstance, w!("AirMonitorPrimary")) {
                    SetWindowLongPtrW(h, GWLP_USERDATA, self_ptr);
                    self.taskbar.primary_hwnd = h;
                    self.taskbar.primary_surface.clear();
                    self.taskbar.invalidate_primary();
                }
            }
            if self.taskbar.secondary_hwnd.0.is_null()
                || !IsWindow(self.taskbar.secondary_hwnd).as_bool()
            {
                if let Ok(h) = create_bar_window(self.hinstance, w!("AirMonitorDevices")) {
                    SetWindowLongPtrW(h, GWLP_USERDATA, self_ptr);
                    self.taskbar.secondary_hwnd = h;
                    self.taskbar.secondary_surface.clear();
                    self.taskbar.invalidate_secondary();
                }
            }
        }
    }

    fn refresh_bars(&mut self) {
        self.ensure_bars_alive();
        if let Some(vs) = self.state.read_versioned() {
            self.taskbar.update_bars(&vs, self.host_hwnd);
        }
        self.sync_geom_settle_timer();
    }

    fn sync_geom_settle_timer(&mut self) {
        let wants = self.taskbar.has_pending_geometry();
        if wants && !self.geom_settle_timer_active {
            unsafe {
                let _ = SetTimer(self.host_hwnd, TIMER_GEOM_SETTLE, GEOM_SETTLE_TICK_MS, None);
            }
            self.geom_settle_timer_active = true;
        } else if !wants && self.geom_settle_timer_active {
            unsafe {
                let _ = KillTimer(self.host_hwnd, TIMER_GEOM_SETTLE);
            }
            self.geom_settle_timer_active = false;
        }
    }

    fn wake_wireless_worker(&self) {
        let (lock, cvar) = &*self.wireless_wake;
        if let Ok(mut woken) = lock.lock() {
            *woken = true;
            cvar.notify_all();
        }
    }

    /// Performs all shutdown cleanup except destroying `PopupManager`'s window,
    /// returning the popup handle so `PopupManager::destroy` can run after `RefMut<UiApp>` is dropped.
    fn prepare_quit(&mut self) -> Rc<RefCell<PopupManager>> {
        // Delete tray icon first so no stale unresponsive icon lingers while workers wind down.
        unsafe {
            let nid = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.host_hwnd,
                uID: 1,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
        self.stop_flag.store(true, Ordering::Release);
        self.hw_signal.stop();
        self.wake_wireless_worker();
        unsafe {
            let _ = KillTimer(self.host_hwnd, TIMER_ZORDER_CHECK);
            let _ = KillTimer(self.host_hwnd, TIMER_DEVICE_DEBOUNCE);
            let _ = KillTimer(self.host_hwnd, TIMER_SMOKE_TEST);
            let _ = KillTimer(self.host_hwnd, TIMER_STARTUP_TRIM);
            let _ = KillTimer(self.host_hwnd, TIMER_GEOM_SETTLE);
            self.geom_settle_timer_active = false;
            if !self.hdevnotify.is_invalid() {
                let _ = UnregisterDeviceNotification(self.hdevnotify);
                self.hdevnotify = HDEVNOTIFY::default();
            }
            PostQuitMessage(0);
        }
        Rc::clone(&self.popup)
    }
}

pub fn run(smoke_test: bool) -> i32 {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let _instance_guard = match SingleInstanceGuard::acquire() {
        Some(g) => g,
        None => {
            if smoke_test {
                eprintln!("AirMonitor instance already running.");
                return 1;
            }
            return 0;
        }
    };

    let _com_guard = ComApartmentGuard::init();

    let mut config = AppConfig::load();
    // Keep config.autostart synced with actual registry state at startup.
    let reg_autostart = tray::is_autostart_enabled();
    if config.autostart != reg_autostart {
        config.autostart = reg_autostart;
    }

    let shared_state = SnapshotStore::new(config);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let hw_signal = HwWakeSignal::new();
    let popup_open = Arc::new(AtomicBool::new(false));
    let wireless_wake = Arc::new((Mutex::new(false), Condvar::new()));

    let hinstance: HINSTANCE = unsafe {
        match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(_) => return 1,
        }
    };

    if register_window_classes(hinstance).is_err() {
        return 1;
    }

    let (host_hwnd, primary_hwnd, secondary_hwnd) = match create_windows(hinstance) {
        Ok(hwnds) => hwnds,
        Err(_) => return 1,
    };

    let taskbar = match TaskbarManager::new(primary_hwnd, secondary_hwnd) {
        Ok(t) => t,
        Err(_) => {
            unsafe {
                let _ = DestroyWindow(secondary_hwnd);
                let _ = DestroyWindow(primary_hwnd);
                let _ = DestroyWindow(host_hwnd);
            }
            return 1;
        }
    };

    let tray = match TrayManager::new(host_hwnd) {
        Ok(t) => t,
        Err(_) => {
            unsafe {
                let _ = DestroyWindow(secondary_hwnd);
                let _ = DestroyWindow(primary_hwnd);
                let _ = DestroyWindow(host_hwnd);
            }
            return 1;
        }
    };

    let popup = match PopupManager::new(
        hinstance,
        host_hwnd,
        shared_state.clone(),
        Arc::clone(&popup_open),
        hw_signal.clone(),
    ) {
        Ok(p) => p,
        Err(_) => {
            unsafe {
                let _ = DestroyWindow(secondary_hwnd);
                let _ = DestroyWindow(primary_hwnd);
                let _ = DestroyWindow(host_hwnd);
            }
            return 1;
        }
    };

    let hdevnotify = register_device_notifications(host_hwnd);
    let wm_taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    WM_TASKBAR_CREATED.store(wm_taskbar_created, Ordering::Release);
    let smoke_report = Arc::new(Mutex::new(None));

    let app = Box::new(RefCell::new(UiApp {
        state: shared_state.clone(),
        hinstance,
        host_hwnd,
        self_cell_ptr: 0,
        taskbar,
        tray,
        popup,
        hw_signal: hw_signal.clone(),
        wireless_wake: Arc::clone(&wireless_wake),
        stop_flag: Arc::clone(&stop_flag),
        hdevnotify,
        smoke_test,
        smoke_steady_start: None,
        smoke_report: Arc::clone(&smoke_report),
        geom_settle_timer_active: false,
    }));

    let app_cell_ptr: *const RefCell<UiApp> = &*app;
    {
        let mut guard = app.borrow_mut();
        guard.self_cell_ptr = app_cell_ptr as isize;
        guard.refresh_bars();
    }
    trim_working_set();

    unsafe {
        SetWindowLongPtrW(host_hwnd, GWLP_USERDATA, app_cell_ptr as isize);
        SetWindowLongPtrW(primary_hwnd, GWLP_USERDATA, app_cell_ptr as isize);
        SetWindowLongPtrW(secondary_hwnd, GWLP_USERDATA, app_cell_ptr as isize);

        let _ = SetTimer(host_hwnd, TIMER_ZORDER_CHECK, ZORDER_TICK_MS, None);
        let _ = SetTimer(host_hwnd, TIMER_STARTUP_TRIM, STARTUP_TRIM_MS, None);
    }

    let hw_thread = spawn_hw_worker(
        shared_state.clone(),
        Arc::clone(&stop_flag),
        hw_signal.clone(),
        Arc::clone(&popup_open),
        host_hwnd.0 as isize,
    );
    let wireless_thread = spawn_wireless_worker(
        shared_state.clone(),
        Arc::clone(&stop_flag),
        Arc::clone(&wireless_wake),
        host_hwnd.0 as isize,
    );

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    stop_flag.store(true, Ordering::Release);
    hw_signal.stop();
    {
        let (lock, cvar) = &*wireless_wake;
        if let Ok(mut woken) = lock.lock() {
            *woken = true;
            cvar.notify_all();
        }
    }

    join_workers_with_timeout(hw_thread, wireless_thread, Duration::from_millis(1500));

    let (cur_primary, cur_secondary, popup_rc) = {
        let guard = app.borrow();
        (
            guard.taskbar.primary_hwnd,
            guard.taskbar.secondary_hwnd,
            Rc::clone(&guard.popup),
        )
    };
    PopupManager::destroy(&popup_rc);

    unsafe {
        // Clear user data before destroying windows and dropping UiApp.
        if !host_hwnd.0.is_null() {
            SetWindowLongPtrW(host_hwnd, GWLP_USERDATA, 0);
        }
        for bar_hwnd in [cur_primary, cur_secondary, primary_hwnd, secondary_hwnd] {
            if !bar_hwnd.0.is_null() && IsWindow(bar_hwnd).as_bool() {
                SetWindowLongPtrW(bar_hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(bar_hwnd);
            }
        }
        if !host_hwnd.0.is_null() && IsWindow(host_hwnd).as_bool() {
            let _ = DestroyWindow(host_hwnd);
        }
    }

    if smoke_test {
        use std::io::Write;
        if let Ok(guard) = smoke_report.lock() {
            if let Some(line) = guard.as_deref() {
                println!("{}", line);
                let _ = std::io::stdout().flush();
            }
        }
    }

    drop(app);
    0
}

fn join_workers_with_timeout(
    mut hw_thread: Option<thread::JoinHandle<()>>,
    mut wireless_thread: Option<thread::JoinHandle<()>>,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        if hw_thread.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(h) = hw_thread.take() {
                let _ = h.join();
            }
        }
        if wireless_thread.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(h) = wireless_thread.take() {
                let _ = h.join();
            }
        }
        if hw_thread.is_none() && wireless_thread.is_none() {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_hw_worker(
    state: SnapshotStore,
    stop_flag: Arc<AtomicBool>,
    hw_signal: HwWakeSignal,
    popup_open: Arc<AtomicBool>,
    host_hwnd_raw: isize,
) -> Option<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("hw-worker".to_string())
        .spawn(move || {
            let mut collector = HardwareCollector::new_lazy();
            let sample_once = |collector: &mut HardwareCollector| {
                let is_popup_open = popup_open.load(Ordering::Acquire);
                let (poll_ms, demand) = state
                    .read_snapshot()
                    .map(|s| {
                        (
                            s.config.hw_poll_interval_ms.clamp(500, 5000),
                            HardwareDemand::from_config(&s.config, is_popup_open),
                        )
                    })
                    .unwrap_or_else(|| {
                        let fallback_cfg = AppConfig::default();
                        (
                            fallback_cfg.hw_poll_interval_ms.clamp(500, 5000),
                            HardwareDemand::from_config(&fallback_cfg, is_popup_open),
                        )
                    });

                let (sample, released_idle) = collector.sample_with_demand(demand);
                state.update_hw(sample);
                post_hw_update(host_hwnd_raw);
                if released_idle {
                    trim_working_set();
                }
                poll_ms
            };

            // Warm up initial delta sample, then wait 350ms for a clean initial reading
            // (waking immediately if the popup opens or config changes, or exiting if stopped).
            let _ = sample_once(&mut collector);
            if !hw_signal.wait_timeout(Duration::from_millis(350)) {
                return;
            }

            while !stop_flag.load(Ordering::Acquire) {
                let poll_ms = sample_once(&mut collector);
                if stop_flag.load(Ordering::Acquire) {
                    break;
                }
                if !hw_signal.wait_timeout(Duration::from_millis(poll_ms)) {
                    break;
                }
            }
        })
        .ok()
}

fn spawn_wireless_worker(
    state: SnapshotStore,
    stop_flag: Arc<AtomicBool>,
    wake_sig: Arc<(Mutex<bool>, Condvar)>,
    host_hwnd_raw: isize,
) -> Option<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("wireless-worker".to_string())
        .spawn(move || {
            let mut mgr = WirelessManager::with_stop_flag(Arc::clone(&stop_flag));
            while !stop_flag.load(Ordering::Relaxed) {
                let devices = mgr.poll_once();
                state.update_devices(devices);
                post_wireless_update(host_hwnd_raw);

                let poll_secs = state
                    .read_snapshot()
                    .map(|s| s.config.hid_poll_interval_secs.clamp(10, 600))
                    .unwrap_or(45);

                let (lock, cvar) = &*wake_sig;
                if let Ok(mut woken) = lock.lock() {
                    if !*woken && !stop_flag.load(Ordering::Relaxed) {
                        let (guard, _) = cvar
                            .wait_timeout(woken, Duration::from_secs(poll_secs))
                            .unwrap_or_else(|e| e.into_inner());
                        woken = guard;
                    }
                    *woken = false;
                }
            }
        })
        .ok()
}

fn post_hw_update(host_hwnd_raw: isize) {
    unsafe {
        let hwnd = HWND(host_hwnd_raw as *mut c_void);
        if !hwnd.0.is_null() {
            let _ = PostMessageW(hwnd, WM_APP_HW_UPDATE, WPARAM(0), LPARAM(0));
        }
    }
}

fn post_wireless_update(host_hwnd_raw: isize) {
    unsafe {
        let hwnd = HWND(host_hwnd_raw as *mut c_void);
        if !hwnd.0.is_null() {
            let _ = PostMessageW(hwnd, WM_APP_WIRELESS_UPDATE, WPARAM(0), LPARAM(0));
        }
    }
}

const HOST_CLASS_NAME: PCWSTR = w!("AirMonitor_HostWindow");
const BAR_CLASS_NAME: PCWSTR = w!("AirMonitor_TaskbarOverlay");

fn register_window_classes(hinstance: HINSTANCE) -> windows::core::Result<()> {
    unsafe {
        let cursor = LoadCursorW(HINSTANCE::default(), IDC_ARROW)?;

        let host_wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(host_wnd_proc),
            hInstance: hinstance,
            lpszClassName: HOST_CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassExW(&host_wc) == 0 {
            return Err(windows::core::Error::from_win32());
        }

        let bar_wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(bar_wnd_proc),
            hInstance: hinstance,
            hCursor: cursor,
            lpszClassName: BAR_CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassExW(&bar_wc) == 0 {
            return Err(windows::core::Error::from_win32());
        }
        register_popup_class(hinstance)?;
        Ok(())
    }
}

fn create_bar_window(hinstance: HINSTANCE, title: PCWSTR) -> windows::core::Result<HWND> {
    unsafe {
        let ex_style = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        CreateWindowExW(
            ex_style,
            BAR_CLASS_NAME,
            title,
            WS_POPUP,
            0,
            0,
            1,
            1,
            HWND::default(),
            None,
            hinstance,
            None,
        )
    }
}

fn create_windows(hinstance: HINSTANCE) -> windows::core::Result<(HWND, HWND, HWND)> {
    unsafe {
        let host_hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            HOST_CLASS_NAME,
            w!("AirMonitorHost"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            HWND::default(),
            None,
            hinstance,
            None,
        )?;

        let primary_hwnd = match create_bar_window(hinstance, w!("AirMonitorPrimary")) {
            Ok(h) => h,
            Err(e) => {
                let _ = DestroyWindow(host_hwnd);
                return Err(e);
            }
        };

        let secondary_hwnd = match create_bar_window(hinstance, w!("AirMonitorDevices")) {
            Ok(h) => h,
            Err(e) => {
                let _ = DestroyWindow(primary_hwnd);
                let _ = DestroyWindow(host_hwnd);
                return Err(e);
            }
        };

        Ok((host_hwnd, primary_hwnd, secondary_hwnd))
    }
}

fn register_device_notifications(hwnd: HWND) -> HDEVNOTIFY {
    unsafe {
        let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
            dbcc_size: size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
            dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
            ..Default::default()
        };
        RegisterDeviceNotificationW(
            HANDLE(hwnd.0),
            &filter as *const _ as *const c_void,
            DEVICE_NOTIFY_WINDOW_HANDLE | DEVICE_NOTIFY_ALL_INTERFACE_CLASSES,
        )
        .unwrap_or_default()
    }
}

unsafe fn get_app_cell<'a>(hwnd: HWND) -> Option<&'a RefCell<UiApp>> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<UiApp>;
    if ptr.is_null() {
        None
    } else {
        Some(&*ptr)
    }
}

unsafe fn run_context_menu(
    app_cell: &RefCell<UiApp>,
    host_hwnd: HWND,
    x: i32,
    y: i32,
    state: &SnapshotStore,
) {
    let Some(cmd_id) = tray::track_context_menu(host_hwnd, x, y, state) else {
        return;
    };
    let (action, save_err) = tray::apply_menu_command(cmd_id, state);
    if let Some(err) = save_err {
        if let Ok(mut app) = app_cell.try_borrow_mut() {
            app.tray.notify_save_error(&err);
        }
    }
    match action {
        MenuAction::None => {}
        MenuAction::ConfigChanged => {
            if let Ok(mut app) = app_cell.try_borrow_mut() {
                app.hw_signal.wake();
                app.refresh_bars();
                if let Some(vs) = app.state.read_versioned() {
                    app.tray.update_tooltip(&vs.snapshot);
                    let popup = Rc::clone(&app.popup);
                    drop(app);
                    PopupManager::refresh_if_visible(&popup, &vs);
                }
            } else {
                let _ = PostMessageW(host_hwnd, WM_APP_CONFIG_CHANGED, WPARAM(0), LPARAM(0));
            }
        }
        MenuAction::RefreshWireless => {
            if let Ok(app) = app_cell.try_borrow() {
                app.wake_wireless_worker();
            } else {
                let _ = PostMessageW(host_hwnd, WM_APP_REFRESH_WIRELESS, WPARAM(0), LPARAM(0));
            }
        }
        MenuAction::OpenConfig => {
            let snap = state.read_snapshot();
            if let Some(s) = snap {
                if let Err(err) = tray::open_config_in_editor(&s) {
                    if let Ok(mut app) = app_cell.try_borrow_mut() {
                        app.tray.notify_save_error(&err);
                    }
                }
            }
        }
        MenuAction::OpenAbout => {
            let popup = app_cell.try_borrow().ok().map(|app| Rc::clone(&app.popup));
            if let Some(popup) = popup {
                PopupManager::show_tab_at(&popup, x, y, PopupTab::About);
            }
        }
        MenuAction::Quit => {
            let popup = if let Ok(mut app) = app_cell.try_borrow_mut() {
                Some(app.prepare_quit())
            } else {
                let _ = PostMessageW(host_hwnd, WM_APP_DEFERRED_QUIT, WPARAM(0), LPARAM(0));
                None
            };
            if let Some(popup) = popup {
                PopupManager::destroy(&popup);
            }
        }
    }
}

unsafe extern "system" fn host_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_DEVICECHANGE {
        let evt = wparam.0 as u32;
        if evt == DBT_DEVICEARRIVAL
            || evt == DBT_DEVICEREMOVECOMPLETE
            || evt == DBT_DEVNODES_CHANGED
        {
            let _ = SetTimer(hwnd, TIMER_DEVICE_DEBOUNCE, DEVICE_DEBOUNCE_MS, None);
        }
        return LRESULT(0);
    }
    if msg == WM_DESTROY {
        return LRESULT(0);
    }

    let wm_taskbar_created = WM_TASKBAR_CREATED.load(Ordering::Relaxed);
    let is_taskbar_created = wm_taskbar_created != 0 && msg == wm_taskbar_created;
    let needs_app = is_taskbar_created
        || msg >= WM_USER
        || matches!(msg, WM_SETTINGCHANGE | WM_DPICHANGED | WM_TIMER | WM_CLOSE);
    if !needs_app {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    let Some(app_cell) = get_app_cell(hwnd) else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };

    let mut app_guard = match app_cell.try_borrow_mut() {
        Ok(guard) => guard,
        Err(_) => {
            // Re-entered while `UiApp` is mutably borrowed: defer via PostMessageW.
            if is_taskbar_created {
                let _ = PostMessageW(hwnd, msg, wparam, lparam);
                return LRESULT(0);
            }
            match msg {
                WM_APP_TRAY_CALLBACK
                | WM_APP_HW_UPDATE
                | WM_APP_WIRELESS_UPDATE
                | WM_APP_CONFIG_CHANGED
                | WM_APP_REFRESH_WIRELESS
                | WM_APP_UPDATE_STATUS
                | WM_APP_UPDATE_RESTART
                | WM_APP_DEFERRED_SETTINGCHANGE
                | WM_APP_DEFERRED_REFRESH_BARS
                | WM_APP_DEFERRED_ZORDER_CHECK
                | WM_APP_DEFERRED_SMOKE_TEST
                | WM_APP_DEFERRED_QUIT
                | WM_CLOSE => {
                    let _ = PostMessageW(hwnd, msg, wparam, lparam);
                    return LRESULT(0);
                }
                WM_SETTINGCHANGE => {
                    let _ = PostMessageW(hwnd, WM_APP_DEFERRED_SETTINGCHANGE, WPARAM(0), LPARAM(0));
                    return LRESULT(0);
                }
                WM_DPICHANGED => {
                    let _ = PostMessageW(hwnd, WM_APP_DEFERRED_REFRESH_BARS, WPARAM(0), LPARAM(0));
                    return LRESULT(0);
                }
                WM_TIMER => {
                    match wparam.0 {
                        TIMER_ZORDER_CHECK | TIMER_GEOM_SETTLE => {
                            let _ = PostMessageW(
                                hwnd,
                                WM_APP_DEFERRED_ZORDER_CHECK,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                        TIMER_DEVICE_DEBOUNCE => {
                            let _ = KillTimer(hwnd, TIMER_DEVICE_DEBOUNCE);
                            let _ =
                                PostMessageW(hwnd, WM_APP_REFRESH_WIRELESS, WPARAM(0), LPARAM(0));
                        }
                        TIMER_SMOKE_TEST => {
                            let _ = KillTimer(hwnd, TIMER_SMOKE_TEST);
                            let _ = PostMessageW(
                                hwnd,
                                WM_APP_DEFERRED_SMOKE_TEST,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                        TIMER_STARTUP_TRIM => {
                            let _ = KillTimer(hwnd, TIMER_STARTUP_TRIM);
                            trim_working_set();
                        }
                        _ => {}
                    }
                    return LRESULT(0);
                }
                _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    };
    let app = &mut *app_guard;

    if is_taskbar_created {
        app.ensure_bars_alive();
        app.refresh_bars();
        if let Some(geom) = find_taskbar_geometry() {
            app.tray.refresh_icon_for_dpi(geom.dpi);
        }
        app.tray.reset_retry_count();
        if app.tray.re_add() {
            if let Some(snap) = app.state.read_snapshot() {
                app.tray.update_tooltip(&snap);
            }
        }
        return LRESULT(0);
    }

    match msg {
        WM_APP_HW_UPDATE => {
            app.ensure_bars_alive();
            if let Some(vs) = app.state.read_versioned() {
                app.taskbar.update_bars(&vs, app.host_hwnd);
                app.sync_geom_settle_timer();
                app.tray.update_tooltip(&vs.snapshot);
                let popup = Rc::clone(&app.popup);
                drop(app_guard);
                PopupManager::refresh_if_visible(&popup, &vs);
            }
            return LRESULT(0);
        }
        WM_APP_WIRELESS_UPDATE => {
            app.ensure_bars_alive();
            if let Some(vs) = app.state.read_versioned() {
                app.taskbar.update_bars(&vs, app.host_hwnd);
                app.sync_geom_settle_timer();
                app.tray.update_tooltip(&vs.snapshot);
                app.tray.check_low_battery_alerts(&vs.snapshot);
                let popup = Rc::clone(&app.popup);
                drop(app_guard);
                PopupManager::refresh_if_visible(&popup, &vs);
            }
            return LRESULT(0);
        }
        WM_APP_CONFIG_CHANGED => {
            app.hw_signal.wake();
            let popup = Rc::clone(&app.popup);
            if let Some(err) = PopupManager::take_save_error(&popup) {
                app.tray.notify_save_error(&err);
            }
            app.refresh_bars();
            if let Some(vs) = app.state.read_versioned() {
                app.tray.update_tooltip(&vs.snapshot);
                drop(app_guard);
                PopupManager::refresh_if_visible(&popup, &vs);
            }
            return LRESULT(0);
        }
        WM_APP_REFRESH_WIRELESS => {
            app.wake_wireless_worker();
            return LRESULT(0);
        }
        WM_APP_UPDATE_STATUS => {
            let popup = Rc::clone(&app.popup);
            drop(app_guard);
            PopupManager::refresh_update_status(&popup);
            return LRESULT(0);
        }
        WM_APP_UPDATE_RESTART => {
            let popup = app.prepare_quit();
            drop(app_guard);
            PopupManager::destroy(&popup);
            return LRESULT(0);
        }
        WM_APP_TRAY_CALLBACK => {
            let event = (lparam.0 & 0xFFFF) as u32;
            let v4 = app.tray.is_v4();
            let is_toggle = if v4 {
                event == NIN_SELECT || event == NIN_KEYSELECT
            } else {
                event == WM_LBUTTONUP || event == NIN_SELECT || event == NIN_KEYSELECT
            };
            let is_ctx_menu = if v4 {
                event == WM_CONTEXTMENU
            } else {
                event == WM_RBUTTONUP || event == WM_CONTEXTMENU
            };

            if is_toggle || is_ctx_menu {
                // Under NOTIFYICON_VERSION_4, wParam encodes the tray icon's screen (x, y)
                // anchor so keyboard activation (NIN_KEYSELECT / WM_CONTEXTMENU) positions
                // popups next to the icon rather than the mouse cursor.
                let (ax, ay) = if v4
                    && (event == NIN_SELECT || event == NIN_KEYSELECT || event == WM_CONTEXTMENU)
                {
                    let wx = (wparam.0 & 0xFFFF) as i16 as i32;
                    let wy = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
                    if wx != 0 || wy != 0 {
                        (wx, wy)
                    } else {
                        let mut pt = POINT::default();
                        if GetCursorPos(&mut pt).is_ok() {
                            (pt.x, pt.y)
                        } else {
                            (0, 0)
                        }
                    }
                } else {
                    let mut pt = POINT::default();
                    if GetCursorPos(&mut pt).is_ok() {
                        (pt.x, pt.y)
                    } else {
                        (0, 0)
                    }
                };

                if is_toggle {
                    let popup = Rc::clone(&app.popup);
                    drop(app_guard);
                    PopupManager::toggle_at(&popup, ax, ay, true);
                } else {
                    let host_hwnd = app.host_hwnd;
                    let state = app.state.clone();
                    drop(app_guard);
                    run_context_menu(app_cell, host_hwnd, ax, ay, &state);
                }
            }
            return LRESULT(0);
        }
        WM_SETTINGCHANGE | WM_APP_DEFERRED_SETTINGCHANGE => {
            app.taskbar.refresh_theme();
            app.refresh_bars();
            let vs = app.state.read_versioned();
            let popup = Rc::clone(&app.popup);
            drop(app_guard);
            PopupManager::refresh_theme(&popup, vs.as_ref());
            return LRESULT(0);
        }
        WM_DPICHANGED | WM_APP_DEFERRED_REFRESH_BARS => {
            let dpi = if msg == WM_DPICHANGED {
                (wparam.0 & 0xFFFF) as u32
            } else {
                find_taskbar_geometry().map(|g| g.dpi).unwrap_or(96)
            };
            if dpi > 0 {
                app.tray.refresh_icon_for_dpi(dpi);
            }
            app.refresh_bars();
            return LRESULT(0);
        }
        WM_APP_DEFERRED_ZORDER_CHECK => {
            if !app.tray.is_added() && app.tray.retry_add_if_needed() {
                if let Some(snap) = app.state.read_snapshot() {
                    app.tray.update_tooltip(&snap);
                }
            }
            app.ensure_bars_alive();
            if let Some(vs) = app.state.read_versioned() {
                app.taskbar.check_tick(&vs, app.host_hwnd);
            }
            app.sync_geom_settle_timer();
            let popup = Rc::clone(&app.popup);
            drop(app_guard);
            PopupManager::hide_if_lost_focus(&popup);
            return LRESULT(0);
        }
        WM_APP_DEFERRED_SMOKE_TEST => {
            let line = build_smoke_report(
                &app.state,
                app.smoke_steady_start,
                &app.taskbar.bar_diagnostic_summary(),
            );
            if let Ok(mut guard) = app.smoke_report.lock() {
                *guard = Some(line);
            }
            let popup = app.prepare_quit();
            drop(app_guard);
            PopupManager::destroy(&popup);
            return LRESULT(0);
        }
        WM_TIMER => {
            match wparam.0 {
                TIMER_ZORDER_CHECK => {
                    if !app.tray.is_added() && app.tray.retry_add_if_needed() {
                        if let Some(snap) = app.state.read_snapshot() {
                            app.tray.update_tooltip(&snap);
                        }
                    }
                    app.ensure_bars_alive();
                    if let Some(vs) = app.state.read_versioned() {
                        app.taskbar.check_tick(&vs, app.host_hwnd);
                    }
                    app.sync_geom_settle_timer();
                    let popup = Rc::clone(&app.popup);
                    drop(app_guard);
                    PopupManager::hide_if_lost_focus(&popup);
                    return LRESULT(0);
                }
                TIMER_GEOM_SETTLE => {
                    app.ensure_bars_alive();
                    if let Some(vs) = app.state.read_versioned() {
                        app.taskbar.check_tick(&vs, app.host_hwnd);
                    }
                    app.sync_geom_settle_timer();
                    return LRESULT(0);
                }
                TIMER_STARTUP_TRIM => {
                    let _ = KillTimer(hwnd, TIMER_STARTUP_TRIM);
                    trim_working_set();
                    if app.smoke_test {
                        app.smoke_steady_start = Some((Instant::now(), query_process_cpu_100ns()));
                        let _ = SetTimer(hwnd, TIMER_SMOKE_TEST, SMOKE_STEADY_MS, None);
                    }
                }
                TIMER_DEVICE_DEBOUNCE => {
                    let _ = KillTimer(hwnd, TIMER_DEVICE_DEBOUNCE);
                    app.wake_wireless_worker();
                }
                TIMER_SMOKE_TEST => {
                    let _ = KillTimer(hwnd, TIMER_SMOKE_TEST);
                    let line = build_smoke_report(
                        &app.state,
                        app.smoke_steady_start,
                        &app.taskbar.bar_diagnostic_summary(),
                    );
                    if let Ok(mut guard) = app.smoke_report.lock() {
                        *guard = Some(line);
                    }
                    let popup = app.prepare_quit();
                    drop(app_guard);
                    PopupManager::destroy(&popup);
                }
                _ => {}
            }
            return LRESULT(0);
        }
        WM_CLOSE | WM_APP_DEFERRED_QUIT => {
            let popup = app.prepare_quit();
            drop(app_guard);
            PopupManager::destroy(&popup);
            return LRESULT(0);
        }
        _ => {}
    }
    drop(app_guard);
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn bar_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }

    if !matches!(
        msg,
        WM_LBUTTONDOWN
            | WM_MOUSEMOVE
            | WM_MOUSELEAVE
            | WM_LBUTTONUP
            | WM_CAPTURECHANGED
            | WM_RBUTTONUP
    ) {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    let Some(app_cell) = get_app_cell(hwnd) else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };

    let mut app_guard = match app_cell.try_borrow_mut() {
        Ok(guard) => guard,
        Err(_) => {
            if msg == WM_MOUSELEAVE || msg == WM_CAPTURECHANGED {
                let _ = PostMessageW(hwnd, msg, wparam, lparam);
                return LRESULT(0);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
    };
    let app = &mut *app_guard;

    match msg {
        WM_LBUTTONDOWN => {
            if (wparam.0 & MK_CONTROL) != 0 {
                let snap = app.state.read_snapshot();
                if let Some(s) = snap {
                    let is_devices_zone = match s.config.layout_mode {
                        LayoutMode::Combined | LayoutMode::HardwareOnly => false,
                        LayoutMode::DevicesOnly => true,
                        LayoutMode::Split => hwnd == app.taskbar.secondary_hwnd,
                    };
                    let zone = match (s.config.layout_mode, is_devices_zone) {
                        (LayoutMode::Combined, _) => &s.config.combined_zone,
                        (LayoutMode::HardwareOnly, _) | (LayoutMode::Split, false) => {
                            &s.config.hardware_zone
                        }
                        (LayoutMode::DevicesOnly, _) | (LayoutMode::Split, true) => {
                            &s.config.devices_zone
                        }
                    };
                    let mut pt = POINT::default();
                    if GetCursorPos(&mut pt).is_ok() {
                        let dpi = find_taskbar_geometry().map(|g| g.dpi).unwrap_or(96);
                        app.taskbar.drag = Some(DragState {
                            hwnd,
                            is_devices_zone,
                            start_cursor_x: pt.x,
                            start_offset_x: zone.offset_x,
                            anchor: zone.anchor,
                            dpi,
                        });
                        drop(app_guard);
                        let _ = SetCapture(hwnd);
                    }
                }
            }
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            if let Some(drag) = app.taskbar.drag {
                if drag.hwnd == hwnd {
                    let mut pt = POINT::default();
                    if GetCursorPos(&mut pt).is_ok() {
                        let scale = drag.dpi.max(96) as f32 / 96.0;
                        let delta_logical =
                            ((pt.x - drag.start_cursor_x) as f32 / scale).round() as i32;
                        let new_offset = match drag.anchor {
                            AnchorEdge::Left => drag
                                .start_offset_x
                                .saturating_add(delta_logical)
                                .clamp(0, crate::config::MAX_OFFSET_X),
                            AnchorEdge::Right => drag
                                .start_offset_x
                                .saturating_sub(delta_logical)
                                .clamp(0, crate::config::MAX_OFFSET_X),
                        };
                        let changed = app.state.update_config(|cfg| {
                            let target = match (cfg.layout_mode, drag.is_devices_zone) {
                                (LayoutMode::Combined, _) => &mut cfg.combined_zone.offset_x,
                                (LayoutMode::HardwareOnly, _) | (LayoutMode::Split, false) => {
                                    &mut cfg.hardware_zone.offset_x
                                }
                                (LayoutMode::DevicesOnly, _) | (LayoutMode::Split, true) => {
                                    &mut cfg.devices_zone.offset_x
                                }
                            };
                            if *target == new_offset {
                                false
                            } else {
                                *target = new_offset;
                                true
                            }
                        });
                        if changed.is_some() {
                            app.refresh_bars();
                        }
                    }
                    return LRESULT(0);
                }
            }

            let is_primary = hwnd == app.taskbar.primary_hwnd;
            let was_hovered = if is_primary {
                app.taskbar.primary_hovered
            } else {
                app.taskbar.secondary_hovered
            };
            if !was_hovered {
                if is_primary {
                    app.taskbar.primary_hovered = true;
                } else {
                    app.taskbar.secondary_hovered = true;
                }
                app.refresh_bars();
                drop(app_guard);
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
            return LRESULT(0);
        }
        WM_MOUSELEAVE => {
            let changed = if hwnd == app.taskbar.primary_hwnd {
                let prev = app.taskbar.primary_hovered;
                app.taskbar.primary_hovered = false;
                prev
            } else if hwnd == app.taskbar.secondary_hwnd {
                let prev = app.taskbar.secondary_hovered;
                app.taskbar.secondary_hovered = false;
                prev
            } else {
                false
            };
            if changed {
                app.refresh_bars();
            }
            return LRESULT(0);
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            let mut was_dragging = false;
            if let Some(drag) = app.taskbar.drag {
                if drag.hwnd == hwnd || msg == WM_CAPTURECHANGED {
                    was_dragging = true;
                    app.taskbar.drag = None;
                }
            }
            let state = app.state.clone();
            let popup = Rc::clone(&app.popup);
            // Release RefMut<UiApp> before ReleaseCapture() (which sends WM_CAPTURECHANGED)
            // or PopupManager::toggle_at() (which calls SetWindowPos/SetForegroundWindow).
            drop(app_guard);

            if was_dragging {
                if msg == WM_LBUTTONUP {
                    let _ = ReleaseCapture();
                }
                if let Some(snap) = state.read_snapshot() {
                    if let Err(err) = snap.config.save() {
                        if let Ok(mut app) = app_cell.try_borrow_mut() {
                            app.tray.notify_save_error(&err);
                        }
                    }
                }
            } else if msg == WM_LBUTTONUP && (wparam.0 & MK_CONTROL) == 0 {
                let mut pt = POINT::default();
                if GetCursorPos(&mut pt).is_ok() {
                    PopupManager::toggle_at(&popup, pt.x, pt.y, false);
                }
            }
            return LRESULT(0);
        }
        WM_RBUTTONUP => {
            let host_hwnd = app.host_hwnd;
            let state = app.state.clone();
            drop(app_guard);
            let mut pt = POINT::default();
            if GetCursorPos(&mut pt).is_ok() {
                run_context_menu(app_cell, host_hwnd, pt.x, pt.y, &state);
            }
            return LRESULT(0);
        }
        _ => {}
    }

    drop(app_guard);
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCountersEx {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

fn query_process_memory_mb() -> Option<(f64, f64)> {
    unsafe {
        type FnGetProcMem =
            unsafe extern "system" fn(HANDLE, *mut ProcessMemoryCountersEx, u32) -> i32;
        let module = LoadLibraryExW(
            w!("kernel32.dll"),
            HANDLE::default(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
        .ok()?;
        let res = (|| {
            let proc_addr = GetProcAddress(module, windows::core::s!("K32GetProcessMemoryInfo"))?;
            let func: FnGetProcMem = std::mem::transmute(proc_addr);
            let mut pmc = ProcessMemoryCountersEx {
                cb: size_of::<ProcessMemoryCountersEx>() as u32,
                ..Default::default()
            };
            if func(GetCurrentProcess(), &mut pmc, pmc.cb) != 0 {
                let ws = pmc.working_set_size as f64 / (1024.0 * 1024.0);
                let priv_bytes = pmc.private_usage as f64 / (1024.0 * 1024.0);
                Some((ws, priv_bytes))
            } else {
                None
            }
        })();
        let _ = FreeLibrary(module);
        res
    }
}

fn filetime_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

fn query_process_cpu_100ns() -> u64 {
    unsafe {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
        .is_ok()
        {
            filetime_to_u64(kernel).saturating_add(filetime_to_u64(user))
        } else {
            0
        }
    }
}

fn build_smoke_report(
    state: &SnapshotStore,
    steady_start: Option<(Instant, u64)>,
    bar_diag: &str,
) -> String {
    let mem_str = match query_process_memory_mb() {
        Some((ws, priv_mb)) => format!("WorkingSet={:.2} MB | PrivateBytes={:.2} MB", ws, priv_mb),
        None => "WorkingSet=N/A | PrivateBytes=N/A".to_string(),
    };
    let cpu_idle_str = match steady_start {
        Some((start_t, start_cpu)) => {
            let elapsed = start_t.elapsed().as_secs_f64().max(0.001);
            let end_cpu = query_process_cpu_100ns();
            let delta_secs = end_cpu.saturating_sub(start_cpu) as f64 / 10_000_000.0;
            let logical_cpus = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1) as f64;
            let pct = (delta_secs / (elapsed * logical_cpus)) * 100.0;
            format!("{:.2}%", pct)
        }
        None => "N/A".to_string(),
    };
    if let Some(snap) = state.read_snapshot() {
        format!(
            "AirMonitor smoke test OK | {} | IdleCpuPct={} | CPU={:.1}% MEM={:.1}% | Devices={} | {}",
            mem_str,
            cpu_idle_str,
            snap.hw.cpu_usage_pct,
            snap.hw.mem_usage_pct,
            snap.devices.len(),
            bar_diag
        )
    } else {
        format!(
            "AirMonitor smoke test OK | {} | IdleCpuPct={} | {}",
            mem_str, cpu_idle_str, bar_diag
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeviceKind, Transport};

    #[test]
    fn snapshot_store_updates_independent_revisions_via_arc_swap() {
        let store = SnapshotStore::new(AppConfig::default());
        let v0 = store.read_versioned().unwrap();
        assert_eq!((v0.hw_rev, v0.wireless_rev, v0.config_rev), (0, 0, 0));

        let hw = HardwareMetrics {
            cpu_usage_pct: 42.5,
            ..Default::default()
        };
        store.update_hw(hw);

        let v1 = store.read_versioned().unwrap();
        assert_eq!((v1.hw_rev, v1.wireless_rev, v1.config_rev), (1, 0, 0));
        assert!(!Arc::ptr_eq(&v0.snapshot, &v1.snapshot));
        assert!((v0.snapshot.hw.cpu_usage_pct - 0.0).abs() < f32::EPSILON);
        assert!((v1.snapshot.hw.cpu_usage_pct - 42.5).abs() < f32::EPSILON);

        store.update_devices(vec![WirelessDevice {
            id: "d1".into(),
            name: "Mouse".into(),
            kind: DeviceKind::Mouse,
            transport: Transport::Wireless24G,
            battery_pct: Some(75),
            charging: false,
            sub_batteries: None,
            online: true,
            last_seen: Instant::now(),
            detail_note: None,
        }]);
        let v2 = store.read_versioned().unwrap();
        assert_eq!((v2.hw_rev, v2.wireless_rev, v2.config_rev), (1, 1, 0));
        assert_eq!(v2.snapshot.devices.len(), 1);

        // No-op config update should not allocate a new snapshot or bump config_rev.
        assert!(store.update_config(|_| false).is_none());
        let v2_same = store.read_versioned().unwrap();
        assert!(Arc::ptr_eq(&v2.snapshot, &v2_same.snapshot));

        let updated = store.update_config(|cfg| {
            cfg.low_battery_threshold = 15;
            true
        });
        assert!(updated.is_some());
        let v3 = store.read_versioned().unwrap();
        assert_eq!((v3.hw_rev, v3.wireless_rev, v3.config_rev), (1, 1, 1));
        assert_eq!(v3.snapshot.config.low_battery_threshold, 15);
    }

    #[test]
    fn hw_wake_signal_wakes_and_stops_promptly() {
        let sig = HwWakeSignal::new();

        // Pre-armed wake returns immediately without waiting full timeout.
        sig.wake();
        let t0 = Instant::now();
        assert!(sig.wait_timeout(Duration::from_secs(2)));
        assert!(t0.elapsed() < Duration::from_millis(500));

        // Wake flag is consumed; subsequent wait with wake from another thread wakes early.
        let sig2 = sig.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            sig2.wake();
        });
        let t1 = Instant::now();
        assert!(sig.wait_timeout(Duration::from_secs(2)));
        assert!(t1.elapsed() < Duration::from_millis(500));
        let _ = handle.join();

        // Stop always returns false even if wake was also called.
        sig.wake();
        sig.stop();
        assert!(!sig.wait_timeout(Duration::from_secs(2)));
    }
}
