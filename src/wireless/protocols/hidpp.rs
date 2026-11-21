use super::{format_hex, BatteryReading, HidIo};

const HIDPP_SHORT: u8 = 0x10;
const HIDPP_LONG: u8 = 0x11;
const SW_ID: u8 = 0x0A;

const FEAT_BATTERY_STATUS: u16 = 0x1000;
const FEAT_UNIFIED_BATTERY: u16 = 0x1004;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidppChargeState {
    Discharging,
    Charging,
    SlowCharging,
    AlmostFull,
    Full,
    Error(&'static str),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidppCoarseLevel {
    Critical,
    Low,
    Good,
    Full,
}

impl HidppCoarseLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Critical => "Critical",
            Self::Low => "Low",
            Self::Good => "Good",
            Self::Full => "Full",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnifiedBatteryCaps {
    pub supported_levels: u8,
    pub rechargeable: bool,
    pub supports_soc: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HidppBatteryDecoded {
    pub battery_pct: Option<u8>,
    pub charging: bool,
    pub charge_state: HidppChargeState,
    pub coarse_level: Option<HidppCoarseLevel>,
}

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
            let caps = hidpp_request(
                io,
                report_id,
                dev_idx,
                feat_idx,
                0,
                &[0, 0, 0],
                &mut raw_packets,
            )
            .and_then(|r| parse_unified_battery_capabilities(&r));
            let supports_soc = caps.map(|c| c.supports_soc);
            if let Some(resp) = hidpp_request(
                io,
                report_id,
                dev_idx,
                feat_idx,
                1,
                &[0, 0, 0],
                &mut raw_packets,
            ) {
                if let Some(decoded) = parse_unified_battery(&resp, supports_soc) {
                    return Some(BatteryReading {
                        battery_pct: decoded.battery_pct,
                        charging: decoded.charging,
                        online: true,
                        detail_note: Some(format_hidpp_detail_note(
                            "UnifiedBattery",
                            dev_idx,
                            &decoded,
                        )),
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
                if let Some(decoded) = parse_battery_status(&resp) {
                    return Some(BatteryReading {
                        battery_pct: decoded.battery_pct,
                        charging: decoded.charging,
                        online: true,
                        detail_note: Some(format_hidpp_detail_note(
                            "BatteryStatus",
                            dev_idx,
                            &decoded,
                        )),
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

pub fn parse_unified_battery_capabilities(resp: &[u8]) -> Option<UnifiedBatteryCaps> {
    if resp.len() < 6 {
        return None;
    }
    Some(UnifiedBatteryCaps {
        supported_levels: resp[4],
        rechargeable: (resp[5] & 0x01) != 0,
        supports_soc: (resp[5] & 0x02) != 0,
    })
}

fn decode_coarse_level(level_bits: u8) -> Option<HidppCoarseLevel> {
    if (level_bits & 0x08) != 0 {
        Some(HidppCoarseLevel::Full)
    } else if (level_bits & 0x04) != 0 {
        Some(HidppCoarseLevel::Good)
    } else if (level_bits & 0x02) != 0 {
        Some(HidppCoarseLevel::Low)
    } else if (level_bits & 0x01) != 0 {
        Some(HidppCoarseLevel::Critical)
    } else {
        None
    }
}

/// Parse Logitech HID++ 2.0 `UnifiedBattery` (0x1004) `get_status` (function 1) response.
/// Matches Linux kernel `drivers/hid/hid-logitech-hidpp.c`:
/// - `resp[4]`: state_of_charge (0..=100 when `supports_soc` is true)
/// - `resp[5]`: battery_level bitmask (0x01=Critical, 0x02=Low, 0x04=Good, 0x08=Full)
/// - `resp[6]`: charging_status (0=Discharging, 1=Charging, 2=Slow charging, 3=Full, 4=Error)
pub fn parse_unified_battery(
    resp: &[u8],
    supports_soc: Option<bool>,
) -> Option<HidppBatteryDecoded> {
    if resp.len() < 7 {
        return None;
    }
    let soc = resp[4];
    let coarse_level = decode_coarse_level(resp[5]);
    let (charging, charge_state) = match resp[6] {
        0 => (false, HidppChargeState::Discharging),
        1 => (true, HidppChargeState::Charging),
        2 => (true, HidppChargeState::SlowCharging),
        3 => (false, HidppChargeState::Full),
        4 => (false, HidppChargeState::Error("Charging error")),
        _ => (false, HidppChargeState::Unknown),
    };

    let battery_pct = match supports_soc {
        Some(true) => (soc <= 100).then_some(soc),
        Some(false) => None,
        None => {
            if soc == 0 && coarse_level.is_some() {
                None
            } else if soc <= 100 {
                Some(soc)
            } else {
                None
            }
        }
    };

    if battery_pct.is_none() && coarse_level.is_none() {
        return None;
    }

    Some(HidppBatteryDecoded {
        battery_pct,
        charging,
        charge_state,
        coarse_level,
    })
}

/// Parse Logitech HID++ 2.0 `BatteryStatus` (0x1000) `get_battery_level_status` (function 0) response.
/// Matches Linux kernel `drivers/hid/hid-logitech-hidpp.c`:
/// - `resp[4]`: battery_level (0..=100)
/// - `resp[6]`: status (0=Discharging, 1=Recharging, 2=Almost full, 3=Charge complete/Full,
///   4=Slow charging, 5=Invalid battery type, 6=Thermal error, 7=Other charging error)
pub fn parse_battery_status(resp: &[u8]) -> Option<HidppBatteryDecoded> {
    if resp.len() < 7 {
        return None;
    }
    let pct = resp[4];
    if pct > 100 {
        return None;
    }
    let (charging, charge_state) = match resp[6] {
        0 => (false, HidppChargeState::Discharging),
        1 => (true, HidppChargeState::Charging),
        2 => (true, HidppChargeState::AlmostFull),
        3 => (false, HidppChargeState::Full),
        4 => (true, HidppChargeState::SlowCharging),
        5 => (false, HidppChargeState::Error("Invalid battery")),
        6 => (false, HidppChargeState::Error("Thermal error")),
        7 => (false, HidppChargeState::Error("Charging error")),
        _ => (false, HidppChargeState::Unknown),
    };
    Some(HidppBatteryDecoded {
        battery_pct: Some(pct),
        charging,
        charge_state,
        coarse_level: None,
    })
}

pub fn format_hidpp_detail_note(
    feature_label: &str,
    dev_idx: u8,
    decoded: &HidppBatteryDecoded,
) -> String {
    let mut extras: Vec<String> = Vec::new();
    if let Some(level) = decoded.coarse_level {
        if decoded.battery_pct.is_none() {
            extras.push(format!("Level: {}", level.label()));
        }
    }
    match decoded.charge_state {
        HidppChargeState::Discharging => {}
        HidppChargeState::Charging => {}
        HidppChargeState::SlowCharging => extras.push("Slow charging".to_string()),
        HidppChargeState::AlmostFull => extras.push("Almost full".to_string()),
        HidppChargeState::Full => extras.push("Charge complete".to_string()),
        HidppChargeState::Error(msg) => extras.push(format!("Charging error: {}", msg)),
        HidppChargeState::Unknown => extras.push("Charge status unknown".to_string()),
    }
    if extras.is_empty() {
        format!("HID++ 2.0 {} (slot {})", feature_label, dev_idx)
    } else {
        format!(
            "HID++ 2.0 {} (slot {}, {})",
            feature_label,
            dev_idx,
            extras.join(", ")
        )
    }
}

pub fn parse_hidpp_battery(resp: &[u8]) -> Option<(u8, bool)> {
    let decoded = parse_battery_status(resp)?;
    decoded.battery_pct.map(|pct| (pct, decoded.charging))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hidpp_battery_status_states() {
        // Discharging
        let discharging = parse_battery_status(&[0x10, 0x01, 0x06, 0x0A, 68, 50, 0x00]).unwrap();
        assert_eq!(discharging.battery_pct, Some(68));
        assert!(!discharging.charging);
        assert_eq!(discharging.charge_state, HidppChargeState::Discharging);

        // Charging (1), Almost full (2), Slow charging (4)
        let charging = parse_battery_status(&[0x10, 0x01, 0x06, 0x0A, 68, 50, 0x01]).unwrap();
        assert!(charging.charging);
        assert_eq!(charging.charge_state, HidppChargeState::Charging);

        let almost_full = parse_battery_status(&[0x10, 0x01, 0x06, 0x0A, 98, 95, 0x02]).unwrap();
        assert!(almost_full.charging);
        assert_eq!(almost_full.charge_state, HidppChargeState::AlmostFull);

        let slow = parse_battery_status(&[0x10, 0x01, 0x06, 0x0A, 40, 20, 0x04]).unwrap();
        assert!(slow.charging);
        assert_eq!(slow.charge_state, HidppChargeState::SlowCharging);

        // Charge complete / Full (3) MUST have charging = false
        let full = parse_battery_status(&[0x10, 0x01, 0x06, 0x0A, 100, 95, 0x03]).unwrap();
        assert_eq!(full.battery_pct, Some(100));
        assert!(!full.charging);
        assert_eq!(full.charge_state, HidppChargeState::Full);

        // Charging errors (5, 6, 7) MUST have charging = false
        for (code, expected_msg) in [
            (0x05u8, "Invalid battery"),
            (0x06u8, "Thermal error"),
            (0x07u8, "Charging error"),
        ] {
            let err = parse_battery_status(&[0x10, 0x01, 0x06, 0x0A, 50, 30, code]).unwrap();
            assert!(!err.charging);
            assert_eq!(err.charge_state, HidppChargeState::Error(expected_msg));
            let note = format_hidpp_detail_note("BatteryStatus", 1, &err);
            assert!(note.contains("Charging error"));
        }

        // Invalid >100% rejected
        assert!(parse_battery_status(&[0x10, 0x01, 0x06, 0x0A, 0xFF, 0, 0x00]).is_none());
    }

    #[test]
    fn test_parse_hidpp_unified_battery_soc_and_coarse_only() {
        // Capabilities: SoC supported (flags bit 1 = 0x02)
        let caps_soc =
            parse_unified_battery_capabilities(&[0x11, 0x01, 0x05, 0x0A, 0x0F, 0x03]).unwrap();
        assert!(caps_soc.supports_soc);
        assert!(caps_soc.rechargeable);

        // UnifiedBattery status: 0=discharging, 1=charging, 2=slow, 3=full (charging=false), 4=error (charging=false)
        let slow =
            parse_unified_battery(&[0x11, 0x01, 0x05, 0x1A, 55, 0x04, 0x02], Some(true)).unwrap();
        assert_eq!(slow.battery_pct, Some(55));
        assert!(slow.charging);
        assert_eq!(slow.charge_state, HidppChargeState::SlowCharging);

        let full =
            parse_unified_battery(&[0x11, 0x01, 0x05, 0x1A, 100, 0x08, 0x03], Some(true)).unwrap();
        assert_eq!(full.battery_pct, Some(100));
        assert!(!full.charging);
        assert_eq!(full.charge_state, HidppChargeState::Full);
        assert!(format_hidpp_detail_note("UnifiedBattery", 1, &full).contains("Charge complete"));

        let err =
            parse_unified_battery(&[0x11, 0x01, 0x05, 0x1A, 60, 0x04, 0x04], Some(true)).unwrap();
        assert_eq!(err.battery_pct, Some(60));
        assert!(!err.charging);
        assert_eq!(err.charge_state, HidppChargeState::Error("Charging error"));
        assert!(format_hidpp_detail_note("UnifiedBattery", 1, &err).contains("Charging error"));

        // Coarse-only device (supports_soc = false): must NOT fabricate exact percentage
        let caps_coarse =
            parse_unified_battery_capabilities(&[0x11, 0x01, 0x05, 0x0A, 0x0F, 0x00]).unwrap();
        assert!(!caps_coarse.supports_soc);

        let coarse_good =
            parse_unified_battery(&[0x11, 0x01, 0x05, 0x1A, 0x00, 0x04, 0x00], Some(false))
                .unwrap();
        assert_eq!(coarse_good.battery_pct, None);
        assert!(!coarse_good.charging);
        assert_eq!(coarse_good.coarse_level, Some(HidppCoarseLevel::Good));
        let note = format_hidpp_detail_note("UnifiedBattery", 2, &coarse_good);
        assert!(note.contains("Level: Good"));

        // Even if capabilities unavailable (None), soc=0 with coarse bit set must not report 0%
        let inferred_coarse =
            parse_unified_battery(&[0x11, 0x01, 0x05, 0x1A, 0x00, 0x02, 0x00], None).unwrap();
        assert_eq!(inferred_coarse.battery_pct, None);
        assert_eq!(inferred_coarse.coarse_level, Some(HidppCoarseLevel::Low));
    }

    #[test]
    fn test_parse_hidpp_battery_compat() {
        let resp = [0x10, 0x01, 0x06, 0x1A, 68, 50, 0x01];
        assert_eq!(parse_hidpp_battery(&resp), Some((68, true)));

        let full_resp = [0x10, 0x01, 0x06, 0x1A, 100, 95, 0x03];
        assert_eq!(parse_hidpp_battery(&full_resp), Some((100, false)));
    }
}
