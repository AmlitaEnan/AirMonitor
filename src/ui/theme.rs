use std::mem::size_of;

use windows::core::w;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Gdi::{
    GetSysColor, COLOR_BTNTEXT, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOW,
    COLOR_WINDOWTEXT,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

use crate::config::HwMetric;
use crate::state::{HardwareMetrics, WirelessDevice};

const HCF_HIGHCONTRASTON: u32 = 0x0000_0001;

#[repr(C)]
struct HighContrastW {
    cb_size: u32,
    dw_flags: u32,
    lpsz_default_scheme: *mut u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighContrastColors {
    pub dark: bool,
    pub bg: (u8, u8, u8),
    pub fg: (u8, u8, u8),
    pub accent: (u8, u8, u8),
    pub accent_fg: (u8, u8, u8),
    pub muted: (u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiTheme {
    Dark,
    Light,
    HighContrast(HighContrastColors),
}

pub type TaskbarTheme = UiTheme;

impl UiTheme {
    pub fn is_high_contrast(&self) -> bool {
        matches!(self, Self::HighContrast(_))
    }

    pub fn is_dark(&self) -> bool {
        match self {
            Self::Dark => true,
            Self::Light => false,
            Self::HighContrast(hc) => hc.dark,
        }
    }
}

#[inline]
pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

/// Converts 8-bit sRGB channels into a Win32 `COLORREF` (`0x00BBGGRR`).
#[inline]
pub const fn rgb_to_colorref(r: u8, g: u8, b: u8) -> u32 {
    ((b as u32) << 16) | ((g as u32) << 8) | (r as u32)
}

#[inline]
pub const fn colorref_to_rgb(colorref: u32) -> (u8, u8, u8) {
    (
        (colorref & 0xFF) as u8,
        ((colorref >> 8) & 0xFF) as u8,
        ((colorref >> 16) & 0xFF) as u8,
    )
}

pub fn srgb_channel_to_linear(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub fn relative_luminance(c: D2D1_COLOR_F) -> f32 {
    0.2126 * srgb_channel_to_linear(c.r)
        + 0.7152 * srgb_channel_to_linear(c.g)
        + 0.0722 * srgb_channel_to_linear(c.b)
}

pub fn composite_over(fg: D2D1_COLOR_F, bg: D2D1_COLOR_F) -> D2D1_COLOR_F {
    let a = fg.a.clamp(0.0, 1.0);
    D2D1_COLOR_F {
        r: fg.r * a + bg.r * (1.0 - a),
        g: fg.g * a + bg.g * (1.0 - a),
        b: fg.b * a + bg.b * (1.0 - a),
        a: 1.0,
    }
}

/// WCAG 2.1 contrast ratio between a foreground color (composited over `bg`) and `bg`.
pub fn contrast_ratio(fg: D2D1_COLOR_F, bg: D2D1_COLOR_F) -> f32 {
    let effective_fg = if fg.a < 1.0 {
        composite_over(fg, bg)
    } else {
        fg
    };
    let l1 = relative_luminance(effective_fg);
    let l2 = relative_luminance(bg);
    let (lighter, darker) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

pub fn is_high_contrast_enabled() -> bool {
    unsafe {
        let mut hc = HighContrastW {
            cb_size: size_of::<HighContrastW>() as u32,
            dw_flags: 0,
            lpsz_default_scheme: std::ptr::null_mut(),
        };
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.cb_size,
            Some(&mut hc as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
            && (hc.dw_flags & HCF_HIGHCONTRASTON) != 0
    }
}

fn read_high_contrast_colors() -> HighContrastColors {
    unsafe {
        let bg = colorref_to_rgb(GetSysColor(COLOR_WINDOW));
        let fg = colorref_to_rgb(GetSysColor(COLOR_WINDOWTEXT));
        let btn_fg = colorref_to_rgb(GetSysColor(COLOR_BTNTEXT));
        let accent = colorref_to_rgb(GetSysColor(COLOR_HIGHLIGHT));
        let accent_fg = colorref_to_rgb(GetSysColor(COLOR_HIGHLIGHTTEXT));
        let gray = colorref_to_rgb(GetSysColor(COLOR_GRAYTEXT));

        let bg_c = rgba(bg.0, bg.1, bg.2, 1.0);
        let dark = relative_luminance(bg_c) < 0.5;
        let fg_resolved = if contrast_ratio(rgba(fg.0, fg.1, fg.2, 1.0), bg_c) >= 4.5 {
            fg
        } else {
            btn_fg
        };
        let muted = if contrast_ratio(rgba(gray.0, gray.1, gray.2, 1.0), bg_c) >= 4.5 {
            gray
        } else {
            fg_resolved
        };

        HighContrastColors {
            dark,
            bg,
            fg: fg_resolved,
            accent,
            accent_fg,
            muted,
        }
    }
}

pub fn read_taskbar_theme() -> UiTheme {
    if is_high_contrast_enabled() {
        return UiTheme::HighContrast(read_high_contrast_colors());
    }

    unsafe {
        let mut hkey = HKEY::default();
        let subkey = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey, 0, KEY_READ, &mut hkey).is_ok() {
            let mut val: u32 = 0;
            let mut size = size_of::<u32>() as u32;
            let mut kind = REG_DWORD;
            let status = RegQueryValueExW(
                hkey,
                w!("SystemUsesLightTheme"),
                None,
                Some(&mut kind.0 as *mut u32 as *mut _),
                Some(&mut val as *mut u32 as *mut u8),
                Some(&mut size),
            );
            let _ = RegCloseKey(hkey);
            if status.is_ok() && val == 1 {
                return UiTheme::Light;
            }
        }
        UiTheme::Dark
    }
}

/// Shared battery/charging status color logic used by both the taskbar bar and popup cards.
pub fn device_status_color(
    dev: &WirelessDevice,
    low_threshold: u8,
    charging: D2D1_COLOR_F,
    warn: D2D1_COLOR_F,
    crit: D2D1_COLOR_F,
    normal: D2D1_COLOR_F,
) -> D2D1_COLOR_F {
    if dev.charging {
        return charging;
    }
    if let Some(pct) = dev.battery_pct {
        if pct <= 10.min(low_threshold) {
            return crit;
        }
        if pct <= low_threshold {
            return warn;
        }
    }
    normal
}

pub struct TaskbarPalette {
    pub hit_bg: D2D1_COLOR_F,
    pub hover_bg: D2D1_COLOR_F,
    pub text_primary: D2D1_COLOR_F,
    pub text_secondary: D2D1_COLOR_F,
    pub up_accent: D2D1_COLOR_F,
    pub down_accent: D2D1_COLOR_F,
    pub divider: D2D1_COLOR_F,
    pub charging: D2D1_COLOR_F,
    pub warn_orange: D2D1_COLOR_F,
    pub crit_red: D2D1_COLOR_F,
}

impl TaskbarPalette {
    pub fn for_theme(theme: UiTheme) -> Self {
        match theme {
            UiTheme::Dark => Self {
                // Alpha ~ 2/255 makes the whole layered bar clickable without visible tint.
                hit_bg: rgba(0, 0, 0, 0.008),
                hover_bg: rgba(255, 255, 255, 0.08),
                text_primary: rgba(244, 244, 245, 0.96),
                text_secondary: rgba(180, 184, 192, 0.92),
                up_accent: rgba(56, 189, 248, 0.95),
                down_accent: rgba(74, 222, 128, 0.95),
                divider: rgba(255, 255, 255, 0.18),
                charging: rgba(74, 222, 128, 0.98),
                warn_orange: rgba(251, 146, 60, 0.98),
                crit_red: rgba(248, 113, 113, 0.98),
            },
            UiTheme::Light => Self {
                hit_bg: rgba(255, 255, 255, 0.008),
                hover_bg: rgba(0, 0, 0, 0.06),
                text_primary: rgba(24, 24, 27, 0.95),
                text_secondary: rgba(82, 82, 91, 0.95),
                up_accent: rgba(2, 132, 199, 0.95),
                down_accent: rgba(22, 163, 74, 0.95),
                divider: rgba(0, 0, 0, 0.15),
                charging: rgba(21, 128, 61, 0.98),
                warn_orange: rgba(234, 88, 12, 0.98),
                crit_red: rgba(220, 38, 38, 0.98),
            },
            UiTheme::HighContrast(hc) => {
                let fg = rgba(hc.fg.0, hc.fg.1, hc.fg.2, 1.0);
                let accent = rgba(hc.accent.0, hc.accent.1, hc.accent.2, 1.0);
                Self {
                    hit_bg: rgba(hc.bg.0, hc.bg.1, hc.bg.2, 0.92),
                    hover_bg: rgba(hc.accent.0, hc.accent.1, hc.accent.2, 0.28),
                    text_primary: fg,
                    text_secondary: fg,
                    up_accent: accent,
                    down_accent: fg,
                    divider: fg,
                    charging: if hc.dark {
                        rgba(74, 222, 128, 1.0)
                    } else {
                        rgba(0, 112, 48, 1.0)
                    },
                    warn_orange: if hc.dark {
                        rgba(255, 196, 64, 1.0)
                    } else {
                        rgba(180, 68, 0, 1.0)
                    },
                    crit_red: if hc.dark {
                        rgba(255, 110, 110, 1.0)
                    } else {
                        rgba(185, 16, 16, 1.0)
                    },
                }
            }
        }
    }

    pub fn device_color(&self, dev: &WirelessDevice, low_threshold: u8) -> D2D1_COLOR_F {
        device_status_color(
            dev,
            low_threshold,
            self.charging,
            self.warn_orange,
            self.crit_red,
            self.text_primary,
        )
    }

    pub fn hw_value_color(
        &self,
        metric: HwMetric,
        hw: &HardwareMetrics,
        low_bat_threshold: u8,
    ) -> D2D1_COLOR_F {
        match metric {
            HwMetric::CpuTemp => temp_alert_color(
                hw.cpu_temp_c,
                80.0,
                90.0,
                self.warn_orange,
                self.crit_red,
                self.text_primary,
            ),
            HwMetric::GpuTemp => temp_alert_color(
                hw.gpu_temp_c,
                80.0,
                90.0,
                self.warn_orange,
                self.crit_red,
                self.text_primary,
            ),
            HwMetric::GpuHotspot => temp_alert_color(
                hw.gpu_hotspot_c,
                90.0,
                100.0,
                self.warn_orange,
                self.crit_red,
                self.text_primary,
            ),
            HwMetric::DiskTemp => temp_alert_color(
                hw.disk_temp_c,
                60.0,
                70.0,
                self.warn_orange,
                self.crit_red,
                self.text_primary,
            ),
            HwMetric::MemUsage => {
                if !hw.mem_usage_pct.is_finite() {
                    self.text_primary
                } else if hw.mem_usage_pct >= 93.0 {
                    self.crit_red
                } else if hw.mem_usage_pct >= 85.0 {
                    self.warn_orange
                } else {
                    self.text_primary
                }
            }
            HwMetric::CpuUsage => {
                if hw.cpu_usage_pct.is_finite() && hw.cpu_usage_pct >= 95.0 {
                    self.warn_orange
                } else {
                    self.text_primary
                }
            }
            HwMetric::GpuUsage => match hw.gpu_usage_pct {
                Some(u) if u.is_finite() && u >= 95.0 => self.warn_orange,
                _ => self.text_primary,
            },
            HwMetric::SysBattery => {
                if hw.sys_battery_charging {
                    self.charging
                } else if let Some(pct) = hw.sys_battery_pct {
                    if pct <= 10.min(low_bat_threshold) {
                        self.crit_red
                    } else if pct <= low_bat_threshold {
                        self.warn_orange
                    } else {
                        self.text_primary
                    }
                } else {
                    self.text_primary
                }
            }
            HwMetric::CpuFreq
            | HwMetric::CpuPower
            | HwMetric::CpuFan
            | HwMetric::NetUp
            | HwMetric::NetDown
            | HwMetric::DiskRead
            | HwMetric::DiskWrite
            | HwMetric::GpuPower
            | HwMetric::GpuVram
            | HwMetric::GpuFan => self.text_primary,
        }
    }
}

#[inline]
fn temp_alert_color(
    val: Option<f32>,
    warn_at: f32,
    crit_at: f32,
    warn: D2D1_COLOR_F,
    crit: D2D1_COLOR_F,
    normal: D2D1_COLOR_F,
) -> D2D1_COLOR_F {
    match val {
        Some(t) if t.is_finite() => {
            if t >= crit_at {
                crit
            } else if t >= warn_at {
                warn
            } else {
                normal
            }
        }
        _ => normal,
    }
}

pub struct PopupPalette {
    pub bg: D2D1_COLOR_F,
    pub card_bg: D2D1_COLOR_F,
    pub card_border: D2D1_COLOR_F,
    pub window_border: D2D1_COLOR_F,
    pub dwm_border_colorref: u32,
    pub pill_bg: D2D1_COLOR_F,
    pub pill_hover: D2D1_COLOR_F,
    pub pill_pressed: D2D1_COLOR_F,
    pub accent: D2D1_COLOR_F,
    pub accent_soft: D2D1_COLOR_F,
    pub accent_hover: D2D1_COLOR_F,
    pub accent_pressed: D2D1_COLOR_F,
    pub accent_text: D2D1_COLOR_F,
    pub text_primary: D2D1_COLOR_F,
    pub text_secondary: D2D1_COLOR_F,
    pub text_muted: D2D1_COLOR_F,
    pub track_bg: D2D1_COLOR_F,
    pub green: D2D1_COLOR_F,
    pub orange: D2D1_COLOR_F,
    pub red: D2D1_COLOR_F,
    pub divider: D2D1_COLOR_F,
}

impl PopupPalette {
    pub fn for_theme(theme: UiTheme) -> Self {
        match theme {
            UiTheme::Dark => {
                let border_rgb = (62, 66, 70);
                Self {
                    bg: rgba(30, 31, 34, 1.0),
                    card_bg: rgba(41, 43, 48, 1.0),
                    card_border: rgba(border_rgb.0, border_rgb.1, border_rgb.2, 1.0),
                    window_border: rgba(76, 80, 88, 1.0),
                    dwm_border_colorref: rgb_to_colorref(border_rgb.0, border_rgb.1, border_rgb.2),
                    pill_bg: rgba(54, 57, 63, 1.0),
                    pill_hover: rgba(70, 74, 82, 1.0),
                    pill_pressed: rgba(84, 89, 99, 1.0),
                    accent: rgba(56, 189, 248, 1.0),
                    accent_soft: rgba(56, 189, 248, 0.16),
                    accent_hover: rgba(76, 200, 252, 1.0),
                    accent_pressed: rgba(36, 162, 218, 1.0),
                    accent_text: rgba(12, 18, 28, 1.0),
                    text_primary: rgba(244, 244, 246, 0.98),
                    text_secondary: rgba(196, 200, 208, 0.96),
                    // WCAG AA >= 4.5:1 against both bg (30,31,34) and card_bg (41,43,48).
                    text_muted: rgba(160, 165, 175, 1.0),
                    track_bg: rgba(24, 25, 28, 1.0),
                    green: rgba(74, 222, 128, 1.0),
                    orange: rgba(251, 146, 60, 1.0),
                    red: rgba(248, 113, 113, 1.0),
                    divider: rgba(255, 255, 255, 0.10),
                }
            }
            UiTheme::Light => {
                let border_rgb = (214, 218, 226);
                Self {
                    bg: rgba(246, 247, 249, 1.0),
                    card_bg: rgba(255, 255, 255, 1.0),
                    card_border: rgba(border_rgb.0, border_rgb.1, border_rgb.2, 1.0),
                    window_border: rgba(196, 201, 210, 1.0),
                    dwm_border_colorref: rgb_to_colorref(border_rgb.0, border_rgb.1, border_rgb.2),
                    pill_bg: rgba(236, 238, 242, 1.0),
                    pill_hover: rgba(222, 225, 232, 1.0),
                    pill_pressed: rgba(204, 208, 218, 1.0),
                    accent: rgba(2, 132, 199, 1.0),
                    accent_soft: rgba(2, 132, 199, 0.12),
                    accent_hover: rgba(3, 118, 178, 1.0),
                    accent_pressed: rgba(2, 98, 150, 1.0),
                    accent_text: rgba(255, 255, 255, 1.0),
                    text_primary: rgba(24, 24, 27, 0.98),
                    text_secondary: rgba(72, 75, 84, 0.96),
                    // WCAG AA >= 4.5:1 against both bg (246,247,249) and card_bg (255,255,255).
                    text_muted: rgba(95, 100, 110, 1.0),
                    track_bg: rgba(228, 230, 236, 1.0),
                    green: rgba(22, 163, 74, 1.0),
                    orange: rgba(234, 88, 12, 1.0),
                    red: rgba(220, 38, 38, 1.0),
                    divider: rgba(0, 0, 0, 0.09),
                }
            }
            UiTheme::HighContrast(hc) => {
                let bg = rgba(hc.bg.0, hc.bg.1, hc.bg.2, 1.0);
                let fg = rgba(hc.fg.0, hc.fg.1, hc.fg.2, 1.0);
                let accent = rgba(hc.accent.0, hc.accent.1, hc.accent.2, 1.0);
                let accent_fg = rgba(hc.accent_fg.0, hc.accent_fg.1, hc.accent_fg.2, 1.0);
                let muted = rgba(hc.muted.0, hc.muted.1, hc.muted.2, 1.0);
                let pill_bg = if hc.dark {
                    rgba(28, 28, 28, 1.0)
                } else {
                    rgba(235, 235, 235, 1.0)
                };
                Self {
                    bg,
                    card_bg: bg,
                    card_border: fg,
                    window_border: fg,
                    dwm_border_colorref: rgb_to_colorref(hc.fg.0, hc.fg.1, hc.fg.2),
                    pill_bg,
                    pill_hover: accent,
                    pill_pressed: fg,
                    accent,
                    accent_soft: rgba(hc.accent.0, hc.accent.1, hc.accent.2, 0.25),
                    accent_hover: accent,
                    accent_pressed: fg,
                    accent_text: accent_fg,
                    text_primary: fg,
                    text_secondary: fg,
                    text_muted: muted,
                    track_bg: pill_bg,
                    green: if hc.dark {
                        rgba(74, 222, 128, 1.0)
                    } else {
                        rgba(0, 112, 48, 1.0)
                    },
                    orange: if hc.dark {
                        rgba(255, 196, 64, 1.0)
                    } else {
                        rgba(180, 68, 0, 1.0)
                    },
                    red: if hc.dark {
                        rgba(255, 110, 110, 1.0)
                    } else {
                        rgba(185, 16, 16, 1.0)
                    },
                    divider: fg,
                }
            }
        }
    }

    pub fn device_color(&self, dev: &WirelessDevice, low_threshold: u8) -> D2D1_COLOR_F {
        device_status_color(
            dev,
            low_threshold,
            self.green,
            self.orange,
            self.red,
            self.accent,
        )
    }

    pub fn pill_colors(
        &self,
        active: bool,
        hovered: bool,
        pressed: bool,
    ) -> (D2D1_COLOR_F, D2D1_COLOR_F) {
        if active {
            let bg = if pressed {
                self.accent_pressed
            } else if hovered {
                self.accent_hover
            } else {
                self.accent
            };
            (bg, self.accent_text)
        } else {
            let bg = if pressed {
                self.pill_pressed
            } else if hovered {
                self.pill_hover
            } else {
                self.pill_bg
            };
            (bg, self.text_primary)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeviceKind, Transport};
    use std::time::Instant;

    #[test]
    fn dark_and_light_text_muted_meet_wcag_aa() {
        let hc_dark = UiTheme::HighContrast(HighContrastColors {
            dark: true,
            bg: (0, 0, 0),
            fg: (255, 255, 255),
            accent: (26, 235, 255),
            accent_fg: (0, 0, 0),
            muted: (180, 180, 180),
        });
        let hc_light = UiTheme::HighContrast(HighContrastColors {
            dark: false,
            bg: (255, 255, 255),
            fg: (0, 0, 0),
            accent: (0, 90, 180),
            accent_fg: (255, 255, 255),
            muted: (80, 80, 80),
        });

        for theme in [UiTheme::Dark, UiTheme::Light, hc_dark, hc_light] {
            let p = PopupPalette::for_theme(theme);
            let muted_vs_bg = contrast_ratio(p.text_muted, p.bg);
            let muted_vs_card = contrast_ratio(p.text_muted, p.card_bg);
            assert!(
                muted_vs_bg >= 4.5,
                "theme {:?}: text_muted vs bg contrast {:.2} < 4.5",
                theme,
                muted_vs_bg
            );
            assert!(
                muted_vs_card >= 4.5,
                "theme {:?}: text_muted vs card_bg contrast {:.2} < 4.5",
                theme,
                muted_vs_card
            );
            assert!(contrast_ratio(p.text_secondary, p.card_bg) >= 4.5);
            assert!(contrast_ratio(p.text_primary, p.card_bg) >= 4.5);
            let soft_bg = composite_over(p.accent_soft, p.card_bg);
            assert!(
                contrast_ratio(p.text_primary, soft_bg) >= 4.5,
                "theme {:?}: text_primary over accent_soft contrast < 4.5",
                theme
            );
        }
    }

    #[test]
    fn rgb_to_colorref_packs_bbggrr_without_channel_swap() {
        assert_eq!(rgb_to_colorref(0x3E, 0x42, 0x46), 0x0046423E);
        assert_eq!(rgb_to_colorref(214, 218, 226), 0x00E2DAD6);

        let light = PopupPalette::for_theme(UiTheme::Light);
        let (r, g, b) = colorref_to_rgb(light.dwm_border_colorref);
        assert!((r as f32 / 255.0 - light.card_border.r).abs() < 1e-4);
        assert!((g as f32 / 255.0 - light.card_border.g).abs() < 1e-4);
        assert!((b as f32 / 255.0 - light.card_border.b).abs() < 1e-4);
    }

    #[test]
    fn device_status_color_respects_charging_and_thresholds() {
        let p = PopupPalette::for_theme(UiTheme::Dark);
        let mut dev = WirelessDevice {
            id: "d1".into(),
            name: "Mouse".into(),
            kind: DeviceKind::Mouse,
            transport: Transport::Wireless24G,
            battery_pct: Some(50),
            charging: false,
            sub_batteries: None,
            online: true,
            last_seen: Instant::now(),
            detail_note: None,
        };

        assert_eq!(p.device_color(&dev, 20).r, p.accent.r);
        dev.battery_pct = Some(18);
        assert_eq!(p.device_color(&dev, 20).r, p.orange.r);
        dev.battery_pct = Some(8);
        assert_eq!(p.device_color(&dev, 20).r, p.red.r);
        dev.charging = true;
        assert_eq!(p.device_color(&dev, 20).r, p.green.r);
    }

    #[test]
    fn hw_value_color_respects_thresholds_and_charging() {
        let p = TaskbarPalette::for_theme(UiTheme::Dark);
        let mut hw = HardwareMetrics::default();

        // Normal defaults
        assert_eq!(
            p.hw_value_color(HwMetric::CpuTemp, &hw, 20).r,
            p.text_primary.r
        );
        assert_eq!(
            p.hw_value_color(HwMetric::CpuUsage, &hw, 20).r,
            p.text_primary.r
        );
        assert_eq!(
            p.hw_value_color(HwMetric::MemUsage, &hw, 20).r,
            p.text_primary.r
        );
        assert_eq!(
            p.hw_value_color(HwMetric::NetUp, &hw, 20).r,
            p.text_primary.r
        );

        // CpuTemp & GpuTemp: >=80 warn, >=90 crit
        hw.cpu_temp_c = Some(79.9);
        assert_eq!(
            p.hw_value_color(HwMetric::CpuTemp, &hw, 20).r,
            p.text_primary.r
        );
        hw.cpu_temp_c = Some(80.0);
        assert_eq!(
            p.hw_value_color(HwMetric::CpuTemp, &hw, 20).r,
            p.warn_orange.r
        );
        hw.cpu_temp_c = Some(90.0);
        assert_eq!(p.hw_value_color(HwMetric::CpuTemp, &hw, 20).r, p.crit_red.r);

        hw.gpu_temp_c = Some(82.0);
        assert_eq!(
            p.hw_value_color(HwMetric::GpuTemp, &hw, 20).r,
            p.warn_orange.r
        );
        hw.gpu_temp_c = Some(92.0);
        assert_eq!(p.hw_value_color(HwMetric::GpuTemp, &hw, 20).r, p.crit_red.r);

        // GpuHotspot: >=90 warn, >=100 crit
        hw.gpu_hotspot_c = Some(89.0);
        assert_eq!(
            p.hw_value_color(HwMetric::GpuHotspot, &hw, 20).r,
            p.text_primary.r
        );
        hw.gpu_hotspot_c = Some(90.0);
        assert_eq!(
            p.hw_value_color(HwMetric::GpuHotspot, &hw, 20).r,
            p.warn_orange.r
        );
        hw.gpu_hotspot_c = Some(100.0);
        assert_eq!(
            p.hw_value_color(HwMetric::GpuHotspot, &hw, 20).r,
            p.crit_red.r
        );

        // DiskTemp: >=60 warn, >=70 crit
        hw.disk_temp_c = Some(59.0);
        assert_eq!(
            p.hw_value_color(HwMetric::DiskTemp, &hw, 20).r,
            p.text_primary.r
        );
        hw.disk_temp_c = Some(60.0);
        assert_eq!(
            p.hw_value_color(HwMetric::DiskTemp, &hw, 20).r,
            p.warn_orange.r
        );
        hw.disk_temp_c = Some(70.0);
        assert_eq!(
            p.hw_value_color(HwMetric::DiskTemp, &hw, 20).r,
            p.crit_red.r
        );

        // MemUsage: >=85 warn, >=93 crit
        hw.mem_usage_pct = 84.9;
        assert_eq!(
            p.hw_value_color(HwMetric::MemUsage, &hw, 20).r,
            p.text_primary.r
        );
        hw.mem_usage_pct = 85.0;
        assert_eq!(
            p.hw_value_color(HwMetric::MemUsage, &hw, 20).r,
            p.warn_orange.r
        );
        hw.mem_usage_pct = 93.0;
        assert_eq!(
            p.hw_value_color(HwMetric::MemUsage, &hw, 20).r,
            p.crit_red.r
        );

        // CpuUsage & GpuUsage: >=95 warn
        hw.cpu_usage_pct = 94.9;
        assert_eq!(
            p.hw_value_color(HwMetric::CpuUsage, &hw, 20).r,
            p.text_primary.r
        );
        hw.cpu_usage_pct = 95.0;
        assert_eq!(
            p.hw_value_color(HwMetric::CpuUsage, &hw, 20).r,
            p.warn_orange.r
        );
        hw.gpu_usage_pct = Some(95.0);
        assert_eq!(
            p.hw_value_color(HwMetric::GpuUsage, &hw, 20).r,
            p.warn_orange.r
        );

        // SysBattery: charging -> charging, <=10.min(thresh) -> crit, <=thresh -> warn
        hw.sys_battery_pct = Some(50);
        assert_eq!(
            p.hw_value_color(HwMetric::SysBattery, &hw, 20).r,
            p.text_primary.r
        );
        hw.sys_battery_pct = Some(20);
        assert_eq!(
            p.hw_value_color(HwMetric::SysBattery, &hw, 20).r,
            p.warn_orange.r
        );
        hw.sys_battery_pct = Some(10);
        assert_eq!(
            p.hw_value_color(HwMetric::SysBattery, &hw, 20).r,
            p.crit_red.r
        );
        hw.sys_battery_pct = Some(5);
        assert_eq!(
            p.hw_value_color(HwMetric::SysBattery, &hw, 5).r,
            p.crit_red.r
        );
        hw.sys_battery_charging = true;
        assert_eq!(
            p.hw_value_color(HwMetric::SysBattery, &hw, 20).r,
            p.charging.r
        );
    }
}
