use std::time::Instant;

use windows::core::{s, w};
use windows::Win32::Foundation::{FreeLibrary, ERROR_SUCCESS, HANDLE, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

use crate::state::{DeviceKind, Transport, WirelessDevice};

const BATTERY_DEVTYPE_GAMEPAD: u8 = 0x00;

const BATTERY_TYPE_DISCONNECTED: u8 = 0x00;
const BATTERY_TYPE_WIRED: u8 = 0x01;
const BATTERY_TYPE_ALKALINE: u8 = 0x02;
const BATTERY_TYPE_NIMH: u8 = 0x03;
const BATTERY_TYPE_UNKNOWN: u8 = 0xFF;

const BATTERY_LEVEL_EMPTY: u8 = 0x00;
const BATTERY_LEVEL_LOW: u8 = 0x01;
const BATTERY_LEVEL_MEDIUM: u8 = 0x02;
const BATTERY_LEVEL_FULL: u8 = 0x03;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct XInputGamepad {
    w_buttons: u16,
    b_left_trigger: u8,
    b_right_trigger: u8,
    s_thumb_lx: i16,
    s_thumb_ly: i16,
    s_thumb_rx: i16,
    s_thumb_ry: i16,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct XInputState {
    dw_packet_number: u32,
    gamepad: XInputGamepad,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct XInputBatteryInformation {
    battery_type: u8,
    battery_level: u8,
}

type FnXInputGetState = unsafe extern "system" fn(u32, *mut XInputState) -> u32;
type FnXInputGetBatteryInformation =
    unsafe extern "system" fn(u32, u8, *mut XInputBatteryInformation) -> u32;

pub struct XInputMonitor {
    module: HMODULE,
    dll_name: &'static str,
    get_state: Option<FnXInputGetState>,
    get_battery: Option<FnXInputGetBatteryInformation>,
}

// XInput DLL function pointers are process-wide and thread-safe to invoke across slots.
unsafe impl Send for XInputMonitor {}
unsafe impl Sync for XInputMonitor {}

impl XInputMonitor {
    pub fn new() -> Self {
        unsafe {
            let (module, dll_name) = match LoadLibraryExW(
                w!("xinput1_4.dll"),
                HANDLE::default(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            ) {
                Ok(h) if !h.is_invalid() => (h, "xinput1_4.dll"),
                _ => match LoadLibraryExW(
                    w!("xinput1_3.dll"),
                    HANDLE::default(),
                    LOAD_LIBRARY_SEARCH_SYSTEM32,
                ) {
                    Ok(h) if !h.is_invalid() => (h, "xinput1_3.dll"),
                    _ => (HMODULE::default(), "none"),
                },
            };

            if module.is_invalid() || module.0.is_null() {
                return Self {
                    module: HMODULE::default(),
                    dll_name: "none",
                    get_state: None,
                    get_battery: None,
                };
            }

            let get_state = GetProcAddress(module, s!("XInputGetState"))
                .map(|p| std::mem::transmute::<_, FnXInputGetState>(p));
            let get_battery = GetProcAddress(module, s!("XInputGetBatteryInformation"))
                .map(|p| std::mem::transmute::<_, FnXInputGetBatteryInformation>(p));

            Self {
                module,
                dll_name,
                get_state,
                get_battery,
            }
        }
    }

    pub fn poll(&self) -> Vec<WirelessDevice> {
        let mut devices = Vec::new();
        let Some(get_state) = self.get_state else {
            return devices;
        };

        for slot in 0u32..4 {
            let mut state = XInputState::default();
            let rc = unsafe { get_state(slot, &mut state) };
            if rc != ERROR_SUCCESS.0 {
                continue;
            }

            let (battery_pct, charging, detail_note) = self.query_battery(slot);
            devices.push(WirelessDevice {
                id: format!("xinput:slot{}", slot),
                name: format!("Xbox Controller #{}", slot + 1),
                kind: DeviceKind::Gamepad,
                transport: Transport::XboxWireless,
                battery_pct,
                charging,
                sub_batteries: None,
                online: true,
                last_seen: Instant::now(),
                detail_note,
            });
        }

        devices
    }

    pub fn diagnostics(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("[XInput] Loaded DLL: {}\n", self.dll_name));
        let Some(get_state) = self.get_state else {
            out.push_str("  XInputGetState unavailable.\n");
            return out;
        };

        let mut any = false;
        for slot in 0u32..4 {
            let mut state = XInputState::default();
            let rc = unsafe { get_state(slot, &mut state) };
            if rc == ERROR_SUCCESS.0 {
                any = true;
                let (pct, charging, note) = self.query_battery(slot);
                out.push_str(&format!(
                    "  Slot {}: connected, packet={}, battery={:?}, charging={}, note={:?}\n",
                    slot, state.dw_packet_number, pct, charging, note
                ));
            }
        }
        if !any {
            out.push_str("  No active XInput controllers in slots 0..3.\n");
        }
        out
    }

    fn query_battery(&self, slot: u32) -> (Option<u8>, bool, Option<String>) {
        let Some(get_battery) = self.get_battery else {
            return (
                None,
                false,
                Some("XInputGetBatteryInformation unavailable".into()),
            );
        };

        let mut info = XInputBatteryInformation::default();
        let rc = unsafe { get_battery(slot, BATTERY_DEVTYPE_GAMEPAD, &mut info) };
        if rc != ERROR_SUCCESS.0 {
            return (None, false, None);
        }

        interpret_battery_info(info)
    }
}

impl Default for XInputMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for XInputMonitor {
    fn drop(&mut self) {
        if !self.module.is_invalid() && !self.module.0.is_null() {
            unsafe {
                let _ = FreeLibrary(self.module);
            }
        }
    }
}

fn interpret_battery_info(info: XInputBatteryInformation) -> (Option<u8>, bool, Option<String>) {
    match info.battery_type {
        BATTERY_TYPE_DISCONNECTED => (None, false, Some("Battery disconnected".into())),
        BATTERY_TYPE_WIRED => (None, false, Some("Wired USB (external power)".into())),
        BATTERY_TYPE_UNKNOWN => (None, false, Some("Battery level unknown".into())),
        BATTERY_TYPE_ALKALINE | BATTERY_TYPE_NIMH => {
            let chem = if info.battery_type == BATTERY_TYPE_ALKALINE {
                "Alkaline"
            } else {
                "NiMH"
            };
            let pct = map_battery_level(info.battery_level);
            let note = if pct.is_some() {
                format!(
                    "{} (Level: {}, approx.)",
                    chem,
                    level_label(info.battery_level)
                )
            } else {
                format!("{} (Level: Unknown)", chem)
            };
            (pct, false, Some(note))
        }
        other => {
            let pct = map_battery_level(info.battery_level);
            let note = if pct.is_some() {
                format!(
                    "Type 0x{:02X} (Level: {}, approx.)",
                    other,
                    level_label(info.battery_level)
                )
            } else {
                format!("Type 0x{:02X} (Level: Unknown)", other)
            };
            (pct, false, Some(note))
        }
    }
}

fn map_battery_level(level: u8) -> Option<u8> {
    match level {
        BATTERY_LEVEL_EMPTY => Some(5),
        BATTERY_LEVEL_LOW => Some(25),
        BATTERY_LEVEL_MEDIUM => Some(65),
        BATTERY_LEVEL_FULL => Some(100),
        _ => None,
    }
}

fn level_label(level: u8) -> &'static str {
    match level {
        BATTERY_LEVEL_EMPTY => "Empty",
        BATTERY_LEVEL_LOW => "Low",
        BATTERY_LEVEL_MEDIUM => "Medium",
        BATTERY_LEVEL_FULL => "Full",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wired_controller_returns_none_with_note() {
        let info = XInputBatteryInformation {
            battery_type: BATTERY_TYPE_WIRED,
            battery_level: BATTERY_LEVEL_FULL,
        };
        let (pct, charging, note) = interpret_battery_info(info);
        assert_eq!(pct, None);
        assert!(!charging);
        assert!(note.as_deref().unwrap_or("").contains("Wired"));
    }

    #[test]
    fn disconnected_and_unknown_type_return_none() {
        let disconnected = interpret_battery_info(XInputBatteryInformation {
            battery_type: BATTERY_TYPE_DISCONNECTED,
            battery_level: BATTERY_LEVEL_EMPTY,
        });
        assert_eq!(disconnected.0, None);
        assert!(disconnected
            .2
            .as_deref()
            .unwrap_or("")
            .contains("disconnected"));

        let unknown_type = interpret_battery_info(XInputBatteryInformation {
            battery_type: BATTERY_TYPE_UNKNOWN,
            battery_level: BATTERY_LEVEL_FULL,
        });
        assert_eq!(unknown_type.0, None);
        assert!(unknown_type.2.as_deref().unwrap_or("").contains("unknown"));
    }

    #[test]
    fn unknown_battery_level_never_fabricates_50() {
        let info = XInputBatteryInformation {
            battery_type: BATTERY_TYPE_ALKALINE,
            battery_level: 0x42,
        };
        let (pct, charging, note) = interpret_battery_info(info);
        assert_eq!(pct, None);
        assert!(!charging);
        assert!(note.as_deref().unwrap_or("").contains("Unknown"));

        let other_unknown = interpret_battery_info(XInputBatteryInformation {
            battery_type: 0x04,
            battery_level: 0xFF,
        });
        assert_eq!(other_unknown.0, None);
        assert!(other_unknown.2.as_deref().unwrap_or("").contains("Unknown"));
    }

    #[test]
    fn discrete_levels_include_tier_note() {
        let cases = [
            (
                BATTERY_TYPE_ALKALINE,
                BATTERY_LEVEL_EMPTY,
                5,
                "Alkaline",
                "Empty",
            ),
            (
                BATTERY_TYPE_ALKALINE,
                BATTERY_LEVEL_LOW,
                25,
                "Alkaline",
                "Low",
            ),
            (
                BATTERY_TYPE_NIMH,
                BATTERY_LEVEL_MEDIUM,
                65,
                "NiMH",
                "Medium",
            ),
            (BATTERY_TYPE_NIMH, BATTERY_LEVEL_FULL, 100, "NiMH", "Full"),
        ];
        for (btype, level, expected_pct, chem, label) in cases {
            let (pct, charging, note) = interpret_battery_info(XInputBatteryInformation {
                battery_type: btype,
                battery_level: level,
            });
            assert_eq!(pct, Some(expected_pct));
            assert!(!charging);
            let note_str = note.expect("expected detail note");
            assert!(note_str.contains(chem), "note missing {chem}: {note_str}");
            assert!(note_str.contains(label), "note missing {label}: {note_str}");
            assert!(
                note_str.contains("Level:"),
                "note missing tier qualifier: {note_str}"
            );
        }
    }
}
