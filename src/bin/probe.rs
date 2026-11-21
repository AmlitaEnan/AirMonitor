use std::env;
use std::fmt::Write as _;
use std::process;
use std::thread;
use std::time::Duration;

use airmonitor::hw::HardwareCollector;
use airmonitor::state::{format_speed, format_speed_full, HardwareMetrics, WirelessDevice};
use airmonitor::wireless::WirelessManager;
use windows::Win32::System::LibraryLoader::{
    SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliAction {
    Run { verbose: bool },
    Help,
}

fn usage_text() -> &'static str {
    "airmonitor-probe - AirMonitor hardware & wireless diagnostics\n\n\
     Usage:\n  \
       airmonitor-probe [OPTIONS]\n\n\
     Options:\n  \
       -v, --verbose    Show unredacted device IDs, MAC addresses, serials, and raw payloads\n  \
       -h, --help       Print this help message and exit"
}

fn parse_args<I, S>(args: I) -> Result<CliAction, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut verbose = false;
    for arg in args {
        match arg.as_ref() {
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => return Ok(CliAction::Help),
            other => {
                return Err(format!(
                    "Unknown argument: {} (use --help for usage)",
                    other
                ));
            }
        }
    }
    Ok(CliAction::Run { verbose })
}

fn mask_mac_token(raw: &str) -> String {
    let trimmed = raw.trim();
    let mut chars = trimmed.chars();
    let count = trimmed.chars().count();
    if count >= 8 {
        let prefix: String = chars.by_ref().take(2).collect();
        let suffix: String = trimmed
            .chars()
            .rev()
            .take(2)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("{}****{}", prefix, suffix)
    } else {
        "****".to_string()
    }
}

fn is_slot_token(s: &str) -> bool {
    s.strip_prefix("slot")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn split_trailing_slot(s: &str) -> Option<(&str, &str)> {
    let (head, tail) = s.rsplit_once(':')?;
    if !head.is_empty() && is_slot_token(tail) {
        Some((head, tail))
    } else {
        None
    }
}

fn mask_hid_ident(ident: &str) -> String {
    if let Some(guid) = ident.strip_prefix("c:") {
        format!("c:<redacted:{}>", guid.chars().count())
    } else {
        format!("<redacted:{}>", ident.chars().count())
    }
}

fn redact_device_id(id: &str, verbose: bool) -> String {
    if verbose {
        return id.to_string();
    }

    if let Some(rest) = id.strip_prefix("bt:") {
        let is_mac_like = rest
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '-')
            && rest.chars().filter(|c| c.is_ascii_hexdigit()).count() >= 8;
        if is_mac_like {
            return format!("bt:{}", mask_mac_token(rest));
        }
        return id.to_string();
    }

    if let Some(rest) = id.strip_prefix("xinput:mac:") {
        return format!("xinput:mac:{}", mask_mac_token(rest));
    }

    if id.starts_with("ble:") {
        if let Some((prefix, addr)) = id.rsplit_once(':') {
            if addr != "addr0" && addr.chars().count() >= 8 {
                return format!("{}:{}", prefix, mask_mac_token(addr));
            }
        }
        return id.to_string();
    }

    if let Some(rest) = id.strip_prefix("hid:") {
        let mut parts = rest.splitn(3, ':');
        if let (Some(vid), Some(pid), Some(tail)) = (parts.next(), parts.next(), parts.next()) {
            if is_slot_token(tail) {
                return id.to_string();
            }
            if let Some((ident, slot_suffix)) = split_trailing_slot(tail) {
                return format!(
                    "hid:{}:{}:{}:{}",
                    vid,
                    pid,
                    mask_hid_ident(ident),
                    slot_suffix
                );
            }
            return format!("hid:{}:{}:{}", vid, pid, mask_hid_ident(tail));
        }
        return id.to_string();
    }

    id.to_string()
}

fn format_hardware_telemetry(m: &HardwareMetrics) -> String {
    let mut out = String::new();

    let cpu_usage_str = if m.cpu_usage_valid {
        format!("{:.1}%", m.cpu_usage_pct)
    } else {
        "N/A".to_string()
    };
    let freq_str = m
        .cpu_freq_ghz
        .map(|f| format!("{:.2} GHz", f))
        .unwrap_or_else(|| "N/A".to_string());
    let cpu_temp_str = m
        .cpu_temp_c
        .map(|v| format!("{:.1} C", v))
        .unwrap_or_else(|| "N/A".to_string());
    let acpi_temp_str = m
        .acpi_temp_c
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

    let _ = writeln!(
        out,
        "  CPU:          {} @ {} | CPU Temp: {} | Power: {} | Fan: {}",
        cpu_usage_str, freq_str, cpu_temp_str, cpu_power_str, cpu_fan_str
    );
    let _ = writeln!(out, "  ACPI Thermal: {}", acpi_temp_str);

    let ram_str = if m.mem_valid {
        format!(
            "{:.1}% ({:.2} / {:.2} GB)",
            m.mem_usage_pct, m.mem_used_gb, m.mem_total_gb
        )
    } else {
        "N/A".to_string()
    };
    let _ = writeln!(out, "  RAM:          {}", ram_str);

    let nic_str = if !m.net_valid {
        "N/A".to_string()
    } else if m.net_adapter_name.is_empty() {
        "(none)".to_string()
    } else {
        format!("{} (active)", m.net_adapter_name)
    };
    let _ = writeln!(out, "  NIC:          {}", nic_str);

    let net_str = if m.net_valid {
        let scope = if m.net_adapter_name.is_empty() {
            "total across physical NICs".to_string()
        } else {
            format!(
                "active NIC: {}; total across physical NICs",
                m.net_adapter_name
            )
        };
        format!(
            "UP [{}] ({}) | DOWN [{}] ({}) [{}]",
            format_speed(m.net_up_bps),
            format_speed_full(m.net_up_bps),
            format_speed(m.net_down_bps),
            format_speed_full(m.net_down_bps),
            scope
        )
    } else {
        "UP N/A | DOWN N/A".to_string()
    };
    let _ = writeln!(out, "  NET:          {}", net_str);

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
    let _ = writeln!(
        out,
        "  DISK:         READ {} | WRITE {} | Temp: {}",
        disk_read_str, disk_write_str, disk_temp_str
    );

    let sys_bat_str = match m.sys_battery_pct {
        Some(pct) if m.sys_battery_charging => format!("{}% (charging)", pct),
        Some(pct) => format!("{}%", pct),
        None => "N/A".to_string(),
    };
    let _ = writeln!(out, "  BAT:          {}", sys_bat_str);

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
    let _ = writeln!(
        out,
        "  GPU:          {} [provider: {} | ext_sensors: {}]",
        gpu_name, gpu_provider, sensor_provider
    );
    let _ = writeln!(
        out,
        "    Usage:   {}",
        m.gpu_usage_pct
            .map(|v| format!("{:.1}%", v))
            .unwrap_or_else(|| "N/A".to_string())
    );
    let _ = writeln!(
        out,
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
    let _ = writeln!(
        out,
        "    Power:   {} | Fan: {} ({})",
        m.gpu_power_w
            .map(|v| format!("{:.1} W", v))
            .unwrap_or_else(|| "N/A".to_string()),
        gpu_fan_rpm_str,
        gpu_fan_pct_str,
    );
    let _ = write!(
        out,
        "    VRAM:    {} / {}",
        m.gpu_vram_used_gb
            .map(|v| format!("{:.2} GB", v))
            .unwrap_or_else(|| "N/A".to_string()),
        m.gpu_vram_total_gb
            .map(|v| format!("{:.2} GB", v))
            .unwrap_or_else(|| "N/A".to_string()),
    );

    out
}

fn format_wireless_poll(devices: &[WirelessDevice], verbose: bool) -> String {
    let mut out = String::new();
    if !verbose {
        let _ = writeln!(
            out,
            "  (Redacted device IDs/MACs/serials & raw payloads by default; pass --verbose for full output)"
        );
    }
    if devices.is_empty() {
        let _ = write!(
            out,
            "  No active wireless battery devices reported by poll_once()."
        );
        return out;
    }

    for (idx, dev) in devices.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let bat = dev
            .battery_pct
            .map(|p| format!("{}%", p))
            .unwrap_or_else(|| "N/A".to_string());
        let chg = if dev.charging { " (charging)" } else { "" };
        let id_disp = redact_device_id(&dev.id, verbose);
        let _ = write!(
            out,
            "  - [{}|{}] {} ({}) -> {}{}",
            dev.transport.badge(),
            dev.kind.short_label(),
            dev.name,
            id_disp,
            bat,
            chg
        );
        if let Some(sub) = dev.sub_batteries {
            let _ = write!(
                out,
                "\n      Sub: L={:?}% (chg={}), R={:?}% (chg={}), Case={:?}% (chg={})",
                sub.left_pct,
                sub.left_charging,
                sub.right_pct,
                sub.right_charging,
                sub.case_pct,
                sub.case_charging
            );
        }
        if let Some(note) = &dev.detail_note {
            let _ = write!(out, "\n      Note: {}", note);
        }
    }

    out
}

fn main() {
    unsafe {
        let _ = SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
    }

    let verbose = match parse_args(env::args().skip(1)) {
        Ok(CliAction::Help) => {
            println!("{}", usage_text());
            return;
        }
        Ok(CliAction::Run { verbose }) => verbose,
        Err(err) => {
            eprintln!("Error: {}\n\n{}", err, usage_text());
            process::exit(2);
        }
    };

    println!("=== AirMonitor Hardware & Wireless Probe ===");

    let mut hw = HardwareCollector::new();
    let _first = hw.sample();
    thread::sleep(Duration::from_secs(1));
    let m = hw.sample();

    println!("\n[Hardware Telemetry]");
    println!("{}", format_hardware_telemetry(&m));

    println!("\n[Hardware Provider Diagnostics]");
    println!("{}", hw.probe_diagnostics());

    println!("\n[Wireless Poll]");
    let mut wireless = WirelessManager::new();
    let devices = wireless.poll_once();
    println!("{}", format_wireless_poll(&devices, verbose));

    println!("\n[Wireless Diagnostics]");
    let diag = wireless.probe_diagnostics_with_options(verbose);
    println!("{}", diag);
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use airmonitor::state::{DeviceKind, Transport};

    use super::*;

    fn mk_device(id: &str, name: &str, transport: Transport) -> WirelessDevice {
        WirelessDevice {
            id: id.to_string(),
            name: name.to_string(),
            kind: DeviceKind::Mouse,
            transport,
            battery_pct: Some(80),
            charging: false,
            sub_batteries: None,
            online: true,
            last_seen: Instant::now(),
            detail_note: None,
        }
    }

    #[test]
    fn parse_args_supports_default_verbose_help_and_rejects_unknown() {
        assert_eq!(
            parse_args::<[&str; 0], &str>([]),
            Ok(CliAction::Run { verbose: false })
        );
        assert_eq!(parse_args(["-v"]), Ok(CliAction::Run { verbose: true }));
        assert_eq!(
            parse_args(["--verbose"]),
            Ok(CliAction::Run { verbose: true })
        );
        assert_eq!(parse_args(["-h"]), Ok(CliAction::Help));
        assert_eq!(parse_args(["--help"]), Ok(CliAction::Help));
        assert_eq!(parse_args(["-v", "--help"]), Ok(CliAction::Help));

        let err = parse_args(["--bogus"]).unwrap_err();
        assert!(err.contains("--bogus"));
        assert!(err.contains("--help"));
    }

    #[test]
    fn redact_device_id_masks_sensitive_tokens_unless_verbose() {
        // Bluetooth MAC
        assert_eq!(redact_device_id("bt:001122334455", false), "bt:00****55");
        assert_eq!(redact_device_id("bt:001122334455", true), "bt:001122334455");
        // Non-MAC fallback BT name stays readable
        assert_eq!(
            redact_device_id("bt:xbox wireless controller", false),
            "bt:xbox wireless controller"
        );

        // BLE AirPods address
        assert_eq!(
            redact_device_id("ble:apple:1420:112233445566", false),
            "ble:apple:1420:11****66"
        );
        assert_eq!(
            redact_device_id("ble:apple:1420:addr0", false),
            "ble:apple:1420:addr0"
        );
        assert_eq!(
            redact_device_id("ble:apple:1420:112233445566", true),
            "ble:apple:1420:112233445566"
        );

        // HID serial / container tag / slot suffix
        let hid_sn = redact_device_id("hid:24AE:4105:SECRET_SN_987654", false);
        assert!(!hid_sn.contains("SECRET_SN_987654"));
        assert_eq!(hid_sn, "hid:24AE:4105:<redacted:16>");
        assert_eq!(
            redact_device_id("hid:24AE:4105:SECRET_SN_987654", true),
            "hid:24AE:4105:SECRET_SN_987654"
        );

        let hid_slot = redact_device_id(
            "hid:046D:C548:c:00112233445566778899AABBCCDDEEFF:slot1",
            false,
        );
        assert!(!hid_slot.contains("00112233445566778899AABBCCDDEEFF"));
        assert_eq!(hid_slot, "hid:046D:C548:c:<redacted:32>:slot1");

        // Slot-only HID and XInput slot stay intact
        assert_eq!(
            redact_device_id("hid:046D:C548:slot1", false),
            "hid:046D:C548:slot1"
        );
        assert_eq!(redact_device_id("xinput:slot0", false), "xinput:slot0");
        assert_eq!(
            redact_device_id("xinput:mac:AABBCCDDEEFF", false),
            "xinput:mac:AA****FF"
        );
    }

    #[test]
    fn format_wireless_poll_redacts_by_default_with_hint_and_shows_full_in_verbose() {
        let devices = vec![
            mk_device(
                "hid:24AE:4105:SECRET_SN_987654",
                "Rapoo VT3",
                Transport::Wireless24G,
            ),
            mk_device(
                "bt:001122334455",
                "Xbox Wireless Controller",
                Transport::Bluetooth,
            ),
        ];

        let default_out = format_wireless_poll(&devices, false);
        assert!(default_out.contains("--verbose"));
        assert!(!default_out.contains("SECRET_SN_987654"));
        assert!(!default_out.contains("001122334455"));
        assert!(default_out.contains("hid:24AE:4105:<redacted:16>"));
        assert!(default_out.contains("bt:00****55"));

        let verbose_out = format_wireless_poll(&devices, true);
        assert!(!verbose_out.contains("--verbose"));
        assert!(verbose_out.contains("SECRET_SN_987654"));
        assert!(verbose_out.contains("001122334455"));
    }

    #[test]
    fn format_hardware_telemetry_renders_na_when_invalid_and_separates_acpi() {
        let m = HardwareMetrics {
            acpi_temp_c: Some(46.5),
            ..HardwareMetrics::unavailable()
        };

        let text = format_hardware_telemetry(&m);
        let cpu_line = text
            .lines()
            .find(|l| l.trim_start().starts_with("CPU:"))
            .expect("CPU line");
        let acpi_line = text
            .lines()
            .find(|l| l.trim_start().starts_with("ACPI Thermal:"))
            .expect("ACPI Thermal line");
        let ram_line = text
            .lines()
            .find(|l| l.trim_start().starts_with("RAM:"))
            .expect("RAM line");
        let nic_line = text
            .lines()
            .find(|l| l.trim_start().starts_with("NIC:"))
            .expect("NIC line");
        let net_line = text
            .lines()
            .find(|l| l.trim_start().starts_with("NET:"))
            .expect("NET line");

        assert!(cpu_line.contains("N/A @ N/A"));
        assert!(cpu_line.contains("CPU Temp: N/A"));
        assert!(!cpu_line.contains("0.0%"));
        assert!(acpi_line.contains("46.5 C"));
        assert!(ram_line.ends_with("N/A"));
        assert!(!ram_line.contains("0.0%"));
        assert!(nic_line.ends_with("N/A"));
        assert!(net_line.contains("UP N/A | DOWN N/A"));
        assert!(!net_line.contains("0B"));
    }

    #[test]
    fn format_hardware_telemetry_renders_valid_metrics_and_nic_aggregation_semantics() {
        let m = HardwareMetrics {
            cpu_usage_pct: 12.3,
            cpu_usage_valid: true,
            cpu_freq_ghz: Some(4.25),
            cpu_temp_c: Some(58.0),
            acpi_temp_c: Some(41.2),
            mem_usage_pct: 45.6,
            mem_used_gb: 14.59,
            mem_total_gb: 32.00,
            mem_valid: true,
            net_up_bps: 1024.0,
            net_down_bps: 2048.0,
            net_adapter_name: "Intel(R) Wi-Fi 6E AX211".to_string(),
            net_valid: true,
            ..Default::default()
        };

        let text = format_hardware_telemetry(&m);
        assert!(text.contains("12.3% @ 4.25 GHz | CPU Temp: 58.0 C"));
        assert!(text.contains("ACPI Thermal: 41.2 C"));
        assert!(text.contains("45.6% (14.59 / 32.00 GB)"));
        assert!(text.contains("Intel(R) Wi-Fi 6E AX211 (active)"));
        assert!(text.contains("active NIC: Intel(R) Wi-Fi 6E AX211; total across physical NICs"));

        let no_nic = HardwareMetrics {
            net_valid: true,
            net_adapter_name: String::new(),
            ..HardwareMetrics::unavailable()
        };
        let no_nic_text = format_hardware_telemetry(&no_nic);
        assert!(no_nic_text.contains("[total across physical NICs]"));
    }
}
