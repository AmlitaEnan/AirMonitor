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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WirelessPollMode {
    Normal,
    DeviceChange,
    Force,
}

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

impl Drop for WirelessManager {
    fn drop(&mut self) {
        // Sub-monitors implement Drop to stop the BLE watcher, close persistent HID
        // handles / shut down I/O reclaim, and FreeLibrary(xinput*.dll) without mutating
        // the caller's shared stop_flag.
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
        self.poll_once_internal(WirelessPollMode::Normal)
    }

    pub fn poll_once_device_change(&mut self) -> Vec<WirelessDevice> {
        self.poll_once_internal(WirelessPollMode::DeviceChange)
    }

    pub fn poll_once_force(&mut self) -> Vec<WirelessDevice> {
        self.poll_once_internal(WirelessPollMode::Force)
    }

    fn poll_once_internal(&mut self, mode: WirelessPollMode) -> Vec<WirelessDevice> {
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
        match mode {
            WirelessPollMode::Normal => devices.extend(self.hid.poll()),
            WirelessPollMode::DeviceChange => devices.extend(self.hid.poll_device_change()),
            WirelessPollMode::Force => devices.extend(self.hid.poll_force()),
        }
        deduplicate_wireless_devices(devices)
    }

    pub fn probe_diagnostics(&mut self) -> String {
        self.probe_diagnostics_with_options(false)
    }

    pub fn probe_diagnostics_with_options(&mut self, verbose: bool) -> String {
        let mut report = String::new();
        report.push_str("=== AirMonitor Wireless Diagnostics ===\n\n");

        report.push_str(&self.xinput.diagnostics());
        report.push('\n');

        report.push_str(&self.bluetooth.diagnostics_with_options(verbose));
        report.push('\n');

        report.push_str(&self.hid.diagnostics_with_options(verbose));
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

fn verified_identity_token(dev: &WirelessDevice) -> Option<String> {
    let raw = dev.id.trim();
    let suffix = raw
        .strip_prefix("bt:")
        .or_else(|| raw.strip_prefix("xinput:mac:"))?;
    let hex: String = suffix
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if hex.len() == 12 && hex != "000000000000" {
        Some(hex)
    } else {
        None
    }
}

fn merge_bt_and_xinput(devices: &mut [WirelessDevice], bt_idx: usize, xi_idx: usize) {
    devices[bt_idx].online |= devices[xi_idx].online;
    if devices[bt_idx].battery_pct.is_none() && devices[xi_idx].battery_pct.is_some() {
        devices[bt_idx].battery_pct = devices[xi_idx].battery_pct;
        devices[bt_idx].charging = devices[xi_idx].charging;
        devices[bt_idx].detail_note = devices[xi_idx].detail_note.clone();
    }
}

/// Deduplicate Xbox controllers reported simultaneously by XInput and Bluetooth PnP.
/// Only merges when there is a 1-to-1 correspondence (single BT Xbox + single wireless XInput)
/// or an explicitly verified matching identity token. Multi-controller setups are preserved
/// unmerged rather than mis-pairing distinct controllers by array index.
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

    if xinput_wireless_indices.is_empty() {
        return devices;
    }

    let mut drop_indices = vec![false; devices.len()];
    if bt_xbox_indices.len() == 1 && xinput_wireless_indices.len() == 1 {
        let bt_idx = bt_xbox_indices[0];
        let xi_idx = xinput_wireless_indices[0];
        merge_bt_and_xinput(&mut devices, bt_idx, xi_idx);
        drop_indices[xi_idx] = true;
    } else {
        let mut matched_bt = vec![false; bt_xbox_indices.len()];
        for &xi_idx in &xinput_wireless_indices {
            let Some(xi_tok) = verified_identity_token(&devices[xi_idx]) else {
                continue;
            };
            if let Some((pos, &bt_idx)) = bt_xbox_indices.iter().enumerate().find(|(pos, &bi)| {
                !matched_bt[*pos] && verified_identity_token(&devices[bi]).as_ref() == Some(&xi_tok)
            }) {
                matched_bt[pos] = true;
                merge_bt_and_xinput(&mut devices, bt_idx, xi_idx);
                drop_indices[xi_idx] = true;
            }
        }
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

    #[test]
    fn test_dedup_does_not_blindly_zip_multiple_controllers() {
        // 2 BT Xbox + 2 XInput wireless without verified MAC identity -> must keep all 4
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
                "xinput:slot1",
                "Xbox Controller #2",
                DeviceKind::Gamepad,
                Transport::XboxWireless,
                Some(25),
                Some("Alkaline (Level: Low, approx.)"),
            ),
            mk_dev(
                "bt:112233445566",
                "Xbox Wireless Controller",
                DeviceKind::Gamepad,
                Transport::Bluetooth,
                None,
                None,
            ),
            mk_dev(
                "bt:AABBCCDDEEFF",
                "Xbox Wireless Controller",
                DeviceKind::Gamepad,
                Transport::Bluetooth,
                Some(90),
                None,
            ),
        ];
        let out = deduplicate_wireless_devices(input);
        assert_eq!(out.len(), 4);

        // 2 BT Xbox + 1 XInput wireless (mixed setup) -> must not guess which BT matches XInput
        let mixed = vec![
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
                None,
            ),
            mk_dev(
                "bt:AABBCCDDEEFF",
                "Xbox Wireless Controller",
                DeviceKind::Gamepad,
                Transport::Bluetooth,
                Some(90),
                None,
            ),
        ];
        let out_mixed = deduplicate_wireless_devices(mixed);
        assert_eq!(out_mixed.len(), 3);

        // Multi-controller WITH verifiable identity token merges only the verified pair
        let verified = vec![
            mk_dev(
                "xinput:mac:112233445566",
                "Xbox Controller #1",
                DeviceKind::Gamepad,
                Transport::XboxWireless,
                Some(65),
                Some("Alkaline (Level: Medium, approx.)"),
            ),
            mk_dev(
                "xinput:slot1",
                "Xbox Controller #2",
                DeviceKind::Gamepad,
                Transport::XboxWireless,
                Some(100),
                Some("NiMH (Level: Full, approx.)"),
            ),
            mk_dev(
                "bt:112233445566",
                "Xbox Wireless Controller",
                DeviceKind::Gamepad,
                Transport::Bluetooth,
                None,
                None,
            ),
            mk_dev(
                "bt:AABBCCDDEEFF",
                "Xbox Wireless Controller",
                DeviceKind::Gamepad,
                Transport::Bluetooth,
                Some(88),
                None,
            ),
        ];
        let out_verified = deduplicate_wireless_devices(verified);
        assert_eq!(out_verified.len(), 3);
        let merged_bt = out_verified
            .iter()
            .find(|d| d.id == "bt:112233445566")
            .unwrap();
        assert_eq!(merged_bt.battery_pct, Some(65));
    }

    #[test]
    fn test_wireless_manager_drop_does_not_mutate_stop_flag_and_supports_device_change() {
        let stop = Arc::new(AtomicBool::new(true));
        {
            let mut mgr = WirelessManager::with_stop_flag(Arc::clone(&stop));
            assert!(mgr.poll_once().is_empty());
            assert!(mgr.poll_once_device_change().is_empty());
            assert!(mgr.poll_once_force().is_empty());
        }
        assert!(stop.load(Ordering::Relaxed));

        let stop_live = Arc::new(AtomicBool::new(false));
        {
            let _mgr = WirelessManager::with_stop_flag(Arc::clone(&stop_live));
        }
        assert!(!stop_live.load(Ordering::Relaxed));
    }
}
