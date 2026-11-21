use std::ffi::c_void;
use std::mem::size_of;
use std::sync::OnceLock;
use std::time::Instant;
use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FILETIME, HANDLE, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE,
};
use windows::Win32::System::Power::{CallNtPowerInformation, ProcessorInformation};
use windows::Win32::System::Threading::{
    GetActiveProcessorCount, GetActiveProcessorGroupCount, GetSystemTimes, ALL_PROCESSOR_GROUPS,
};

use super::pdh::PdhBackoff;

const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessorPowerInformation {
    number: u32,
    max_mhz: u32,
    current_mhz: u32,
    mhz_limit: u32,
    max_idle_state: u32,
    current_idle_state: u32,
}

const _: () = assert!(size_of::<ProcessorPowerInformation>() == 24);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SystemProcessorPerformanceInformation {
    idle_time: i64,
    kernel_time: i64,
    user_time: i64,
    dpc_time: i64,
    interrupt_time: i64,
    interrupt_count: u32,
}

const _: () = assert!(size_of::<SystemProcessorPerformanceInformation>() == 48);

type FnNtQuerySystemInformationEx = unsafe extern "system" fn(
    system_information_class: u32,
    input_buffer: *const c_void,
    input_buffer_length: u32,
    system_information: *mut c_void,
    system_information_length: u32,
    return_length: *mut u32,
) -> i32;

type FnNtQuerySystemInformation = unsafe extern "system" fn(
    system_information_class: u32,
    system_information: *mut c_void,
    system_information_length: u32,
    return_length: *mut u32,
) -> i32;

struct NtdllCpuProcs {
    _module: usize,
    nt_query_ex: Option<FnNtQuerySystemInformationEx>,
    nt_query: Option<FnNtQuerySystemInformation>,
}

static NTDLL_CPU_PROCS: OnceLock<NtdllCpuProcs> = OnceLock::new();

fn ntdll_cpu_procs() -> &'static NtdllCpuProcs {
    NTDLL_CPU_PROCS.get_or_init(|| unsafe {
        let Ok(h) = LoadLibraryExW(
            w!("ntdll.dll"),
            HANDLE::default(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        ) else {
            return NtdllCpuProcs {
                _module: 0,
                nt_query_ex: None,
                nt_query: None,
            };
        };
        if h.0.is_null() {
            return NtdllCpuProcs {
                _module: 0,
                nt_query_ex: None,
                nt_query: None,
            };
        }
        let get_fn = |name: &[u8]| -> Option<unsafe extern "system" fn() -> isize> {
            GetProcAddress(HMODULE(h.0), PCSTR(name.as_ptr()))
        };
        let nt_query_ex =
            get_fn(b"NtQuerySystemInformationEx\0").map(|p| {
                std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    FnNtQuerySystemInformationEx,
                >(p)
            });
        let nt_query = get_fn(b"NtQuerySystemInformation\0").map(|p| {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, FnNtQuerySystemInformation>(
                p,
            )
        });
        NtdllCpuProcs {
            _module: h.0 as usize,
            nt_query_ex,
            nt_query,
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CpuTimesSource {
    NtAllGroups,
    GetSystemTimes,
}

pub struct CpuMonitor {
    prev_source: Option<CpuTimesSource>,
    prev_idle: u64,
    prev_kernel: u64,
    prev_user: u64,
    initialized: bool,
    base_mhz: Option<f32>,
    base_mhz_attempted: bool,
    pdh_query: isize,
    pdh_perf_counter: isize,
    pdh_init_attempted: bool,
    pdh_invalid_streak: u8,
    pdh_backoff: PdhBackoff,
}

impl CpuMonitor {
    pub fn new() -> Self {
        let base_mhz = read_cpu_base_mhz();
        let (pdh_query, pdh_perf_counter) = init_cpu_perf_pdh();
        let mut pdh_backoff = PdhBackoff::default();
        if pdh_query == 0 {
            pdh_backoff.record_failure(Instant::now());
        }
        Self {
            prev_source: None,
            prev_idle: 0,
            prev_kernel: 0,
            prev_user: 0,
            initialized: false,
            base_mhz,
            base_mhz_attempted: true,
            pdh_query,
            pdh_perf_counter,
            pdh_init_attempted: true,
            pdh_invalid_streak: 0,
            pdh_backoff,
        }
    }

    pub fn new_lazy() -> Self {
        Self {
            prev_source: None,
            prev_idle: 0,
            prev_kernel: 0,
            prev_user: 0,
            initialized: false,
            base_mhz: None,
            base_mhz_attempted: false,
            pdh_query: 0,
            pdh_perf_counter: 0,
            pdh_init_attempted: false,
            pdh_invalid_streak: 0,
            pdh_backoff: PdhBackoff::default(),
        }
    }

    pub fn sample(&mut self) -> (Option<f32>, Option<f32>) {
        self.sample_with_freq(true)
    }

    pub fn sample_with_freq(&mut self, need_freq: bool) -> (Option<f32>, Option<f32>) {
        let usage = self.sample_usage_with_reader(read_system_cpu_times);
        let freq_ghz = if need_freq {
            self.sample_freq_ghz_at(Instant::now())
        } else {
            None
        };
        (usage, freq_ghz)
    }

    fn sample_usage_with_reader<F>(&mut self, reader: F) -> Option<f32>
    where
        F: FnOnce() -> Option<(CpuTimesSource, u64, u64, u64)>,
    {
        match reader() {
            Some((source, i, k, u)) => {
                if !self.initialized || self.prev_source != Some(source) {
                    self.prev_source = Some(source);
                    self.prev_idle = i;
                    self.prev_kernel = k;
                    self.prev_user = u;
                    self.initialized = true;
                    None
                } else {
                    let pct = compute_cpu_usage_pct(
                        self.prev_idle,
                        self.prev_kernel,
                        self.prev_user,
                        i,
                        k,
                        u,
                    );
                    self.prev_idle = i;
                    self.prev_kernel = k;
                    self.prev_user = u;
                    Some(pct)
                }
            }
            None => {
                // Reset baseline so recovery always takes two fresh samples on the same clock source.
                self.initialized = false;
                self.prev_source = None;
                None
            }
        }
    }

    pub fn release_freq_pdh(&mut self) -> bool {
        let was_open = self.pdh_query != 0;
        if self.pdh_query != 0 {
            unsafe {
                let _ = PdhCloseQuery(self.pdh_query);
            }
            self.pdh_query = 0;
        }
        self.pdh_perf_counter = 0;
        let had_attempted = self.pdh_init_attempted;
        self.pdh_init_attempted = false;
        self.pdh_invalid_streak = 0;
        self.base_mhz_attempted = false;
        self.base_mhz = None;
        let had_backoff = self.pdh_backoff.reset();
        was_open || had_attempted || had_backoff
    }

    pub fn release(&mut self) -> bool {
        let had_usage = self.initialized || self.prev_source.is_some();
        self.initialized = false;
        self.prev_source = None;
        self.prev_idle = 0;
        self.prev_kernel = 0;
        self.prev_user = 0;
        let released_pdh = self.release_freq_pdh();
        had_usage || released_pdh
    }

    pub(crate) fn has_freq_pdh(&self) -> bool {
        self.pdh_query != 0
    }

    fn close_pdh_handle_keep_backoff(&mut self) {
        if self.pdh_query != 0 {
            unsafe {
                let _ = PdhCloseQuery(self.pdh_query);
            }
            self.pdh_query = 0;
        }
        self.pdh_perf_counter = 0;
        self.pdh_invalid_streak = 0;
    }

    fn sample_freq_ghz_at(&mut self, now: Instant) -> Option<f32> {
        if !self.base_mhz_attempted {
            self.base_mhz_attempted = true;
            self.base_mhz = read_cpu_base_mhz();
        }
        let base_mhz = self.base_mhz.filter(|&m| m > 0.0)?;
        if self.pdh_query == 0 || self.pdh_perf_counter == 0 {
            if !self.pdh_backoff.can_attempt(now) {
                return None;
            }
            self.pdh_init_attempted = true;
            let (q, c) = init_cpu_perf_pdh();
            self.pdh_query = q;
            self.pdh_perf_counter = c;
            if q == 0 || c == 0 {
                self.close_pdh_handle_keep_backoff();
                self.pdh_backoff.record_failure(now);
                return None;
            }
        }
        unsafe {
            if PdhCollectQueryData(self.pdh_query) != 0 {
                self.close_pdh_handle_keep_backoff();
                self.pdh_backoff.record_failure(now);
                return None;
            }
            let mut val = PDH_FMT_COUNTERVALUE::default();
            if PdhGetFormattedCounterValue(self.pdh_perf_counter, PDH_FMT_DOUBLE, None, &mut val)
                == 0
            {
                let pct = val.Anonymous.doubleValue;
                if pct.is_finite() && pct > 1.0 && pct < 500.0 {
                    self.pdh_invalid_streak = 0;
                    self.pdh_backoff.record_success();
                    let ghz = (base_mhz as f64 * (pct / 100.0)) / 1000.0;
                    return Some(ghz as f32);
                }
            }
        }
        self.pdh_invalid_streak = self.pdh_invalid_streak.saturating_add(1);
        if self.pdh_invalid_streak >= 3 {
            self.close_pdh_handle_keep_backoff();
            self.pdh_backoff.record_failure(now);
        }
        None
    }
}

impl Default for CpuMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CpuMonitor {
    fn drop(&mut self) {
        let _ = self.release_freq_pdh();
    }
}

fn read_system_cpu_times() -> Option<(CpuTimesSource, u64, u64, u64)> {
    if let Some((idle, kernel, user)) = read_nt_all_groups_cpu_times() {
        return Some((CpuTimesSource::NtAllGroups, idle, kernel, user));
    }

    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() {
            Some((
                CpuTimesSource::GetSystemTimes,
                ft_to_u64(idle),
                ft_to_u64(kernel),
                ft_to_u64(user),
            ))
        } else {
            None
        }
    }
}

fn read_nt_all_groups_cpu_times() -> Option<(u64, u64, u64)> {
    let procs = ntdll_cpu_procs();
    let group_count = unsafe { GetActiveProcessorGroupCount() };
    if group_count == 0 {
        return None;
    }

    if let Some(nt_query_ex) = procs.nt_query_ex {
        let mut total_idle: u64 = 0;
        let mut total_kernel: u64 = 0;
        let mut total_user: u64 = 0;
        let mut read_any = false;

        for group in 0..group_count {
            let count = unsafe { GetActiveProcessorCount(group) } as usize;
            if count == 0 {
                continue;
            }
            let mut entries = vec![SystemProcessorPerformanceInformation::default(); count];
            let buf_bytes =
                u32::try_from(entries.len() * size_of::<SystemProcessorPerformanceInformation>())
                    .ok()?;
            let mut ret_len: u32 = 0;
            let status = unsafe {
                nt_query_ex(
                    SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
                    &group as *const u16 as *const c_void,
                    size_of::<u16>() as u32,
                    entries.as_mut_ptr() as *mut c_void,
                    buf_bytes,
                    &mut ret_len,
                )
            };
            if status < 0 {
                return None;
            }
            let elem_size = size_of::<SystemProcessorPerformanceInformation>();
            let valid = (ret_len as usize / elem_size).min(entries.len());
            if let Some((gi, gk, gu)) = aggregate_processor_performance(&entries[..valid]) {
                total_idle = total_idle.saturating_add(gi);
                total_kernel = total_kernel.saturating_add(gk);
                total_user = total_user.saturating_add(gu);
                read_any = true;
            } else {
                return None;
            }
        }
        return read_any.then_some((total_idle, total_kernel, total_user));
    }

    if group_count == 1 {
        if let Some(nt_query) = procs.nt_query {
            let count = unsafe { GetActiveProcessorCount(0) } as usize;
            if count == 0 {
                return None;
            }
            let mut entries = vec![SystemProcessorPerformanceInformation::default(); count];
            let buf_bytes =
                u32::try_from(entries.len() * size_of::<SystemProcessorPerformanceInformation>())
                    .ok()?;
            let mut ret_len: u32 = 0;
            let status = unsafe {
                nt_query(
                    SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
                    entries.as_mut_ptr() as *mut c_void,
                    buf_bytes,
                    &mut ret_len,
                )
            };
            if status < 0 {
                return None;
            }
            let elem_size = size_of::<SystemProcessorPerformanceInformation>();
            let valid = (ret_len as usize / elem_size).min(entries.len());
            return aggregate_processor_performance(&entries[..valid]);
        }
    }

    None
}

fn aggregate_processor_performance(
    entries: &[SystemProcessorPerformanceInformation],
) -> Option<(u64, u64, u64)> {
    if entries.is_empty() {
        return None;
    }
    let mut total_idle: u64 = 0;
    let mut total_kernel: u64 = 0;
    let mut total_user: u64 = 0;
    for e in entries {
        total_idle = total_idle.saturating_add(e.idle_time.max(0) as u64);
        total_kernel = total_kernel.saturating_add(e.kernel_time.max(0) as u64);
        total_user = total_user.saturating_add(e.user_time.max(0) as u64);
    }
    Some((total_idle, total_kernel, total_user))
}

fn compute_cpu_usage_pct(
    prev_idle: u64,
    prev_kernel: u64,
    prev_user: u64,
    idle: u64,
    kernel: u64,
    user: u64,
) -> f32 {
    let di = idle.saturating_sub(prev_idle);
    let dk = kernel.saturating_sub(prev_kernel);
    let du = user.saturating_sub(prev_user);
    let total = dk.saturating_add(du);
    if total == 0 {
        0.0
    } else {
        let busy = total.saturating_sub(di);
        ((busy as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32
    }
}

fn ft_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

fn read_cpu_base_mhz() -> Option<f32> {
    unsafe {
        let count = GetActiveProcessorCount(ALL_PROCESSOR_GROUPS).max(1) as usize;
        let mut buf = vec![ProcessorPowerInformation::default(); count];
        let buf_bytes = u32::try_from(count * size_of::<ProcessorPowerInformation>()).ok()?;
        let status = CallNtPowerInformation(
            ProcessorInformation,
            None,
            0,
            Some(buf.as_mut_ptr() as *mut c_void),
            buf_bytes,
        );
        if status.is_ok() && buf[0].max_mhz > 0 {
            return Some(buf[0].max_mhz as f32);
        }
    }
    None
}

fn init_cpu_perf_pdh() -> (isize, isize) {
    unsafe {
        let mut query: isize = 0;
        if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
            return (0, 0);
        }
        let path: Vec<u16> = "\\Processor Information(_Total)\\% Processor Performance\0"
            .encode_utf16()
            .collect();
        let mut counter: isize = 0;
        if PdhAddEnglishCounterW(query, PCWSTR(path.as_ptr()), 0, &mut counter) != 0 {
            let _ = PdhCloseQuery(query);
            return (0, 0);
        }
        let _ = PdhCollectQueryData(query);
        (query, counter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn cpu_usage_math_clamps_and_handles_zero_delta() {
        assert_eq!(compute_cpu_usage_pct(100, 200, 100, 100, 200, 100), 0.0);
        let pct = compute_cpu_usage_pct(100, 200, 100, 150, 300, 200);
        // dk=100, du=100 => total=200; di=50 => busy=150 => 75%
        assert!((pct - 75.0).abs() < 1e-4);
        // Idle delta exceeds total => saturates to 0%
        assert_eq!(compute_cpu_usage_pct(100, 200, 100, 500, 250, 150), 0.0);
    }

    #[test]
    fn filetime_conversion_combines_high_and_low_parts() {
        let ft = FILETIME {
            dwLowDateTime: 0x89AB_CDEF,
            dwHighDateTime: 0x0123_4567,
        };
        assert_eq!(ft_to_u64(ft), 0x0123_4567_89AB_CDEF);
    }

    #[test]
    fn aggregates_over_64_logical_processors_across_groups() {
        assert_eq!(aggregate_processor_performance(&[]), None);

        // Simulate 128 logical processors (two 64-processor groups):
        // Group 0: 64 processors, each with idle=50, kernel=100, user=0 (50% busy)
        // Group 1: 64 processors, each with idle=100, kernel=100, user=0 (0% busy)
        let mut group0 = vec![
            SystemProcessorPerformanceInformation {
                idle_time: 50,
                kernel_time: 100,
                user_time: 0,
                ..Default::default()
            };
            64
        ];
        let group1 = vec![
            SystemProcessorPerformanceInformation {
                idle_time: 100,
                kernel_time: 100,
                user_time: 0,
                ..Default::default()
            };
            64
        ];
        let (i0, k0, u0) = aggregate_processor_performance(&group0).unwrap();
        let (i1, k1, u1) = aggregate_processor_performance(&group1).unwrap();
        let pct_t1 = compute_cpu_usage_pct(0, 0, 0, i0 + i1, k0 + k1, u0 + u1);
        // Total kernel = 12800, idle = 3200 + 6400 = 9600 => busy = 3200 => 25.0%
        assert!((pct_t1 - 25.0).abs() < 1e-4);

        // Next sample: Group 0 idle stays same, kernel +100; Group 1 all idle +100
        for e in &mut group0 {
            e.kernel_time += 100;
        }
        let (ni0, nk0, nu0) = aggregate_processor_performance(&group0).unwrap();
        let pct_t2 = compute_cpu_usage_pct(i0 + i1, k0 + k1, u0 + u1, ni0 + i1, nk0 + k1, nu0 + u1);
        assert!((pct_t2 - 100.0).abs() < 1e-4);
    }

    #[test]
    fn cpu_usage_distinguishes_unready_failure_and_real_zero() {
        let mut cpu = CpuMonitor::new_lazy();

        // First sample establishes baseline -> returns None (not ready), not 0.0
        let first =
            cpu.sample_usage_with_reader(|| Some((CpuTimesSource::NtAllGroups, 100, 200, 50)));
        assert_eq!(first, None);

        // Second sample with zero busy delta -> real Some(0.0)
        let second =
            cpu.sample_usage_with_reader(|| Some((CpuTimesSource::NtAllGroups, 200, 300, 50)));
        assert_eq!(second, Some(0.0));

        // API failure -> returns None and invalidates baseline
        let failed = cpu.sample_usage_with_reader(|| None);
        assert_eq!(failed, None);
        assert!(!cpu.initialized);

        // Recovery requires two valid samples before returning Some(...)
        let rec1 =
            cpu.sample_usage_with_reader(|| Some((CpuTimesSource::GetSystemTimes, 300, 400, 100)));
        assert_eq!(rec1, None);
        let rec2 =
            cpu.sample_usage_with_reader(|| Some((CpuTimesSource::GetSystemTimes, 350, 500, 200)));
        assert!((rec2.unwrap() - 75.0).abs() < 1e-4);
    }

    #[test]
    fn lazy_cpu_monitor_defers_pdh_until_freq_demanded_and_releases() {
        let mut cpu = CpuMonitor::new_lazy();
        assert!(!cpu.has_freq_pdh());
        assert!(!cpu.pdh_init_attempted);
        assert!(!cpu.base_mhz_attempted);

        let (usage, freq) = cpu.sample_with_freq(false);
        // First sample only captures baseline -> None
        assert!(usage.is_none());
        assert!(freq.is_none());
        assert!(!cpu.has_freq_pdh());
        assert!(!cpu.pdh_init_attempted);
        assert!(!cpu.base_mhz_attempted);

        std::thread::sleep(Duration::from_millis(20));
        let (usage2, _) = cpu.sample_with_freq(true);
        assert!(usage2.is_some_and(|v| v.is_finite() && (0.0..=100.0).contains(&v)));
        assert!(cpu.pdh_init_attempted);
        assert!(cpu.base_mhz_attempted);

        if cpu.has_freq_pdh() {
            assert!(cpu.release_freq_pdh());
            assert!(!cpu.has_freq_pdh());
            assert!(!cpu.pdh_init_attempted);
        } else {
            // Even when PDH handle wasn't opened, release resets attempted/backoff state.
            assert!(cpu.release_freq_pdh());
        }
        assert!(!cpu.release_freq_pdh());
    }

    #[test]
    fn cpu_release_resets_backoff_even_when_handle_closed() {
        let mut cpu = CpuMonitor::new_lazy();
        let t0 = Instant::now();
        cpu.pdh_backoff.record_failure(t0);
        cpu.pdh_init_attempted = true;
        assert!(!cpu.has_freq_pdh());
        assert!(!cpu.pdh_backoff.can_attempt(t0 + Duration::from_secs(1)));

        assert!(cpu.release_freq_pdh());
        assert!(!cpu.pdh_init_attempted);
        assert!(!cpu.pdh_backoff.is_backing_off());
        assert!(cpu.pdh_backoff.can_attempt(t0 + Duration::from_secs(1)));
        assert!(!cpu.release_freq_pdh());
    }

    #[test]
    fn live_nt_all_groups_cpu_times_succeeds() {
        let res = read_nt_all_groups_cpu_times();
        assert!(res.is_some());
    }
}
