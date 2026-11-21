use std::time::Instant;
use windows::core::PCWSTR;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE,
};

use super::pdh::PdhBackoff;

const DISK_READ_COUNTER: &str = "\\PhysicalDisk(_Total)\\Disk Read Bytes/sec\0";
const DISK_WRITE_COUNTER: &str = "\\PhysicalDisk(_Total)\\Disk Write Bytes/sec\0";

pub struct DiskMonitor {
    pdh_query: isize,
    read_counter: isize,
    write_counter: isize,
    primed: bool,
    last_collect_ok: bool,
    init_status: String,
    backoff: PdhBackoff,
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DiskMonitor>();
};

impl DiskMonitor {
    pub fn new() -> Self {
        let mut monitor = Self {
            pdh_query: 0,
            read_counter: 0,
            write_counter: 0,
            primed: false,
            last_collect_ok: false,
            init_status: String::new(),
            backoff: PdhBackoff::default(),
        };
        monitor.try_init_at(Instant::now());
        monitor
    }

    fn try_init_at(&mut self, now: Instant) -> bool {
        let (query, read_counter, write_counter, primed, status) = init_disk_pdh();
        self.pdh_query = query;
        self.read_counter = read_counter;
        self.write_counter = write_counter;
        self.primed = primed;
        self.last_collect_ok = primed;
        self.init_status = status;
        if query == 0 {
            self.backoff.record_failure(now);
            false
        } else {
            self.backoff.record_success();
            true
        }
    }

    pub fn sample(&mut self) -> (Option<f64>, Option<f64>) {
        self.sample_at(Instant::now())
    }

    fn sample_at(&mut self, now: Instant) -> (Option<f64>, Option<f64>) {
        if self.pdh_query == 0 && (!self.backoff.can_attempt(now) || !self.try_init_at(now)) {
            return (None, None);
        }

        unsafe {
            if PdhCollectQueryData(self.pdh_query) != 0 {
                self.close_query_for_failure(now);
                return (None, None);
            }
            self.last_collect_ok = true;
            self.backoff.record_success();

            if !self.primed {
                self.primed = true;
                return (None, None);
            }

            let read_bps = read_pdh_counter_bps(self.read_counter);
            let write_bps = read_pdh_counter_bps(self.write_counter);
            (read_bps, write_bps)
        }
    }

    fn close_query_for_failure(&mut self, now: Instant) {
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
        self.backoff.record_failure(now);
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
        let had_backoff = self.backoff.reset();
        was_open || had_backoff
    }

    pub fn diagnostics(&self) -> String {
        format!(
            "DiskMonitor(PDH): {} | active_query={} | last_collect_ok={} | backing_off={}",
            self.init_status,
            self.pdh_query != 0,
            self.last_collect_ok,
            self.backoff.is_backing_off(),
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

fn init_disk_pdh() -> (isize, isize, isize, bool, String) {
    unsafe {
        let mut query: isize = 0;
        let open_status = PdhOpenQueryW(PCWSTR::null(), 0, &mut query);
        if open_status != 0 || query == 0 {
            return (
                0,
                0,
                0,
                false,
                format!("PdhOpenQueryW failed (0x{:08X})", open_status as u32),
            );
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
            return (
                0,
                0,
                0,
                false,
                format!(
                    "PdhAddEnglishCounterW failed (read=0x{:08X}, write=0x{:08X})",
                    read_status as u32, write_status as u32
                ),
            );
        }

        let primed = PdhCollectQueryData(query) == 0;
        let init_status = format!(
            "PDH ready (read={}, write={}, primed={})",
            read_counter != 0,
            write_counter != 0,
            primed
        );
        (query, read_counter, write_counter, primed, init_status)
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
        let was_active = monitor.pdh_query != 0 || monitor.backoff.is_backing_off();
        assert_eq!(monitor.release(), was_active);
        assert_eq!(monitor.pdh_query, 0);
        assert!(!monitor.backoff.is_backing_off());
        assert!(!monitor.release());

        // After release, sampling re-initializes if needed; simulated backoff prevents immediate retry
        let t0 = Instant::now();
        monitor.close_query_for_failure(t0);
        assert!(monitor.backoff.is_backing_off());
        assert_eq!(monitor.sample_at(t0 + Duration::from_secs(1)), (None, None));
        assert_eq!(monitor.pdh_query, 0);

        // release() clears backoff even when pdh_query == 0
        assert!(monitor.release());
        assert!(!monitor.backoff.is_backing_off());
        assert!(!monitor.release());
    }
}
