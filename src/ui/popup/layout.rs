use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;

use crate::config::{AnchorEdge, CombinedOrder, HwMetric, LayoutMode};
use crate::state::{format_speed_full, HardwareMetrics, MonitorSnapshot};
use crate::updater::UpdateStatus;

pub const POPUP_WIDTH_LOGICAL: f32 = 368.0;
pub const POPUP_FIXED_HEIGHT_LOGICAL: f32 = 468.0;
pub const POPUP_MIN_HEIGHT_LOGICAL: f32 = 240.0;
pub const POPUP_MIN_MAX_HEIGHT_LOGICAL: f32 = 360.0;
pub const HEADER_HEIGHT: f32 = 42.0;
pub const PAD_X: f32 = 14.0;
pub const CARD_PAD: f32 = 12.0;

pub(crate) const HW_CARD_TOP_PAD: f32 = 10.0;
pub(crate) const HW_CARD_HEADER_H: f32 = 24.0;
pub(crate) const HW_ROW_CPU_H: f32 = 22.0;
pub(crate) const HW_ROW_CPU_EXT_H: f32 = 20.0;
pub(crate) const HW_ROW_MEM_H: f32 = 22.0;
pub(crate) const HW_ROW_NIC_H: f32 = 20.0;
pub(crate) const HW_ROW_NET_H: f32 = 20.0;
pub(crate) const HW_ROW_DISK_H: f32 = 20.0;
pub(crate) const HW_ROW_BAT_H: f32 = 20.0;
pub(crate) const HW_PRE_DIVIDER_GAP: f32 = 4.0;
pub(crate) const HW_DIVIDER_GAP: f32 = 6.0;
pub(crate) const HW_GPU_TITLE_H: f32 = 20.0;
pub(crate) const HW_GPU_ROW_H: f32 = 19.0;
pub(crate) const HW_CARD_BOTTOM_PAD: f32 = 10.0;

pub fn compute_max_popup_height_dip(monitor_rect: RECT, dpi: u32) -> f32 {
    let scale = dpi.max(96) as f32 / 96.0;
    let monitor_height_dip = (monitor_rect.bottom - monitor_rect.top).max(0) as f32 / scale;
    (monitor_height_dip * 0.5).max(POPUP_MIN_MAX_HEIGHT_LOGICAL)
}

pub fn compute_popup_height_dip(monitor_rect: RECT, dpi: u32) -> f32 {
    POPUP_FIXED_HEIGHT_LOGICAL.clamp(
        POPUP_MIN_HEIGHT_LOGICAL,
        compute_max_popup_height_dip(monitor_rect, dpi),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupTab {
    Overview,
    Settings,
    About,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsSubTab {
    #[default]
    Metrics,
    Layout,
    General,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MetricSlotRect {
    pub index: usize,
    pub rect: D2D_RECT_F,
}

pub(crate) struct MetricGroup {
    pub title: &'static str,
    pub metrics: &'static [HwMetric],
}

pub(crate) const METRIC_GROUPS: &[MetricGroup] = &[
    MetricGroup {
        title: "核心指标",
        metrics: &[
            HwMetric::NetUp,
            HwMetric::NetDown,
            HwMetric::CpuUsage,
            HwMetric::MemUsage,
            HwMetric::CpuFreq,
        ],
    },
    MetricGroup {
        title: "显卡 (GPU)",
        metrics: &[
            HwMetric::GpuUsage,
            HwMetric::GpuTemp,
            HwMetric::GpuHotspot,
            HwMetric::GpuPower,
            HwMetric::GpuVram,
            HwMetric::GpuFan,
        ],
    },
    MetricGroup {
        title: "磁盘与电池",
        metrics: &[
            HwMetric::DiskRead,
            HwMetric::DiskWrite,
            HwMetric::SysBattery,
        ],
    },
    MetricGroup {
        title: "扩展传感器（需 HWiNFO / AIDA64 等）",
        metrics: &[
            HwMetric::CpuTemp,
            HwMetric::CpuPower,
            HwMetric::CpuFan,
            HwMetric::DiskTemp,
        ],
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PopupAction {
    SwitchTab(PopupTab),
    SwitchSettingsSubTab(SettingsSubTab),
    RefreshWireless,
    CheckForUpdates,
    StartDownloadUpdate,
    OpenUrl(String),
    OpenConfig,
    ToggleHiddenDevice(String),
    ToggleMetric(HwMetric),
    SelectMetricSlot(usize),
    SwapMetricSlots(usize, usize),
    MoveSelectedMetricLeft(usize),
    SwapSelectedMetricRow(usize),
    MoveSelectedMetricRight(usize),
    RemoveMetricSlot(usize),
    ResetDefaultMetrics,
    SetLayout(LayoutMode, Option<CombinedOrder>),
    SetAnchor {
        is_devices: bool,
        is_combined: bool,
        anchor: AnchorEdge,
    },
    StepOffset {
        is_devices: bool,
        is_combined: bool,
        delta: i32,
    },
    ResetZoneOffset {
        is_devices: bool,
        is_combined: bool,
    },
    StepLowThreshold(i8),
    ToggleHideUnknownBatteryDevices,
    StepHwPollInterval(i8),
    ToggleEnableExternalSensors,
    ToggleHideUnavailableMetrics,
    ToggleAutostart,
}

pub(crate) struct HitBox {
    pub rect: D2D_RECT_F,
    pub action: PopupAction,
}

pub(crate) struct DeviceCardLayout {
    pub index: usize,
    pub rect: D2D_RECT_F,
}

pub(crate) struct HwCardSpec {
    pub provider_summary: String,
    pub cpu_ext_line: Option<String>,
    pub disk_line: Option<String>,
    pub sys_battery_line: Option<String>,
    pub gpu_items: Vec<(&'static str, String)>,
    pub card_h: f32,
}

pub(crate) fn build_hw_card_spec(hw: &HardwareMetrics) -> HwCardSpec {
    let gpu_p = hw.gpu_provider.trim();
    let sensor_p = hw.sensor_provider.trim();
    let provider_summary = match (gpu_p.is_empty(), sensor_p.is_empty()) {
        (false, false) => format!("{gpu_p} · {sensor_p}"),
        (false, true) => gpu_p.to_string(),
        (true, false) => sensor_p.to_string(),
        (true, true) => "基础模式".to_string(),
    };

    let mut cpu_ext_parts = Vec::with_capacity(3);
    if let Some(t) = hw.cpu_temp_c.filter(|v| v.is_finite()) {
        cpu_ext_parts.push(format!("{:.0}°C", t));
    }
    if let Some(p) = hw.cpu_power_w.filter(|v| v.is_finite()) {
        cpu_ext_parts.push(format!("{:.0}W", p));
    }
    if let Some(r) = hw.cpu_fan_rpm {
        cpu_ext_parts.push(format!("{} RPM", r));
    }
    let cpu_ext_line = if cpu_ext_parts.is_empty() {
        None
    } else {
        Some(cpu_ext_parts.join(" · "))
    };

    let mut disk_parts = Vec::with_capacity(3);
    if let Some(r) = hw.disk_read_bps.filter(|v| v.is_finite()) {
        disk_parts.push(format!("读 {}", format_speed_full(r)));
    }
    if let Some(w) = hw.disk_write_bps.filter(|v| v.is_finite()) {
        disk_parts.push(format!("写 {}", format_speed_full(w)));
    }
    if let Some(t) = hw.disk_temp_c.filter(|v| v.is_finite()) {
        disk_parts.push(format!("{:.0}°C", t));
    }
    let disk_line = if disk_parts.is_empty() {
        None
    } else {
        Some(disk_parts.join(" · "))
    };

    let sys_battery_line = hw.sys_battery_pct.map(|pct| {
        let p = pct.min(100);
        if hw.sys_battery_charging {
            format!("⚡ {}% (充电中)", p)
        } else {
            format!("{}% (未充电)", p)
        }
    });

    let mut gpu_items = Vec::with_capacity(6);
    if let Some(u) = hw.gpu_usage_pct.filter(|v| v.is_finite()) {
        gpu_items.push(("占用", format!("{:.0}%", u)));
    }
    if let Some(t) = hw.gpu_temp_c.filter(|v| v.is_finite()) {
        gpu_items.push(("温度", format!("{:.0}°C", t)));
    }
    if let Some(h) = hw.gpu_hotspot_c.filter(|v| v.is_finite()) {
        gpu_items.push(("热点", format!("{:.0}°C", h)));
    }
    if let Some(p) = hw.gpu_power_w.filter(|v| v.is_finite()) {
        gpu_items.push(("功耗", format!("{:.0}W", p)));
    }
    if let Some(u) = hw.gpu_vram_used_gb.filter(|v| v.is_finite()) {
        let vram = match hw.gpu_vram_total_gb.filter(|t| t.is_finite()) {
            Some(t) => format!("{:.1}/{:.0}GB", u, t),
            None => format!("{:.1}GB", u),
        };
        gpu_items.push(("显存", vram));
    }
    if let Some(rpm) = hw.gpu_fan_rpm {
        gpu_items.push(("风扇", format!("{} RPM", rpm)));
    } else if let Some(pct) = hw.gpu_fan_pct {
        gpu_items.push(("风扇", format!("{}%", pct.min(100))));
    }

    let gpu_rows = gpu_items.len().div_ceil(2);
    let mut card_h = HW_CARD_TOP_PAD
        + HW_CARD_HEADER_H
        + HW_ROW_CPU_H
        + HW_ROW_MEM_H
        + HW_ROW_NIC_H
        + HW_ROW_NET_H
        + HW_PRE_DIVIDER_GAP
        + HW_DIVIDER_GAP
        + HW_GPU_TITLE_H
        + (gpu_rows as f32) * HW_GPU_ROW_H
        + HW_CARD_BOTTOM_PAD;
    if cpu_ext_line.is_some() {
        card_h += HW_ROW_CPU_EXT_H;
    }
    if disk_line.is_some() {
        card_h += HW_ROW_DISK_H;
    }
    if sys_battery_line.is_some() {
        card_h += HW_ROW_BAT_H;
    }

    HwCardSpec {
        provider_summary,
        cpu_ext_line,
        disk_line,
        sys_battery_line,
        gpu_items,
        card_h,
    }
}

pub(crate) struct OverviewLayout {
    pub card_w: f32,
    pub hw_spec: HwCardSpec,
    pub hw_card_rect: D2D_RECT_F,
    pub wireless_header_y: f32,
    pub refresh_rect: D2D_RECT_F,
    pub empty_rect: Option<D2D_RECT_F>,
    pub device_cards: Vec<DeviceCardLayout>,
    pub content_bottom: f32,
}

pub(crate) struct MetricGroupLayout {
    pub title: &'static str,
    pub title_rect: D2D_RECT_F,
    pub chips: Vec<(HwMetric, D2D_RECT_F)>,
}

pub(crate) struct SettingsMetricsLayout {
    pub grid_card_rect: D2D_RECT_F,
    pub grid_title_rect: D2D_RECT_F,
    pub grid_subtitle_rect: D2D_RECT_F,
    pub reset_btn_rect: D2D_RECT_F,
    pub grid_box_rect: D2D_RECT_F,
    pub grid_cells: Vec<MetricSlotRect>,
    pub empty_slot_rect: Option<D2D_RECT_F>,
    pub act_left_rect: D2D_RECT_F,
    pub act_swap_row_rect: D2D_RECT_F,
    pub act_right_rect: D2D_RECT_F,
    pub act_remove_rect: D2D_RECT_F,
    pub chips_card_rect: D2D_RECT_F,
    pub chips_title_rect: D2D_RECT_F,
    pub chips_hint_rect: D2D_RECT_F,
    pub groups: Vec<MetricGroupLayout>,
}

pub(crate) struct ZoneBoxLayout {
    pub label: &'static str,
    pub is_devices: bool,
    pub is_combined: bool,
    pub box_rect: D2D_RECT_F,
    pub label_rect: D2D_RECT_F,
    pub left_anchor_rect: D2D_RECT_F,
    pub right_anchor_rect: D2D_RECT_F,
    pub offset_label_rect: D2D_RECT_F,
    pub stepper_rect: D2D_RECT_F,
    pub reset_rect: D2D_RECT_F,
}

pub(crate) struct SettingsLayoutModeLayout {
    pub mode_card_rect: D2D_RECT_F,
    pub mode_title_rect: D2D_RECT_F,
    pub mode_subtitle_rect: D2D_RECT_F,
    pub mode_seg_rect: D2D_RECT_F,
    pub order_divider_y: Option<f32>,
    pub order_label_rect: Option<D2D_RECT_F>,
    pub order_dev_first_rect: Option<D2D_RECT_F>,
    pub order_hw_first_rect: Option<D2D_RECT_F>,
    pub dock_card_rect: D2D_RECT_F,
    pub dock_title_rect: D2D_RECT_F,
    pub dock_subtitle_rect: D2D_RECT_F,
    pub zones: Vec<ZoneBoxLayout>,
}

pub(crate) struct GeneralRowLayout {
    pub row_rect: D2D_RECT_F,
    pub title_rect: D2D_RECT_F,
    pub subtitle_rect: D2D_RECT_F,
    pub control_rect: D2D_RECT_F,
}

pub(crate) struct SettingsGeneralLayout {
    pub collect_card_rect: D2D_RECT_F,
    pub collect_title_rect: D2D_RECT_F,
    pub poll_row: GeneralRowLayout,
    pub collect_div1_y: f32,
    pub ext_sensors_row: GeneralRowLayout,
    pub collect_div2_y: f32,
    pub hide_unavail_row: GeneralRowLayout,
    pub device_card_rect: D2D_RECT_F,
    pub device_title_rect: D2D_RECT_F,
    pub low_bat_row: GeneralRowLayout,
    pub device_div1_y: f32,
    pub hide_unknown_row: GeneralRowLayout,
    pub device_div2_y: f32,
    pub autostart_row: GeneralRowLayout,
}

pub(crate) enum SettingsBodyLayout {
    Metrics(SettingsMetricsLayout),
    Layout(SettingsLayoutModeLayout),
    General(SettingsGeneralLayout),
}

pub(crate) struct SettingsLayout {
    pub sub_nav_rect: D2D_RECT_F,
    pub body: SettingsBodyLayout,
    pub content_bottom: f32,
}

pub(crate) struct AboutLayout {
    pub card_w: f32,
    pub info_card_rect: D2D_RECT_F,
    pub info_header_y: f32,
    pub version_badge_rect: D2D_RECT_F,
    pub info_subtitle_y: f32,
    pub info_divider_y: f32,
    pub info_kv_start_y: f32,
    pub repo_btn_rect: D2D_RECT_F,
    pub releases_btn_rect: D2D_RECT_F,
    pub update_card_rect: D2D_RECT_F,
    pub update_header_y: f32,
    pub check_btn_rect: D2D_RECT_F,
    pub update_status_rect: D2D_RECT_F,
    pub update_meta_rect: Option<D2D_RECT_F>,
    pub update_prompt_rect: Option<D2D_RECT_F>,
    pub progress_bar_rect: Option<D2D_RECT_F>,
    pub auto_update_btn_rect: Option<D2D_RECT_F>,
    pub manual_download_btn_rect: Option<D2D_RECT_F>,
    pub download_btn_rect: Option<D2D_RECT_F>,
    pub maint_card_rect: D2D_RECT_F,
    pub maint_row_rect: D2D_RECT_F,
    pub open_config_btn_rect: D2D_RECT_F,
    pub content_bottom: f32,
}

pub(crate) fn format_published_date(published_at: Option<&str>) -> Option<String> {
    let trimmed = published_at?.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(10).collect())
}

pub(crate) fn format_bytes_compact(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if (bytes as f64) < MB {
        format!("{:.0} KB", bytes as f64 / KB)
    } else {
        format!("{:.1} MB", bytes as f64 / MB)
    }
}

pub(crate) fn format_update_meta(
    published_at: Option<&str>,
    asset_size: Option<u64>,
) -> Option<String> {
    let date = format_published_date(published_at);
    let size = asset_size.filter(|&s| s > 0).map(format_bytes_compact);
    match (date, size) {
        (Some(d), Some(s)) => Some(format!("发布：{} · 大小：{}", d, s)),
        (Some(d), None) => Some(format!("发布日期：{}", d)),
        (None, Some(s)) => Some(format!("安装包大小：{}", s)),
        (None, None) => None,
    }
}

pub(crate) fn overview_visible_devices(
    snapshot: &MonitorSnapshot,
) -> Vec<(usize, &crate::state::WirelessDevice)> {
    snapshot
        .devices
        .iter()
        .enumerate()
        .filter(|(_, d)| !(snapshot.config.hide_unknown_battery_devices && d.battery_pct.is_none()))
        .collect()
}

pub(crate) fn compute_overview_layout(
    snapshot: &MonitorSnapshot,
    logical_w: f32,
    scroll_y: f32,
) -> OverviewLayout {
    let mut y = HEADER_HEIGHT + 8.0 - scroll_y;
    let card_w = (logical_w - PAD_X * 2.0).max(1.0);

    let hw_spec = build_hw_card_spec(&snapshot.hw);
    let hw_card_h = hw_spec.card_h;
    let hw_card_rect = D2D_RECT_F {
        left: PAD_X,
        top: y,
        right: PAD_X + card_w,
        bottom: y + hw_card_h,
    };
    y += hw_card_h + 10.0;

    let wireless_header_y = y;
    let refresh_rect = D2D_RECT_F {
        left: PAD_X + card_w - 74.0,
        top: y,
        right: PAD_X + card_w,
        bottom: y + 24.0,
    };
    y += 28.0;

    let visible_devices = overview_visible_devices(snapshot);
    let mut empty_rect = None;
    let mut device_cards = Vec::with_capacity(visible_devices.len());
    if visible_devices.is_empty() {
        empty_rect = Some(D2D_RECT_F {
            left: PAD_X,
            top: y,
            right: PAD_X + card_w,
            bottom: y + 48.0,
        });
        y += 56.0;
    } else {
        for (index, dev) in visible_devices {
            let mut card_h = if dev.sub_batteries.is_some() {
                78.0
            } else {
                56.0
            };
            if dev
                .detail_note
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
            {
                card_h += 18.0;
            }
            device_cards.push(DeviceCardLayout {
                index,
                rect: D2D_RECT_F {
                    left: PAD_X,
                    top: y,
                    right: PAD_X + card_w,
                    bottom: y + card_h,
                },
            });
            y += card_h + 8.0;
        }
    }

    OverviewLayout {
        card_w,
        hw_spec,
        hw_card_rect,
        wireless_header_y,
        refresh_rect,
        empty_rect,
        device_cards,
        content_bottom: y + 4.0,
    }
}

pub(crate) fn compute_settings_layout(
    snapshot: &MonitorSnapshot,
    sub_tab: SettingsSubTab,
    logical_w: f32,
    scroll_y: f32,
) -> SettingsLayout {
    let mut y = HEADER_HEIGHT + 8.0 - scroll_y;
    let card_w = (logical_w - PAD_X * 2.0).max(1.0);
    let inner_x = PAD_X + CARD_PAD;
    let inner_r = PAD_X + card_w - CARD_PAD;
    let inner_w = (inner_r - inner_x).max(1.0);

    let sub_nav_rect = D2D_RECT_F {
        left: PAD_X,
        top: y,
        right: PAD_X + card_w,
        bottom: y + 32.0,
    };
    y += 32.0 + 10.0;

    let (body, content_bottom) = match sub_tab {
        SettingsSubTab::Metrics => {
            let items = &snapshot.config.hardware_items;

            // Card 1: 2-row WYSIWYG taskbar grid + spatial action bar
            let grid_card_top = y;
            let mut cy = grid_card_top + 10.0;
            let reset_btn_rect = D2D_RECT_F {
                left: inner_r - 64.0,
                top: cy + 2.0,
                right: inner_r,
                bottom: cy + 24.0,
            };
            let grid_title_rect = D2D_RECT_F {
                left: inner_x,
                top: cy,
                right: (reset_btn_rect.left - 8.0).max(inner_x + 1.0),
                bottom: cy + 18.0,
            };
            let grid_subtitle_rect = D2D_RECT_F {
                left: inner_x,
                top: cy + 18.0,
                right: (reset_btn_rect.left - 8.0).max(inner_x + 1.0),
                bottom: cy + 34.0,
            };
            cy += 38.0;

            let box_pad = 6.0;
            let cell_h = 44.0;
            let cell_gap = 6.0;
            let strip_gap = 8.0;
            let grid_inner_w = (inner_w - box_pad * 2.0).max(1.0);
            let strip_h = cell_h * 2.0 + cell_gap;
            let mut grid_cells = Vec::with_capacity(items.len());
            let mut empty_slot_rect = None;

            let grid_box_h = if items.is_empty() {
                box_pad * 2.0 + strip_h
            } else {
                let items_per_strip = 8usize;
                let num_strips = items.len().div_ceil(items_per_strip);
                for s in 0..num_strips {
                    let strip_top = cy + box_pad + (s as f32) * (strip_h + strip_gap);
                    let strip_start = s * items_per_strip;
                    let strip_end = ((s + 1) * items_per_strip).min(items.len());
                    let strip_len = strip_end - strip_start;
                    let cols = if num_strips > 1 {
                        4
                    } else {
                        strip_len.div_ceil(2).clamp(2, 4)
                    };
                    let col_w =
                        ((grid_inner_w - ((cols - 1) as f32) * cell_gap) / (cols as f32)).max(1.0);

                    for local_idx in 0..strip_len {
                        let col = local_idx / 2;
                        let row = local_idx % 2;
                        let cx = inner_x + box_pad + (col as f32) * (col_w + cell_gap);
                        let cell_y = strip_top + (row as f32) * (cell_h + cell_gap);
                        grid_cells.push(MetricSlotRect {
                            index: strip_start + local_idx,
                            rect: D2D_RECT_F {
                                left: cx,
                                top: cell_y,
                                right: cx + col_w,
                                bottom: cell_y + cell_h,
                            },
                        });
                    }

                    if items.len() % 2 == 1 && s == num_strips - 1 {
                        let col = strip_len / 2;
                        let cx = inner_x + box_pad + (col as f32) * (col_w + cell_gap);
                        let cell_y = strip_top + cell_h + cell_gap;
                        empty_slot_rect = Some(D2D_RECT_F {
                            left: cx,
                            top: cell_y,
                            right: cx + col_w,
                            bottom: cell_y + cell_h,
                        });
                    }
                }
                box_pad * 2.0
                    + (num_strips as f32) * strip_h
                    + ((num_strips.saturating_sub(1)) as f32) * strip_gap
            };

            let grid_box_rect = D2D_RECT_F {
                left: inner_x,
                top: cy,
                right: inner_r,
                bottom: cy + grid_box_h,
            };
            cy = grid_box_rect.bottom + 8.0;

            let act_w = 58.0;
            let act_gap = 5.0;
            let act_left_rect = D2D_RECT_F {
                left: inner_x,
                top: cy,
                right: inner_x + act_w,
                bottom: cy + 24.0,
            };
            let act_swap_row_rect = D2D_RECT_F {
                left: act_left_rect.right + act_gap,
                top: cy,
                right: act_left_rect.right + act_gap + act_w,
                bottom: cy + 24.0,
            };
            let act_right_rect = D2D_RECT_F {
                left: act_swap_row_rect.right + act_gap,
                top: cy,
                right: act_swap_row_rect.right + act_gap + act_w,
                bottom: cy + 24.0,
            };
            let act_remove_rect = D2D_RECT_F {
                left: inner_r - act_w,
                top: cy,
                right: inner_r,
                bottom: cy + 24.0,
            };
            cy += 24.0 + 10.0;

            let grid_card_rect = D2D_RECT_F {
                left: PAD_X,
                top: grid_card_top,
                right: PAD_X + card_w,
                bottom: cy,
            };
            y = grid_card_rect.bottom + 10.0;

            // Card 2: Categorized chip wall
            let chips_card_top = y;
            let mut chy = chips_card_top + 10.0;
            let chips_hint_rect = D2D_RECT_F {
                left: inner_r - 96.0,
                top: chy + 1.0,
                right: inner_r,
                bottom: chy + 18.0,
            };
            let chips_title_rect = D2D_RECT_F {
                left: inner_x,
                top: chy,
                right: (chips_hint_rect.left - 8.0).max(inner_x + 1.0),
                bottom: chy + 18.0,
            };
            chy += 24.0;

            let chip_h = 26.0;
            let chip_gap = 6.0;
            let chip_w = ((inner_w - chip_gap * 2.0) / 3.0).max(1.0);
            let mut groups = Vec::with_capacity(METRIC_GROUPS.len());
            for (gi, grp) in METRIC_GROUPS.iter().enumerate() {
                if gi > 0 {
                    chy += 8.0;
                }
                let title_rect = D2D_RECT_F {
                    left: inner_x,
                    top: chy,
                    right: inner_r,
                    bottom: chy + 16.0,
                };
                chy += 20.0;

                let mut chips = Vec::with_capacity(grp.metrics.len());
                let rows = grp.metrics.len().div_ceil(3);
                for (mi, &metric) in grp.metrics.iter().enumerate() {
                    let row = mi / 3;
                    let col = mi % 3;
                    let cx = inner_x + (col as f32) * (chip_w + chip_gap);
                    let chip_y = chy + (row as f32) * (chip_h + chip_gap);
                    chips.push((
                        metric,
                        D2D_RECT_F {
                            left: cx,
                            top: chip_y,
                            right: cx + chip_w,
                            bottom: chip_y + chip_h,
                        },
                    ));
                }
                chy += (rows as f32) * chip_h + ((rows.saturating_sub(1)) as f32) * chip_gap;
                groups.push(MetricGroupLayout {
                    title: grp.title,
                    title_rect,
                    chips,
                });
            }
            chy += 10.0;

            let chips_card_rect = D2D_RECT_F {
                left: PAD_X,
                top: chips_card_top,
                right: PAD_X + card_w,
                bottom: chy,
            };
            let bottom = chips_card_rect.bottom + 10.0;
            (
                SettingsBodyLayout::Metrics(SettingsMetricsLayout {
                    grid_card_rect,
                    grid_title_rect,
                    grid_subtitle_rect,
                    reset_btn_rect,
                    grid_box_rect,
                    grid_cells,
                    empty_slot_rect,
                    act_left_rect,
                    act_swap_row_rect,
                    act_right_rect,
                    act_remove_rect,
                    chips_card_rect,
                    chips_title_rect,
                    chips_hint_rect,
                    groups,
                }),
                bottom,
            )
        }
        SettingsSubTab::Layout => {
            // Card 1: Display mode
            let mode_card_top = y;
            let mut ly = mode_card_top + 10.0;
            let mode_title_rect = D2D_RECT_F {
                left: inner_x,
                top: ly,
                right: inner_r,
                bottom: ly + 18.0,
            };
            let mode_subtitle_rect = D2D_RECT_F {
                left: inner_x,
                top: ly + 18.0,
                right: inner_r,
                bottom: ly + 34.0,
            };
            ly += 40.0;
            let mode_seg_rect = D2D_RECT_F {
                left: inner_x,
                top: ly,
                right: inner_r,
                bottom: ly + 32.0,
            };
            ly += 32.0;

            let (order_divider_y, order_label_rect, order_dev_first_rect, order_hw_first_rect) =
                if snapshot.config.layout_mode == LayoutMode::Combined {
                    let div_y = ly + 8.0;
                    ly += 15.0;
                    let lbl = D2D_RECT_F {
                        left: inner_x,
                        top: ly,
                        right: inner_r,
                        bottom: ly + 18.0,
                    };
                    ly += 22.0;
                    let btn_w = (inner_w - 6.0) * 0.5;
                    let dev_btn = D2D_RECT_F {
                        left: inner_x,
                        top: ly,
                        right: inner_x + btn_w,
                        bottom: ly + 26.0,
                    };
                    let hw_btn = D2D_RECT_F {
                        left: inner_x + btn_w + 6.0,
                        top: ly,
                        right: inner_r,
                        bottom: ly + 26.0,
                    };
                    ly += 26.0;
                    (Some(div_y), Some(lbl), Some(dev_btn), Some(hw_btn))
                } else {
                    (None, None, None, None)
                };
            ly += 10.0;

            let mode_card_rect = D2D_RECT_F {
                left: PAD_X,
                top: mode_card_top,
                right: PAD_X + card_w,
                bottom: ly,
            };
            y = mode_card_rect.bottom + 10.0;

            // Card 2: Dock position & horizontal offset
            let dock_card_top = y;
            let mut dy = dock_card_top + 10.0;
            let dock_title_rect = D2D_RECT_F {
                left: inner_x,
                top: dy,
                right: inner_r,
                bottom: dy + 18.0,
            };
            let dock_subtitle_rect = D2D_RECT_F {
                left: inner_x,
                top: dy + 18.0,
                right: inner_r,
                bottom: dy + 34.0,
            };
            dy += 40.0;

            let active_zones: &[(&'static str, bool, bool)] = match snapshot.config.layout_mode {
                LayoutMode::Combined => &[("合并浮条", false, true)],
                LayoutMode::Split => &[("硬件区", false, false), ("无线设备区", true, false)],
                LayoutMode::HardwareOnly => &[("硬件区", false, false)],
                LayoutMode::DevicesOnly => &[("无线设备区", true, false)],
            };
            let mut zones = Vec::with_capacity(active_zones.len());
            for (zi, &(label, is_devices, is_combined)) in active_zones.iter().enumerate() {
                if zi > 0 {
                    dy += 8.0;
                }
                let box_top = dy;
                let box_h = 70.0;
                let box_rect = D2D_RECT_F {
                    left: inner_x,
                    top: box_top,
                    right: inner_r,
                    bottom: box_top + box_h,
                };
                let box_in_l = inner_x + 8.0;
                let box_in_r = inner_r - 8.0;
                let row1_y = box_top + 8.0;
                let right_anchor_rect = D2D_RECT_F {
                    left: box_in_r - 44.0,
                    top: row1_y,
                    right: box_in_r,
                    bottom: row1_y + 24.0,
                };
                let left_anchor_rect = D2D_RECT_F {
                    left: right_anchor_rect.left - 5.0 - 44.0,
                    top: row1_y,
                    right: right_anchor_rect.left - 5.0,
                    bottom: row1_y + 24.0,
                };
                let label_rect = D2D_RECT_F {
                    left: box_in_l,
                    top: row1_y,
                    right: (left_anchor_rect.left - 8.0).max(box_in_l + 1.0),
                    bottom: row1_y + 24.0,
                };

                let row2_y = row1_y + 24.0 + 6.0;
                let reset_rect = D2D_RECT_F {
                    left: box_in_r - 44.0,
                    top: row2_y,
                    right: box_in_r,
                    bottom: row2_y + 24.0,
                };
                let stepper_rect = D2D_RECT_F {
                    left: reset_rect.left - 6.0 - 102.0,
                    top: row2_y,
                    right: reset_rect.left - 6.0,
                    bottom: row2_y + 24.0,
                };
                let offset_label_rect = D2D_RECT_F {
                    left: box_in_l,
                    top: row2_y,
                    right: (stepper_rect.left - 8.0).max(box_in_l + 1.0),
                    bottom: row2_y + 24.0,
                };

                zones.push(ZoneBoxLayout {
                    label,
                    is_devices,
                    is_combined,
                    box_rect,
                    label_rect,
                    left_anchor_rect,
                    right_anchor_rect,
                    offset_label_rect,
                    stepper_rect,
                    reset_rect,
                });
                dy += box_h;
            }
            dy += 10.0;

            let dock_card_rect = D2D_RECT_F {
                left: PAD_X,
                top: dock_card_top,
                right: PAD_X + card_w,
                bottom: dy,
            };
            let bottom = dock_card_rect.bottom + 10.0;
            (
                SettingsBodyLayout::Layout(SettingsLayoutModeLayout {
                    mode_card_rect,
                    mode_title_rect,
                    mode_subtitle_rect,
                    mode_seg_rect,
                    order_divider_y,
                    order_label_rect,
                    order_dev_first_rect,
                    order_hw_first_rect,
                    dock_card_rect,
                    dock_title_rect,
                    dock_subtitle_rect,
                    zones,
                }),
                bottom,
            )
        }
        SettingsSubTab::General => {
            let make_row = |top: f32, ctrl_w: f32, ctrl_h: f32| -> GeneralRowLayout {
                let row_h = 34.0;
                let control_rect = D2D_RECT_F {
                    left: inner_r - ctrl_w,
                    top: top + (row_h - ctrl_h) * 0.5,
                    right: inner_r,
                    bottom: top + (row_h + ctrl_h) * 0.5,
                };
                GeneralRowLayout {
                    row_rect: D2D_RECT_F {
                        left: inner_x - 4.0,
                        top,
                        right: inner_r + 4.0,
                        bottom: top + row_h,
                    },
                    title_rect: D2D_RECT_F {
                        left: inner_x,
                        top: top + 1.0,
                        right: (control_rect.left - 10.0).max(inner_x + 1.0),
                        bottom: top + 18.0,
                    },
                    subtitle_rect: D2D_RECT_F {
                        left: inner_x,
                        top: top + 18.0,
                        right: (control_rect.left - 10.0).max(inner_x + 1.0),
                        bottom: top + 33.0,
                    },
                    control_rect,
                }
            };

            // Card 1: Collection & Display
            let collect_top = y;
            let mut cy = collect_top + 10.0;
            let collect_title_rect = D2D_RECT_F {
                left: inner_x,
                top: cy,
                right: inner_r,
                bottom: cy + 20.0,
            };
            cy += 26.0;
            let poll_row = make_row(cy, 104.0, 24.0);
            cy += 34.0;
            let collect_div1_y = cy + 6.0;
            cy += 13.0;
            let ext_sensors_row = make_row(cy, 38.0, 20.0);
            cy += 34.0;
            let collect_div2_y = cy + 6.0;
            cy += 13.0;
            let hide_unavail_row = make_row(cy, 38.0, 20.0);
            cy += 34.0 + 10.0;
            let collect_card_rect = D2D_RECT_F {
                left: PAD_X,
                top: collect_top,
                right: PAD_X + card_w,
                bottom: cy,
            };
            y = collect_card_rect.bottom + 10.0;

            // Card 2: Peripheral Alerts & System
            let device_top = y;
            let mut dy = device_top + 10.0;
            let device_title_rect = D2D_RECT_F {
                left: inner_x,
                top: dy,
                right: inner_r,
                bottom: dy + 20.0,
            };
            dy += 26.0;
            let low_bat_row = make_row(dy, 104.0, 24.0);
            dy += 34.0;
            let device_div1_y = dy + 6.0;
            dy += 13.0;
            let hide_unknown_row = make_row(dy, 38.0, 20.0);
            dy += 34.0;
            let device_div2_y = dy + 6.0;
            dy += 13.0;
            let autostart_row = make_row(dy, 38.0, 20.0);
            dy += 34.0 + 10.0;
            let device_card_rect = D2D_RECT_F {
                left: PAD_X,
                top: device_top,
                right: PAD_X + card_w,
                bottom: dy,
            };
            let bottom = device_card_rect.bottom + 10.0;
            (
                SettingsBodyLayout::General(SettingsGeneralLayout {
                    collect_card_rect,
                    collect_title_rect,
                    poll_row,
                    collect_div1_y,
                    ext_sensors_row,
                    collect_div2_y,
                    hide_unavail_row,
                    device_card_rect,
                    device_title_rect,
                    low_bat_row,
                    device_div1_y,
                    hide_unknown_row,
                    device_div2_y,
                    autostart_row,
                }),
                bottom,
            )
        }
    };

    SettingsLayout {
        sub_nav_rect,
        body,
        content_bottom,
    }
}

pub(crate) fn compute_about_layout(
    status: &UpdateStatus,
    self_update_writable: bool,
    logical_w: f32,
    scroll_y: f32,
) -> AboutLayout {
    let mut y = HEADER_HEIGHT + 8.0 - scroll_y;
    let card_w = (logical_w - PAD_X * 2.0).max(1.0);
    let inner_x = PAD_X + CARD_PAD;
    let inner_r = PAD_X + card_w - CARD_PAD;

    // Card 1: App information
    let info_top = y;
    let info_header_y = info_top + 10.0;
    let version_badge_rect = D2D_RECT_F {
        left: inner_r - 64.0,
        top: info_header_y,
        right: inner_r,
        bottom: info_header_y + 20.0,
    };
    let info_subtitle_y = info_header_y + 22.0;
    let info_divider_y = info_subtitle_y + 20.0;
    let info_kv_start_y = info_divider_y + 5.0;
    let info_btn_top = info_kv_start_y + 4.0 * 20.0 + 4.0;
    let btn_w = (inner_r - inner_x - 8.0) * 0.5;
    let repo_btn_rect = D2D_RECT_F {
        left: inner_x,
        top: info_btn_top,
        right: inner_x + btn_w,
        bottom: info_btn_top + 26.0,
    };
    let releases_btn_rect = D2D_RECT_F {
        left: inner_x + btn_w + 8.0,
        top: info_btn_top,
        right: inner_r,
        bottom: info_btn_top + 26.0,
    };
    let info_card_rect = D2D_RECT_F {
        left: PAD_X,
        top: info_top,
        right: PAD_X + card_w,
        bottom: info_btn_top + 26.0 + 10.0,
    };
    y = info_card_rect.bottom + 8.0;

    // Card 2: Software update
    let update_top = y;
    let update_header_y = update_top + 10.0;
    let check_btn_rect = D2D_RECT_F {
        left: inner_r - 78.0,
        top: update_header_y,
        right: inner_r,
        bottom: update_header_y + 22.0,
    };
    let status_top = update_header_y + 26.0;
    let mut update_meta_rect = None;
    let mut update_prompt_rect = None;
    let mut progress_bar_rect = None;
    let mut auto_update_btn_rect = None;
    let mut manual_download_btn_rect = None;
    let mut download_btn_rect = None;

    let (update_status_rect, update_body_bottom) = match status {
        UpdateStatus::Error(_) => (
            D2D_RECT_F {
                left: inner_x,
                top: status_top,
                right: inner_r,
                bottom: status_top + 36.0,
            },
            status_top + 36.0,
        ),
        UpdateStatus::UpdateAvailable {
            published_at,
            download_url,
            asset_size,
            ..
        } => {
            let status_rect = D2D_RECT_F {
                left: inner_x,
                top: status_top,
                right: inner_r,
                bottom: status_top + 20.0,
            };
            let has_meta = format_update_meta(published_at.as_deref(), *asset_size).is_some();
            let prompt_top = if has_meta {
                let meta_top = status_top + 20.0;
                update_meta_rect = Some(D2D_RECT_F {
                    left: inner_x,
                    top: meta_top,
                    right: inner_r,
                    bottom: meta_top + 18.0,
                });
                meta_top + 20.0
            } else {
                status_top + 22.0
            };
            update_prompt_rect = Some(D2D_RECT_F {
                left: inner_x,
                top: prompt_top,
                right: inner_r,
                bottom: prompt_top + 18.0,
            });
            let btn_top = prompt_top + 22.0;
            if download_url.is_some() && self_update_writable {
                let gap = 8.0;
                let auto_w = ((inner_r - inner_x - gap) * 0.6).round();
                auto_update_btn_rect = Some(D2D_RECT_F {
                    left: inner_x,
                    top: btn_top,
                    right: inner_x + auto_w,
                    bottom: btn_top + 26.0,
                });
                manual_download_btn_rect = Some(D2D_RECT_F {
                    left: inner_x + auto_w + gap,
                    top: btn_top,
                    right: inner_r,
                    bottom: btn_top + 26.0,
                });
            } else {
                download_btn_rect = Some(D2D_RECT_F {
                    left: inner_x,
                    top: btn_top,
                    right: inner_r,
                    bottom: btn_top + 26.0,
                });
            }
            (status_rect, btn_top + 26.0)
        }
        UpdateStatus::Downloading { .. }
        | UpdateStatus::Verifying { .. }
        | UpdateStatus::Applying { .. } => {
            let status_rect = D2D_RECT_F {
                left: inner_x,
                top: status_top,
                right: inner_r,
                bottom: status_top + 20.0,
            };
            progress_bar_rect = Some(D2D_RECT_F {
                left: inner_x,
                top: status_top + 25.0,
                right: inner_r,
                bottom: status_top + 32.0,
            });
            (status_rect, status_top + 34.0)
        }
        UpdateStatus::Idle
        | UpdateStatus::Checking
        | UpdateStatus::UpToDate { .. }
        | UpdateStatus::NoReleaseYet { .. } => (
            D2D_RECT_F {
                left: inner_x,
                top: status_top,
                right: inner_r,
                bottom: status_top + 20.0,
            },
            status_top + 20.0,
        ),
    };
    let update_card_rect = D2D_RECT_F {
        left: PAD_X,
        top: update_top,
        right: PAD_X + card_w,
        bottom: update_body_bottom + 10.0,
    };
    y = update_card_rect.bottom + 8.0;

    // Card 3: Quick maintenance
    let maint_top = y;
    let maint_h = 42.0;
    let maint_card_rect = D2D_RECT_F {
        left: PAD_X,
        top: maint_top,
        right: PAD_X + card_w,
        bottom: maint_top + maint_h,
    };
    let maint_row_rect = D2D_RECT_F {
        left: inner_x,
        top: maint_top + 8.0,
        right: inner_r - 102.0,
        bottom: maint_top + 34.0,
    };
    let open_config_btn_rect = D2D_RECT_F {
        left: inner_r - 96.0,
        top: maint_top + 8.0,
        right: inner_r,
        bottom: maint_top + 34.0,
    };
    y = maint_card_rect.bottom + 10.0;

    AboutLayout {
        card_w,
        info_card_rect,
        info_header_y,
        version_badge_rect,
        info_subtitle_y,
        info_divider_y,
        info_kv_start_y,
        repo_btn_rect,
        releases_btn_rect,
        update_card_rect,
        update_header_y,
        check_btn_rect,
        update_status_rect,
        update_meta_rect,
        update_prompt_rect,
        progress_bar_rect,
        auto_update_btn_rect,
        manual_download_btn_rect,
        download_btn_rect,
        maint_card_rect,
        maint_row_rect,
        open_config_btn_rect,
        content_bottom: y,
    }
}

/// Dry-runs the active tab's layout at `scroll_y = 0` to determine the exact
/// unclipped logical height (`content_bottom`).
pub fn measure_content_height(
    tab: PopupTab,
    sub_tab: SettingsSubTab,
    snapshot: &MonitorSnapshot,
    update_status: &UpdateStatus,
    self_update_writable: bool,
) -> f32 {
    match tab {
        PopupTab::Overview => {
            compute_overview_layout(snapshot, POPUP_WIDTH_LOGICAL, 0.0).content_bottom
        }
        PopupTab::Settings => {
            compute_settings_layout(snapshot, sub_tab, POPUP_WIDTH_LOGICAL, 0.0).content_bottom
        }
        PopupTab::About => {
            compute_about_layout(
                update_status,
                self_update_writable,
                POPUP_WIDTH_LOGICAL,
                0.0,
            )
            .content_bottom
        }
    }
}

pub fn compute_popup_rect(
    work_area: RECT,
    taskbar_rect: Option<RECT>,
    click_x: i32,
    width_px: i32,
    height_px: i32,
    margin_px: i32,
) -> RECT {
    let min_x = work_area.left + margin_px;
    let max_x = (work_area.right - width_px - margin_px).max(min_x);
    let x = (click_x - width_px / 2).clamp(min_x, max_x);

    let min_y = work_area.top + margin_px;
    let max_y = (work_area.bottom - height_px - margin_px).max(min_y);

    let top_taskbar =
        taskbar_rect.is_some_and(|tb| tb.bottom <= work_area.top + 8 && tb.top < work_area.top);
    let y = if top_taskbar { min_y } else { max_y };

    RECT {
        left: x,
        top: y,
        right: x + width_px,
        bottom: y + height_px,
    }
}

pub(crate) fn hit_test_boxes(
    hit_boxes: &[HitBox],
    dpi: u32,
    px_x: i32,
    px_y: i32,
) -> Option<usize> {
    let scale = dpi.max(96) as f32 / 96.0;
    let lx = px_x as f32 / scale;
    let ly = px_y as f32 / scale;
    hit_boxes.iter().position(|hb| {
        lx >= hb.rect.left && lx <= hb.rect.right && ly >= hb.rect.top && ly <= hb.rect.bottom
    })
}

pub(crate) fn hit_test_pt(hit_boxes: &[HitBox], dpi: u32, pt: POINT) -> Option<usize> {
    hit_test_boxes(hit_boxes, dpi, pt.x, pt.y)
}

pub(crate) fn hit_test_metric_slot(
    slots: &[MetricSlotRect],
    dpi: u32,
    px_x: i32,
    px_y: i32,
) -> Option<usize> {
    let scale = dpi.max(96) as f32 / 96.0;
    let lx = px_x as f32 / scale;
    let ly = px_y as f32 / scale;
    slots
        .iter()
        .find(|s| {
            lx >= s.rect.left && lx <= s.rect.right && ly >= s.rect.top && ly <= s.rect.bottom
        })
        .map(|s| s.index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::state::{DeviceKind, SubBatteries, Transport, WirelessDevice};
    use std::time::Instant;

    #[test]
    fn popup_rect_clamps_inside_work_area() {
        let wa = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1032,
        };
        let tb = Some(RECT {
            left: 0,
            top: 1032,
            right: 1920,
            bottom: 1080,
        });

        let right_edge = compute_popup_rect(wa, tb, 1915, 368, 460, 8);
        assert_eq!(right_edge.right, 1920 - 8);
        assert_eq!(right_edge.bottom, 1032 - 8);

        let left_edge = compute_popup_rect(wa, tb, 10, 368, 460, 8);
        assert_eq!(left_edge.left, 8);
        assert_eq!(left_edge.bottom, 1032 - 8);

        let top_tb = Some(RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 48,
        });
        let wa_top = RECT {
            left: 0,
            top: 48,
            right: 1920,
            bottom: 1080,
        };
        let top_pos = compute_popup_rect(wa_top, top_tb, 960, 368, 460, 8);
        assert_eq!(top_pos.top, 48 + 8);
    }

    #[test]
    fn dry_run_layout_height_is_exact_across_tabs_and_devices() {
        let mut snap = MonitorSnapshot::new(AppConfig::default());
        let idle = UpdateStatus::Idle;
        let empty_h = measure_content_height(
            PopupTab::Overview,
            SettingsSubTab::Metrics,
            &snap,
            &idle,
            false,
        );
        // Header(42) + pad(8) + HwCard(158) + gap(10) + WirelessHdr(28) + EmptyCard(56) + bottom(4) = 306
        assert!((empty_h - 306.0).abs() < 0.01);

        // Partial GPU (usage + VRAM only -> 1 GPU row = +19)
        snap.hw.gpu_usage_pct = Some(30.0);
        snap.hw.gpu_vram_used_gb = Some(2.5);
        snap.hw.gpu_provider = "DXGI/PDH".to_string();
        let partial_gpu_h = measure_content_height(
            PopupTab::Overview,
            SettingsSubTab::Metrics,
            &snap,
            &idle,
            false,
        );
        assert!((partial_gpu_h - (empty_h + 19.0)).abs() < 0.01);

        // Full HW (CPU ext + disk + battery + 3 GPU rows)
        snap.hw.cpu_temp_c = Some(62.0);
        snap.hw.disk_read_bps = Some(1024.0);
        snap.hw.sys_battery_pct = Some(88);
        snap.hw.gpu_temp_c = Some(55.0);
        snap.hw.gpu_hotspot_c = Some(68.0);
        snap.hw.gpu_power_w = Some(120.0);
        snap.hw.gpu_fan_pct = Some(40);
        let full_hw_h = measure_content_height(
            PopupTab::Overview,
            SettingsSubTab::Metrics,
            &snap,
            &idle,
            false,
        );
        // +20 (cpu ext) +20 (disk) +20 (battery) +38 (2 more GPU rows) = +98 over partial_gpu_h
        assert!((full_hw_h - (partial_gpu_h + 98.0)).abs() < 0.01);

        // Unknown-battery device is hidden by default -> overview remains empty & same height.
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
            detail_note: Some("未连接 2.4G 电量通道".into()),
        });
        assert!(overview_visible_devices(&snap).is_empty());
        assert!(
            (measure_content_height(
                PopupTab::Overview,
                SettingsSubTab::Metrics,
                &snap,
                &idle,
                false
            ) - full_hw_h)
                .abs()
                < 0.01
        );

        // Disabling hide_unknown_battery_devices reveals it (with +18 detail_note height).
        snap.config.hide_unknown_battery_devices = false;
        assert_eq!(overview_visible_devices(&snap).len(), 1);
        let skn_visible_h = measure_content_height(
            PopupTab::Overview,
            SettingsSubTab::Metrics,
            &snap,
            &idle,
            false,
        );
        assert!((skn_visible_h - (full_hw_h - 56.0 + 56.0 + 18.0 + 8.0)).abs() < 0.01);

        snap.config.hide_unknown_battery_devices = true;
        snap.devices.push(WirelessDevice {
            id: "m1".into(),
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
        assert_eq!(overview_visible_devices(&snap).len(), 1);
        let one_dev_h = measure_content_height(
            PopupTab::Overview,
            SettingsSubTab::Metrics,
            &snap,
            &idle,
            false,
        );
        assert!((one_dev_h - (full_hw_h - 56.0 + 64.0)).abs() < 0.01);

        snap.devices.push(WirelessDevice {
            id: "pods".into(),
            name: "AirPods Pro".into(),
            kind: DeviceKind::Earbuds,
            transport: Transport::Bluetooth,
            battery_pct: Some(90),
            charging: false,
            sub_batteries: Some(SubBatteries {
                left_pct: Some(90),
                right_pct: Some(85),
                case_pct: Some(70),
                ..Default::default()
            }),
            online: true,
            last_seen: Instant::now(),
            detail_note: Some("BLE 广播".into()),
        });
        let two_dev_h = measure_content_height(
            PopupTab::Overview,
            SettingsSubTab::Metrics,
            &snap,
            &idle,
            false,
        );
        assert!((two_dev_h - (one_dev_h + 78.0 + 18.0 + 8.0)).abs() < 0.01);

        // Settings v2 sub-tabs sync with compute_settings_layout.
        for sub_tab in [
            SettingsSubTab::Metrics,
            SettingsSubTab::Layout,
            SettingsSubTab::General,
        ] {
            let l = compute_settings_layout(&snap, sub_tab, POPUP_WIDTH_LOGICAL, 0.0);
            let measured = measure_content_height(PopupTab::Settings, sub_tab, &snap, &idle, false);
            assert!((l.content_bottom - measured).abs() < 0.01);
        }

        // Layout sub-tab adapts cleanly across layout modes.
        snap.config.layout_mode = LayoutMode::Combined;
        let layout_combined_h = measure_content_height(
            PopupTab::Settings,
            SettingsSubTab::Layout,
            &snap,
            &idle,
            false,
        );
        snap.config.layout_mode = LayoutMode::HardwareOnly;
        let layout_hw_only_h = measure_content_height(
            PopupTab::Settings,
            SettingsSubTab::Layout,
            &snap,
            &idle,
            false,
        );
        snap.config.layout_mode = LayoutMode::DevicesOnly;
        let layout_dev_only_h = measure_content_height(
            PopupTab::Settings,
            SettingsSubTab::Layout,
            &snap,
            &idle,
            false,
        );
        assert!((layout_hw_only_h - layout_dev_only_h).abs() < 0.01);
        // Combined adds divider+label+buttons (+63px) over single-zone without order block.
        assert!((layout_combined_h - (layout_hw_only_h + 63.0)).abs() < 0.01);

        snap.config.layout_mode = LayoutMode::Split;
        let layout_split_h = measure_content_height(
            PopupTab::Settings,
            SettingsSubTab::Layout,
            &snap,
            &idle,
            false,
        );
        // Split has no order block (-63px vs combined) and 2 zone boxes (+78px vs 1 box).
        assert!((layout_split_h - (layout_hw_only_h + 78.0)).abs() < 0.01);
    }

    #[test]
    fn settings_metric_groups_cover_all_eighteen_metrics_without_duplicates() {
        let mut seen = Vec::new();
        for grp in METRIC_GROUPS {
            for &m in grp.metrics {
                assert!(
                    !seen.contains(&m),
                    "duplicate metric {:?} in group {}",
                    m,
                    grp.title
                );
                seen.push(m);
            }
        }
        assert_eq!(seen.len(), HwMetric::ALL.len());
        assert_eq!(seen.len(), 18);
        for &m in HwMetric::ALL {
            assert!(
                seen.contains(&m),
                "missing metric {:?} from METRIC_GROUPS",
                m
            );
        }
    }

    #[test]
    fn settings_metrics_grid_layout_handles_empty_odd_and_multi_strip() {
        let mut snap = MonitorSnapshot::new(AppConfig::default());

        // Empty items -> 0 cells, no empty slot, 2-row height preserved.
        snap.config.hardware_items.clear();
        let empty_l =
            compute_settings_layout(&snap, SettingsSubTab::Metrics, POPUP_WIDTH_LOGICAL, 0.0);
        let SettingsBodyLayout::Metrics(m_empty) = empty_l.body else {
            panic!("expected Metrics layout");
        };
        assert!(m_empty.grid_cells.is_empty());
        assert!(m_empty.empty_slot_rect.is_none());

        // 5 items (odd) -> 5 cells, 1 empty slot below index 4 (col 2, row 1).
        snap.config.hardware_items = vec![
            HwMetric::NetUp,
            HwMetric::NetDown,
            HwMetric::CpuUsage,
            HwMetric::MemUsage,
            HwMetric::CpuFreq,
        ];
        let odd_l =
            compute_settings_layout(&snap, SettingsSubTab::Metrics, POPUP_WIDTH_LOGICAL, 0.0);
        let SettingsBodyLayout::Metrics(m_odd) = odd_l.body else {
            panic!("expected Metrics layout");
        };
        assert_eq!(m_odd.grid_cells.len(), 5);
        let empty_slot = m_odd.empty_slot_rect.expect("odd count needs empty slot");
        assert!((empty_slot.left - m_odd.grid_cells[4].rect.left).abs() < 0.01);
        assert!(empty_slot.top > m_odd.grid_cells[4].rect.bottom);
        // Column-major order: 0 top, 1 bottom, 2 top, 3 bottom, 4 top.
        assert!((m_odd.grid_cells[0].rect.left - m_odd.grid_cells[1].rect.left).abs() < 0.01);
        assert!(m_odd.grid_cells[0].rect.top < m_odd.grid_cells[1].rect.top);
        assert!(m_odd.grid_cells[2].rect.left > m_odd.grid_cells[0].rect.right);

        // 9 items (>8) -> wraps to 2 strips so all 9 items remain clickable and visible.
        snap.config.hardware_items = HwMetric::ALL.iter().take(9).copied().collect();
        let multi_l =
            compute_settings_layout(&snap, SettingsSubTab::Metrics, POPUP_WIDTH_LOGICAL, 0.0);
        let SettingsBodyLayout::Metrics(m_multi) = multi_l.body else {
            panic!("expected Metrics layout");
        };
        assert_eq!(m_multi.grid_cells.len(), 9);
        assert!(m_multi.empty_slot_rect.is_some());
        assert!(m_multi.grid_cells[8].rect.top > m_multi.grid_cells[1].rect.bottom);
        assert!(multi_l.content_bottom > odd_l.content_bottom);
    }

    #[test]
    fn settings_subtabs_layout_and_scroll_sync() {
        let snap = MonitorSnapshot::new(AppConfig::default());
        let idle = UpdateStatus::Idle;
        for sub_tab in [
            SettingsSubTab::Metrics,
            SettingsSubTab::Layout,
            SettingsSubTab::General,
        ] {
            let l0 = compute_settings_layout(&snap, sub_tab, POPUP_WIDTH_LOGICAL, 0.0);
            let l_scrolled = compute_settings_layout(&snap, sub_tab, POPUP_WIDTH_LOGICAL, 24.0);
            assert!((l0.sub_nav_rect.top - l_scrolled.sub_nav_rect.top - 24.0).abs() < 0.01);
            assert!((l0.content_bottom - l_scrolled.content_bottom - 24.0).abs() < 0.01);
            let h = measure_content_height(PopupTab::Settings, sub_tab, &snap, &idle, false);
            assert!((h - l0.content_bottom).abs() < 0.01);
        }
        // Layout & General fit cleanly within fixed 468 DIP popup height without clipping.
        assert!(
            measure_content_height(
                PopupTab::Settings,
                SettingsSubTab::Layout,
                &snap,
                &idle,
                false
            ) <= POPUP_FIXED_HEIGHT_LOGICAL
        );
        assert!(
            measure_content_height(
                PopupTab::Settings,
                SettingsSubTab::General,
                &snap,
                &idle,
                false
            ) <= POPUP_FIXED_HEIGHT_LOGICAL
        );
    }

    #[test]
    fn about_layout_height_adapts_across_all_update_statuses() {
        let snap = MonitorSnapshot::new(AppConfig::default());

        let idle_h = measure_content_height(
            PopupTab::About,
            SettingsSubTab::Metrics,
            &snap,
            &UpdateStatus::Idle,
            false,
        );
        let checking_h = measure_content_height(
            PopupTab::About,
            SettingsSubTab::Metrics,
            &snap,
            &UpdateStatus::Checking,
            false,
        );
        let up_to_date_h = measure_content_height(
            PopupTab::About,
            SettingsSubTab::Metrics,
            &snap,
            &UpdateStatus::UpToDate {
                current: "0.1.0".into(),
                latest: "0.1.0".into(),
            },
            false,
        );
        let no_release_h = measure_content_height(
            PopupTab::About,
            SettingsSubTab::Metrics,
            &snap,
            &UpdateStatus::NoReleaseYet {
                current: "0.1.0".into(),
            },
            false,
        );
        assert!((idle_h - checking_h).abs() < 0.01);
        assert!((idle_h - up_to_date_h).abs() < 0.01);
        assert!((idle_h - no_release_h).abs() < 0.01);

        let err_h = measure_content_height(
            PopupTab::About,
            SettingsSubTab::Metrics,
            &snap,
            &UpdateStatus::Error("WinHttpSendRequest failed".into()),
            false,
        );
        // Error adds +16px for 2-line wrapped message over single-line states.
        assert!((err_h - (idle_h + 16.0)).abs() < 0.01);

        let avail_no_meta = UpdateStatus::UpdateAvailable {
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            release_url: "https://github.com/AmlitaEnan/AirMonitor/releases/tag/v0.2.0".into(),
            published_at: None,
            download_url: None,
            asset_name: None,
            asset_size: None,
            sha256_url: None,
        };
        let avail_no_meta_h = measure_content_height(
            PopupTab::About,
            SettingsSubTab::Metrics,
            &snap,
            &avail_no_meta,
            false,
        );
        // Status (20) + gap (2) + Prompt (18) + gap (4) + Button (26) = 70 => +50 over idle_h.
        assert!((avail_no_meta_h - (idle_h + 50.0)).abs() < 0.01);

        let avail_with_meta = UpdateStatus::UpdateAvailable {
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            release_url: "https://github.com/AmlitaEnan/AirMonitor/releases/tag/v0.2.0".into(),
            published_at: Some("2026-11-20T12:00:00Z".into()),
            download_url: Some(
                "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/airmonitor.exe"
                    .into(),
            ),
            asset_name: Some("airmonitor.exe".into()),
            asset_size: Some(1_153_433),
            sha256_url: None,
        };
        let avail_auto_h = measure_content_height(
            PopupTab::About,
            SettingsSubTab::Metrics,
            &snap,
            &avail_with_meta,
            true,
        );
        let avail_manual_h = measure_content_height(
            PopupTab::About,
            SettingsSubTab::Metrics,
            &snap,
            &avail_with_meta,
            false,
        );
        // Meta row adds +18px over avail_no_meta_h.
        assert!((avail_auto_h - (avail_no_meta_h + 18.0)).abs() < 0.01);
        assert!((avail_manual_h - avail_auto_h).abs() < 0.01);

        // Writable dir + download_url -> dual side-by-side buttons.
        let layout_auto = compute_about_layout(&avail_with_meta, true, POPUP_WIDTH_LOGICAL, 0.0);
        assert!(layout_auto.update_meta_rect.is_some());
        assert!(layout_auto.update_prompt_rect.is_some());
        let auto_btn = layout_auto.auto_update_btn_rect.unwrap();
        let manual_btn = layout_auto.manual_download_btn_rect.unwrap();
        assert!(auto_btn.right < manual_btn.left);
        assert!(layout_auto.download_btn_rect.is_none());
        assert!(layout_auto.info_card_rect.bottom < layout_auto.update_card_rect.top);
        assert!(layout_auto.update_card_rect.bottom < layout_auto.maint_card_rect.top);
        assert!(layout_auto.maint_card_rect.bottom < layout_auto.content_bottom);

        // Read-only dir or missing download_url -> single full-width fallback button.
        let layout_readonly =
            compute_about_layout(&avail_with_meta, false, POPUP_WIDTH_LOGICAL, 0.0);
        assert!(layout_readonly.auto_update_btn_rect.is_none());
        assert!(layout_readonly.manual_download_btn_rect.is_none());
        assert!(layout_readonly.download_btn_rect.is_some());

        // Downloading / Verifying / Applying add +14px for progress bar over single-line states.
        for progress_status in [
            UpdateStatus::Downloading {
                latest: "0.2.0".into(),
                downloaded_bytes: 524_288,
                total_bytes: Some(1_048_576),
            },
            UpdateStatus::Verifying {
                latest: "0.2.0".into(),
            },
            UpdateStatus::Applying {
                latest: "0.2.0".into(),
            },
        ] {
            let h = measure_content_height(
                PopupTab::About,
                SettingsSubTab::Metrics,
                &snap,
                &progress_status,
                true,
            );
            assert!((h - (idle_h + 14.0)).abs() < 0.01);
            let layout_prog =
                compute_about_layout(&progress_status, true, POPUP_WIDTH_LOGICAL, 0.0);
            assert!(layout_prog.progress_bar_rect.is_some());
        }
    }

    #[test]
    fn update_meta_and_byte_formatting_are_compact() {
        assert_eq!(format_bytes_compact(512), "512 B");
        assert_eq!(format_bytes_compact(520 * 1024), "520 KB");
        assert_eq!(format_bytes_compact(1_153_434), "1.1 MB");
        assert_eq!(
            format_update_meta(Some("2026-11-20T12:34:56Z"), Some(1_153_434)),
            Some("发布：2026-11-20 · 大小：1.1 MB".to_string())
        );
        assert_eq!(
            format_update_meta(Some("2026-11-20T12:34:56Z"), None),
            Some("发布日期：2026-11-20".to_string())
        );
        assert_eq!(
            format_update_meta(None, Some(1_153_434)),
            Some("安装包大小：1.1 MB".to_string())
        );
        assert_eq!(format_update_meta(None, Some(0)), None);
    }

    #[test]
    fn max_popup_height_is_half_monitor_height_with_360_floor() {
        let mon_1080p = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert!((compute_max_popup_height_dip(mon_1080p, 96) - 540.0).abs() < 0.01);
        // 150% DPI -> 1080 / 1.5 = 720 DIP -> half = 360 DIP
        assert!((compute_max_popup_height_dip(mon_1080p, 144) - 360.0).abs() < 0.01);
        // 200% DPI -> 1080 / 2.0 = 540 DIP -> half = 270 -> clamped to 360.0 floor
        assert!((compute_max_popup_height_dip(mon_1080p, 192) - 360.0).abs() < 0.01);

        let mon_4k = RECT {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 2160,
        };
        // 4K @ 150% DPI -> 1440 DIP -> half = 720 DIP
        assert!((compute_max_popup_height_dip(mon_4k, 144) - 720.0).abs() < 0.01);
    }

    #[test]
    fn popup_height_dip_is_fixed_and_independent_of_tab_and_content() {
        let mon_1080p = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let mon_4k = RECT {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 2160,
        };

        // 1080p @ 100% (max 540 DIP) -> 468 DIP
        assert!(
            (compute_popup_height_dip(mon_1080p, 96) - POPUP_FIXED_HEIGHT_LOGICAL).abs() < 0.01
        );
        // 1080p @ 150% (max 360 DIP) -> clamped to 360 DIP
        assert!((compute_popup_height_dip(mon_1080p, 144) - 360.0).abs() < 0.01);
        // 1080p @ 200% (max 360 DIP floor) -> clamped to 360 DIP
        assert!((compute_popup_height_dip(mon_1080p, 192) - 360.0).abs() < 0.01);
        // 4K @ 150% (max 720 DIP) -> 468 DIP
        assert!((compute_popup_height_dip(mon_4k, 144) - POPUP_FIXED_HEIGHT_LOGICAL).abs() < 0.01);

        // Confirm window height does not vary even though each tab's content height differs.
        let snap = MonitorSnapshot::new(AppConfig::default());
        let h_overview = measure_content_height(
            PopupTab::Overview,
            SettingsSubTab::Metrics,
            &snap,
            &UpdateStatus::Idle,
            false,
        );
        let h_settings = measure_content_height(
            PopupTab::Settings,
            SettingsSubTab::Metrics,
            &snap,
            &UpdateStatus::Idle,
            false,
        );
        let h_about = measure_content_height(
            PopupTab::About,
            SettingsSubTab::Metrics,
            &snap,
            &UpdateStatus::Idle,
            false,
        );
        assert!((h_overview - h_settings).abs() > 10.0);
        assert!((h_settings - h_about).abs() > 10.0);
        assert!(
            (compute_popup_height_dip(mon_1080p, 96) - POPUP_FIXED_HEIGHT_LOGICAL).abs() < 0.01
        );
    }
}
