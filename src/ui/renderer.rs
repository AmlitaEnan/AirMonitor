use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;

use windows::core::w;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
    D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_PIXEL_FORMAT,
    D2D_POINT_2F, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1PathGeometry, ID2D1RenderTarget,
    ID2D1SolidColorBrush, D2D1_ELLIPSE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_ROUNDED_RECT,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

use crate::config::{CombinedOrder, HwMetric};
use crate::state::{format_speed, DeviceKind, HardwareMetrics, MonitorSnapshot, WirelessDevice};
use crate::ui::text::{create_text_format, draw_text_clipped};
pub use crate::ui::theme::{read_taskbar_theme, TaskbarTheme};
use crate::ui::theme::{rgba, TaskbarPalette};

const ZONE_PAD_X: f32 = 6.0;
const HW_COL_GAP: f32 = 8.0;
const HW_NET_COL_WIDTH: f32 = 52.0;
const HW_STAT_COL_WIDTH: f32 = 58.0;
const DEVICE_COL_WIDTH: f32 = 40.0;
const DEVICE_GAP: f32 = 2.0;
const DIVIDER_WIDTH: f32 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderContent {
    Combined,
    HardwareOnly,
    DevicesOnly,
}

pub fn metric_col_width(metric: HwMetric) -> f32 {
    match metric {
        HwMetric::NetUp | HwMetric::NetDown => HW_NET_COL_WIDTH,
        _ => HW_STAT_COL_WIDTH,
    }
}

pub fn format_hw_metric(metric: HwMetric, hw: &HardwareMetrics) -> (&'static str, String) {
    match metric {
        HwMetric::NetUp => (
            "↑",
            if hw.net_valid {
                format_speed(hw.net_up_bps)
            } else {
                "  N/A".to_string()
            },
        ),
        HwMetric::NetDown => (
            "↓",
            if hw.net_valid {
                format_speed(hw.net_down_bps)
            } else {
                "  N/A".to_string()
            },
        ),
        HwMetric::CpuUsage => (
            "CPU",
            if hw.cpu_usage_valid && hw.cpu_usage_pct.is_finite() {
                format!("{:>3.0}%", hw.cpu_usage_pct.clamp(0.0, 999.0))
            } else {
                " N/A".to_string()
            },
        ),
        HwMetric::MemUsage => (
            "MEM",
            if hw.mem_valid && hw.mem_usage_pct.is_finite() {
                format!("{:>3.0}%", hw.mem_usage_pct.clamp(0.0, 999.0))
            } else {
                " N/A".to_string()
            },
        ),
        HwMetric::CpuFreq => (
            "FRQ",
            match hw.cpu_freq_ghz {
                Some(f) if f.is_finite() => format!("{:>3.1}G", f.clamp(0.0, 99.9)),
                _ => "  --".to_string(),
            },
        ),
        HwMetric::CpuTemp => (
            "CTM",
            match hw.cpu_temp_c {
                Some(t) if t.is_finite() => format!("{:>3.0}°", t.clamp(-99.0, 999.0)),
                _ => "  --".to_string(),
            },
        ),
        HwMetric::CpuPower => (
            "CPW",
            match hw.cpu_power_w {
                Some(p) if p.is_finite() => format!("{:>3.0}W", p.clamp(0.0, 999.0)),
                _ => "  --".to_string(),
            },
        ),
        HwMetric::CpuFan => ("CFN", format_fan_rpm(hw.cpu_fan_rpm)),
        HwMetric::DiskRead => (
            "R",
            match hw.disk_read_bps {
                Some(bps) if bps.is_finite() => format_speed(bps),
                _ => "   --".to_string(),
            },
        ),
        HwMetric::DiskWrite => (
            "W",
            match hw.disk_write_bps {
                Some(bps) if bps.is_finite() => format_speed(bps),
                _ => "   --".to_string(),
            },
        ),
        HwMetric::DiskTemp => (
            "DTM",
            match hw.disk_temp_c {
                Some(t) if t.is_finite() => format!("{:>3.0}°", t.clamp(-99.0, 999.0)),
                _ => "  --".to_string(),
            },
        ),
        HwMetric::SysBattery => (
            "BAT",
            match hw.sys_battery_pct {
                Some(pct) => {
                    let p = pct.min(100);
                    if hw.sys_battery_charging {
                        format!("⚡{}%", p)
                    } else {
                        format!("{:>3}%", p)
                    }
                }
                None => "  --".to_string(),
            },
        ),
        HwMetric::GpuUsage => (
            "GPU",
            match hw.gpu_usage_pct {
                Some(u) if u.is_finite() => format!("{:>3.0}%", u.clamp(0.0, 100.0)),
                _ => "  --".to_string(),
            },
        ),
        HwMetric::GpuTemp => (
            "TMP",
            match hw.gpu_temp_c {
                Some(t) if t.is_finite() => format!("{:>3.0}°", t.clamp(-99.0, 999.0)),
                _ => "  --".to_string(),
            },
        ),
        HwMetric::GpuHotspot => (
            "HOT",
            match hw.gpu_hotspot_c {
                Some(t) if t.is_finite() => format!("{:>3.0}°", t.clamp(-99.0, 999.0)),
                _ => "  --".to_string(),
            },
        ),
        HwMetric::GpuPower => (
            "PWR",
            match hw.gpu_power_w {
                Some(p) if p.is_finite() => format!("{:>3.0}W", p.clamp(0.0, 999.0)),
                _ => "  --".to_string(),
            },
        ),
        HwMetric::GpuVram => (
            "VRM",
            match hw.gpu_vram_used_gb {
                Some(v) if v.is_finite() => {
                    let v = v.max(0.0);
                    if v >= 9.95 {
                        format!("{:>3.0}G", v)
                    } else {
                        format!("{:>3.1}G", v)
                    }
                }
                _ => "  --".to_string(),
            },
        ),
        HwMetric::GpuFan => (
            "FAN",
            match (hw.gpu_fan_rpm, hw.gpu_fan_pct) {
                (Some(r), _) => format_fan_rpm(Some(r)),
                (None, Some(pct)) => format!("{:>3}%", pct.min(100)),
                (None, None) => "  --".to_string(),
            },
        ),
    }
}

fn format_fan_rpm(rpm: Option<u32>) -> String {
    match rpm {
        Some(r) => {
            if r >= 1000 {
                format!("{:>3.1}k", r as f32 / 1000.0)
            } else {
                format!("{:>4}", r)
            }
        }
        None => "  --".to_string(),
    }
}

pub fn visible_hardware_items(snapshot: &MonitorSnapshot) -> Vec<HwMetric> {
    if !snapshot.config.hardware_enabled {
        return Vec::new();
    }
    if snapshot.config.hide_unavailable_metrics {
        snapshot
            .config
            .hardware_items
            .iter()
            .copied()
            .filter(|&m| snapshot.hw.is_metric_available(m))
            .collect()
    } else {
        snapshot.config.hardware_items.clone()
    }
}

pub fn active_wireless_devices(snapshot: &MonitorSnapshot) -> Vec<&WirelessDevice> {
    if !snapshot.config.wireless_enabled {
        return Vec::new();
    }
    snapshot
        .devices
        .iter()
        .filter(|d| {
            d.online
                && !snapshot.config.hidden_devices.contains(&d.id)
                && !(snapshot.config.hide_unknown_battery_devices && d.battery_pct.is_none())
        })
        .collect()
}

pub fn compute_hw_logical_width(items: &[HwMetric]) -> f32 {
    if items.is_empty() {
        return 0.0;
    }
    let cols: Vec<f32> = items
        .chunks(2)
        .map(|chunk| {
            chunk
                .iter()
                .map(|m| metric_col_width(*m))
                .fold(0.0_f32, f32::max)
        })
        .collect();
    let cols_sum: f32 = cols.iter().sum();
    let gaps = (cols.len().saturating_sub(1) as f32) * HW_COL_GAP;
    ZONE_PAD_X * 2.0 + cols_sum + gaps
}

pub fn compute_devices_logical_width(device_count: usize) -> f32 {
    if device_count == 0 {
        return 0.0;
    }
    let cols = device_count as f32 * DEVICE_COL_WIDTH;
    let gaps = (device_count.saturating_sub(1) as f32) * DEVICE_GAP;
    ZONE_PAD_X * 2.0 + cols + gaps
}

fn combine_zone_widths(hw_w: f32, dev_w: f32, content: RenderContent) -> f32 {
    match content {
        RenderContent::HardwareOnly => hw_w,
        RenderContent::DevicesOnly => dev_w,
        RenderContent::Combined => {
            if hw_w > 0.0 && dev_w > 0.0 {
                hw_w + DIVIDER_WIDTH + dev_w
            } else {
                hw_w + dev_w
            }
        }
    }
}

fn drop_last_hw_column(hw_items: &mut Vec<HwMetric>) {
    if hw_items.is_empty() {
        return;
    }
    let rem = hw_items.len() % 2;
    let drop_n = if rem == 0 { 2 } else { rem };
    hw_items.truncate(hw_items.len().saturating_sub(drop_n));
}

/// Trims trailing hardware/device columns in priority order so the rendered bar
/// stays strictly within `max_logical_w`.
pub fn fit_content_to_width(
    snapshot: &MonitorSnapshot,
    content: RenderContent,
    max_logical_w: f32,
) -> (Vec<HwMetric>, usize, f32) {
    if max_logical_w <= 0.0 || !max_logical_w.is_finite() {
        return (Vec::new(), 0, 0.0);
    }

    let mut hw_items = if content == RenderContent::DevicesOnly {
        Vec::new()
    } else {
        visible_hardware_items(snapshot)
    };
    let mut dev_count = if content == RenderContent::HardwareOnly {
        0
    } else {
        active_wireless_devices(snapshot).len()
    };

    loop {
        let hw_w = if content == RenderContent::DevicesOnly {
            0.0
        } else {
            compute_hw_logical_width(&hw_items)
        };
        let dev_w = if content == RenderContent::HardwareOnly {
            0.0
        } else {
            compute_devices_logical_width(dev_count)
        };
        let total_w = combine_zone_widths(hw_w, dev_w, content);
        if total_w <= max_logical_w + 0.01 {
            return (hw_items, dev_count, total_w);
        }

        let hw_cols = hw_items.len().div_ceil(2);
        if hw_cols == 0 && dev_count == 0 {
            return (Vec::new(), 0, 0.0);
        }

        match content {
            RenderContent::HardwareOnly => drop_last_hw_column(&mut hw_items),
            RenderContent::DevicesOnly => {
                dev_count = dev_count.saturating_sub(1);
            }
            RenderContent::Combined => {
                if hw_cols > 1 && dev_count > 1 {
                    if dev_w >= hw_w {
                        dev_count -= 1;
                    } else {
                        drop_last_hw_column(&mut hw_items);
                    }
                } else if dev_count > 1 {
                    dev_count -= 1;
                } else if hw_cols > 1 {
                    drop_last_hw_column(&mut hw_items);
                } else if hw_cols == 1 && dev_count == 1 {
                    match snapshot.config.combined_order {
                        CombinedOrder::DevicesFirst => drop_last_hw_column(&mut hw_items),
                        CombinedOrder::HardwareFirst => dev_count = 0,
                    }
                } else if dev_count > 0 {
                    dev_count = 0;
                } else {
                    drop_last_hw_column(&mut hw_items);
                }
            }
        }
    }
}

pub fn compute_logical_width(snapshot: &MonitorSnapshot, content: RenderContent) -> f32 {
    let hw_items = visible_hardware_items(snapshot);
    let hw_w = compute_hw_logical_width(&hw_items);
    let dev_count = active_wireless_devices(snapshot).len();
    let dev_w = compute_devices_logical_width(dev_count);
    combine_zone_widths(hw_w, dev_w, content)
}

struct DibSurface {
    hdc: HDC,
    hbitmap: HBITMAP,
    old_obj: HGDIOBJ,
    width_px: i32,
    height_px: i32,
}

impl DibSurface {
    fn new(width_px: i32, height_px: i32) -> windows::core::Result<Self> {
        unsafe {
            let hdc = CreateCompatibleDC(None);
            if hdc.is_invalid() {
                return Err(windows::core::Error::from_win32());
            }

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width_px,
                    // Negative height creates a top-down DIB matching Direct2D coordinates.
                    biHeight: -height_px,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut c_void = std::ptr::null_mut();
            let hbitmap = match CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(bmp) => bmp,
                Err(e) => {
                    let _ = DeleteDC(hdc);
                    return Err(e);
                }
            };

            let old_obj = SelectObject(hdc, HGDIOBJ(hbitmap.0));
            Ok(Self {
                hdc,
                hbitmap,
                old_obj,
                width_px,
                height_px,
            })
        }
    }
}

impl Drop for DibSurface {
    fn drop(&mut self) {
        unsafe {
            if !self.hdc.is_invalid() {
                if !self.old_obj.is_invalid() {
                    let _ = SelectObject(self.hdc, self.old_obj);
                }
                if !self.hbitmap.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(self.hbitmap.0));
                }
                let _ = DeleteDC(self.hdc);
            }
        }
    }
}

thread_local! {
    static SHARED_D2D_FACTORY: RefCell<Option<ID2D1Factory>> = const { RefCell::new(None) };
    static SHARED_DWRITE_FACTORY: RefCell<Option<IDWriteFactory>> = const { RefCell::new(None) };
    static SHARED_ICON_GEOMETRIES: RefCell<HashMap<(IconGeoKind, u32), ID2D1PathGeometry>> =
        RefCell::new(HashMap::new());
}

pub(crate) fn shared_d2d_factory() -> windows::core::Result<ID2D1Factory> {
    SHARED_D2D_FACTORY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(factory) = slot.as_ref() {
            return Ok(factory.clone());
        }
        let factory: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        *slot = Some(factory.clone());
        Ok(factory)
    })
}

pub(crate) fn shared_dwrite_factory() -> windows::core::Result<IDWriteFactory> {
    SHARED_DWRITE_FACTORY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(factory) = slot.as_ref() {
            return Ok(factory.clone());
        }
        let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        *slot = Some(factory.clone());
        Ok(factory)
    })
}

pub fn release_thread_local_d2d() {
    SHARED_ICON_GEOMETRIES.with(|c| c.borrow_mut().clear());
    SHARED_DWRITE_FACTORY.with(|c| *c.borrow_mut() = None);
    SHARED_D2D_FACTORY.with(|c| *c.borrow_mut() = None);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum IconGeoKind {
    Headset,
    Gamepad,
}

pub struct TaskbarRenderer {
    d2d_factory: ID2D1Factory,
    render_target: Option<ID2D1DCRenderTarget>,
    base_render_target: Option<ID2D1RenderTarget>,
    brush: Option<ID2D1SolidColorBrush>,
    icon_geometries: HashMap<(IconGeoKind, u32), ID2D1PathGeometry>,
    text_buf: Vec<u16>,
    target_dpi: u32,
    hw_label_fmt: IDWriteTextFormat,
    hw_val_fmt: IDWriteTextFormat,
    dev_pct_fmt: IDWriteTextFormat,
}

pub struct WindowSurface {
    dib: Option<DibSurface>,
}

impl WindowSurface {
    pub fn new() -> Self {
        Self { dib: None }
    }

    pub(crate) fn clear(&mut self) {
        self.dib = None;
    }

    pub(crate) fn ensure_size(
        &mut self,
        width_px: i32,
        height_px: i32,
    ) -> windows::core::Result<HDC> {
        let needs_recreate = match &self.dib {
            Some(d) => d.width_px != width_px || d.height_px != height_px,
            None => true,
        };
        if needs_recreate {
            self.dib = None;
            self.dib = Some(DibSurface::new(width_px, height_px)?);
        }
        Ok(self.dib.as_ref().unwrap().hdc)
    }
}

impl Default for WindowSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskbarRenderer {
    pub fn new() -> windows::core::Result<Self> {
        unsafe {
            let d2d_factory = shared_d2d_factory()?;
            let dwrite_factory = shared_dwrite_factory()?;

            let hw_label_fmt = create_text_format(
                &dwrite_factory,
                w!("Consolas"),
                DWRITE_FONT_WEIGHT_MEDIUM,
                11.0,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                false,
            )?;
            let hw_val_fmt = create_text_format(
                &dwrite_factory,
                w!("Consolas"),
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                11.5,
                DWRITE_TEXT_ALIGNMENT_TRAILING,
                false,
            )?;
            let dev_pct_fmt = create_text_format(
                &dwrite_factory,
                w!("Segoe UI"),
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                10.5,
                DWRITE_TEXT_ALIGNMENT_CENTER,
                false,
            )?;

            Ok(Self {
                d2d_factory,
                render_target: None,
                base_render_target: None,
                brush: None,
                icon_geometries: HashMap::new(),
                text_buf: Vec::with_capacity(64),
                target_dpi: 0,
                hw_label_fmt,
                hw_val_fmt,
                dev_pct_fmt,
            })
        }
    }

    fn ensure_render_target(&mut self, dpi: u32) -> windows::core::Result<()> {
        let dpi = dpi.max(96);
        if self.render_target.is_none()
            || self.base_render_target.is_none()
            || self.brush.is_none()
            || self.target_dpi != dpi
        {
            self.render_target = None;
            self.base_render_target = None;
            self.brush = None;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: dpi as f32,
                dpiY: dpi as f32,
                ..Default::default()
            };
            unsafe {
                let rt = self.d2d_factory.CreateDCRenderTarget(&props)?;
                rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                let base_rt: ID2D1RenderTarget = windows::core::Interface::cast(&rt)?;
                let brush = base_rt.CreateSolidColorBrush(&rgba(255, 255, 255, 1.0), None)?;
                self.render_target = Some(rt);
                self.base_render_target = Some(base_rt);
                self.brush = Some(brush);
                self.target_dpi = dpi;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_to_window(
        &mut self,
        hwnd: HWND,
        surface: &mut WindowSurface,
        snapshot: &MonitorSnapshot,
        content: RenderContent,
        theme: TaskbarTheme,
        dpi: u32,
        pos_x: i32,
        pos_y: i32,
        width_px: i32,
        height_px: i32,
        hovered: bool,
    ) -> windows::core::Result<()> {
        if width_px <= 0 || height_px <= 0 {
            return Ok(());
        }

        let hdc = surface.ensure_size(width_px, height_px)?;
        let scale = (dpi.max(96) as f32) / 96.0;
        let logical_w = width_px as f32 / scale;
        let logical_h = height_px as f32 / scale;

        for attempt in 0..2 {
            self.ensure_render_target(dpi)?;
            let rt = self.render_target.as_ref().unwrap().clone();
            let base_rt = self.base_render_target.as_ref().unwrap().clone();
            let brush = self.brush.as_ref().unwrap().clone();
            let bind_rect = RECT {
                left: 0,
                top: 0,
                right: width_px,
                bottom: height_px,
            };
            unsafe {
                rt.BindDC(hdc, &bind_rect)?;
                rt.BeginDraw();
                rt.Clear(Some(&D2D1_COLOR_F {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.0,
                }));

                self.draw_scene(
                    &base_rt, &brush, snapshot, content, theme, logical_w, logical_h, hovered,
                );

                match rt.EndDraw(None, None) {
                    Ok(()) => break,
                    Err(e) if attempt == 0 => {
                        let _ = e;
                        self.render_target = None;
                        self.base_render_target = None;
                        self.brush = None;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        unsafe {
            let dst_pt = POINT { x: pos_x, y: pos_y };
            let size = SIZE {
                cx: width_px,
                cy: height_px,
            };
            let src_pt = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            UpdateLayeredWindow(
                hwnd,
                HDC::default(),
                Some(&dst_pt),
                Some(&size),
                hdc,
                Some(&src_pt),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn draw_scene(
        &mut self,
        base_rt: &ID2D1RenderTarget,
        brush: &ID2D1SolidColorBrush,
        snapshot: &MonitorSnapshot,
        content: RenderContent,
        theme: TaskbarTheme,
        logical_w: f32,
        logical_h: f32,
        hovered: bool,
    ) {
        let palette = TaskbarPalette::for_theme(theme);

        brush.SetColor(&palette.hit_bg);
        let full_rect = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: logical_w,
            bottom: logical_h,
        };
        base_rt.FillRectangle(&full_rect, brush);

        if hovered {
            brush.SetColor(&palette.hover_bg);
            let pill = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: 1.0,
                    top: 3.0,
                    right: (logical_w - 1.0).max(2.0),
                    bottom: (logical_h - 3.0).max(4.0),
                },
                radiusX: 5.0,
                radiusY: 5.0,
            };
            base_rt.FillRoundedRectangle(&pill, brush);
        }

        let all_devices = active_wireless_devices(snapshot);
        let (hw_items, dev_count, _) = fit_content_to_width(snapshot, content, logical_w);
        let devices = &all_devices[..dev_count.min(all_devices.len())];
        let hw_w = compute_hw_logical_width(&hw_items);
        let dev_w = compute_devices_logical_width(devices.len());

        match content {
            RenderContent::HardwareOnly => {
                if hw_w > 0.0 {
                    self.draw_hardware_zone(
                        base_rt, brush, snapshot, &hw_items, &palette, 0.0, logical_h,
                    );
                }
            }
            RenderContent::DevicesOnly => {
                if dev_w > 0.0 {
                    self.draw_devices_zone(
                        base_rt,
                        brush,
                        devices,
                        snapshot.config.low_battery_threshold,
                        &palette,
                        0.0,
                        logical_h,
                    );
                }
            }
            RenderContent::Combined => {
                let has_hw = hw_w > 0.0;
                let has_dev = dev_w > 0.0;
                match snapshot.config.combined_order {
                    CombinedOrder::HardwareFirst => {
                        let mut x = 0.0;
                        if has_hw {
                            self.draw_hardware_zone(
                                base_rt, brush, snapshot, &hw_items, &palette, x, logical_h,
                            );
                            x += hw_w;
                        }
                        if has_hw && has_dev {
                            self.draw_divider(base_rt, brush, &palette, x, logical_h);
                            x += DIVIDER_WIDTH;
                        }
                        if has_dev {
                            self.draw_devices_zone(
                                base_rt,
                                brush,
                                devices,
                                snapshot.config.low_battery_threshold,
                                &palette,
                                x,
                                logical_h,
                            );
                        }
                    }
                    CombinedOrder::DevicesFirst => {
                        let mut x = 0.0;
                        if has_dev {
                            self.draw_devices_zone(
                                base_rt,
                                brush,
                                devices,
                                snapshot.config.low_battery_threshold,
                                &palette,
                                x,
                                logical_h,
                            );
                            x += dev_w;
                        }
                        if has_hw && has_dev {
                            self.draw_divider(base_rt, brush, &palette, x, logical_h);
                            x += DIVIDER_WIDTH;
                        }
                        if has_hw {
                            self.draw_hardware_zone(
                                base_rt, brush, snapshot, &hw_items, &palette, x, logical_h,
                            );
                        }
                    }
                }
            }
        }
    }

    unsafe fn draw_divider(
        &self,
        rt: &ID2D1RenderTarget,
        brush: &ID2D1SolidColorBrush,
        palette: &TaskbarPalette,
        x: f32,
        logical_h: f32,
    ) {
        brush.SetColor(&palette.divider);
        let line_x = x + 0.5;
        let pad_y = (logical_h * 0.24).clamp(6.0, 12.0);
        rt.DrawLine(
            D2D_POINT_2F {
                x: line_x,
                y: pad_y,
            },
            D2D_POINT_2F {
                x: line_x,
                y: (logical_h - pad_y).max(pad_y + 2.0),
            },
            brush,
            1.0,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn draw_hardware_zone(
        &mut self,
        rt: &ID2D1RenderTarget,
        brush: &ID2D1SolidColorBrush,
        snapshot: &MonitorSnapshot,
        hw_items: &[HwMetric],
        palette: &TaskbarPalette,
        start_x: f32,
        logical_h: f32,
    ) {
        let row_h = ((logical_h - 10.0) / 2.0).clamp(14.0, 18.0);
        let total_rows_h = row_h * 2.0;
        let top_y = ((logical_h - total_rows_h) / 2.0).max(2.0);
        let bot_y = top_y + row_h;

        let mut col_x = start_x + ZONE_PAD_X;
        for chunk in hw_items.chunks(2) {
            let col_w = chunk
                .iter()
                .map(|m| metric_col_width(*m))
                .fold(0.0_f32, f32::max);

            for (row_idx, metric) in chunk.iter().enumerate() {
                let row_y = if row_idx == 0 { top_y } else { bot_y };
                let (label, value) = format_hw_metric(*metric, &snapshot.hw);
                let label_color = match metric {
                    HwMetric::NetUp => &palette.up_accent,
                    HwMetric::NetDown => &palette.down_accent,
                    _ => &palette.text_secondary,
                };
                let val_color = palette.hw_value_color(
                    *metric,
                    &snapshot.hw,
                    snapshot.config.low_battery_threshold,
                );

                let cell_rect = D2D_RECT_F {
                    left: col_x,
                    top: row_y,
                    right: col_x + col_w,
                    bottom: row_y + row_h,
                };

                draw_text_clipped(
                    rt,
                    brush,
                    &mut self.text_buf,
                    label,
                    &self.hw_label_fmt,
                    label_color,
                    cell_rect,
                );
                draw_text_clipped(
                    rt,
                    brush,
                    &mut self.text_buf,
                    &value,
                    &self.hw_val_fmt,
                    &val_color,
                    cell_rect,
                );
            }

            col_x += col_w + HW_COL_GAP;
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn draw_devices_zone(
        &mut self,
        rt: &ID2D1RenderTarget,
        brush: &ID2D1SolidColorBrush,
        devices: &[&WirelessDevice],
        low_threshold: u8,
        palette: &TaskbarPalette,
        start_x: f32,
        logical_h: f32,
    ) {
        let top_h = (logical_h * 0.46).clamp(15.0, 22.0);
        let bot_h = (logical_h * 0.38).clamp(13.0, 17.0);
        let top_y = ((logical_h - (top_h + bot_h)) / 2.0).max(2.0);
        let bot_y = top_y + top_h;

        let mut col_x = start_x + ZONE_PAD_X;
        for dev in devices {
            let accent_color = palette.device_color(dev, low_threshold);
            brush.SetColor(&accent_color);

            // Top row: 16x16 vector icon centered in column.
            let icon_size = 15.0;
            let icon_x = col_x + (DEVICE_COL_WIDTH - icon_size) * 0.5;
            let icon_y = top_y + (top_h - icon_size) * 0.5;
            draw_device_icon_cached(
                &self.d2d_factory,
                &mut self.icon_geometries,
                rt,
                brush,
                dev.kind,
                icon_x,
                icon_y,
                icon_size,
            );

            // Bottom row: battery text centered in column.
            let pct_text = format_device_battery(dev);
            let text_rect = D2D_RECT_F {
                left: col_x,
                top: bot_y,
                right: col_x + DEVICE_COL_WIDTH,
                bottom: bot_y + bot_h,
            };
            draw_text_clipped(
                rt,
                brush,
                &mut self.text_buf,
                &pct_text,
                &self.dev_pct_fmt,
                &accent_color,
                text_rect,
            );

            col_x += DEVICE_COL_WIDTH + DEVICE_GAP;
        }
    }
}

unsafe fn get_or_create_icon_geometry(
    d2d_factory: &ID2D1Factory,
    cache: &mut HashMap<(IconGeoKind, u32), ID2D1PathGeometry>,
    geo_kind: IconGeoKind,
    s: f32,
) -> Option<ID2D1PathGeometry> {
    let key = (geo_kind, s.to_bits());
    if let Some(existing) = cache.get(&key) {
        return Some(existing.clone());
    }
    let path = d2d_factory.CreatePathGeometry().ok()?;
    let sink = path.Open().ok()?;
    match geo_kind {
        IconGeoKind::Headset => {
            sink.BeginFigure(
                D2D_POINT_2F {
                    x: s * 0.18,
                    y: s * 0.54,
                },
                D2D1_FIGURE_BEGIN_HOLLOW,
            );
            sink.AddBezier(
                &windows::Win32::Graphics::Direct2D::Common::D2D1_BEZIER_SEGMENT {
                    point1: D2D_POINT_2F {
                        x: s * 0.18,
                        y: s * 0.06,
                    },
                    point2: D2D_POINT_2F {
                        x: s * 0.82,
                        y: s * 0.06,
                    },
                    point3: D2D_POINT_2F {
                        x: s * 0.82,
                        y: s * 0.54,
                    },
                },
            );
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
        }
        IconGeoKind::Gamepad => {
            sink.BeginFigure(
                D2D_POINT_2F {
                    x: s * 0.14,
                    y: s * 0.60,
                },
                D2D1_FIGURE_BEGIN_FILLED,
            );
            sink.AddLine(D2D_POINT_2F {
                x: s * 0.10,
                y: s * 0.86,
            });
            sink.AddLine(D2D_POINT_2F {
                x: s * 0.26,
                y: s * 0.70,
            });
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);

            sink.BeginFigure(
                D2D_POINT_2F {
                    x: s * 0.86,
                    y: s * 0.60,
                },
                D2D1_FIGURE_BEGIN_FILLED,
            );
            sink.AddLine(D2D_POINT_2F {
                x: s * 0.90,
                y: s * 0.86,
            });
            sink.AddLine(D2D_POINT_2F {
                x: s * 0.74,
                y: s * 0.70,
            });
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        }
    }
    sink.Close().ok()?;
    cache.insert(key, path.clone());
    Some(path)
}

unsafe fn with_icon_translation<F>(rt: &ID2D1RenderTarget, x: f32, y: f32, f: F)
where
    F: FnOnce(),
{
    let mut prev = Matrix3x2::default();
    rt.GetTransform(&mut prev);
    let translated = Matrix3x2 {
        M11: prev.M11,
        M12: prev.M12,
        M21: prev.M21,
        M22: prev.M22,
        M31: x * prev.M11 + y * prev.M21 + prev.M31,
        M32: x * prev.M12 + y * prev.M22 + prev.M32,
    };
    rt.SetTransform(&translated);
    f();
    rt.SetTransform(&prev);
}

#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn draw_device_icon_with_factory(
    d2d_factory: &ID2D1Factory,
    rt: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    kind: DeviceKind,
    x: f32,
    y: f32,
    s: f32,
) {
    SHARED_ICON_GEOMETRIES.with(|cache| {
        let mut cache = cache.borrow_mut();
        draw_device_icon_cached(d2d_factory, &mut cache, rt, brush, kind, x, y, s);
    });
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_device_icon_cached(
    d2d_factory: &ID2D1Factory,
    geo_cache: &mut HashMap<(IconGeoKind, u32), ID2D1PathGeometry>,
    rt: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    kind: DeviceKind,
    x: f32,
    y: f32,
    s: f32,
) {
    let stroke = 1.3;
    match kind {
        DeviceKind::Mouse => {
            let body = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: x + s * 0.22,
                    top: y + s * 0.06,
                    right: x + s * 0.78,
                    bottom: y + s * 0.94,
                },
                radiusX: s * 0.26,
                radiusY: s * 0.26,
            };
            rt.DrawRoundedRectangle(&body, brush, stroke, None);
            // Scroll wheel
            rt.DrawLine(
                D2D_POINT_2F {
                    x: x + s * 0.50,
                    y: y + s * 0.20,
                },
                D2D_POINT_2F {
                    x: x + s * 0.50,
                    y: y + s * 0.38,
                },
                brush,
                stroke * 1.1,
                None,
            );
            // Button split
            rt.DrawLine(
                D2D_POINT_2F {
                    x: x + s * 0.24,
                    y: y + s * 0.44,
                },
                D2D_POINT_2F {
                    x: x + s * 0.76,
                    y: y + s * 0.44,
                },
                brush,
                1.0,
                None,
            );
        }
        DeviceKind::Keyboard => {
            let body = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: x + s * 0.04,
                    top: y + s * 0.22,
                    right: x + s * 0.96,
                    bottom: y + s * 0.78,
                },
                radiusX: 2.0,
                radiusY: 2.0,
            };
            rt.DrawRoundedRectangle(&body, brush, stroke, None);
            // Key rows
            for row in [0.40_f32, 0.54_f32] {
                for col in [0.18_f32, 0.38_f32, 0.58_f32, 0.74_f32] {
                    rt.FillRectangle(
                        &D2D_RECT_F {
                            left: x + s * col,
                            top: y + s * row,
                            right: x + s * (col + 0.10),
                            bottom: y + s * (row + 0.09),
                        },
                        brush,
                    );
                }
            }
            // Spacebar
            rt.FillRectangle(
                &D2D_RECT_F {
                    left: x + s * 0.30,
                    top: y + s * 0.67,
                    right: x + s * 0.70,
                    bottom: y + s * 0.73,
                },
                brush,
            );
        }
        DeviceKind::Headset => {
            // Arch headband via cached PathGeometry
            if let Some(path) =
                get_or_create_icon_geometry(d2d_factory, geo_cache, IconGeoKind::Headset, s)
            {
                with_icon_translation(rt, x, y, || {
                    rt.DrawGeometry(&path, brush, stroke, None);
                });
            }
            // Earcups
            for (l, r) in [(0.10_f32, 0.30_f32), (0.70_f32, 0.90_f32)] {
                let cup = D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: x + s * l,
                        top: y + s * 0.46,
                        right: x + s * r,
                        bottom: y + s * 0.84,
                    },
                    radiusX: 2.0,
                    radiusY: 2.0,
                };
                rt.FillRoundedRectangle(&cup, brush);
            }
        }
        DeviceKind::Earbuds => {
            // Left earbud + stem
            let left_bud = D2D1_ELLIPSE {
                point: D2D_POINT_2F {
                    x: x + s * 0.28,
                    y: y + s * 0.36,
                },
                radiusX: s * 0.17,
                radiusY: s * 0.18,
            };
            rt.FillEllipse(&left_bud, brush);
            rt.DrawLine(
                D2D_POINT_2F {
                    x: x + s * 0.32,
                    y: y + s * 0.48,
                },
                D2D_POINT_2F {
                    x: x + s * 0.24,
                    y: y + s * 0.84,
                },
                brush,
                1.6,
                None,
            );

            // Right earbud + stem
            let right_bud = D2D1_ELLIPSE {
                point: D2D_POINT_2F {
                    x: x + s * 0.72,
                    y: y + s * 0.36,
                },
                radiusX: s * 0.17,
                radiusY: s * 0.18,
            };
            rt.FillEllipse(&right_bud, brush);
            rt.DrawLine(
                D2D_POINT_2F {
                    x: x + s * 0.68,
                    y: y + s * 0.48,
                },
                D2D_POINT_2F {
                    x: x + s * 0.76,
                    y: y + s * 0.84,
                },
                brush,
                1.6,
                None,
            );
        }
        DeviceKind::Gamepad => {
            // Controller body
            let body = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: x + s * 0.08,
                    top: y + s * 0.26,
                    right: x + s * 0.92,
                    bottom: y + s * 0.72,
                },
                radiusX: s * 0.20,
                radiusY: s * 0.20,
            };
            rt.DrawRoundedRectangle(&body, brush, stroke, None);
            // Left/right handles via cached PathGeometry
            if let Some(path) =
                get_or_create_icon_geometry(d2d_factory, geo_cache, IconGeoKind::Gamepad, s)
            {
                with_icon_translation(rt, x, y, || {
                    rt.FillGeometry(&path, brush, None);
                });
            }
            // D-pad & action dots
            rt.DrawLine(
                D2D_POINT_2F {
                    x: x + s * 0.26,
                    y: y + s * 0.49,
                },
                D2D_POINT_2F {
                    x: x + s * 0.40,
                    y: y + s * 0.49,
                },
                brush,
                1.2,
                None,
            );
            rt.DrawLine(
                D2D_POINT_2F {
                    x: x + s * 0.33,
                    y: y + s * 0.42,
                },
                D2D_POINT_2F {
                    x: x + s * 0.33,
                    y: y + s * 0.56,
                },
                brush,
                1.2,
                None,
            );
            rt.FillEllipse(
                &D2D1_ELLIPSE {
                    point: D2D_POINT_2F {
                        x: x + s * 0.69,
                        y: y + s * 0.44,
                    },
                    radiusX: 1.1,
                    radiusY: 1.1,
                },
                brush,
            );
            rt.FillEllipse(
                &D2D1_ELLIPSE {
                    point: D2D_POINT_2F {
                        x: x + s * 0.63,
                        y: y + s * 0.54,
                    },
                    radiusX: 1.1,
                    radiusY: 1.1,
                },
                brush,
            );
        }
        DeviceKind::Other => {
            let outer = D2D1_ELLIPSE {
                point: D2D_POINT_2F {
                    x: x + s * 0.50,
                    y: y + s * 0.50,
                },
                radiusX: s * 0.36,
                radiusY: s * 0.36,
            };
            let inner = D2D1_ELLIPSE {
                point: D2D_POINT_2F {
                    x: x + s * 0.50,
                    y: y + s * 0.50,
                },
                radiusX: s * 0.14,
                radiusY: s * 0.14,
            };
            rt.DrawEllipse(&outer, brush, stroke, None);
            rt.FillEllipse(&inner, brush);
        }
    }
}

pub fn format_device_battery(dev: &WirelessDevice) -> String {
    match (dev.charging, dev.battery_pct) {
        (true, Some(pct)) => format!("⚡{}%", pct),
        (true, None) => "⚡".to_string(),
        (false, Some(pct)) => format!("{}%", pct),
        (false, None) => "--".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::state::{Transport, WirelessDevice};
    use std::time::Instant;

    #[test]
    fn hw_metric_formatting_covers_all_variants() {
        let mut hw = HardwareMetrics {
            cpu_usage_pct: 23.4,
            cpu_freq_ghz: Some(4.85),
            cpu_temp_c: Some(61.6),
            cpu_power_w: Some(64.8),
            cpu_fan_rpm: Some(1250),
            mem_usage_pct: 48.2,
            mem_used_gb: 15.4,
            mem_total_gb: 32.0,
            net_up_bps: 1_536_000.0,
            net_down_bps: 25_000_000.0,
            net_adapter_name: "Ethernet".to_string(),
            disk_read_bps: Some(1_048_576.0),
            disk_write_bps: Some(512_000.0),
            disk_temp_c: Some(41.2),
            sys_battery_pct: Some(85),
            sys_battery_charging: false,
            gpu_name: "RX 9070 XT".to_string(),
            gpu_usage_pct: Some(67.0),
            gpu_temp_c: Some(54.0),
            gpu_hotspot_c: Some(69.0),
            gpu_power_w: Some(185.0),
            gpu_fan_rpm: Some(1450),
            gpu_fan_pct: Some(45),
            gpu_vram_used_gb: Some(7.4),
            gpu_vram_total_gb: Some(16.0),
            ..Default::default()
        };
        for &m in HwMetric::ALL {
            let (lbl, val) = format_hw_metric(m, &hw);
            assert!(!lbl.is_empty());
            assert!(!val.is_empty());
        }

        assert_eq!(
            format_hw_metric(HwMetric::CpuTemp, &hw),
            ("CTM", " 62°".to_string())
        );
        assert_eq!(
            format_hw_metric(HwMetric::CpuPower, &hw),
            ("CPW", " 65W".to_string())
        );
        assert_eq!(
            format_hw_metric(HwMetric::CpuFan, &hw),
            ("CFN", "1.2k".to_string())
        );
        assert_eq!(
            format_hw_metric(HwMetric::DiskRead, &hw),
            ("R", " 1.0M".to_string())
        );
        assert_eq!(
            format_hw_metric(HwMetric::DiskWrite, &hw),
            ("W", " 500K".to_string())
        );
        assert_eq!(
            format_hw_metric(HwMetric::DiskTemp, &hw),
            ("DTM", " 41°".to_string())
        );
        assert_eq!(
            format_hw_metric(HwMetric::SysBattery, &hw),
            ("BAT", " 85%".to_string())
        );
        assert_eq!(
            format_hw_metric(HwMetric::GpuFan, &hw),
            ("FAN", "1.5k".to_string())
        );

        // GpuFan falls back to gpu_fan_pct when gpu_fan_rpm is None
        hw.gpu_fan_rpm = None;
        assert_eq!(
            format_hw_metric(HwMetric::GpuFan, &hw),
            ("FAN", " 45%".to_string())
        );
        hw.gpu_fan_pct = None;
        assert_eq!(
            format_hw_metric(HwMetric::GpuFan, &hw),
            ("FAN", "  --".to_string())
        );

        // SysBattery charging format
        hw.sys_battery_charging = true;
        assert_eq!(
            format_hw_metric(HwMetric::SysBattery, &hw),
            ("BAT", "⚡85%".to_string())
        );
    }

    #[test]
    fn hide_unavailable_metrics_filters_items_and_narrows_width() {
        let mut snap = MonitorSnapshot::new(AppConfig::default());
        snap.config.hardware_items = vec![
            HwMetric::NetUp,
            HwMetric::NetDown,
            HwMetric::CpuUsage,
            HwMetric::CpuTemp,
            HwMetric::DiskTemp,
            HwMetric::GpuFan,
        ];
        snap.config.hide_unavailable_metrics = false;
        assert_eq!(visible_hardware_items(&snap).len(), 6);
        let full_w = compute_logical_width(&snap, RenderContent::HardwareOnly);

        snap.config.hide_unavailable_metrics = true;
        // Only NetUp, NetDown, CpuUsage are available by default
        assert_eq!(
            visible_hardware_items(&snap),
            vec![HwMetric::NetUp, HwMetric::NetDown, HwMetric::CpuUsage]
        );
        let filtered_w = compute_logical_width(&snap, RenderContent::HardwareOnly);
        assert!(filtered_w < full_w);

        // Making GpuFan available via fan_pct brings it back
        snap.hw.gpu_fan_pct = Some(30);
        assert_eq!(
            visible_hardware_items(&snap),
            vec![
                HwMetric::NetUp,
                HwMetric::NetDown,
                HwMetric::CpuUsage,
                HwMetric::GpuFan,
            ]
        );
    }

    #[test]
    fn dynamic_width_zero_when_no_devices() {
        let cfg = AppConfig::default();
        let snap = MonitorSnapshot::new(cfg);
        assert_eq!(
            compute_logical_width(&snap, RenderContent::DevicesOnly),
            0.0
        );
        let hw_w = compute_logical_width(&snap, RenderContent::HardwareOnly);
        let comb_w = compute_logical_width(&snap, RenderContent::Combined);
        assert!((hw_w - comb_w).abs() < f32::EPSILON);
    }

    #[test]
    fn dynamic_width_grows_with_online_devices() {
        let mut snap = MonitorSnapshot::new(AppConfig::default());
        let base_w = compute_logical_width(&snap, RenderContent::Combined);
        snap.devices.push(WirelessDevice {
            id: "d1".into(),
            name: "VT3".into(),
            kind: DeviceKind::Mouse,
            transport: Transport::Wireless24G,
            battery_pct: Some(85),
            charging: false,
            sub_batteries: None,
            online: true,
            last_seen: Instant::now(),
            detail_note: None,
        });
        let one_dev_w = compute_logical_width(&snap, RenderContent::Combined);
        assert!(one_dev_w > base_w);
    }

    #[test]
    fn hide_unknown_battery_devices_filters_taskbar_and_narrows_combined_seamlessly() {
        let mut snap = MonitorSnapshot::new(AppConfig::default());
        assert!(snap.config.hide_unknown_battery_devices);
        snap.devices.push(WirelessDevice {
            id: "skn".into(),
            name: "SKN 青龙 4.0".into(),
            kind: DeviceKind::Keyboard,
            transport: Transport::Wireless24G,
            battery_pct: None,
            charging: false,
            sub_batteries: None,
            online: true,
            last_seen: Instant::now(),
            detail_note: None,
        });

        assert!(active_wireless_devices(&snap).is_empty());
        assert_eq!(
            compute_logical_width(&snap, RenderContent::DevicesOnly),
            0.0
        );
        let hw_w = compute_logical_width(&snap, RenderContent::HardwareOnly);
        let comb_w = compute_logical_width(&snap, RenderContent::Combined);
        assert!((hw_w - comb_w).abs() < f32::EPSILON);

        snap.config.hide_unknown_battery_devices = false;
        assert_eq!(active_wireless_devices(&snap).len(), 1);
        assert!(compute_logical_width(&snap, RenderContent::DevicesOnly) > 0.0);
        assert!(compute_logical_width(&snap, RenderContent::Combined) > hw_w);
    }

    #[test]
    fn unavailable_validity_flags_render_na_and_respect_hide_unavailable() {
        let mut snap = MonitorSnapshot::new(AppConfig::default());
        snap.hw = HardwareMetrics::unavailable();

        assert_eq!(
            format_hw_metric(HwMetric::CpuUsage, &snap.hw).1.trim(),
            "N/A"
        );
        assert_eq!(
            format_hw_metric(HwMetric::MemUsage, &snap.hw).1.trim(),
            "N/A"
        );
        assert_eq!(format_hw_metric(HwMetric::NetUp, &snap.hw).1.trim(), "N/A");
        assert_eq!(
            format_hw_metric(HwMetric::NetDown, &snap.hw).1.trim(),
            "N/A"
        );

        snap.config.hide_unavailable_metrics = false;
        assert_eq!(visible_hardware_items(&snap).len(), 4);

        snap.config.hide_unavailable_metrics = true;
        assert!(visible_hardware_items(&snap).is_empty());
    }

    #[test]
    fn fit_content_to_width_trims_overflow_columns_gracefully() {
        let mut snap = MonitorSnapshot::new(AppConfig::default());
        snap.config.hardware_items = vec![
            HwMetric::NetUp,
            HwMetric::NetDown,
            HwMetric::CpuUsage,
            HwMetric::MemUsage,
            HwMetric::GpuUsage,
            HwMetric::GpuTemp,
        ];
        for i in 0..5 {
            snap.devices.push(WirelessDevice {
                id: format!("dev{i}"),
                name: format!("Dev{i}"),
                kind: DeviceKind::Mouse,
                transport: Transport::Wireless24G,
                battery_pct: Some(80),
                charging: false,
                sub_batteries: None,
                online: true,
                last_seen: Instant::now(),
                detail_note: None,
            });
        }

        let full_w = compute_logical_width(&snap, RenderContent::Combined);
        assert!(full_w > 300.0);

        // Narrow budget trims columns so fitted width never exceeds budget.
        let (hw, dev_n, fitted_w) = fit_content_to_width(&snap, RenderContent::Combined, 220.0);
        assert!(fitted_w <= 220.01);
        assert!(!hw.is_empty());
        assert!((1..5).contains(&dev_n));

        // Tiny budget below a single column yields empty cleanly.
        let (hw0, dev0, w0) = fit_content_to_width(&snap, RenderContent::Combined, 10.0);
        assert!(hw0.is_empty());
        assert_eq!(dev0, 0);
        assert_eq!(w0, 0.0);

        // Disabled zones yield empty regardless of budget.
        snap.config.hardware_enabled = false;
        snap.config.wireless_enabled = false;
        assert!(visible_hardware_items(&snap).is_empty());
        assert!(active_wireless_devices(&snap).is_empty());
    }
}
