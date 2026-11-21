use std::thread;
use std::time::Duration;

use serde::Deserialize;

use super::{format_hex, BatteryReading, HidInterfaceInfo, HidIo};
use crate::state::DeviceKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomReportType {
    OutputInput,
    Feature,
    PassiveInput,
}

impl CustomReportType {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "output_input" | "output" => Some(Self::OutputInput),
            "feature" | "get_feature" | "set_feature" => Some(Self::Feature),
            "passive_input" | "passive" => Some(Self::PassiveInput),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RxMatchItem {
    Pair(usize, u8),
    Table { offset: usize, value: u8 },
}

impl RxMatchItem {
    fn as_pair(&self) -> (usize, u8) {
        match *self {
            Self::Pair(offset, value) => (offset, value),
            Self::Table { offset, value } => (offset, value),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RxMatchToml {
    List(Vec<RxMatchItem>),
    Single(RxMatchItem),
}

impl RxMatchToml {
    pub fn to_pairs(&self) -> Vec<(usize, u8)> {
        match self {
            Self::List(items) => items.iter().map(RxMatchItem::as_pair).collect(),
            Self::Single(item) => vec![item.as_pair()],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CustomHidTomlEntry {
    pub vid: String,
    pub pid: String,
    pub name: String,
    pub kind: String,
    pub usage_page: String,
    pub usage: String,
    pub interface: Option<i32>,
    pub report_type: String,
    pub in_len: Option<u16>,
    pub out_len: Option<u16>,
    pub feat_len: Option<u16>,
    #[serde(default)]
    pub tx_hex: String,
    #[serde(default)]
    pub rx_prefix_hex: String,
    #[serde(default)]
    pub rx_match: Option<RxMatchToml>,
    pub battery_offset: usize,
    pub battery_max: u16,
    pub battery_mask: Option<u8>,
    pub battery_xor: Option<u8>,
    #[serde(default = "default_neg_one")]
    pub charging_offset: i32,
    #[serde(default)]
    pub charging_mask: u8,
    #[serde(default)]
    pub allow_write: bool,
    #[serde(default)]
    pub report_id: Option<u8>,
    #[serde(default)]
    pub min_poll_secs: Option<u64>,
}

fn default_neg_one() -> i32 {
    -1
}

#[derive(Debug, Clone)]
pub struct CustomHidRule {
    pub vid: u16,
    pub pid: u16,
    pub name: String,
    pub kind: DeviceKind,
    pub usage_page: u16,
    pub usage: u16,
    pub interface: Option<i32>,
    pub report_type: CustomReportType,
    pub in_len: Option<u16>,
    pub out_len: Option<u16>,
    pub feat_len: Option<u16>,
    pub tx_bytes: Vec<u8>,
    pub rx_prefix: Vec<u8>,
    pub rx_match: Vec<(usize, u8)>,
    pub battery_offset: usize,
    pub battery_max: u16,
    pub battery_mask: u8,
    pub battery_xor: u8,
    pub charging_offset: Option<usize>,
    pub charging_mask: u8,
    pub allow_write: bool,
    pub report_id: Option<u8>,
    pub min_poll_secs: Option<u64>,
    pub is_external: bool,
}

const VIA_FORBIDDEN_OPCODES: &[u8] = &[
    0x03, 0x05, 0x06, 0x07, 0x09, 0x0A, 0x0B, 0x0D, 0x0F, 0x10, 0x13, 0x15,
];

impl CustomHidRule {
    pub fn from_toml(entry: &CustomHidTomlEntry) -> Option<Self> {
        Self::from_toml_with_source(entry, false)
    }

    pub fn from_toml_with_source(entry: &CustomHidTomlEntry, is_external: bool) -> Option<Self> {
        let vid = parse_hex_u16(&entry.vid)?;
        let pid = parse_hex_u16(&entry.pid)?;
        let usage_page = parse_hex_u16(&entry.usage_page)?;
        let usage = parse_hex_u16(&entry.usage)?;
        let report_type = CustomReportType::parse(&entry.report_type)?;
        let tx_bytes = parse_hex_bytes(&entry.tx_hex).ok()?;
        let rx_prefix = parse_hex_bytes(&entry.rx_prefix_hex).ok()?;
        let rx_match = entry
            .rx_match
            .as_ref()
            .map(RxMatchToml::to_pairs)
            .unwrap_or_default();
        let charging_offset = if entry.charging_offset >= 0 {
            Some(entry.charging_offset as usize)
        } else {
            None
        };

        Some(Self {
            vid,
            pid,
            name: entry.name.clone(),
            kind: parse_kind(&entry.kind),
            usage_page,
            usage,
            interface: entry.interface,
            report_type,
            in_len: entry.in_len,
            out_len: entry.out_len,
            feat_len: entry.feat_len,
            tx_bytes,
            rx_prefix,
            rx_match,
            battery_offset: entry.battery_offset,
            battery_max: entry.battery_max.max(1),
            battery_mask: entry.battery_mask.unwrap_or(0xFF),
            battery_xor: entry.battery_xor.unwrap_or(0x00),
            charging_offset,
            charging_mask: entry.charging_mask,
            allow_write: entry.allow_write,
            report_id: entry.report_id,
            min_poll_secs: entry.min_poll_secs,
            is_external,
        })
    }

    pub fn is_read_only(&self) -> bool {
        matches!(
            self.report_type,
            CustomReportType::PassiveInput | CustomReportType::Feature
        ) && self.tx_bytes.is_empty()
    }

    pub fn matches_interface(&self, info: &HidInterfaceInfo) -> bool {
        if info.vid != self.vid
            || info.pid != self.pid
            || info.usage_page != self.usage_page
            || info.usage != self.usage
        {
            return false;
        }
        if let Some(iface) = self.interface {
            if iface >= 0 && parse_mi_number(&info.interface_tag) != Some(iface) {
                return false;
            }
        }
        if let Some(len) = self.in_len {
            if info.input_len != len {
                return false;
            }
        }
        if let Some(len) = self.out_len {
            if info.output_len != len {
                return false;
            }
        }
        if let Some(len) = self.feat_len {
            if info.feature_len != len {
                return false;
            }
        }
        true
    }
}

pub fn requires_write_for_entry(
    entry: &CustomHidTomlEntry,
    report_type: CustomReportType,
    tx_bytes: &[u8],
) -> bool {
    let rt = entry.report_type.trim().to_ascii_lowercase();
    matches!(report_type, CustomReportType::OutputInput)
        || !tx_bytes.is_empty()
        || rt == "set_feature"
        || rt == "output"
}

pub fn validate_tx_opcodes(rule: &CustomHidRule) -> Result<(), String> {
    if rule.tx_bytes.is_empty() {
        return Ok(());
    }

    // VIA / QMK Raw HID (FF60) dangerous opcode blacklist
    if rule.usage_page == 0xFF60 {
        let opcode = if rule.tx_bytes.first() == Some(&0x00) {
            rule.tx_bytes.get(1).copied()
        } else {
            rule.tx_bytes.first().copied()
        };
        if let Some(op) = opcode {
            if VIA_FORBIDDEN_OPCODES.contains(&op) {
                return Err(format!("blocked forbidden VIA/FF60 opcode 0x{:02X}", op));
            }
        }
    }

    // Logitech HID++ dangerous write/set/DFU/reset opcode blacklist
    let is_hidpp_frame = rule.vid == 0x046D
        || (rule.usage_page == 0xFF00
            && matches!(rule.tx_bytes.first(), Some(0x10..=0x12))
            && rule.tx_bytes.len() >= 4);
    if is_hidpp_frame && rule.tx_bytes.len() >= 3 {
        let sub_id = rule.tx_bytes[2];
        // HID++ 1.0 SET_REGISTER (0x80), SET_LONG_REGISTER (0x82), SET_VERY_LONG_REGISTER (0x84),
        // pairing/receiver write (0xB2, 0xB5), error/DFU/reset/reserved ranges
        if matches!(
            sub_id,
            0x80 | 0x82 | 0x84 | 0x8E | 0x8F | 0xB2 | 0xB5 | 0xD0..=0xDF | 0xF0..=0xFF
        ) {
            return Err(format!(
                "blocked high-risk HID++ write/control sub-id 0x{:02X}",
                sub_id
            ));
        }
        // IRoot (0x00) only permits read-only getFeature (func 0) and getProtocolVersion (func 1)
        if sub_id == 0x00 && rule.tx_bytes.len() >= 4 && (rule.tx_bytes[3] >> 4) > 1 {
            return Err(format!(
                "blocked non-read HID++ IRoot function 0x{:02X}",
                rule.tx_bytes[3] >> 4
            ));
        }
    }

    Ok(())
}

pub fn validate_external_rule(
    entry: &CustomHidTomlEntry,
    rule: &CustomHidRule,
) -> Result<(), String> {
    let rt = entry.report_type.trim().to_ascii_lowercase();
    if rt == "get_feature" && !rule.tx_bytes.is_empty() {
        return Err("get_feature rule must not specify tx_hex".into());
    }
    if rt == "passive_input" && !rule.tx_bytes.is_empty() {
        return Err("passive_input rule must not specify tx_hex".into());
    }
    if requires_write_for_entry(entry, rule.report_type, &rule.tx_bytes) && !rule.allow_write {
        return Err(
            "external write rule rejected: set allow_write = true to enable write/output/set_feature"
                .into(),
        );
    }
    if rule.report_type == CustomReportType::OutputInput && rule.tx_bytes.is_empty() {
        return Err("output_input rule requires non-empty tx_hex".into());
    }
    if let Some(out_len) = rule.out_len {
        if rule.report_type == CustomReportType::OutputInput
            && rule.tx_bytes.len() > out_len as usize
        {
            return Err(format!(
                "tx_hex len ({}) exceeds declared out_len ({})",
                rule.tx_bytes.len(),
                out_len
            ));
        }
    }
    if let Some(feat_len) = rule.feat_len {
        if rule.report_type == CustomReportType::Feature && rule.tx_bytes.len() > feat_len as usize
        {
            return Err(format!(
                "tx_hex len ({}) exceeds declared feat_len ({})",
                rule.tx_bytes.len(),
                feat_len
            ));
        }
    }
    validate_tx_opcodes(rule)
}

pub fn validate_tx_length(rule: &CustomHidRule, info: &HidInterfaceInfo) -> Result<(), String> {
    if rule.tx_bytes.is_empty() {
        return Ok(());
    }
    match rule.report_type {
        CustomReportType::OutputInput => {
            if info.output_len == 0 {
                return Err("target HID interface has output_len = 0".into());
            }
            if rule.tx_bytes.len() > info.output_len as usize {
                return Err(format!(
                    "tx len ({}) exceeds HID output_len ({})",
                    rule.tx_bytes.len(),
                    info.output_len
                ));
            }
        }
        CustomReportType::Feature => {
            if info.feature_len == 0 {
                return Err("target HID interface has feature_len = 0".into());
            }
            if rule.tx_bytes.len() > info.feature_len as usize {
                return Err(format!(
                    "tx len ({}) exceeds HID feature_len ({})",
                    rule.tx_bytes.len(),
                    info.feature_len
                ));
            }
        }
        CustomReportType::PassiveInput => {
            return Err("passive_input rule cannot transmit tx bytes".into());
        }
    }
    Ok(())
}

pub fn probe_rules(rules: &[&CustomHidRule], io: &dyn HidIo) -> Option<BatteryReading> {
    let info = io.info();
    let mut raw_packets = Vec::new();
    let mut blocked_only = false;

    for rule in rules {
        if !rule.matches_interface(info) {
            continue;
        }
        let before = raw_packets.len();
        if let Some(reading) = probe_single_rule(rule, io, &mut raw_packets) {
            return Some(reading);
        }
        if raw_packets.len() > before
            && raw_packets[before..]
                .iter()
                .all(|p| p.starts_with("BLOCKED"))
        {
            blocked_only = true;
        } else if raw_packets.len() > before {
            blocked_only = false;
        }
    }

    if raw_packets.is_empty() {
        None
    } else {
        Some(BatteryReading {
            battery_pct: None,
            charging: false,
            online: !blocked_only,
            detail_note: Some(if blocked_only {
                "Custom HID (TX blocked by safety gate)".into()
            } else {
                "Custom HID (raw captured)".into()
            }),
            raw_packets,
        })
    }
}

fn check_rule_tx_safety(rule: &CustomHidRule, info: &HidInterfaceInfo) -> Result<(), String> {
    if rule.is_external {
        if !rule.is_read_only() && !rule.allow_write {
            return Err("external rule lacks allow_write = true".into());
        }
        validate_tx_opcodes(rule)?;
    }
    validate_tx_length(rule, info)
}

fn probe_single_rule(
    rule: &CustomHidRule,
    io: &dyn HidIo,
    raw_packets: &mut Vec<String>,
) -> Option<BatteryReading> {
    let info = io.info();
    match rule.report_type {
        CustomReportType::Feature => {
            let rid = if !rule.tx_bytes.is_empty() {
                rule.tx_bytes[0]
            } else {
                rule.report_id.unwrap_or(0)
            };
            if !rule.tx_bytes.is_empty() {
                if let Err(reason) = check_rule_tx_safety(rule, info) {
                    raw_packets.push(format!("BLOCKED [{}]: {}", rule.name, reason));
                    return None;
                }
                if io.set_feature(&rule.tx_bytes).is_err() {
                    return None;
                }
                thread::sleep(Duration::from_millis(60));
            }
            let mut zero_seen = false;
            for _ in 0..3 {
                if let Ok(buf) = io.get_feature(rid) {
                    if buf.iter().any(|&b| b != 0) {
                        raw_packets.push(format!("FEAT[0x{:02X}]: {}", rid, format_hex(&buf)));
                        if let Some((pct, chg)) = decode_response(rule, &buf) {
                            if pct == 0 && !zero_seen {
                                zero_seen = true;
                                thread::sleep(Duration::from_millis(40));
                                continue;
                            }
                            return Some(BatteryReading {
                                battery_pct: Some(pct),
                                charging: chg,
                                online: true,
                                detail_note: Some("Custom HID (Feature)".into()),
                                raw_packets: raw_packets.clone(),
                            });
                        }
                    }
                }
                thread::sleep(Duration::from_millis(40));
            }
            None
        }
        CustomReportType::OutputInput => {
            if !io.can_rw() || rule.tx_bytes.is_empty() {
                return None;
            }
            if let Err(reason) = check_rule_tx_safety(rule, info) {
                raw_packets.push(format!("BLOCKED [{}]: {}", rule.name, reason));
                return None;
            }
            for _ in 0..3 {
                if io.read_input(15).is_err() {
                    break;
                }
            }
            if io.write_output(&rule.tx_bytes).is_err() {
                return None;
            }
            let mut zero_seen = false;
            for _ in 0..4 {
                let Ok(rx) = io.read_input(250) else {
                    break;
                };
                if rx.iter().any(|&b| b != 0) {
                    let preview = &rule.tx_bytes[..rule.tx_bytes.len().min(6)];
                    raw_packets.push(format!(
                        "TX[{}..] -> RX[{}]",
                        format_hex(preview),
                        format_hex(&rx)
                    ));
                    if let Some((pct, chg)) = decode_response(rule, &rx) {
                        if pct == 0 && !zero_seen {
                            zero_seen = true;
                            // Secondary confirmation read before accepting 0%
                            let _ = io.write_output(&rule.tx_bytes);
                            continue;
                        }
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some("Custom HID (Output/Input)".into()),
                            raw_packets: raw_packets.clone(),
                        });
                    }
                }
            }
            None
        }
        CustomReportType::PassiveInput => {
            if !io.can_rw() {
                return None;
            }
            let mut zero_seen = false;
            for _ in 0..16 {
                let Ok(rx) = io.read_input(35) else {
                    break;
                };
                if rx.iter().any(|&b| b != 0) {
                    raw_packets.push(format!("PASSIVE: {}", format_hex(&rx)));
                    if let Some((pct, chg)) = decode_response(rule, &rx) {
                        if pct == 0 && !zero_seen {
                            zero_seen = true;
                            continue;
                        }
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some("Custom HID (Passive)".into()),
                            raw_packets: raw_packets.clone(),
                        });
                    }
                }
            }
            for _ in 0..2 {
                let Ok(rx) = io.read_input(250) else {
                    break;
                };
                if rx.iter().any(|&b| b != 0) {
                    raw_packets.push(format!("PASSIVE: {}", format_hex(&rx)));
                    if let Some((pct, chg)) = decode_response(rule, &rx) {
                        if pct == 0 && !zero_seen {
                            zero_seen = true;
                            continue;
                        }
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging: chg,
                            online: true,
                            detail_note: Some("Custom HID (Passive)".into()),
                            raw_packets: raw_packets.clone(),
                        });
                    }
                }
            }
            None
        }
    }
}

pub fn decode_response(rule: &CustomHidRule, rx: &[u8]) -> Option<(u8, bool)> {
    if rx.is_empty() || rx.iter().all(|&b| b == 0) {
        return None;
    }

    let slice = if !rule.rx_prefix.is_empty() {
        if rx.starts_with(&rule.rx_prefix) {
            rx
        } else if rx.len() > 1 && rx[0] == 0x00 && rx[1..].starts_with(&rule.rx_prefix) {
            // Handle Windows preserving leading Report ID 0x00 when rule expects unprefixed payload
            &rx[1..]
        } else {
            return None;
        }
    } else {
        rx
    };

    if slice.iter().all(|&b| b == 0) {
        return None;
    }

    for &(offset, expected) in &rule.rx_match {
        if slice.get(offset).copied() != Some(expected) {
            return None;
        }
    }

    let raw_val: u16 = if rule.battery_max > 255 {
        if rule.battery_offset + 1 >= slice.len() {
            return None;
        }
        u16::from_le_bytes([slice[rule.battery_offset], slice[rule.battery_offset + 1]])
    } else {
        if rule.battery_offset >= slice.len() {
            return None;
        }
        ((slice[rule.battery_offset] ^ rule.battery_xor) & rule.battery_mask) as u16
    };

    if raw_val > rule.battery_max {
        return None;
    }

    let pct = (((raw_val as u32) * 100) / (rule.battery_max as u32)).min(100) as u8;

    if pct == 0 && rule.battery_xor == 0 {
        let payload_start = if rule.rx_prefix.is_empty() {
            1.min(slice.len())
        } else {
            rule.rx_prefix.len()
        };
        if slice[payload_start..].iter().all(|&b| b == 0) {
            return None;
        }
    }

    let charging = if let Some(chg_off) = rule.charging_offset {
        if chg_off < slice.len() && rule.charging_mask != 0 {
            ((slice[chg_off] ^ rule.battery_xor) & rule.charging_mask) != 0
        } else {
            false
        }
    } else {
        false
    };

    Some((pct, charging))
}

pub fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if !clean.len().is_multiple_of(2) {
        return Err("Odd hex length".into());
    }
    let mut out = Vec::with_capacity(clean.len() / 2);
    for i in (0..clean.len()).step_by(2) {
        let b = u8::from_str_radix(&clean[i..i + 2], 16).map_err(|e| e.to_string())?;
        out.push(b);
    }
    Ok(out)
}

fn parse_hex_u16(s: &str) -> Option<u16> {
    let trimmed = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    u16::from_str_radix(trimmed, 16).ok()
}

fn parse_mi_number(tag: &str) -> Option<i32> {
    let upper = tag.to_ascii_uppercase();
    let hex = upper.strip_prefix("MI_")?;
    i32::from_str_radix(hex, 16).ok()
}

fn parse_kind(s: &str) -> DeviceKind {
    match s.trim().to_ascii_lowercase().as_str() {
        "mouse" => DeviceKind::Mouse,
        "keyboard" => DeviceKind::Keyboard,
        "headset" => DeviceKind::Headset,
        "earbuds" => DeviceKind::Earbuds,
        "gamepad" => DeviceKind::Gamepad,
        _ => DeviceKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn sample_info() -> HidInterfaceInfo {
        HidInterfaceInfo {
            path: "\\\\?\\hid#vid_1038&pid_2202&mi_00".into(),
            vid: 0x1038,
            pid: 0x2202,
            usage_page: 0xFFC0,
            usage: 0x0001,
            input_len: 64,
            output_len: 64,
            feature_len: 0,
            manufacturer: "SteelSeries".into(),
            product: "Arctis Nova 7".into(),
            serial: String::new(),
            interface_tag: "MI_00".into(),
        }
    }

    fn base_entry() -> CustomHidTomlEntry {
        CustomHidTomlEntry {
            vid: "1038".into(),
            pid: "2202".into(),
            name: "SteelSeries Arctis Nova 7".into(),
            kind: "headset".into(),
            usage_page: "FFC0".into(),
            usage: "0001".into(),
            interface: Some(0),
            report_type: "output_input".into(),
            in_len: Some(64),
            out_len: Some(64),
            feat_len: None,
            tx_hex: "00B0".into(),
            rx_prefix_hex: "".into(),
            rx_match: None,
            battery_offset: 2,
            battery_max: 4,
            battery_mask: None,
            battery_xor: None,
            charging_offset: 3,
            charging_mask: 3,
            allow_write: false,
            report_id: None,
            min_poll_secs: None,
        }
    }

    struct MockHidIo {
        info: HidInterfaceInfo,
        writes: RefCell<Vec<Vec<u8>>>,
        features_set: RefCell<Vec<Vec<u8>>>,
        feature_gets: RefCell<Vec<u8>>,
        feature_responses: RefCell<std::collections::HashMap<u8, Vec<u8>>>,
        inputs: RefCell<Vec<Vec<u8>>>,
    }

    impl MockHidIo {
        fn new(info: HidInterfaceInfo, inputs: Vec<Vec<u8>>) -> Self {
            Self {
                info,
                writes: RefCell::new(Vec::new()),
                features_set: RefCell::new(Vec::new()),
                feature_gets: RefCell::new(Vec::new()),
                feature_responses: RefCell::new(std::collections::HashMap::new()),
                inputs: RefCell::new(inputs),
            }
        }
    }

    impl HidIo for MockHidIo {
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
            let mut q = self.inputs.borrow_mut();
            if q.is_empty() {
                Err("timeout".into())
            } else {
                Ok(q.remove(0))
            }
        }
        fn get_feature(&self, report_id: u8) -> Result<Vec<u8>, String> {
            self.feature_gets.borrow_mut().push(report_id);
            self.feature_responses
                .borrow()
                .get(&report_id)
                .cloned()
                .ok_or_else(|| "unsupported".into())
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
    fn test_custom_rule_matching_and_scaling() {
        let toml_entry = base_entry();
        let rule = CustomHidRule::from_toml(&toml_entry).unwrap();
        let info = sample_info();
        assert!(rule.matches_interface(&info));

        let mut wrong_iface = info.clone();
        wrong_iface.interface_tag = "MI_03".into();
        assert!(!rule.matches_interface(&wrong_iface));

        // level=3 of max=4 -> 75%, charging byte=1 (mask 3) -> true
        let rx = [0x00, 0xB0, 3, 1];
        assert_eq!(decode_response(&rule, &rx), Some((75, true)));

        // level=2 of max=4 -> 50%, charging byte=0 -> false
        let rx2 = [0x00, 0xB0, 2, 0];
        assert_eq!(decode_response(&rule, &rx2), Some((50, false)));
    }

    #[test]
    fn test_custom_rule_xor_and_bitmask() {
        // Realtek XOR 0xFF (MCHOSE 5253:1020)
        let realtek_entry = CustomHidTomlEntry {
            vid: "5253".into(),
            pid: "1020".into(),
            name: "MCHOSE L7".into(),
            kind: "mouse".into(),
            usage_page: "FF01".into(),
            usage: "0001".into(),
            interface: None,
            report_type: "feature".into(),
            in_len: None,
            out_len: None,
            feat_len: Some(65),
            tx_hex: "12F9FFFFFFFFFFFFFF".into(),
            rx_prefix_hex: "12F9".into(),
            rx_match: None,
            battery_offset: 11,
            battery_max: 100,
            battery_mask: None,
            battery_xor: Some(0xFF),
            charging_offset: 12,
            charging_mask: 0xFF,
            allow_write: false,
            report_id: None,
            min_poll_secs: None,
        };
        let rule = CustomHidRule::from_toml(&realtek_entry).unwrap();
        let mut rx = vec![0xFFu8; 65];
        rx[0] = 0x12;
        rx[1] = 0xF9;
        rx[11] = 84 ^ 0xFF; // 84%
        rx[12] = 0x01 ^ 0xFF; // charging
        assert_eq!(decode_response(&rule, &rx), Some((84, true)));

        rx[12] = 0xFF; // 0x00 ^ 0xFF (discharging)
        assert_eq!(decode_response(&rule, &rx), Some((84, false)));
    }

    #[test]
    fn test_custom_rule_packed_mask_with_rid_shift() {
        // MCHOSE GLW (3837:3033): bit7 charging, bits 0..6 pct, rx_prefix AA05
        let glw = CustomHidTomlEntry {
            vid: "3837".into(),
            pid: "3033".into(),
            name: "MCHOSE G75 V2".into(),
            kind: "keyboard".into(),
            usage_page: "0001".into(),
            usage: "0000".into(),
            interface: None,
            report_type: "output_input".into(),
            in_len: Some(65),
            out_len: Some(65),
            feat_len: None,
            tx_hex: "005505003838000000".into(),
            rx_prefix_hex: "AA05".into(),
            rx_match: None,
            battery_offset: 40,
            battery_max: 100,
            battery_mask: Some(127),
            battery_xor: None,
            charging_offset: 40,
            charging_mask: 128,
            allow_write: false,
            report_id: None,
            min_poll_secs: None,
        };
        let rule = CustomHidRule::from_toml(&glw).unwrap();
        let mut rx = vec![0u8; 65];
        rx[0] = 0x00; // leading RID preserved by Win32
        rx[1] = 0xAA;
        rx[2] = 0x05;
        rx[41] = 0x80 | 64; // shifted by 1 due to leading RID 0x00
        assert_eq!(decode_response(&rule, &rx), Some((64, true)));
    }

    #[test]
    fn test_rx_match_and_zero_payload_rejection() {
        let mut cherry = base_entry();
        cherry.vid = "046A".into();
        cherry.pid = "00EA".into();
        cherry.usage_page = "FF1C".into();
        cherry.usage = "0092".into();
        cherry.rx_prefix_hex = "04".into();
        cherry.rx_match = Some(RxMatchToml::List(vec![RxMatchItem::Pair(3, 0x1A)]));
        cherry.battery_offset = 8;
        cherry.battery_max = 100;
        cherry.charging_offset = 9;
        cherry.charging_mask = 255;

        let rule = CustomHidRule::from_toml(&cherry).unwrap();
        let mut rx = vec![0u8; 64];
        rx[0] = 0x04;
        rx[3] = 0x1B; // wrong type byte
        rx[8] = 85;
        assert_eq!(decode_response(&rule, &rx), None);

        rx[3] = 0x1A;
        assert_eq!(decode_response(&rule, &rx), Some((85, false)));

        // All-zero packet and prefix-with-all-zero-payload must not report 0%
        let all_zero = vec![0u8; 64];
        assert_eq!(decode_response(&rule, &all_zero), None);

        let mut prefix_only = base_entry();
        prefix_only.rx_prefix_hex = "04".into();
        prefix_only.battery_offset = 2;
        prefix_only.battery_max = 100;
        let prefix_rule = CustomHidRule::from_toml(&prefix_only).unwrap();
        assert_eq!(
            decode_response(&prefix_rule, &[0x04, 0x00, 0x00, 0x00]),
            None
        );
    }

    #[test]
    fn test_external_write_gate_and_blacklist() {
        // 1. Write rule without allow_write = true is rejected
        let mut ext_entry = base_entry();
        ext_entry.allow_write = false;
        let rule = CustomHidRule::from_toml_with_source(&ext_entry, true).unwrap();
        assert!(validate_external_rule(&ext_entry, &rule).is_err());

        // 2. Read-only passive / get_feature without allow_write is allowed
        let mut ro_entry = base_entry();
        ro_entry.report_type = "get_feature".into();
        ro_entry.tx_hex = "".into();
        let ro_rule = CustomHidRule::from_toml_with_source(&ro_entry, true).unwrap();
        assert!(validate_external_rule(&ro_entry, &ro_rule).is_ok());

        // 3. VIA dangerous opcodes (0x0B bootloader jump, 0x0A eeprom reset) blocked even with allow_write = true
        for &bad_op in VIA_FORBIDDEN_OPCODES {
            let mut via = base_entry();
            via.usage_page = "FF60".into();
            via.usage = "0061".into();
            via.allow_write = true;
            via.tx_hex = format!("00{:02X}0000", bad_op);
            let via_rule = CustomHidRule::from_toml_with_source(&via, true).unwrap();
            assert!(
                validate_external_rule(&via, &via_rule).is_err(),
                "expected VIA opcode 0x{:02X} to be blocked",
                bad_op
            );
        }

        // Safe VIA query (0x20) allowed when allow_write = true
        let mut safe_via = base_entry();
        safe_via.usage_page = "FF60".into();
        safe_via.usage = "0061".into();
        safe_via.allow_write = true;
        safe_via.tx_hex = "00200100".into();
        let safe_via_rule = CustomHidRule::from_toml_with_source(&safe_via, true).unwrap();
        assert!(validate_external_rule(&safe_via, &safe_via_rule).is_ok());

        // 4. HID++ SET_REGISTER (0x80/0x82/0x84) blocked even with allow_write = true
        for &sub_id in &[0x80u8, 0x82, 0x84] {
            let mut hidpp = base_entry();
            hidpp.vid = "046D".into();
            hidpp.pid = "C548".into();
            hidpp.usage_page = "FF00".into();
            hidpp.usage = "0002".into();
            hidpp.allow_write = true;
            hidpp.tx_hex = format!("10FF{:02X}00000000", sub_id);
            let hidpp_rule = CustomHidRule::from_toml_with_source(&hidpp, true).unwrap();
            assert!(validate_external_rule(&hidpp, &hidpp_rule).is_err());
        }
    }

    #[test]
    fn test_tx_length_guard_blocks_oversized_tx() {
        let mut entry = base_entry();
        entry.out_len = None;
        entry.tx_hex = "0102030405060708".into(); // 8 bytes
        entry.allow_write = true;
        let rule = CustomHidRule::from_toml_with_source(&entry, true).unwrap();

        let mut info = sample_info();
        info.output_len = 4; // smaller than tx_bytes.len() (8)

        let io = MockHidIo::new(info, vec![]);
        let reading = probe_rules(&[&rule], &io).unwrap();
        assert!(
            io.writes.borrow().is_empty(),
            "oversized TX must not be sent"
        );
        assert!(!reading.online);
        assert!(reading
            .raw_packets
            .iter()
            .any(|p| p.contains("BLOCKED") && p.contains("exceeds HID output_len")));
    }

    #[test]
    fn test_readonly_get_feature_uses_explicit_report_id_and_defaults_to_zero() {
        let mut entry = base_entry();
        entry.report_type = "get_feature".into();
        entry.tx_hex = "".into();
        entry.rx_prefix_hex = "07".into();
        entry.battery_offset = 2;
        entry.battery_max = 100;
        entry.charging_offset = -1;
        entry.report_id = Some(0x07);
        entry.out_len = None;
        entry.in_len = None;
        entry.feat_len = Some(16);

        let rule = CustomHidRule::from_toml_with_source(&entry, true).unwrap();
        assert!(rule.is_read_only());
        assert!(validate_external_rule(&entry, &rule).is_ok());

        let mut info = sample_info();
        info.feature_len = 16;
        let io = MockHidIo::new(info.clone(), vec![]);
        io.feature_responses
            .borrow_mut()
            .insert(0x07, vec![0x07, 0x01, 66, 0x00]);
        let reading = probe_rules(&[&rule], &io).expect("get_feature(0x07) should succeed");
        assert_eq!(reading.battery_pct, Some(66));
        assert_eq!(*io.feature_gets.borrow(), vec![0x07]);
        assert!(io.writes.borrow().is_empty());
        assert!(io.features_set.borrow().is_empty());

        // Legacy rule without report_id defaults to 0
        entry.report_id = None;
        entry.rx_prefix_hex = "".into();
        let legacy_rule = CustomHidRule::from_toml_with_source(&entry, true).unwrap();
        let io_legacy = MockHidIo::new(info, vec![]);
        io_legacy
            .feature_responses
            .borrow_mut()
            .insert(0x00, vec![0x00, 0x01, 52, 0x00]);
        let legacy_reading =
            probe_rules(&[&legacy_rule], &io_legacy).expect("legacy get_feature should use rid 0");
        assert_eq!(legacy_reading.battery_pct, Some(52));
        assert_eq!(*io_legacy.feature_gets.borrow(), vec![0x00]);
    }
}
