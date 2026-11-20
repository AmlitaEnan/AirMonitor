use std::time::{Duration, Instant};

use super::{format_hex, BatteryReading, HidIo};

pub fn probe(io: &dyn HidIo) -> Option<BatteryReading> {
    let info = io.info();
    if info.usage_page < 0xFF00 {
        return None;
    }

    let mut raw_packets = Vec::new();

    // 1. CompX / Nordic 54L15 (64B Report 0x08 Cmd 0x72 Sub 0x07 on FF05:0001)
    if info.usage_page == 0xFF05 && info.usage == 0x0001 && info.output_len >= 64 && io.can_rw() {
        let mut cmd = vec![0u8; 64];
        cmd[0] = 0x08;
        cmd[1] = 0x7D;
        cmd[2] = 0x72;
        cmd[3] = 0x02;
        cmd[4] = 0x00;
        cmd[5] = 0x01; // dongle link
        cmd[6] = 0x07;
        cmd[7] = 0x01;
        for _ in 0..4 {
            if io.read_input(15).is_err() {
                break;
            }
        }
        if io.write_output(&cmd).is_ok() {
            if let Ok(buf) = io.read_input(250) {
                if buf.iter().any(|&b| b != 0) {
                    raw_packets.push(format!("TX[08 7D 72..] -> RX[{}]", format_hex(&buf)));
                    if let Some((pct, chg)) = parse_compx_packet(&buf) {
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some("CompX Nordic 54L15".into()),
                            raw_packets,
                        });
                    }
                }
            }
        }
    }

    // 2. CompX / Nordic 52840 Feature fallback (FF02:0002 17B)
    if info.feature_len > 0 {
        for &rid in &[0x08u8, 0x04] {
            if let Ok(buf) = io.get_feature(rid) {
                if buf.iter().any(|&b| b != 0) {
                    raw_packets.push(format!("FEAT[0x{:02X}]: {}", rid, format_hex(&buf)));
                    if let Some((pct, chg)) = parse_compx_packet(&buf) {
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some("Compx Feature".into()),
                            raw_packets,
                        });
                    }
                }
            }
        }
    }

    // 3. CompX / Nordic 52840 Output/Input query (FF02:0002 17B Report 0x08 Cmd 0x04, checksum 0x55)
    //    Matches zenith.cpp (260D:1114) & kysona.cpp: flush input, send 08 04 ... 49, read up to 600ms.
    if io.can_rw() && info.output_len >= 17 && info.input_len >= 17 {
        let mut cmd = vec![0u8; 17];
        cmd[0] = 0x08;
        cmd[1] = 0x04;
        let sum: u8 = cmd[..16].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        cmd[16] = 0x55u8.wrapping_sub(sum); // 0x49

        for _ in 0..4 {
            if io.read_input(15).is_err() {
                break;
            }
        }

        if io.write_output(&cmd).is_ok() {
            let deadline = Instant::now() + Duration::from_millis(600);
            for _ in 0..16 {
                if Instant::now() >= deadline {
                    break;
                }
                let rem = deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u32;
                let Ok(buf) = io.read_input(rem.clamp(25, 250)) else {
                    break;
                };
                if buf.iter().any(|&b| b != 0) {
                    raw_packets.push(format!(
                        "TX[{}] -> RX[{}]",
                        format_hex(&cmd),
                        format_hex(&buf)
                    ));
                    if let Some((pct, chg)) = parse_compx_packet(&buf) {
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some("CompX 0x08/0x04".into()),
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
            detail_note: Some("Compx vendor interface (raw captured)".into()),
            raw_packets,
        })
    }
}

pub fn parse_compx_packet(buf: &[u8]) -> Option<(u8, bool)> {
    // CompX Nordic 52840: [0]=0x08, [1]=0x04, [6]=pct(1..=100), [7]=charging(0/1)
    if buf.len() >= 8 && buf[0] == 0x08 && buf[1] == 0x04 {
        let pct = buf[6];
        let chg = buf[7];
        if buf.len() == 17 {
            let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
            if sum != 0x55 {
                return None;
            }
        }
        if (1..=100).contains(&pct) && chg <= 2 {
            return Some((pct, chg != 0));
        }
    }

    // CompX Nordic 54L15: [0]=0x08, [1]=0x72, [2]=0x00, [5]=0x07, [7]=pct
    if buf.len() >= 8 && buf[0] == 0x08 && buf[1] == 0x72 && buf[2] == 0x00 && buf[5] == 0x07 {
        let pct = buf[7];
        if (1..=100).contains(&pct) {
            return Some((pct, false));
        }
    }

    if buf.len() >= 6 && buf[0] == 0x04 && (1..=100).contains(&buf[4]) && buf[5] <= 2 {
        return Some((buf[4], buf[5] != 0));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_compx() {
        // Real 17-byte Nordic52 packet from nordic52.py: 08 04 00 00 00 02 23 00 0E 94 00 00 00 00 00 00 82 (sum=0x55, 35%)
        let pkt = [
            0x08, 0x04, 0x00, 0x00, 0x00, 0x02, 0x23, 0x00, 0x0E, 0x94, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x82,
        ];
        assert_eq!(parse_compx_packet(&pkt), Some((35, false)));

        let asleep = [0u8; 17];
        assert_eq!(parse_compx_packet(&asleep), None);
    }

    #[test]
    fn test_parse_compx_54l15() {
        let pkt = [0x08, 0x72, 0x00, 0x02, 0x01, 0x07, 0x01, 83];
        assert_eq!(parse_compx_packet(&pkt), Some((83, false)));
    }
}
