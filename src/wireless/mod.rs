pub mod bluetooth;
pub mod hid;
pub mod protocols;
pub mod xinput;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::state::{DeviceKind, Transport, WirelessDevice};
use bluetooth::BluetoothMonitor;
use hid::HidMonitor;
use xinput::XInputMonitor;

pub struct WirelessManager {
    xinput: XInputMonitor,
    bluetooth: BluetoothMonitor,
    hid: HidMonitor,
    stop_flag: Arc<AtomicBool>,
}

impl Default for WirelessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WirelessManager {
    pub fn new() -> Self {
        Self::with_stop_flag(Arc::new(AtomicBool::new(false)))
    }

    pub fn with_stop_flag(stop_flag: Arc<AtomicBool>) -> Self {
        Self {
            xinput: XInputMonitor::new(),
            bluetooth: BluetoothMonitor::with_stop_flag(Arc::clone(&stop_flag)),
            hid: HidMonitor::with_stop_flag(Arc::clone(&stop_flag)),
            stop_flag,
        }
    }

    pub fn poll_once(&mut self) -> Vec<WirelessDevice> {
        self.poll_once_internal(false)
    }

    pub fn poll_once_force(&mut self) -> Vec<WirelessDevice> {
        self.poll_once_internal(true)
    }

    fn poll_once_internal(&mut self, force_refresh: bool) -> Vec<WirelessDevice> {
        if self.stop_flag.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let mut devices = Vec::new();
        devices.extend(self.xinput.poll());
        if self.stop_flag.load(Ordering::Relaxed) {
            return deduplicate_wireless_devices(devices);
        }
        devices.extend(self.bluetooth.poll());
        if self.stop_flag.load(Ordering::Relaxed) {
            return deduplicate_wireless_devices(devices);
        }
        if force_refresh {
            devices.extend(self.hid.poll_force());
        } else {
            devices.extend(self.hid.poll());
        }
        deduplicate_wireless_devices(devices)
    }

    pub fn probe_diagnostics(&mut self) -> String {
        let mut report = String::new();
        report.push_str("=== AirMonitor Wireless Diagnostics ===\n\n");

        report.push_str(&self.xinput.diagnostics());
        report.push('\n');

        report.push_str(&self.bluetooth.diagnostics());
        report.push('\n');

        report.push_str(&self.hid.diagnostics());
        report.push('\n');

        let devices = self.poll_once();
        report.push_str(&format!(
            "[Active Wireless Summary] {} device(s)\n",
            devices.len()
        ));
        for d in &devices {
            report.push_str(&format!(
                "  * {} [{}] kind={:?} online={} battery={:?}% charging={} note={:?}\n",
                d.name,
                d.transport.badge(),
                d.kind,
                d.online,
                d.battery_pct,
                d.charging,
                d.detail_note
            ));
            if let Some(sub) = d.sub_batteries {
                report.push_str(&format!(
                    "    sub: L={:?}%(chg={}) R={:?}%(chg={}) Case={:?}%(chg={})\n",
                    sub.left_pct,
                    sub.left_charging,
                    sub.right_pct,
                    sub.right_charging,
                    sub.case_pct,
                    sub.case_charging
                ));
            }
        }

        report
    }
}

fn is_bt_xbox_controller(dev: &WirelessDevice) -> bool {
    dev.transport == Transport::Bluetooth
        && dev.kind == DeviceKind::Gamepad
        && dev
            .name
            .to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|tok| tok == "xbox")
}

fn is_wired_xinput(dev: &WirelessDevice) -> bool {
    dev.detail_note
        .as_deref()
        .map(|n| n.to_ascii_lowercase().contains("wired"))
        .unwrap_or(false)
}

/// Deduplicate Xbox controllers reported simultaneously by XInput and Bluetooth PnP.
/// Keeps the Bluetooth entry (preserving MAC ID & model name) and merges XInput's
/// battery level/note if Bluetooth PnP lacked a percentage. Wired XInput slots are kept separate.
pub fn deduplicate_wireless_devices(mut devices: Vec<WirelessDevice>) -> Vec<WirelessDevice> {
    let bt_xbox_indices: Vec<usize> = devices
        .iter()
        .enumerate()
        .filter_map(|(i, d)| is_bt_xbox_controller(d).then_some(i))
        .collect();

    if bt_xbox_indices.is_empty() {
        return devices;
    }

    let xinput_wireless_indices: Vec<usize> = devices
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            (d.transport == Transport::XboxWireless && !is_wired_xinput(d)).then_some(i)
        })
        .collect();

    let mut drop_indices = vec![false; devices.len()];
    for (&bt_idx, &xi_idx) in bt_xbox_indices.iter().zip(xinput_wireless_indices.iter()) {
        devices[bt_idx].online |= devices[xi_idx].online;
        if devices[bt_idx].battery_pct.is_none() && devices[xi_idx].battery_pct.is_some() {
            devices[bt_idx].battery_pct = devices[xi_idx].battery_pct;
            devices[bt_idx].charging = devices[xi_idx].charging;
            devices[bt_idx].detail_note = devices[xi_idx].detail_note.clone();
        }
        drop_indices[xi_idx] = true;
    }

    devices
        .into_iter()
        .enumerate()
        .filter_map(|(i, d)| (!drop_indices[i]).then_some(d))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn mk_dev(
        id: &str,
        name: &str,
        kind: DeviceKind,
        transport: Transport,
        battery_pct: Option<u8>,
        note: Option<&str>,
    ) -> WirelessDevice {
        WirelessDevice {
            id: id.to_string(),
            name: name.to_string(),
            kind,
            transport,
            battery_pct,
            charging: false,
            sub_batteries: None,
            online: true,
            last_seen: Instant::now(),
            detail_note: note.map(str::to_string),
        }
    }

    #[test]
    fn test_dedup_xbox_prefers_bt_exact_percentage() {
        let input = vec![
            mk_dev(
                "xinput:slot0",
                "Xbox Controller #1",
                DeviceKind::Gamepad,
                Transport::XboxWireless,
                Some(65),
                Some("Alkaline (Level: Medium, approx.)"),
            ),
            mk_dev(
                "bt:112233445566",
                "Xbox Wireless Controller",
                DeviceKind::Gamepad,
                Transport::Bluetooth,
                Some(78),
                None,
            ),
        ];
        let out = deduplicate_wireless_devices(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "bt:112233445566");
        assert_eq!(out[0].transport, Transport::Bluetooth);
        assert_eq!(out[0].battery_pct, Some(78));
    }

    #[test]
    fn test_dedup_xbox_merges_xinput_battery_when_bt_missing() {
        let input = vec![
            mk_dev(
                "xinput:slot0",
                "Xbox Controller #1",
                DeviceKind::Gamepad,
                Transport::XboxWireless,
                Some(65),
                Some("Alkaline (Level: Medium, approx.)"),
            ),
            mk_dev(
                "bt:112233445566",
                "Xbox Wireless Controller",
                DeviceKind::Gamepad,
                Transport::Bluetooth,
                None,
                Some("Connected (PnP battery unavailable)"),
            ),
        ];
        let out = deduplicate_wireless_devices(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "bt:112233445566");
        assert_eq!(out[0].battery_pct, Some(65));
        assert!(out[0]
            .detail_note
            .as_deref()
            .unwrap_or("")
            .contains("Alkaline"));
    }

    #[test]
    fn test_dedup_keeps_wired_xinput_and_non_xbox_bt_separate() {
        let input = vec![
            mk_dev(
                "xinput:slot0",
                "Xbox Controller #1",
                DeviceKind::Gamepad,
                Transport::XboxWireless,
                None,
                Some("Wired USB (external power)"),
            ),
            mk_dev(
                "bt:112233445566",
                "Xbox Wireless Controller",
                DeviceKind::Gamepad,
                Transport::Bluetooth,
                Some(90),
                None,
            ),
            mk_dev(
                "bt:AABBCCDDEEFF",
                "DualSense Wireless Controller",
                DeviceKind::Gamepad,
                Transport::Bluetooth,
                Some(80),
                None,
            ),
        ];
        let out = deduplicate_wireless_devices(input);
        assert_eq!(out.len(), 3);
    }
}
