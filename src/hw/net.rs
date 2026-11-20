use std::collections::HashMap;
use std::time::Instant;
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::{
    IfOperStatusUp, MediaConnectStateConnected, NET_IF_ACCESS_LOOPBACK,
};

type NicCounters = HashMap<u64, (u64, u64)>;

pub struct NetMonitor {
    prev_counters: NicCounters,
    prev_time: Instant,
    initialized: bool,
    active_name: String,
}

impl NetMonitor {
    pub fn new() -> Self {
        let mut m = Self {
            prev_counters: HashMap::new(),
            prev_time: Instant::now(),
            initialized: false,
            active_name: String::new(),
        };
        let _ = m.sample();
        m
    }

    pub fn sample(&mut self) -> (f64, f64, String) {
        let (curr_counters, primary_name) = read_physical_nic_counters();
        let now = Instant::now();

        if !primary_name.is_empty() {
            self.active_name = primary_name;
        }

        if !self.initialized {
            self.prev_counters = curr_counters;
            self.prev_time = now;
            self.initialized = true;
            return (0.0, 0.0, self.active_name.clone());
        }

        let dt = now.duration_since(self.prev_time).as_secs_f64();
        self.prev_time = now;

        let (up_bps, down_bps) = step_nic_sample(&mut self.prev_counters, curr_counters, dt);
        (up_bps, down_bps, self.active_name.clone())
    }
}

impl Default for NetMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Computes `(up_bps, down_bps)` by diffing only NICs present in both `prev_counters`
/// and `curr_counters`, then unconditionally updating `prev_counters` to `curr_counters`
/// (including when `dt < 0.05` so the next sample will not overestimate rates).
fn step_nic_sample(
    prev_counters: &mut NicCounters,
    curr_counters: NicCounters,
    dt: f64,
) -> (f64, f64) {
    if !dt.is_finite() || dt < 0.05 {
        *prev_counters = curr_counters;
        return (0.0, 0.0);
    }

    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;

    for (luid, &(cur_in, cur_out)) in &curr_counters {
        if let Some(&(prev_in, prev_out)) = prev_counters.get(luid) {
            total_in = total_in.saturating_add(cur_in.saturating_sub(prev_in));
            total_out = total_out.saturating_add(cur_out.saturating_sub(prev_out));
        }
    }

    *prev_counters = curr_counters;

    let down_bps = total_in as f64 / dt;
    let up_bps = total_out as f64 / dt;
    (up_bps, down_bps)
}

fn read_physical_nic_counters() -> (NicCounters, String) {
    unsafe {
        let mut table_ptr: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table_ptr).is_err() || table_ptr.is_null() {
            return (HashMap::new(), String::new());
        }

        let count = (*table_ptr).NumEntries as usize;
        let table_base = std::ptr::addr_of!((*table_ptr).Table) as *const MIB_IF_ROW2;
        let rows = if count == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(table_base, count)
        };

        let mut counters = HashMap::new();
        let mut best_traffic: u64 = 0;
        let mut best_name = String::new();

        for row in rows {
            if row.OperStatus != IfOperStatusUp {
                continue;
            }
            if row.MediaConnectState != MediaConnectStateConnected {
                continue;
            }
            if row.AccessType == NET_IF_ACCESS_LOOPBACK {
                continue;
            }
            // Only count physical hardware interfaces (avoid virtual filter/vEthernet duplicates)
            // InterfaceAndOperStatusFlags bit 0 = HardwareInterface, bit 1 = FilterInterface
            let flags = row.InterfaceAndOperStatusFlags._bitfield;
            let is_hardware = (flags & 0x01) != 0;
            let is_filter = (flags & 0x02) != 0;
            if !is_hardware || is_filter {
                continue;
            }
            // Standard Ethernet (6) or IEEE 802.11 Wi-Fi (71)
            if row.Type != 6 && row.Type != 71 {
                continue;
            }

            let desc = utf16_slice_to_string(&row.Description);
            let desc_lower = desc.to_lowercase();
            if desc_lower.contains("virtual")
                || desc_lower.contains("hyper-v")
                || desc_lower.contains("vmware")
                || desc_lower.contains("vbox")
                || desc_lower.contains("tap-")
                || desc_lower.contains("wintun")
                || desc_lower.contains("loopback")
            {
                continue;
            }

            let luid = row.InterfaceLuid.Value;
            counters.insert(luid, (row.InOctets, row.OutOctets));

            let traffic = row.InOctets.saturating_add(row.OutOctets);
            if traffic >= best_traffic {
                best_traffic = traffic;
                best_name = desc;
            }
        }

        FreeMibTable(table_ptr as *const _);
        (counters, best_name)
    }
}

fn utf16_slice_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_increment_sums_present_nics() {
        let mut prev = HashMap::from([(1, (1000, 500)), (2, (2000, 1000))]);
        let curr = HashMap::from([(1, (3000, 1500)), (2, (5000, 2000))]);
        // delta_in = 2000 + 3000 = 5000; delta_out = 1000 + 1000 = 2000
        let (up, down) = step_nic_sample(&mut prev, curr, 2.0);
        assert!((up - 1000.0).abs() < 1e-6);
        assert!((down - 2500.0).abs() < 1e-6);
        assert_eq!(prev.get(&1), Some(&(3000, 1500)));
        assert_eq!(prev.get(&2), Some(&(5000, 2000)));
    }

    #[test]
    fn new_nic_online_causes_no_spike_then_tracks_delta() {
        let mut prev = HashMap::from([(1, (1000, 500))]);
        // NIC 2 wakes up with 10 GB pre-existing cumulative counters
        let curr1 = HashMap::from([(1, (2000, 1000)), (2, (10_000_000_000, 5_000_000_000))]);
        let (up1, down1) = step_nic_sample(&mut prev, curr1, 1.0);
        assert!((up1 - 500.0).abs() < 1e-6);
        assert!((down1 - 1000.0).abs() < 1e-6);

        // Next frame: NIC 2's new delta IS tracked normally
        let curr2 = HashMap::from([(1, (2000, 1000)), (2, (10_000_004_000, 5_000_002_000))]);
        let (up2, down2) = step_nic_sample(&mut prev, curr2, 1.0);
        assert!((up2 - 2000.0).abs() < 1e-6);
        assert!((down2 - 4000.0).abs() < 1e-6);
    }

    #[test]
    fn nic_offline_does_not_corrupt_remaining_nics() {
        let mut prev = HashMap::from([(1, (1000, 500)), (2, (9000, 9000))]);
        // NIC 2 goes offline
        let curr = HashMap::from([(1, (2500, 1500))]);
        let (up, down) = step_nic_sample(&mut prev, curr, 1.0);
        assert!((up - 1000.0).abs() < 1e-6);
        assert!((down - 1500.0).abs() < 1e-6);
        assert!(!prev.contains_key(&2));
    }

    #[test]
    fn counter_reset_or_wrap_saturates_to_zero() {
        let mut prev = HashMap::from([(1, (5000, 4000))]);
        let curr = HashMap::from([(1, (100, 0))]);
        let (up, down) = step_nic_sample(&mut prev, curr, 1.0);
        assert_eq!(up, 0.0);
        assert_eq!(down, 0.0);
        assert_eq!(prev.get(&1), Some(&(100, 0)));
    }

    #[test]
    fn short_dt_returns_zero_and_updates_baseline() {
        let mut prev = HashMap::from([(1, (1000, 1000))]);
        // Short dt (< 0.05s) where counter jumped to 2000
        let curr1 = HashMap::from([(1, (2000, 2000))]);
        let (up1, down1) = step_nic_sample(&mut prev, curr1, 0.01);
        assert_eq!(up1, 0.0);
        assert_eq!(down1, 0.0);
        // Baseline must have updated to 2000, so next 1.0s frame at 2500 reports 500 B/s, not 1500 B/s
        let curr2 = HashMap::from([(1, (2500, 2500))]);
        let (up2, down2) = step_nic_sample(&mut prev, curr2, 1.0);
        assert!((up2 - 500.0).abs() < 1e-6);
        assert!((down2 - 500.0).abs() < 1e-6);
    }
}
