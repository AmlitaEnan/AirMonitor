use std::time::{Duration, Instant};

use super::{format_hex, BatteryReading, HidIo};

pub fn probe(io: &dyn HidIo) -> Option<BatteryReading> {
    probe_with_listen(io, 3600)
}

pub fn probe_fast(io: &dyn HidIo) -> Option<BatteryReading> {
    probe_with_listen(io, 80)
}

pub fn probe_diagnostics_listen(io: &dyn HidIo) -> Option<BatteryReading> {
    probe_with_listen(io, 3200)
}

fn probe_with_listen(io: &dyn HidIo, listen_budget_ms: u64) -> Option<BatteryReading> {
    let info = io.info();
    // Only touch vendor-defined collections (e.g., 0xFF00:0x0002), never standard mouse/kbd.
    if info.usage_page < 0xFF00 {
        return None;
    }

    // Only the telemetry status endpoint (0x0002, 7/13/16/19-byte) emits battery reports on Rapoo.
    // The other vendor collections (0x0001 bulk, 0x000E control, 0x000F/0x0010 feature, etc.) are config/OTA.
    if info.usage != 0x0002 {
        return None;
    }

    let is_gen2_54l = info.serial.to_ascii_uppercase().contains("54L") || info.input_len >= 13;
    let variant = if is_gen2_54l {
        "Rapoo Gen2 (54L)"
    } else {
        "Rapoo Legacy"
    };

    let mut raw_packets = Vec::new();
    let mut best_reading: Option<BatteryReading> = None;

    // 1. Drain any queued passive 0x07 / 0xBB telemetry reports from the HID input buffer first
    if io.can_rw() && info.input_len > 0 {
        for _ in 0..32 {
            match io.read_input(35) {
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
                Err(_) => break,
            }
        }
        if let Some(mut r) = best_reading {
            r.raw_packets = raw_packets;
            return Some(r);
        }

        // 2. If buffer was empty, listen passively in short 250ms overlapped chunks up to budget
        // (breaks immediately once a valid 0x07/0xBB battery packet arrives).
        let deadline = Instant::now() + Duration::from_millis(listen_budget_ms);
        while Instant::now() < deadline {
            let rem = deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u32;
            let step = rem.clamp(35, 250);
            if let Ok(buf) = io.read_input(step) {
                if buf.iter().any(|&b| b != 0) {
                    raw_packets.push(format!("PASSIVE: {}", format_hex(&buf)));
                    if let Some((pct, chg)) = parse_rapoo_packet(&buf) {
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some(variant.into()),
                            raw_packets,
                        });
                    }
                }
            }
        }
    }

    // 3. Fallback: inspect GetInputReport / GetFeature for diagnostics
    for &rid in &[0x07u8, 0xBB] {
        if info.feature_len > 0 {
            if let Ok(buf) = io.get_feature(rid) {
                if buf.iter().any(|&b| b != 0) {
                    raw_packets.push(format!("FEAT[0x{:02X}]: {}", rid, format_hex(&buf)));
                    if let Some((pct, chg)) = parse_rapoo_packet(&buf) {
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some(variant.into()),
                            raw_packets,
                        });
                    }
                }
            }
        }
        if info.input_len > 0 {
            if let Ok(buf) = io.get_input_report(rid) {
                if buf.iter().any(|&b| b != 0) {
                    raw_packets.push(format!("IN[0x{:02X}]: {}", rid, format_hex(&buf)));
                    if let Some((pct, chg)) = parse_rapoo_packet(&buf) {
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some(variant.into()),
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
            detail_note: Some(format!("{} (no telemetry frame yet)", variant)),
            raw_packets,
        })
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
}
