use std::collections::HashMap;
use std::time::Instant;
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::{
    IfOperStatusUp, MediaConnectStateConnected, NET_IF_ACCESS_LOOPBACK,
};

type NicCounters = HashMap<u64, (u64, u64)>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct NicEntry {
    name: String,
    in_octets: u64,
    out_octets: u64,
}

type NicTable = HashMap<u64, NicEntry>;

pub struct NetMonitor {
    prev_counters: NicCounters,
    prev_time: Option<Instant>,
    baseline_ready: bool,
    active_name: String,
}

impl NetMonitor {
    pub fn new() -> Self {
        Self {
            prev_counters: HashMap::new(),
            prev_time: None,
            baseline_ready: false,
            active_name: String::new(),
        }
    }

    pub fn sample(&mut self) -> Option<(f64, f64, String)> {
        let now = Instant::now();
        let res = read_physical_nic_counters();
        self.sample_with_table(res, now)
    }

    fn sample_with_table(
        &mut self,
        table: Result<NicTable, ()>,
        now: Instant,
    ) -> Option<(f64, f64, String)> {
        let Ok(curr) = table else {
            // Preserve `prev_counters` and `active_name`, but require two fresh
            // samples after recovery before emitting rates.
            self.baseline_ready = false;
            self.prev_time = None;
            return None;
        };

        if !self.baseline_ready || self.prev_time.is_none() {
            self.prev_counters = to_counter_map(&curr);
            self.prev_time = Some(now);
            self.baseline_ready = true;
            return None;
        }

        let prev_time = self.prev_time.unwrap_or(now);
        let dt = now.saturating_duration_since(prev_time).as_secs_f64();
        self.prev_time = Some(now);

        let (up_bps, down_bps) =
            step_nic_sample(&mut self.prev_counters, &mut self.active_name, curr, dt)?;
        Some((up_bps, down_bps, self.active_name.clone()))
    }
}

impl Default for NetMonitor {
    fn default() -> Self {
        Self::new()
    }
}

fn to_counter_map(table: &NicTable) -> NicCounters {
    table
        .iter()
        .map(|(&luid, entry)| (luid, (entry.in_octets, entry.out_octets)))
        .collect()
}

/// Computes `(up_bps, down_bps)` by diffing only NICs present in both `prev_counters`
/// and `curr`, selecting `active_name` by the largest per-interval byte delta (keeping
/// the previous `active_name` if all deltas are zero), and updating `prev_counters`.
/// Returns `None` when `dt < 0.05` (while still updating the baseline).
fn step_nic_sample(
    prev_counters: &mut NicCounters,
    active_name: &mut String,
    curr: NicTable,
    dt: f64,
) -> Option<(f64, f64)> {
    if !dt.is_finite() || dt < 0.05 {
        *prev_counters = to_counter_map(&curr);
        return None;
    }

    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;
    let mut best_delta: u64 = 0;
    let mut best_name: Option<String> = None;

    for (luid, entry) in &curr {
        if let Some(&(prev_in, prev_out)) = prev_counters.get(luid) {
            let d_in = entry.in_octets.saturating_sub(prev_in);
            let d_out = entry.out_octets.saturating_sub(prev_out);
            total_in = total_in.saturating_add(d_in);
            total_out = total_out.saturating_add(d_out);

            let d_total = d_in.saturating_add(d_out);
            if d_total > best_delta && !entry.name.is_empty() {
                best_delta = d_total;
                best_name = Some(entry.name.clone());
            }
        }
    }

    if let Some(name) = best_name {
        *active_name = name;
    }

    *prev_counters = to_counter_map(&curr);

    let down_bps = total_in as f64 / dt;
    let up_bps = total_out as f64 / dt;
    Some((up_bps, down_bps))
}

fn read_physical_nic_counters() -> Result<NicTable, ()> {
    unsafe {
        let mut table_ptr: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table_ptr).is_err() || table_ptr.is_null() {
            return Err(());
        }

        let count = (*table_ptr).NumEntries as usize;
        let table_base = std::ptr::addr_of!((*table_ptr).Table) as *const MIB_IF_ROW2;
        let rows = if count == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(table_base, count)
        };

        let mut entries = HashMap::new();

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
            entries.insert(
                luid,
                NicEntry {
                    name: desc,
                    in_octets: row.InOctets,
                    out_octets: row.OutOctets,
                },
            );
        }

        FreeMibTable(table_ptr as *const _);
        Ok(entries)
    }
}

fn utf16_slice_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_table(items: &[(u64, &str, u64, u64)]) -> NicTable {
        items
            .iter()
            .map(|&(luid, name, in_octets, out_octets)| {
                (
                    luid,
                    NicEntry {
                        name: name.to_string(),
                        in_octets,
                        out_octets,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn normal_increment_sums_present_nics_and_picks_max_delta_nic() {
        let mut prev = HashMap::from([(1, (100_000_000, 50_000_000)), (2, (2000, 1000))]);
        let mut active = String::from("NIC-1");
        // NIC 1 has huge historical bytes but only +1000/+500 delta;
        // NIC 2 has tiny historical bytes but +3000/+1000 delta -> NIC 2 must be active.
        let curr = make_table(&[
            (1, "NIC-1", 100_001_000, 50_000_500),
            (2, "NIC-2", 5000, 2000),
        ]);
        let (up, down) = step_nic_sample(&mut prev, &mut active, curr, 2.0).unwrap();
        assert!((up - 750.0).abs() < 1e-6);
        assert!((down - 2000.0).abs() < 1e-6);
        assert_eq!(active, "NIC-2");
        assert_eq!(prev.get(&1), Some(&(100_001_000, 50_000_500)));
        assert_eq!(prev.get(&2), Some(&(5000, 2000)));
    }

    #[test]
    fn zero_delta_preserves_previous_active_nic_name() {
        let mut prev = HashMap::from([(1, (10_000, 5_000)), (2, (1_000, 500))]);
        let mut active = String::from("NIC-2");
        let curr = make_table(&[(1, "NIC-1", 10_000, 5_000), (2, "NIC-2", 1_000, 500)]);
        let (up, down) = step_nic_sample(&mut prev, &mut active, curr, 1.0).unwrap();
        assert_eq!(up, 0.0);
        assert_eq!(down, 0.0);
        assert_eq!(active, "NIC-2");
    }

    #[test]
    fn api_failure_preserves_prev_counters_and_requires_two_samples_on_recovery() {
        let mut mon = NetMonitor::new();
        let t0 = Instant::now();

        // Sample 1: baseline -> None
        assert_eq!(
            mon.sample_with_table(Ok(make_table(&[(1, "Eth0", 1000, 500)])), t0),
            None
        );

        // Sample 2: valid rate -> Some
        let s2 = mon
            .sample_with_table(
                Ok(make_table(&[(1, "Eth0", 3000, 1500)])),
                t0 + Duration::from_secs(1),
            )
            .expect("second sample ready");
        assert!((s2.0 - 1000.0).abs() < 1e-6);
        assert!((s2.1 - 2000.0).abs() < 1e-6);
        assert_eq!(s2.2, "Eth0");

        // Sample 3: GetIfTable2 failure -> returns None, keeps prev_counters & active_name
        assert_eq!(
            mon.sample_with_table(Err(()), t0 + Duration::from_secs(2)),
            None
        );
        assert_eq!(mon.prev_counters.get(&1), Some(&(3000, 1500)));
        assert_eq!(mon.active_name, "Eth0");
        assert!(!mon.baseline_ready);

        // Sample 4 (1st sample after recovery): refreshes baseline -> must return None!
        assert_eq!(
            mon.sample_with_table(
                Ok(make_table(&[(1, "Eth0", 10_000, 5_000)])),
                t0 + Duration::from_secs(3),
            ),
            None
        );
        assert!(mon.baseline_ready);

        // Sample 5 (2nd sample after recovery): produces valid delta against sample 4
        let s5 = mon
            .sample_with_table(
                Ok(make_table(&[(1, "Eth0", 14_000, 7_000)])),
                t0 + Duration::from_secs(4),
            )
            .expect("second recovered sample ready");
        assert!((s5.0 - 2000.0).abs() < 1e-6);
        assert!((s5.1 - 4000.0).abs() < 1e-6);
        assert_eq!(s5.2, "Eth0");
    }

    #[test]
    fn new_nic_online_causes_no_spike_then_tracks_delta() {
        let mut prev = HashMap::from([(1, (1000, 500))]);
        let mut active = String::new();
        // NIC 2 wakes up with 10 GB pre-existing cumulative counters
        let curr1 = make_table(&[
            (1, "Eth0", 2000, 1000),
            (2, "WiFi", 10_000_000_000, 5_000_000_000),
        ]);
        let (up1, down1) = step_nic_sample(&mut prev, &mut active, curr1, 1.0).unwrap();
        assert!((up1 - 500.0).abs() < 1e-6);
        assert!((down1 - 1000.0).abs() < 1e-6);
        assert_eq!(active, "Eth0");

        // Next frame: NIC 2's new delta IS tracked normally and becomes active
        let curr2 = make_table(&[
            (1, "Eth0", 2000, 1000),
            (2, "WiFi", 10_000_004_000, 5_000_002_000),
        ]);
        let (up2, down2) = step_nic_sample(&mut prev, &mut active, curr2, 1.0).unwrap();
        assert!((up2 - 2000.0).abs() < 1e-6);
        assert!((down2 - 4000.0).abs() < 1e-6);
        assert_eq!(active, "WiFi");
    }

    #[test]
    fn nic_offline_does_not_corrupt_remaining_nics() {
        let mut prev = HashMap::from([(1, (1000, 500)), (2, (9000, 9000))]);
        let mut active = String::new();
        // NIC 2 goes offline
        let curr = make_table(&[(1, "Eth0", 2500, 1500)]);
        let (up, down) = step_nic_sample(&mut prev, &mut active, curr, 1.0).unwrap();
        assert!((up - 1000.0).abs() < 1e-6);
        assert!((down - 1500.0).abs() < 1e-6);
        assert!(!prev.contains_key(&2));
    }

    #[test]
    fn counter_reset_or_wrap_saturates_to_zero() {
        let mut prev = HashMap::from([(1, (5000, 4000))]);
        let mut active = String::from("Eth0");
        let curr = make_table(&[(1, "Eth0", 100, 0)]);
        let (up, down) = step_nic_sample(&mut prev, &mut active, curr, 1.0).unwrap();
        assert_eq!(up, 0.0);
        assert_eq!(down, 0.0);
        assert_eq!(prev.get(&1), Some(&(100, 0)));
        assert_eq!(active, "Eth0");
    }

    #[test]
    fn short_dt_returns_none_and_updates_baseline() {
        let mut prev = HashMap::from([(1, (1000, 1000))]);
        let mut active = String::from("Eth0");
        // Short dt (< 0.05s) where counter jumped to 2000
        let curr1 = make_table(&[(1, "Eth0", 2000, 2000)]);
        assert_eq!(step_nic_sample(&mut prev, &mut active, curr1, 0.01), None);
        // Baseline must have updated to 2000, so next 1.0s frame at 2500 reports 500 B/s
        let curr2 = make_table(&[(1, "Eth0", 2500, 2500)]);
        let (up2, down2) = step_nic_sample(&mut prev, &mut active, curr2, 1.0).unwrap();
        assert!((up2 - 500.0).abs() < 1e-6);
        assert!((down2 - 500.0).abs() < 1e-6);
    }
}
