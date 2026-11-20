use std::time::Instant;

use crate::config::AppConfig;

#[derive(Debug, Clone, Default)]
pub struct HardwareMetrics {
    pub cpu_usage_pct: f32,
    pub cpu_freq_ghz: Option<f32>,
    pub cpu_temp_c: Option<f32>,
    pub cpu_power_w: Option<f32>,
    pub cpu_fan_rpm: Option<u32>,
    pub mem_usage_pct: f32,
    pub mem_used_gb: f32,
    pub mem_total_gb: f32,
    pub net_up_bps: f64,
    pub net_down_bps: f64,
    pub net_adapter_name: String,
    pub disk_read_bps: Option<f64>,
    pub disk_write_bps: Option<f64>,
    pub disk_temp_c: Option<f32>,
    pub sys_battery_pct: Option<u8>,
    pub sys_battery_charging: bool,
    pub gpu_name: String,
    pub gpu_usage_pct: Option<f32>,
    pub gpu_temp_c: Option<f32>,
    pub gpu_hotspot_c: Option<f32>,
    pub gpu_power_w: Option<f32>,
    pub gpu_fan_rpm: Option<u32>,
    pub gpu_fan_pct: Option<u32>,
    pub gpu_vram_used_gb: Option<f32>,
    pub gpu_vram_total_gb: Option<f32>,
    pub gpu_provider: String,
    pub sensor_provider: String,
}

impl HardwareMetrics {
    pub fn is_metric_available(&self, metric: crate::config::HwMetric) -> bool {
        use crate::config::HwMetric;
        match metric {
            HwMetric::CpuUsage | HwMetric::MemUsage | HwMetric::NetUp | HwMetric::NetDown => true,
            HwMetric::CpuFreq => self.cpu_freq_ghz.is_some(),
            HwMetric::CpuTemp => self.cpu_temp_c.is_some(),
            HwMetric::CpuPower => self.cpu_power_w.is_some(),
            HwMetric::CpuFan => self.cpu_fan_rpm.is_some(),
            HwMetric::DiskRead => self.disk_read_bps.is_some(),
            HwMetric::DiskWrite => self.disk_write_bps.is_some(),
            HwMetric::DiskTemp => self.disk_temp_c.is_some(),
            HwMetric::SysBattery => self.sys_battery_pct.is_some(),
            HwMetric::GpuUsage => self.gpu_usage_pct.is_some(),
            HwMetric::GpuTemp => self.gpu_temp_c.is_some(),
            HwMetric::GpuHotspot => self.gpu_hotspot_c.is_some(),
            HwMetric::GpuPower => self.gpu_power_w.is_some(),
            HwMetric::GpuVram => self.gpu_vram_used_gb.is_some(),
            HwMetric::GpuFan => self.gpu_fan_rpm.is_some() || self.gpu_fan_pct.is_some(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Mouse,
    Keyboard,
    Headset,
    Earbuds,
    Gamepad,
    Other,
}

impl DeviceKind {
    pub fn short_label(&self) -> &'static str {
        match self {
            DeviceKind::Mouse => "鼠",
            DeviceKind::Keyboard => "键",
            DeviceKind::Headset => "耳",
            DeviceKind::Earbuds => "芽",
            DeviceKind::Gamepad => "柄",
            DeviceKind::Other => "设",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Wireless24G,
    Bluetooth,
    BleBroadcast,
    XboxWireless,
}

impl Transport {
    pub fn badge(&self) -> &'static str {
        match self {
            Transport::Wireless24G => "2.4G",
            Transport::Bluetooth => "BT",
            Transport::BleBroadcast => "BLE",
            Transport::XboxWireless => "Xbox",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SubBatteries {
    pub left_pct: Option<u8>,
    pub left_charging: bool,
    pub right_pct: Option<u8>,
    pub right_charging: bool,
    pub case_pct: Option<u8>,
    pub case_charging: bool,
}

#[derive(Debug, Clone)]
pub struct WirelessDevice {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub transport: Transport,
    pub battery_pct: Option<u8>,
    pub charging: bool,
    pub sub_batteries: Option<SubBatteries>,
    pub online: bool,
    pub last_seen: Instant,
    pub detail_note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MonitorSnapshot {
    pub hw: HardwareMetrics,
    pub devices: Vec<WirelessDevice>,
    pub config: AppConfig,
}

impl MonitorSnapshot {
    pub fn new(config: AppConfig) -> Self {
        Self {
            hw: HardwareMetrics::default(),
            devices: Vec::new(),
            config,
        }
    }
}

const KB: f64 = 1024.0;
const MB: f64 = 1024.0 * 1024.0;
const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Format network speed into a fixed 5-character right-aligned string (B/K/M/G)
/// to prevent horizontal jitter in the taskbar overlay.
pub fn format_speed(bps: f64) -> String {
    let bps = if bps.is_finite() { bps.max(0.0) } else { 0.0 };
    if bps < KB {
        format!("{:>4.0}B", bps)
    } else if bps < MB {
        let kb = bps / KB;
        if kb < 99.95 {
            format!("{:>4.1}K", kb)
        } else {
            format!("{:>4.0}K", kb)
        }
    } else if bps < GB {
        let mb = bps / MB;
        if mb < 99.95 {
            format!("{:>4.1}M", mb)
        } else {
            format!("{:>4.0}M", mb)
        }
    } else {
        let gb = bps / GB;
        if gb < 9.995 {
            format!("{:>4.2}G", gb)
        } else {
            format!("{:>4.1}G", gb)
        }
    }
}

/// Format network speed with full human-readable units for detail popups.
pub fn format_speed_full(bps: f64) -> String {
    let bps = if bps.is_finite() { bps.max(0.0) } else { 0.0 };
    if bps < KB {
        format!("{:.0} B/s", bps)
    } else if bps < MB {
        format!("{:.1} KB/s", bps / KB)
    } else if bps < GB {
        format!("{:.2} MB/s", bps / MB)
    } else {
        format!("{:.2} GB/s", bps / GB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_speed_zero_and_bytes() {
        assert_eq!(format_speed(0.0), "   0B");
        assert_eq!(format_speed(-100.0), "   0B");
        assert_eq!(format_speed(f64::NAN), "   0B");
        assert_eq!(format_speed(1.0), "   1B");
        assert_eq!(format_speed(512.0), " 512B");
        assert_eq!(format_speed(1023.0), "1023B");
    }

    #[test]
    fn format_speed_kilobytes_and_boundary() {
        assert_eq!(format_speed(1024.0), " 1.0K");
        assert_eq!(format_speed(9.9 * 1024.0), " 9.9K");
        assert_eq!(format_speed(99.9 * 1024.0), "99.9K");
        assert_eq!(format_speed(99.96 * 1024.0), " 100K");
        assert_eq!(format_speed(100.0 * 1024.0), " 100K");
        assert_eq!(format_speed(1023.0 * 1024.0), "1023K");
    }

    #[test]
    fn format_speed_megabytes_and_boundary() {
        assert_eq!(format_speed(1024.0 * 1024.0), " 1.0M");
        assert_eq!(format_speed(12.34 * 1024.0 * 1024.0), "12.3M");
        assert_eq!(format_speed(99.9 * 1024.0 * 1024.0), "99.9M");
        assert_eq!(format_speed(99.96 * 1024.0 * 1024.0), " 100M");
        assert_eq!(format_speed(100.0 * 1024.0 * 1024.0), " 100M");
        assert_eq!(format_speed(1023.0 * 1024.0 * 1024.0), "1023M");
    }

    #[test]
    fn format_speed_gigabytes_and_boundary() {
        let gb = 1024.0 * 1024.0 * 1024.0;
        assert_eq!(format_speed(gb), "1.00G");
        assert_eq!(format_speed(2.34 * gb), "2.34G");
        assert_eq!(format_speed(9.99 * gb), "9.99G");
        assert_eq!(format_speed(9.996 * gb), "10.0G");
        assert_eq!(format_speed(10.0 * gb), "10.0G");
        assert_eq!(format_speed(25.4 * gb), "25.4G");
    }

    #[test]
    fn format_speed_fixed_five_char_width() {
        let samples = [
            0.0,
            5.0,
            512.0,
            1023.4,
            1024.0,
            15_360.0,
            102_350.0,
            512_000.0,
            1_048_575.0,
            1_048_576.0,
            104_800_000.0,
            1_073_741_823.0,
            1_073_741_824.0,
            10_730_000_000.0,
            50_000_000_000.0,
        ];
        for s in samples {
            let formatted = format_speed(s);
            assert_eq!(
                formatted.len(),
                5,
                "unexpected width for {s}: {formatted:?}"
            );
        }
    }

    #[test]
    fn format_speed_full_units() {
        assert_eq!(format_speed_full(0.0), "0 B/s");
        assert_eq!(format_speed_full(-5.0), "0 B/s");
        assert_eq!(format_speed_full(f64::NAN), "0 B/s");
        assert_eq!(format_speed_full(f64::INFINITY), "0 B/s");
        assert_eq!(format_speed_full(f64::NEG_INFINITY), "0 B/s");
        assert_eq!(format_speed_full(512.0), "512 B/s");
        assert_eq!(format_speed_full(1023.0), "1023 B/s");
        assert_eq!(format_speed_full(1024.0), "1.0 KB/s");
        assert_eq!(format_speed_full(1536.0), "1.5 KB/s");
        assert_eq!(format_speed_full(1024.0 * 1024.0), "1.00 MB/s");
        assert_eq!(format_speed_full(2.5 * 1024.0 * 1024.0), "2.50 MB/s");
        assert_eq!(
            format_speed_full(1.25 * 1024.0 * 1024.0 * 1024.0),
            "1.25 GB/s"
        );
    }

    #[test]
    fn format_speed_handles_non_finite_inputs() {
        assert_eq!(format_speed(f64::INFINITY), "   0B");
        assert_eq!(format_speed(f64::NEG_INFINITY), "   0B");
        assert_eq!(format_speed(f64::NAN), "   0B");
    }

    #[test]
    fn device_kind_and_transport_labels_and_snapshot_defaults() {
        assert_eq!(DeviceKind::Mouse.short_label(), "鼠");
        assert_eq!(DeviceKind::Keyboard.short_label(), "键");
        assert_eq!(DeviceKind::Headset.short_label(), "耳");
        assert_eq!(DeviceKind::Earbuds.short_label(), "芽");
        assert_eq!(DeviceKind::Gamepad.short_label(), "柄");
        assert_eq!(DeviceKind::Other.short_label(), "设");

        assert_eq!(Transport::Wireless24G.badge(), "2.4G");
        assert_eq!(Transport::Bluetooth.badge(), "BT");
        assert_eq!(Transport::BleBroadcast.badge(), "BLE");
        assert_eq!(Transport::XboxWireless.badge(), "Xbox");

        let sub = SubBatteries::default();
        assert_eq!(sub.left_pct, None);
        assert_eq!(sub.right_pct, None);
        assert_eq!(sub.case_pct, None);
        assert!(!sub.left_charging && !sub.right_charging && !sub.case_charging);

        let snap = MonitorSnapshot::new(AppConfig::default());
        assert!(snap.devices.is_empty());
        assert_eq!(snap.hw.cpu_usage_pct, 0.0);
    }

    #[test]
    fn hardware_metrics_is_metric_available_checks_required_and_optional_fields() {
        use crate::config::HwMetric;

        let mut hw = HardwareMetrics::default();
        assert!(hw.is_metric_available(HwMetric::CpuUsage));
        assert!(hw.is_metric_available(HwMetric::MemUsage));
        assert!(hw.is_metric_available(HwMetric::NetUp));
        assert!(hw.is_metric_available(HwMetric::NetDown));

        for metric in [
            HwMetric::CpuFreq,
            HwMetric::CpuTemp,
            HwMetric::CpuPower,
            HwMetric::CpuFan,
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
        ] {
            assert!(
                !hw.is_metric_available(metric),
                "expected {:?} to be unavailable by default",
                metric
            );
        }

        hw.cpu_temp_c = Some(52.0);
        hw.disk_read_bps = Some(1024.0);
        hw.sys_battery_pct = Some(88);
        hw.gpu_fan_pct = Some(40);
        assert!(hw.is_metric_available(HwMetric::CpuTemp));
        assert!(hw.is_metric_available(HwMetric::DiskRead));
        assert!(hw.is_metric_available(HwMetric::SysBattery));
        assert!(hw.is_metric_available(HwMetric::GpuFan));

        hw.gpu_fan_pct = None;
        hw.gpu_fan_rpm = Some(1500);
        assert!(hw.is_metric_available(HwMetric::GpuFan));
    }
}
