use super::{format_hex, BatteryReading, HidIo};

const HIDPP_SHORT: u8 = 0x10;
const HIDPP_LONG: u8 = 0x11;
const SW_ID: u8 = 0x0A;

const FEAT_BATTERY_STATUS: u16 = 0x1000;
const FEAT_UNIFIED_BATTERY: u16 = 0x1004;

pub fn probe(io: &dyn HidIo) -> Option<BatteryReading> {
    let info = io.info();
    if info.usage_page != 0xFF00 || !io.can_rw() {
        return None;
    }
    if info.output_len < 7 || info.input_len < 7 {
        return None;
    }

    let report_id = if info.output_len >= 20 && info.usage == 0x0002 {
        HIDPP_LONG
    } else if info.usage == 0x0001 || info.output_len == 7 {
        HIDPP_SHORT
    } else {
        return None;
    };

    let mut raw_packets = Vec::new();
    // Try direct device (0xFF) and receiver wireless slots (1..=6)
    for &dev_idx in &[0xFFu8, 1, 2, 3, 4, 5, 6] {
        // 1. Try UnifiedBattery (0x1004)
        if let Some(feat_idx) = get_feature_index(
            io,
            report_id,
            dev_idx,
            FEAT_UNIFIED_BATTERY,
            &mut raw_packets,
        ) {
            if let Some(resp) = hidpp_request(
                io,
                report_id,
                dev_idx,
                feat_idx,
                1,
                &[0, 0, 0],
                &mut raw_packets,
            ) {
                if let Some((pct, chg)) = parse_hidpp_battery(&resp) {
                    return Some(BatteryReading {
                        battery_pct: Some(pct),
                        charging: chg,
                        online: true,
                        detail_note: Some(format!("HID++ 2.0 UnifiedBattery (slot {})", dev_idx)),
                        raw_packets,
                    });
                }
            }
        }

        // 2. Fallback BatteryLevelStatus (0x1000)
        if let Some(feat_idx) = get_feature_index(
            io,
            report_id,
            dev_idx,
            FEAT_BATTERY_STATUS,
            &mut raw_packets,
        ) {
            if let Some(resp) = hidpp_request(
                io,
                report_id,
                dev_idx,
                feat_idx,
                0,
                &[0, 0, 0],
                &mut raw_packets,
            ) {
                if let Some((pct, chg)) = parse_hidpp_battery(&resp) {
                    return Some(BatteryReading {
                        battery_pct: Some(pct),
                        charging: chg,
                        online: true,
                        detail_note: Some(format!("HID++ 2.0 BatteryStatus (slot {})", dev_idx)),
                        raw_packets,
                    });
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
            online: false,
            detail_note: Some("HID++ interface probed".into()),
            raw_packets,
        })
    }
}

fn get_feature_index(
    io: &dyn HidIo,
    report_id: u8,
    dev_idx: u8,
    feature_id: u16,
    raw_packets: &mut Vec<String>,
) -> Option<u8> {
    let hi = (feature_id >> 8) as u8;
    let lo = (feature_id & 0xFF) as u8;
    let resp = hidpp_request(
        io,
        report_id,
        dev_idx,
        0x00,
        0,
        &[hi, lo, 0x00],
        raw_packets,
    )?;
    let feat_idx = resp.get(4).copied().unwrap_or(0);
    if feat_idx == 0 {
        None
    } else {
        Some(feat_idx)
    }
}

fn hidpp_request(
    io: &dyn HidIo,
    report_id: u8,
    dev_idx: u8,
    feat_idx: u8,
    func_idx: u8,
    params: &[u8],
    raw_packets: &mut Vec<String>,
) -> Option<Vec<u8>> {
    let len = if report_id == HIDPP_LONG { 20 } else { 7 };
    let mut pkt = vec![0u8; len];
    pkt[0] = report_id;
    pkt[1] = dev_idx;
    pkt[2] = feat_idx;
    pkt[3] = ((func_idx & 0x0F) << 4) | (SW_ID & 0x0F);
    for (i, &b) in params.iter().take(len - 4).enumerate() {
        pkt[4 + i] = b;
    }

    if io.write_output(&pkt).is_err() {
        return None;
    }

    for _ in 0..3 {
        let Ok(resp) = io.read_input(120) else {
            break;
        };
        if resp.len() < 7 {
            continue;
        }
        // Ignore unrelated reports, check matching device index
        if resp[1] != dev_idx {
            continue;
        }
        // HID++ error response (0x8F in HID++ 1.0 or 0xFF in HID++ 2.0)
        if resp[2] == 0x8F || resp[2] == 0xFF {
            raw_packets.push(format!("HID++ ERR: {}", format_hex(&resp)));
            return None;
        }
        if resp[2] == feat_idx && (resp[3] & 0x0F) == SW_ID {
            raw_packets.push(format!("HID++ RX: {}", format_hex(&resp)));
            return Some(resp);
        }
    }
    None
}

pub fn parse_hidpp_battery(resp: &[u8]) -> Option<(u8, bool)> {
    if resp.len() < 7 {
        return None;
    }
    let pct = resp[4];
    let chg_status = resp[6];
    if pct <= 100 {
        let charging = matches!(chg_status, 1..=4);
        Some((pct, charging))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hidpp_battery() {
        let resp = [0x10, 0x01, 0x06, 0x1A, 68, 50, 0x01];
        assert_eq!(parse_hidpp_battery(&resp), Some((68, true)));
    }
}
