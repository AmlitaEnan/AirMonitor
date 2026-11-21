use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

pub(crate) const MAX_OFFSET_X: i32 = 8192;
const MIN_LOW_BATTERY_THRESHOLD: u8 = 5;
const MAX_LOW_BATTERY_THRESHOLD: u8 = 50;
const MIN_HID_POLL_SECS: u64 = 10;
const MAX_HID_POLL_SECS: u64 = 600;
const MIN_HW_POLL_INTERVAL_MS: u64 = 500;
const MAX_HW_POLL_INTERVAL_MS: u64 = 5000;

static READ_ONLY_MODE: AtomicBool = AtomicBool::new(false);
static TMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Enables or disables read-only config mode (used by `--smoke-test` so it
/// never writes `config.toml` or `.bak` files).
pub fn set_read_only_mode(enabled: bool) {
    READ_ONLY_MODE.store(enabled, Ordering::Release);
}

pub fn is_read_only_mode() -> bool {
    READ_ONLY_MODE.load(Ordering::Acquire)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HwMetric {
    CpuUsage,
    CpuFreq,
    CpuTemp,
    CpuPower,
    CpuFan,
    MemUsage,
    NetUp,
    NetDown,
    DiskRead,
    DiskWrite,
    DiskTemp,
    SysBattery,
    GpuUsage,
    GpuTemp,
    GpuHotspot,
    GpuPower,
    GpuVram,
    GpuFan,
}

impl HwMetric {
    pub const ALL: &'static [HwMetric] = &[
        HwMetric::CpuUsage,
        HwMetric::CpuFreq,
        HwMetric::CpuTemp,
        HwMetric::CpuPower,
        HwMetric::CpuFan,
        HwMetric::MemUsage,
        HwMetric::NetUp,
        HwMetric::NetDown,
        HwMetric::DiskRead,
        HwMetric::DiskWrite,
        HwMetric::DiskTemp,
        HwMetric::SysBattery,
        HwMetric::GpuUsage,
        HwMetric::GpuTemp,
        HwMetric::GpuHotspot,
        HwMetric::GpuPower,
        HwMetric::GpuVram,
        HwMetric::GpuFan,
    ];

    pub fn display_name(&self) -> &'static str {
        match self {
            HwMetric::CpuUsage => "CPU 占用",
            HwMetric::CpuFreq => "CPU 频率",
            HwMetric::CpuTemp => "CPU 温度",
            HwMetric::CpuPower => "CPU 功耗",
            HwMetric::CpuFan => "CPU 风扇",
            HwMetric::MemUsage => "内存占用",
            HwMetric::NetUp => "网络上行",
            HwMetric::NetDown => "网络下行",
            HwMetric::DiskRead => "磁盘读取",
            HwMetric::DiskWrite => "磁盘写入",
            HwMetric::DiskTemp => "硬盘温度",
            HwMetric::SysBattery => "本机电池",
            HwMetric::GpuUsage => "GPU 占用",
            HwMetric::GpuTemp => "GPU 温度",
            HwMetric::GpuHotspot => "GPU 热点",
            HwMetric::GpuPower => "GPU 功耗",
            HwMetric::GpuVram => "GPU 显存",
            HwMetric::GpuFan => "GPU 风扇",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutMode {
    /// Hardware & wireless devices in a single unified taskbar bar
    #[default]
    Combined,
    /// Hardware & wireless devices in two independent taskbar bars
    Split,
    /// Only hardware metrics bar
    HardwareOnly,
    /// Only wireless devices bar
    DevicesOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CombinedOrder {
    HardwareFirst,
    #[default]
    DevicesFirst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorEdge {
    /// Anchored to the left edge of the taskbar; grows rightward
    Left,
    /// Anchored to the left edge of the system tray (right side); grows leftward
    #[default]
    Right,
}

impl AnchorEdge {
    pub fn opposite(self) -> Self {
        match self {
            AnchorEdge::Left => AnchorEdge::Right,
            AnchorEdge::Right => AnchorEdge::Left,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZoneLayout {
    #[serde(deserialize_with = "deserialize_enum_or_default")]
    pub anchor: AnchorEdge,
    pub offset_x: i32,
    pub visible: bool,
}

impl Default for ZoneLayout {
    fn default() -> Self {
        Self {
            anchor: AnchorEdge::Right,
            offset_x: 8,
            visible: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    #[serde(default = "default_true")]
    pub hardware_enabled: bool,
    #[serde(default = "default_true")]
    pub wireless_enabled: bool,
    #[serde(deserialize_with = "deserialize_enum_or_default")]
    pub layout_mode: LayoutMode,
    #[serde(deserialize_with = "deserialize_enum_or_default")]
    pub combined_order: CombinedOrder,
    pub combined_zone: ZoneLayout,
    pub hardware_zone: ZoneLayout,
    pub devices_zone: ZoneLayout,
    #[serde(deserialize_with = "deserialize_hardware_items")]
    pub hardware_items: Vec<HwMetric>,
    pub low_battery_threshold: u8,
    pub hide_on_fullscreen: bool,
    #[serde(default = "default_true")]
    pub hide_unknown_battery_devices: bool,
    pub autostart: bool,
    pub hid_poll_interval_secs: u64,
    #[serde(default = "default_hw_poll_interval_ms")]
    pub hw_poll_interval_ms: u64,
    #[serde(default = "default_true")]
    pub enable_external_sensors: bool,
    #[serde(default)]
    pub hide_unavailable_metrics: bool,
    pub device_aliases: HashMap<String, String>,
    pub hidden_devices: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_hw_poll_interval_ms() -> u64 {
    1000
}

fn default_hardware_items() -> Vec<HwMetric> {
    // Default 2x2 ultra-compact grid (column-major, 2 rows per column):
    // Col 0: NET UP / NET DOWN
    // Col 1: CPU% / MEM%
    vec![
        HwMetric::NetUp,
        HwMetric::NetDown,
        HwMetric::CpuUsage,
        HwMetric::MemUsage,
    ]
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hardware_enabled: true,
            wireless_enabled: true,
            layout_mode: LayoutMode::Combined,
            combined_order: CombinedOrder::DevicesFirst,
            combined_zone: ZoneLayout {
                anchor: AnchorEdge::Right,
                offset_x: 8,
                visible: true,
            },
            hardware_zone: ZoneLayout {
                anchor: AnchorEdge::Left,
                offset_x: 12,
                visible: true,
            },
            devices_zone: ZoneLayout {
                anchor: AnchorEdge::Right,
                offset_x: 8,
                visible: true,
            },
            hardware_items: default_hardware_items(),
            low_battery_threshold: 20,
            hide_on_fullscreen: true,
            hide_unknown_battery_devices: true,
            autostart: false,
            hid_poll_interval_secs: 45,
            hw_poll_interval_ms: default_hw_poll_interval_ms(),
            enable_external_sensors: true,
            hide_unavailable_metrics: false,
            device_aliases: HashMap::new(),
            hidden_devices: Vec::new(),
        }
    }
}

fn deserialize_enum_or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: for<'v> Deserialize<'v> + Default,
{
    let value = toml::Value::deserialize(deserializer)?;
    match T::deserialize(value.clone()) {
        Ok(parsed) => Ok(parsed),
        Err(err) => {
            eprintln!(
                "AirMonitor: ignoring unknown/invalid config value ({value}): {err}; using default"
            );
            Ok(T::default())
        }
    }
}

fn deserialize_hardware_items<'de, D>(deserializer: D) -> Result<Vec<HwMetric>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = toml::Value::deserialize(deserializer)?;
    match value {
        toml::Value::Array(arr) => {
            let mut items = Vec::with_capacity(arr.len());
            for elem in arr {
                match HwMetric::deserialize(elem.clone()) {
                    Ok(metric) => items.push(metric),
                    Err(err) => {
                        eprintln!(
                            "AirMonitor: ignoring unknown/invalid hardware_item ({elem}): {err}"
                        );
                    }
                }
            }
            Ok(items)
        }
        other => {
            eprintln!("AirMonitor: invalid hardware_items ({other}); using defaults");
            Ok(default_hardware_items())
        }
    }
}

fn to_wide_null(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn last_good_backup_path_for(path: &Path) -> PathBuf {
    let mut os: OsString = path.as_os_str().to_os_string();
    os.push(".last_good.bak");
    PathBuf::from(os)
}

fn corrupt_backup_path_for(path: &Path) -> PathBuf {
    let mut os: OsString = path.as_os_str().to_os_string();
    os.push(".corrupt.bak");
    PathBuf::from(os)
}

fn atomic_write_bytes(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create config directory: {e}"))?;
        }
    }
    let tmp_path = temp_path_for(path);

    let write_res = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|e| format!("failed to create temp config {}: {e}", tmp_path.display()))?;
        file.write_all(data)
            .map_err(|e| format!("failed to write temp config: {e}"))?;
        file.flush()
            .map_err(|e| format!("failed to flush temp config: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("failed to sync temp config: {e}"))?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    let tmp_w = to_wide_null(&tmp_path);
    let dst_w = to_wide_null(path);
    let replace_res = unsafe {
        MoveFileExW(
            PCWSTR(tmp_w.as_ptr()),
            PCWSTR(dst_w.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if let Err(err) = replace_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("atomic config replace failed: {err}"));
    }
    Ok(())
}

fn parse_config_str(text: &str) -> Result<AppConfig, toml::de::Error> {
    let mut cfg = toml::from_str::<AppConfig>(text)?;
    if let Ok(toml::Value::Table(table)) = toml::from_str::<toml::Value>(text) {
        let has_hw_key = table.contains_key("hardware_enabled");
        let has_wl_key = table.contains_key("wireless_enabled");
        if !has_hw_key && !has_wl_key {
            match cfg.layout_mode {
                LayoutMode::DevicesOnly => {
                    cfg.hardware_enabled = false;
                    cfg.wireless_enabled = true;
                }
                LayoutMode::HardwareOnly => {
                    cfg.hardware_enabled = true;
                    cfg.wireless_enabled = false;
                }
                LayoutMode::Combined | LayoutMode::Split => {}
            }
        }
    }
    cfg.sanitize();
    Ok(cfg)
}

fn load_valid_backup(path: &Path) -> Option<AppConfig> {
    if !path.exists() {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    parse_config_str(&text).ok()
}

fn temp_path_for(path: &Path) -> PathBuf {
    let seq = TMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");
    let tmp_name = format!("{file_name}.tmp.{pid}.{seq}");
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(tmp_name),
        _ => PathBuf::from(tmp_name),
    }
}

fn dedup_preserve_order<T: Eq + Clone>(items: &mut Vec<T>) {
    let mut seen: Vec<T> = Vec::with_capacity(items.len());
    items.retain(|item| {
        if seen.contains(item) {
            false
        } else {
            seen.push(item.clone());
            true
        }
    });
}

impl AppConfig {
    pub fn config_path() -> PathBuf {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let candidate = dir.join("config.toml");
                if candidate.exists() {
                    return candidate;
                }
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata)
                .join("AirMonitor")
                .join("config.toml");
        }
        PathBuf::from("config.toml")
    }

    /// Returns the effective taskbar layout mode derived from `hardware_enabled`
    /// and `wireless_enabled` together with the dual-zone `layout_mode` preference.
    /// Returns `None` when both monitoring zones are disabled.
    pub fn effective_layout_mode(&self) -> Option<LayoutMode> {
        match (self.hardware_enabled, self.wireless_enabled) {
            (false, false) => None,
            (true, false) => Some(LayoutMode::HardwareOnly),
            (false, true) => Some(LayoutMode::DevicesOnly),
            (true, true) => match self.layout_mode {
                LayoutMode::Split => Some(LayoutMode::Split),
                _ => Some(LayoutMode::Combined),
            },
        }
    }

    pub fn set_hardware_enabled(&mut self, enabled: bool) {
        self.hardware_enabled = enabled;
        if self.hardware_enabled
            && self.wireless_enabled
            && matches!(
                self.layout_mode,
                LayoutMode::HardwareOnly | LayoutMode::DevicesOnly
            )
        {
            self.layout_mode = LayoutMode::Combined;
        }
    }

    pub fn set_wireless_enabled(&mut self, enabled: bool) {
        self.wireless_enabled = enabled;
        if self.hardware_enabled
            && self.wireless_enabled
            && matches!(
                self.layout_mode,
                LayoutMode::HardwareOnly | LayoutMode::DevicesOnly
            )
        {
            self.layout_mode = LayoutMode::Combined;
        }
    }

    /// In `Split` mode, hardware and wireless zones must stay on opposite edges
    /// so they cannot overlap on the same side of the taskbar.
    pub fn ensure_split_anchors_distinct(&mut self) {
        if self.layout_mode == LayoutMode::Split
            && self.hardware_zone.anchor == self.devices_zone.anchor
        {
            self.devices_zone.anchor = self.hardware_zone.anchor.opposite();
        }
    }

    /// Sets the hardware zone anchor; in `Split` mode, automatically moves the
    /// wireless zone to the opposite edge if they would collide on the same side.
    pub fn set_hardware_anchor(&mut self, anchor: AnchorEdge) {
        self.hardware_zone.anchor = anchor;
        if self.layout_mode == LayoutMode::Split && self.devices_zone.anchor == anchor {
            self.devices_zone.anchor = anchor.opposite();
        }
    }

    /// Sets the wireless devices zone anchor; in `Split` mode, automatically moves
    /// the hardware zone to the opposite edge if they would collide on the same side.
    pub fn set_devices_anchor(&mut self, anchor: AnchorEdge) {
        self.devices_zone.anchor = anchor;
        if self.layout_mode == LayoutMode::Split && self.hardware_zone.anchor == anchor {
            self.hardware_zone.anchor = anchor.opposite();
        }
    }

    /// Clamps numeric settings to safe bounds and removes duplicates/invalid entries.
    pub fn sanitize(&mut self) {
        // Fullscreen auto-hide is always active at runtime; keep field normalized.
        self.hide_on_fullscreen = true;
        self.ensure_split_anchors_distinct();
        self.combined_zone.offset_x = self.combined_zone.offset_x.clamp(0, MAX_OFFSET_X);
        self.hardware_zone.offset_x = self.hardware_zone.offset_x.clamp(0, MAX_OFFSET_X);
        self.devices_zone.offset_x = self.devices_zone.offset_x.clamp(0, MAX_OFFSET_X);

        self.low_battery_threshold = self
            .low_battery_threshold
            .clamp(MIN_LOW_BATTERY_THRESHOLD, MAX_LOW_BATTERY_THRESHOLD);
        self.hid_poll_interval_secs = self
            .hid_poll_interval_secs
            .clamp(MIN_HID_POLL_SECS, MAX_HID_POLL_SECS);
        self.hw_poll_interval_ms = self
            .hw_poll_interval_ms
            .clamp(MIN_HW_POLL_INTERVAL_MS, MAX_HW_POLL_INTERVAL_MS);

        dedup_preserve_order(&mut self.hardware_items);

        for id in &mut self.hidden_devices {
            let trimmed = id.trim();
            if trimmed.len() != id.len() {
                *id = trimmed.to_string();
            }
        }
        self.hidden_devices.retain(|id| !id.is_empty());
        dedup_preserve_order(&mut self.hidden_devices);

        let cleaned_aliases: HashMap<String, String> = self
            .device_aliases
            .drain()
            .filter_map(|(k, v)| {
                let k_trimmed = k.trim();
                let v_trimmed = v.trim();
                if k_trimmed.is_empty() || v_trimmed.is_empty() {
                    None
                } else {
                    Some((k_trimmed.to_string(), v_trimmed.to_string()))
                }
            })
            .collect();
        self.device_aliases = cleaned_aliases;
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        Self::load_from_path(&path, !is_read_only_mode())
    }

    fn load_from_path(path: &Path, allow_write: bool) -> Self {
        if !path.exists() {
            let mut cfg = Self::default();
            cfg.sanitize();
            if allow_write {
                if let Err(err) = cfg.save_to_path(path) {
                    eprintln!(
                        "AirMonitor: failed to initialize default config {}: {}",
                        path.display(),
                        err
                    );
                }
            }
            return cfg;
        }
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(err) => {
                eprintln!(
                    "AirMonitor: failed to read config {}: {}",
                    path.display(),
                    err
                );
                if allow_write {
                    let corrupt_path = corrupt_backup_path_for(path);
                    let _ = fs::copy(path, &corrupt_path);
                }
                let last_good_path = last_good_backup_path_for(path);
                if let Some(cfg) = load_valid_backup(&last_good_path) {
                    eprintln!(
                        "AirMonitor: recovered config from last good backup {}",
                        last_good_path.display()
                    );
                    return cfg;
                }
                return Self::default();
            }
        };
        match parse_config_str(&text) {
            Ok(cfg) => cfg,
            Err(err) => {
                eprintln!(
                    "AirMonitor: failed to parse config {}: {}",
                    path.display(),
                    err
                );
                if allow_write {
                    let corrupt_path = corrupt_backup_path_for(path);
                    if let Err(copy_err) = fs::copy(path, &corrupt_path) {
                        eprintln!(
                            "AirMonitor: failed to backup corrupt config to {}: {}",
                            corrupt_path.display(),
                            copy_err
                        );
                    } else {
                        eprintln!(
                            "AirMonitor: backed up corrupt config to {} without overwriting original",
                            corrupt_path.display()
                        );
                    }
                }
                let last_good_path = last_good_backup_path_for(path);
                if let Some(cfg) = load_valid_backup(&last_good_path) {
                    eprintln!(
                        "AirMonitor: recovered config from last good backup {}",
                        last_good_path.display()
                    );
                    return cfg;
                }
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        if is_read_only_mode() {
            return Ok(());
        }
        let path = Self::config_path();
        self.save_to_path(&path)
    }

    fn save_to_path(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        if path.exists() {
            if let Ok(prev_text) = fs::read_to_string(path) {
                if parse_config_str(&prev_text).is_ok() {
                    let last_good_path = last_good_backup_path_for(path);
                    if let Err(err) = atomic_write_bytes(&last_good_path, prev_text.as_bytes()) {
                        eprintln!(
                            "AirMonitor: failed to update last good config backup {}: {}",
                            last_good_path.display(),
                            err
                        );
                    }
                }
            }
        }
        atomic_write_bytes(path, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(name: &str) -> Self {
            let seq = TMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "airmonitor_cfg_test_{}_{}_{}",
                name,
                std::process::id(),
                seq
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create temp test dir");
            Self { path: dir }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn default_config_round_trip() {
        let dir = TempDirGuard::new("round_trip");
        let path = dir.path.join("config.toml");

        let mut cfg = AppConfig::default();
        cfg.device_aliases
            .insert("hid:1234".to_string(), "My Mouse".to_string());
        cfg.hidden_devices.push("bt:abcd".to_string());
        cfg.save_to_path(&path).expect("save default config");

        let loaded = AppConfig::load_from_path(&path, true);
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let dir = TempDirGuard::new("missing_fields");
        let path = dir.path.join("config.toml");

        let partial = r#"
low_battery_threshold = 15

[combined_zone]
offset_x = 24

[device_aliases]
"bt:1122" = "Headset"
"#;
        fs::write(&path, partial).expect("write partial config");

        let loaded = AppConfig::load_from_path(&path, true);
        let defaults = AppConfig::default();
        assert_eq!(loaded.low_battery_threshold, 15);
        assert_eq!(loaded.combined_zone.offset_x, 24);
        assert_eq!(loaded.combined_zone.anchor, defaults.combined_zone.anchor);
        assert_eq!(loaded.combined_zone.visible, defaults.combined_zone.visible);
        assert_eq!(loaded.layout_mode, defaults.layout_mode);
        assert_eq!(loaded.hardware_items, defaults.hardware_items);
        assert_eq!(
            loaded.hid_poll_interval_secs,
            defaults.hid_poll_interval_secs
        );
        assert_eq!(
            loaded.device_aliases.get("bt:1122").map(String::as_str),
            Some("Headset")
        );
    }

    #[test]
    fn unknown_fields_and_unknown_enums_are_tolerated() {
        let dir = TempDirGuard::new("unknown_fields");
        let path = dir.path.join("config.toml");

        let future_toml = r#"
layout_mode = "future_floating_mode"
combined_order = "future_order"
unknown_top_level_key = "ignored"
hardware_items = ["net_up", "future_npu_usage", "cpu_usage"]
hidden_devices = ["bt:keep"]

[combined_zone]
anchor = "top_center"
offset_x = 16
future_zone_prop = true

[future_section]
foo = 42

[device_aliases]
"hid:keep" = "Keyboard"
"#;
        fs::write(&path, future_toml).expect("write future config");

        let loaded = AppConfig::load_from_path(&path, true);
        assert_eq!(loaded.layout_mode, LayoutMode::Combined);
        assert_eq!(loaded.combined_order, CombinedOrder::DevicesFirst);
        assert_eq!(loaded.combined_zone.anchor, AnchorEdge::Right);
        assert_eq!(loaded.combined_zone.offset_x, 16);
        assert_eq!(
            loaded.hardware_items,
            vec![HwMetric::NetUp, HwMetric::CpuUsage]
        );
        assert_eq!(loaded.hidden_devices, vec!["bt:keep".to_string()]);
        assert_eq!(
            loaded.device_aliases.get("hid:keep").map(String::as_str),
            Some("Keyboard")
        );
        assert!(!dir.path.join("config.toml.corrupt.bak").exists());
        assert!(!dir.path.join("config.toml.last_good.bak").exists());
    }

    #[test]
    fn parse_failure_creates_corrupt_bak_without_overwriting_original() {
        let dir = TempDirGuard::new("corrupt_bak");
        let path = dir.path.join("config.toml");
        let corrupt_bak_path = dir.path.join("config.toml.corrupt.bak");

        let corrupt = "layout_mode = [unclosed\n[device_aliases]\n\"k\" = \"v\"\n";
        fs::write(&path, corrupt).expect("write corrupt config");

        let loaded = AppConfig::load_from_path(&path, true);
        assert_eq!(loaded, AppConfig::default());

        assert!(
            corrupt_bak_path.exists(),
            "backup .corrupt.bak file must be created"
        );
        assert_eq!(
            fs::read_to_string(&corrupt_bak_path).expect("read .corrupt.bak"),
            corrupt,
            ".corrupt.bak must contain the exact original corrupt content"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("read original"),
            corrupt,
            "original config.toml must NOT be overwritten on parse failure"
        );
    }

    #[test]
    fn atomic_save_overwrites_existing_and_cleans_temp_files() {
        let dir = TempDirGuard::new("atomic_save");
        let path = dir.path.join("config.toml");

        let cfg1 = AppConfig {
            low_battery_threshold: 15,
            ..AppConfig::default()
        };
        cfg1.save_to_path(&path).expect("initial save");

        let mut cfg2 = cfg1.clone();
        cfg2.low_battery_threshold = 35;
        cfg2.autostart = true;
        cfg2.save_to_path(&path).expect("overwrite save");

        let loaded = AppConfig::load_from_path(&path, true);
        assert_eq!(loaded.low_battery_threshold, 35);
        assert!(loaded.autostart);

        let leftover_tmps: Vec<_> = fs::read_dir(&dir.path)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftover_tmps.is_empty(),
            "no temp files should remain after atomic save: {leftover_tmps:?}"
        );
    }

    #[test]
    fn sanitize_clamps_values_and_deduplicates_lists() {
        let mut cfg = AppConfig {
            combined_zone: ZoneLayout {
                anchor: AnchorEdge::Right,
                offset_x: -50,
                visible: true,
            },
            hardware_zone: ZoneLayout {
                anchor: AnchorEdge::Left,
                offset_x: 99_999,
                visible: true,
            },
            devices_zone: ZoneLayout {
                anchor: AnchorEdge::Right,
                offset_x: -1,
                visible: false,
            },
            hardware_items: vec![
                HwMetric::NetUp,
                HwMetric::CpuUsage,
                HwMetric::NetUp,
                HwMetric::MemUsage,
                HwMetric::CpuUsage,
            ],
            low_battery_threshold: 1,
            hid_poll_interval_secs: 2,
            hidden_devices: vec![
                "  ".to_string(),
                "bt:1".to_string(),
                " bt:1 ".to_string(),
                "hid:2".to_string(),
                "".to_string(),
            ],
            device_aliases: HashMap::from([
                ("".to_string(), "EmptyKey".to_string()),
                ("hid:valid".to_string(), "   ".to_string()),
                (" bt:ok ".to_string(), " My Earbuds ".to_string()),
            ]),
            ..AppConfig::default()
        };

        cfg.sanitize();

        assert_eq!(cfg.combined_zone.offset_x, 0);
        assert_eq!(cfg.hardware_zone.offset_x, MAX_OFFSET_X);
        assert_eq!(cfg.devices_zone.offset_x, 0);
        assert_eq!(cfg.low_battery_threshold, MIN_LOW_BATTERY_THRESHOLD);
        assert_eq!(cfg.hid_poll_interval_secs, MIN_HID_POLL_SECS);
        assert_eq!(
            cfg.hardware_items,
            vec![HwMetric::NetUp, HwMetric::CpuUsage, HwMetric::MemUsage]
        );
        assert_eq!(
            cfg.hidden_devices,
            vec!["bt:1".to_string(), "hid:2".to_string()]
        );
        assert_eq!(cfg.device_aliases.len(), 1);
        assert_eq!(
            cfg.device_aliases.get("bt:ok").map(String::as_str),
            Some("My Earbuds")
        );

        // Upper-bound clamping
        cfg.low_battery_threshold = 99;
        cfg.hid_poll_interval_secs = 99_999;
        cfg.sanitize();
        assert_eq!(cfg.low_battery_threshold, MAX_LOW_BATTERY_THRESHOLD);
        assert_eq!(cfg.hid_poll_interval_secs, MAX_HID_POLL_SECS);
    }

    #[test]
    fn hardware_only_and_devices_only_round_trip_and_compat() {
        let dir = TempDirGuard::new("layout_modes_and_flags");
        let path = dir.path.join("config.toml");

        for mode in [LayoutMode::HardwareOnly, LayoutMode::DevicesOnly] {
            let cfg = AppConfig {
                layout_mode: mode,
                hide_unknown_battery_devices: false,
                ..AppConfig::default()
            };
            cfg.save_to_path(&path).expect("save config");
            let loaded = AppConfig::load_from_path(&path, true);
            assert_eq!(loaded.layout_mode, mode);
            assert!(!loaded.hide_unknown_battery_devices);
        }

        // Legacy config missing hide_unknown_battery_devices & with hide_on_fullscreen = false
        let legacy = r#"
layout_mode = "devices_only"
hide_on_fullscreen = false
"#;
        fs::write(&path, legacy).expect("write legacy config");
        let loaded = AppConfig::load_from_path(&path, true);
        assert_eq!(loaded.layout_mode, LayoutMode::DevicesOnly);
        assert!(loaded.hide_unknown_battery_devices);
        assert!(loaded.hide_on_fullscreen);
    }

    #[test]
    fn hw_poll_and_sensor_flags_defaults_compat_and_clamping() {
        let dir = TempDirGuard::new("hw_poll_and_sensors");
        let path = dir.path.join("config.toml");

        let defaults = AppConfig::default();
        assert_eq!(defaults.hw_poll_interval_ms, 1000);
        assert!(defaults.enable_external_sensors);
        assert!(!defaults.hide_unavailable_metrics);
        assert_eq!(
            defaults.hardware_items,
            vec![
                HwMetric::NetUp,
                HwMetric::NetDown,
                HwMetric::CpuUsage,
                HwMetric::MemUsage,
            ]
        );

        // Legacy config missing all three new fields
        let legacy = r#"
layout_mode = "combined"
low_battery_threshold = 25
hardware_items = ["cpu_temp", "cpu_power", "cpu_fan", "disk_read", "disk_write", "disk_temp", "sys_battery"]
"#;
        fs::write(&path, legacy).expect("write legacy config");
        let loaded = AppConfig::load_from_path(&path, true);
        assert_eq!(loaded.hw_poll_interval_ms, 1000);
        assert!(loaded.enable_external_sensors);
        assert!(!loaded.hide_unavailable_metrics);
        assert_eq!(
            loaded.hardware_items,
            vec![
                HwMetric::CpuTemp,
                HwMetric::CpuPower,
                HwMetric::CpuFan,
                HwMetric::DiskRead,
                HwMetric::DiskWrite,
                HwMetric::DiskTemp,
                HwMetric::SysBattery,
            ]
        );

        // Boundary clamping for hw_poll_interval_ms
        let mut cfg = AppConfig {
            hw_poll_interval_ms: 100,
            enable_external_sensors: false,
            hide_unavailable_metrics: true,
            ..AppConfig::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.hw_poll_interval_ms, MIN_HW_POLL_INTERVAL_MS);

        cfg.hw_poll_interval_ms = 20_000;
        cfg.sanitize();
        assert_eq!(cfg.hw_poll_interval_ms, MAX_HW_POLL_INTERVAL_MS);

        cfg.save_to_path(&path).expect("save modified config");
        let round_trip = AppConfig::load_from_path(&path, true);
        assert_eq!(round_trip.hw_poll_interval_ms, MAX_HW_POLL_INTERVAL_MS);
        assert!(!round_trip.enable_external_sensors);
        assert!(round_trip.hide_unavailable_metrics);
    }

    #[test]
    fn hw_metric_all_and_display_names_cover_all_variants() {
        assert_eq!(HwMetric::ALL.len(), 18);
        assert_eq!(HwMetric::CpuTemp.display_name(), "CPU 温度");
        assert_eq!(HwMetric::CpuPower.display_name(), "CPU 功耗");
        assert_eq!(HwMetric::CpuFan.display_name(), "CPU 风扇");
        assert_eq!(HwMetric::DiskRead.display_name(), "磁盘读取");
        assert_eq!(HwMetric::DiskWrite.display_name(), "磁盘写入");
        assert_eq!(HwMetric::DiskTemp.display_name(), "硬盘温度");
        assert_eq!(HwMetric::SysBattery.display_name(), "本机电池");
    }

    #[test]
    fn split_mode_anchors_stay_on_opposite_edges() {
        let mut cfg = AppConfig {
            layout_mode: LayoutMode::Split,
            hardware_zone: ZoneLayout {
                anchor: AnchorEdge::Right,
                offset_x: 12,
                visible: true,
            },
            devices_zone: ZoneLayout {
                anchor: AnchorEdge::Right,
                offset_x: 8,
                visible: true,
            },
            ..AppConfig::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.hardware_zone.anchor, AnchorEdge::Right);
        assert_eq!(cfg.devices_zone.anchor, AnchorEdge::Left);

        // Moving devices to Right swaps hardware to Left.
        cfg.set_devices_anchor(AnchorEdge::Right);
        assert_eq!(cfg.devices_zone.anchor, AnchorEdge::Right);
        assert_eq!(cfg.hardware_zone.anchor, AnchorEdge::Left);

        // Moving hardware to Right swaps devices to Left.
        cfg.set_hardware_anchor(AnchorEdge::Right);
        assert_eq!(cfg.hardware_zone.anchor, AnchorEdge::Right);
        assert_eq!(cfg.devices_zone.anchor, AnchorEdge::Left);

        // Single-zone modes allow independent anchor changes without touching the other zone.
        cfg.layout_mode = LayoutMode::HardwareOnly;
        cfg.set_hardware_anchor(AnchorEdge::Left);
        assert_eq!(cfg.hardware_zone.anchor, AnchorEdge::Left);
        assert_eq!(cfg.devices_zone.anchor, AnchorEdge::Left);

        // Entering Split mode separates colliding anchors immediately.
        cfg.layout_mode = LayoutMode::Split;
        cfg.ensure_split_anchors_distinct();
        assert_eq!(cfg.hardware_zone.anchor, AnchorEdge::Left);
        assert_eq!(cfg.devices_zone.anchor, AnchorEdge::Right);
    }

    #[test]
    fn zone_enable_flags_migration_and_effective_layout_mode() {
        let dir = TempDirGuard::new("zone_enable_migration");
        let path = dir.path.join("config.toml");

        // Legacy devices_only without explicit flags migrates to hardware=false, wireless=true.
        fs::write(&path, "layout_mode = \"devices_only\"\n").expect("write legacy devices_only");
        let loaded_dev = AppConfig::load_from_path(&path, true);
        assert!(!loaded_dev.hardware_enabled);
        assert!(loaded_dev.wireless_enabled);
        assert_eq!(
            loaded_dev.effective_layout_mode(),
            Some(LayoutMode::DevicesOnly)
        );

        // Legacy hardware_only without explicit flags migrates to hardware=true, wireless=false.
        fs::write(&path, "layout_mode = \"hardware_only\"\n").expect("write legacy hardware_only");
        let loaded_hw = AppConfig::load_from_path(&path, true);
        assert!(loaded_hw.hardware_enabled);
        assert!(!loaded_hw.wireless_enabled);
        assert_eq!(
            loaded_hw.effective_layout_mode(),
            Some(LayoutMode::HardwareOnly)
        );

        // Effective layout mode for all toggle combinations + Split preservation.
        let mut cfg = AppConfig {
            layout_mode: LayoutMode::Split,
            ..AppConfig::default()
        };
        assert_eq!(cfg.effective_layout_mode(), Some(LayoutMode::Split));
        cfg.set_hardware_enabled(false);
        assert_eq!(cfg.effective_layout_mode(), Some(LayoutMode::DevicesOnly));
        cfg.set_wireless_enabled(false);
        assert_eq!(cfg.effective_layout_mode(), None);
        cfg.set_hardware_enabled(true);
        assert_eq!(cfg.effective_layout_mode(), Some(LayoutMode::HardwareOnly));
        cfg.set_wireless_enabled(true);
        assert_eq!(cfg.effective_layout_mode(), Some(LayoutMode::Split));

        // Legacy HardwareOnly upgraded to dual-enabled normalizes to Combined.
        let mut legacy_up = loaded_hw;
        legacy_up.set_wireless_enabled(true);
        assert_eq!(legacy_up.layout_mode, LayoutMode::Combined);
        assert_eq!(
            legacy_up.effective_layout_mode(),
            Some(LayoutMode::Combined)
        );
    }

    #[test]
    fn initial_config_creation_and_last_good_backup_recovery() {
        let dir = TempDirGuard::new("last_good_recovery");
        let path = dir.path.join("config.toml");
        let last_good = last_good_backup_path_for(&path);
        let corrupt_bak = corrupt_backup_path_for(&path);

        // Read-only mode must NOT create config.toml when missing.
        let ro_cfg = AppConfig::load_from_path(&path, false);
        assert_eq!(ro_cfg, AppConfig::default());
        assert!(!path.exists());

        // Normal mode creates default config.toml on first launch.
        let init_cfg = AppConfig::load_from_path(&path, true);
        assert_eq!(init_cfg, AppConfig::default());
        assert!(path.exists());
        assert!(!last_good.exists());

        // Saving updated config maintains .last_good.bak with previous valid config.
        let v1 = AppConfig {
            low_battery_threshold: 17,
            ..AppConfig::default()
        };
        v1.save_to_path(&path).expect("save v1");
        assert!(last_good.exists());

        let v2 = AppConfig {
            low_battery_threshold: 29,
            ..AppConfig::default()
        };
        v2.save_to_path(&path).expect("save v2");
        let prev_good = load_valid_backup(&last_good).expect("valid last_good.bak");
        assert_eq!(prev_good.low_battery_threshold, 17);

        // Corrupting config.toml backs up corrupt original and recovers from .last_good.bak.
        let corrupt = "layout_mode = [broken\n";
        fs::write(&path, corrupt).expect("write corrupt");
        let recovered = AppConfig::load_from_path(&path, true);
        assert_eq!(recovered.low_battery_threshold, 17);
        assert_eq!(fs::read_to_string(&corrupt_bak).unwrap(), corrupt);
        assert_eq!(fs::read_to_string(&path).unwrap(), corrupt);
    }
}
