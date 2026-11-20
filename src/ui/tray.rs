use std::collections::HashSet;
use std::ffi::c_void;
use std::fmt::Write as _;
use std::mem::size_of;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_BEZIER_SEGMENT, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
    D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_PIXEL_FORMAT,
    D2D_POINT_2F,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1RenderTarget, D2D1_CAP_STYLE_ROUND, D2D1_ELLIPSE, D2D1_LINE_JOIN_ROUND,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_STROKE_STYLE_PROPERTIES,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP,
    NIIF_RESPECT_QUIET_TIME, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NINF_KEY, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4, NOTIFY_ICON_INFOTIP_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, PostMessageW,
    SetForegroundWindow, TrackPopupMenu, HICON, HMENU, ICONINFO, MF_CHECKED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, MF_UNCHECKED, SM_CXSMICON, SW_SHOWNORMAL, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_NULL,
};

use crate::config::{AnchorEdge, AppConfig, CombinedOrder, HwMetric, LayoutMode};
use crate::state::MonitorSnapshot;
use crate::ui::renderer::shared_d2d_factory;
use crate::ui::taskbar::find_taskbar_geometry;
use crate::ui::theme::rgba;
use crate::ui::SnapshotStore;

pub const WM_APP_TRAY_CALLBACK: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 4;
pub const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

const RUN_SUBKEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("AirMonitor");

const ID_LAYOUT_COMBINED_DEV_FIRST: u32 = 1001;
const ID_LAYOUT_COMBINED_HW_FIRST: u32 = 1002;
const ID_LAYOUT_SPLIT: u32 = 1003;
const ID_LAYOUT_HARDWARE_ONLY: u32 = 1004;
const ID_LAYOUT_DEVICES_ONLY: u32 = 1005;
const ID_ANCHOR_COMBINED_LEFT: u32 = 1010;
const ID_ANCHOR_COMBINED_RIGHT: u32 = 1011;
const ID_ANCHOR_HW_LEFT: u32 = 1012;
const ID_ANCHOR_HW_RIGHT: u32 = 1013;
const ID_ANCHOR_DEV_LEFT: u32 = 1014;
const ID_ANCHOR_DEV_RIGHT: u32 = 1015;
const ID_METRIC_BASE: u32 = 1100;
const ID_REFRESH_WIRELESS: u32 = 1200;
const ID_AUTOSTART: u32 = 1202;
const ID_OPEN_CONFIG: u32 = 1203;
const ID_QUIT: u32 = 1204;
const ID_ABOUT: u32 = 1205;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    None,
    ConfigChanged,
    RefreshWireless,
    OpenConfig,
    OpenAbout,
    Quit,
}

pub fn is_autostart_enabled() -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, 0, KEY_READ, &mut hkey).is_ok() {
            let mut size = 0u32;
            let status = RegQueryValueExW(hkey, RUN_VALUE, None, None, None, Some(&mut size));
            let _ = RegCloseKey(hkey);
            return status.is_ok() && size > 0;
        }
        false
    }
}

pub fn set_autostart(enabled: bool) -> Result<(), String> {
    unsafe {
        let mut hkey = HKEY::default();
        if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let cmd = format!("\"{}\"", exe.display());
            let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let res = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                RUN_SUBKEY,
                0,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut hkey,
                None,
            );
            if res.is_err() {
                return Err(format!("RegCreateKeyExW failed: {:?}", res));
            }
            let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            let set_res = RegSetValueExW(hkey, RUN_VALUE, 0, REG_SZ, Some(bytes));
            let _ = RegCloseKey(hkey);
            if set_res.is_err() {
                return Err(format!("RegSetValueExW failed: {:?}", set_res));
            }
        } else {
            let res = RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, 0, KEY_SET_VALUE, &mut hkey);
            if res.is_ok() {
                let del_res = RegDeleteValueW(hkey, RUN_VALUE);
                let _ = RegCloseKey(hkey);
                if del_res.is_err() && del_res != ERROR_FILE_NOT_FOUND {
                    return Err(format!("RegDeleteValueW failed: {:?}", del_res));
                }
            }
        }
        Ok(())
    }
}

pub fn open_config_in_editor(snapshot: &MonitorSnapshot) -> Result<(), String> {
    let path = AppConfig::config_path();
    if !path.exists() {
        snapshot.config.save()?;
    }
    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let _ = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
    Ok(())
}

pub fn tray_icon_size_for_dpi(dpi: u32) -> i32 {
    let dpi = dpi.max(96);
    let metric = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) };
    if metric > 0 {
        metric.clamp(16, 64)
    } else {
        ((16 * dpi as i32) / 96).clamp(16, 64)
    }
}

pub fn create_tray_icon(size: i32) -> windows::core::Result<HICON> {
    let size = size.clamp(16, 64);
    let s = size as f32 / 512.0;

    unsafe {
        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return Err(windows::core::Error::from_win32());
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let hbm_color: HBITMAP =
            match CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(b) => b,
                Err(e) => {
                    let _ = DeleteDC(hdc);
                    return Err(e);
                }
            };
        let old_obj = SelectObject(hdc, HGDIOBJ(hbm_color.0));

        // Render minimalist hand-drawn AirMonitor icon (matching assets/airmonitor.svg) via Direct2D.
        let render_res = (|| -> windows::core::Result<()> {
            let d2d_factory = shared_d2d_factory()?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                ..Default::default()
            };
            let rt = d2d_factory.CreateDCRenderTarget(&props)?;
            let bind_rect = RECT {
                left: 0,
                top: 0,
                right: size,
                bottom: size,
            };
            rt.BindDC(hdc, &bind_rect)?;
            let base_rt: ID2D1RenderTarget = windows::core::Interface::cast(&rt)?;
            let stroke_style = d2d_factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    dashCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    miterLimit: 4.0,
                    ..Default::default()
                },
                None,
            )?;

            let fill_paper = base_rt.CreateSolidColorBrush(&rgba(248, 250, 252, 1.0), None)?;
            let ink_brush = base_rt.CreateSolidColorBrush(&rgba(31, 41, 55, 1.0), None)?;
            let sky_brush = base_rt.CreateSolidColorBrush(&rgba(14, 165, 233, 1.0), None)?;
            let mint_brush = base_rt.CreateSolidColorBrush(&rgba(16, 185, 129, 1.0), None)?;

            let pt = |x: f32, y: f32| D2D_POINT_2F { x: x * s, y: y * s };

            // 1. Hand-drawn organic squircle badge
            let badge_geo = d2d_factory.CreatePathGeometry()?;
            {
                let sink = badge_geo.Open()?;
                sink.BeginFigure(pt(132.0, 58.0), D2D1_FIGURE_BEGIN_FILLED);
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(210.0, 50.0),
                    point2: pt(305.0, 52.0),
                    point3: pt(382.0, 60.0),
                });
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(438.0, 66.0),
                    point2: pt(454.0, 92.0),
                    point3: pt(456.0, 148.0),
                });
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(462.0, 224.0),
                    point2: pt(458.0, 302.0),
                    point3: pt(452.0, 372.0),
                });
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(446.0, 432.0),
                    point2: pt(422.0, 452.0),
                    point3: pt(366.0, 456.0),
                });
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(292.0, 462.0),
                    point2: pt(212.0, 460.0),
                    point3: pt(140.0, 454.0),
                });
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(80.0, 448.0),
                    point2: pt(58.0, 424.0),
                    point3: pt(56.0, 364.0),
                });
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(50.0, 290.0),
                    point2: pt(52.0, 212.0),
                    point3: pt(58.0, 142.0),
                });
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(64.0, 84.0),
                    point2: pt(86.0, 64.0),
                    point3: pt(132.0, 58.0),
                });
                sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                sink.Close()?;
            }

            // 2. Wireless signal arcs
            let arc1_geo = d2d_factory.CreatePathGeometry()?;
            {
                let sink = arc1_geo.Open()?;
                sink.BeginFigure(pt(164.0, 134.0), D2D1_FIGURE_BEGIN_HOLLOW);
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(196.0, 130.0),
                    point2: pt(224.0, 154.0),
                    point3: pt(226.0, 188.0),
                });
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
                sink.Close()?;
            }

            let arc2_geo = d2d_factory.CreatePathGeometry()?;
            {
                let sink = arc2_geo.Open()?;
                sink.BeginFigure(pt(162.0, 96.0), D2D1_FIGURE_BEGIN_HOLLOW);
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(220.0, 92.0),
                    point2: pt(266.0, 136.0),
                    point3: pt(268.0, 192.0),
                });
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
                sink.Close()?;
            }

            // 3. Flowing air / breeze waves
            let wave1_geo = d2d_factory.CreatePathGeometry()?;
            {
                let sink = wave1_geo.Open()?;
                sink.BeginFigure(pt(118.0, 282.0), D2D1_FIGURE_BEGIN_HOLLOW);
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(175.0, 248.0),
                    point2: pt(240.0, 250.0),
                    point3: pt(294.0, 280.0),
                });
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(336.0, 304.0),
                    point2: pt(386.0, 288.0),
                    point3: pt(396.0, 250.0),
                });
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(402.0, 226.0),
                    point2: pt(380.0, 206.0),
                    point3: pt(356.0, 216.0),
                });
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
                sink.Close()?;
            }

            let wave2_geo = d2d_factory.CreatePathGeometry()?;
            {
                let sink = wave2_geo.Open()?;
                sink.BeginFigure(pt(134.0, 356.0), D2D1_FIGURE_BEGIN_HOLLOW);
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(194.0, 332.0),
                    point2: pt(254.0, 338.0),
                    point3: pt(308.0, 362.0),
                });
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: pt(344.0, 378.0),
                    point2: pt(382.0, 366.0),
                    point3: pt(386.0, 338.0),
                });
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
                sink.Close()?;
            }

            base_rt.BeginDraw();
            base_rt.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            base_rt.FillGeometry(&badge_geo, &fill_paper, None);
            base_rt.DrawGeometry(&badge_geo, &ink_brush, 24.0 * s, &stroke_style);

            let dot = D2D1_ELLIPSE {
                point: pt(154.0, 186.0),
                radiusX: 24.0 * s,
                radiusY: 24.0 * s,
            };
            base_rt.FillEllipse(&dot, &mint_brush);
            base_rt.DrawEllipse(&dot, &ink_brush, 14.0 * s, &stroke_style);

            base_rt.DrawGeometry(&arc1_geo, &ink_brush, 22.0 * s, &stroke_style);
            base_rt.DrawGeometry(&arc2_geo, &ink_brush, 22.0 * s, &stroke_style);
            base_rt.DrawGeometry(&wave1_geo, &sky_brush, 26.0 * s, &stroke_style);
            base_rt.DrawGeometry(&wave2_geo, &sky_brush, 24.0 * s, &stroke_style);

            base_rt.EndDraw(None, None)
        })();

        let _ = SelectObject(hdc, old_obj);
        let _ = DeleteDC(hdc);

        if let Err(e) = render_res {
            if !hbm_color.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(hbm_color.0));
            }
            return Err(e);
        }

        let mask_stride = ((size + 15) / 16) * 2;
        let mask_bytes = vec![0u8; (mask_stride * size) as usize];
        let hbm_mask = CreateBitmap(size, size, 1, 1, Some(mask_bytes.as_ptr() as *const c_void));
        let ii = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };
        let hicon = CreateIconIndirect(&ii);
        if !hbm_color.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(hbm_color.0));
        }
        if !hbm_mask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(hbm_mask.0));
        }
        hicon
    }
}

const MAX_TRAY_RETRIES: u32 = 20;
const SAVE_ERROR_COOLDOWN: Duration = Duration::from_secs(5);

pub struct TrayManager {
    hwnd: HWND,
    hicon: HICON,
    icon_size: i32,
    added: bool,
    v4_active: bool,
    retry_count: u32,
    last_tip: String,
    tip_scratch: String,
    alerted_low_battery: HashSet<String>,
    last_save_error_at: Option<Instant>,
}

impl TrayManager {
    pub fn new(hwnd: HWND) -> windows::core::Result<Self> {
        let dpi = find_taskbar_geometry().map(|g| g.dpi).unwrap_or(96);
        let icon_size = tray_icon_size_for_dpi(dpi);
        let hicon = create_tray_icon(icon_size)?;
        let mut mgr = Self {
            hwnd,
            hicon,
            icon_size,
            added: false,
            v4_active: false,
            retry_count: 0,
            last_tip: "AirMonitor".to_string(),
            tip_scratch: String::with_capacity(128),
            alerted_low_battery: HashSet::new(),
            last_save_error_at: None,
        };

        mgr.try_add_icon();
        Ok(mgr)
    }

    fn try_add_icon(&mut self) -> bool {
        let tip = if self.last_tip.is_empty() {
            "AirMonitor"
        } else {
            &self.last_tip
        };
        let mut nid = self.base_nid();
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_APP_TRAY_CALLBACK;
        nid.hIcon = self.hicon;
        write_wide_buf(&mut nid.szTip, tip);

        unsafe {
            if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                self.added = true;
                self.retry_count = 0;
                nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                self.v4_active = Shell_NotifyIconW(NIM_SETVERSION, &nid).as_bool();
                return true;
            }
        }
        false
    }

    pub fn is_v4(&self) -> bool {
        self.v4_active
    }

    pub fn refresh_icon_for_dpi(&mut self, dpi: u32) {
        let next_size = tray_icon_size_for_dpi(dpi);
        if next_size == self.icon_size {
            return;
        }
        if let Ok(new_icon) = create_tray_icon(next_size) {
            let old_icon = self.hicon;
            self.hicon = new_icon;
            self.icon_size = next_size;
            if self.added {
                let mut nid = self.base_nid();
                nid.uFlags = NIF_ICON;
                nid.hIcon = self.hicon;
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
                }
            }
            unsafe {
                if !old_icon.is_invalid() {
                    let _ = DestroyIcon(old_icon);
                }
            }
        }
    }

    /// Clean up any stale shell registration and re-add the tray icon, restoring tooltip state.
    pub fn re_add(&mut self) -> bool {
        self.added = false;
        self.v4_active = false;
        let nid = self.base_nid();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
        self.try_add_icon()
    }

    pub fn is_added(&self) -> bool {
        self.added
    }

    pub fn reset_retry_count(&mut self) {
        self.retry_count = 0;
    }

    pub fn retry_add_if_needed(&mut self) -> bool {
        if self.added || self.retry_count >= MAX_TRAY_RETRIES {
            return false;
        }
        self.retry_count += 1;
        self.re_add()
    }

    pub fn show_balloon(&self, title: &str, body: &str, info_flags: NOTIFY_ICON_INFOTIP_FLAGS) {
        if !self.added {
            return;
        }
        let mut nid = self.base_nid();
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = info_flags;
        write_wide_buf(&mut nid.szInfoTitle, title);
        write_wide_buf(&mut nid.szInfo, body);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    pub fn notify_save_error(&mut self, err: &str) {
        if !self.added {
            return;
        }
        let now = Instant::now();
        if let Some(prev) = self.last_save_error_at {
            if now.duration_since(prev) < SAVE_ERROR_COOLDOWN {
                return;
            }
        }
        self.last_save_error_at = Some(now);
        let body = format!("配置未能写入磁盘，更改仅在本次生效：{}", err);
        self.show_balloon("AirMonitor 配置保存失败", &body, NIIF_WARNING);
    }

    fn base_nid(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        }
    }

    pub fn update_tooltip(&mut self, snapshot: &MonitorSnapshot) {
        if !self.added {
            return;
        }
        format_tooltip_into(&mut self.tip_scratch, snapshot);
        if self.tip_scratch == self.last_tip {
            return;
        }

        let mut nid = self.base_nid();
        nid.uFlags = NIF_TIP;
        write_wide_buf(&mut nid.szTip, &self.tip_scratch);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
        self.last_tip.clear();
        self.last_tip.push_str(&self.tip_scratch);
    }

    pub fn check_low_battery_alerts(&mut self, snapshot: &MonitorSnapshot) {
        if !self.added {
            return;
        }
        let threshold = snapshot.config.low_battery_threshold;
        let reset_threshold = threshold.saturating_add(5);

        for dev in &snapshot.devices {
            if !dev.online || snapshot.config.hidden_devices.contains(&dev.id) {
                continue;
            }
            if dev.charging {
                self.alerted_low_battery.remove(&dev.id);
                continue;
            }
            if let Some(pct) = dev.battery_pct {
                if pct <= threshold {
                    if self.alerted_low_battery.insert(dev.id.clone()) {
                        let name = snapshot
                            .config
                            .device_aliases
                            .get(&dev.id)
                            .map(String::as_str)
                            .unwrap_or(&dev.name);
                        let body = format!("{} 电量仅剩 {}%，请及时充电。", name, pct);
                        self.show_balloon(
                            "AirMonitor 低电量提醒",
                            &body,
                            NIIF_WARNING | NIIF_RESPECT_QUIET_TIME,
                        );
                    }
                } else if pct >= reset_threshold {
                    self.alerted_low_battery.remove(&dev.id);
                }
            }
        }
    }
}

impl Drop for TrayManager {
    fn drop(&mut self) {
        unsafe {
            if self.added {
                let nid = self.base_nid();
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                self.added = false;
            }
            if !self.hicon.is_invalid() {
                let _ = DestroyIcon(self.hicon);
            }
        }
    }
}

pub(crate) fn format_tooltip_into(out: &mut String, snapshot: &MonitorSnapshot) {
    out.clear();
    let _ = write!(
        out,
        "AirMonitor | CPU {:.0}% MEM {:.0}%",
        snapshot.hw.cpu_usage_pct, snapshot.hw.mem_usage_pct
    );
    for dev in snapshot
        .devices
        .iter()
        .filter(|d| {
            d.online
                && !snapshot.config.hidden_devices.contains(&d.id)
                && !(snapshot.config.hide_unknown_battery_devices && d.battery_pct.is_none())
        })
        .take(3)
    {
        let prev_len = out.len();
        let name = snapshot
            .config
            .device_aliases
            .get(&dev.id)
            .map(String::as_str)
            .unwrap_or(&dev.name);
        let _ = write!(out, " | {} ", name);
        match (dev.charging, dev.battery_pct) {
            (true, Some(p)) => {
                let _ = write!(out, "⚡{}%", p);
            }
            (true, None) => {
                out.push('⚡');
            }
            (false, Some(p)) => {
                let _ = write!(out, "{}%", p);
            }
            (false, None) => {
                out.push_str("--");
            }
        }
        if out.encode_utf16().count() >= 127 {
            out.truncate(prev_len);
            break;
        }
    }
}

/// Builds and runs the tray/bar right-click menu completely outside any `SnapshotStore`
/// lock and outside any `RefCell<UiApp>` borrow so `TrackPopupMenu`'s modal loop can
/// safely dispatch window messages without aliasing UB.
pub(crate) fn track_context_menu(
    anchor_hwnd: HWND,
    x: i32,
    y: i32,
    state: &SnapshotStore,
) -> Option<u32> {
    let cfg = state.read_snapshot()?.config.clone();
    let autostart_active = is_autostart_enabled();

    unsafe {
        let Ok(hmenu) = CreatePopupMenu() else {
            return None;
        };

        let is_comb_dev = cfg.layout_mode == LayoutMode::Combined
            && cfg.combined_order == CombinedOrder::DevicesFirst;
        let is_comb_hw = cfg.layout_mode == LayoutMode::Combined
            && cfg.combined_order == CombinedOrder::HardwareFirst;
        let is_split = cfg.layout_mode == LayoutMode::Split;
        let is_hw_only = cfg.layout_mode == LayoutMode::HardwareOnly;
        let is_dev_only = cfg.layout_mode == LayoutMode::DevicesOnly;

        append_checked(
            hmenu,
            ID_LAYOUT_COMBINED_DEV_FIRST,
            "联体 [设备 | 硬件]",
            is_comb_dev,
        );
        append_checked(
            hmenu,
            ID_LAYOUT_COMBINED_HW_FIRST,
            "联体 [硬件 | 设备]",
            is_comb_hw,
        );
        append_checked(hmenu, ID_LAYOUT_SPLIT, "分体独立模式", is_split);
        append_checked(hmenu, ID_LAYOUT_HARDWARE_ONLY, "仅硬件模式", is_hw_only);
        append_checked(hmenu, ID_LAYOUT_DEVICES_ONLY, "仅设备模式", is_dev_only);

        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, None);

        match cfg.layout_mode {
            LayoutMode::Combined => {
                append_checked(
                    hmenu,
                    ID_ANCHOR_COMBINED_LEFT,
                    "左锚定",
                    cfg.combined_zone.anchor == AnchorEdge::Left,
                );
                append_checked(
                    hmenu,
                    ID_ANCHOR_COMBINED_RIGHT,
                    "右锚定 (托盘旁)",
                    cfg.combined_zone.anchor == AnchorEdge::Right,
                );
            }
            LayoutMode::HardwareOnly => {
                append_checked(
                    hmenu,
                    ID_ANCHOR_HW_LEFT,
                    "左锚定",
                    cfg.hardware_zone.anchor == AnchorEdge::Left,
                );
                append_checked(
                    hmenu,
                    ID_ANCHOR_HW_RIGHT,
                    "右锚定 (托盘旁)",
                    cfg.hardware_zone.anchor == AnchorEdge::Right,
                );
            }
            LayoutMode::DevicesOnly => {
                append_checked(
                    hmenu,
                    ID_ANCHOR_DEV_LEFT,
                    "左锚定",
                    cfg.devices_zone.anchor == AnchorEdge::Left,
                );
                append_checked(
                    hmenu,
                    ID_ANCHOR_DEV_RIGHT,
                    "右锚定 (托盘旁)",
                    cfg.devices_zone.anchor == AnchorEdge::Right,
                );
            }
            LayoutMode::Split => {
                if let Ok(hw_anchor_menu) = CreatePopupMenu() {
                    append_checked(
                        hw_anchor_menu,
                        ID_ANCHOR_HW_LEFT,
                        "左锚定",
                        cfg.hardware_zone.anchor == AnchorEdge::Left,
                    );
                    append_checked(
                        hw_anchor_menu,
                        ID_ANCHOR_HW_RIGHT,
                        "右锚定 (托盘旁)",
                        cfg.hardware_zone.anchor == AnchorEdge::Right,
                    );
                    append_submenu(hmenu, hw_anchor_menu, "硬件区锚定");
                }
                if let Ok(dev_anchor_menu) = CreatePopupMenu() {
                    append_checked(
                        dev_anchor_menu,
                        ID_ANCHOR_DEV_LEFT,
                        "左锚定",
                        cfg.devices_zone.anchor == AnchorEdge::Left,
                    );
                    append_checked(
                        dev_anchor_menu,
                        ID_ANCHOR_DEV_RIGHT,
                        "右锚定 (托盘旁)",
                        cfg.devices_zone.anchor == AnchorEdge::Right,
                    );
                    append_submenu(hmenu, dev_anchor_menu, "设备区锚定");
                }
            }
        }

        if let Ok(metrics_menu) = CreatePopupMenu() {
            for (idx, metric) in HwMetric::ALL.iter().enumerate() {
                let checked = cfg.hardware_items.contains(metric);
                append_checked(
                    metrics_menu,
                    ID_METRIC_BASE + idx as u32,
                    metric.display_name(),
                    checked,
                );
            }
            append_submenu(hmenu, metrics_menu, "硬件监控项");
        }

        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, None);
        append_item(hmenu, ID_REFRESH_WIRELESS, "立即刷新无线设备");
        append_checked(hmenu, ID_AUTOSTART, "开机自启动", autostart_active);
        append_item(hmenu, ID_OPEN_CONFIG, "打开配置文件");
        append_item(hmenu, ID_ABOUT, "关于 / 检查更新");

        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, None);
        append_item(hmenu, ID_QUIT, "退出");

        let (menu_x, menu_y, tpm_flags) = compute_menu_anchor(x, y);
        let _ = SetForegroundWindow(anchor_hwnd);
        let cmd = TrackPopupMenu(hmenu, tpm_flags, menu_x, menu_y, 0, anchor_hwnd, None);
        let _ = PostMessageW(anchor_hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(hmenu);

        if cmd.as_bool() {
            Some(cmd.0 as u32)
        } else {
            None
        }
    }
}

pub(crate) fn apply_menu_command(
    cmd_id: u32,
    state: &SnapshotStore,
) -> (MenuAction, Option<String>) {
    match cmd_id {
        ID_REFRESH_WIRELESS => (MenuAction::RefreshWireless, None),
        ID_OPEN_CONFIG => (MenuAction::OpenConfig, None),
        ID_ABOUT => (MenuAction::OpenAbout, None),
        ID_QUIT => (MenuAction::Quit, None),
        ID_AUTOSTART => {
            // Perform registry IO completely outside any SnapshotStore lock.
            let target = !is_autostart_enabled();
            if let Err(err) = set_autostart(target) {
                return (MenuAction::None, Some(format!("设置开机自启失败: {err}")));
            }
            let (changed, save_err) = state.update_and_save_config(|cfg| {
                cfg.autostart = target;
                true
            });
            if changed {
                (MenuAction::ConfigChanged, save_err)
            } else {
                (MenuAction::None, None)
            }
        }
        _ => {
            let (changed, save_err) = state.update_and_save_config(|cfg| match cmd_id {
                ID_LAYOUT_COMBINED_DEV_FIRST => {
                    cfg.layout_mode = LayoutMode::Combined;
                    cfg.combined_order = CombinedOrder::DevicesFirst;
                    true
                }
                ID_LAYOUT_COMBINED_HW_FIRST => {
                    cfg.layout_mode = LayoutMode::Combined;
                    cfg.combined_order = CombinedOrder::HardwareFirst;
                    true
                }
                ID_LAYOUT_SPLIT => {
                    cfg.layout_mode = LayoutMode::Split;
                    true
                }
                ID_LAYOUT_HARDWARE_ONLY => {
                    cfg.layout_mode = LayoutMode::HardwareOnly;
                    true
                }
                ID_LAYOUT_DEVICES_ONLY => {
                    cfg.layout_mode = LayoutMode::DevicesOnly;
                    true
                }
                ID_ANCHOR_COMBINED_LEFT => {
                    cfg.combined_zone.anchor = AnchorEdge::Left;
                    true
                }
                ID_ANCHOR_COMBINED_RIGHT => {
                    cfg.combined_zone.anchor = AnchorEdge::Right;
                    true
                }
                ID_ANCHOR_HW_LEFT => {
                    cfg.hardware_zone.anchor = AnchorEdge::Left;
                    true
                }
                ID_ANCHOR_HW_RIGHT => {
                    cfg.hardware_zone.anchor = AnchorEdge::Right;
                    true
                }
                ID_ANCHOR_DEV_LEFT => {
                    cfg.devices_zone.anchor = AnchorEdge::Left;
                    true
                }
                ID_ANCHOR_DEV_RIGHT => {
                    cfg.devices_zone.anchor = AnchorEdge::Right;
                    true
                }
                id if id >= ID_METRIC_BASE
                    && (id - ID_METRIC_BASE) < HwMetric::ALL.len() as u32 =>
                {
                    let metric = HwMetric::ALL[(id - ID_METRIC_BASE) as usize];
                    if let Some(pos) = cfg.hardware_items.iter().position(|&m| m == metric) {
                        cfg.hardware_items.remove(pos);
                    } else {
                        cfg.hardware_items.push(metric);
                    }
                    true
                }
                _ => false,
            });
            if changed {
                (MenuAction::ConfigChanged, save_err)
            } else {
                (MenuAction::None, None)
            }
        }
    }
}

unsafe fn append_item(hmenu: HMENU, id: u32, label: &str) {
    let wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = AppendMenuW(hmenu, MF_STRING, id as usize, PCWSTR(wide.as_ptr()));
}

unsafe fn append_checked(hmenu: HMENU, id: u32, label: &str, checked: bool) {
    let wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let flags = MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED };
    let _ = AppendMenuW(hmenu, flags, id as usize, PCWSTR(wide.as_ptr()));
}

unsafe fn append_submenu(hmenu: HMENU, sub: HMENU, label: &str) {
    let wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = AppendMenuW(
        hmenu,
        MF_STRING | MF_POPUP,
        sub.0 as usize,
        PCWSTR(wide.as_ptr()),
    );
}

fn compute_menu_anchor(
    x: i32,
    y: i32,
) -> (
    i32,
    i32,
    windows::Win32::UI::WindowsAndMessaging::TRACK_POPUP_MENU_FLAGS,
) {
    let base_flags = TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY;
    if let Some(geom) = find_taskbar_geometry() {
        let tb = geom.taskbar_rect;
        // Position menu strictly above a bottom taskbar so it never overlaps the taskbar bar.
        if tb.top > 100 && y >= tb.top - 4 {
            return (x, tb.top - 4, base_flags | TPM_BOTTOMALIGN);
        }
        if tb.top <= 4 && y <= tb.bottom + 4 {
            return (x, tb.bottom + 4, base_flags);
        }
    }
    (x, y, base_flags)
}

fn write_wide_buf(buf: &mut [u16], text: &str) {
    buf.fill(0);
    if buf.is_empty() {
        return;
    }
    let max_chars = buf.len() - 1;
    for (i, ch) in text.encode_utf16().take(max_chars).enumerate() {
        buf[i] = ch;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DeviceKind, Transport, WirelessDevice};

    #[test]
    fn tooltip_formatting_is_deterministic_and_bounded() {
        let mut snap = MonitorSnapshot::new(AppConfig::default());
        snap.hw.cpu_usage_pct = 12.2;
        snap.hw.mem_usage_pct = 55.4;
        snap.devices.push(WirelessDevice {
            id: "d1".into(),
            name: "VT3".into(),
            kind: DeviceKind::Mouse,
            transport: Transport::Wireless24G,
            battery_pct: Some(80),
            charging: false,
            sub_batteries: None,
            online: true,
            last_seen: Instant::now(),
            detail_note: None,
        });

        let mut buf1 = String::new();
        let mut buf2 = String::new();
        format_tooltip_into(&mut buf1, &snap);
        snap.hw.cpu_usage_pct = 12.4; // Rounds to same 12%
        format_tooltip_into(&mut buf2, &snap);
        assert_eq!(buf1, buf2);
        assert_eq!(buf1, "AirMonitor | CPU 12% MEM 55% | VT3 80%");
        assert!(buf1.encode_utf16().count() < 127);

        snap.devices.push(WirelessDevice {
            id: "skn".into(),
            name: "SKN".into(),
            kind: DeviceKind::Keyboard,
            transport: Transport::Wireless24G,
            battery_pct: None,
            charging: false,
            sub_batteries: None,
            online: true,
            last_seen: Instant::now(),
            detail_note: None,
        });
        format_tooltip_into(&mut buf1, &snap);
        assert_eq!(buf1, "AirMonitor | CPU 12% MEM 55% | VT3 80%");

        snap.config.hide_unknown_battery_devices = false;
        format_tooltip_into(&mut buf1, &snap);
        assert_eq!(buf1, "AirMonitor | CPU 12% MEM 55% | VT3 80% | SKN --");
    }

    #[test]
    fn metric_menu_ids_do_not_collide_with_action_ids() {
        assert!(ID_METRIC_BASE + (HwMetric::ALL.len() as u32) < ID_REFRESH_WIRELESS);
        assert_ne!(ID_ABOUT, ID_QUIT);
        assert_ne!(ID_ABOUT, ID_OPEN_CONFIG);
    }

    #[test]
    fn about_menu_command_maps_to_open_about() {
        crate::config::set_read_only_mode(true);
        let store = SnapshotStore::new(AppConfig::default());
        let (action, save_err) = apply_menu_command(ID_ABOUT, &store);
        assert_eq!(action, MenuAction::OpenAbout);
        assert!(save_err.is_none());
    }
}
