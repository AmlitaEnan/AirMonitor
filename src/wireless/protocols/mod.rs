pub mod compx;
pub mod custom;
pub mod hidpp;
pub mod rapoo;
pub mod rongyuan;
pub mod sonix;
pub mod standard;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::state::DeviceKind;
use custom::{CustomHidRule, CustomHidTomlEntry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolFamily {
    Standard,
    Rapoo,
    Sonix,
    Compx,
    Rongyuan,
    Hidpp,
    CustomHid,
    Unknown,
}

impl ProtocolFamily {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "standard" => ProtocolFamily::Standard,
            "rapoo" => ProtocolFamily::Rapoo,
            "sonix" => ProtocolFamily::Sonix,
            "compx" => ProtocolFamily::Compx,
            "rongyuan" => ProtocolFamily::Rongyuan,
            "hidpp" | "hid++" => ProtocolFamily::Hidpp,
            "custom_hid" => ProtocolFamily::CustomHid,
            _ => ProtocolFamily::Unknown,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ProtocolFamily::Standard => "Standard HID Battery",
            ProtocolFamily::Rapoo => "Rapoo",
            ProtocolFamily::Sonix => "SONiX",
            ProtocolFamily::Compx => "Compx",
            ProtocolFamily::Rongyuan => "RongYuan",
            ProtocolFamily::Hidpp => "Logitech HID++",
            ProtocolFamily::CustomHid => "Custom HID",
            ProtocolFamily::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HidInterfaceInfo {
    pub path: String,
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub input_len: u16,
    pub output_len: u16,
    pub feature_len: u16,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
    pub interface_tag: String,
}

#[derive(Debug, Clone, Default)]
pub struct BatteryReading {
    pub battery_pct: Option<u8>,
    pub charging: bool,
    pub online: bool,
    pub detail_note: Option<String>,
    pub raw_packets: Vec<String>,
}

pub trait HidIo {
    fn info(&self) -> &HidInterfaceInfo;
    fn can_rw(&self) -> bool;
    fn write_output(&self, data: &[u8]) -> Result<(), String>;
    fn read_input(&self, timeout_ms: u32) -> Result<Vec<u8>, String>;
    fn get_feature(&self, report_id: u8) -> Result<Vec<u8>, String>;
    fn get_input_report(&self, report_id: u8) -> Result<Vec<u8>, String>;
    fn set_feature(&self, data: &[u8]) -> Result<(), String>;
}

#[derive(Debug, Clone)]
pub struct DeviceMeta {
    pub name: String,
    pub kind: DeviceKind,
    pub protocol: ProtocolFamily,
    pub min_poll_secs: Option<u64>,
    pub allow_active_tx: bool,
}

#[derive(Debug, Deserialize)]
struct DevicesToml {
    #[serde(default)]
    device: Vec<DeviceTomlEntry>,
    #[serde(default)]
    vendor: Vec<VendorTomlEntry>,
    #[serde(default)]
    custom_hid: Vec<CustomHidTomlEntry>,
}

#[derive(Debug, Deserialize)]
struct DeviceTomlEntry {
    vid: String,
    pid: String,
    name: String,
    kind: String,
    protocol: String,
    #[serde(default)]
    min_poll_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct VendorTomlEntry {
    vid: String,
    name: String,
    kind: String,
    protocol: String,
    #[serde(default)]
    min_poll_secs: Option<u64>,
}

pub struct DeviceRegistry {
    exact: HashMap<(u16, u16), DeviceMeta>,
    builtin_exact_protocols: HashMap<(u16, u16), ProtocolFamily>,
    vendors: HashMap<u16, DeviceMeta>,
    custom_rules: Vec<CustomHidRule>,
    diagnostics: Vec<String>,
}

impl DeviceRegistry {
    pub fn load() -> Self {
        let builtin = include_str!("../../../devices.toml");
        let mut overrides: Vec<(String, String)> = Vec::new();
        for path in candidate_registry_paths() {
            if let Ok(text) = fs::read_to_string(&path) {
                overrides.push((path.display().to_string(), text));
            }
        }
        let override_refs: Vec<(&str, &str)> = overrides
            .iter()
            .map(|(label, text)| (label.as_str(), text.as_str()))
            .collect();
        Self::from_toml_parts_labeled(builtin, &override_refs)
    }

    pub fn from_toml_parts(builtin: &str, overrides: &[&str]) -> Self {
        let labeled: Vec<(String, &str)> = overrides
            .iter()
            .enumerate()
            .map(|(i, text)| (format!("external#{}", i + 1), *text))
            .collect();
        let refs: Vec<(&str, &str)> = labeled.iter().map(|(l, t)| (l.as_str(), *t)).collect();
        Self::from_toml_parts_labeled(builtin, &refs)
    }

    fn from_toml_parts_labeled(builtin: &str, overrides: &[(&str, &str)]) -> Self {
        let mut reg = Self {
            exact: HashMap::new(),
            builtin_exact_protocols: HashMap::new(),
            vendors: HashMap::new(),
            custom_rules: Vec::new(),
            diagnostics: Vec::new(),
        };

        reg.merge_toml(builtin, "builtin", false);
        for (label, text) in overrides {
            reg.merge_toml(text, label, true);
        }
        reg
    }

    fn merge_toml(&mut self, text: &str, source_label: &str, is_override: bool) {
        let parsed = match toml::from_str::<DevicesToml>(text) {
            Ok(p) => p,
            Err(e) => {
                if is_override {
                    self.diagnostics
                        .push(format!("[EXT {}] TOML parse error: {}", source_label, e));
                }
                return;
            }
        };

        for d in parsed.device {
            let (Some(vid), Some(pid)) = (parse_hex_u16(&d.vid), parse_hex_u16(&d.pid)) else {
                if is_override {
                    self.diagnostics.push(format!(
                        "[EXT {}] REJECTED [[device]] {}:{} ({}): invalid hex VID/PID",
                        source_label, d.vid, d.pid, d.name
                    ));
                }
                continue;
            };
            let protocol = ProtocolFamily::parse(&d.protocol);
            if protocol == ProtocolFamily::Unknown {
                if is_override {
                    self.diagnostics.push(format!(
                        "[EXT {}] REJECTED [[device]] {:04X}:{:04X} ({}): unknown protocol '{}'",
                        source_label, vid, pid, d.name, d.protocol
                    ));
                }
                continue;
            }

            let allow_active_tx = if is_override {
                let had_builtin = self.exact.contains_key(&(vid, pid))
                    || self
                        .custom_rules
                        .iter()
                        .any(|r| r.vid == vid && r.pid == pid);
                self.custom_rules
                    .retain(|r| !(r.vid == vid && r.pid == pid));
                let same_builtin_proto =
                    self.builtin_exact_protocols.get(&(vid, pid)).copied() == Some(protocol);
                self.diagnostics.push(format!(
                    "[EXT {}] {} [[device]] {:04X}:{:04X} ({}) -> {} [{}]",
                    source_label,
                    if had_builtin { "OVERRIDDEN" } else { "LOADED" },
                    vid,
                    pid,
                    d.name,
                    protocol.label(),
                    if same_builtin_proto {
                        "active-tx-preserved"
                    } else {
                        "read-only"
                    }
                ));
                same_builtin_proto
            } else {
                self.builtin_exact_protocols.insert((vid, pid), protocol);
                true
            };
            self.exact.insert(
                (vid, pid),
                DeviceMeta {
                    name: d.name,
                    kind: parse_kind(&d.kind),
                    protocol,
                    min_poll_secs: d.min_poll_secs,
                    allow_active_tx,
                },
            );
        }

        for v in parsed.vendor {
            let Some(vid) = parse_hex_u16(&v.vid) else {
                if is_override {
                    self.diagnostics.push(format!(
                        "[EXT {}] REJECTED [[vendor]] {} ({}): invalid hex VID",
                        source_label, v.vid, v.name
                    ));
                }
                continue;
            };
            let protocol = ProtocolFamily::parse(&v.protocol);
            if protocol == ProtocolFamily::Unknown {
                if is_override {
                    self.diagnostics.push(format!(
                        "[EXT {}] REJECTED [[vendor]] {:04X} ({}): unknown protocol '{}'",
                        source_label, vid, v.name, v.protocol
                    ));
                }
                continue;
            }
            if is_override {
                self.diagnostics.push(format!(
                    "[EXT {}] LOADED [[vendor]] {:04X} ({}) -> {} [read-only]",
                    source_label,
                    vid,
                    v.name,
                    protocol.label()
                ));
            }
            self.vendors.insert(
                vid,
                DeviceMeta {
                    name: v.name,
                    kind: parse_kind(&v.kind),
                    protocol,
                    min_poll_secs: v.min_poll_secs,
                    allow_active_tx: false,
                },
            );
        }

        let mut parsed_rules = Vec::with_capacity(parsed.custom_hid.len());
        for entry in &parsed.custom_hid {
            let Some(rule) = CustomHidRule::from_toml_with_source(entry, is_override) else {
                if is_override {
                    self.diagnostics.push(format!(
                        "[EXT {}] REJECTED [[custom_hid]] {}:{} ({}): invalid fields/hex",
                        source_label, entry.vid, entry.pid, entry.name
                    ));
                }
                continue;
            };

            if is_override {
                if let Err(reason) = custom::validate_external_rule(entry, &rule) {
                    self.diagnostics.push(format!(
                        "[EXT {}] REJECTED [[custom_hid]] {:04X}:{:04X} ({}): {}",
                        source_label, rule.vid, rule.pid, rule.name, reason
                    ));
                    continue;
                }
                let conflict = self.exact.contains_key(&(rule.vid, rule.pid))
                    || self
                        .custom_rules
                        .iter()
                        .any(|existing| existing.vid == rule.vid && existing.pid == rule.pid);
                let mode = if rule.is_read_only() {
                    "read-only"
                } else {
                    "allow_write=true"
                };
                self.diagnostics.push(format!(
                    "[EXT {}] {} [[custom_hid]] {:04X}:{:04X} ({}) [{}]",
                    source_label,
                    if conflict { "OVERRIDDEN" } else { "LOADED" },
                    rule.vid,
                    rule.pid,
                    rule.name,
                    mode
                ));
            }
            parsed_rules.push(rule);
        }

        if is_override && !parsed_rules.is_empty() {
            // Only replace built-in entries for VID:PIDs that passed external safety validation
            for r in &parsed_rules {
                self.exact.remove(&(r.vid, r.pid));
                self.custom_rules
                    .retain(|existing| !(existing.vid == r.vid && existing.pid == r.pid));
            }
        }
        self.custom_rules.extend(parsed_rules);
    }

    pub fn lookup(&self, info: &HidInterfaceInfo) -> Option<DeviceMeta> {
        if let Some(m) = self.exact.get(&(info.vid, info.pid)) {
            return Some(m.clone());
        }
        if let Some(rule) = self
            .custom_rules
            .iter()
            .find(|r| r.vid == info.vid && r.pid == info.pid && r.matches_interface(info))
            .or_else(|| {
                self.custom_rules
                    .iter()
                    .find(|r| r.vid == info.vid && r.pid == info.pid)
            })
        {
            return Some(DeviceMeta {
                name: rule.name.clone(),
                kind: rule.kind,
                protocol: ProtocolFamily::CustomHid,
                min_poll_secs: rule.min_poll_secs,
                allow_active_tx: !rule.is_external || rule.allow_write,
            });
        }
        if let Some(m) = self.vendors.get(&info.vid) {
            return Some(m.clone());
        }
        if standard::matches(info) {
            let name = if !info.product.is_empty() {
                info.product.clone()
            } else {
                format!("HID Battery {:04X}:{:04X}", info.vid, info.pid)
            };
            return Some(DeviceMeta {
                name,
                kind: DeviceKind::Other,
                protocol: ProtocolFamily::Standard,
                min_poll_secs: None,
                allow_active_tx: false,
            });
        }
        None
    }

    pub fn allows_active_tx(&self, info: &HidInterfaceInfo, family: ProtocolFamily) -> bool {
        match family {
            ProtocolFamily::Standard | ProtocolFamily::Unknown => false,
            ProtocolFamily::CustomHid => self
                .matching_custom_rules(info)
                .into_iter()
                .any(|r| !r.is_external || r.allow_write),
            ProtocolFamily::Rapoo
            | ProtocolFamily::Sonix
            | ProtocolFamily::Compx
            | ProtocolFamily::Rongyuan
            | ProtocolFamily::Hidpp => self
                .exact
                .get(&(info.vid, info.pid))
                .is_some_and(|m| m.allow_active_tx && m.protocol == family),
        }
    }

    pub fn is_vendor_fallback(&self, info: &HidInterfaceInfo) -> bool {
        !self.exact.contains_key(&(info.vid, info.pid))
            && !self
                .custom_rules
                .iter()
                .any(|r| r.vid == info.vid && r.pid == info.pid)
            && self.vendors.contains_key(&info.vid)
    }

    pub fn min_poll_secs(&self, vid: u16, pid: u16) -> Option<u64> {
        if let Some(secs) = self.exact.get(&(vid, pid)).and_then(|m| m.min_poll_secs) {
            return Some(secs);
        }
        if let Some(secs) = self
            .custom_rules
            .iter()
            .filter(|r| r.vid == vid && r.pid == pid)
            .filter_map(|r| r.min_poll_secs)
            .max()
        {
            return Some(secs);
        }
        self.vendors.get(&vid).and_then(|m| m.min_poll_secs)
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub fn matching_custom_rules(&self, info: &HidInterfaceInfo) -> Vec<&CustomHidRule> {
        self.custom_rules
            .iter()
            .filter(|r| r.matches_interface(info))
            .collect()
    }

    pub fn entry_counts(&self) -> (usize, usize, usize) {
        (
            self.exact.len(),
            self.custom_rules.len(),
            self.vendors.len(),
        )
    }
}

pub struct ReadOnlyHidIo<'a>(pub &'a dyn HidIo);

impl<'a> HidIo for ReadOnlyHidIo<'a> {
    fn info(&self) -> &HidInterfaceInfo {
        self.0.info()
    }

    fn can_rw(&self) -> bool {
        self.0.can_rw()
    }

    fn write_output(&self, _data: &[u8]) -> Result<(), String> {
        Err("Read-only mode: unauthorized interface blocks write_output".into())
    }

    fn read_input(&self, timeout_ms: u32) -> Result<Vec<u8>, String> {
        self.0.read_input(timeout_ms)
    }

    fn get_feature(&self, report_id: u8) -> Result<Vec<u8>, String> {
        self.0.get_feature(report_id)
    }

    fn get_input_report(&self, report_id: u8) -> Result<Vec<u8>, String> {
        self.0.get_input_report(report_id)
    }

    fn set_feature(&self, _data: &[u8]) -> Result<(), String> {
        Err("Read-only mode: unauthorized interface blocks set_feature".into())
    }
}

pub fn probe_protocol(
    registry: &DeviceRegistry,
    family: ProtocolFamily,
    io: &dyn HidIo,
) -> Option<BatteryReading> {
    let info = io.info();
    let ro_wrapper = ReadOnlyHidIo(io);
    if standard::matches(info) {
        if let Some(reading) = standard::probe(&ro_wrapper) {
            return Some(reading);
        }
    }

    let effective_io: &dyn HidIo = if registry.allows_active_tx(info, family) {
        io
    } else {
        &ro_wrapper
    };

    match family {
        ProtocolFamily::Standard => standard::probe(&ro_wrapper),
        ProtocolFamily::Rapoo => rapoo::probe(effective_io),
        ProtocolFamily::Sonix => sonix::probe(effective_io),
        ProtocolFamily::Compx => compx::probe(effective_io),
        ProtocolFamily::Rongyuan => rongyuan::probe(effective_io),
        ProtocolFamily::Hidpp => hidpp::probe(effective_io),
        ProtocolFamily::CustomHid => {
            let rules = registry.matching_custom_rules(info);
            custom::probe_rules(&rules, effective_io)
        }
        ProtocolFamily::Unknown => None,
    }
}

pub fn format_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(" ")
}

fn candidate_registry_paths() -> Vec<PathBuf> {
    // Load lower-priority AppData override first, then EXE-adjacent override on top
    let mut paths = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        paths.push(
            PathBuf::from(appdata)
                .join("AirMonitor")
                .join("devices.toml"),
        );
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("devices.toml"));
        }
    }
    paths
}

fn parse_hex_u16(s: &str) -> Option<u16> {
    let trimmed = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    u16::from_str_radix(trimmed, 16).ok()
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

    fn mk_info(vid: u16, pid: u16, up: u16, u: u16) -> HidInterfaceInfo {
        HidInterfaceInfo {
            path: String::new(),
            vid,
            pid,
            usage_page: up,
            usage: u,
            input_len: 64,
            output_len: 64,
            feature_len: 0,
            manufacturer: String::new(),
            product: String::new(),
            serial: String::new(),
            interface_tag: "MI_00".into(),
        }
    }

    struct RecordingHidIo {
        info: HidInterfaceInfo,
        writes: RefCell<Vec<Vec<u8>>>,
        features_set: RefCell<Vec<Vec<u8>>>,
    }

    impl RecordingHidIo {
        fn new(info: HidInterfaceInfo) -> Self {
            Self {
                info,
                writes: RefCell::new(Vec::new()),
                features_set: RefCell::new(Vec::new()),
            }
        }
    }

    impl HidIo for RecordingHidIo {
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
    fn test_embedded_devices_toml_parses_all_entries() {
        let reg = DeviceRegistry::from_toml_parts(include_str!("../../../devices.toml"), &[]);
        let (exact_cnt, custom_cnt, vendor_cnt) = reg.entry_counts();
        assert!(
            exact_cnt >= 100,
            "expected >= 100 [[device]] entries, got {}",
            exact_cnt
        );
        assert_eq!(custom_cnt, 30, "expected 30 [[custom_hid]] entries");
        assert_eq!(vendor_cnt, 6, "expected 6 [[vendor]] entries");
        assert!(exact_cnt + custom_cnt >= 130);

        // Verify key target devices resolve to expected protocol families
        let rapoo = reg
            .lookup(&mk_info(0x24AE, 0x1412, 0xFF00, 0x0002))
            .unwrap();
        assert_eq!(rapoo.protocol, ProtocolFamily::Rapoo);
        assert_eq!(rapoo.name, "Rapoo VT3");

        let skn = reg
            .lookup(&mk_info(0x362D, 0xD101, 0xFF60, 0x0061))
            .unwrap();
        assert_eq!(skn.protocol, ProtocolFamily::Sonix);

        let dareu_a84 = reg
            .lookup(&mk_info(0x260D, 0x0022, 0x0001, 0x0006))
            .unwrap();
        assert_eq!(dareu_a84.protocol, ProtocolFamily::Sonix);

        let hs80 = reg
            .lookup(&mk_info(0x1B1C, 0x0A97, 0xFF42, 0x0001))
            .unwrap();
        assert_eq!(hs80.protocol, ProtocolFamily::CustomHid);
        assert_eq!(hs80.kind, DeviceKind::Headset);

        assert_eq!(reg.min_poll_secs(0x046A, 0x01A1), Some(600));
        assert_eq!(reg.min_poll_secs(0x046A, 0x00EA), Some(600));
        assert_eq!(reg.min_poll_secs(0x320F, 0x5088), Some(600));
        assert_eq!(reg.min_poll_secs(0x24AE, 0x1412), None);
    }

    #[test]
    fn test_no_dead_custom_hid_rules() {
        let reg = DeviceRegistry::from_toml_parts(include_str!("../../../devices.toml"), &[]);
        let mut dead_rules = Vec::new();
        for rule in &reg.custom_rules {
            let info = HidInterfaceInfo {
                path: String::new(),
                vid: rule.vid,
                pid: rule.pid,
                usage_page: rule.usage_page,
                usage: rule.usage,
                input_len: rule.in_len.unwrap_or(64),
                output_len: rule.out_len.unwrap_or(64),
                feature_len: rule.feat_len.unwrap_or(65),
                manufacturer: String::new(),
                product: String::new(),
                serial: String::new(),
                interface_tag: rule
                    .interface
                    .filter(|&i| i >= 0)
                    .map(|i| format!("MI_{:02X}", i))
                    .unwrap_or_else(|| "MI_00".into()),
            };
            let meta = reg.lookup(&info);
            let matched = reg.matching_custom_rules(&info);
            let reachable = meta
                .as_ref()
                .map(|m| m.protocol == ProtocolFamily::CustomHid)
                .unwrap_or(false)
                && !matched.is_empty();
            if !reachable {
                dead_rules.push(format!(
                    "{:04X}:{:04X} ({}) -> meta={:?}",
                    rule.vid,
                    rule.pid,
                    rule.name,
                    meta.map(|m| m.protocol)
                ));
            }
        }
        assert_eq!(
            dead_rules.len(),
            0,
            "found dead [[custom_hid]] rules shadowed by [[device]]: {:?}",
            dead_rules
        );
    }

    #[test]
    fn test_external_override_precedence() {
        let builtin = include_str!("../../../devices.toml");
        let ext = r#"
[[device]]
vid = "362D"
pid = "D101"
name = "SKN Qinglong Custom"
kind = "keyboard"
protocol = "sonix"

[[custom_hid]]
vid = "24AE"
pid = "1412"
name = "Rapoo VT3 Custom Rule"
kind = "mouse"
usage_page = "FF00"
usage = "0002"
report_type = "passive_input"
tx_hex = ""
rx_prefix_hex = "07"
battery_offset = 8
battery_max = 100
charging_offset = 7
charging_mask = 2
"#;
        let reg = DeviceRegistry::from_toml_parts(builtin, &[ext]);

        let skn = reg
            .lookup(&mk_info(0x362D, 0xD101, 0xFF60, 0x0061))
            .unwrap();
        assert_eq!(skn.name, "SKN Qinglong Custom");

        let rapoo = reg
            .lookup(&mk_info(0x24AE, 0x1412, 0xFF00, 0x0002))
            .unwrap();
        assert_eq!(rapoo.protocol, ProtocolFamily::CustomHid);
        assert_eq!(rapoo.name, "Rapoo VT3 Custom Rule");
    }

    #[test]
    fn test_external_write_gate_preserves_builtin_and_logs_diagnostics() {
        let builtin = include_str!("../../../devices.toml");
        let ext = r#"
# Rejected: output_input without allow_write = true trying to hijack built-in 362D:D101
[[custom_hid]]
vid = "362D"
pid = "D101"
name = "Malicious Bootloader Jump"
kind = "keyboard"
usage_page = "FF60"
usage = "0061"
report_type = "output_input"
tx_hex = "000B0000"
battery_offset = 4
battery_max = 100

# Rejected: allow_write = true but forbidden VIA bootloader opcode 0x0B
[[custom_hid]]
vid = "0C45"
pid = "FEFE"
name = "Forbidden VIA Bootloader"
kind = "keyboard"
usage_page = "FF60"
usage = "0061"
report_type = "output_input"
allow_write = true
tx_hex = "000B0000"
battery_offset = 4
battery_max = 100

# Rejected: HID++ SET_REGISTER 0x80 with allow_write = true
[[custom_hid]]
vid = "046D"
pid = "C548"
name = "Forbidden HID++ Set"
kind = "mouse"
usage_page = "FF00"
usage = "0002"
report_type = "output_input"
allow_write = true
tx_hex = "10FF8000000000"
battery_offset = 4
battery_max = 100

# Accepted: read-only get_feature without allow_write
[[custom_hid]]
vid = "1234"
pid = "5678"
name = "Safe ReadOnly Feature"
kind = "mouse"
usage_page = "FF00"
usage = "0001"
report_type = "get_feature"
tx_hex = ""
battery_offset = 2
battery_max = 100
"#;
        let reg = DeviceRegistry::from_toml_parts(builtin, &[ext]);

        // Built-in rules must remain intact when external rules are rejected
        let skn = reg
            .lookup(&mk_info(0x362D, 0xD101, 0xFF60, 0x0061))
            .unwrap();
        assert_eq!(skn.protocol, ProtocolFamily::Sonix);

        let ajazz = reg
            .lookup(&mk_info(0x0C45, 0xFEFE, 0xFF60, 0x0061))
            .unwrap();
        assert_eq!(ajazz.protocol, ProtocolFamily::Sonix);

        let bolt = reg
            .lookup(&mk_info(0x046D, 0xC548, 0xFF00, 0x0002))
            .unwrap();
        assert_eq!(bolt.protocol, ProtocolFamily::Hidpp);

        // Safe read-only external rule is loaded
        let safe = reg
            .lookup(&mk_info(0x1234, 0x5678, 0xFF00, 0x0001))
            .unwrap();
        assert_eq!(safe.protocol, ProtocolFamily::CustomHid);

        let diags = reg.diagnostics();
        assert!(diags
            .iter()
            .any(|d| d.contains("REJECTED") && d.contains("362D:D101")));
        assert!(diags
            .iter()
            .any(|d| d.contains("REJECTED") && d.contains("0C45:FEFE")));
        assert!(diags
            .iter()
            .any(|d| d.contains("REJECTED") && d.contains("046D:C548")));
        assert!(diags
            .iter()
            .any(|d| d.contains("LOADED") && d.contains("1234:5678")));
    }

    #[test]
    fn test_vendor_fallback_blocks_write_frames() {
        let reg = DeviceRegistry::from_toml_parts(include_str!("../../../devices.toml"), &[]);
        // Unknown PID under 362D (vendor -> sonix)
        let mut info = mk_info(0x362D, 0x9999, 0xFF60, 0x0061);
        info.input_len = 33;
        info.output_len = 33;
        assert!(reg.is_vendor_fallback(&info));

        let io = RecordingHidIo::new(info);
        let _ = probe_protocol(&reg, ProtocolFamily::Sonix, &io);
        assert!(
            io.writes.borrow().is_empty(),
            "vendor fallback must not send write_output"
        );
        assert!(
            io.features_set.borrow().is_empty(),
            "vendor fallback must not send set_feature"
        );

        // Unknown PID under 3151 (vendor -> rongyuan)
        let mut ry_info = mk_info(0x3151, 0x9999, 0xFFFF, 0x0002);
        ry_info.feature_len = 65;
        assert!(reg.is_vendor_fallback(&ry_info));
        let ry_io = RecordingHidIo::new(ry_info);
        let _ = probe_protocol(&reg, ProtocolFamily::Rongyuan, &ry_io);
        assert!(ry_io.writes.borrow().is_empty());
        assert!(ry_io.features_set.borrow().is_empty());
    }

    #[test]
    fn test_external_device_mapping_cannot_grant_active_tx_or_bypass_readonly() {
        let builtin = include_str!("../../../devices.toml");
        let ext = r#"
# External [[device]] adding unverified VID:PID with sonix
[[device]]
vid = "1234"
pid = "ABCD"
name = "Unverified External Sonix"
kind = "keyboard"
protocol = "sonix"

# External [[device]] trying to promote unknown PID under vendor 362D to exact sonix
[[device]]
vid = "362D"
pid = "9999"
name = "Unverified Vendor Override"
kind = "keyboard"
protocol = "sonix"

# External [[device]] trying to assign hidpp to unverified VID:PID
[[device]]
vid = "046D"
pid = "9999"
name = "Unverified Logitech PID"
kind = "mouse"
protocol = "hidpp"

# External [[device]] switching protocol on an existing built-in VID:PID (24AE:1412 rapoo -> compx)
[[device]]
vid = "24AE"
pid = "1412"
name = "Rapoo Switched Protocol"
kind = "mouse"
protocol = "compx"

# External [[device]] overriding only name/min_poll_secs on existing built-in 362D:D101 with SAME protocol
[[device]]
vid = "362D"
pid = "D101"
name = "SKN Qinglong Renamed"
kind = "keyboard"
protocol = "sonix"
min_poll_secs = 120
"#;
        let reg = DeviceRegistry::from_toml_parts(builtin, &[ext]);

        // 1. Unverified external [[device]] (1234:ABCD -> sonix) must be read-only (0 TX)
        let mut ext_sonix_info = mk_info(0x1234, 0xABCD, 0xFF60, 0x0061);
        ext_sonix_info.input_len = 33;
        ext_sonix_info.output_len = 33;
        let meta1 = reg.lookup(&ext_sonix_info).unwrap();
        assert!(!meta1.allow_active_tx);
        assert!(!reg.allows_active_tx(&ext_sonix_info, ProtocolFamily::Sonix));
        let io1 = RecordingHidIo::new(ext_sonix_info);
        let _ = probe_protocol(&reg, ProtocolFamily::Sonix, &io1);
        assert!(io1.writes.borrow().is_empty());
        assert!(io1.features_set.borrow().is_empty());

        // 2. External [[device]] overriding vendor fallback unknown PID (362D:9999) must stay read-only (0 TX)
        let mut vendor_override_info = mk_info(0x362D, 0x9999, 0xFF60, 0x0061);
        vendor_override_info.input_len = 33;
        vendor_override_info.output_len = 33;
        let meta2 = reg.lookup(&vendor_override_info).unwrap();
        assert!(!meta2.allow_active_tx);
        assert!(!reg.allows_active_tx(&vendor_override_info, ProtocolFamily::Sonix));
        let io2 = RecordingHidIo::new(vendor_override_info);
        let _ = probe_protocol(&reg, ProtocolFamily::Sonix, &io2);
        assert!(io2.writes.borrow().is_empty());
        assert!(io2.features_set.borrow().is_empty());

        // 3. External [[device]] assigning hidpp to unverified PID (046D:9999) must stay read-only (0 TX)
        let mut ext_hidpp_info = mk_info(0x046D, 0x9999, 0xFF00, 0x0002);
        ext_hidpp_info.input_len = 20;
        ext_hidpp_info.output_len = 20;
        let meta3 = reg.lookup(&ext_hidpp_info).unwrap();
        assert!(!meta3.allow_active_tx);
        assert!(!reg.allows_active_tx(&ext_hidpp_info, ProtocolFamily::Hidpp));
        let io3 = RecordingHidIo::new(ext_hidpp_info);
        let _ = probe_protocol(&reg, ProtocolFamily::Hidpp, &io3);
        assert!(io3.writes.borrow().is_empty());
        assert!(io3.features_set.borrow().is_empty());

        // 4. External [[device]] switching protocol of built-in VID:PID (24AE:1412 rapoo -> compx) must be read-only (0 TX)
        let mut switched_info = mk_info(0x24AE, 0x1412, 0xFF02, 0x0002);
        switched_info.input_len = 17;
        switched_info.output_len = 17;
        let meta4 = reg.lookup(&switched_info).unwrap();
        assert!(!meta4.allow_active_tx);
        let io4 = RecordingHidIo::new(switched_info);
        let _ = probe_protocol(&reg, ProtocolFamily::Compx, &io4);
        assert!(io4.writes.borrow().is_empty());
        assert!(io4.features_set.borrow().is_empty());

        // 5. Completely unknown interface (9999:9999) must produce 0 active writes for any protocol
        for (proto, up, u, len) in [
            (ProtocolFamily::Sonix, 0xFF60, 0x0061, 33),
            (ProtocolFamily::Compx, 0xFF05, 0x0001, 64),
            (ProtocolFamily::Rongyuan, 0xFFFF, 0x0002, 65),
            (ProtocolFamily::Hidpp, 0xFF00, 0x0002, 20),
        ] {
            let mut unk = mk_info(0x9999, 0x9999, up, u);
            unk.input_len = len;
            unk.output_len = len;
            unk.feature_len = len;
            assert!(!reg.allows_active_tx(&unk, proto));
            let io_unk = RecordingHidIo::new(unk);
            let _ = probe_protocol(&reg, proto, &io_unk);
            assert!(
                io_unk.writes.borrow().is_empty(),
                "unknown interface produced write_output for {:?}",
                proto
            );
            assert!(
                io_unk.features_set.borrow().is_empty(),
                "unknown interface produced set_feature for {:?}",
                proto
            );
        }

        // 6. Metadata-only override on existing built-in 362D:D101 with SAME protocol retains allow_active_tx
        let mut skn_info = mk_info(0x362D, 0xD101, 0xFF60, 0x0061);
        skn_info.input_len = 33;
        skn_info.output_len = 33;
        let skn_meta = reg.lookup(&skn_info).unwrap();
        assert_eq!(skn_meta.name, "SKN Qinglong Renamed");
        assert_eq!(skn_meta.min_poll_secs, Some(120));
        assert!(skn_meta.allow_active_tx);
        assert!(reg.allows_active_tx(&skn_info, ProtocolFamily::Sonix));
    }
}
