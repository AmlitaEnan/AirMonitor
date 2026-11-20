use std::thread;
use std::time::Duration;

use airmonitor::hw::HardwareCollector;
use airmonitor::state::{format_speed, format_speed_full};
use airmonitor::wireless::WirelessManager;
use windows::Win32::System::LibraryLoader::{
    SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

fn main() {
    unsafe {
        let _ = SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
    }

    println!("=== AirMonitor Hardware & Wireless Probe ===");

    let mut hw = HardwareCollector::new();
    let _first = hw.sample();
    thread::sleep(Duration::from_secs(1));
    let m = hw.sample();

    println!("\n[Hardware Telemetry]");
    let freq_str = m
        .cpu_freq_ghz
        .map(|f| format!("{:.2} GHz", f))
        .unwrap_or_else(|| "N/A".to_string());
    let cpu_temp_str = m
        .cpu_temp_c
        .map(|v| format!("{:.1} C", v))
        .unwrap_or_else(|| "N/A".to_string());
    let cpu_power_str = m
        .cpu_power_w
        .map(|v| format!("{:.1} W", v))
        .unwrap_or_else(|| "N/A".to_string());
    let cpu_fan_str = m
        .cpu_fan_rpm
        .map(|v| format!("{} RPM", v))
        .unwrap_or_else(|| "N/A".to_string());
    println!(
        "  CPU:   {:.1}% @ {} | Temp: {} | Power: {} | Fan: {}",
        m.cpu_usage_pct, freq_str, cpu_temp_str, cpu_power_str, cpu_fan_str
    );
    println!(
        "  RAM:   {:.1}% ({:.2} / {:.2} GB)",
        m.mem_usage_pct, m.mem_used_gb, m.mem_total_gb
    );
    let nic = if m.net_adapter_name.is_empty() {
        "(none)"
    } else {
        &m.net_adapter_name
    };
    println!("  NIC:   {}", nic);
    println!(
        "  NET:   UP [{}] ({}) | DOWN [{}] ({})",
        format_speed(m.net_up_bps),
        format_speed_full(m.net_up_bps),
        format_speed(m.net_down_bps),
        format_speed_full(m.net_down_bps),
    );
    let disk_read_str = m
        .disk_read_bps
        .map(|v| format!("[{}] ({})", format_speed(v), format_speed_full(v)))
        .unwrap_or_else(|| "N/A".to_string());
    let disk_write_str = m
        .disk_write_bps
        .map(|v| format!("[{}] ({})", format_speed(v), format_speed_full(v)))
        .unwrap_or_else(|| "N/A".to_string());
    let disk_temp_str = m
        .disk_temp_c
        .map(|v| format!("{:.1} C", v))
        .unwrap_or_else(|| "N/A".to_string());
    println!(
        "  DISK:  READ {} | WRITE {} | Temp: {}",
        disk_read_str, disk_write_str, disk_temp_str
    );
    let sys_bat_str = match m.sys_battery_pct {
        Some(pct) if m.sys_battery_charging => format!("{}% (charging)", pct),
        Some(pct) => format!("{}%", pct),
        None => "N/A".to_string(),
    };
    println!("  BAT:   {}", sys_bat_str);

    let gpu_name = if m.gpu_name.is_empty() {
        "(none)"
    } else {
        &m.gpu_name
    };
    let gpu_provider = if m.gpu_provider.is_empty() {
        "(none)"
    } else {
        &m.gpu_provider
    };
    let sensor_provider = if m.sensor_provider.is_empty() {
        "(none)"
    } else {
        &m.sensor_provider
    };
    println!(
        "  GPU:   {} [provider: {} | ext_sensors: {}]",
        gpu_name, gpu_provider, sensor_provider
    );
    println!(
        "    Usage:   {}",
        m.gpu_usage_pct
            .map(|v| format!("{:.1}%", v))
            .unwrap_or_else(|| "N/A".to_string())
    );
    println!(
        "    Temp:    {} (Hotspot: {})",
        m.gpu_temp_c
            .map(|v| format!("{:.1} C", v))
            .unwrap_or_else(|| "N/A".to_string()),
        m.gpu_hotspot_c
            .map(|v| format!("{:.1} C", v))
            .unwrap_or_else(|| "N/A".to_string()),
    );
    let gpu_fan_rpm_str = m
        .gpu_fan_rpm
        .map(|v| format!("{} RPM", v))
        .unwrap_or_else(|| "N/A".to_string());
    let gpu_fan_pct_str = m
        .gpu_fan_pct
        .map(|v| format!("{}%", v))
        .unwrap_or_else(|| "N/A".to_string());
    println!(
        "    Power:   {} | Fan: {} ({})",
        m.gpu_power_w
            .map(|v| format!("{:.1} W", v))
            .unwrap_or_else(|| "N/A".to_string()),
        gpu_fan_rpm_str,
        gpu_fan_pct_str,
    );
    println!(
        "    VRAM:    {} / {}",
        m.gpu_vram_used_gb
            .map(|v| format!("{:.2} GB", v))
            .unwrap_or_else(|| "N/A".to_string()),
        m.gpu_vram_total_gb
            .map(|v| format!("{:.2} GB", v))
            .unwrap_or_else(|| "N/A".to_string()),
    );

    println!("\n[Hardware Provider Diagnostics]");
    println!("{}", hw.probe_diagnostics());

    println!("\n[Wireless Poll]");
    let mut wireless = WirelessManager::new();
    let devices = wireless.poll_once();
    if devices.is_empty() {
        println!("  No active wireless battery devices reported by poll_once().");
    } else {
        for dev in &devices {
            let bat = dev
                .battery_pct
                .map(|p| format!("{}%", p))
                .unwrap_or_else(|| "N/A".to_string());
            let chg = if dev.charging { " (charging)" } else { "" };
            println!(
                "  - [{}|{}] {} ({}) -> {}{}",
                dev.transport.badge(),
                dev.kind.short_label(),
                dev.name,
                dev.id,
                bat,
                chg
            );
            if let Some(sub) = dev.sub_batteries {
                println!(
                    "      Sub: L={:?}% (chg={}), R={:?}% (chg={}), Case={:?}% (chg={})",
                    sub.left_pct,
                    sub.left_charging,
                    sub.right_pct,
                    sub.right_charging,
                    sub.case_pct,
                    sub.case_charging
                );
            }
            if let Some(note) = &dev.detail_note {
                println!("      Note: {}", note);
            }
        }
    }

    println!("\n[Wireless Diagnostics]");
    let diag = wireless.probe_diagnostics();
    println!("{}", diag);
}
