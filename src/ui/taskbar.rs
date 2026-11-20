use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, ScreenToClient, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, GetClassNameW, GetForegroundWindow, GetParent, GetWindow,
    GetWindowLongW, GetWindowRect, IsIconic, IsWindow, IsWindowVisible, SetParent, SetWindowLongW,
    SetWindowPos, ShowWindow, GWL_EXSTYLE, GWL_STYLE, GW_HWNDPREV, HWND_TOP, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, WS_CHILD, WS_CLIPSIBLINGS,
    WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::config::{AnchorEdge, LayoutMode, ZoneLayout};
use crate::state::MonitorSnapshot;
use crate::ui::renderer::{
    compute_logical_width, read_taskbar_theme, RenderContent, TaskbarRenderer, TaskbarTheme,
    WindowSurface,
};
use crate::ui::VersionedSnapshot;

#[derive(Debug, Clone)]
pub struct TaskbarGeometry {
    pub taskbar_hwnd: HWND,
    pub taskbar_rect: RECT,
    pub tray_rect: Option<RECT>,
    pub dpi: u32,
    pub taskbar_hidden: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct DragState {
    pub hwnd: HWND,
    pub is_devices_zone: bool,
    pub start_cursor_x: i32,
    pub start_offset_x: i32,
    pub anchor: AnchorEdge,
    pub dpi: u32,
}

pub fn find_taskbar_geometry() -> Option<TaskbarGeometry> {
    unsafe {
        let taskbar_hwnd = FindWindowW(w!("Shell_TrayWnd"), None).ok()?;
        if taskbar_hwnd.0.is_null() {
            return None;
        }
        let mut taskbar_rect = RECT::default();
        if GetWindowRect(taskbar_hwnd, &mut taskbar_rect).is_err() {
            return None;
        }

        let taskbar_hidden = !IsWindowVisible(taskbar_hwnd).as_bool()
            || taskbar_rect.right <= taskbar_rect.left
            || taskbar_rect.bottom <= taskbar_rect.top;

        let tray_rect = FindWindowExW(taskbar_hwnd, HWND::default(), w!("TrayNotifyWnd"), None)
            .ok()
            .and_then(|tray_hwnd| {
                if tray_hwnd.0.is_null() || !IsWindowVisible(tray_hwnd).as_bool() {
                    return None;
                }
                let mut r = RECT::default();
                if GetWindowRect(tray_hwnd, &mut r).is_ok() && r.right > r.left && r.bottom > r.top
                {
                    Some(r)
                } else {
                    None
                }
            });

        let raw_dpi = GetDpiForWindow(taskbar_hwnd);
        let dpi = if raw_dpi == 0 { 96 } else { raw_dpi };

        Some(TaskbarGeometry {
            taskbar_hwnd,
            taskbar_rect,
            tray_rect,
            dpi,
            taskbar_hidden,
        })
    }
}

pub fn is_vertical_taskbar(rect: &RECT) -> bool {
    (rect.bottom - rect.top) > (rect.right - rect.left)
}

pub fn is_shell_or_nonfullscreen_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "Progman"
            | "WorkerW"
            | "Windows.UI.Core.CoreWindow"
            | "XamlExplorerHostIslandWindow"
            | "TopLevelWindowForOverflowXamlIsland"
            | "NotifyIconOverflowWindow"
            | "#32768"
            | "AirMonitor_PopupFlyout"
            | "AirMonitor_TaskbarOverlay"
            | "AirMonitor_HostWindow"
    )
}

pub fn is_fullscreen_active(geom: &TaskbarGeometry, own_hwnds: &[HWND]) -> bool {
    unsafe {
        if let Ok(state) = SHQueryUserNotificationState() {
            if state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE {
                return true;
            }
        }

        let fg = GetForegroundWindow();
        if fg.0.is_null()
            || !IsWindowVisible(fg).as_bool()
            || IsIconic(fg).as_bool()
            || own_hwnds.contains(&fg)
        {
            return false;
        }

        let mut cloaked: u32 = 0;
        if DwmGetWindowAttribute(
            fg,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return false;
        }

        let ex_style = GetWindowLongW(fg, GWL_EXSTYLE);
        if (ex_style & WS_EX_TRANSPARENT.0 as i32) != 0 {
            return false;
        }

        let mut cls = [0u16; 128];
        let len = GetClassNameW(fg, &mut cls);
        if len > 0 {
            let class_name = String::from_utf16_lossy(&cls[..len as usize]);
            if is_shell_or_nonfullscreen_class(&class_name) {
                return false;
            }
        }

        let tb_mon = MonitorFromWindow(geom.taskbar_hwnd, MONITOR_DEFAULTTONEAREST);
        let fg_mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        if tb_mon != fg_mon {
            return false;
        }

        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(tb_mon, &mut mi).as_bool() {
            return false;
        }

        let mut fg_rect = RECT::default();
        if GetWindowRect(fg, &mut fg_rect).is_err() {
            return false;
        }

        fg_rect.left <= mi.rcMonitor.left
            && fg_rect.top <= mi.rcMonitor.top
            && fg_rect.right >= mi.rcMonitor.right
            && fg_rect.bottom >= mi.rcMonitor.bottom
    }
}

pub fn compute_zone_rect(
    geom: &TaskbarGeometry,
    zone: &ZoneLayout,
    logical_w: f32,
) -> (i32, i32, i32, i32) {
    let scale = geom.dpi.max(96) as f32 / 96.0;
    let width_px = (logical_w * scale).ceil().max(1.0) as i32;
    let tb_h = (geom.taskbar_rect.bottom - geom.taskbar_rect.top).max(24);
    let y = geom.taskbar_rect.top;
    let offset_px = (zone.offset_x.max(0) as f32 * scale).round() as i32;

    let raw_x = match zone.anchor {
        AnchorEdge::Left => geom.taskbar_rect.left + offset_px,
        AnchorEdge::Right => {
            let right_anchor = geom
                .tray_rect
                .filter(|tr| tr.left > geom.taskbar_rect.left)
                .map(|tr| tr.left)
                .unwrap_or(geom.taskbar_rect.right);
            right_anchor - offset_px - width_px
        }
    };

    let min_x = geom.taskbar_rect.left;
    let max_x = (geom.taskbar_rect.right - width_px).max(min_x);
    let x = raw_x.clamp(min_x, max_x);
    (x, y, width_px, tb_h)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BarRenderKey {
    layout_mode: LayoutMode,
    theme: TaskbarTheme,
    dpi: u32,
    tb_rect: RECT,
    tray_rect: Option<RECT>,
    global_hide: bool,
    hovered: bool,
    config_rev: u64,
    data_rev: u64,
    extra_data_rev: u64,
}

impl BarRenderKey {
    fn for_combined(
        theme: TaskbarTheme,
        geom: &TaskbarGeometry,
        global_hide: bool,
        hovered: bool,
        vs: &VersionedSnapshot,
    ) -> Self {
        Self {
            layout_mode: LayoutMode::Combined,
            theme,
            dpi: geom.dpi,
            tb_rect: geom.taskbar_rect,
            tray_rect: geom.tray_rect,
            global_hide,
            hovered,
            config_rev: vs.config_rev,
            data_rev: vs.hw_rev,
            extra_data_rev: vs.wireless_rev,
        }
    }

    fn for_hardware_bar(
        layout_mode: LayoutMode,
        theme: TaskbarTheme,
        geom: &TaskbarGeometry,
        global_hide: bool,
        hovered: bool,
        vs: &VersionedSnapshot,
    ) -> Self {
        Self {
            layout_mode,
            theme,
            dpi: geom.dpi,
            tb_rect: geom.taskbar_rect,
            tray_rect: geom.tray_rect,
            global_hide,
            hovered,
            config_rev: vs.config_rev,
            data_rev: vs.hw_rev,
            extra_data_rev: 0,
        }
    }

    fn for_devices_bar(
        layout_mode: LayoutMode,
        theme: TaskbarTheme,
        geom: &TaskbarGeometry,
        global_hide: bool,
        hovered: bool,
        vs: &VersionedSnapshot,
    ) -> Self {
        Self {
            layout_mode,
            theme,
            dpi: geom.dpi,
            tb_rect: geom.taskbar_rect,
            tray_rect: geom.tray_rect,
            global_hide,
            hovered,
            config_rev: vs.config_rev,
            data_rev: vs.wireless_rev,
            extra_data_rev: 0,
        }
    }
}

const GEOM_SETTLE_NORMAL_MS: u64 = 180;
const GEOM_SETTLE_POST_FS_MS: u64 = 320;
const POST_FS_COOLDOWN_MS: u64 = 600;

fn same_layout_geom(a: &TaskbarGeometry, b: &TaskbarGeometry) -> bool {
    a.taskbar_hwnd == b.taskbar_hwnd
        && a.taskbar_rect == b.taskbar_rect
        && a.tray_rect == b.tray_rect
        && a.dpi == b.dpi
        && a.taskbar_hidden == b.taskbar_hidden
}

#[derive(Debug, Clone)]
struct PendingGeometry {
    geom: TaskbarGeometry,
    first_seen: Instant,
    matches: u32,
}

/// Stabilizes taskbar / TrayNotifyWnd geometry across transient Win11 XAML
/// layout frames and fullscreen-exit transitions:
/// - Freezes geometry while fullscreen is active so transient fullscreen-res
///   or shell changes cannot shift the bar.
/// - Requires consecutive matching samples across a settle window before
///   committing geometry changes, filtering out 1-frame `TrayNotifyWnd`
///   left-shift glitches on fullscreen exit.
#[derive(Debug, Default)]
pub(crate) struct GeometryStabilizer {
    committed: Option<TaskbarGeometry>,
    pending: Option<PendingGeometry>,
    was_fullscreen: bool,
    post_fullscreen_until: Option<Instant>,
}

impl GeometryStabilizer {
    pub fn reset(&mut self) {
        self.committed = None;
        self.pending = None;
        self.was_fullscreen = false;
        self.post_fullscreen_until = None;
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn resolve(
        &mut self,
        now: Instant,
        raw: TaskbarGeometry,
        fullscreen: bool,
    ) -> TaskbarGeometry {
        if fullscreen {
            self.was_fullscreen = true;
            self.pending = None;
            if let Some(committed) = &self.committed {
                if committed.taskbar_hwnd == raw.taskbar_hwnd {
                    return committed.clone();
                }
            }
            self.committed = Some(raw.clone());
            return raw;
        }

        if self.was_fullscreen {
            self.was_fullscreen = false;
            self.post_fullscreen_until = Some(now + Duration::from_millis(POST_FS_COOLDOWN_MS));
        }

        let needs_immediate = match &self.committed {
            None => true,
            Some(committed) => {
                committed.taskbar_hwnd != raw.taskbar_hwnd
                    || raw.taskbar_hidden
                    || is_vertical_taskbar(&raw.taskbar_rect)
                    || committed.taskbar_hidden
                    || is_vertical_taskbar(&committed.taskbar_rect)
            }
        };

        if needs_immediate {
            self.pending = None;
            self.committed = Some(raw.clone());
            return raw;
        }

        let committed = self.committed.as_ref().unwrap();
        if same_layout_geom(committed, &raw) {
            self.pending = None;
            return committed.clone();
        }

        let in_post_fs = self.post_fullscreen_until.is_some_and(|until| now < until);
        if !in_post_fs {
            self.post_fullscreen_until = None;
        }
        let settle_dur = if in_post_fs {
            Duration::from_millis(GEOM_SETTLE_POST_FS_MS)
        } else {
            Duration::from_millis(GEOM_SETTLE_NORMAL_MS)
        };

        match &mut self.pending {
            Some(pend) if same_layout_geom(&pend.geom, &raw) => {
                pend.matches = pend.matches.saturating_add(1);
                if pend.matches >= 2 && now.saturating_duration_since(pend.first_seen) >= settle_dur
                {
                    self.committed = Some(raw.clone());
                    self.pending = None;
                    return raw;
                }
            }
            _ => {
                self.pending = Some(PendingGeometry {
                    geom: raw,
                    first_seen: now,
                    matches: 1,
                });
            }
        }

        self.committed.clone().unwrap()
    }
}

pub struct TaskbarManager {
    pub primary_hwnd: HWND,
    pub secondary_hwnd: HWND,
    pub primary_surface: WindowSurface,
    pub secondary_surface: WindowSurface,
    pub primary_hovered: bool,
    pub secondary_hovered: bool,
    pub renderer: TaskbarRenderer,
    pub theme: TaskbarTheme,
    pub drag: Option<DragState>,
    geom_stabilizer: GeometryStabilizer,
    last_fullscreen: bool,
    last_global_hide: Option<bool>,
    last_tb_rect: Option<RECT>,
    last_tray_rect: Option<RECT>,
    last_dpi: u32,
    last_primary_key: Option<BarRenderKey>,
    last_secondary_key: Option<BarRenderKey>,
}

impl TaskbarManager {
    pub fn new(primary_hwnd: HWND, secondary_hwnd: HWND) -> windows::core::Result<Self> {
        Ok(Self {
            primary_hwnd,
            secondary_hwnd,
            primary_surface: WindowSurface::new(),
            secondary_surface: WindowSurface::new(),
            primary_hovered: false,
            secondary_hovered: false,
            renderer: TaskbarRenderer::new()?,
            theme: read_taskbar_theme(),
            drag: None,
            geom_stabilizer: GeometryStabilizer::default(),
            last_fullscreen: false,
            last_global_hide: None,
            last_tb_rect: None,
            last_tray_rect: None,
            last_dpi: 0,
            last_primary_key: None,
            last_secondary_key: None,
        })
    }

    pub(crate) fn has_pending_geometry(&self) -> bool {
        self.geom_stabilizer.has_pending()
    }

    pub fn refresh_theme(&mut self) {
        self.theme = read_taskbar_theme();
    }

    pub(crate) fn invalidate_primary(&mut self) {
        self.last_primary_key = None;
    }

    pub(crate) fn invalidate_secondary(&mut self) {
        self.last_secondary_key = None;
    }

    pub fn bars_alive(&self) -> bool {
        unsafe {
            !self.primary_hwnd.0.is_null()
                && IsWindow(self.primary_hwnd).as_bool()
                && !self.secondary_hwnd.0.is_null()
                && IsWindow(self.secondary_hwnd).as_bool()
        }
    }

    pub fn bar_diagnostic_summary(&self) -> String {
        unsafe {
            let tb = find_taskbar_geometry();
            let parent_match = match (&tb, GetParent(self.primary_hwnd)) {
                (Some(g), Ok(p)) => p == g.taskbar_hwnd,
                _ => false,
            };
            let vis =
                !self.primary_hwnd.0.is_null() && IsWindowVisible(self.primary_hwnd).as_bool();
            let mut bar_rect = RECT::default();
            let has_rect = !self.primary_hwnd.0.is_null()
                && GetWindowRect(self.primary_hwnd, &mut bar_rect).is_ok();
            let inside_tb = match (&tb, has_rect) {
                (Some(g), true) => {
                    bar_rect.left >= g.taskbar_rect.left
                        && bar_rect.right <= g.taskbar_rect.right
                        && bar_rect.top >= g.taskbar_rect.top
                        && bar_rect.bottom <= g.taskbar_rect.bottom
                }
                _ => false,
            };
            format!(
                "ChildAttached={} | BarVisible={} | InTaskbarRect={}",
                parent_match, vis, inside_tb
            )
        }
    }

    /// Lightweight periodic watchdog: stabilizes taskbar geometry against transient
    /// Win11 layout/fullscreen-exit frames, avoids redundant Direct2D redraws, and keeps
    /// child bars above Win11's XAML DesktopWindowContentBridge inside `Shell_TrayWnd`.
    pub(crate) fn check_tick(&mut self, vs: &VersionedSnapshot, host_hwnd: HWND) {
        let Some(raw_geom) = find_taskbar_geometry() else {
            self.geom_stabilizer.reset();
            self.last_fullscreen = false;
            if self.last_global_hide != Some(true) {
                self.hide_all_bars();
            }
            return;
        };

        let own_hwnds = [host_hwnd, self.primary_hwnd, self.secondary_hwnd];
        let fullscreen = is_fullscreen_active(&raw_geom, &own_hwnds);
        let geom = self
            .geom_stabilizer
            .resolve(Instant::now(), raw_geom, fullscreen);
        // Child bars inside Shell_TrayWnd are naturally occluded by fullscreen windows
        // in DWM/Win32 Z-order; hiding them with SW_HIDE during fullscreen would cause
        // a 0..500 ms pop-in flash on fullscreen exit.
        let global_hide = geom.taskbar_hidden || is_vertical_taskbar(&geom.taskbar_rect);

        let is_split = vs.snapshot.config.layout_mode == LayoutMode::Split;
        let parent_changed = unsafe {
            GetParent(self.primary_hwnd).ok() != Some(geom.taskbar_hwnd)
                || (is_split && GetParent(self.secondary_hwnd).ok() != Some(geom.taskbar_hwnd))
        };

        if parent_changed {
            self.last_primary_key = None;
            self.last_secondary_key = None;
        }

        let changed = parent_changed
            || self.last_fullscreen != fullscreen
            || self.last_global_hide != Some(global_hide)
            || self.last_tb_rect != Some(geom.taskbar_rect)
            || self.last_tray_rect != geom.tray_rect
            || self.last_dpi != geom.dpi
            || self.last_primary_key.is_none()
            || (is_split && self.last_secondary_key.is_none());

        if changed {
            self.apply_bars_with_geom_inner(vs, &geom, global_hide, fullscreen);
        } else if !global_hide && !fullscreen {
            unsafe {
                if IsWindowVisible(self.primary_hwnd).as_bool() {
                    ensure_child_top_order(self.primary_hwnd, self.secondary_hwnd);
                }
                if is_split && IsWindowVisible(self.secondary_hwnd).as_bool() {
                    ensure_child_top_order(self.secondary_hwnd, self.primary_hwnd);
                }
            }
        }
    }

    pub(crate) fn update_bars(&mut self, vs: &VersionedSnapshot, host_hwnd: HWND) {
        let Some(raw_geom) = find_taskbar_geometry() else {
            self.geom_stabilizer.reset();
            self.last_fullscreen = false;
            self.hide_all_bars();
            return;
        };

        let own_hwnds = [host_hwnd, self.primary_hwnd, self.secondary_hwnd];
        let fullscreen = is_fullscreen_active(&raw_geom, &own_hwnds);
        let geom = self
            .geom_stabilizer
            .resolve(Instant::now(), raw_geom, fullscreen);
        let global_hide = geom.taskbar_hidden || is_vertical_taskbar(&geom.taskbar_rect);
        self.apply_bars_with_geom_inner(vs, &geom, global_hide, fullscreen);
    }

    fn hide_all_bars(&mut self) {
        self.last_global_hide = Some(true);
        self.last_primary_key = None;
        self.last_secondary_key = None;
        hide_window(self.primary_hwnd);
        hide_window(self.secondary_hwnd);
        self.primary_surface.clear();
        self.secondary_surface.clear();
    }

    fn hide_secondary_bar(&mut self) {
        if self.last_secondary_key.is_some()
            || unsafe { IsWindowVisible(self.secondary_hwnd).as_bool() }
        {
            hide_window(self.secondary_hwnd);
            self.secondary_surface.clear();
        }
        self.last_secondary_key = None;
    }

    #[cfg(test)]
    fn apply_bars_with_geom(
        &mut self,
        vs: &VersionedSnapshot,
        geom: &TaskbarGeometry,
        global_hide: bool,
    ) {
        self.apply_bars_with_geom_inner(vs, geom, global_hide, false);
    }

    fn apply_bars_with_geom_inner(
        &mut self,
        vs: &VersionedSnapshot,
        geom: &TaskbarGeometry,
        global_hide: bool,
        fullscreen: bool,
    ) {
        let snapshot = &vs.snapshot;
        let layout_mode = snapshot.config.layout_mode;

        let primary_attached = ensure_attached_to_taskbar(self.primary_hwnd, geom.taskbar_hwnd);
        let secondary_attached = if layout_mode == LayoutMode::Split {
            ensure_attached_to_taskbar(self.secondary_hwnd, geom.taskbar_hwnd)
        } else {
            false
        };

        self.last_fullscreen = fullscreen;
        self.last_global_hide = Some(global_hide);
        self.last_tb_rect = Some(geom.taskbar_rect);
        self.last_tray_rect = geom.tray_rect;
        self.last_dpi = geom.dpi;

        if layout_mode != LayoutMode::Split {
            self.hide_secondary_bar();
        }

        if !primary_attached {
            hide_window(self.primary_hwnd);
            self.primary_surface.clear();
            self.last_primary_key = None;
        } else if global_hide || !fullscreen || self.last_primary_key.is_none() {
            let (key, zone, content) = match layout_mode {
                LayoutMode::Combined => (
                    BarRenderKey::for_combined(
                        self.theme,
                        geom,
                        global_hide,
                        self.primary_hovered,
                        vs,
                    ),
                    &snapshot.config.combined_zone,
                    RenderContent::Combined,
                ),
                LayoutMode::HardwareOnly | LayoutMode::Split => (
                    BarRenderKey::for_hardware_bar(
                        layout_mode,
                        self.theme,
                        geom,
                        global_hide,
                        self.primary_hovered,
                        vs,
                    ),
                    &snapshot.config.hardware_zone,
                    RenderContent::HardwareOnly,
                ),
                LayoutMode::DevicesOnly => (
                    BarRenderKey::for_devices_bar(
                        LayoutMode::DevicesOnly,
                        self.theme,
                        geom,
                        global_hide,
                        self.primary_hovered,
                        vs,
                    ),
                    &snapshot.config.devices_zone,
                    RenderContent::DevicesOnly,
                ),
            };
            if self.last_primary_key.as_ref() != Some(&key)
                && update_single_bar(
                    &mut self.renderer,
                    self.theme,
                    self.primary_hwnd,
                    self.secondary_hwnd,
                    &mut self.primary_surface,
                    self.primary_hovered,
                    snapshot,
                    zone,
                    content,
                    geom,
                    global_hide,
                )
            {
                self.last_primary_key = Some(key);
            }
        }

        if layout_mode == LayoutMode::Split {
            if !secondary_attached {
                hide_window(self.secondary_hwnd);
                self.secondary_surface.clear();
                self.last_secondary_key = None;
            } else if global_hide || !fullscreen || self.last_secondary_key.is_none() {
                let dev_key = BarRenderKey::for_devices_bar(
                    LayoutMode::Split,
                    self.theme,
                    geom,
                    global_hide,
                    self.secondary_hovered,
                    vs,
                );
                if self.last_secondary_key.as_ref() != Some(&dev_key)
                    && update_single_bar(
                        &mut self.renderer,
                        self.theme,
                        self.secondary_hwnd,
                        self.primary_hwnd,
                        &mut self.secondary_surface,
                        self.secondary_hovered,
                        snapshot,
                        &snapshot.config.devices_zone,
                        RenderContent::DevicesOnly,
                        geom,
                        global_hide,
                    )
                {
                    self.last_secondary_key = Some(dev_key);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn update_single_bar(
    renderer: &mut TaskbarRenderer,
    theme: TaskbarTheme,
    hwnd: HWND,
    sibling_bar: HWND,
    surface: &mut WindowSurface,
    hovered: bool,
    snapshot: &MonitorSnapshot,
    zone: &ZoneLayout,
    content: RenderContent,
    geom: &TaskbarGeometry,
    global_hide: bool,
) -> bool {
    let logical_w = compute_logical_width(snapshot, content);
    if global_hide || !zone.visible || logical_w <= 0.5 {
        hide_window(hwnd);
        surface.clear();
        return true;
    }

    let (screen_x, screen_y, w_px, h_px) = compute_zone_rect(geom, zone, logical_w);
    // Child windows of Shell_TrayWnd take client coordinates for UpdateLayeredWindow.
    let Some((client_x, client_y)) =
        screen_to_taskbar_client(geom.taskbar_hwnd, screen_x, screen_y)
    else {
        return false;
    };
    let was_visible = unsafe { IsWindowVisible(hwnd).as_bool() };
    if renderer
        .render_to_window(
            hwnd, surface, snapshot, content, theme, geom.dpi, client_x, client_y, w_px, h_px,
            hovered,
        )
        .is_ok()
    {
        // Commit position + size + DIB atomically via UpdateLayeredWindow above before
        // raising Z-order and revealing a previously hidden bar, avoiding a 1-frame left flash.
        ensure_child_top_order(hwnd, sibling_bar);
        if !was_visible {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
        }
        true
    } else {
        false
    }
}

fn screen_to_taskbar_client(
    taskbar_hwnd: HWND,
    screen_x: i32,
    screen_y: i32,
) -> Option<(i32, i32)> {
    unsafe {
        let mut pt = POINT {
            x: screen_x,
            y: screen_y,
        };
        if ScreenToClient(taskbar_hwnd, &mut pt).as_bool() {
            Some((pt.x, pt.y))
        } else {
            None
        }
    }
}

fn ensure_attached_to_taskbar(hwnd: HWND, taskbar_hwnd: HWND) -> bool {
    unsafe {
        if hwnd.0.is_null() || taskbar_hwnd.0.is_null() {
            return false;
        }
        if GetParent(hwnd).ok() == Some(taskbar_hwnd) {
            return true;
        }
        if IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let new_style = (style & !WS_POPUP.0) | WS_CHILD.0 | WS_CLIPSIBLINGS.0;
        if new_style != style {
            SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);
        }
        // Note: Win32 SetParent returns the previous parent (NULL for top-level windows),
        // which windows 0.58 turns into Err; always verify attachment via GetParent().
        let _ = SetParent(hwnd, taskbar_hwnd);
        if GetParent(hwnd).ok() != Some(taskbar_hwnd) {
            return false;
        }
        let _ = SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        true
    }
}

fn hide_window(hwnd: HWND) {
    unsafe {
        if !hwnd.0.is_null() && IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Ensures our child bar sits above Shell_TrayWnd's XAML DesktopWindowContentBridge
/// inside Shell_TrayWnd's child list without changing position, size, or visibility.
fn ensure_child_top_order(hwnd: HWND, sibling_bar: HWND) {
    unsafe {
        if hwnd.0.is_null() {
            return;
        }

        let mut needs_raise = !IsWindowVisible(hwnd).as_bool();
        if !needs_raise {
            let mut cur = hwnd;
            for _ in 0..32 {
                match GetWindow(cur, GW_HWNDPREV) {
                    Ok(prev) if !prev.0.is_null() => {
                        if prev != sibling_bar && IsWindowVisible(prev).as_bool() {
                            needs_raise = true;
                            break;
                        }
                        cur = prev;
                    }
                    _ => break,
                }
            }
        }

        if needs_raise {
            let _ = SetWindowPos(
                hwnd,
                HWND_TOP,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_anchor_grows_leftward_without_overlapping_tray() {
        let geom = TaskbarGeometry {
            taskbar_hwnd: HWND::default(),
            taskbar_rect: RECT {
                left: 0,
                top: 1032,
                right: 1920,
                bottom: 1080,
            },
            tray_rect: Some(RECT {
                left: 1650,
                top: 1032,
                right: 1920,
                bottom: 1080,
            }),
            dpi: 96,
            taskbar_hidden: false,
        };
        let zone = ZoneLayout {
            anchor: AnchorEdge::Right,
            offset_x: 8,
            visible: true,
        };

        let (x1, _, w1, _) = compute_zone_rect(&geom, &zone, 120.0);
        let (x2, _, w2, _) = compute_zone_rect(&geom, &zone, 200.0);
        assert_eq!(x1 + w1, 1650 - 8);
        assert_eq!(x2 + w2, 1650 - 8);
        assert!(x2 < x1);
    }

    #[test]
    fn left_anchor_grows_rightward() {
        let geom = TaskbarGeometry {
            taskbar_hwnd: HWND::default(),
            taskbar_rect: RECT {
                left: 0,
                top: 1032,
                right: 1920,
                bottom: 1080,
            },
            tray_rect: None,
            dpi: 144, // 150% DPI
            taskbar_hidden: false,
        };
        let zone = ZoneLayout {
            anchor: AnchorEdge::Left,
            offset_x: 12,
            visible: true,
        };

        let (x1, _, w1, _) = compute_zone_rect(&geom, &zone, 100.0);
        let (x2, _, w2, _) = compute_zone_rect(&geom, &zone, 180.0);
        assert_eq!(x1, 18); // 12 * 1.5
        assert_eq!(x2, 18);
        assert!(w2 > w1);
    }

    #[test]
    fn shell_and_xaml_classes_never_trigger_fullscreen_hide() {
        for cls in [
            "Shell_TrayWnd",
            "Windows.UI.Core.CoreWindow",
            "XamlExplorerHostIslandWindow",
            "TopLevelWindowForOverflowXamlIsland",
            "NotifyIconOverflowWindow",
            "#32768",
            "Progman",
            "WorkerW",
            "AirMonitor_PopupFlyout",
        ] {
            assert!(is_shell_or_nonfullscreen_class(cls));
        }
        assert!(!is_shell_or_nonfullscreen_class("Chrome_WidgetWin_1"));
        assert!(!is_shell_or_nonfullscreen_class("UnrealWindow"));
    }

    #[test]
    fn child_layered_bar_attaches_and_updates_in_parent_coords() {
        use crate::config::AppConfig;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, WS_EX_LAYERED, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW,
        };

        unsafe {
            let hinst = GetModuleHandleW(None).unwrap();
            let parent = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("TestTaskbarParent"),
                WS_POPUP,
                100,
                200,
                800,
                48,
                HWND::default(),
                None,
                hinst,
                None,
            )
            .unwrap();
            let _ = ShowWindow(
                parent,
                windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE,
            );

            let child = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("TestChildBar"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                HWND::default(),
                None,
                hinst,
                None,
            )
            .unwrap();

            assert!(ensure_attached_to_taskbar(child, parent));
            assert_eq!(GetParent(child).ok(), Some(parent));

            let mut renderer = TaskbarRenderer::new().unwrap();
            let mut surface = WindowSurface::new();
            let mut snap = MonitorSnapshot::new(AppConfig::default());
            let geom = TaskbarGeometry {
                taskbar_hwnd: parent,
                taskbar_rect: RECT {
                    left: 100,
                    top: 200,
                    right: 900,
                    bottom: 248,
                },
                tray_rect: Some(RECT {
                    left: 760,
                    top: 200,
                    right: 900,
                    bottom: 248,
                }),
                dpi: 96,
                taskbar_hidden: false,
            };
            let zone = ZoneLayout {
                anchor: AnchorEdge::Right,
                offset_x: 8,
                visible: true,
            };

            let (sx1, sy1, w1, h1) = compute_zone_rect(&geom, &zone, 200.0);
            let (cx1, cy1) = screen_to_taskbar_client(parent, sx1, sy1).unwrap();
            renderer
                .render_to_window(
                    child,
                    &mut surface,
                    &snap,
                    RenderContent::Combined,
                    TaskbarTheme::Dark,
                    96,
                    cx1,
                    cy1,
                    w1,
                    h1,
                    false,
                )
                .unwrap();
            ensure_child_top_order(child, HWND::default());
            let _ = ShowWindow(child, SW_SHOWNOACTIVATE);

            assert!(IsWindowVisible(child).as_bool());
            let mut p_rect = RECT::default();
            let mut c_rect = RECT::default();
            GetWindowRect(parent, &mut p_rect).unwrap();
            GetWindowRect(child, &mut c_rect).unwrap();
            assert_eq!(c_rect.left, sx1);
            assert_eq!(c_rect.right, 760 - 8);
            assert!(c_rect.left >= p_rect.left && c_rect.right <= p_rect.right);
            assert!(c_rect.top >= p_rect.top && c_rect.bottom <= p_rect.bottom);

            // Narrower width must atomically update both left and width so right edge stays pinned.
            snap.config.layout_mode = LayoutMode::HardwareOnly;
            let (sx2, sy2, w2, h2) = compute_zone_rect(&geom, &zone, 120.0);
            let (cx2, cy2) = screen_to_taskbar_client(parent, sx2, sy2).unwrap();
            renderer
                .render_to_window(
                    child,
                    &mut surface,
                    &snap,
                    RenderContent::HardwareOnly,
                    TaskbarTheme::Dark,
                    96,
                    cx2,
                    cy2,
                    w2,
                    h2,
                    false,
                )
                .unwrap();
            GetWindowRect(child, &mut c_rect).unwrap();
            assert_eq!(c_rect.left, sx2);
            assert_eq!(c_rect.right, 760 - 8);
            assert!(c_rect.left > sx1);

            let _ = DefWindowProcW(
                child,
                0,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
            let _ = DestroyWindow(child);
            let _ = DestroyWindow(parent);
        }
    }

    #[test]
    fn split_mode_bar_keys_isolate_hw_and_wireless_revisions() {
        use crate::config::AppConfig;
        use std::sync::Arc;

        let geom = TaskbarGeometry {
            taskbar_hwnd: HWND::default(),
            taskbar_rect: RECT {
                left: 0,
                top: 1032,
                right: 1920,
                bottom: 1080,
            },
            tray_rect: None,
            dpi: 96,
            taskbar_hidden: false,
        };
        let snap = Arc::new(MonitorSnapshot::new(AppConfig::default()));
        let vs0 = VersionedSnapshot {
            snapshot: Arc::clone(&snap),
            hw_rev: 1,
            wireless_rev: 1,
            config_rev: 1,
        };
        let vs_hw_tick = VersionedSnapshot {
            snapshot: Arc::clone(&snap),
            hw_rev: 2,
            wireless_rev: 1,
            config_rev: 1,
        };
        let vs_wl_tick = VersionedSnapshot {
            snapshot: Arc::clone(&snap),
            hw_rev: 2,
            wireless_rev: 2,
            config_rev: 1,
        };

        let hw0 = BarRenderKey::for_hardware_bar(
            LayoutMode::Split,
            TaskbarTheme::Dark,
            &geom,
            false,
            false,
            &vs0,
        );
        let dev0 = BarRenderKey::for_devices_bar(
            LayoutMode::Split,
            TaskbarTheme::Dark,
            &geom,
            false,
            false,
            &vs0,
        );
        let hw1 = BarRenderKey::for_hardware_bar(
            LayoutMode::Split,
            TaskbarTheme::Dark,
            &geom,
            false,
            false,
            &vs_hw_tick,
        );
        let dev1 = BarRenderKey::for_devices_bar(
            LayoutMode::Split,
            TaskbarTheme::Dark,
            &geom,
            false,
            false,
            &vs_hw_tick,
        );
        assert_ne!(hw0, hw1);
        assert_eq!(dev0, dev1);

        let hw2 = BarRenderKey::for_hardware_bar(
            LayoutMode::Split,
            TaskbarTheme::Dark,
            &geom,
            false,
            false,
            &vs_wl_tick,
        );
        let dev2 = BarRenderKey::for_devices_bar(
            LayoutMode::Split,
            TaskbarTheme::Dark,
            &geom,
            false,
            false,
            &vs_wl_tick,
        );
        assert_eq!(hw1, hw2);
        assert_ne!(dev1, dev2);

        // HardwareOnly ignores wireless_rev; DevicesOnly ignores hw_rev.
        let hw_only0 = BarRenderKey::for_hardware_bar(
            LayoutMode::HardwareOnly,
            TaskbarTheme::Dark,
            &geom,
            false,
            false,
            &vs_hw_tick,
        );
        let hw_only1 = BarRenderKey::for_hardware_bar(
            LayoutMode::HardwareOnly,
            TaskbarTheme::Dark,
            &geom,
            false,
            false,
            &vs_wl_tick,
        );
        assert_eq!(hw_only0, hw_only1);

        let dev_only0 = BarRenderKey::for_devices_bar(
            LayoutMode::DevicesOnly,
            TaskbarTheme::Dark,
            &geom,
            false,
            false,
            &vs0,
        );
        let dev_only1 = BarRenderKey::for_devices_bar(
            LayoutMode::DevicesOnly,
            TaskbarTheme::Dark,
            &geom,
            false,
            false,
            &vs_hw_tick,
        );
        let dev_only2 = BarRenderKey::for_devices_bar(
            LayoutMode::DevicesOnly,
            TaskbarTheme::Dark,
            &geom,
            false,
            false,
            &vs_wl_tick,
        );
        assert_eq!(dev_only0, dev_only1);
        assert_ne!(dev_only1, dev_only2);

        let comb0 = BarRenderKey::for_combined(TaskbarTheme::Dark, &geom, false, false, &vs0);
        let comb1 =
            BarRenderKey::for_combined(TaskbarTheme::Dark, &geom, false, false, &vs_hw_tick);
        assert_ne!(comb0, comb1);
    }

    #[test]
    fn devices_only_hides_empty_bar_and_renders_when_visible_device_present() {
        use crate::config::AppConfig;
        use crate::state::{DeviceKind, Transport, WirelessDevice};
        use std::sync::Arc;
        use std::time::Instant;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        };

        unsafe {
            let hinst = GetModuleHandleW(None).unwrap();
            let parent = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("TestTaskbarParentDevices"),
                WS_POPUP,
                0,
                0,
                1000,
                48,
                HWND::default(),
                None,
                hinst,
                None,
            )
            .unwrap();
            let _ = ShowWindow(parent, SW_SHOWNOACTIVATE);
            let primary = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("TestPrimaryBar"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                HWND::default(),
                None,
                hinst,
                None,
            )
            .unwrap();
            let secondary = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("TestSecondaryBar"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                HWND::default(),
                None,
                hinst,
                None,
            )
            .unwrap();

            let mut mgr = TaskbarManager::new(primary, secondary).unwrap();
            let geom = TaskbarGeometry {
                taskbar_hwnd: parent,
                taskbar_rect: RECT {
                    left: 0,
                    top: 0,
                    right: 1000,
                    bottom: 48,
                },
                tray_rect: Some(RECT {
                    left: 850,
                    top: 0,
                    right: 1000,
                    bottom: 48,
                }),
                dpi: 96,
                taskbar_hidden: false,
            };

            let mut cfg = AppConfig {
                layout_mode: LayoutMode::DevicesOnly,
                ..AppConfig::default()
            };
            cfg.devices_zone.anchor = AnchorEdge::Right;
            cfg.devices_zone.offset_x = 8;

            let mut snap = MonitorSnapshot::new(cfg.clone());
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
            let vs_empty = VersionedSnapshot {
                snapshot: Arc::new(snap.clone()),
                hw_rev: 1,
                wireless_rev: 1,
                config_rev: 1,
            };
            mgr.apply_bars_with_geom(&vs_empty, &geom, false);
            assert!(!IsWindowVisible(primary).as_bool());
            assert!(!IsWindowVisible(secondary).as_bool());

            snap.devices.push(WirelessDevice {
                id: "vt3".into(),
                name: "VT3".into(),
                kind: DeviceKind::Mouse,
                transport: Transport::Wireless24G,
                battery_pct: Some(75),
                charging: false,
                sub_batteries: None,
                online: true,
                last_seen: Instant::now(),
                detail_note: None,
            });
            let vs_one = VersionedSnapshot {
                snapshot: Arc::new(snap),
                hw_rev: 1,
                wireless_rev: 2,
                config_rev: 1,
            };
            mgr.apply_bars_with_geom(&vs_one, &geom, false);
            assert!(IsWindowVisible(primary).as_bool());
            assert!(!IsWindowVisible(secondary).as_bool());
            let mut rect = RECT::default();
            GetWindowRect(primary, &mut rect).unwrap();
            assert_eq!(rect.right, 850 - 8);

            // Switch to HardwareOnly -> primary stays right/left per hardware_zone, secondary hidden.
            let mut hw_snap = MonitorSnapshot::new(AppConfig {
                layout_mode: LayoutMode::HardwareOnly,
                ..cfg
            });
            hw_snap.hw.cpu_usage_pct = 25.0;
            let vs_hw = VersionedSnapshot {
                snapshot: Arc::new(hw_snap),
                hw_rev: 2,
                wireless_rev: 2,
                config_rev: 2,
            };
            mgr.apply_bars_with_geom(&vs_hw, &geom, false);
            assert!(IsWindowVisible(primary).as_bool());
            assert!(!IsWindowVisible(secondary).as_bool());

            let _ = DestroyWindow(secondary);
            let _ = DestroyWindow(primary);
            let _ = DestroyWindow(parent);
        }
    }

    #[test]
    fn vertical_taskbar_is_detected_from_rect() {
        let horiz = RECT {
            left: 0,
            top: 1032,
            right: 1920,
            bottom: 1080,
        };
        assert!(!is_vertical_taskbar(&horiz));

        let vert_left = RECT {
            left: 0,
            top: 0,
            right: 62,
            bottom: 1080,
        };
        assert!(is_vertical_taskbar(&vert_left));

        let vert_right = RECT {
            left: 1858,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!(is_vertical_taskbar(&vert_right));

        let zero = RECT::default();
        assert!(!is_vertical_taskbar(&zero));
    }

    fn make_test_geom(hwnd_raw: isize, tray_left: Option<i32>, width: i32) -> TaskbarGeometry {
        TaskbarGeometry {
            taskbar_hwnd: HWND(hwnd_raw as *mut std::ffi::c_void),
            taskbar_rect: RECT {
                left: 0,
                top: 1032,
                right: width,
                bottom: 1080,
            },
            tray_rect: tray_left.map(|l| RECT {
                left: l,
                top: 1032,
                right: width,
                bottom: 1080,
            }),
            dpi: 96,
            taskbar_hidden: false,
        }
    }

    #[test]
    fn geometry_stabilizer_freezes_during_fullscreen_and_filters_post_exit_transient_shift() {
        let mut stab = GeometryStabilizer::default();
        let t0 = Instant::now();
        let steady = make_test_geom(1, Some(1650), 1920);

        // Initial geometry commits immediately.
        let g0 = stab.resolve(t0, steady.clone(), false);
        assert_eq!(g0.tray_rect.unwrap().left, 1650);
        assert!(!stab.has_pending());

        // While fullscreen is active, transient resolution/tray shifts are frozen.
        let fs_glitch = make_test_geom(1, Some(1100), 1280);
        let g1 = stab.resolve(t0 + Duration::from_secs(1), fs_glitch, true);
        assert_eq!(g1.tray_rect.unwrap().left, 1650);
        assert_eq!(g1.taskbar_rect.right, 1920);
        assert!(!stab.has_pending());

        // Immediately upon exiting fullscreen, TrayNotifyWnd reports a transient left-shift
        // (e.g. Win11 XAML animation frame at 1380 instead of 1650).
        let exit_glitch = make_test_geom(1, Some(1380), 1920);
        let g2 = stab.resolve(t0 + Duration::from_secs(2), exit_glitch.clone(), false);
        assert_eq!(g2.tray_rect.unwrap().left, 1650);
        assert!(stab.has_pending());

        // Even if sampled again 180ms later during post-fullscreen cooldown, it stays frozen
        // until the post-fullscreen settle window (320ms) has elapsed.
        let g3 = stab.resolve(
            t0 + Duration::from_secs(2) + Duration::from_millis(180),
            exit_glitch,
            false,
        );
        assert_eq!(g3.tray_rect.unwrap().left, 1650);
        assert!(stab.has_pending());

        // When TrayNotifyWnd settles back to 1650, pending clears without ever jumping left.
        let g4 = stab.resolve(
            t0 + Duration::from_secs(2) + Duration::from_millis(360),
            steady,
            false,
        );
        assert_eq!(g4.tray_rect.unwrap().left, 1650);
        assert!(!stab.has_pending());
    }

    #[test]
    fn geometry_stabilizer_commits_genuine_change_after_settle_window() {
        let mut stab = GeometryStabilizer::default();
        let t0 = Instant::now();
        let g_start = make_test_geom(1, Some(1650), 1920);
        let g_shifted = make_test_geom(1, Some(1600), 1920);

        let _ = stab.resolve(t0, g_start, false);

        // First observation of new tray width is held pending.
        let r1 = stab.resolve(t0 + Duration::from_millis(100), g_shifted.clone(), false);
        assert_eq!(r1.tray_rect.unwrap().left, 1650);
        assert!(stab.has_pending());

        // Second observation before settle window elapses still waits.
        let r2 = stab.resolve(t0 + Duration::from_millis(200), g_shifted.clone(), false);
        assert_eq!(r2.tray_rect.unwrap().left, 1650);
        assert!(stab.has_pending());

        // Once GEOM_SETTLE_NORMAL_MS (180ms) elapses with matching candidate, commit once.
        let r3 = stab.resolve(t0 + Duration::from_millis(300), g_shifted, false);
        assert_eq!(r3.tray_rect.unwrap().left, 1600);
        assert!(!stab.has_pending());

        // Post-fullscreen genuine resolution change commits after GEOM_SETTLE_POST_FS_MS (320ms).
        let _ = stab.resolve(
            t0 + Duration::from_secs(1),
            make_test_geom(1, Some(1600), 1920),
            true,
        );
        let g_2560 = make_test_geom(1, Some(2290), 2560);
        let e1 = stab.resolve(t0 + Duration::from_secs(2), g_2560.clone(), false);
        assert_eq!(e1.taskbar_rect.right, 1920);
        let e2 = stab.resolve(
            t0 + Duration::from_secs(2) + Duration::from_millis(350),
            g_2560,
            false,
        );
        assert_eq!(e2.taskbar_rect.right, 2560);
        assert_eq!(e2.tray_rect.unwrap().left, 2290);
        assert!(!stab.has_pending());
    }

    #[test]
    fn geometry_stabilizer_commits_immediately_on_taskbar_hwnd_change_or_hide() {
        let mut stab = GeometryStabilizer::default();
        let t0 = Instant::now();
        let _ = stab.resolve(t0, make_test_geom(1, Some(1650), 1920), false);

        // New Shell_TrayWnd HWND (Explorer restart) commits immediately.
        let new_tb = make_test_geom(2, Some(1620), 1920);
        let r1 = stab.resolve(t0 + Duration::from_millis(10), new_tb, false);
        assert_eq!(r1.taskbar_hwnd.0 as isize, 2);
        assert_eq!(r1.tray_rect.unwrap().left, 1620);
        assert!(!stab.has_pending());

        // Taskbar hidden commits immediately, and un-hiding commits immediately.
        let mut hidden_tb = make_test_geom(2, Some(1620), 1920);
        hidden_tb.taskbar_hidden = true;
        let r2 = stab.resolve(t0 + Duration::from_millis(20), hidden_tb, false);
        assert!(r2.taskbar_hidden);
        assert!(!stab.has_pending());

        let visible_tb = make_test_geom(2, Some(1620), 1920);
        let r3 = stab.resolve(t0 + Duration::from_millis(30), visible_tb, false);
        assert!(!r3.taskbar_hidden);
        assert!(!stab.has_pending());
    }

    #[test]
    fn fullscreen_keeps_child_bar_visible_and_unattached_bar_stays_hidden() {
        use crate::config::AppConfig;
        use std::sync::Arc;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        };

        unsafe {
            let hinst = GetModuleHandleW(None).unwrap();
            let parent = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("TestTaskbarParentFS"),
                WS_POPUP,
                0,
                0,
                1000,
                48,
                HWND::default(),
                None,
                hinst,
                None,
            )
            .unwrap();
            let _ = ShowWindow(parent, SW_SHOWNOACTIVATE);
            let primary = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("TestPrimaryFS"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                HWND::default(),
                None,
                hinst,
                None,
            )
            .unwrap();
            let secondary = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("TestSecondaryFS"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                HWND::default(),
                None,
                hinst,
                None,
            )
            .unwrap();

            let mut mgr = TaskbarManager::new(primary, secondary).unwrap();
            let geom = TaskbarGeometry {
                taskbar_hwnd: parent,
                taskbar_rect: RECT {
                    left: 0,
                    top: 0,
                    right: 1000,
                    bottom: 48,
                },
                tray_rect: Some(RECT {
                    left: 850,
                    top: 0,
                    right: 1000,
                    bottom: 48,
                }),
                dpi: 96,
                taskbar_hidden: false,
            };

            let mut cfg = AppConfig {
                layout_mode: LayoutMode::HardwareOnly,
                ..AppConfig::default()
            };
            cfg.hardware_zone.anchor = AnchorEdge::Right;
            cfg.hardware_zone.offset_x = 8;

            let snap = MonitorSnapshot::new(cfg);
            let vs1 = VersionedSnapshot {
                snapshot: Arc::new(snap.clone()),
                hw_rev: 1,
                wireless_rev: 1,
                config_rev: 1,
            };
            mgr.apply_bars_with_geom_inner(&vs1, &geom, false, false);
            assert!(IsWindowVisible(primary).as_bool());
            assert_eq!(GetParent(primary).ok(), Some(parent));
            let mut before_rect = RECT::default();
            GetWindowRect(primary, &mut before_rect).unwrap();
            assert_eq!(before_rect.right, 850 - 8);

            // During fullscreen, the child bar remains WS_VISIBLE at its pinned position
            // (no SW_HIDE -> SW_SHOWNOACTIVATE flash) and skips redundant redraws.
            let vs2 = VersionedSnapshot {
                snapshot: Arc::new(snap),
                hw_rev: 2,
                wireless_rev: 1,
                config_rev: 1,
            };
            mgr.apply_bars_with_geom_inner(&vs2, &geom, false, true);
            assert!(IsWindowVisible(primary).as_bool());
            let mut fs_rect = RECT::default();
            GetWindowRect(primary, &mut fs_rect).unwrap();
            assert_eq!(fs_rect, before_rect);

            // Exiting fullscreen updates content in place without hiding/re-showing.
            mgr.apply_bars_with_geom_inner(&vs2, &geom, false, false);
            assert!(IsWindowVisible(primary).as_bool());
            let mut after_rect = RECT::default();
            GetWindowRect(primary, &mut after_rect).unwrap();
            assert_eq!(after_rect, before_rect);

            // Unattached window (invalid parent HWND) must never be shown.
            let bad_geom = TaskbarGeometry {
                taskbar_hwnd: HWND::default(),
                ..geom
            };
            mgr.apply_bars_with_geom_inner(&vs2, &bad_geom, false, false);
            assert!(!IsWindowVisible(primary).as_bool());

            let _ = DestroyWindow(secondary);
            let _ = DestroyWindow(primary);
            let _ = DestroyWindow(parent);
        }
    }
}
