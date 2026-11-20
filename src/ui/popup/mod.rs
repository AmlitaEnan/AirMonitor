pub mod actions;
pub mod layout;
pub mod widgets;

use std::cell::RefCell;
use std::mem::size_of;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_IGNORE, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::{
    ID2D1DCRenderTarget, ID2D1Factory, ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
    D2D1_DASH_STYLE_DASH, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
    D2D1_STROKE_STYLE_PROPERTIES, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DWMWINDOWATTRIBUTE,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint,
    MonitorFromWindow, ScreenToClient, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetForegroundWindow,
    GetWindowLongPtrW, LoadCursorW, PostMessageW, RegisterClassExW, SetCursor, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HCURSOR,
    HTCLIENT, HWND_TOPMOST, IDC_ARROW, IDC_HAND, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE,
    WA_INACTIVE, WM_ACTIVATE, WM_APP, WM_CAPTURECHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_SETCURSOR, WNDCLASSEXW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::state::MonitorSnapshot;
use crate::ui::popup::actions::{
    apply_popup_action, try_begin_download_update, try_begin_update_check, ActionEffect,
};
use crate::ui::popup::layout::{
    compute_popup_height_dip, compute_popup_rect, hit_test_boxes, hit_test_metric_slot,
    hit_test_pt, HitBox, MetricSlotRect, PopupAction, PopupTab, SettingsSubTab,
    POPUP_WIDTH_LOGICAL,
};
use crate::ui::popup::widgets::{draw_popup_scene, PopupFonts, SceneInput};
use crate::ui::renderer::{
    read_taskbar_theme, shared_d2d_factory, shared_dwrite_factory, TaskbarTheme, WindowSurface,
};
use crate::ui::taskbar::find_taskbar_geometry;
use crate::ui::theme::{rgba, PopupPalette};
use crate::ui::tray::is_autostart_enabled;
use crate::ui::{HwWakeSignal, SnapshotStore, VersionedSnapshot};
use crate::updater::{is_self_update_dir_writable, UpdateStatus};

pub const WM_APP_CONFIG_CHANGED: u32 = WM_APP + 5;
pub const WM_APP_REFRESH_WIRELESS: u32 = WM_APP + 6;
pub const WM_APP_UPDATE_STATUS: u32 = WM_APP + 7;
pub const WM_APP_UPDATE_RESTART: u32 = WM_APP + 8;
const WM_APP_POPUP_DEFERRED_HIDE: u32 = WM_APP + 20;
const WM_APP_POPUP_DEFERRED_REFRESH: u32 = WM_APP + 21;

const POPUP_CLASS_NAME: PCWSTR = w!("AirMonitor_PopupFlyout");
const DWMWA_BORDER_COLOR: DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(34i32);
const FOCUS_FALLBACK_GRACE: Duration = Duration::from_millis(500);

pub fn register_popup_class(hinstance: HINSTANCE) -> windows::core::Result<()> {
    unsafe {
        let cursor = LoadCursorW(HINSTANCE::default(), IDC_ARROW)?;
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(popup_wnd_proc),
            hInstance: hinstance,
            hCursor: cursor,
            lpszClassName: POPUP_CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(windows::core::Error::from_win32());
        }
        Ok(())
    }
}

pub struct PopupManager {
    pub hwnd: HWND,
    host_hwnd: HWND,
    state: SnapshotStore,
    popup_open: Arc<AtomicBool>,
    hw_signal: HwWakeSignal,
    cached_snapshot: Option<Arc<MonitorSnapshot>>,
    last_rendered_rev: Option<(u64, u64, u64)>,
    cached_autostart: bool,
    visible: bool,
    shown_at: Option<Instant>,
    last_hidden_at: Option<Instant>,
    anchor_click_x: i32,
    anchor_click_y: i32,
    active_tab: PopupTab,
    settings_sub_tab: SettingsSubTab,
    selected_metric_idx: Option<usize>,
    scroll_y: f32,
    max_scroll_y: f32,
    hovered_idx: Option<usize>,
    pressed_idx: Option<usize>,
    tracking_leave: bool,
    hit_boxes: Vec<HitBox>,
    metric_slot_rects: Vec<MetricSlotRect>,
    drag_metric_from: Option<usize>,
    drag_metric_start_px: POINT,
    drag_metric_active: bool,
    drag_metric_hover: Option<usize>,
    dpi: u32,
    width_px: i32,
    height_px: i32,
    theme: TaskbarTheme,
    has_dwm_border: bool,
    hand_cursor: HCURSOR,
    arrow_cursor: HCURSOR,
    d2d_factory: ID2D1Factory,
    dash_stroke: ID2D1StrokeStyle,
    render_target: Option<ID2D1DCRenderTarget>,
    base_rt: Option<ID2D1RenderTarget>,
    brush: Option<ID2D1SolidColorBrush>,
    utf16_buf: Vec<u16>,
    target_dpi: u32,
    surface: WindowSurface,
    fonts: PopupFonts,
    update_status: Arc<Mutex<UpdateStatus>>,
    self_update_writable: bool,
    last_save_error: Option<String>,
}

enum PostBorrowAction {
    OpenUrl(String),
    OpenConfig(Arc<MonitorSnapshot>),
}

impl PopupManager {
    pub(crate) fn new(
        hinstance: HINSTANCE,
        host_hwnd: HWND,
        state: SnapshotStore,
        popup_open: Arc<AtomicBool>,
        hw_signal: HwWakeSignal,
    ) -> windows::core::Result<Rc<RefCell<Self>>> {
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                POPUP_CLASS_NAME,
                w!("AirMonitorFlyout"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                HWND::default(),
                None,
                hinstance,
                None,
            )?;

            let corner = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const _ as *const _,
                size_of::<windows::Win32::Graphics::Dwm::DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            );

            let d2d_factory = shared_d2d_factory()?;
            let dash_props = D2D1_STROKE_STYLE_PROPERTIES {
                dashStyle: D2D1_DASH_STYLE_DASH,
                ..Default::default()
            };
            let dash_stroke = d2d_factory.CreateStrokeStyle(&dash_props, None)?;
            let dwrite_factory = shared_dwrite_factory()?;
            let fonts = PopupFonts::new(&dwrite_factory)?;
            let hand_cursor = LoadCursorW(HINSTANCE::default(), IDC_HAND).unwrap_or_default();
            let arrow_cursor = LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default();

            let theme = read_taskbar_theme();
            let has_dwm_border = apply_dwm_border(hwnd, theme);

            let popup = Rc::new(RefCell::new(Self {
                hwnd,
                host_hwnd,
                state,
                popup_open,
                hw_signal,
                cached_snapshot: None,
                last_rendered_rev: None,
                cached_autostart: false,
                visible: false,
                shown_at: None,
                last_hidden_at: None,
                anchor_click_x: 0,
                anchor_click_y: 0,
                active_tab: PopupTab::Overview,
                settings_sub_tab: SettingsSubTab::Metrics,
                selected_metric_idx: Some(2),
                scroll_y: 0.0,
                max_scroll_y: 0.0,
                hovered_idx: None,
                pressed_idx: None,
                tracking_leave: false,
                hit_boxes: Vec::new(),
                metric_slot_rects: Vec::new(),
                drag_metric_from: None,
                drag_metric_start_px: POINT::default(),
                drag_metric_active: false,
                drag_metric_hover: None,
                dpi: 96,
                width_px: 1,
                height_px: 1,
                theme,
                has_dwm_border,
                hand_cursor,
                arrow_cursor,
                d2d_factory,
                dash_stroke,
                render_target: None,
                base_rt: None,
                brush: None,
                utf16_buf: Vec::with_capacity(128),
                target_dpi: 0,
                surface: WindowSurface::new(),
                fonts,
                update_status: Arc::new(Mutex::new(UpdateStatus::Idle)),
                self_update_writable: false,
                last_save_error: None,
            }));

            let ptr: *const RefCell<PopupManager> = Rc::as_ptr(&popup);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            Ok(popup)
        }
    }

    pub(crate) fn take_save_error(cell: &RefCell<Self>) -> Option<String> {
        cell.try_borrow_mut()
            .ok()
            .and_then(|mut p| p.last_save_error.take())
    }

    pub(crate) fn refresh_theme(cell: &RefCell<Self>, vs: Option<&VersionedSnapshot>) {
        let invalidate_hwnd = {
            let Ok(mut p) = cell.try_borrow_mut() else {
                return;
            };
            p.theme = read_taskbar_theme();
            p.has_dwm_border = apply_dwm_border(p.hwnd, p.theme);
            if p.visible {
                if let Some(vs) = vs {
                    p.cached_snapshot = Some(Arc::clone(&vs.snapshot));
                    p.last_rendered_rev = Some((vs.hw_rev, vs.wireless_rev, vs.config_rev));
                }
                if p.rerender_cached() {
                    Some(p.hwnd)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(hwnd) = invalidate_hwnd {
            unsafe {
                let _ = InvalidateRect(hwnd, None, false);
            }
        }
    }

    /// Fallback for `SetForegroundWindow` failure: if the popup is visible past the
    /// initial focus-transition grace window and another window holds foreground, hide it.
    pub(crate) fn hide_if_lost_focus(cell: &RefCell<Self>) {
        let hide_hwnd = {
            let Ok(mut p) = cell.try_borrow_mut() else {
                return;
            };
            if !p.visible {
                return;
            }
            if p.shown_at
                .is_some_and(|t| t.elapsed() < FOCUS_FALLBACK_GRACE)
            {
                return;
            }
            let fg = unsafe { GetForegroundWindow() };
            if !fg.0.is_null() && fg != p.hwnd && fg != p.host_hwnd {
                p.prepare_hide()
            } else {
                None
            }
        };
        if let Some(h) = hide_hwnd {
            finish_hide(h);
        }
    }

    /// Toggles visibility while dropping the `RefCell<PopupManager>` borrow before
    /// invoking `ShowWindow` / `SetWindowPos` / `SetForegroundWindow` (which can
    /// synchronously re-enter `popup_wnd_proc`).
    pub(crate) fn toggle_at(cell: &RefCell<Self>, click_x: i32, click_y: i32, from_tray: bool) {
        enum Op {
            None,
            Hide(HWND),
            Show(HWND, RECT),
        }

        let op = {
            let Ok(mut p) = cell.try_borrow_mut() else {
                return;
            };
            if p.visible {
                match p.prepare_hide() {
                    Some(hwnd) => Op::Hide(hwnd),
                    None => Op::None,
                }
            } else {
                // Tray clicks momentarily deactivate the popup via Explorer before the callback arrives.
                if from_tray {
                    if let Some(t) = p.last_hidden_at {
                        if t.elapsed() < Duration::from_millis(280) {
                            p.last_hidden_at = None;
                            return;
                        }
                    }
                }
                match p.prepare_show_at(click_x, click_y) {
                    Some((hwnd, rect)) => Op::Show(hwnd, rect),
                    None => Op::None,
                }
            }
        };

        match op {
            Op::None => {}
            Op::Hide(hwnd) => finish_hide(hwnd),
            Op::Show(hwnd, rect) => unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    rect.left,
                    rect.top,
                    (rect.right - rect.left).max(1),
                    (rect.bottom - rect.top).max(1),
                    SWP_SHOWWINDOW | SWP_NOACTIVATE,
                );
                let _ = SetForegroundWindow(hwnd);
                let _ = InvalidateRect(hwnd, None, false);
            },
        }
    }

    pub(crate) fn show_tab_at(cell: &RefCell<Self>, click_x: i32, click_y: i32, tab: PopupTab) {
        let op = {
            let Ok(mut p) = cell.try_borrow_mut() else {
                return;
            };
            p.last_hidden_at = None;
            if p.visible {
                p.anchor_click_x = click_x;
                p.anchor_click_y = click_y;
                if p.active_tab != tab {
                    p.active_tab = tab;
                    p.scroll_y = 0.0;
                    p.hovered_idx = None;
                    p.pressed_idx = None;
                    p.clear_metric_drag();
                }
                if tab == PopupTab::Settings {
                    p.cached_autostart = is_autostart_enabled();
                } else if tab == PopupTab::About {
                    p.self_update_writable = is_self_update_dir_writable();
                }
                p.sync_popup_open(true);
                let (rendered, resize_rect) = p.refresh_from_state_now();
                let rect = resize_rect.or_else(|| {
                    p.cached_snapshot
                        .clone()
                        .map(|snap| p.compute_rect_for_snapshot(&snap))
                });
                rect.map(|r| (p.hwnd, r, rendered))
            } else {
                p.active_tab = tab;
                p.prepare_show_at(click_x, click_y)
                    .map(|(hwnd, rect)| (hwnd, rect, true))
            }
        };

        if let Some((hwnd, rect, _)) = op {
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    rect.left,
                    rect.top,
                    (rect.right - rect.left).max(1),
                    (rect.bottom - rect.top).max(1),
                    SWP_SHOWWINDOW | SWP_NOACTIVATE,
                );
                let _ = SetForegroundWindow(hwnd);
                let _ = InvalidateRect(hwnd, None, false);
            }
        }
    }

    pub(crate) fn refresh_update_status(cell: &RefCell<Self>) {
        let pending = {
            let Ok(mut p) = cell.try_borrow_mut() else {
                return;
            };
            if !p.visible || p.active_tab != PopupTab::About {
                return;
            }
            if matches!(
                p.snapshot_update_status(),
                UpdateStatus::UpdateAvailable { .. }
            ) {
                p.self_update_writable = is_self_update_dir_writable();
            }
            let snap = match p.cached_snapshot.clone() {
                Some(s) => s,
                None => {
                    let Some(vs) = p.state.read_versioned() else {
                        return;
                    };
                    let s = Arc::clone(&vs.snapshot);
                    p.cached_snapshot = Some(Arc::clone(&s));
                    p.last_rendered_rev = Some((vs.hw_rev, vs.wireless_rev, vs.config_rev));
                    s
                }
            };
            let resize_rect = p.recalculate_rect_if_needed(&snap);
            p.render_current(&snap);
            Some((p.hwnd, resize_rect))
        };
        if let Some((hwnd, resize_rect)) = pending {
            apply_resize_and_invalidate(hwnd, resize_rect, true);
        }
    }

    pub(crate) fn refresh_if_visible(cell: &RefCell<Self>, vs: &VersionedSnapshot) {
        let pending = {
            let Ok(mut p) = cell.try_borrow_mut() else {
                return;
            };
            if !p.visible {
                return;
            }
            let rev = (vs.hw_rev, vs.wireless_rev, vs.config_rev);
            if p.last_rendered_rev == Some(rev) {
                return;
            }
            let needs_live_hw = matches!(p.active_tab, PopupTab::Overview)
                || (matches!(p.active_tab, PopupTab::Settings)
                    && matches!(p.settings_sub_tab, SettingsSubTab::Metrics));
            let needs_live_wireless = matches!(p.active_tab, PopupTab::Overview);
            if let Some((prev_hw, prev_wl, prev_cfg)) = p.last_rendered_rev {
                let config_changed = prev_cfg != vs.config_rev;
                let hw_changed = needs_live_hw && prev_hw != vs.hw_rev;
                let wl_changed = needs_live_wireless && prev_wl != vs.wireless_rev;
                if !config_changed && !hw_changed && !wl_changed {
                    p.cached_snapshot = Some(Arc::clone(&vs.snapshot));
                    p.last_rendered_rev = Some(rev);
                    return;
                }
            }
            if p.cached_snapshot
                .as_ref()
                .is_none_or(|prev| prev.config.autostart != vs.snapshot.config.autostart)
            {
                p.cached_autostart = vs.snapshot.config.autostart;
            }
            let snap = Arc::clone(&vs.snapshot);
            p.cached_snapshot = Some(Arc::clone(&snap));
            let resize_rect = p.recalculate_rect_if_needed(&snap);
            p.render_current(&snap);
            p.last_rendered_rev = Some(rev);
            Some((p.hwnd, resize_rect))
        };
        if let Some((hwnd, resize_rect)) = pending {
            apply_resize_and_invalidate(hwnd, resize_rect, true);
        }
    }

    pub(crate) fn destroy(cell: &RefCell<Self>) {
        let hwnd = {
            let Ok(mut p) = cell.try_borrow_mut() else {
                return;
            };
            p.prepare_hide();
            p.popup_open.store(false, Ordering::Release);
            let h = p.hwnd;
            p.hwnd = HWND::default();
            h
        };
        unsafe {
            if !hwnd.0.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(hwnd);
            }
        }
    }

    fn snapshot_update_status(&self) -> UpdateStatus {
        self.update_status
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    fn sync_popup_open(&self, wake_if_open: bool) {
        let needs_hw_detail = self.visible && self.active_tab != PopupTab::About;
        self.popup_open.store(needs_hw_detail, Ordering::Release);
        if needs_hw_detail && wake_if_open {
            self.hw_signal.wake();
        }
    }

    fn compute_rect_for_snapshot(&mut self, _snap: &MonitorSnapshot) -> RECT {
        let geom = find_taskbar_geometry();
        let (monitor_rect, work_area, tb_rect, dpi) = match &geom {
            Some(g) => {
                let target_pt = POINT {
                    x: self.anchor_click_x,
                    y: self.anchor_click_y,
                };
                let (mon_rect, wa, mon_dpi) =
                    if self.anchor_click_x != 0 || self.anchor_click_y != 0 {
                        monitor_info_for_point(target_pt, g.dpi)
                    } else {
                        monitor_info_for_hwnd(g.taskbar_hwnd, g.dpi)
                    };
                (mon_rect, wa, Some(g.taskbar_rect), mon_dpi)
            }
            None => {
                let raw_dpi = unsafe { GetDpiForWindow(self.hwnd) };
                let fallback_dpi = if raw_dpi == 0 { 96 } else { raw_dpi };
                let (mon_rect, wa, mon_dpi) = monitor_info_for_point(
                    POINT {
                        x: self.anchor_click_x,
                        y: self.anchor_click_y,
                    },
                    fallback_dpi,
                );
                (mon_rect, wa, None, mon_dpi)
            }
        };

        self.dpi = dpi.max(96);
        let scale = self.dpi as f32 / 96.0;
        let margin_px = (8.0 * scale).round() as i32;
        let max_avail_h_px = (work_area.bottom - work_area.top - margin_px * 2).max(200);
        let desired_h_logical = compute_popup_height_dip(monitor_rect, self.dpi);
        let w_px = (POPUP_WIDTH_LOGICAL * scale).round() as i32;
        let h_px = ((desired_h_logical * scale).ceil() as i32).min(max_avail_h_px);

        compute_popup_rect(
            work_area,
            tb_rect,
            self.anchor_click_x,
            w_px,
            h_px,
            margin_px,
        )
    }

    fn recalculate_rect_if_needed(&mut self, snap: &MonitorSnapshot) -> Option<RECT> {
        let rect = self.compute_rect_for_snapshot(snap);
        let next_w = (rect.right - rect.left).max(1);
        let next_h = (rect.bottom - rect.top).max(1);
        if next_w != self.width_px || next_h != self.height_px {
            self.width_px = next_w;
            self.height_px = next_h;
            Some(rect)
        } else {
            None
        }
    }

    fn clear_metric_drag(&mut self) -> bool {
        let had = self.drag_metric_from.is_some()
            || self.drag_metric_active
            || self.drag_metric_hover.is_some();
        self.drag_metric_from = None;
        self.drag_metric_start_px = POINT::default();
        self.drag_metric_active = false;
        self.drag_metric_hover = None;
        had
    }

    fn clamp_selected_metric_for_snapshot(&mut self, snap: &MonitorSnapshot) {
        let len = snap.config.hardware_items.len();
        self.selected_metric_idx = if len == 0 {
            None
        } else {
            Some(self.selected_metric_idx.unwrap_or(0).min(len - 1))
        };
    }

    fn prepare_show_at(&mut self, click_x: i32, click_y: i32) -> Option<(HWND, RECT)> {
        self.theme = read_taskbar_theme();
        self.has_dwm_border = apply_dwm_border(self.hwnd, self.theme);
        self.cached_autostart = is_autostart_enabled();
        if self.active_tab == PopupTab::About {
            self.self_update_writable = is_self_update_dir_writable();
        }
        self.anchor_click_x = click_x;
        self.anchor_click_y = click_y;

        let vs = self.state.read_versioned()?;
        let snap = Arc::clone(&vs.snapshot);
        let rect = self.compute_rect_for_snapshot(&snap);

        self.width_px = (rect.right - rect.left).max(1);
        self.height_px = (rect.bottom - rect.top).max(1);
        self.scroll_y = 0.0;
        self.hovered_idx = None;
        self.pressed_idx = None;
        self.clear_metric_drag();
        self.clamp_selected_metric_for_snapshot(&snap);
        self.visible = true;
        self.shown_at = Some(Instant::now());
        self.cached_snapshot = Some(Arc::clone(&snap));

        self.render_current(&snap);
        self.last_rendered_rev = Some((vs.hw_rev, vs.wireless_rev, vs.config_rev));
        self.sync_popup_open(true);
        Some((self.hwnd, rect))
    }

    fn prepare_hide(&mut self) -> Option<HWND> {
        self.popup_open.store(false, Ordering::Release);
        if !self.visible {
            return None;
        }
        self.visible = false;
        self.shown_at = None;
        self.last_hidden_at = Some(Instant::now());
        self.hovered_idx = None;
        self.pressed_idx = None;
        self.clear_metric_drag();
        self.tracking_leave = false;
        self.hit_boxes.clear();
        self.metric_slot_rects.clear();
        self.cached_snapshot = None;
        self.last_rendered_rev = None;
        // Release temporary DIB/render target so closed popup consumes minimal idle RAM.
        self.surface.clear();
        self.render_target = None;
        self.base_rt = None;
        self.brush = None;
        self.target_dpi = 0;
        Some(self.hwnd)
    }

    fn rerender_cached(&mut self) -> bool {
        if !self.visible {
            return false;
        }
        let Some(snap) = self.cached_snapshot.clone() else {
            return false;
        };
        self.render_current(&snap);
        true
    }

    fn refresh_from_state_now(&mut self) -> (bool, Option<RECT>) {
        if !self.visible {
            return (false, None);
        }
        let Some(vs) = self.state.read_versioned() else {
            return (false, None);
        };
        let snap = Arc::clone(&vs.snapshot);
        self.cached_snapshot = Some(Arc::clone(&snap));
        let resize_rect = self.recalculate_rect_if_needed(&snap);
        self.render_current(&snap);
        self.last_rendered_rev = Some((vs.hw_rev, vs.wireless_rev, vs.config_rev));
        (true, resize_rect)
    }

    fn ensure_render_target(
        &mut self,
        dpi: u32,
    ) -> windows::core::Result<(ID2D1DCRenderTarget, ID2D1RenderTarget, ID2D1SolidColorBrush)> {
        let dpi = dpi.max(96);
        if self.render_target.is_none()
            || self.base_rt.is_none()
            || self.brush.is_none()
            || self.target_dpi != dpi
        {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: dpi as f32,
                dpiY: dpi as f32,
                ..Default::default()
            };
            unsafe {
                let rt = self.d2d_factory.CreateDCRenderTarget(&props)?;
                rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                let base_rt: ID2D1RenderTarget = rt.cast()?;
                let brush = base_rt.CreateSolidColorBrush(&rgba(255, 255, 255, 1.0), None)?;
                self.render_target = Some(rt);
                self.base_rt = Some(base_rt);
                self.brush = Some(brush);
                self.target_dpi = dpi;
            }
        }
        Ok((
            self.render_target.as_ref().unwrap().clone(),
            self.base_rt.as_ref().unwrap().clone(),
            self.brush.as_ref().unwrap().clone(),
        ))
    }

    fn render_current(&mut self, snapshot: &MonitorSnapshot) {
        if self.width_px <= 0 || self.height_px <= 0 {
            return;
        }
        let Ok(hdc) = self.surface.ensure_size(self.width_px, self.height_px) else {
            return;
        };
        let scale = self.dpi.max(96) as f32 / 96.0;
        let logical_w = self.width_px as f32 / scale;
        let logical_h = self.height_px as f32 / scale;
        let update_status = self.snapshot_update_status();

        self.clamp_selected_metric_for_snapshot(snapshot);
        self.hit_boxes.clear();
        self.metric_slot_rects.clear();

        for attempt in 0..2 {
            let Ok((rt, base_rt, brush)) = self.ensure_render_target(self.dpi) else {
                return;
            };
            let bind_rect = RECT {
                left: 0,
                top: 0,
                right: self.width_px,
                bottom: self.height_px,
            };
            unsafe {
                if rt.BindDC(hdc, &bind_rect).is_err() {
                    return;
                }
                rt.BeginDraw();
                let mut scene = SceneInput {
                    rt: &base_rt,
                    brush: &brush,
                    d2d_factory: &self.d2d_factory,
                    dash_stroke: &self.dash_stroke,
                    fonts: &self.fonts,
                    snapshot,
                    update_status: &update_status,
                    self_update_writable: self.self_update_writable,
                    theme: self.theme,
                    active_tab: self.active_tab,
                    settings_sub_tab: self.settings_sub_tab,
                    selected_metric_idx: self.selected_metric_idx,
                    drag_metric_from: self.drag_metric_from,
                    drag_metric_active: self.drag_metric_active,
                    drag_metric_hover: self.drag_metric_hover,
                    cached_autostart: self.cached_autostart,
                    has_dwm_border: self.has_dwm_border,
                    logical_w,
                    logical_h,
                    hovered_idx: self.hovered_idx,
                    pressed_idx: self.pressed_idx,
                    scroll_y: &mut self.scroll_y,
                    max_scroll_y: &mut self.max_scroll_y,
                    hit_boxes: &mut self.hit_boxes,
                    metric_slot_rects: &mut self.metric_slot_rects,
                    utf16_buf: &mut self.utf16_buf,
                };
                draw_popup_scene(&mut scene);
                match rt.EndDraw(None, None) {
                    Ok(()) => break,
                    Err(_) if attempt == 0 => {
                        self.render_target = None;
                        self.base_rt = None;
                        self.brush = None;
                        self.hit_boxes.clear();
                        self.metric_slot_rects.clear();
                    }
                    Err(_) => break,
                }
            }
        }
    }

    fn execute_action(
        &mut self,
        action: crate::ui::popup::layout::PopupAction,
    ) -> (bool, Option<RECT>, Option<PostBorrowAction>) {
        match apply_popup_action(action, &self.state) {
            ActionEffect::None => (false, None, None),
            ActionEffect::SwitchTab(tab) => {
                if self.active_tab != tab {
                    self.active_tab = tab;
                    if tab == PopupTab::Settings {
                        self.cached_autostart = is_autostart_enabled();
                    } else if tab == PopupTab::About {
                        self.self_update_writable = is_self_update_dir_writable();
                    }
                    self.sync_popup_open(true);
                    self.scroll_y = 0.0;
                    self.hovered_idx = None;
                    self.pressed_idx = None;
                    self.clear_metric_drag();
                    let resize_rect = self
                        .cached_snapshot
                        .clone()
                        .and_then(|snap| self.recalculate_rect_if_needed(&snap));
                    return (self.rerender_cached(), resize_rect, None);
                }
                (false, None, None)
            }
            ActionEffect::SwitchSettingsSubTab(sub_tab) => {
                if self.settings_sub_tab != sub_tab {
                    self.settings_sub_tab = sub_tab;
                    self.scroll_y = 0.0;
                    self.hovered_idx = None;
                    self.pressed_idx = None;
                    self.clear_metric_drag();
                    let resize_rect = self
                        .cached_snapshot
                        .clone()
                        .and_then(|snap| self.recalculate_rect_if_needed(&snap));
                    return (self.rerender_cached(), resize_rect, None);
                }
                (false, None, None)
            }
            ActionEffect::SelectMetricSlot(idx) => {
                self.selected_metric_idx = Some(idx);
                if let Some(snap) = self.cached_snapshot.clone() {
                    self.clamp_selected_metric_for_snapshot(&snap);
                }
                (self.rerender_cached(), None, None)
            }
            ActionEffect::RequestWirelessRefresh => {
                post_host_message(self.host_hwnd.0 as isize, WM_APP_REFRESH_WIRELESS);
                (false, None, None)
            }
            ActionEffect::CheckForUpdates => {
                let started = {
                    let mut guard = self.update_status.lock().unwrap_or_else(|e| e.into_inner());
                    try_begin_update_check(&mut guard)
                };
                if !started {
                    return (false, None, None);
                }

                let status_arc = Arc::clone(&self.update_status);
                let host_raw = self.host_hwnd.0 as isize;
                if let Err(err) = std::thread::Builder::new()
                    .name("update-checker".to_string())
                    .spawn(move || {
                        let result = crate::updater::check_latest_release();
                        if let Ok(mut guard) = status_arc.lock() {
                            *guard = result;
                        }
                        post_host_message(host_raw, WM_APP_UPDATE_STATUS);
                    })
                {
                    if let Ok(mut guard) = self.update_status.lock() {
                        *guard = UpdateStatus::Error(format!("启动更新线程失败: {err}"));
                    }
                }

                let resize_rect = self
                    .cached_snapshot
                    .clone()
                    .and_then(|snap| self.recalculate_rect_if_needed(&snap));
                (self.rerender_cached(), resize_rect, None)
            }
            ActionEffect::StartDownloadUpdate => {
                let target = {
                    let mut guard = self.update_status.lock().unwrap_or_else(|e| e.into_inner());
                    try_begin_download_update(&mut guard)
                };
                let Some(target) = target else {
                    return (false, None, None);
                };

                let status_arc = Arc::clone(&self.update_status);
                let host_raw = self.host_hwnd.0 as isize;
                if let Err(err) = std::thread::Builder::new()
                    .name("update-downloader".to_string())
                    .spawn(move || {
                        let progress_arc = Arc::clone(&status_arc);
                        let res = crate::updater::download_and_apply_update(
                            &target.latest,
                            &target.download_url,
                            target.asset_name.as_deref(),
                            target.sha256_url.as_deref(),
                            move |next_status| {
                                if let Ok(mut guard) = progress_arc.lock() {
                                    *guard = next_status;
                                }
                                post_host_message(host_raw, WM_APP_UPDATE_STATUS);
                            },
                        );
                        match res {
                            Ok(()) => {
                                post_host_message(host_raw, WM_APP_UPDATE_RESTART);
                            }
                            Err(msg) => {
                                if let Ok(mut guard) = status_arc.lock() {
                                    *guard = UpdateStatus::Error(msg);
                                }
                                post_host_message(host_raw, WM_APP_UPDATE_STATUS);
                            }
                        }
                    })
                {
                    if let Ok(mut guard) = self.update_status.lock() {
                        *guard = UpdateStatus::Error(format!("启动下载线程失败: {err}"));
                    }
                }

                let resize_rect = self
                    .cached_snapshot
                    .clone()
                    .and_then(|snap| self.recalculate_rect_if_needed(&snap));
                (self.rerender_cached(), resize_rect, None)
            }
            ActionEffect::OpenUrl(url) => (false, None, Some(PostBorrowAction::OpenUrl(url))),
            ActionEffect::OpenConfig => {
                let snap = self
                    .cached_snapshot
                    .clone()
                    .or_else(|| self.state.read_snapshot());
                (false, None, snap.map(PostBorrowAction::OpenConfig))
            }
            ActionEffect::ConfigMutated {
                save_error,
                autostart_state,
                select_metric_idx,
            } => {
                if let Some(err) = save_error {
                    self.last_save_error = Some(err);
                }
                if let Some(auto_on) = autostart_state {
                    self.cached_autostart = auto_on;
                }
                if let Some(idx) = select_metric_idx {
                    self.selected_metric_idx = Some(idx);
                }
                let (rendered, resize_rect) = self.refresh_from_state_now();
                unsafe {
                    let _ =
                        PostMessageW(self.host_hwnd, WM_APP_CONFIG_CHANGED, WPARAM(0), LPARAM(0));
                }
                (rendered, resize_rect, None)
            }
            ActionEffect::ErrorOnly(err) => {
                self.last_save_error = Some(err);
                unsafe {
                    let _ =
                        PostMessageW(self.host_hwnd, WM_APP_CONFIG_CHANGED, WPARAM(0), LPARAM(0));
                }
                (self.rerender_cached(), None, None)
            }
        }
    }
}

fn post_host_message(host_raw: isize, msg: u32) {
    unsafe {
        let hwnd = HWND(host_raw as *mut std::ffi::c_void);
        if !hwnd.0.is_null() {
            let _ = PostMessageW(hwnd, msg, WPARAM(0), LPARAM(0));
        }
    }
}

fn apply_resize_and_invalidate(hwnd: HWND, resize_rect: Option<RECT>, need_invalidate: bool) {
    unsafe {
        if let Some(rect) = resize_rect {
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                rect.left,
                rect.top,
                (rect.right - rect.left).max(1),
                (rect.bottom - rect.top).max(1),
                SWP_NOACTIVATE,
            );
        }
        if need_invalidate || resize_rect.is_some() {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

fn finish_hide(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    crate::ui::trim_working_set();
}

unsafe fn get_popup_cell<'a>(hwnd: HWND) -> Option<&'a RefCell<PopupManager>> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<PopupManager>;
    if ptr.is_null() {
        None
    } else {
        Some(&*ptr)
    }
}

unsafe extern "system" fn popup_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }
    let Some(cell) = get_popup_cell(hwnd) else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };

    if msg == WM_SETCURSOR {
        if (lparam.0 & 0xFFFF) as u32 == HTCLIENT {
            if let Ok(popup) = cell.try_borrow() {
                let cursor = if (popup.hovered_idx.is_some() || popup.drag_metric_active)
                    && !popup.hand_cursor.0.is_null()
                {
                    popup.hand_cursor
                } else {
                    popup.arrow_cursor
                };
                if !cursor.0.is_null() {
                    SetCursor(cursor);
                    return LRESULT(1);
                }
            }
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    let mut popup = match cell.try_borrow_mut() {
        Ok(guard) => guard,
        Err(_) => {
            // Synchronous re-entry while borrowed: defer via PostMessageW to prevent aliasing UB.
            if msg == WM_ACTIVATE && (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                let _ = PostMessageW(hwnd, WM_APP_POPUP_DEFERRED_HIDE, WPARAM(0), LPARAM(0));
                return LRESULT(0);
            }
            if msg == WM_PAINT || msg == WM_CAPTURECHANGED {
                let _ = PostMessageW(hwnd, WM_APP_POPUP_DEFERRED_REFRESH, WPARAM(0), LPARAM(0));
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
    };

    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let p_ref = &mut *popup;
            if p_ref.visible {
                if let Ok(src_dc) = p_ref.surface.ensure_size(p_ref.width_px, p_ref.height_px) {
                    let _ = BitBlt(
                        hdc,
                        0,
                        0,
                        p_ref.width_px,
                        p_ref.height_px,
                        src_dc,
                        0,
                        0,
                        SRCCOPY,
                    );
                }
            }
            let _ = EndPaint(hwnd, &ps);
            return LRESULT(0);
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                let hide_hwnd = popup.prepare_hide();
                drop(popup);
                if let Some(h) = hide_hwnd {
                    finish_hide(h);
                }
            }
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            if wparam.0 as u16 == VK_ESCAPE.0 {
                let hide_hwnd = popup.prepare_hide();
                drop(popup);
                if let Some(h) = hide_hwnd {
                    finish_hide(h);
                }
                return LRESULT(0);
            }
        }
        WM_MOUSEMOVE => {
            let need_tme = if !popup.tracking_leave {
                popup.tracking_leave = true;
                true
            } else {
                false
            };
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let next_hover = hit_test_boxes(&popup.hit_boxes, popup.dpi, x, y);

            let need_invalidate = if let Some(from_idx) = popup.drag_metric_from {
                let dx = x - popup.drag_metric_start_px.x;
                let dy = y - popup.drag_metric_start_px.y;
                let mut just_activated = false;
                if !popup.drag_metric_active && (dx * dx + dy * dy >= 25) {
                    popup.drag_metric_active = true;
                    popup.pressed_idx = None;
                    just_activated = true;
                }
                if popup.drag_metric_active {
                    let next_drag_hover =
                        hit_test_metric_slot(&popup.metric_slot_rects, popup.dpi, x, y)
                            .filter(|&idx| idx != from_idx);
                    if just_activated
                        || next_drag_hover != popup.drag_metric_hover
                        || next_hover != popup.hovered_idx
                    {
                        popup.drag_metric_hover = next_drag_hover;
                        popup.hovered_idx = next_hover;
                        popup.rerender_cached()
                    } else {
                        false
                    }
                } else if next_hover != popup.hovered_idx {
                    popup.hovered_idx = next_hover;
                    popup.rerender_cached()
                } else {
                    false
                }
            } else if next_hover != popup.hovered_idx {
                popup.hovered_idx = next_hover;
                popup.rerender_cached()
            } else {
                false
            };

            drop(popup);
            if need_tme {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
            if need_invalidate {
                let _ = InvalidateRect(hwnd, None, false);
            }
            return LRESULT(0);
        }
        WM_MOUSELEAVE => {
            popup.tracking_leave = false;
            let need_invalidate = if popup.hovered_idx.is_some() {
                popup.hovered_idx = None;
                popup.rerender_cached()
            } else {
                false
            };
            drop(popup);
            if need_invalidate {
                let _ = InvalidateRect(hwnd, None, false);
            }
            return LRESULT(0);
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let hit = hit_test_boxes(&popup.hit_boxes, popup.dpi, x, y);
            let slot_hit = if popup.active_tab == PopupTab::Settings
                && popup.settings_sub_tab == SettingsSubTab::Metrics
            {
                hit_test_metric_slot(&popup.metric_slot_rects, popup.dpi, x, y)
            } else {
                None
            };
            if let Some(slot_idx) = slot_hit {
                popup.drag_metric_from = Some(slot_idx);
                popup.drag_metric_start_px = POINT { x, y };
                popup.drag_metric_active = false;
                popup.drag_metric_hover = None;
            } else {
                popup.clear_metric_drag();
            }

            let need_capture = hit.is_some() || slot_hit.is_some();
            let need_invalidate = if need_capture {
                popup.hovered_idx = hit;
                popup.pressed_idx = hit;
                popup.rerender_cached()
            } else {
                false
            };
            // Drop RefCell borrow BEFORE SetCapture so any synchronous message cannot alias.
            drop(popup);
            if need_capture {
                SetCapture(hwnd);
            }
            if need_invalidate {
                let _ = InvalidateRect(hwnd, None, false);
            }
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let drag_from = popup.drag_metric_from;
            let was_drag_active = popup.drag_metric_active;
            popup.clear_metric_drag();

            let was_pressed = popup.pressed_idx.take();
            let hit = hit_test_boxes(&popup.hit_boxes, popup.dpi, x, y);
            popup.hovered_idx = hit;

            let action = if was_drag_active {
                drag_from.map(|from_idx| {
                    let drop_target =
                        hit_test_metric_slot(&popup.metric_slot_rects, popup.dpi, x, y)
                            .filter(|&to_idx| to_idx != from_idx);
                    if let Some(to_idx) = drop_target {
                        PopupAction::SwapMetricSlots(from_idx, to_idx)
                    } else {
                        PopupAction::SelectMetricSlot(from_idx)
                    }
                })
            } else {
                match (was_pressed, hit) {
                    (Some(p_idx), Some(h_idx)) if p_idx == h_idx => {
                        popup.hit_boxes.get(h_idx).map(|hb| hb.action.clone())
                    }
                    // Fallback if LBUTTONUP arrived without prior LBUTTONDOWN capture (e.g. synthetic tests).
                    (None, Some(h_idx)) => popup.hit_boxes.get(h_idx).map(|hb| hb.action.clone()),
                    _ => None,
                }
            };

            let need_release = was_pressed.is_some() || drag_from.is_some();
            let (need_invalidate, resize_rect, post_action) = if let Some(act) = action {
                popup.execute_action(act)
            } else if was_pressed.is_some() || was_drag_active {
                (popup.rerender_cached(), None, None)
            } else {
                (false, None, None)
            };

            // Release RefCell borrow BEFORE ReleaseCapture() (sends WM_CAPTURECHANGED),
            // SetWindowPos() (sends WM_SIZE/WM_WINDOWPOSCHANGED), or ShellExecuteW().
            drop(popup);
            if need_release {
                let _ = ReleaseCapture();
            }
            apply_resize_and_invalidate(hwnd, resize_rect, need_invalidate);
            if let Some(post_act) = post_action {
                let err = match post_act {
                    PostBorrowAction::OpenUrl(url) => crate::updater::open_repo_url(&url).err(),
                    PostBorrowAction::OpenConfig(snap) => {
                        crate::ui::tray::open_config_in_editor(&snap).err()
                    }
                };
                if let Some(err_msg) = err {
                    if let Ok(mut p) = cell.try_borrow_mut() {
                        let host_hwnd = p.host_hwnd;
                        p.last_save_error = Some(err_msg);
                        drop(p);
                        let _ =
                            PostMessageW(host_hwnd, WM_APP_CONFIG_CHANGED, WPARAM(0), LPARAM(0));
                    }
                }
            }
            return LRESULT(0);
        }
        WM_CAPTURECHANGED => {
            let had_press = popup.pressed_idx.take().is_some();
            let had_drag = popup.clear_metric_drag();
            let need_invalidate = if had_press || had_drag {
                popup.rerender_cached()
            } else {
                false
            };
            drop(popup);
            if need_invalidate {
                let _ = InvalidateRect(hwnd, None, false);
            }
            return LRESULT(0);
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32;
            let step = -(delta / 120.0) * 30.0;
            let next = (popup.scroll_y + step).clamp(0.0, popup.max_scroll_y);
            let need_invalidate = if (next - popup.scroll_y).abs() > 0.1 {
                popup.scroll_y = next;
                let mut ok = popup.rerender_cached();
                // Re-evaluate hover hit-test against the newly scrolled hit_boxes so hover stays aligned.
                let mut pt = POINT::default();
                if GetCursorPos(&mut pt).is_ok() && ScreenToClient(hwnd, &mut pt).as_bool() {
                    let next_hover = hit_test_pt(&popup.hit_boxes, popup.dpi, pt);
                    if next_hover != popup.hovered_idx {
                        popup.hovered_idx = next_hover;
                        ok = popup.rerender_cached() || ok;
                    }
                }
                ok
            } else {
                false
            };
            drop(popup);
            if need_invalidate {
                let _ = InvalidateRect(hwnd, None, false);
            }
            return LRESULT(0);
        }
        WM_APP_POPUP_DEFERRED_HIDE => {
            let hide_hwnd = popup.prepare_hide();
            drop(popup);
            if let Some(h) = hide_hwnd {
                finish_hide(h);
            }
            return LRESULT(0);
        }
        WM_APP_POPUP_DEFERRED_REFRESH => {
            popup.pressed_idx = None;
            popup.clear_metric_drag();
            let need_invalidate = popup.rerender_cached();
            drop(popup);
            if need_invalidate {
                let _ = InvalidateRect(hwnd, None, false);
            }
            return LRESULT(0);
        }
        _ => {}
    }

    drop(popup);
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn apply_dwm_border(hwnd: HWND, theme: TaskbarTheme) -> bool {
    let colorref = PopupPalette::for_theme(theme).dwm_border_colorref;
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &colorref as *const _ as *const _,
            size_of::<u32>() as u32,
        )
        .is_ok()
    }
}

fn monitor_info_from_hmon(
    hmon: windows::Win32::Graphics::Gdi::HMONITOR,
    fallback_dpi: u32,
) -> (RECT, RECT, u32) {
    unsafe {
        let mut mi = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let (mon_rect, work_rect) = if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            (mi.rcMonitor, mi.rcWork)
        } else {
            (
                RECT {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                },
                RECT {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1032,
                },
            )
        };
        let mut dpi_x = 0u32;
        let mut dpi_y = 0u32;
        let dpi = if GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok()
            && dpi_x > 0
        {
            dpi_x
        } else {
            fallback_dpi.max(96)
        };
        (mon_rect, work_rect, dpi)
    }
}

fn monitor_info_for_hwnd(hwnd: HWND, fallback_dpi: u32) -> (RECT, RECT, u32) {
    unsafe {
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        monitor_info_from_hmon(hmon, fallback_dpi)
    }
}

fn monitor_info_for_point(pt: POINT, fallback_dpi: u32) -> (RECT, RECT, u32) {
    unsafe {
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        monitor_info_from_hmon(hmon, fallback_dpi)
    }
}
