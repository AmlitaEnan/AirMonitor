use std::ffi::c_void;
use windows::core::PCWSTR;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE,
};
use windows::Win32::System::Power::{CallNtPowerInformation, ProcessorInformation};
use windows::Win32::System::Threading::{
    GetActiveProcessorCount, GetSystemTimes, ALL_PROCESSOR_GROUPS,
};

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

const _: () = assert!(std::mem::size_of::<ProcessorPowerInformation>() == 24);

pub struct CpuMonitor {
    prev_idle: u64,
    prev_kernel: u64,
    prev_user: u64,
    initialized: bool,
    base_mhz: Option<f32>,
    base_mhz_attempted: bool,
    pdh_query: isize,
    pdh_perf_counter: isize,
    pdh_init_attempted: bool,
}

impl CpuMonitor {
    pub fn new() -> Self {
        let base_mhz = read_cpu_base_mhz();
        let (pdh_query, pdh_perf_counter) = init_cpu_perf_pdh();
        let mut monitor = Self {
            prev_idle: 0,
            prev_kernel: 0,
            prev_user: 0,
            initialized: false,
            base_mhz,
            base_mhz_attempted: true,
            pdh_query,
            pdh_perf_counter,
            pdh_init_attempted: true,
        };
        let _ = monitor.sample_with_freq(true);
        monitor
    }

    pub fn new_lazy() -> Self {
        let mut monitor = Self {
            prev_idle: 0,
            prev_kernel: 0,
            prev_user: 0,
            initialized: false,
            base_mhz: None,
            base_mhz_attempted: false,
            pdh_query: 0,
            pdh_perf_counter: 0,
            pdh_init_attempted: false,
        };
        let _ = monitor.sample_with_freq(false);
        monitor
    }

    pub fn sample(&mut self) -> (f32, Option<f32>) {
        self.sample_with_freq(true)
    }

    pub fn sample_with_freq(&mut self, need_freq: bool) -> (f32, Option<f32>) {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();

        let usage = unsafe {
            if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() {
                let i = ft_to_u64(idle);
                let k = ft_to_u64(kernel);
                let u = ft_to_u64(user);
                if !self.initialized {
                    self.prev_idle = i;
                    self.prev_kernel = k;
                    self.prev_user = u;
                    self.initialized = true;
                    0.0
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
                    pct
                }
            } else {
                // HardwareMetrics::cpu_usage_pct is f32 (not Option<f32>) in src/state.rs.
                0.0
            }
        };

        let freq_ghz = if need_freq {
            self.sample_freq_ghz()
        } else {
            None
        };
        (usage, freq_ghz)
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
        self.pdh_init_attempted = false;
        was_open
    }

    pub(crate) fn has_freq_pdh(&self) -> bool {
        self.pdh_query != 0
    }

    fn sample_freq_ghz(&mut self) -> Option<f32> {
        if !self.base_mhz_attempted {
            self.base_mhz_attempted = true;
            self.base_mhz = read_cpu_base_mhz();
        }
        let base_mhz = self.base_mhz.filter(|&m| m > 0.0)?;
        if !self.pdh_init_attempted {
            self.pdh_init_attempted = true;
            let (q, c) = init_cpu_perf_pdh();
            self.pdh_query = q;
            self.pdh_perf_counter = c;
        }
        if self.pdh_query == 0 || self.pdh_perf_counter == 0 {
            return None;
        }
        unsafe {
            if PdhCollectQueryData(self.pdh_query) != 0 {
                return None;
            }
            let mut val = PDH_FMT_COUNTERVALUE::default();
            if PdhGetFormattedCounterValue(self.pdh_perf_counter, PDH_FMT_DOUBLE, None, &mut val)
                == 0
            {
                let pct = val.Anonymous.doubleValue;
                if pct.is_finite() && pct > 1.0 && pct < 500.0 {
                    let ghz = (base_mhz as f64 * (pct / 100.0)) / 1000.0;
                    return Some(ghz as f32);
                }
            }
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
        let buf_bytes =
            u32::try_from(count * std::mem::size_of::<ProcessorPowerInformation>()).ok()?;
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
    fn lazy_cpu_monitor_defers_pdh_until_freq_demanded_and_releases() {
        let mut cpu = CpuMonitor::new_lazy();
        assert!(!cpu.has_freq_pdh());
        assert!(!cpu.pdh_init_attempted);
        assert!(!cpu.base_mhz_attempted);

        let (usage, freq) = cpu.sample_with_freq(false);
        assert!(usage.is_finite() && (0.0..=100.0).contains(&usage));
        assert!(freq.is_none());
        assert!(!cpu.has_freq_pdh());
        assert!(!cpu.pdh_init_attempted);
        assert!(!cpu.base_mhz_attempted);

        let _ = cpu.sample_with_freq(true);
        assert!(cpu.pdh_init_attempted);
        assert!(cpu.base_mhz_attempted);

        if cpu.has_freq_pdh() {
            assert!(cpu.release_freq_pdh());
            assert!(!cpu.has_freq_pdh());
            assert!(!cpu.pdh_init_attempted);
        }
        assert!(!cpu.release_freq_pdh());
    }
}
