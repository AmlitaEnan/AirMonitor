use std::thread;
use std::time::Duration;

use super::{format_hex, BatteryReading, HidIo};

pub fn probe(io: &dyn HidIo) -> Option<BatteryReading> {
    let info = io.info();

    // Dareu A84/A87 SONiX-chip 2.4G keyboards (exact 05AC:024F "Smart Wireless Device" or 260D:0022):
    // MI_00 keyboard collection (0001:0006, 65B feature report 00 04 F6 ...)
    if info.usage_page == 0x0001
        && info.usage == 0x0006
        && info.feature_len == 65
        && ((info.vid == 0x05AC && info.pid == 0x024F)
            || (info.vid == 0x260D && info.pid == 0x0022))
    {
        return probe_dareu_kbd_feature(io);
    }

    if info.usage_page < 0xFF00 {
        return None;
    }

    let mut raw_packets = Vec::new();
    let is_qmk_raw_hid = info.usage_page == 0xFF60
        && info.usage == 0x0061
        && info.input_len == 33
        && info.output_len == 33;

    if is_qmk_raw_hid && io.can_rw() {
        // Verified SONiX 2.4G dongle battery query (per krisgfx9 sonix.cpp / FerrisPulse):
        // [0x00, 0x20, 0x01, ..., checksum]
        // Note: vendor-fallback unknown PIDs are wrapped in ReadOnlyHidIo by probe_protocol,
        // so write_output returns Err and zero TX frames are transmitted on unknown PIDs.
        let mut sonix_cmd = [0u8; 33];
        sonix_cmd[0] = 0x00;
        sonix_cmd[1] = 0x20;
        sonix_cmd[2] = 0x01;
        sonix_cmd[32] = sonix_cmd[1..32]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_add(b));

        for _attempt in 0..2 {
            if let Some(rx) = send_and_read(io, &sonix_cmd, 250, &mut raw_packets) {
                if let Some((pct, chg)) = parse_sonix_packet(&rx) {
                    return Some(BatteryReading {
                        battery_pct: Some(pct),
                        charging: chg,
                        online: true,
                        detail_note: Some("SONiX 20:01 (FF60:0061)".into()),
                        raw_packets,
                    });
                }
                if rx.len() >= 2 && rx[1] == 0xFF {
                    break; // Firmware returned VIA unhandled (0xFF)
                }
            }
        }
    }

    // Verified AULA / MCHOSE "BY" 20B protocol (FF02:0002 20/20 on known VID:PIDs only)
    let is_by_20b_device = (info.vid == 0x05AC && info.pid == 0x024F)
        || (info.vid == 0x3554 && info.pid == 0xFA09)
        || (info.vid == 0x41E4 && info.pid == 0x2001);
    if is_by_20b_device
        && info.usage_page == 0xFF02
        && info.usage == 0x0002
        && info.input_len == 20
        && info.output_len == 20
        && io.can_rw()
    {
        let mut cmd = [0u8; 20];
        cmd[0] = 0x13;
        cmd[1] = 0x4A;
        cmd[19] = 0x5D;
        if let Some(rx) = send_and_read(io, &cmd, 300, &mut raw_packets) {
            if rx.len() >= 7
                && rx[0] == 0x13
                && (rx[1] & 0x7F) == 0x4A
                && (1..=100).contains(&rx[5])
            {
                return Some(BatteryReading {
                    battery_pct: Some(rx[5]),
                    charging: (rx[6] >> 4) == 1,
                    online: true,
                    detail_note: Some("BY 0x13/0x4A (20B)".into()),
                    raw_packets,
                });
            }
        }
    }

    // Read-only inspection on non-QMK SONiX/vendor interfaces (GetFeature / passive input only; no blind TX)
    if !is_qmk_raw_hid {
        for &rid in &[0x00u8, 0x04, 0x07, 0x08, 0x13] {
            if info.feature_len > 0 {
                if let Ok(buf) = io.get_feature(rid) {
                    if buf.iter().any(|&b| b != 0) {
                        raw_packets.push(format!("FEAT[0x{:02X}]: {}", rid, format_hex(&buf)));
                        if let Some((pct, chg)) = parse_sonix_packet(&buf) {
                            return Some(BatteryReading {
                                battery_pct: Some(pct),
                                charging: chg,
                                online: true,
                                detail_note: Some("SONiX Feature report".into()),
                                raw_packets,
                            });
                        }
                    }
                }
            }
        }

        if io.can_rw() && info.input_len > 0 {
            for rid in 0u8..=4 {
                if let Ok(buf) = io.get_input_report(rid) {
                    if buf.iter().any(|&b| b != 0) {
                        raw_packets.push(format!("IN[0x{:02X}]: {}", rid, format_hex(&buf)));
                        if let Some((pct, chg)) = parse_sonix_packet(&buf) {
                            return Some(BatteryReading {
                                battery_pct: Some(pct),
                                charging: chg,
                                online: true,
                                detail_note: Some("SONiX Input report".into()),
                                raw_packets,
                            });
                        }
                    }
                }
            }
            if let Ok(buf) = io.read_input(80) {
                if buf.iter().any(|&b| b != 0) {
                    raw_packets.push(format!("PASSIVE: {}", format_hex(&buf)));
                    if let Some((pct, chg)) = parse_sonix_packet(&buf) {
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some("SONiX Passive report".into()),
                            raw_packets,
                        });
                    }
                }
            }
        }
    }

    if raw_packets.is_empty() {
        None
    } else {
        Some(BatteryReading {
            battery_pct: None,
            charging: false,
            online: true,
            detail_note: Some("SONiX/QMK Raw HID (raw captured)".into()),
            raw_packets,
        })
    }
}

/// Read-only Dareu A84 / A87 65-byte Feature [00 04 F6 ...] query on 0001:0006.
fn probe_dareu_kbd_feature(io: &dyn HidIo) -> Option<BatteryReading> {
    let mut raw_packets = Vec::new();
    let mut req = [0u8; 65];
    req[0] = 0x00;
    req[1] = 0x04;
    req[2] = 0xF6;
    if io.set_feature(&req).is_err() {
        return None;
    }
    thread::sleep(Duration::from_millis(15));
    for _ in 0..3 {
        let _ = io.get_feature(0x00);
    }
    for _ in 0..3 {
        if let Ok(buf) = io.get_feature(0x00) {
            if buf.iter().any(|&b| b != 0) {
                raw_packets.push(format!("FEAT[04 F6]: {}", format_hex(&buf)));
                if let Some(pct) = parse_dareu_kbd_f6(&buf) {
                    return Some(BatteryReading {
                        battery_pct: Some(pct),
                        charging: false,
                        online: true,
                        detail_note: Some("Dareu/SONiX 04:F6 Feature".into()),
                        raw_packets,
                    });
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }

    if raw_packets.is_empty() {
        None
    } else {
        Some(BatteryReading {
            battery_pct: None,
            charging: false,
            online: true,
            detail_note: Some("Dareu/SONiX 0001:0006 (raw captured)".into()),
            raw_packets,
        })
    }
}

pub fn parse_dareu_kbd_f6(buf: &[u8]) -> Option<u8> {
    // Validated Dareu A84/SONiX reply: buf[0..2] == [0x00, 0x04], buf[15] != 0, buf[5] in 1..=100.
    // Checking buf[1] == 0x04 prevents reading stale/unrelated feature buffers (e.g. [00 00 00 01 00 02 ...]).
    if buf.len() >= 16
        && buf[0] == 0x00
        && buf[1] == 0x04
        && buf[15] != 0
        && (1..=100).contains(&buf[5])
    {
        Some(buf[5])
    } else {
        None
    }
}

fn send_and_read(
    io: &dyn HidIo,
    cmd: &[u8],
    timeout_ms: u32,
    raw_packets: &mut Vec<String>,
) -> Option<Vec<u8>> {
    for _ in 0..4 {
        if io.read_input(20).is_err() {
            break;
        }
    }
    if io.write_output(cmd).is_err() {
        return None;
    }
    let rx = io.read_input(timeout_ms).ok()?;
    if rx.iter().any(|&b| b != 0) {
        let preview = if cmd.len() > 6 { &cmd[..6] } else { cmd };
        raw_packets.push(format!(
            "TX[{}..] -> RX[{}]",
            format_hex(preview),
            format_hex(&rx)
        ));
        Some(rx)
    } else {
        None
    }
}

pub fn parse_sonix_packet(buf: &[u8]) -> Option<(u8, bool)> {
    if buf.len() < 5 {
        return None;
    }

    // Pattern 1a: SONiX [00 20 01 00 <pct> ...] (33-byte Windows report with Report ID 0x00)
    if buf[0] == 0x00 && buf[1] == 0x20 && buf[2] == 0x01 && buf[3] == 0x00 {
        let pct = buf[4];
        if (1..=100).contains(&pct) {
            return Some((pct, false));
        }
        return None;
    }

    // Pattern 1b: SONiX [20 01 00 <pct> ...] (32-byte payload without Report ID)
    if buf[0] == 0x20 && buf[1] == 0x01 && buf[2] == 0x00 {
        let pct = buf[3];
        if (1..=100).contains(&pct) {
            return Some((pct, false));
        }
        return None;
    }

    // Pattern 2: VIA custom get [00 08 00 07 <pct> ...]
    if buf.len() >= 5 && buf[0] == 0x00 && buf[1] == 0x08 && buf[2] == 0x00 && buf[3] == 0x07 {
        let pct = buf[4];
        if (1..=100).contains(&pct) {
            return Some((pct, false));
        }
    }

    // Pattern 3: Keychron/KC_GET_BATTERY [00 A4 <pct> <mV lo> <mV hi> <chg>]
    if buf.len() >= 6
        && buf[0] == 0x00
        && buf[1] == 0xA4
        && (1..=100).contains(&buf[2])
        && buf[5] <= 2
    {
        return Some((buf[2], buf[5] == 1));
    }

    if buf.len() < 8 {
        return None;
    }

    // Pattern 4: Compx/SONiX style [0x08, 0x04, ..., pct@6, chg@7]
    if buf[0] == 0x08 && buf[1] == 0x04 && buf[6] <= 100 && buf[7] <= 2 {
        return Some((buf[6], buf[7] != 0));
    }

    // Pattern 5: Report 0x04 / 0x07 / 0x13 status frame: [rid, status, ..., chg, pct]
    if (buf[0] == 0x04 || buf[0] == 0x07 || buf[0] == 0x13)
        && buf[1] == 0x01
        && (1..=100).contains(&buf[4])
        && buf[3] <= 2
    {
        return Some((buf[4], buf[3] != 0));
    }

    None
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::wireless::protocols::HidInterfaceInfo;

    struct MockSonixIo {
        info: HidInterfaceInfo,
        writes: RefCell<Vec<Vec<u8>>>,
        features_set: RefCell<Vec<Vec<u8>>>,
    }

    impl MockSonixIo {
        fn new(
            vid: u16,
            pid: u16,
            up: u16,
            u: u16,
            in_len: u16,
            out_len: u16,
            feat_len: u16,
        ) -> Self {
            Self {
                info: HidInterfaceInfo {
                    path: String::new(),
                    vid,
                    pid,
                    usage_page: up,
                    usage: u,
                    input_len: in_len,
                    output_len: out_len,
                    feature_len: feat_len,
                    manufacturer: String::new(),
                    product: String::new(),
                    serial: String::new(),
                    interface_tag: "MI_00".into(),
                },
                writes: RefCell::new(Vec::new()),
                features_set: RefCell::new(Vec::new()),
            }
        }
    }

    impl HidIo for MockSonixIo {
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
        fn set_feature(&self, data: &[u8]) -> Result<(), String> {
            self.features_set.borrow_mut().push(data.to_vec());
            Ok(())
        }
    }

    #[test]
    fn test_parse_sonix_20_01_with_report_id() {
        let mut pkt = [0u8; 33];
        pkt[0] = 0x00;
        pkt[1] = 0x20;
        pkt[2] = 0x01;
        pkt[3] = 0x00;
        pkt[4] = 84;
        assert_eq!(parse_sonix_packet(&pkt), Some((84, false)));

        pkt[4] = 0;
        assert_eq!(parse_sonix_packet(&pkt), None);
    }

    #[test]
    fn test_parse_sonix_20_01_stripped() {
        let mut pkt = [0u8; 32];
        pkt[0] = 0x20;
        pkt[1] = 0x01;
        pkt[2] = 0x00;
        pkt[3] = 56;
        assert_eq!(parse_sonix_packet(&pkt), Some((56, false)));
    }

    #[test]
    fn test_no_unverified_blind_writes_on_qmk_or_vendor_interfaces() {
        // FF60:0061 must only send 00 20 01 ... (never 00 B2, 00 08 00 07, 00 A4)
        let qmk_io = MockSonixIo::new(0x362D, 0xD101, 0xFF60, 0x0061, 33, 33, 0);
        let _ = probe(&qmk_io);
        let writes = qmk_io.writes.borrow();
        assert!(!writes.is_empty());
        assert!(writes
            .iter()
            .all(|w| w.len() == 33 && w[0] == 0x00 && w[1] == 0x20 && w[2] == 0x01));

        // Non-QMK vendor interface must not send [04/07/08/13 ...] blind writes
        let vendor_io = MockSonixIo::new(0x362D, 0xD101, 0xFF00, 0x0001, 64, 64, 0);
        let _ = probe(&vendor_io);
        assert!(vendor_io.writes.borrow().is_empty());

        // 0001:0006 65B Feature must only match exact 05AC:024F or 260D:0022, not other 05AC PIDs
        let other_apple = MockSonixIo::new(0x05AC, 0x0250, 0x0001, 0x0006, 8, 0, 65);
        assert!(probe(&other_apple).is_none());
        assert!(other_apple.features_set.borrow().is_empty());

        let dareu_a84 = MockSonixIo::new(0x260D, 0x0022, 0x0001, 0x0006, 8, 0, 65);
        let _ = probe(&dareu_a84);
        assert_eq!(dareu_a84.features_set.borrow().len(), 1);
    }

    #[test]
    fn test_parse_keychron_a4_battery() {
        let pkt = [0x00, 0xA4, 79, 0x60, 0x0F, 0x01, 0x04];
        assert_eq!(parse_sonix_packet(&pkt), Some((79, true)));
    }

    #[test]
    fn test_parse_dareu_kbd_f6() {
        let mut pkt = [0u8; 65];
        pkt[0] = 0x00;
        pkt[1] = 0x04;
        pkt[5] = 72;
        pkt[15] = 0x01;
        assert_eq!(parse_dareu_kbd_f6(&pkt), Some(72));
        pkt[15] = 0x00;
        assert_eq!(parse_dareu_kbd_f6(&pkt), None);

        // Stale/unrelated feature frame without [00 04] header must be rejected
        let stale = [
            0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00, 0x75, 0x97, 0xA5, 0xE5, 0x3F, 0x60, 0x3F,
            0xE4, 0xF5,
        ];
        assert_eq!(parse_dareu_kbd_f6(&stale), None);
    }

    #[test]
    fn test_parse_sonix_packet() {
        let mut pkt = [0u8; 16];
        pkt[0] = 0x08;
        pkt[1] = 0x04;
        pkt[6] = 75;
        pkt[7] = 1;
        assert_eq!(parse_sonix_packet(&pkt), Some((75, true)));
    }
}
