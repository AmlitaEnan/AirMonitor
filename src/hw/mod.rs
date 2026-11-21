pub mod cpu;
pub mod disk;
pub mod gpu;
pub mod mem;
pub mod net;
mod pdh;
pub mod power;
pub mod sensors;

use std::time::{Duration, Instant};

use crate::config::{AppConfig, HwMetric, LayoutMode};
use crate::state::HardwareMetrics;
use cpu::CpuMonitor;
use disk::DiskMonitor;
use gpu::{GpuDemand, GpuMonitor, GpuSample};
use mem::sample_memory;
use net::NetMonitor;
use power::{battery_diagnostics, sample_system_battery};
use sensors::{external_gpu_matches_selected, ExternalSensorSample, SensorDemand, SensorHub};

const IDLE_RELEASE_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HardwareDemand {
    pub cpu_usage: bool,
    pub cpu_freq: bool,
    pub memory: bool,
    pub network: bool,
    pub gpu_vendor: bool,
    pub gpu_usage_pdh: bool,
    pub gpu_vram: bool,
    pub disk_io: bool,
    pub sys_battery: bool,
    pub external_sensors: bool,
    pub gpu_temp: bool,
    pub gpu_hotspot: bool,
    pub gpu_power: bool,
    pub gpu_fan: bool,
    pub cpu_temp: bool,
    pub cpu_power: bool,
    pub cpu_fan: bool,
    pub disk_temp: bool,
    pub acpi_temp: bool,
}

impl HardwareDemand {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::none()
    }

    pub fn all(enable_external_sensors: bool) -> Self {
        Self {
            cpu_usage: true,
            cpu_freq: true,
            memory: true,
            network: true,
            gpu_vendor: true,
            gpu_usage_pdh: true,
            gpu_vram: true,
            disk_io: true,
            sys_battery: true,
            external_sensors: enable_external_sensors,
            gpu_temp: true,
            gpu_hotspot: true,
            gpu_power: true,
            gpu_fan: true,
            cpu_temp: enable_external_sensors,
            cpu_power: enable_external_sensors,
            cpu_fan: enable_external_sensors,
            disk_temp: enable_external_sensors,
            acpi_temp: enable_external_sensors,
        }
    }

    pub fn from_config(cfg: &AppConfig, popup_open: bool) -> Self {
        Self::from_config_with_mode(cfg, true, popup_open)
    }

    pub fn from_config_with_mode(
        cfg: &AppConfig,
        hardware_enabled: bool,
        popup_overview_open: bool,
    ) -> Self {
        if !hardware_enabled {
            return Self::none();
        }
        if popup_overview_open {
            return Self::all(cfg.enable_external_sensors);
        }
        if !is_hw_taskbar_visible(cfg) {
            return Self::none();
        }

        let has = |m: HwMetric| cfg.hardware_items.contains(&m);
        let gpu_usage_pdh = has(HwMetric::GpuUsage);
        let gpu_vram = has(HwMetric::GpuVram);
        let gpu_temp = has(HwMetric::GpuTemp);
        let gpu_hotspot = has(HwMetric::GpuHotspot);
        let gpu_power = has(HwMetric::GpuPower);
        let gpu_fan = has(HwMetric::GpuFan);
        let gpu_vendor =
            gpu_usage_pdh || gpu_vram || gpu_temp || gpu_hotspot || gpu_power || gpu_fan;

        let cpu_temp = cfg.enable_external_sensors && has(HwMetric::CpuTemp);
        let cpu_power = cfg.enable_external_sensors && has(HwMetric::CpuPower);
        let cpu_fan = cfg.enable_external_sensors && has(HwMetric::CpuFan);
        let disk_temp = cfg.enable_external_sensors && has(HwMetric::DiskTemp);
        let ext_gpu =
            cfg.enable_external_sensors && (gpu_temp || gpu_hotspot || gpu_power || gpu_fan);
        let external_sensors = cpu_temp || cpu_power || cpu_fan || disk_temp || ext_gpu;

        Self {
            cpu_usage: has(HwMetric::CpuUsage),
            cpu_freq: has(HwMetric::CpuFreq),
            memory: has(HwMetric::MemUsage),
            network: has(HwMetric::NetUp) || has(HwMetric::NetDown),
            gpu_vendor,
            gpu_usage_pdh,
            gpu_vram,
            disk_io: has(HwMetric::DiskRead) || has(HwMetric::DiskWrite),
            sys_battery: has(HwMetric::SysBattery),
            external_sensors,
            gpu_temp,
            gpu_hotspot,
            gpu_power,
            gpu_fan,
            cpu_temp,
            cpu_power,
            cpu_fan,
            disk_temp,
            acpi_temp: false,
        }
    }

    pub fn gpu_demand(&self) -> GpuDemand {
        if !self.gpu_vendor {
            return GpuDemand::none();
        }
        let has_fine_grained = self.gpu_usage_pdh
            || self.gpu_vram
            || self.gpu_temp
            || self.gpu_hotspot
            || self.gpu_power
            || self.gpu_fan;
        if has_fine_grained {
            GpuDemand {
                usage: self.gpu_usage_pdh,
                temp: self.gpu_temp,
                hotspot: self.gpu_hotspot,
                power: self.gpu_power,
                fan: self.gpu_fan,
                vram: self.gpu_vram,
            }
        } else {
            // Backward compatibility when callers set `gpu_vendor: true` with `..Default::default()`.
            GpuDemand {
                usage: self.gpu_usage_pdh,
                temp: true,
                hotspot: true,
                power: true,
                fan: true,
                vram: self.gpu_vram,
            }
        }
    }

    pub fn sensor_demand(&self) -> SensorDemand {
        if !self.external_sensors {
            return SensorDemand::none();
        }
        let has_fine_grained = self.cpu_temp
            || self.cpu_power
            || self.cpu_fan
            || self.disk_temp
            || self.acpi_temp
            || (self.gpu_temp || self.gpu_hotspot || self.gpu_power || self.gpu_fan);
        if has_fine_grained {
            SensorDemand {
                cpu_temp: self.cpu_temp,
                cpu_power: self.cpu_power,
                cpu_fan: self.cpu_fan,
                disk_temp: self.disk_temp,
                gpu_temp: self.gpu_temp,
                gpu_hotspot: self.gpu_hotspot,
                gpu_power: self.gpu_power,
                gpu_fan: self.gpu_fan,
                acpi_temp: self.acpi_temp || self.cpu_temp,
            }
        } else {
            SensorDemand::all()
        }
    }
}

fn is_hw_taskbar_visible(cfg: &AppConfig) -> bool {
    match cfg.layout_mode {
        LayoutMode::Combined => cfg.combined_zone.visible,
        LayoutMode::HardwareOnly | LayoutMode::Split => cfg.hardware_zone.visible,
        LayoutMode::DevicesOnly => false,
    }
}

pub struct HardwareCollector {
    cpu: CpuMonitor,
    net: Option<NetMonitor>,
    gpu: Option<GpuMonitor>,
    disk: Option<DiskMonitor>,
    sensors: SensorHub,
    cpu_freq_idle_since: Option<Instant>,
    net_idle_since: Option<Instant>,
    gpu_idle_since: Option<Instant>,
    gpu_usage_pdh_idle_since: Option<Instant>,
    gpu_vram_pdh_idle_since: Option<Instant>,
    disk_idle_since: Option<Instant>,
    sensors_idle_since: Option<Instant>,
}

impl HardwareCollector {
    pub fn new() -> Self {
        Self {
            cpu: CpuMonitor::new(),
            net: Some(NetMonitor::new()),
            gpu: Some(GpuMonitor::new()),
            disk: Some(DiskMonitor::new()),
            sensors: SensorHub::new(),
            cpu_freq_idle_since: None,
            net_idle_since: None,
            gpu_idle_since: None,
            gpu_usage_pdh_idle_since: None,
            gpu_vram_pdh_idle_since: None,
            disk_idle_since: None,
            sensors_idle_since: None,
        }
    }

    pub fn new_lazy() -> Self {
        Self {
            cpu: CpuMonitor::new_lazy(),
            net: None,
            gpu: None,
            disk: None,
            sensors: SensorHub::new(),
            cpu_freq_idle_since: None,
            net_idle_since: None,
            gpu_idle_since: None,
            gpu_usage_pdh_idle_since: None,
            gpu_vram_pdh_idle_since: None,
            disk_idle_since: None,
            sensors_idle_since: None,
        }
    }

    pub fn sample(&mut self) -> HardwareMetrics {
        self.sample_with_demand(HardwareDemand::all(true)).0
    }

    pub fn sample_with_options(&mut self, enable_external_sensors: bool) -> HardwareMetrics {
        self.sample_with_demand(HardwareDemand::all(enable_external_sensors))
            .0
    }

    pub fn sample_with_demand(&mut self, demand: HardwareDemand) -> (HardwareMetrics, bool) {
        self.sample_with_demand_at(demand, Instant::now(), IDLE_RELEASE_GRACE)
    }

    pub fn release_all(&mut self) -> bool {
        let mut released = false;
        if self.cpu.release() {
            released = true;
        }
        if self.net.take().is_some() {
            released = true;
        }
        if let Some(mut d) = self.disk.take() {
            let _ = d.release();
            released = true;
        }
        if let Some(mut g) = self.gpu.take() {
            let _ = g.release_pdh_queries();
            released = true;
        }
        if self.sensors.release() {
            released = true;
        }
        self.cpu_freq_idle_since = None;
        self.net_idle_since = None;
        self.gpu_idle_since = None;
        self.gpu_usage_pdh_idle_since = None;
        self.gpu_vram_pdh_idle_since = None;
        self.disk_idle_since = None;
        self.sensors_idle_since = None;
        released
    }

    fn sample_with_demand_at(
        &mut self,
        demand: HardwareDemand,
        now: Instant,
        grace: Duration,
    ) -> (HardwareMetrics, bool) {
        if demand.is_empty() {
            let released = self.release_all();
            return (HardwareMetrics::unavailable(), released);
        }

        let mut released_idle = false;

        let (cpu_usage_pct, cpu_usage_valid, cpu_freq_ghz) = if demand.cpu_usage || demand.cpu_freq
        {
            let (usage_opt, freq) = if demand.cpu_usage {
                self.cpu.sample_with_freq(demand.cpu_freq)
            } else {
                // Only CPU freq demanded: skip CPU usage delta read.
                (None, self.cpu.sample_with_freq(true).1)
            };
            if demand.cpu_usage {
                match usage_opt {
                    Some(u) => (u, true, freq),
                    None => (0.0, false, freq),
                }
            } else {
                (0.0, false, freq)
            }
        } else {
            (0.0, false, None)
        };
        if update_idle_release(
            &mut self.cpu_freq_idle_since,
            self.cpu.has_freq_pdh(),
            demand.cpu_freq,
            now,
            grace,
        ) && self.cpu.release_freq_pdh()
        {
            released_idle = true;
        }

        let (mem, mem_valid) = if demand.memory {
            match sample_memory() {
                Some(m) => (m, true),
                None => ((0.0, 0.0, 0.0), false),
            }
        } else {
            ((0.0, 0.0, 0.0), false)
        };

        let (net, net_valid) = if demand.network {
            self.net_idle_since = None;
            match self.net.get_or_insert_with(NetMonitor::new).sample() {
                Some(n) => (n, true),
                None => ((0.0, 0.0, String::new()), false),
            }
        } else {
            if update_idle_release(
                &mut self.net_idle_since,
                self.net.is_some(),
                false,
                now,
                grace,
            ) {
                self.net = None;
            }
            ((0.0, 0.0, String::new()), false)
        };

        let disk = if demand.disk_io {
            self.disk_idle_since = None;
            self.disk.get_or_insert_with(DiskMonitor::new).sample()
        } else {
            if update_idle_release(
                &mut self.disk_idle_since,
                self.disk.is_some(),
                false,
                now,
                grace,
            ) {
                if let Some(mut d) = self.disk.take() {
                    let _ = d.release();
                    released_idle = true;
                }
            }
            (None, None)
        };

        let sys_battery = if demand.sys_battery {
            sample_system_battery()
        } else {
            (None, false)
        };

        let gpu_demand = demand.gpu_demand();
        let gpu = if demand.gpu_vendor {
            self.gpu_idle_since = None;
            let gpu_mon = self.gpu.get_or_insert_with(|| {
                GpuMonitor::new_with_demand(gpu_demand.usage, gpu_demand.vram)
            });
            let sample = gpu_mon.sample_with_gpu_demand(gpu_demand);

            if update_idle_release(
                &mut self.gpu_usage_pdh_idle_since,
                gpu_mon.has_usage_pdh(),
                gpu_demand.usage,
                now,
                grace,
            ) && gpu_mon.release_usage_pdh()
            {
                released_idle = true;
            }
            if update_idle_release(
                &mut self.gpu_vram_pdh_idle_since,
                gpu_mon.has_vram_pdh(),
                gpu_demand.vram,
                now,
                grace,
            ) && gpu_mon.release_vram_pdh()
            {
                released_idle = true;
            }
            sample
        } else {
            self.gpu_usage_pdh_idle_since = None;
            self.gpu_vram_pdh_idle_since = None;
            if update_idle_release(
                &mut self.gpu_idle_since,
                self.gpu.is_some(),
                false,
                now,
                grace,
            ) {
                self.gpu = None;
                released_idle = true;
            }
            GpuSample::default()
        };

        let sensor_demand = demand.sensor_demand();
        let ext = if demand.external_sensors && sensor_demand.any() {
            self.sensors_idle_since = None;
            let selected_gpu_name = (!gpu.name.is_empty()).then_some(gpu.name.as_str());
            self.sensors
                .sample_with_demand(true, sensor_demand, selected_gpu_name)
        } else {
            if update_idle_release(
                &mut self.sensors_idle_since,
                self.sensors.is_active(),
                false,
                now,
                grace,
            ) && self.sensors.release()
            {
                released_idle = true;
            }
            ExternalSensorSample::default()
        };

        let metrics = assemble_metrics(
            cpu_usage_pct,
            cpu_usage_valid,
            cpu_freq_ghz,
            mem,
            mem_valid,
            net,
            net_valid,
            disk,
            sys_battery,
            gpu,
            ext,
        );
        (metrics, released_idle)
    }

    pub fn probe_diagnostics(&mut self) -> String {
        let mut lines = Vec::new();
        lines.push("  [GPU]".to_string());
        if let Some(gpu) = &self.gpu {
            lines.push(gpu.diagnostics());
        } else {
            let temp_gpu = GpuMonitor::new();
            lines.push(temp_gpu.diagnostics());
        }
        lines.push("  [Disk & Power]".to_string());
        if let Some(disk) = &self.disk {
            lines.push(format!("  {}", disk.diagnostics()));
        } else {
            let temp_disk = DiskMonitor::new();
            lines.push(format!("  {}", temp_disk.diagnostics()));
        }
        lines.push(format!("  {}", battery_diagnostics()));
        lines.push("  [External Sensors]".to_string());
        lines.push(self.sensors.diagnostics());
        lines.join("\n")
    }
}

fn update_idle_release(
    idle_since: &mut Option<Instant>,
    active: bool,
    demanded: bool,
    now: Instant,
    grace: Duration,
) -> bool {
    if demanded || !active {
        *idle_since = None;
        return false;
    }
    let start = *idle_since.get_or_insert(now);
    if now.saturating_duration_since(start) >= grace {
        *idle_since = None;
        true
    } else {
        false
    }
}

fn should_backfill_external_gpu(gpu: &GpuSample, ext: &ExternalSensorSample) -> bool {
    if gpu.name.is_empty() {
        return false;
    }
    match ext.gpu_sensor_name.as_deref() {
        Some(ext_name) => external_gpu_matches_selected(&gpu.name, ext_name),
        // When external tool doesn't expose GPU adapter name (e.g. AIDA64/Afterburner GPU1),
        // only backfill on single-GPU systems to avoid cross-GPU contamination.
        None => !gpu.multi_gpu,
    }
}

#[allow(clippy::too_many_arguments)]
fn assemble_metrics(
    cpu_usage_pct: f32,
    cpu_usage_valid: bool,
    cpu_freq_ghz: Option<f32>,
    mem: (f32, f32, f32),
    mem_valid: bool,
    net: (f64, f64, String),
    net_valid: bool,
    disk: (Option<f64>, Option<f64>),
    sys_battery: (Option<u8>, bool),
    gpu: GpuSample,
    ext: ExternalSensorSample,
) -> HardwareMetrics {
    let (mem_usage_pct, mem_used_gb, mem_total_gb) = mem;
    let (net_up_bps, net_down_bps, net_adapter_name) = net;
    let (disk_read_bps, disk_write_bps) = disk;
    let (sys_battery_pct, sys_battery_charging) = sys_battery;
    let allow_ext_gpu = should_backfill_external_gpu(&gpu, &ext);
    let sensor_provider = ext.effective_provider().to_string();

    HardwareMetrics {
        cpu_usage_pct,
        cpu_usage_valid,
        cpu_freq_ghz,
        cpu_temp_c: ext.cpu_temp_c,
        acpi_temp_c: ext.acpi_temp_c,
        cpu_power_w: ext.cpu_power_w,
        cpu_fan_rpm: ext.cpu_fan_rpm,
        mem_usage_pct,
        mem_used_gb,
        mem_total_gb,
        mem_valid,
        net_up_bps,
        net_down_bps,
        net_adapter_name,
        net_valid,
        disk_read_bps,
        disk_write_bps,
        disk_temp_c: ext.disk_temp_c,
        sys_battery_pct,
        sys_battery_charging,
        gpu_name: gpu.name,
        gpu_usage_pct: gpu.usage_pct,
        gpu_temp_c: gpu
            .temp_c
            .or_else(|| allow_ext_gpu.then_some(ext.gpu_temp_c).flatten()),
        gpu_hotspot_c: gpu
            .hotspot_c
            .or_else(|| allow_ext_gpu.then_some(ext.gpu_hotspot_c).flatten()),
        gpu_power_w: gpu
            .power_w
            .or_else(|| allow_ext_gpu.then_some(ext.gpu_power_w).flatten()),
        gpu_fan_rpm: gpu
            .fan_rpm
            .or_else(|| allow_ext_gpu.then_some(ext.gpu_fan_rpm).flatten()),
        gpu_fan_pct: gpu.fan_pct,
        gpu_vram_used_gb: gpu.vram_used_gb,
        gpu_vram_total_gb: gpu.vram_total_gb,
        gpu_provider: gpu.provider.to_string(),
        sensor_provider,
    }
}

impl Default for HardwareCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn assert_metrics_sane(m: &HardwareMetrics) {
        assert!(m.cpu_usage_pct.is_finite());
        assert!((0.0..=100.0).contains(&m.cpu_usage_pct));

        if let Some(freq) = m.cpu_freq_ghz {
            assert!(freq.is_finite());
            assert!(freq > 0.0 && freq < 20.0, "unexpected cpu freq: {freq}");
        }
        if let Some(temp) = m.cpu_temp_c {
            assert!(temp.is_finite() && (-20.0..=150.0).contains(&temp));
        }
        if let Some(power) = m.cpu_power_w {
            assert!(power.is_finite() && (0.0..=2000.0).contains(&power));
        }

        assert!(m.mem_usage_pct.is_finite());
        assert!((0.0..=100.0).contains(&m.mem_usage_pct));
        if m.mem_valid {
            assert!(m.mem_total_gb.is_finite() && m.mem_total_gb > 0.0);
        }
        assert!(m.mem_used_gb.is_finite() && m.mem_used_gb >= 0.0);
        assert!(
            m.mem_used_gb <= m.mem_total_gb + 0.01,
            "mem_used_gb ({}) exceeded mem_total_gb ({})",
            m.mem_used_gb,
            m.mem_total_gb
        );

        assert!(m.net_up_bps.is_finite() && m.net_up_bps >= 0.0);
        assert!(m.net_down_bps.is_finite() && m.net_down_bps >= 0.0);

        if let Some(r) = m.disk_read_bps {
            assert!(r.is_finite() && r >= 0.0);
        }
        if let Some(w) = m.disk_write_bps {
            assert!(w.is_finite() && w >= 0.0);
        }
        if let Some(temp) = m.disk_temp_c {
            assert!(temp.is_finite() && (-20.0..=150.0).contains(&temp));
        }
        if let Some(pct) = m.sys_battery_pct {
            assert!(pct <= 100);
        }

        if let Some(gpu_pct) = m.gpu_usage_pct {
            assert!(gpu_pct.is_finite() && (0.0..=100.0).contains(&gpu_pct));
        }
        if let Some(temp) = m.gpu_temp_c {
            assert!(temp.is_finite() && (-20.0..=150.0).contains(&temp));
        }
        if let Some(hotspot) = m.gpu_hotspot_c {
            assert!(hotspot.is_finite() && (-20.0..=150.0).contains(&hotspot));
        }
        if let Some(power) = m.gpu_power_w {
            assert!(power.is_finite() && (0.0..=2000.0).contains(&power));
        }
        if let Some(fan_pct) = m.gpu_fan_pct {
            assert!(fan_pct <= 100);
        }
        if let Some(vram_used) = m.gpu_vram_used_gb {
            assert!(vram_used.is_finite() && vram_used >= 0.0);
        }
        if let Some(vram_total) = m.gpu_vram_total_gb {
            assert!(vram_total.is_finite() && vram_total > 0.0);
        }
    }

    #[test]
    fn gpu_vendor_metrics_take_priority_over_external_sensors() {
        let gpu = GpuSample {
            name: "Test GPU".to_string(),
            usage_pct: Some(33.0),
            temp_c: Some(61.0),
            hotspot_c: None,
            power_w: Some(120.0),
            fan_rpm: None,
            fan_pct: Some(45),
            vram_used_gb: Some(2.5),
            vram_total_gb: Some(8.0),
            provider: "NVIDIA NVML",
            multi_gpu: false,
        };
        let ext = ExternalSensorSample {
            provider: "HWiNFO64",
            cpu_temp_c: Some(55.0),
            acpi_temp_c: Some(44.0),
            cpu_power_w: Some(42.5),
            cpu_fan_rpm: Some(1100),
            disk_temp_c: Some(39.0),
            gpu_temp_c: Some(70.0),
            gpu_hotspot_c: Some(79.0),
            gpu_power_w: Some(150.0),
            gpu_fan_rpm: Some(1650),
            ..Default::default()
        };

        let merged = assemble_metrics(
            10.0,
            true,
            Some(4.2),
            (50.0, 16.0, 32.0),
            true,
            (100.0, 200.0, "NIC".to_string()),
            true,
            (Some(1024.0), Some(2048.0)),
            (Some(85), true),
            gpu,
            ext,
        );

        assert!(merged.cpu_usage_valid && merged.mem_valid && merged.net_valid);
        assert_eq!(merged.acpi_temp_c, Some(44.0));
        assert_eq!(merged.gpu_temp_c, Some(61.0));
        assert_eq!(merged.gpu_hotspot_c, Some(79.0));
        assert_eq!(merged.gpu_power_w, Some(120.0));
        assert_eq!(merged.gpu_fan_rpm, Some(1650));
        assert_eq!(merged.gpu_fan_pct, Some(45));
        assert_eq!(merged.gpu_provider, "NVIDIA NVML");
        assert_eq!(merged.sensor_provider, "HWiNFO64");
        assert_eq!(merged.cpu_temp_c, Some(55.0));
        assert_eq!(merged.cpu_power_w, Some(42.5));
        assert_eq!(merged.cpu_fan_rpm, Some(1100));
        assert_eq!(merged.disk_temp_c, Some(39.0));
        assert_eq!(merged.sys_battery_pct, Some(85));
        assert!(merged.sys_battery_charging);
    }

    #[test]
    fn external_gpu_backfill_blocks_mismatched_or_unidentified_multi_gpu_sensors() {
        let gpu_nv = GpuSample {
            name: "NVIDIA GeForce RTX 4090".to_string(),
            multi_gpu: true,
            ..Default::default()
        };

        // 1. Mismatched AMD sensor must NOT backfill into NVIDIA dGPU
        let ext_amd = ExternalSensorSample {
            provider: "HWiNFO64",
            gpu_temp_c: Some(41.0),
            gpu_power_w: Some(18.0),
            gpu_sensor_name: Some("GPU [#0]: AMD Radeon(TM) Graphics".to_string()),
            ..Default::default()
        };
        let merged_mismatch = assemble_metrics(
            0.0,
            false,
            None,
            (0.0, 0.0, 0.0),
            false,
            (0.0, 0.0, String::new()),
            false,
            (None, None),
            (None, false),
            gpu_nv.clone(),
            ext_amd,
        );
        assert_eq!(merged_mismatch.gpu_temp_c, None);
        assert_eq!(merged_mismatch.gpu_power_w, None);

        // 2. Unidentified external GPU on multi-GPU host must NOT backfill
        let ext_anon = ExternalSensorSample {
            provider: "AIDA64",
            gpu_temp_c: Some(50.0),
            gpu_sensor_name: None,
            ..Default::default()
        };
        let merged_anon = assemble_metrics(
            0.0,
            false,
            None,
            (0.0, 0.0, 0.0),
            false,
            (0.0, 0.0, String::new()),
            false,
            (None, None),
            (None, false),
            gpu_nv.clone(),
            ext_anon,
        );
        assert_eq!(merged_anon.gpu_temp_c, None);

        // 3. Matching external GPU sensor name DOES backfill missing fields
        let ext_nv = ExternalSensorSample {
            provider: "HWiNFO64",
            gpu_temp_c: Some(64.0),
            gpu_hotspot_c: Some(76.0),
            gpu_sensor_name: Some("GPU [#1]: NVIDIA GeForce RTX 4090".to_string()),
            ..Default::default()
        };
        let merged_match = assemble_metrics(
            0.0,
            false,
            None,
            (0.0, 0.0, 0.0),
            false,
            (0.0, 0.0, String::new()),
            false,
            (None, None),
            (None, false),
            gpu_nv,
            ext_nv,
        );
        assert_eq!(merged_match.gpu_temp_c, Some(64.0));
        assert_eq!(merged_match.gpu_hotspot_c, Some(76.0));
    }

    #[test]
    fn hardware_collector_default_samples_sane_metrics() {
        let mut collector = HardwareCollector::default();
        let _warm = collector.sample();
        thread::sleep(Duration::from_millis(60));
        let first = collector.sample();
        assert_metrics_sane(&first);
        let diag = collector.probe_diagnostics();
        assert!(!diag.is_empty());
    }

    #[test]
    fn hardware_collector_repeated_samples_remain_consistent() {
        let mut collector = HardwareCollector::new();
        let first = collector.sample();
        thread::sleep(Duration::from_millis(60));
        let second = collector.sample_with_options(false);

        assert_metrics_sane(&first);
        assert_metrics_sane(&second);
        assert!((first.mem_total_gb - second.mem_total_gb).abs() < 0.01);
        assert_eq!(first.gpu_name, second.gpu_name);
        assert_eq!(second.cpu_temp_c, None);
        assert_eq!(second.acpi_temp_c, None);
        assert_eq!(second.cpu_power_w, None);
        assert_eq!(second.cpu_fan_rpm, None);
        assert_eq!(second.disk_temp_c, None);
    }

    #[test]
    fn hardware_demand_from_config_covers_modes_and_single_metrics() {
        let mut cfg = AppConfig::default();

        // Default config with popup closed: only cpu_usage, memory, and network
        let d_default = HardwareDemand::from_config(&cfg, false);
        assert_eq!(
            d_default,
            HardwareDemand {
                cpu_usage: true,
                memory: true,
                network: true,
                ..HardwareDemand::none()
            }
        );

        // Disabled hardware monitoring returns completely empty demand even with popup open
        assert_eq!(
            HardwareDemand::from_config_with_mode(&cfg, false, true),
            HardwareDemand::none()
        );
        assert!(HardwareDemand::none().is_empty());

        // Popup open with hardware enabled enables all hardware metrics
        assert_eq!(
            HardwareDemand::from_config(&cfg, true),
            HardwareDemand::all(true)
        );
        cfg.enable_external_sensors = false;
        assert_eq!(
            HardwareDemand::from_config(&cfg, true),
            HardwareDemand::all(false)
        );

        // DevicesOnly hides hardware bar when popup closed -> completely none (no hardcoded CPU/MEM!)
        cfg.enable_external_sensors = true;
        cfg.layout_mode = LayoutMode::DevicesOnly;
        cfg.hardware_items = HwMetric::ALL.to_vec();
        assert_eq!(
            HardwareDemand::from_config(&cfg, false),
            HardwareDemand::none()
        );
        // DevicesOnly with popup open still samples all when hardware_enabled
        assert_eq!(
            HardwareDemand::from_config(&cfg, true),
            HardwareDemand::all(true)
        );

        // Hidden zone visibility also yields HardwareDemand::none() when popup closed
        cfg.layout_mode = LayoutMode::Combined;
        cfg.combined_zone.visible = false;
        assert_eq!(
            HardwareDemand::from_config(&cfg, false),
            HardwareDemand::none()
        );
        cfg.combined_zone.visible = true;

        cfg.layout_mode = LayoutMode::Split;
        cfg.hardware_zone.visible = false;
        assert_eq!(
            HardwareDemand::from_config(&cfg, false),
            HardwareDemand::none()
        );
        cfg.hardware_zone.visible = true;

        // Empty hardware_items with Combined layout -> completely none
        cfg.layout_mode = LayoutMode::Combined;
        cfg.hardware_items.clear();
        assert_eq!(
            HardwareDemand::from_config(&cfg, false),
            HardwareDemand::none()
        );

        // Individual checkboxes (no hardcoded cpu_usage/memory)
        cfg.hardware_items = vec![HwMetric::CpuFreq];
        let d_freq = HardwareDemand::from_config(&cfg, false);
        assert!(d_freq.cpu_freq && !d_freq.cpu_usage && !d_freq.memory && !d_freq.gpu_vendor);

        cfg.hardware_items = vec![HwMetric::DiskRead];
        assert!(HardwareDemand::from_config(&cfg, false).disk_io);

        cfg.hardware_items = vec![HwMetric::SysBattery];
        assert!(HardwareDemand::from_config(&cfg, false).sys_battery);

        cfg.hardware_items = vec![HwMetric::GpuVram];
        let d_vram = HardwareDemand::from_config(&cfg, false);
        assert!(d_vram.gpu_vendor && d_vram.gpu_vram);
        assert!(!d_vram.gpu_usage_pdh && !d_vram.external_sensors);
        assert_eq!(
            d_vram.gpu_demand(),
            GpuDemand {
                vram: true,
                ..GpuDemand::none()
            }
        );

        cfg.hardware_items = vec![HwMetric::GpuUsage];
        let d_gpu_usage = HardwareDemand::from_config(&cfg, false);
        assert!(d_gpu_usage.gpu_vendor && d_gpu_usage.gpu_usage_pdh);
        assert!(!d_gpu_usage.gpu_vram && !d_gpu_usage.external_sensors);

        cfg.hardware_items = vec![HwMetric::GpuTemp];
        let d_gpu_temp = HardwareDemand::from_config(&cfg, false);
        assert!(d_gpu_temp.gpu_vendor && d_gpu_temp.gpu_temp && d_gpu_temp.external_sensors);
        assert!(!d_gpu_temp.gpu_usage_pdh && !d_gpu_temp.gpu_vram && !d_gpu_temp.gpu_power);

        cfg.enable_external_sensors = false;
        assert!(!HardwareDemand::from_config(&cfg, false).external_sensors);

        cfg.enable_external_sensors = true;
        cfg.hardware_items = vec![HwMetric::CpuTemp];
        let d_cpu_temp = HardwareDemand::from_config(&cfg, false);
        assert!(d_cpu_temp.external_sensors && d_cpu_temp.cpu_temp);
        assert!(!d_cpu_temp.gpu_vendor);
    }

    #[test]
    fn lazy_collector_initializes_on_demand_and_releases_after_idle_grace() {
        let mut collector = HardwareCollector::new_lazy();
        assert!(collector.net.is_none());
        assert!(collector.gpu.is_none());
        assert!(collector.disk.is_none());
        assert!(!collector.cpu.has_freq_pdh());
        assert!(!collector.sensors.is_active());

        let t0 = Instant::now();
        let minimal = HardwareDemand {
            cpu_usage: true,
            memory: true,
            ..HardwareDemand::none()
        };

        // Minimal demand keeps heavy subsystems uninitialized
        let (m0, released0) = collector.sample_with_demand_at(minimal, t0, IDLE_RELEASE_GRACE);
        assert!(!released0);
        assert!(m0.mem_valid && m0.mem_total_gb > 0.0);
        assert!(!m0.net_valid);
        assert!(collector.net.is_none());
        assert!(collector.gpu.is_none());
        assert!(collector.disk.is_none());
        assert!(!collector.cpu.has_freq_pdh());
        assert!(!collector.sensors.is_active());

        // Demand all -> initializes heavy subsystems
        let (_m1, released1) =
            collector.sample_with_demand_at(HardwareDemand::all(true), t0, IDLE_RELEASE_GRACE);
        assert!(!released1);
        assert!(collector.net.is_some());
        assert!(collector.gpu.is_some());
        assert!(collector.disk.is_some());
        assert!(collector.sensors.is_active());

        // Idle for 10s (< 30s grace) -> stays warm, nothing released
        let (_m2, released2) = collector.sample_with_demand_at(
            minimal,
            t0 + Duration::from_secs(10),
            IDLE_RELEASE_GRACE,
        );
        assert!(!released2);
        assert!(collector.gpu.is_some());
        assert!(collector.disk.is_some());

        // Re-demand within 30s grace resets the idle timer
        let (_m3, released3) = collector.sample_with_demand_at(
            HardwareDemand::all(true),
            t0 + Duration::from_secs(20),
            IDLE_RELEASE_GRACE,
        );
        assert!(!released3);
        assert!(collector.gpu.is_some());
        assert!(collector.disk.is_some());

        // Idle started at t0+25s -> at t0+45s (20s elapsed) still warm
        let (_m4, released4) = collector.sample_with_demand_at(
            minimal,
            t0 + Duration::from_secs(25),
            IDLE_RELEASE_GRACE,
        );
        assert!(!released4);
        let (_m5, released5) = collector.sample_with_demand_at(
            minimal,
            t0 + Duration::from_secs(45),
            IDLE_RELEASE_GRACE,
        );
        assert!(!released5);
        assert!(collector.gpu.is_some());
        assert!(collector.disk.is_some());

        // At t0+56s (31s elapsed since t0+25s) -> releases all idle subsystems
        let (_m6, released6) = collector.sample_with_demand_at(
            minimal,
            t0 + Duration::from_secs(56),
            IDLE_RELEASE_GRACE,
        );
        assert!(released6);
        assert!(collector.net.is_none());
        assert!(collector.gpu.is_none());
        assert!(collector.disk.is_none());
        assert!(!collector.cpu.has_freq_pdh());
        assert!(!collector.sensors.is_active());

        // Subsequent idle ticks remain idempotent (released = false)
        let (_m7, released7) = collector.sample_with_demand_at(
            minimal,
            t0 + Duration::from_secs(60),
            IDLE_RELEASE_GRACE,
        );
        assert!(!released7);
    }

    #[test]
    fn empty_demand_and_release_all_immediately_release_every_subsystem() {
        let mut collector = HardwareCollector::new_lazy();
        let t0 = Instant::now();
        let _ = collector.sample_with_demand_at(HardwareDemand::all(true), t0, IDLE_RELEASE_GRACE);
        assert!(collector.net.is_some());
        assert!(collector.gpu.is_some());
        assert!(collector.disk.is_some());
        assert!(collector.sensors.is_active());

        // Empty demand immediately releases everything without waiting 30s grace
        let (m_empty, released) =
            collector.sample_with_demand_at(HardwareDemand::none(), t0, IDLE_RELEASE_GRACE);
        assert!(released);
        assert!(!m_empty.cpu_usage_valid);
        assert!(!m_empty.mem_valid);
        assert!(!m_empty.net_valid);
        assert!(collector.net.is_none());
        assert!(collector.gpu.is_none());
        assert!(collector.disk.is_none());
        assert!(!collector.cpu.has_freq_pdh());
        assert!(!collector.sensors.is_active());

        // Subsequent release_all / empty sample is idempotent
        assert!(!collector.release_all());
        let (_m_empty2, released2) =
            collector.sample_with_demand_at(HardwareDemand::none(), t0, IDLE_RELEASE_GRACE);
        assert!(!released2);
    }
}
