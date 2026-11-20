use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

const BATTERY_FLAG_CHARGING: u8 = 8;
const BATTERY_FLAG_NO_SYSTEM_BATTERY: u8 = 128;
const BATTERY_PERCENT_UNKNOWN: u8 = 255;

pub fn sample_system_battery() -> (Option<u8>, bool) {
    let mut status = SYSTEM_POWER_STATUS::default();
    let ok = unsafe { GetSystemPowerStatus(&mut status).is_ok() };
    if !ok {
        return (None, false);
    }
    parse_system_power_status(&status)
}

pub fn battery_diagnostics() -> String {
    let mut status = SYSTEM_POWER_STATUS::default();
    match unsafe { GetSystemPowerStatus(&mut status) } {
        Ok(()) => {
            let (pct, charging) = parse_system_power_status(&status);
            let pct_str = pct
                .map(|v| format!("{v}%"))
                .unwrap_or_else(|| "None".to_string());
            format!(
                "SystemBattery(GetSystemPowerStatus): acl_line={}, flag=0x{:02X}, raw_pct={}, parsed=({}, charging={})",
                status.ACLineStatus, status.BatteryFlag, status.BatteryLifePercent, pct_str, charging
            )
        }
        Err(err) => format!("SystemBattery(GetSystemPowerStatus): failed ({err})"),
    }
}

fn parse_system_power_status(status: &SYSTEM_POWER_STATUS) -> (Option<u8>, bool) {
    if (status.BatteryFlag & BATTERY_FLAG_NO_SYSTEM_BATTERY) != 0
        || status.BatteryLifePercent == BATTERY_PERCENT_UNKNOWN
        || status.BatteryLifePercent > 100
    {
        return (None, false);
    }
    let charging = (status.BatteryFlag & BATTERY_FLAG_CHARGING) != 0;
    (Some(status.BatteryLifePercent), charging)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_with(flag: u8, pct: u8, ac_line: u8) -> SYSTEM_POWER_STATUS {
        SYSTEM_POWER_STATUS {
            ACLineStatus: ac_line,
            BatteryFlag: flag,
            BatteryLifePercent: pct,
            ..Default::default()
        }
    }

    #[test]
    fn no_system_battery_flag_returns_none_false() {
        let st = status_with(BATTERY_FLAG_NO_SYSTEM_BATTERY, 100, 1);
        assert_eq!(parse_system_power_status(&st), (None, false));

        // Combined flag including bit 128 and charging bit 8 must still yield (None, false)
        let st2 = status_with(
            BATTERY_FLAG_NO_SYSTEM_BATTERY | BATTERY_FLAG_CHARGING,
            80,
            1,
        );
        assert_eq!(parse_system_power_status(&st2), (None, false));

        // Unknown flag (255 = 0xFF) also has bit 128 set
        let st3 = status_with(255, 50, 255);
        assert_eq!(parse_system_power_status(&st3), (None, false));
    }

    #[test]
    fn unknown_battery_percent_returns_none_false() {
        let st = status_with(1, BATTERY_PERCENT_UNKNOWN, 0);
        assert_eq!(parse_system_power_status(&st), (None, false));

        let st_charging = status_with(BATTERY_FLAG_CHARGING, BATTERY_PERCENT_UNKNOWN, 1);
        assert_eq!(parse_system_power_status(&st_charging), (None, false));

        let st_oob = status_with(1, 101, 0);
        assert_eq!(parse_system_power_status(&st_oob), (None, false));
    }

    #[test]
    fn valid_battery_and_charging_state_are_parsed() {
        let discharging = status_with(1, 72, 0);
        assert_eq!(parse_system_power_status(&discharging), (Some(72), false));

        let charging = status_with(1 | BATTERY_FLAG_CHARGING, 45, 1);
        assert_eq!(parse_system_power_status(&charging), (Some(45), true));

        let zero_pct = status_with(4 | BATTERY_FLAG_CHARGING, 0, 1);
        assert_eq!(parse_system_power_status(&zero_pct), (Some(0), true));

        let full_pct = status_with(1, 100, 1);
        assert_eq!(parse_system_power_status(&full_pct), (Some(100), false));
    }

    #[test]
    fn live_sample_and_diagnostics_are_well_formed() {
        let (pct, charging) = sample_system_battery();
        if let Some(p) = pct {
            assert!(p <= 100);
        } else {
            assert!(!charging);
        }
        let diag = battery_diagnostics();
        assert!(diag.starts_with("SystemBattery(GetSystemPowerStatus):"));
    }
}
