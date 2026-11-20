use windows::core::PCWSTR;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE,
};

const DISK_READ_COUNTER: &str = "\\PhysicalDisk(_Total)\\Disk Read Bytes/sec\0";
const DISK_WRITE_COUNTER: &str = "\\PhysicalDisk(_Total)\\Disk Write Bytes/sec\0";

pub struct DiskMonitor {
    pdh_query: isize,
    read_counter: isize,
    write_counter: isize,
    primed: bool,
    last_collect_ok: bool,
    init_status: String,
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DiskMonitor>();
};

impl DiskMonitor {
    pub fn new() -> Self {
        unsafe {
            let mut query: isize = 0;
            let open_status = PdhOpenQueryW(PCWSTR::null(), 0, &mut query);
            if open_status != 0 || query == 0 {
                return Self {
                    pdh_query: 0,
                    read_counter: 0,
                    write_counter: 0,
                    primed: false,
                    last_collect_ok: false,
                    init_status: format!("PdhOpenQueryW failed (0x{:08X})", open_status as u32),
                };
            }

            let read_w: Vec<u16> = DISK_READ_COUNTER.encode_utf16().collect();
            let write_w: Vec<u16> = DISK_WRITE_COUNTER.encode_utf16().collect();

            let mut read_counter: isize = 0;
            let read_status =
                PdhAddEnglishCounterW(query, PCWSTR(read_w.as_ptr()), 0, &mut read_counter);
            if read_status != 0 {
                read_counter = 0;
            }

            let mut write_counter: isize = 0;
            let write_status =
                PdhAddEnglishCounterW(query, PCWSTR(write_w.as_ptr()), 0, &mut write_counter);
            if write_status != 0 {
                write_counter = 0;
            }

            if read_counter == 0 && write_counter == 0 {
                let _ = PdhCloseQuery(query);
                return Self {
                    pdh_query: 0,
                    read_counter: 0,
                    write_counter: 0,
                    primed: false,
                    last_collect_ok: false,
                    init_status: format!(
                        "PdhAddEnglishCounterW failed (read=0x{:08X}, write=0x{:08X})",
                        read_status as u32, write_status as u32
                    ),
                };
            }

            let primed = PdhCollectQueryData(query) == 0;
            let init_status = format!(
                "PDH ready (read={}, write={}, primed={})",
                read_counter != 0,
                write_counter != 0,
                primed
            );

            Self {
                pdh_query: query,
                read_counter,
                write_counter,
                primed,
                last_collect_ok: primed,
                init_status,
            }
        }
    }

    pub fn sample(&mut self) -> (Option<f64>, Option<f64>) {
        if self.pdh_query == 0 {
            return (None, None);
        }

        unsafe {
            if PdhCollectQueryData(self.pdh_query) != 0 {
                self.last_collect_ok = false;
                return (None, None);
            }
            self.last_collect_ok = true;

            if !self.primed {
                self.primed = true;
                return (None, None);
            }

            let read_bps = read_pdh_counter_bps(self.read_counter);
            let write_bps = read_pdh_counter_bps(self.write_counter);
            (read_bps, write_bps)
        }
    }

    pub fn release(&mut self) -> bool {
        let was_open = self.pdh_query != 0;
        if self.pdh_query != 0 {
            unsafe {
                let _ = PdhCloseQuery(self.pdh_query);
            }
            self.pdh_query = 0;
        }
        self.read_counter = 0;
        self.write_counter = 0;
        self.primed = false;
        self.last_collect_ok = false;
        was_open
    }

    pub fn diagnostics(&self) -> String {
        format!(
            "DiskMonitor(PDH): {} | active_query={} | last_collect_ok={}",
            self.init_status,
            self.pdh_query != 0,
            self.last_collect_ok
        )
    }
}

impl Default for DiskMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DiskMonitor {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

unsafe fn read_pdh_counter_bps(counter: isize) -> Option<f64> {
    if counter == 0 {
        return None;
    }
    let mut val = PDH_FMT_COUNTERVALUE::default();
    if PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, None, &mut val) == 0 {
        sanitize_disk_bps(val.Anonymous.doubleValue)
    } else {
        None
    }
}

fn sanitize_disk_bps(value: f64) -> Option<f64> {
    if value.is_finite() && value >= 0.0 {
        Some(value)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn sanitize_disk_bps_rejects_nan_infinity_and_negative() {
        assert_eq!(sanitize_disk_bps(f64::NAN), None);
        assert_eq!(sanitize_disk_bps(f64::INFINITY), None);
        assert_eq!(sanitize_disk_bps(f64::NEG_INFINITY), None);
        assert_eq!(sanitize_disk_bps(-0.001), None);
        assert_eq!(sanitize_disk_bps(-1024.0), None);
        assert_eq!(sanitize_disk_bps(0.0), Some(0.0));
        assert_eq!(sanitize_disk_bps(12_345.5), Some(12_345.5));
    }

    #[test]
    fn disk_monitor_samples_and_reports_diagnostics() {
        let mut monitor = DiskMonitor::default();
        let diag = monitor.diagnostics();
        assert!(diag.contains("DiskMonitor(PDH):"));

        thread::sleep(Duration::from_millis(40));
        let (read_bps, write_bps) = monitor.sample();
        if let Some(r) = read_bps {
            assert!(r.is_finite() && r >= 0.0);
        }
        if let Some(w) = write_bps {
            assert!(w.is_finite() && w >= 0.0);
        }
    }

    #[test]
    fn disk_monitor_release_closes_pdh_and_is_idempotent() {
        let mut monitor = DiskMonitor::new();
        let was_open = monitor.pdh_query != 0;
        assert_eq!(monitor.release(), was_open);
        assert_eq!(monitor.pdh_query, 0);
        assert!(!monitor.release());
        assert_eq!(monitor.sample(), (None, None));
    }
}
