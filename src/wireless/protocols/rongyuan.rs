use std::thread;
use std::time::Duration;

use super::{format_hex, BatteryReading, HidIo};

pub fn probe(io: &dyn HidIo) -> Option<BatteryReading> {
    let info = io.info();
    if info.usage_page < 0xFF00 {
        return None;
    }

    let mut raw_packets = Vec::new();
    let is_rongyuan_feature =
        info.usage_page == 0xFFFF && info.usage == 0x0002 && info.feature_len >= 7;

    // 1. Verified RongYuan 2.4G receiver protocol (FFFF:0002, 65B Feature 0x00 0xF7)
    //    per qu1ncky/BatteryTray, krisgfx9/rongyuan.cpp, dniminenn/sharkfin
    if is_rongyuan_feature && info.feature_len > 0 {
        let mut set_ok = false;
        if io.can_rw() {
            let mut feat = vec![0u8; info.feature_len as usize];
            feat[0] = 0x00;
            feat[1] = 0xF7;
            if io.set_feature(&feat).is_ok() {
                set_ok = true;
                for _ in 0..3 {
                    thread::sleep(Duration::from_millis(50));
                    if let Ok(resp) = io.get_feature(0x00) {
                        if resp.iter().any(|&b| b != 0) {
                            raw_packets.push(format!("GET_FEAT[0xF7]: {}", format_hex(&resp)));
                            if let Some(reading) = parse_rongyuan_f7_status(&resp, &raw_packets) {
                                return Some(reading);
                            }
                        }
                    }
                }
            }
        }
        if !set_ok {
            if let Ok(resp) = io.get_feature(0x00) {
                if resp.iter().any(|&b| b != 0) {
                    raw_packets.push(format!("FEAT[0x00]: {}", format_hex(&resp)));
                    if let Some(reading) = parse_rongyuan_f7_status(&resp, &raw_packets) {
                        return Some(reading);
                    }
                }
            }
        }
    }

    // 2. Read-only GetFeature inspection on non-FFFF:0002 collections (no blind write_output)
    if !is_rongyuan_feature && info.feature_len > 0 {
        for &rid in &[0x00u8, 0x06, 0x07] {
            if let Ok(buf) = io.get_feature(rid) {
                if buf.iter().any(|&b| b != 0) {
                    raw_packets.push(format!("FEAT[0x{:02X}]: {}", rid, format_hex(&buf)));
                    if let Some((pct, chg)) = parse_rongyuan_packet(&buf) {
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some("RongYuan Feature".into()),
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
            detail_note: Some("RongYuan vendor interface (raw captured)".into()),
            raw_packets,
        })
    }
}

/// Verified `00 F7` response layout:
/// `[0]=0x00, [1]=reply_ready(!=0), [2]=kb_pct, [3]=mouse_pct, [4]=kb_offline(0=online), [5]=mouse_offline(0=online)`
pub fn parse_rongyuan_f7_status(buf: &[u8], raw_packets: &[String]) -> Option<BatteryReading> {
    if buf.len() < 6 || buf[0] != 0x00 || buf[1] == 0 {
        return None;
    }

    let kb_pct = buf[2];
    let mouse_pct = buf[3];
    let kb_online = buf[4] == 0;
    let mouse_online = buf[5] == 0;

    if kb_online && (1..=100).contains(&kb_pct) {
        return Some(BatteryReading {
            battery_pct: Some(kb_pct),
            charging: false,
            online: true,
            detail_note: Some("RongYuan 0xF7 (Keyboard)".into()),
            raw_packets: raw_packets.to_vec(),
        });
    }

    if mouse_online && (1..=100).contains(&mouse_pct) {
        return Some(BatteryReading {
            battery_pct: Some(mouse_pct),
            charging: false,
            online: true,
            detail_note: Some("RongYuan 0xF7 (Mouse)".into()),
            raw_packets: raw_packets.to_vec(),
        });
    }

    if !kb_online && !mouse_online {
        return Some(BatteryReading {
            battery_pct: None,
            charging: false,
            online: false,
            detail_note: Some("RongYuan 2.4G (device offline/sleeping)".into()),
            raw_packets: raw_packets.to_vec(),
        });
    }

    None
}

pub fn parse_rongyuan_packet(buf: &[u8]) -> Option<(u8, bool)> {
    if buf.len() < 6 {
        return None;
    }

    if buf[0] == 0x00 && buf[1] != 0 {
        if buf[4] == 0 && (1..=100).contains(&buf[2]) {
            return Some((buf[2], false));
        }
        if buf[5] == 0 && (1..=100).contains(&buf[3]) {
            return Some((buf[3], false));
        }
    }

    // Report ID 0x06 / 0x07 response: [0]=0x06|0x07, [1]=0x02, [2]=pct, [3]=chg
    if (buf[0] == 0x06 || buf[0] == 0x07) && buf[1] == 0x02 && buf[2] <= 100 && buf[3] <= 2 {
        return Some((buf[2], buf[3] != 0));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rongyuan_f7_keyboard_and_mouse() {
        let kb = [0x00, 0x01, 88, 0, 0x00, 0x01, 0x01, 0x00];
        let res = parse_rongyuan_f7_status(&kb, &[]).unwrap();
        assert_eq!(res.battery_pct, Some(88));
        assert!(res.online);

        let mouse = [0x00, 0x01, 0, 74, 0x01, 0x00, 0x01, 0x01];
        let res_m = parse_rongyuan_f7_status(&mouse, &[]).unwrap();
        assert_eq!(res_m.battery_pct, Some(74));
        assert!(res_m.online);

        let offline = [0x00, 0x01, 0, 0, 0x01, 0x01, 0x00, 0x00];
        let res_off = parse_rongyuan_f7_status(&offline, &[]).unwrap();
        assert_eq!(res_off.battery_pct, None);
        assert!(!res_off.online);
    }

    #[test]
    fn test_parse_rongyuan() {
        let pkt = [0x06, 0x02, 88, 0x01, 0x00, 0x00];
        assert_eq!(parse_rongyuan_packet(&pkt), Some((88, true)));
    }

    #[test]
    fn test_non_ffff_collection_never_writes_output() {
        use crate::wireless::protocols::HidInterfaceInfo;
        use std::cell::RefCell;

        struct MockRyIo {
            info: HidInterfaceInfo,
            writes: RefCell<Vec<Vec<u8>>>,
        }
        impl HidIo for MockRyIo {
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
            fn set_feature(&self, _data: &[u8]) -> Result<(), String> {
                Ok(())
            }
        }

        let io = MockRyIo {
            info: HidInterfaceInfo {
                path: String::new(),
                vid: 0x3151,
                pid: 0x4015,
                usage_page: 0xFF00,
                usage: 0x0001,
                input_len: 64,
                output_len: 64,
                feature_len: 65,
                manufacturer: String::new(),
                product: String::new(),
                serial: String::new(),
                interface_tag: "MI_00".into(),
            },
            writes: RefCell::new(Vec::new()),
        };
        let _ = probe(&io);
        assert!(io.writes.borrow().is_empty());
    }
}
