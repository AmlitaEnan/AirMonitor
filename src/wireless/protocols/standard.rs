use windows::core::PCWSTR;
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetPreparsedData, HidP_Feature, HidP_GetButtonCaps, HidP_GetCaps,
    HidP_GetUsageValue, HidP_GetUsages, HidP_GetValueCaps, HidP_Input, HIDP_BUTTON_CAPS, HIDP_CAPS,
    HIDP_REPORT_TYPE, HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

use super::{format_hex, BatteryReading, HidInterfaceInfo, HidIo};

const USAGE_PAGE_GENERIC_CONTROLS: u16 = 0x0006;
const USAGE_PAGE_BATTERY_SYSTEM: u16 = 0x0085;
const USAGE_BATTERY_STRENGTH: u16 = 0x0020;
const USAGE_CHARGING: u16 = 0x0044;
const USAGE_RELATIVE_STATE_OF_CHARGE: u16 = 0x0064;
const USAGE_ABSOLUTE_STATE_OF_CHARGE: u16 = 0x0065;
const USAGE_REMAINING_CAPACITY: u16 = 0x0066;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BatteryValueCap {
    report_type: HIDP_REPORT_TYPE,
    report_id: u8,
    usage_page: u16,
    usage: u16,
    link_collection: u16,
    logical_min: i32,
    logical_max: i32,
    priority: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChargingCap {
    Button {
        report_type: HIDP_REPORT_TYPE,
        report_id: u8,
        usage_page: u16,
        usage: u16,
        link_collection: u16,
    },
    Value {
        report_type: HIDP_REPORT_TYPE,
        report_id: u8,
        usage_page: u16,
        usage: u16,
        link_collection: u16,
    },
}

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0 .0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct PreparsedGuard(PHIDP_PREPARSED_DATA);

impl Drop for PreparsedGuard {
    fn drop(&mut self) {
        if self.0 .0 != 0 {
            unsafe {
                let _ = HidD_FreePreparsedData(self.0);
            }
        }
    }
}

pub fn matches(info: &HidInterfaceInfo) -> bool {
    info.usage_page == USAGE_PAGE_BATTERY_SYSTEM
        || (info.usage_page == USAGE_PAGE_GENERIC_CONTROLS && info.usage == USAGE_BATTERY_STRENGTH)
}

pub fn probe(io: &dyn HidIo) -> Option<BatteryReading> {
    let info = io.info();
    if !matches(info) || info.path.is_empty() {
        return None;
    }

    let wide: Vec<u16> = info.path.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
        .ok()?
    };
    if handle.is_invalid() {
        return None;
    }
    let _handle_guard = HandleGuard(handle);

    let mut preparsed = PHIDP_PREPARSED_DATA::default();
    if !unsafe { HidD_GetPreparsedData(handle, &mut preparsed).as_bool() } {
        return None;
    }
    let _preparsed_guard = PreparsedGuard(preparsed);

    let mut caps = HIDP_CAPS::default();
    if unsafe { HidP_GetCaps(preparsed, &mut caps) } != HIDP_STATUS_SUCCESS {
        return None;
    }

    let mut battery_caps = Vec::new();
    let mut charging_caps = Vec::new();

    collect_caps_for_type(
        preparsed,
        HidP_Feature,
        caps.NumberFeatureValueCaps,
        caps.NumberFeatureButtonCaps,
        &mut battery_caps,
        &mut charging_caps,
    );
    collect_caps_for_type(
        preparsed,
        HidP_Input,
        caps.NumberInputValueCaps,
        caps.NumberInputButtonCaps,
        &mut battery_caps,
        &mut charging_caps,
    );

    if battery_caps.is_empty() {
        return None;
    }

    // Prefer RemainingCapacity (0x0066), then feature reports over input reports
    battery_caps.sort_by_key(|c| {
        let rt_order = if c.report_type == HidP_Feature {
            0u8
        } else {
            1u8
        };
        (c.priority, rt_order, c.report_id)
    });

    let mut raw_packets = Vec::new();

    for cap in &battery_caps {
        if cap.report_type == HidP_Feature && info.feature_len > 0 {
            if let Ok(buf) = io.get_feature(cap.report_id) {
                if buf.iter().any(|&b| b != 0) {
                    raw_packets.push(format!(
                        "FEAT[0x{:02X}]: {}",
                        cap.report_id,
                        format_hex(&buf)
                    ));
                    if let Some(pct) = extract_battery_pct(preparsed, cap, &buf) {
                        let charging = extract_charging_state(
                            preparsed,
                            &charging_caps,
                            HidP_Feature,
                            cap.report_id,
                            &buf,
                        );
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging,
                            online: true,
                            detail_note: Some("HID Power Device (Feature)".into()),
                            raw_packets,
                        });
                    }
                }
            }
        } else if cap.report_type == HidP_Input && info.input_len > 0 {
            if let Ok(buf) = io.get_input_report(cap.report_id) {
                if buf.iter().any(|&b| b != 0) {
                    raw_packets.push(format!("IN[0x{:02X}]: {}", cap.report_id, format_hex(&buf)));
                    if let Some(pct) = extract_battery_pct(preparsed, cap, &buf) {
                        let charging = extract_charging_state(
                            preparsed,
                            &charging_caps,
                            HidP_Input,
                            cap.report_id,
                            &buf,
                        );
                        return Some(BatteryReading {
                            battery_pct: Some(pct),
                            charging,
                            online: true,
                            detail_note: Some("HID Power Device (Input)".into()),
                            raw_packets,
                        });
                    }
                }
            }
            if io.can_rw() {
                if let Ok(buf) = io.read_input(150) {
                    if buf.iter().any(|&b| b != 0) {
                        raw_packets.push(format!("READ: {}", format_hex(&buf)));
                        if let Some(pct) = extract_battery_pct(preparsed, cap, &buf) {
                            let charging = extract_charging_state(
                                preparsed,
                                &charging_caps,
                                HidP_Input,
                                cap.report_id,
                                &buf,
                            );
                            return Some(BatteryReading {
                                battery_pct: Some(pct),
                                charging,
                                online: true,
                                detail_note: Some("HID Power Device (Stream)".into()),
                                raw_packets,
                            });
                        }
                    }
                }
            }
        }
    }

    None
}

fn collect_caps_for_type(
    preparsed: PHIDP_PREPARSED_DATA,
    report_type: HIDP_REPORT_TYPE,
    value_cap_count: u16,
    button_cap_count: u16,
    battery_caps: &mut Vec<BatteryValueCap>,
    charging_caps: &mut Vec<ChargingCap>,
) {
    if value_cap_count > 0 {
        let mut count = value_cap_count;
        let mut vcaps = vec![HIDP_VALUE_CAPS::default(); count as usize];
        let status =
            unsafe { HidP_GetValueCaps(report_type, vcaps.as_mut_ptr(), &mut count, preparsed) };
        if status == HIDP_STATUS_SUCCESS {
            for vc in &vcaps[..count as usize] {
                if let Some((usage, priority)) = match_battery_value_usage(vc) {
                    battery_caps.push(BatteryValueCap {
                        report_type,
                        report_id: vc.ReportID,
                        usage_page: vc.UsagePage,
                        usage,
                        link_collection: vc.LinkCollection,
                        logical_min: vc.LogicalMin,
                        logical_max: vc.LogicalMax,
                        priority,
                    });
                }
                if value_cap_contains_usage(vc, USAGE_PAGE_BATTERY_SYSTEM, USAGE_CHARGING) {
                    charging_caps.push(ChargingCap::Value {
                        report_type,
                        report_id: vc.ReportID,
                        usage_page: USAGE_PAGE_BATTERY_SYSTEM,
                        usage: USAGE_CHARGING,
                        link_collection: vc.LinkCollection,
                    });
                }
            }
        }
    }

    if button_cap_count > 0 {
        let mut count = button_cap_count;
        let mut bcaps = vec![HIDP_BUTTON_CAPS::default(); count as usize];
        let status =
            unsafe { HidP_GetButtonCaps(report_type, bcaps.as_mut_ptr(), &mut count, preparsed) };
        if status == HIDP_STATUS_SUCCESS {
            for bc in &bcaps[..count as usize] {
                if button_cap_contains_usage(bc, USAGE_PAGE_BATTERY_SYSTEM, USAGE_CHARGING) {
                    charging_caps.push(ChargingCap::Button {
                        report_type,
                        report_id: bc.ReportID,
                        usage_page: USAGE_PAGE_BATTERY_SYSTEM,
                        usage: USAGE_CHARGING,
                        link_collection: bc.LinkCollection,
                    });
                }
            }
        }
    }
}

fn match_battery_value_usage(vc: &HIDP_VALUE_CAPS) -> Option<(u16, u8)> {
    if vc.UsagePage == USAGE_PAGE_BATTERY_SYSTEM {
        if value_cap_contains_usage(vc, USAGE_PAGE_BATTERY_SYSTEM, USAGE_REMAINING_CAPACITY) {
            return Some((USAGE_REMAINING_CAPACITY, 0));
        }
        if value_cap_contains_usage(
            vc,
            USAGE_PAGE_BATTERY_SYSTEM,
            USAGE_ABSOLUTE_STATE_OF_CHARGE,
        ) {
            return Some((USAGE_ABSOLUTE_STATE_OF_CHARGE, 1));
        }
        if value_cap_contains_usage(
            vc,
            USAGE_PAGE_BATTERY_SYSTEM,
            USAGE_RELATIVE_STATE_OF_CHARGE,
        ) {
            return Some((USAGE_RELATIVE_STATE_OF_CHARGE, 2));
        }
    } else if vc.UsagePage == USAGE_PAGE_GENERIC_CONTROLS
        && value_cap_contains_usage(vc, USAGE_PAGE_GENERIC_CONTROLS, USAGE_BATTERY_STRENGTH)
    {
        return Some((USAGE_BATTERY_STRENGTH, 0));
    }
    None
}

fn value_cap_contains_usage(vc: &HIDP_VALUE_CAPS, usage_page: u16, usage: u16) -> bool {
    if vc.UsagePage != usage_page {
        return false;
    }
    unsafe {
        if vc.IsRange.as_bool() {
            vc.Anonymous.Range.UsageMin <= usage && usage <= vc.Anonymous.Range.UsageMax
        } else {
            vc.Anonymous.NotRange.Usage == usage
        }
    }
}

fn button_cap_contains_usage(bc: &HIDP_BUTTON_CAPS, usage_page: u16, usage: u16) -> bool {
    if bc.UsagePage != usage_page {
        return false;
    }
    unsafe {
        if bc.IsRange.as_bool() {
            bc.Anonymous.Range.UsageMin <= usage && usage <= bc.Anonymous.Range.UsageMax
        } else {
            bc.Anonymous.NotRange.Usage == usage
        }
    }
}

fn extract_battery_pct(
    preparsed: PHIDP_PREPARSED_DATA,
    cap: &BatteryValueCap,
    report: &[u8],
) -> Option<u8> {
    if cap.report_id != 0 && report.first().copied() != Some(cap.report_id) {
        return None;
    }
    let mut raw_val = 0u32;
    let status = unsafe {
        HidP_GetUsageValue(
            cap.report_type,
            cap.usage_page,
            cap.link_collection,
            cap.usage,
            &mut raw_val,
            preparsed,
            report,
        )
    };
    if status != HIDP_STATUS_SUCCESS {
        return None;
    }
    scale_hid_value(raw_val, cap.logical_min, cap.logical_max)
}

fn extract_charging_state(
    preparsed: PHIDP_PREPARSED_DATA,
    charging_caps: &[ChargingCap],
    report_type: HIDP_REPORT_TYPE,
    report_id: u8,
    report: &[u8],
) -> bool {
    for cap in charging_caps {
        match *cap {
            ChargingCap::Value {
                report_type: ct,
                report_id: cid,
                usage_page,
                usage,
                link_collection,
            } if ct == report_type && cid == report_id => {
                let mut val = 0u32;
                let status = unsafe {
                    HidP_GetUsageValue(
                        ct,
                        usage_page,
                        link_collection,
                        usage,
                        &mut val,
                        preparsed,
                        report,
                    )
                };
                if status == HIDP_STATUS_SUCCESS {
                    return val != 0;
                }
            }
            ChargingCap::Button {
                report_type: ct,
                report_id: cid,
                usage_page,
                usage,
                link_collection,
            } if ct == report_type && cid == report_id => {
                let mut usages = [0u16; 16];
                let mut len = usages.len() as u32;
                let mut report_copy = report.to_vec();
                let status = unsafe {
                    HidP_GetUsages(
                        ct,
                        usage_page,
                        link_collection,
                        usages.as_mut_ptr(),
                        &mut len,
                        preparsed,
                        &mut report_copy,
                    )
                };
                if status == HIDP_STATUS_SUCCESS && usages[..len as usize].contains(&usage) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

pub fn scale_hid_value(raw_val: u32, logical_min: i32, logical_max: i32) -> Option<u8> {
    let val = raw_val as i64;
    let min = logical_min as i64;
    let max = logical_max as i64;
    if max > min {
        if val < min || val > max {
            return None;
        }
        if min == 0 && max == 100 {
            return Some(val as u8);
        }
        let pct = ((val - min) * 100) / (max - min);
        Some(pct.clamp(0, 100) as u8)
    } else if min == 0 && max == 0 && raw_val <= 100 {
        Some(raw_val as u8)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scale_hid_value() {
        assert_eq!(scale_hid_value(75, 0, 100), Some(75));
        assert_eq!(scale_hid_value(0, 0, 100), Some(0));
        assert_eq!(scale_hid_value(101, 0, 100), None);
        assert_eq!(scale_hid_value(128, 0, 255), Some(50));
        assert_eq!(scale_hid_value(3, 0, 4), Some(75));
    }

    #[test]
    fn test_standard_probe_without_preparsed_path_returns_none() {
        struct EmptyPathIo(HidInterfaceInfo);
        impl HidIo for EmptyPathIo {
            fn info(&self) -> &HidInterfaceInfo {
                &self.0
            }
            fn can_rw(&self) -> bool {
                false
            }
            fn write_output(&self, _: &[u8]) -> Result<(), String> {
                Err("ro".into())
            }
            fn read_input(&self, _: u32) -> Result<Vec<u8>, String> {
                Err("ro".into())
            }
            fn get_feature(&self, _: u8) -> Result<Vec<u8>, String> {
                Ok(vec![0x01, 0x42])
            }
            fn get_input_report(&self, _: u8) -> Result<Vec<u8>, String> {
                Ok(vec![0x01, 0x42])
            }
            fn set_feature(&self, _: &[u8]) -> Result<(), String> {
                Err("ro".into())
            }
        }

        // Must NOT guess buf[1] == 0x42 (66%) without HidP_GetValueCaps RemainingCapacity
        let io = EmptyPathIo(HidInterfaceInfo {
            path: String::new(),
            vid: 0x046D,
            pid: 0x1234,
            usage_page: 0x0085,
            usage: 0x0000,
            input_len: 4,
            output_len: 0,
            feature_len: 4,
            manufacturer: String::new(),
            product: String::new(),
            serial: String::new(),
            interface_tag: String::new(),
        });
        assert!(probe(&io).is_none());
    }
}
