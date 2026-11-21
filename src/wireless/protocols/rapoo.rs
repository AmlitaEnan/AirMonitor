use std::time::{Duration, Instant};

use super::{format_hex, BatteryReading, HidIo};

#[derive(Debug, Clone)]
pub struct RapooProbeResult {
    pub reading: Option<BatteryReading>,
    pub io_error: bool,
}

pub fn probe(io: &dyn HidIo) -> Option<BatteryReading> {
    probe_detailed(io, 3600).reading
}

pub fn probe_fast(io: &dyn HidIo) -> Option<BatteryReading> {
    probe_detailed(io, 80).reading
}

pub fn probe_diagnostics_listen(io: &dyn HidIo) -> Option<BatteryReading> {
    probe_detailed(io, 3200).reading
}

/// Non-blocking/short-timeout drain of already-queued Windows HID input reports.
/// Performs zero USB TX and skips passive waiting / GetInputReport fallback so sleeping
/// mice are never disturbed while newly-woken telemetry frames are picked up immediately.
pub fn probe_drain_only(io: &dyn HidIo) -> RapooProbeResult {
    probe_detailed(io, 0)
}

fn is_cancelled_err(err: &str) -> bool {
    err.contains("cancelled")
}

fn is_transient_read_err(err: &str) -> bool {
    err.contains("timeout") || is_cancelled_err(err)
}

pub fn probe_detailed(io: &dyn HidIo, listen_budget_ms: u64) -> RapooProbeResult {
    let info = io.info();
    // Only touch vendor-defined collections (e.g., 0xFF00:0x0002), never standard mouse/kbd.
    if info.usage_page < 0xFF00 {
        return RapooProbeResult {
            reading: None,
            io_error: false,
        };
    }

    // Only the telemetry status endpoint (0x0002, 7/13/16/19-byte) emits battery reports on Rapoo.
    // The other vendor collections (0x0001 bulk, 0x000E control, 0x000F/0x0010 feature, etc.) are config/OTA.
    if info.usage != 0x0002 {
        return RapooProbeResult {
            reading: None,
            io_error: false,
        };
    }

    let is_gen2_54l = info.serial.to_ascii_uppercase().contains("54L") || info.input_len >= 13;
    let variant = if is_gen2_54l {
        "Rapoo Gen2 (54L)"
    } else {
        "Rapoo Legacy"
    };

    let mut raw_packets = Vec::new();
    let mut best_reading: Option<BatteryReading> = None;
    let mut io_error = false;
    let mut cancelled = false;

    // 1. Drain any queued passive 0x07 / 0xBB telemetry reports from the HID input buffer first
    if io.can_rw() && info.input_len > 0 {
        let drain_timeout_ms = if listen_budget_ms == 0 { 25 } else { 35 };
        for _ in 0..32 {
            match io.read_input(drain_timeout_ms) {
                Ok(buf) => {
                    if buf.iter().any(|&b| b != 0) {
                        raw_packets.push(format!("QUEUED: {}", format_hex(&buf)));
                        if let Some((pct, chg)) = parse_rapoo_packet(&buf) {
                            best_reading = Some(BatteryReading {
                                battery_pct: Some(pct),
                                charging: chg,
                                online: true,
                                detail_note: Some(variant.into()),
                                raw_packets: raw_packets.clone(),
                            });
                        }
                    }
                }
                Err(err) => {
                    if is_cancelled_err(&err) {
                        cancelled = true;
                    } else if !is_transient_read_err(&err) {
                        io_error = true;
                    }
                    break;
                }
            }
        }
        if let Some(mut r) = best_reading {
            r.raw_packets = raw_packets;
            return RapooProbeResult {
                reading: Some(r),
                io_error,
            };
        }

        if io_error || cancelled || listen_budget_ms == 0 {
            return RapooProbeResult {
                reading: None,
                io_error,
            };
        }

        // 2. If buffer was empty, listen passively in short 250ms overlapped chunks up to budget
        // (breaks immediately once a valid 0x07/0xBB battery packet arrives, or on I/O error/cancel).
        let deadline = Instant::now() + Duration::from_millis(listen_budget_ms);
        while Instant::now() < deadline {
            let rem = deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u32;
            let step = rem.clamp(35, 250);
            match io.read_input(step) {
                Ok(buf) => {
                    if buf.iter().any(|&b| b != 0) {
                        raw_packets.push(format!("PASSIVE: {}", format_hex(&buf)));
                        if let Some((pct, chg)) = parse_rapoo_packet(&buf) {
                            return RapooProbeResult {
                                reading: Some(BatteryReading {
                                    battery_pct: Some(pct),
                                    charging: chg,
                                    online: true,
                                    detail_note: Some(variant.into()),
                                    raw_packets,
                                }),
                                io_error: false,
                            };
                        }
                    }
                }
                Err(err) => {
                    if is_cancelled_err(&err) {
                        cancelled = true;
                        break;
                    }
                    if !is_transient_read_err(&err) {
                        io_error = true;
                        break;
                    }
                }
            }
        }
    } else if listen_budget_ms == 0 {
        return RapooProbeResult {
            reading: None,
            io_error: !io.can_rw(),
        };
    }

    if io_error || cancelled {
        return RapooProbeResult {
            reading: None,
            io_error,
        };
    }

    // 3. Fallback: inspect GetFeature / GetInputReport for diagnostics.
    // Ignore unrelated USB control-buffer residue whose Report ID byte does not match `rid`
    // (e.g., 54L firmware returning stale descriptor bytes starting with 0x02 for rid=0x07).
    for &rid in &[0x07u8, 0xBB] {
        if info.feature_len > 0 {
            if let Ok(buf) = io.get_feature(rid) {
                if let Some((pct, chg)) = parse_rapoo_packet(&buf) {
                    raw_packets.push(format!("FEAT[0x{:02X}]: {}", rid, format_hex(&buf)));
                    return RapooProbeResult {
                        reading: Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some(variant.into()),
                            raw_packets,
                        }),
                        io_error: false,
                    };
                }
                if buf.first() == Some(&rid) && buf[1..].iter().any(|&b| b != 0) {
                    raw_packets.push(format!("FEAT[0x{:02X}]: {}", rid, format_hex(&buf)));
                }
            }
        }
        if info.input_len > 0 {
            if let Ok(buf) = io.get_input_report(rid) {
                if let Some((pct, chg)) = parse_rapoo_packet(&buf) {
                    raw_packets.push(format!("IN[0x{:02X}]: {}", rid, format_hex(&buf)));
                    return RapooProbeResult {
                        reading: Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some(variant.into()),
                            raw_packets,
                        }),
                        io_error: false,
                    };
                }
                if buf.first() == Some(&rid) && buf[1..].iter().any(|&b| b != 0) {
                    raw_packets.push(format!("IN[0x{:02X}]: {}", rid, format_hex(&buf)));
                }
            }
        }
    }

    if raw_packets.is_empty() {
        RapooProbeResult {
            reading: None,
            io_error: false,
        }
    } else {
        RapooProbeResult {
            reading: Some(BatteryReading {
                battery_pct: None,
                charging: false,
                online: true,
                detail_note: Some(format!("{} (no telemetry frame yet)", variant)),
                raw_packets,
            }),
            io_error: false,
        }
    }
}

/// Parses Rapoo `0xBB` (7-byte legacy / unsolicited) or `0x07` (13..19-byte Gen2 54L) reports.
pub fn parse_rapoo_packet(buf: &[u8]) -> Option<(u8, bool)> {
    if buf.is_empty() {
        return None;
    }

    // Legacy / unsolicited 0xBB 7-byte format:
    // [0]=0xBB, [1]=0xB0 (on VT3 Pro), [5]=charging (0x01=discharging, 0x02=charging), [6]=battery (0..=100)
    if buf[0] == 0xBB && buf.len() >= 7 {
        let chg_flag = buf[5];
        let pct = buf[6];
        if (chg_flag == 0x01 || chg_flag == 0x02) && pct <= 100 {
            return Some((pct, chg_flag == 0x02));
        }
    }

    // Gen2 54L Report ID 0x07 (9..19-byte telemetry packet):
    // Example: 07 20 03 20 03 20 03 01 19 00 04 84 00 17 46 00 02 03 BC
    // [0]=0x07, [1] low nibble=conn(0=2.4G,1=BT,2=Wired), [7]=charging(1=discharging,2=charging), [8]=pct(0..=100)
    if buf[0] == 0x07 && buf.len() >= 9 && (buf[1] & 0x0F) <= 2 {
        let chg_flag = buf[7];
        let pct = buf[8];
        if (chg_flag == 0x01 || chg_flag == 0x02) && pct <= 100 {
            return Some((pct, chg_flag == 0x02));
        }
    }

    // Gen2 54L Report ID 0x07 carrying nested 0xBB status frame
    if buf[0] == 0x07 && buf.len() >= 8 && buf[1] == 0xBB {
        let chg_flag = buf[6];
        let pct = buf[7];
        if (chg_flag == 0x01 || chg_flag == 0x02) && pct <= 100 {
            return Some((pct, chg_flag == 0x02));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_legacy_0xbb() {
        let pkt = [0xBB, 0xB0, 0x81, 0x20, 0x03, 0x01, 0x64];
        assert_eq!(parse_rapoo_packet(&pkt), Some((100, false)));

        let pkt_chg = [0xBB, 0xB0, 0x81, 0x20, 0x03, 0x02, 0x37];
        assert_eq!(parse_rapoo_packet(&pkt_chg), Some((55, true)));
    }

    #[test]
    fn test_parse_gen2_54l_0x07_telemetry() {
        let pkt = [
            0x07, 0x20, 0x03, 0x20, 0x03, 0x20, 0x03, 0x01, 0x19, 0x00, 0x04, 0x84, 0x00, 0x17,
            0x46, 0x00, 0x02, 0x03, 0xBC,
        ];
        assert_eq!(parse_rapoo_packet(&pkt), Some((25, false)));

        let pkt_chg = [
            0x07, 0x22, 0x02, 0x40, 0x06, 0x40, 0x06, 0x02, 0x52, 0x00, 0x04, 0x84, 0x00, 0x12,
            0x46, 0x00, 0x02, 0x03, 0xBC,
        ];
        assert_eq!(parse_rapoo_packet(&pkt_chg), Some((82, true)));
    }

    #[test]
    fn test_parse_gen2_0x07_nested() {
        let mut pkt = [0u8; 19];
        pkt[0] = 0x07;
        pkt[1] = 0xBB;
        pkt[5] = 0x03;
        pkt[6] = 0x02;
        pkt[7] = 82;
        assert_eq!(parse_rapoo_packet(&pkt), Some((82, true)));
    }

    struct MockRapooIo {
        info: super::super::HidInterfaceInfo,
        reads: std::cell::RefCell<std::collections::VecDeque<Result<Vec<u8>, String>>>,
        read_calls: std::cell::Cell<usize>,
        input_report: Option<Vec<u8>>,
    }

    impl MockRapooIo {
        fn new(reads: Vec<Result<Vec<u8>, String>>, input_report: Option<Vec<u8>>) -> Self {
            Self {
                info: super::super::HidInterfaceInfo {
                    path: String::new(),
                    vid: 0x24AE,
                    pid: 0x1412,
                    usage_page: 0xFF00,
                    usage: 0x0002,
                    input_len: 19,
                    output_len: 19,
                    feature_len: 0,
                    manufacturer: "Rapoo".into(),
                    product: "Rapoo Gaming Device".into(),
                    serial: "2607-54L".into(),
                    interface_tag: "MI_01".into(),
                },
                reads: std::cell::RefCell::new(reads.into()),
                read_calls: std::cell::Cell::new(0),
                input_report,
            }
        }
    }

    impl HidIo for MockRapooIo {
        fn info(&self) -> &super::super::HidInterfaceInfo {
            &self.info
        }
        fn can_rw(&self) -> bool {
            true
        }
        fn write_output(&self, _data: &[u8]) -> Result<(), String> {
            Err("read-only".into())
        }
        fn read_input(&self, _timeout_ms: u32) -> Result<Vec<u8>, String> {
            self.read_calls.set(self.read_calls.get() + 1);
            self.reads
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err("ReadFile timeout".into()))
        }
        fn get_feature(&self, _report_id: u8) -> Result<Vec<u8>, String> {
            Err("unsupported".into())
        }
        fn get_input_report(&self, _report_id: u8) -> Result<Vec<u8>, String> {
            self.input_report
                .clone()
                .ok_or_else(|| "IOCTL_HID_GET_INPUT_REPORT error".into())
        }
        fn set_feature(&self, _data: &[u8]) -> Result<(), String> {
            Err("read-only".into())
        }
    }

    #[test]
    fn test_probe_detailed_detects_io_error_without_spinning() {
        let io = MockRapooIo::new(
            vec![Err("ReadFile error: device not connected".into())],
            None,
        );
        let res = probe_detailed(&io, 3600);
        assert!(res.io_error);
        assert!(res.reading.is_none());
        assert_eq!(io.read_calls.get(), 1);
    }

    #[test]
    fn test_probe_drain_only_reads_queued_packet_without_waiting() {
        let pkt = vec![
            0x07, 0x10, 0x03, 0x40, 0x06, 0x40, 0x06, 0x01, 0x3F, 0x00, 0x03, 0x01, 0x01, 0x12,
            0x46, 0x01, 0x02, 0x02, 0xB3,
        ];
        let io_with_pkt = MockRapooIo::new(vec![Ok(pkt)], None);
        let res = probe_drain_only(&io_with_pkt);
        assert!(!res.io_error);
        let reading = res.reading.expect("queued 0x07 frame should be parsed");
        assert_eq!(reading.battery_pct, Some(63));
        assert!(!reading.charging);

        let io_empty = MockRapooIo::new(vec![], None);
        let empty = probe_drain_only(&io_empty);
        assert!(!empty.io_error);
        assert!(empty.reading.is_none());
        assert_eq!(io_empty.read_calls.get(), 1);
    }

    #[test]
    fn test_probe_ignores_mismatched_get_input_report_residue() {
        // Real 54L firmware quirk: GetInputReport(0x07) returns stale descriptor bytes starting with 0x02.
        let residue = vec![
            0x02, 0x01, 0x07, 0x01, 0x01, 0x00, 0x13, 0x00, 0x37, 0x00, 0x2D, 0x00, 0x35, 0x00,
            0x34, 0x00, 0x4C, 0x00, 0x6D,
        ];
        let io = MockRapooIo::new(vec![], Some(residue));
        let res = probe_detailed(&io, 40);
        assert!(!res.io_error);
        assert!(res.reading.is_none());
    }
}
