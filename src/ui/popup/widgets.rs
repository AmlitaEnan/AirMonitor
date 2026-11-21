use windows::core::{w, HSTRING};
use windows::Win32::Graphics::Direct2D::Common::{D2D_POINT_2F, D2D_RECT_F};
use windows::Win32::Graphics::Direct2D::{
    ID2D1Factory, ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ELLIPSE, D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::DirectWrite::{
    IDWriteFactory, IDWriteTextFormat, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_MEDIUM, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_WRAP,
};

use crate::config::{AnchorEdge, CombinedOrder, HwMetric, LayoutMode};
use crate::state::{MonitorSnapshot, SubBatteries, WirelessDevice};
use crate::ui::popup::layout::{
    compute_about_layout, compute_overview_layout, compute_settings_layout, format_bytes_compact,
    format_overview_cpu_value, format_overview_mem_value, format_overview_net_value,
    format_overview_nic_value, format_update_meta, measure_content_height, ControlId, FocusItem,
    HitBox, MetricSlotRect, PopupAction, PopupTab, SettingsBodyLayout, SettingsSubTab, CARD_PAD,
    HEADER_HEIGHT, HW_DIVIDER_GAP, HW_GPU_ROW_H, HW_GPU_TITLE_H, HW_PRE_DIVIDER_GAP, HW_ROW_ACPI_H,
    HW_ROW_BAT_H, HW_ROW_CPU_EXT_H, HW_ROW_CPU_H, HW_ROW_DISK_H, HW_ROW_MEM_H, HW_ROW_NET_H,
    HW_ROW_NIC_H, PAD_X,
};
use crate::ui::renderer::{draw_device_icon_with_factory, format_device_battery, format_hw_metric};
use crate::ui::text::{create_text_format, draw_text_clipped};
use crate::ui::theme::{composite_over, rgba, PopupPalette, UiTheme};
use crate::updater::{UpdateStatus, CURRENT_VERSION, RELEASES_URL, REPO_URL};

pub(crate) struct PopupFonts {
    pub title_fmt: IDWriteTextFormat,
    pub section_fmt: IDWriteTextFormat,
    pub body_fmt: IDWriteTextFormat,
    pub body_bold_fmt: IDWriteTextFormat,
    pub mono_fmt: IDWriteTextFormat,
    pub mono_left_fmt: IDWriteTextFormat,
    pub mono_right_fmt: IDWriteTextFormat,
    pub mono_center_fmt: IDWriteTextFormat,
    pub small_fmt: IDWriteTextFormat,
    pub small_right_fmt: IDWriteTextFormat,
    pub small_center_fmt: IDWriteTextFormat,
    pub error_wrap_fmt: IDWriteTextFormat,
    pub center_fmt: IDWriteTextFormat,
    pub right_fmt: IDWriteTextFormat,
}

impl PopupFonts {
    pub unsafe fn new(dwrite_factory: &IDWriteFactory) -> windows::core::Result<Self> {
        let error_wrap_fmt = dwrite_factory.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT_MEDIUM,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            10.5,
            &HSTRING::new(),
        )?;
        error_wrap_fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        error_wrap_fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
        error_wrap_fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP)?;
        let err_sign = dwrite_factory.CreateEllipsisTrimmingSign(&error_wrap_fmt)?;
        let err_trim = DWRITE_TRIMMING {
            granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
            delimiter: 0,
            delimiterCount: 0,
        };
        error_wrap_fmt.SetTrimming(&err_trim, &err_sign)?;

        Ok(Self {
            title_fmt: create_text_format(
                dwrite_factory,
                w!("Segoe UI"),
                DWRITE_FONT_WEIGHT_BOLD,
                14.5,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                true,
            )?,
            section_fmt: create_text_format(
                dwrite_factory,
                w!("Segoe UI"),
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                12.5,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                true,
            )?,
            body_fmt: create_text_format(
                dwrite_factory,
                w!("Segoe UI"),
                DWRITE_FONT_WEIGHT_NORMAL,
                12.0,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                true,
            )?,
            body_bold_fmt: create_text_format(
                dwrite_factory,
                w!("Segoe UI"),
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                12.0,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                true,
            )?,
            mono_fmt: create_text_format(
                dwrite_factory,
                w!("Consolas"),
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                12.0,
                DWRITE_TEXT_ALIGNMENT_TRAILING,
                true,
            )?,
            mono_left_fmt: create_text_format(
                dwrite_factory,
                w!("Consolas"),
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                10.5,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                false,
            )?,
            mono_right_fmt: create_text_format(
                dwrite_factory,
                w!("Consolas"),
                DWRITE_FONT_WEIGHT_BOLD,
                10.5,
                DWRITE_TEXT_ALIGNMENT_TRAILING,
                false,
            )?,
            mono_center_fmt: create_text_format(
                dwrite_factory,
                w!("Consolas"),
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                11.5,
                DWRITE_TEXT_ALIGNMENT_CENTER,
                false,
            )?,
            small_fmt: create_text_format(
                dwrite_factory,
                w!("Segoe UI"),
                DWRITE_FONT_WEIGHT_MEDIUM,
                10.5,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                true,
            )?,
            small_right_fmt: create_text_format(
                dwrite_factory,
                w!("Segoe UI"),
                DWRITE_FONT_WEIGHT_MEDIUM,
                10.5,
                DWRITE_TEXT_ALIGNMENT_TRAILING,
                true,
            )?,
            small_center_fmt: create_text_format(
                dwrite_factory,
                w!("Segoe UI"),
                DWRITE_FONT_WEIGHT_MEDIUM,
                10.5,
                DWRITE_TEXT_ALIGNMENT_CENTER,
                true,
            )?,
            error_wrap_fmt,
            center_fmt: create_text_format(
                dwrite_factory,
                w!("Segoe UI"),
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                11.5,
                DWRITE_TEXT_ALIGNMENT_CENTER,
                false,
            )?,
            right_fmt: create_text_format(
                dwrite_factory,
                w!("Segoe UI"),
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                12.0,
                DWRITE_TEXT_ALIGNMENT_TRAILING,
                true,
            )?,
        })
    }
}

pub(crate) struct SceneInput<'a> {
    pub rt: &'a ID2D1RenderTarget,
    pub brush: &'a ID2D1SolidColorBrush,
    pub d2d_factory: &'a ID2D1Factory,
    pub dash_stroke: &'a ID2D1StrokeStyle,
    pub fonts: &'a PopupFonts,
    pub snapshot: &'a MonitorSnapshot,
    pub update_status: &'a UpdateStatus,
    pub self_update_writable: bool,
    pub theme: UiTheme,
    pub active_tab: PopupTab,
    pub settings_sub_tab: SettingsSubTab,
    pub selected_metric_idx: Option<usize>,
    pub drag_metric_from: Option<usize>,
    pub drag_metric_active: bool,
    pub drag_metric_hover: Option<usize>,
    pub cached_autostart: bool,
    pub has_dwm_border: bool,
    pub logical_w: f32,
    pub logical_h: f32,
    pub hovered_id: Option<&'a ControlId>,
    pub pressed_id: Option<&'a ControlId>,
    pub focused_id: Option<&'a ControlId>,
    pub scroll_y: &'a mut f32,
    pub max_scroll_y: &'a mut f32,
    pub hit_boxes: &'a mut Vec<HitBox>,
    pub focus_items: &'a mut Vec<FocusItem>,
    pub metric_slot_rects: &'a mut Vec<MetricSlotRect>,
    pub utf16_buf: &'a mut Vec<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScrollMetrics {
    pub view_h: f32,
    pub body_content_h: f32,
    pub max_scroll_y: f32,
    pub thumb_ratio: f32,
}

pub(crate) fn compute_scroll_metrics(total_content_h: f32, logical_h: f32) -> ScrollMetrics {
    let view_h = (logical_h - HEADER_HEIGHT).max(1.0);
    // `measure_content_height` measures from y = 0 and already includes `HEADER_HEIGHT`.
    let body_content_h = (total_content_h - HEADER_HEIGHT).max(0.0);
    let max_scroll_y = (body_content_h - view_h).max(0.0);
    let thumb_ratio = (view_h / body_content_h.max(1.0)).clamp(0.12, 1.0);
    ScrollMetrics {
        view_h,
        body_content_h,
        max_scroll_y,
        thumb_ratio,
    }
}

pub(crate) unsafe fn draw_popup_scene(ctx: &mut SceneInput<'_>) {
    let p = PopupPalette::for_theme(ctx.theme);
    ctx.rt.Clear(Some(&p.bg));

    // Clamp scroll_y before drawing so hit_boxes and visual coordinates stay 100% aligned.
    let total_content_h = measure_content_height(
        ctx.active_tab,
        ctx.settings_sub_tab,
        ctx.snapshot,
        ctx.update_status,
        ctx.self_update_writable,
    );
    let scroll = compute_scroll_metrics(total_content_h, ctx.logical_h);
    *ctx.max_scroll_y = scroll.max_scroll_y;
    *ctx.scroll_y = ctx.scroll_y.clamp(0.0, *ctx.max_scroll_y);

    draw_header(ctx, &p);

    let viewport = D2D_RECT_F {
        left: 0.0,
        top: HEADER_HEIGHT,
        right: ctx.logical_w,
        bottom: ctx.logical_h,
    };
    ctx.rt
        .PushAxisAlignedClip(&viewport, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);

    match ctx.active_tab {
        PopupTab::Overview => draw_overview_tab(ctx, &p),
        PopupTab::Settings => draw_settings_tab(ctx, &p),
        PopupTab::About => draw_about_tab(ctx, &p),
    }

    ctx.rt.PopAxisAlignedClip();

    // Subtle scrollbar indicator when scrollable.
    if *ctx.max_scroll_y > 1.0 {
        ctx.brush.SetColor(&p.card_border);
        let thumb_h = (scroll.view_h * scroll.thumb_ratio).max(20.0);
        let scroll_frac = *ctx.scroll_y / *ctx.max_scroll_y;
        let thumb_y = HEADER_HEIGHT + 4.0 + scroll_frac * (scroll.view_h - thumb_h - 8.0);
        let thumb = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: ctx.logical_w - 5.0,
                top: thumb_y,
                right: ctx.logical_w - 2.0,
                bottom: thumb_y + thumb_h,
            },
            radiusX: 1.5,
            radiusY: 1.5,
        };
        ctx.rt.FillRoundedRectangle(&thumb, ctx.brush);
    }

    // Win10 (no DWM rounded corner border) and High Contrast mode get a crisp 1px outer frame.
    if !ctx.has_dwm_border || ctx.theme.is_high_contrast() {
        ctx.brush.SetColor(&p.window_border);
        let border_rect = D2D_RECT_F {
            left: 0.5,
            top: 0.5,
            right: (ctx.logical_w - 0.5).max(1.0),
            bottom: (ctx.logical_h - 0.5).max(1.0),
        };
        ctx.rt.DrawRectangle(&border_rect, ctx.brush, 1.0, None);
    }
}

unsafe fn draw_header(ctx: &mut SceneInput<'_>, p: &PopupPalette) {
    let tab_w = 48.0;
    let tab_gap = 5.0;
    let tab_h = 26.0;
    let tab_y = (HEADER_HEIGHT - tab_h) * 0.5;
    let about_rect = D2D_RECT_F {
        left: ctx.logical_w - PAD_X - tab_w,
        top: tab_y,
        right: ctx.logical_w - PAD_X,
        bottom: tab_y + tab_h,
    };
    let settings_rect = D2D_RECT_F {
        left: about_rect.left - tab_gap - tab_w,
        top: tab_y,
        right: about_rect.left - tab_gap,
        bottom: tab_y + tab_h,
    };
    let overview_rect = D2D_RECT_F {
        left: settings_rect.left - tab_gap - tab_w,
        top: tab_y,
        right: settings_rect.left - tab_gap,
        bottom: tab_y + tab_h,
    };

    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        "AirMonitor",
        &ctx.fonts.title_fmt,
        &p.text_primary,
        D2D_RECT_F {
            left: PAD_X,
            top: 0.0,
            right: (overview_rect.left - 8.0).max(PAD_X + 1.0),
            bottom: HEADER_HEIGHT,
        },
    );

    draw_pill_button(
        ctx,
        p,
        ControlId::Tab(PopupTab::Overview),
        overview_rect,
        "概览",
        ctx.active_tab == PopupTab::Overview,
        PopupAction::SwitchTab(PopupTab::Overview),
        false,
    );
    draw_pill_button(
        ctx,
        p,
        ControlId::Tab(PopupTab::Settings),
        settings_rect,
        "设置",
        ctx.active_tab == PopupTab::Settings,
        PopupAction::SwitchTab(PopupTab::Settings),
        false,
    );
    draw_pill_button(
        ctx,
        p,
        ControlId::Tab(PopupTab::About),
        about_rect,
        "关于",
        ctx.active_tab == PopupTab::About,
        PopupAction::SwitchTab(PopupTab::About),
        false,
    );

    ctx.brush.SetColor(&p.divider);
    ctx.rt.DrawLine(
        D2D_POINT_2F {
            x: PAD_X,
            y: HEADER_HEIGHT - 0.5,
        },
        D2D_POINT_2F {
            x: ctx.logical_w - PAD_X,
            y: HEADER_HEIGHT - 0.5,
        },
        ctx.brush,
        1.0,
        None,
    );
}

unsafe fn draw_overview_tab(ctx: &mut SceneInput<'_>, p: &PopupPalette) {
    let layout = compute_overview_layout(ctx.snapshot, ctx.logical_w, *ctx.scroll_y);

    // Card 1: Hardware Telemetry (or disabled placeholder)
    draw_card(ctx.rt, ctx.brush, p, layout.hw_card_rect);

    let inner_x = PAD_X + CARD_PAD;
    let inner_r = PAD_X + layout.card_w - CARD_PAD;

    if !ctx.snapshot.config.hardware_enabled {
        let text_r = layout
            .hw_enable_btn_rect
            .map_or(inner_r, |b| (b.left - 10.0).max(inner_x + 1.0));
        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            "硬件监测已关闭",
            &ctx.fonts.body_bold_fmt,
            &p.text_primary,
            D2D_RECT_F {
                left: inner_x,
                top: layout.hw_card_rect.top + 11.0,
                right: text_r,
                bottom: layout.hw_card_rect.top + 29.0,
            },
        );
        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            "后台硬件采集已停止并释放资源",
            &ctx.fonts.small_fmt,
            &p.text_muted,
            D2D_RECT_F {
                left: inner_x,
                top: layout.hw_card_rect.top + 31.0,
                right: text_r,
                bottom: layout.hw_card_rect.top + 48.0,
            },
        );
        if let Some(btn_rect) = layout.hw_enable_btn_rect {
            draw_pill_button(
                ctx,
                p,
                ControlId::OverviewToggleHardware,
                btn_rect,
                "立即开启",
                true,
                PopupAction::ToggleHardwareEnabled,
                true,
            );
        }
    } else {
        let mut cy = layout.hw_card_rect.top + 10.0;

        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            "硬件遥测",
            &ctx.fonts.section_fmt,
            &p.text_primary,
            D2D_RECT_F {
                left: inner_x,
                top: cy,
                right: inner_x + 76.0,
                bottom: cy + 20.0,
            },
        );
        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            &layout.hw_spec.provider_summary,
            &ctx.fonts.small_right_fmt,
            &p.text_muted,
            D2D_RECT_F {
                left: inner_x + 76.0,
                top: cy + 2.0,
                right: inner_r,
                bottom: cy + 20.0,
            },
        );
        cy += 24.0;

        let hw = &ctx.snapshot.hw;
        let cpu_val = format_overview_cpu_value(hw);
        draw_kv_row(ctx, p, (inner_x, inner_r), cy, "CPU", &cpu_val);
        cy += HW_ROW_CPU_H;

        if let Some(cpu_ext) = &layout.hw_spec.cpu_ext_line {
            draw_kv_row(ctx, p, (inner_x, inner_r), cy, "详情", cpu_ext);
            cy += HW_ROW_CPU_EXT_H;
        }

        if let Some(acpi_temp) = &layout.hw_spec.acpi_temp_line {
            draw_kv_row_with_key_width(
                ctx,
                p,
                (inner_x, inner_r),
                cy,
                "主板/ACPI 热区",
                acpi_temp,
                114.0,
            );
            cy += HW_ROW_ACPI_H;
        }

        let mem_val = format_overview_mem_value(hw);
        draw_kv_row(ctx, p, (inner_x, inner_r), cy, "内存", &mem_val);
        cy += HW_ROW_MEM_H;

        let nic = format_overview_nic_value(hw);
        draw_kv_row(ctx, p, (inner_x, inner_r), cy, "网卡", nic);
        cy += HW_ROW_NIC_H;

        let net_val = format_overview_net_value(hw);
        draw_kv_row(ctx, p, (inner_x, inner_r), cy, "网速", &net_val);
        cy += HW_ROW_NET_H;

        if let Some(disk_line) = &layout.hw_spec.disk_line {
            draw_kv_row(ctx, p, (inner_x, inner_r), cy, "磁盘", disk_line);
            cy += HW_ROW_DISK_H;
        }

        if let Some(bat_line) = &layout.hw_spec.sys_battery_line {
            draw_kv_row(ctx, p, (inner_x, inner_r), cy, "电池", bat_line);
            cy += HW_ROW_BAT_H;
        }

        cy += HW_PRE_DIVIDER_GAP;
        ctx.brush.SetColor(&p.divider);
        ctx.rt.DrawLine(
            D2D_POINT_2F { x: inner_x, y: cy },
            D2D_POINT_2F { x: inner_r, y: cy },
            ctx.brush,
            1.0,
            None,
        );
        cy += HW_DIVIDER_GAP;

        let gpu_title = if hw.gpu_name.is_empty() {
            "GPU: --"
        } else {
            &hw.gpu_name
        };
        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            gpu_title,
            &ctx.fonts.body_bold_fmt,
            &p.text_primary,
            D2D_RECT_F {
                left: inner_x,
                top: cy,
                right: inner_r,
                bottom: cy + 18.0,
            },
        );
        cy += HW_GPU_TITLE_H;

        let mid_x = (inner_x + inner_r) * 0.5 - 6.0;
        let right_col_x = mid_x + 14.0;
        for pair in layout.hw_spec.gpu_items.chunks(2) {
            let (k1, v1) = &pair[0];
            draw_kv_row(ctx, p, (inner_x, mid_x), cy, k1, v1);
            if let Some((k2, v2)) = pair.get(1) {
                draw_kv_row(ctx, p, (right_col_x, inner_r), cy, k2, v2);
            }
            cy += HW_GPU_ROW_H;
        }
    }

    // Wireless section header + optional Refresh button
    let wl_header_text = if ctx.snapshot.config.wireless_enabled {
        format!("无线设备 ({})", layout.device_cards.len())
    } else {
        "无线设备".to_string()
    };
    let wl_header_right = layout
        .refresh_rect
        .map_or(PAD_X + layout.card_w, |r| r.left - 8.0);
    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        &wl_header_text,
        &ctx.fonts.section_fmt,
        &p.text_primary,
        D2D_RECT_F {
            left: PAD_X + 2.0,
            top: layout.wireless_header_y + 2.0,
            right: wl_header_right,
            bottom: layout.wireless_header_y + 24.0,
        },
    );
    if let Some(refresh_rect) = layout.refresh_rect {
        draw_pill_button(
            ctx,
            p,
            ControlId::OverviewRefreshWireless,
            refresh_rect,
            "刷新设备",
            false,
            PopupAction::RefreshWireless,
            true,
        );
    }

    if let Some(disabled_rect) = layout.wireless_disabled_rect {
        draw_card(ctx.rt, ctx.brush, p, disabled_rect);
        let text_r = layout
            .wireless_enable_btn_rect
            .map_or(disabled_rect.right - CARD_PAD, |b| {
                (b.left - 10.0).max(disabled_rect.left + CARD_PAD + 1.0)
            });
        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            "无线设备监测已关闭",
            &ctx.fonts.body_bold_fmt,
            &p.text_primary,
            D2D_RECT_F {
                left: disabled_rect.left + CARD_PAD,
                top: disabled_rect.top + 11.0,
                right: text_r,
                bottom: disabled_rect.top + 29.0,
            },
        );
        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            "后台无线轮询与低电量提醒已暂停",
            &ctx.fonts.small_fmt,
            &p.text_muted,
            D2D_RECT_F {
                left: disabled_rect.left + CARD_PAD,
                top: disabled_rect.top + 31.0,
                right: text_r,
                bottom: disabled_rect.top + 48.0,
            },
        );
        if let Some(btn_rect) = layout.wireless_enable_btn_rect {
            draw_pill_button(
                ctx,
                p,
                ControlId::OverviewToggleWireless,
                btn_rect,
                "立即开启",
                true,
                PopupAction::ToggleWirelessEnabled,
                true,
            );
        }
    } else if let Some(empty_rect) = layout.empty_rect {
        draw_card(ctx.rt, ctx.brush, p, empty_rect);
        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            "暂未检测到无线电量设备，可点击右上角「刷新设备」",
            &ctx.fonts.small_fmt,
            &p.text_muted,
            D2D_RECT_F {
                left: empty_rect.left + CARD_PAD,
                top: empty_rect.top,
                right: empty_rect.right - CARD_PAD,
                bottom: empty_rect.bottom,
            },
        );
    } else {
        for card_layout in &layout.device_cards {
            let dev = &ctx.snapshot.devices[card_layout.index];
            draw_card(ctx.rt, ctx.brush, p, card_layout.rect);
            draw_device_card_contents(ctx, dev, p, card_layout.rect);
        }
    }
}

unsafe fn draw_device_card_contents(
    ctx: &mut SceneInput<'_>,
    dev: &WirelessDevice,
    p: &PopupPalette,
    card: D2D_RECT_F,
) {
    let is_hidden = ctx.snapshot.config.hidden_devices.contains(&dev.id);
    let status_col = p.device_color(dev, ctx.snapshot.config.low_battery_threshold);

    ctx.brush.SetColor(&status_col);
    draw_device_icon_with_factory(
        ctx.d2d_factory,
        ctx.rt,
        ctx.brush,
        dev.kind,
        card.left + 12.0,
        card.top + 11.0,
        18.0,
    );

    let name = ctx
        .snapshot
        .config
        .device_aliases
        .get(&dev.id)
        .map(String::as_str)
        .unwrap_or(&dev.name);
    let mut title = name.to_string();
    if !dev.online {
        title.push_str(" (离线)");
    } else if is_hidden {
        title.push_str(" (已隐藏)");
    }

    let name_x = card.left + 38.0;
    // DirectWrite ellipsis trimming + D2D1_DRAW_TEXT_OPTIONS_CLIP strictly bounds long CJK/ASCII names.
    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        &title,
        &ctx.fonts.body_bold_fmt,
        if is_hidden {
            &p.text_muted
        } else {
            &p.text_primary
        },
        D2D_RECT_F {
            left: name_x,
            top: card.top + 7.0,
            right: card.right - 116.0,
            bottom: card.top + 25.0,
        },
    );

    // Transport badge
    let badge_rect = D2D_RECT_F {
        left: card.right - 112.0,
        top: card.top + 9.0,
        right: card.right - 74.0,
        bottom: card.top + 25.0,
    };
    ctx.brush.SetColor(&p.pill_bg);
    ctx.rt.FillRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect: badge_rect,
            radiusX: 4.0,
            radiusY: 4.0,
        },
        ctx.brush,
    );
    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        dev.transport.badge(),
        &ctx.fonts.center_fmt,
        &p.text_secondary,
        badge_rect,
    );

    // Hide / Show toggle pill
    let hide_rect = D2D_RECT_F {
        left: card.right - 66.0,
        top: card.top + 7.0,
        right: card.right - 10.0,
        bottom: card.top + 27.0,
    };
    draw_pill_button(
        ctx,
        p,
        ControlId::DeviceToggleHidden(dev.id.clone()),
        hide_rect,
        if is_hidden { "显示" } else { "隐藏" },
        is_hidden,
        PopupAction::ToggleHiddenDevice(dev.id.clone()),
        true,
    );

    // Battery bar + percentage
    let bar_y = card.top + 35.0;
    let bar_l = name_x;
    let bar_r = card.right - 66.0;
    let bar_h = 7.0;
    ctx.brush.SetColor(&p.track_bg);
    ctx.rt.FillRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: bar_l,
                top: bar_y,
                right: bar_r,
                bottom: bar_y + bar_h,
            },
            radiusX: 3.5,
            radiusY: 3.5,
        },
        ctx.brush,
    );
    if let Some(pct) = dev.battery_pct {
        let fill_w = ((bar_r - bar_l) * (pct.min(100) as f32 / 100.0)).max(4.0);
        ctx.brush.SetColor(&status_col);
        ctx.rt.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: bar_l,
                    top: bar_y,
                    right: (bar_l + fill_w).min(bar_r),
                    bottom: bar_y + bar_h,
                },
                radiusX: 3.5,
                radiusY: 3.5,
            },
            ctx.brush,
        );
    }

    let bat_label = format_device_battery(dev);
    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        &bat_label,
        &ctx.fonts.right_fmt,
        &status_col,
        D2D_RECT_F {
            left: card.right - 64.0,
            top: bar_y - 5.0,
            right: card.right - 10.0,
            bottom: bar_y + 12.0,
        },
    );

    if let Some(sub) = dev.sub_batteries {
        let sub_text = format_sub_batteries(&sub);
        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            &sub_text,
            &ctx.fonts.small_fmt,
            &p.text_secondary,
            D2D_RECT_F {
                left: name_x,
                top: card.top + 51.0,
                right: card.right - 12.0,
                bottom: card.top + 70.0,
            },
        );
    }

    if let Some(note) = dev
        .detail_note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let note_y = if dev.sub_batteries.is_some() {
            card.top + 71.0
        } else {
            card.top + 50.0
        };
        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            note,
            &ctx.fonts.small_fmt,
            &p.text_muted,
            D2D_RECT_F {
                left: name_x,
                top: note_y,
                right: card.right - 12.0,
                bottom: note_y + 18.0,
            },
        );
    }
}

fn clamp_selected_metric(selected: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(selected.unwrap_or(0).min(len - 1))
    }
}

unsafe fn draw_settings_tab(ctx: &mut SceneInput<'_>, p: &PopupPalette) {
    let layout = compute_settings_layout(
        ctx.snapshot,
        ctx.settings_sub_tab,
        ctx.logical_w,
        *ctx.scroll_y,
    );
    let cfg = &ctx.snapshot.config;

    draw_segmented_bar(
        ctx,
        p,
        layout.sub_nav_rect,
        [
            (
                ControlId::SettingsSubTab(SettingsSubTab::Metrics),
                "显示指标",
                ctx.settings_sub_tab == SettingsSubTab::Metrics,
                PopupAction::SwitchSettingsSubTab(SettingsSubTab::Metrics),
            ),
            (
                ControlId::SettingsSubTab(SettingsSubTab::Layout),
                "布局停靠",
                ctx.settings_sub_tab == SettingsSubTab::Layout,
                PopupAction::SwitchSettingsSubTab(SettingsSubTab::Layout),
            ),
            (
                ControlId::SettingsSubTab(SettingsSubTab::General),
                "通用选项",
                ctx.settings_sub_tab == SettingsSubTab::General,
                PopupAction::SwitchSettingsSubTab(SettingsSubTab::General),
            ),
        ],
    );

    match &layout.body {
        SettingsBodyLayout::Metrics(m) => {
            if let (Some(banner_rect), Some(btn_rect)) =
                (m.disabled_banner_rect, m.disabled_enable_btn_rect)
            {
                draw_card(ctx.rt, ctx.brush, p, banner_rect);
                let inner_x = banner_rect.left + CARD_PAD;
                draw_text_clipped(
                    ctx.rt,
                    ctx.brush,
                    ctx.utf16_buf,
                    "硬件监测已关闭，开启后以下指标配置生效",
                    &ctx.fonts.small_fmt,
                    &p.orange,
                    D2D_RECT_F {
                        left: inner_x,
                        top: banner_rect.top,
                        right: (btn_rect.left - 8.0).max(inner_x + 1.0),
                        bottom: banner_rect.bottom,
                    },
                );
                draw_pill_button(
                    ctx,
                    p,
                    ControlId::SettingsMetricsToggleHardware,
                    btn_rect,
                    "立即开启",
                    true,
                    PopupAction::ToggleHardwareEnabled,
                    true,
                );
            }

            // Card 1: WYSIWYG 2-row horizontal taskbar grid + spatial action bar
            draw_card(ctx.rt, ctx.brush, p, m.grid_card_rect);
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                "任务栏排布（拖拽或点选调位）",
                &ctx.fonts.section_fmt,
                &p.text_primary,
                m.grid_title_rect,
            );
            let (grid_sub_text, grid_sub_color) = if cfg.hardware_enabled {
                ("格子位置与任务栏上下/左右完全一致", &p.text_muted)
            } else {
                ("硬件监测已关闭 · 仅预览排布，开启后生效", &p.orange)
            };
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                grid_sub_text,
                &ctx.fonts.small_fmt,
                grid_sub_color,
                m.grid_subtitle_rect,
            );
            draw_pill_button(
                ctx,
                p,
                ControlId::ResetDefaultMetrics,
                m.reset_btn_rect,
                "恢复默认",
                false,
                PopupAction::ResetDefaultMetrics,
                true,
            );

            // Recessed grid container
            let box_rr = D2D1_ROUNDED_RECT {
                rect: m.grid_box_rect,
                radiusX: 8.0,
                radiusY: 8.0,
            };
            ctx.brush.SetColor(&p.track_bg);
            ctx.rt.FillRoundedRectangle(&box_rr, ctx.brush);
            ctx.brush.SetColor(&p.card_border);
            ctx.rt.DrawRoundedRectangle(&box_rr, ctx.brush, 1.0, None);

            let items = &cfg.hardware_items;
            let sel = clamp_selected_metric(ctx.selected_metric_idx, items.len());

            if items.is_empty() {
                draw_text_clipped(
                    ctx.rt,
                    ctx.brush,
                    ctx.utf16_buf,
                    "任务栏暂无硬件指标，请从下方点选添加",
                    &ctx.fonts.small_center_fmt,
                    &p.text_muted,
                    m.grid_box_rect,
                );
            } else {
                for slot in &m.grid_cells {
                    let metric = items[slot.index];
                    let avail = ctx.snapshot.hw.is_metric_available(metric);
                    let is_drag_src =
                        ctx.drag_metric_active && ctx.drag_metric_from == Some(slot.index);
                    let is_drag_hover = ctx.drag_metric_active
                        && ctx.drag_metric_hover == Some(slot.index)
                        && !is_drag_src;
                    let is_selected = sel == Some(slot.index);

                    let slot_id = ControlId::MetricSlot(slot.index);
                    let hovered = !ctx.drag_metric_active && ctx.hovered_id == Some(&slot_id);

                    let cell_rr = D2D1_ROUNDED_RECT {
                        rect: slot.rect,
                        radiusX: 6.0,
                        radiusY: 6.0,
                    };
                    if is_drag_src {
                        ctx.brush.SetColor(&p.track_bg);
                        ctx.rt.FillRoundedRectangle(&cell_rr, ctx.brush);
                        ctx.brush.SetColor(&p.text_muted);
                        ctx.rt.DrawRoundedRectangle(
                            &cell_rr,
                            ctx.brush,
                            1.2,
                            Some(ctx.dash_stroke),
                        );
                    } else if is_drag_hover {
                        let hover_bg = composite_over(p.accent_soft, p.card_bg);
                        ctx.brush.SetColor(&hover_bg);
                        ctx.rt.FillRoundedRectangle(&cell_rr, ctx.brush);
                        ctx.brush.SetColor(&p.green);
                        ctx.rt.DrawRoundedRectangle(
                            &cell_rr,
                            ctx.brush,
                            1.8,
                            Some(ctx.dash_stroke),
                        );
                    } else if is_selected {
                        let sel_bg = composite_over(p.accent_soft, p.card_bg);
                        ctx.brush.SetColor(&sel_bg);
                        ctx.rt.FillRoundedRectangle(&cell_rr, ctx.brush);
                        ctx.brush.SetColor(&p.accent);
                        ctx.rt.DrawRoundedRectangle(&cell_rr, ctx.brush, 1.5, None);
                    } else if hovered {
                        ctx.brush.SetColor(&p.pill_bg);
                        ctx.rt.FillRoundedRectangle(&cell_rr, ctx.brush);
                        ctx.brush.SetColor(&p.window_border);
                        ctx.rt.DrawRoundedRectangle(&cell_rr, ctx.brush, 1.2, None);
                    } else {
                        ctx.brush.SetColor(&p.card_bg);
                        ctx.rt.FillRoundedRectangle(&cell_rr, ctx.brush);
                        ctx.brush.SetColor(&p.card_border);
                        ctx.rt.DrawRoundedRectangle(&cell_rr, ctx.brush, 1.2, None);
                    }

                    let (short_lbl, raw_val) = format_hw_metric(metric, &ctx.snapshot.hw);
                    let val_text = if avail { raw_val.trim() } else { "--" };
                    let top_rect = D2D_RECT_F {
                        left: slot.rect.left + 7.0,
                        top: slot.rect.top + 4.0,
                        right: slot.rect.right - 7.0,
                        bottom: slot.rect.top + 21.0,
                    };
                    let lbl_color = if is_drag_src {
                        &p.text_muted
                    } else {
                        match metric {
                            HwMetric::NetUp => &p.accent,
                            HwMetric::NetDown => &p.green,
                            _ => &p.text_muted,
                        }
                    };
                    draw_text_clipped(
                        ctx.rt,
                        ctx.brush,
                        ctx.utf16_buf,
                        short_lbl,
                        &ctx.fonts.mono_left_fmt,
                        lbl_color,
                        top_rect,
                    );
                    let val_color = if !avail || is_drag_src {
                        &p.text_muted
                    } else {
                        &p.text_primary
                    };
                    draw_text_clipped(
                        ctx.rt,
                        ctx.brush,
                        ctx.utf16_buf,
                        val_text,
                        &ctx.fonts.mono_right_fmt,
                        val_color,
                        top_rect,
                    );

                    let bot_rect = D2D_RECT_F {
                        left: slot.rect.left + 7.0,
                        top: slot.rect.top + 21.0,
                        right: slot.rect.right - 7.0,
                        bottom: slot.rect.bottom - 4.0,
                    };
                    let name_text = if avail {
                        metric.display_name().to_string()
                    } else {
                        format!("{} (无源)", metric.display_name())
                    };
                    let name_color = if !avail || is_drag_src {
                        &p.text_muted
                    } else {
                        &p.text_secondary
                    };
                    draw_text_clipped(
                        ctx.rt,
                        ctx.brush,
                        ctx.utf16_buf,
                        &name_text,
                        &ctx.fonts.small_fmt,
                        name_color,
                        bot_rect,
                    );

                    let max_y = ctx.logical_h.max(HEADER_HEIGHT);
                    let clipped = D2D_RECT_F {
                        left: slot.rect.left,
                        top: slot.rect.top.max(HEADER_HEIGHT),
                        right: slot.rect.right,
                        bottom: slot.rect.bottom.min(max_y),
                    };
                    if clipped.bottom > clipped.top {
                        ctx.metric_slot_rects.push(MetricSlotRect {
                            index: slot.index,
                            rect: clipped,
                        });
                    }
                    if ctx.focused_id == Some(&slot_id) {
                        draw_focus_ring(ctx, p, slot.rect, 6.0);
                    }
                    register_hit(
                        ctx,
                        slot_id,
                        slot.rect,
                        PopupAction::SelectMetricSlot(slot.index),
                        true,
                    );
                }

                if let Some(empty_rect) = m.empty_slot_rect {
                    let empty_rr = D2D1_ROUNDED_RECT {
                        rect: empty_rect,
                        radiusX: 6.0,
                        radiusY: 6.0,
                    };
                    ctx.brush.SetColor(&p.card_border);
                    ctx.rt
                        .DrawRoundedRectangle(&empty_rr, ctx.brush, 1.0, Some(ctx.dash_stroke));
                    draw_text_clipped(
                        ctx.rt,
                        ctx.brush,
                        ctx.utf16_buf,
                        "空槽位",
                        &ctx.fonts.small_center_fmt,
                        &p.text_muted,
                        empty_rect,
                    );
                }
            }

            // 2D Spatial Action Bar
            let len = items.len();
            let sel_i = sel.unwrap_or(0);
            let can_left = sel.is_some_and(|i| i >= 2);
            let can_swap_row = sel.is_some_and(|i| (i ^ 1) < len);
            let can_right = sel.is_some_and(|i| i + 2 < len || (i % 2 == 1 && i + 1 < len));
            let can_remove = sel.is_some_and(|i| i < len);

            draw_action_pill(
                ctx,
                p,
                ControlId::MetricMoveLeft,
                m.act_left_rect,
                "← 左移",
                can_left,
                false,
                PopupAction::MoveSelectedMetricLeft(sel_i),
            );
            draw_action_pill(
                ctx,
                p,
                ControlId::MetricSwapRow,
                m.act_swap_row_rect,
                "⇅ 上下",
                can_swap_row,
                false,
                PopupAction::SwapSelectedMetricRow(sel_i),
            );
            draw_action_pill(
                ctx,
                p,
                ControlId::MetricMoveRight,
                m.act_right_rect,
                "右移 →",
                can_right,
                false,
                PopupAction::MoveSelectedMetricRight(sel_i),
            );
            draw_action_pill(
                ctx,
                p,
                ControlId::MetricRemove,
                m.act_remove_rect,
                "× 移除",
                can_remove,
                true,
                PopupAction::RemoveMetricSlot(sel_i),
            );

            // Card 2: Categorized 3-column chip wall
            draw_card(ctx.rt, ctx.brush, p, m.chips_card_rect);
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                "点击增减指标",
                &ctx.fonts.section_fmt,
                &p.text_primary,
                m.chips_title_rect,
            );
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                "虚线 = 当前无源",
                &ctx.fonts.small_right_fmt,
                &p.text_muted,
                m.chips_hint_rect,
            );

            for grp in &m.groups {
                draw_text_clipped(
                    ctx.rt,
                    ctx.brush,
                    ctx.utf16_buf,
                    grp.title,
                    &ctx.fonts.small_fmt,
                    &p.text_muted,
                    grp.title_rect,
                );
                for &(metric, chip_rect) in &grp.chips {
                    let on = cfg.hardware_items.contains(&metric);
                    let avail = ctx.snapshot.hw.is_metric_available(metric);
                    draw_metric_chip(ctx, p, chip_rect, metric, on, avail);
                }
            }
        }
        SettingsBodyLayout::Layout(l) => {
            let effective_mode = cfg.effective_layout_mode();
            // Card 1: Display mode
            draw_card(ctx.rt, ctx.brush, p, l.mode_card_rect);
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                "显示模式",
                &ctx.fonts.section_fmt,
                &p.text_primary,
                l.mode_title_rect,
            );
            let (mode_sub_text, mode_sub_color) = match (cfg.hardware_enabled, cfg.wireless_enabled)
            {
                (false, false) => ("硬件与无线监测均已关闭，任务栏浮条已隐藏", &p.orange),
                (false, true) => ("硬件监测已关闭（当前任务栏仅显示无线设备区）", &p.orange),
                (true, false) => ("无线监测已关闭（当前任务栏仅显示硬件区）", &p.orange),
                (true, true) => ("选择任务栏硬件监控与无线电量的组合方式", &p.text_muted),
            };
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                mode_sub_text,
                &ctx.fonts.small_fmt,
                mode_sub_color,
                l.mode_subtitle_rect,
            );
            draw_segmented_bar(
                ctx,
                p,
                l.mode_seg_rect,
                [
                    (
                        ControlId::LayoutMode(LayoutMode::Combined),
                        "合并",
                        effective_mode == Some(LayoutMode::Combined),
                        PopupAction::SetLayout(LayoutMode::Combined, None),
                    ),
                    (
                        ControlId::LayoutMode(LayoutMode::Split),
                        "分体",
                        effective_mode == Some(LayoutMode::Split),
                        PopupAction::SetLayout(LayoutMode::Split, None),
                    ),
                    (
                        ControlId::LayoutMode(LayoutMode::HardwareOnly),
                        "仅硬件",
                        effective_mode == Some(LayoutMode::HardwareOnly),
                        PopupAction::SetLayout(LayoutMode::HardwareOnly, None),
                    ),
                    (
                        ControlId::LayoutMode(LayoutMode::DevicesOnly),
                        "仅设备",
                        effective_mode == Some(LayoutMode::DevicesOnly),
                        PopupAction::SetLayout(LayoutMode::DevicesOnly, None),
                    ),
                ],
            );

            if let (Some(div_y), Some(lbl_rect), Some(dev_rect), Some(hw_rect)) = (
                l.order_divider_y,
                l.order_label_rect,
                l.order_dev_first_rect,
                l.order_hw_first_rect,
            ) {
                ctx.brush.SetColor(&p.divider);
                ctx.rt.DrawLine(
                    D2D_POINT_2F {
                        x: l.mode_title_rect.left,
                        y: div_y,
                    },
                    D2D_POINT_2F {
                        x: l.mode_title_rect.right,
                        y: div_y,
                    },
                    ctx.brush,
                    1.0,
                    None,
                );
                draw_text_clipped(
                    ctx.rt,
                    ctx.brush,
                    ctx.utf16_buf,
                    "合并排列顺序",
                    &ctx.fonts.body_fmt,
                    &p.text_secondary,
                    lbl_rect,
                );
                draw_pill_button(
                    ctx,
                    p,
                    ControlId::CombinedOrder(CombinedOrder::DevicesFirst),
                    dev_rect,
                    "设备在左 · 硬件在右",
                    cfg.combined_order == CombinedOrder::DevicesFirst,
                    PopupAction::SetLayout(LayoutMode::Combined, Some(CombinedOrder::DevicesFirst)),
                    true,
                );
                draw_pill_button(
                    ctx,
                    p,
                    ControlId::CombinedOrder(CombinedOrder::HardwareFirst),
                    hw_rect,
                    "硬件在左 · 设备在右",
                    cfg.combined_order == CombinedOrder::HardwareFirst,
                    PopupAction::SetLayout(
                        LayoutMode::Combined,
                        Some(CombinedOrder::HardwareFirst),
                    ),
                    true,
                );
            }

            // Card 2: Dock position & horizontal offset
            draw_card(ctx.rt, ctx.brush, p, l.dock_card_rect);
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                "停靠位置与水平偏移",
                &ctx.fonts.section_fmt,
                &p.text_primary,
                l.dock_title_rect,
            );
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                "按住 Ctrl + 左键拖动任务栏浮条可直接微调",
                &ctx.fonts.small_fmt,
                &p.text_muted,
                l.dock_subtitle_rect,
            );

            if let Some(disabled_box) = l.disabled_box_rect {
                let d_rr = D2D1_ROUNDED_RECT {
                    rect: disabled_box,
                    radiusX: 6.0,
                    radiusY: 6.0,
                };
                ctx.brush.SetColor(&p.track_bg);
                ctx.rt.FillRoundedRectangle(&d_rr, ctx.brush);
                ctx.brush.SetColor(&p.card_border);
                ctx.rt.DrawRoundedRectangle(&d_rr, ctx.brush, 1.0, None);
                draw_text_clipped(
                    ctx.rt,
                    ctx.brush,
                    ctx.utf16_buf,
                    "硬件与无线设备监测均已关闭，任务栏无停靠浮条",
                    &ctx.fonts.small_center_fmt,
                    &p.text_muted,
                    disabled_box,
                );
            }

            for z in &l.zones {
                let zone_cfg = if z.is_combined {
                    &cfg.combined_zone
                } else if z.is_devices {
                    &cfg.devices_zone
                } else {
                    &cfg.hardware_zone
                };
                let z_rr = D2D1_ROUNDED_RECT {
                    rect: z.box_rect,
                    radiusX: 6.0,
                    radiusY: 6.0,
                };
                ctx.brush.SetColor(&p.track_bg);
                ctx.rt.FillRoundedRectangle(&z_rr, ctx.brush);
                ctx.brush.SetColor(&p.card_border);
                ctx.rt.DrawRoundedRectangle(&z_rr, ctx.brush, 1.0, None);

                draw_text_clipped(
                    ctx.rt,
                    ctx.brush,
                    ctx.utf16_buf,
                    z.label,
                    &ctx.fonts.body_bold_fmt,
                    &p.text_primary,
                    z.label_rect,
                );
                draw_pill_button(
                    ctx,
                    p,
                    ControlId::ZoneAnchor {
                        is_devices: z.is_devices,
                        is_combined: z.is_combined,
                        anchor: AnchorEdge::Left,
                    },
                    z.left_anchor_rect,
                    "左锚",
                    zone_cfg.anchor == AnchorEdge::Left,
                    PopupAction::SetAnchor {
                        is_devices: z.is_devices,
                        is_combined: z.is_combined,
                        anchor: AnchorEdge::Left,
                    },
                    true,
                );
                draw_pill_button(
                    ctx,
                    p,
                    ControlId::ZoneAnchor {
                        is_devices: z.is_devices,
                        is_combined: z.is_combined,
                        anchor: AnchorEdge::Right,
                    },
                    z.right_anchor_rect,
                    "右锚",
                    zone_cfg.anchor == AnchorEdge::Right,
                    PopupAction::SetAnchor {
                        is_devices: z.is_devices,
                        is_combined: z.is_combined,
                        anchor: AnchorEdge::Right,
                    },
                    true,
                );

                draw_text_clipped(
                    ctx.rt,
                    ctx.brush,
                    ctx.utf16_buf,
                    "水平偏移",
                    &ctx.fonts.body_fmt,
                    &p.text_secondary,
                    z.offset_label_rect,
                );
                draw_stepper(
                    ctx,
                    p,
                    z.stepper_rect,
                    &format!("{}px", zone_cfg.offset_x),
                    ControlId::ZoneOffsetStep {
                        is_devices: z.is_devices,
                        is_combined: z.is_combined,
                        delta: -4,
                    },
                    PopupAction::StepOffset {
                        is_devices: z.is_devices,
                        is_combined: z.is_combined,
                        delta: -4,
                    },
                    ControlId::ZoneOffsetStep {
                        is_devices: z.is_devices,
                        is_combined: z.is_combined,
                        delta: 4,
                    },
                    PopupAction::StepOffset {
                        is_devices: z.is_devices,
                        is_combined: z.is_combined,
                        delta: 4,
                    },
                );
                draw_pill_button(
                    ctx,
                    p,
                    ControlId::ZoneOffsetReset {
                        is_devices: z.is_devices,
                        is_combined: z.is_combined,
                    },
                    z.reset_rect,
                    "归零",
                    false,
                    PopupAction::ResetZoneOffset {
                        is_devices: z.is_devices,
                        is_combined: z.is_combined,
                    },
                    true,
                );
            }
        }
        SettingsBodyLayout::General(g) => {
            // Card 1: Collection & display
            draw_card(ctx.rt, ctx.brush, p, g.collect_card_rect);
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                "采集与显示",
                &ctx.fonts.section_fmt,
                &p.text_primary,
                g.collect_title_rect,
            );
            draw_toggle_switch(
                ctx,
                p,
                ControlId::GeneralToggleHardware,
                g.hw_enabled_row.row_rect,
                g.hw_enabled_row.title_rect,
                g.hw_enabled_row.subtitle_rect,
                g.hw_enabled_row.control_rect,
                "硬件监测",
                if cfg.hardware_enabled {
                    "开启任务栏与浮窗硬件遥测采集"
                } else {
                    "已关闭：后台硬件采集已停止并释放资源"
                },
                cfg.hardware_enabled,
                PopupAction::ToggleHardwareEnabled,
            );

            ctx.brush.SetColor(&p.divider);
            ctx.rt.DrawLine(
                D2D_POINT_2F {
                    x: g.collect_title_rect.left,
                    y: g.collect_div0_y,
                },
                D2D_POINT_2F {
                    x: g.collect_title_rect.right,
                    y: g.collect_div0_y,
                },
                ctx.brush,
                1.0,
                None,
            );
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                "硬件刷新间隔",
                &ctx.fonts.body_bold_fmt,
                &p.text_primary,
                g.poll_row.title_rect,
            );
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                if cfg.hardware_enabled {
                    "间隔越长，后台唤醒越少"
                } else {
                    "硬件监测已关闭，开启后恢复轮询"
                },
                &ctx.fonts.small_fmt,
                if cfg.hardware_enabled {
                    &p.text_muted
                } else {
                    &p.orange
                },
                g.poll_row.subtitle_rect,
            );
            draw_stepper(
                ctx,
                p,
                g.poll_row.control_rect,
                &format!("{:.1}s", cfg.hw_poll_interval_ms as f64 / 1000.0),
                ControlId::HwPollStep(-1),
                PopupAction::StepHwPollInterval(-1),
                ControlId::HwPollStep(1),
                PopupAction::StepHwPollInterval(1),
            );

            ctx.brush.SetColor(&p.divider);
            ctx.rt.DrawLine(
                D2D_POINT_2F {
                    x: g.collect_title_rect.left,
                    y: g.collect_div1_y,
                },
                D2D_POINT_2F {
                    x: g.collect_title_rect.right,
                    y: g.collect_div1_y,
                },
                ctx.brush,
                1.0,
                None,
            );
            draw_toggle_switch(
                ctx,
                p,
                ControlId::ToggleExternalSensors,
                g.ext_sensors_row.row_rect,
                g.ext_sensors_row.title_rect,
                g.ext_sensors_row.subtitle_rect,
                g.ext_sensors_row.control_rect,
                "自动探测外部传感器",
                "HWiNFO64 / AIDA64 / Afterburner / ACPI",
                cfg.enable_external_sensors,
                PopupAction::ToggleEnableExternalSensors,
            );

            ctx.brush.SetColor(&p.divider);
            ctx.rt.DrawLine(
                D2D_POINT_2F {
                    x: g.collect_title_rect.left,
                    y: g.collect_div2_y,
                },
                D2D_POINT_2F {
                    x: g.collect_title_rect.right,
                    y: g.collect_div2_y,
                },
                ctx.brush,
                1.0,
                None,
            );
            draw_toggle_switch(
                ctx,
                p,
                ControlId::ToggleHideUnavailableMetrics,
                g.hide_unavail_row.row_rect,
                g.hide_unavail_row.title_rect,
                g.hide_unavail_row.subtitle_rect,
                g.hide_unavail_row.control_rect,
                "隐藏任务栏无数据指标",
                "数据源离线时自动折叠，不占空白",
                cfg.hide_unavailable_metrics,
                PopupAction::ToggleHideUnavailableMetrics,
            );

            // Card 2: Peripheral alerts & system
            draw_card(ctx.rt, ctx.brush, p, g.device_card_rect);
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                "外设提醒与系统",
                &ctx.fonts.section_fmt,
                &p.text_primary,
                g.device_title_rect,
            );
            draw_toggle_switch(
                ctx,
                p,
                ControlId::GeneralToggleWireless,
                g.wl_enabled_row.row_rect,
                g.wl_enabled_row.title_rect,
                g.wl_enabled_row.subtitle_rect,
                g.wl_enabled_row.control_rect,
                "无线设备监测",
                if cfg.wireless_enabled {
                    "轮询 2.4G / 蓝牙 / Xbox 无线外设电量"
                } else {
                    "已关闭：后台无线轮询已停止并释放资源"
                },
                cfg.wireless_enabled,
                PopupAction::ToggleWirelessEnabled,
            );

            ctx.brush.SetColor(&p.divider);
            ctx.rt.DrawLine(
                D2D_POINT_2F {
                    x: g.device_title_rect.left,
                    y: g.device_div0_y,
                },
                D2D_POINT_2F {
                    x: g.device_title_rect.right,
                    y: g.device_div0_y,
                },
                ctx.brush,
                1.0,
                None,
            );
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                "低电量提醒阈值",
                &ctx.fonts.body_bold_fmt,
                &p.text_primary,
                g.low_bat_row.title_rect,
            );
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                if !cfg.wireless_enabled {
                    "无线监测已关闭，低电量提醒已暂停"
                } else {
                    "低于该电量时弹窗并变色警告"
                },
                &ctx.fonts.small_fmt,
                if !cfg.wireless_enabled {
                    &p.orange
                } else {
                    &p.text_muted
                },
                g.low_bat_row.subtitle_rect,
            );
            draw_stepper(
                ctx,
                p,
                g.low_bat_row.control_rect,
                &format!("{}%", cfg.low_battery_threshold),
                ControlId::LowBatteryStep(-5),
                PopupAction::StepLowThreshold(-5),
                ControlId::LowBatteryStep(5),
                PopupAction::StepLowThreshold(5),
            );

            ctx.brush.SetColor(&p.divider);
            ctx.rt.DrawLine(
                D2D_POINT_2F {
                    x: g.device_title_rect.left,
                    y: g.device_div1_y,
                },
                D2D_POINT_2F {
                    x: g.device_title_rect.right,
                    y: g.device_div1_y,
                },
                ctx.brush,
                1.0,
                None,
            );
            draw_toggle_switch(
                ctx,
                p,
                ControlId::ToggleHideUnknownBattery,
                g.hide_unknown_row.row_rect,
                g.hide_unknown_row.title_rect,
                g.hide_unknown_row.subtitle_rect,
                g.hide_unknown_row.control_rect,
                "隐藏无电量状态设备",
                "过滤仅上报在线但无百分比的接收器",
                cfg.hide_unknown_battery_devices,
                PopupAction::ToggleHideUnknownBatteryDevices,
            );

            ctx.brush.SetColor(&p.divider);
            ctx.rt.DrawLine(
                D2D_POINT_2F {
                    x: g.device_title_rect.left,
                    y: g.device_div2_y,
                },
                D2D_POINT_2F {
                    x: g.device_title_rect.right,
                    y: g.device_div2_y,
                },
                ctx.brush,
                1.0,
                None,
            );
            draw_toggle_switch(
                ctx,
                p,
                ControlId::ToggleAutostart,
                g.autostart_row.row_rect,
                g.autostart_row.title_rect,
                g.autostart_row.subtitle_rect,
                g.autostart_row.control_rect,
                "开机自启动",
                "随 Windows 登录静默运行",
                ctx.cached_autostart,
                PopupAction::ToggleAutostart,
            );
        }
    }
}

unsafe fn draw_about_tab(ctx: &mut SceneInput<'_>, p: &PopupPalette) {
    let layout = compute_about_layout(
        ctx.update_status,
        ctx.self_update_writable,
        ctx.logical_w,
        *ctx.scroll_y,
    );
    let inner_x = PAD_X + CARD_PAD;
    let inner_r = PAD_X + layout.card_w - CARD_PAD;

    // Card 1: App information
    draw_card(ctx.rt, ctx.brush, p, layout.info_card_rect);

    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        "AirMonitor",
        &ctx.fonts.section_fmt,
        &p.text_primary,
        D2D_RECT_F {
            left: inner_x,
            top: layout.info_header_y,
            right: (layout.version_badge_rect.left - 8.0).max(inner_x + 1.0),
            bottom: layout.info_header_y + 20.0,
        },
    );

    ctx.brush.SetColor(&p.pill_bg);
    ctx.rt.FillRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect: layout.version_badge_rect,
            radiusX: 6.0,
            radiusY: 6.0,
        },
        ctx.brush,
    );
    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        &format!("v{}", CURRENT_VERSION),
        &ctx.fonts.center_fmt,
        &p.text_primary,
        layout.version_badge_rect,
    );

    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        "超轻量级 Windows 任务栏硬件与无线电量监视器",
        &ctx.fonts.small_fmt,
        &p.text_muted,
        D2D_RECT_F {
            left: inner_x,
            top: layout.info_subtitle_y,
            right: inner_r,
            bottom: layout.info_subtitle_y + 16.0,
        },
    );

    ctx.brush.SetColor(&p.divider);
    ctx.rt.DrawLine(
        D2D_POINT_2F {
            x: inner_x,
            y: layout.info_divider_y,
        },
        D2D_POINT_2F {
            x: inner_r,
            y: layout.info_divider_y,
        },
        ctx.brush,
        1.0,
        None,
    );

    let mut cy = layout.info_kv_start_y;
    let ver_val = format!("v{} (x64 · 静态 CRT)", CURRENT_VERSION);
    for (k, v) in [
        ("版本", ver_val.as_str()),
        ("架构", "纯用户态 · Direct2D Software"),
        ("协议", "MIT License"),
        ("仓库", "AmlitaEnan/AirMonitor"),
    ] {
        draw_kv_row(ctx, p, (inner_x, inner_r), cy, k, v);
        cy += 20.0;
    }

    draw_pill_button(
        ctx,
        p,
        ControlId::AboutRepo,
        layout.repo_btn_rect,
        "GitHub 主页",
        false,
        PopupAction::OpenUrl(REPO_URL.to_string()),
        true,
    );
    draw_pill_button(
        ctx,
        p,
        ControlId::AboutReleases,
        layout.releases_btn_rect,
        "查看 Releases",
        false,
        PopupAction::OpenUrl(RELEASES_URL.to_string()),
        true,
    );

    // Card 2: Software update
    draw_card(ctx.rt, ctx.brush, p, layout.update_card_rect);

    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        "软件更新",
        &ctx.fonts.section_fmt,
        &p.text_primary,
        D2D_RECT_F {
            left: inner_x,
            top: layout.update_header_y,
            right: (layout.check_btn_rect.left - 8.0).max(inner_x + 1.0),
            bottom: layout.update_header_y + 22.0,
        },
    );

    let (check_label, is_busy) = match ctx.update_status {
        UpdateStatus::Checking => ("检查中...", true),
        UpdateStatus::Downloading { .. } => ("下载中...", true),
        UpdateStatus::Verifying { .. } => ("校验中...", true),
        UpdateStatus::Applying { .. } => ("更新中...", true),
        _ => ("检查更新", false),
    };
    if is_busy {
        draw_static_pill(ctx, p, layout.check_btn_rect, check_label, true);
    } else {
        draw_pill_button(
            ctx,
            p,
            ControlId::AboutCheckUpdates,
            layout.check_btn_rect,
            check_label,
            false,
            PopupAction::CheckForUpdates,
            true,
        );
    }

    match ctx.update_status {
        UpdateStatus::Idle => {
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                "点击右上角「检查更新」获取 GitHub 最新版本",
                &ctx.fonts.small_fmt,
                &p.text_muted,
                layout.update_status_rect,
            );
        }
        UpdateStatus::Checking => {
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                "正在连接 GitHub Releases 检查最新版本...",
                &ctx.fonts.small_fmt,
                &p.text_secondary,
                layout.update_status_rect,
            );
        }
        UpdateStatus::UpToDate { current, .. } => {
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                &format!("✓ 当前已是最新版本 (v{})", current),
                &ctx.fonts.body_bold_fmt,
                &p.green,
                layout.update_status_rect,
            );
        }
        UpdateStatus::NoReleaseYet { current } => {
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                &format!("仓库暂无已发布版本，当前为 v{}", current),
                &ctx.fonts.small_fmt,
                &p.text_secondary,
                layout.update_status_rect,
            );
        }
        UpdateStatus::UpdateAvailable {
            current,
            latest,
            release_url,
            published_at,
            download_url,
            asset_size,
            sha256_url,
            ..
        } => {
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                &format!("发现新版本 v{}（当前 v{}）", latest, current),
                &ctx.fonts.body_bold_fmt,
                &p.accent,
                layout.update_status_rect,
            );
            if let (Some(meta_rect), Some(meta_text)) = (
                layout.update_meta_rect,
                format_update_meta(published_at.as_deref(), *asset_size),
            ) {
                draw_text_clipped(
                    ctx.rt,
                    ctx.brush,
                    ctx.utf16_buf,
                    &meta_text,
                    &ctx.fonts.small_fmt,
                    &p.text_muted,
                    meta_rect,
                );
            }
            let has_download = download_url.is_some();
            let has_sha = sha256_url.as_deref().is_some_and(|s| !s.trim().is_empty())
                && ctx.update_status.can_auto_update();
            let can_auto = ctx.update_status.can_auto_update() && ctx.self_update_writable;
            if let Some(prompt_rect) = layout.update_prompt_rect {
                let (prompt, prompt_color) = if can_auto {
                    ("是否自动下载更新并重启应用？", &p.text_secondary)
                } else if !has_download {
                    (
                        "当前版本未附带直装程序，请前往 GitHub 手动下载",
                        &p.text_secondary,
                    )
                } else if !has_sha {
                    (
                        "该版本缺少 SHA-256 校验文件，请前往发布页手动下载",
                        &p.orange,
                    )
                } else {
                    (
                        "当前程序目录无写入权限，请点击前往手动下载",
                        &p.text_secondary,
                    )
                };
                draw_text_clipped(
                    ctx.rt,
                    ctx.brush,
                    ctx.utf16_buf,
                    prompt,
                    &ctx.fonts.small_fmt,
                    prompt_color,
                    prompt_rect,
                );
            }
            if let (Some(auto_rect), Some(manual_rect)) =
                (layout.auto_update_btn_rect, layout.manual_download_btn_rect)
            {
                draw_pill_button(
                    ctx,
                    p,
                    ControlId::AboutAutoUpdate,
                    auto_rect,
                    "立即下载并更新",
                    true,
                    PopupAction::StartDownloadUpdate,
                    true,
                );
                draw_pill_button(
                    ctx,
                    p,
                    ControlId::AboutManualDownload,
                    manual_rect,
                    "手动下载",
                    false,
                    PopupAction::OpenUrl(release_url.clone()),
                    true,
                );
            } else if let Some(dl_rect) = layout.download_btn_rect {
                let btn_label = if has_download && !has_sha {
                    "手动下载".to_string()
                } else {
                    format!("手动下载 v{}", latest)
                };
                draw_pill_button(
                    ctx,
                    p,
                    ControlId::AboutManualDownload,
                    dl_rect,
                    &btn_label,
                    true,
                    PopupAction::OpenUrl(release_url.clone()),
                    true,
                );
            }
        }
        UpdateStatus::Downloading {
            latest,
            downloaded_bytes,
            total_bytes,
        } => {
            let (text, ratio) = format_download_progress(latest, *downloaded_bytes, *total_bytes);
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                &text,
                &ctx.fonts.small_fmt,
                &p.text_primary,
                layout.update_status_rect,
            );
            if let Some(bar_rect) = layout.progress_bar_rect {
                draw_progress_bar(ctx.rt, ctx.brush, p.track_bg, p.accent, bar_rect, ratio);
            }
            if let Some(cancel_rect) = layout.cancel_update_btn_rect {
                draw_pill_button(
                    ctx,
                    p,
                    ControlId::AboutCancelUpdate,
                    cancel_rect,
                    "取消更新",
                    false,
                    PopupAction::CancelUpdate,
                    true,
                );
            }
        }
        UpdateStatus::Verifying { latest } => {
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                &format!("正在校验 v{} 完整性...", latest),
                &ctx.fonts.small_fmt,
                &p.text_primary,
                layout.update_status_rect,
            );
            if let Some(bar_rect) = layout.progress_bar_rect {
                draw_progress_bar(ctx.rt, ctx.brush, p.track_bg, p.accent, bar_rect, 1.0);
            }
            if let Some(cancel_rect) = layout.cancel_update_btn_rect {
                draw_pill_button(
                    ctx,
                    p,
                    ControlId::AboutCancelUpdate,
                    cancel_rect,
                    "取消更新",
                    false,
                    PopupAction::CancelUpdate,
                    true,
                );
            }
        }
        UpdateStatus::Applying { latest } => {
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                &format!("正在替换程序并重启至 v{}...", latest),
                &ctx.fonts.small_fmt,
                &p.text_primary,
                layout.update_status_rect,
            );
            if let Some(bar_rect) = layout.progress_bar_rect {
                draw_progress_bar(ctx.rt, ctx.brush, p.track_bg, p.green, bar_rect, 1.0);
            }
        }
        UpdateStatus::Error(msg) => {
            draw_text_clipped(
                ctx.rt,
                ctx.brush,
                ctx.utf16_buf,
                &format!("更新失败：{}", msg),
                &ctx.fonts.error_wrap_fmt,
                &p.orange,
                layout.update_status_rect,
            );
        }
    }

    // Card 3: Quick maintenance
    draw_card(ctx.rt, ctx.brush, p, layout.maint_card_rect);
    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        "配置文件与开源主页",
        &ctx.fonts.small_fmt,
        &p.text_muted,
        layout.maint_row_rect,
    );
    draw_pill_button(
        ctx,
        p,
        ControlId::AboutOpenConfig,
        layout.open_config_btn_rect,
        "打开配置文件",
        false,
        PopupAction::OpenConfig,
        true,
    );
}

fn format_download_progress(
    latest: &str,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
) -> (String, f32) {
    match total_bytes.filter(|&t| t > 0) {
        Some(total) => {
            let ratio = (downloaded_bytes as f64 / total as f64).clamp(0.0, 1.0);
            let pct = (ratio * 100.0).round() as u32;
            (
                format!(
                    "正在下载 v{}... {}% ({} / {})",
                    latest,
                    pct,
                    format_bytes_compact(downloaded_bytes),
                    format_bytes_compact(total)
                ),
                ratio as f32,
            )
        }
        None => {
            let fallback_ratio = if downloaded_bytes == 0 {
                0.0
            } else {
                (downloaded_bytes as f32 / (2.0 * 1024.0 * 1024.0)).clamp(0.08, 0.92)
            };
            (
                format!(
                    "正在下载 v{}... {}",
                    latest,
                    format_bytes_compact(downloaded_bytes)
                ),
                fallback_ratio,
            )
        }
    }
}

unsafe fn draw_progress_bar(
    rt: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    track_bg: windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F,
    fill_color: windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F,
    rect: D2D_RECT_F,
    ratio: f32,
) {
    brush.SetColor(&track_bg);
    rt.FillRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect,
            radiusX: 3.5,
            radiusY: 3.5,
        },
        brush,
    );
    let r = ratio.clamp(0.0, 1.0);
    if r > 0.0 {
        let total_w = (rect.right - rect.left).max(0.0);
        let fill_w = (total_w * r).max(6.0).min(total_w);
        if fill_w > 0.0 {
            brush.SetColor(&fill_color);
            rt.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: D2D_RECT_F {
                        left: rect.left,
                        top: rect.top,
                        right: (rect.left + fill_w).min(rect.right),
                        bottom: rect.bottom,
                    },
                    radiusX: 3.5,
                    radiusY: 3.5,
                },
                brush,
            );
        }
    }
}

unsafe fn draw_segmented_bar<const N: usize>(
    ctx: &mut SceneInput<'_>,
    p: &PopupPalette,
    rect: D2D_RECT_F,
    segments: [(ControlId, &str, bool, PopupAction); N],
) {
    let track_rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: 8.0,
        radiusY: 8.0,
    };
    ctx.brush.SetColor(&p.track_bg);
    ctx.rt.FillRoundedRectangle(&track_rr, ctx.brush);
    ctx.brush.SetColor(&p.card_border);
    ctx.rt.DrawRoundedRectangle(&track_rr, ctx.brush, 1.0, None);

    let pad = 3.0;
    let gap = 3.0;
    let inner_w =
        (rect.right - rect.left - pad * 2.0 - gap * (N.saturating_sub(1) as f32)).max(1.0);
    let seg_w = inner_w / (N as f32);

    for (i, (id, label, selected, action)) in segments.into_iter().enumerate() {
        let left = rect.left + pad + (i as f32) * (seg_w + gap);
        let seg_rect = D2D_RECT_F {
            left,
            top: rect.top + pad,
            right: left + seg_w,
            bottom: rect.bottom - pad,
        };
        let seg_rr = D2D1_ROUNDED_RECT {
            rect: seg_rect,
            radiusX: 6.0,
            radiusY: 6.0,
        };

        let hovered = ctx.hovered_id == Some(&id);
        let pressed = ctx.pressed_id == Some(&id) && hovered;

        let text_color = if selected {
            ctx.brush.SetColor(&p.card_bg);
            ctx.rt.FillRoundedRectangle(&seg_rr, ctx.brush);
            ctx.brush.SetColor(&p.card_border);
            ctx.rt.DrawRoundedRectangle(&seg_rr, ctx.brush, 1.0, None);

            let inset = (seg_w * 0.22).clamp(8.0, 18.0);
            ctx.brush.SetColor(&p.accent);
            ctx.rt.DrawLine(
                D2D_POINT_2F {
                    x: seg_rect.left + inset,
                    y: seg_rect.bottom - 2.0,
                },
                D2D_POINT_2F {
                    x: seg_rect.right - inset,
                    y: seg_rect.bottom - 2.0,
                },
                ctx.brush,
                2.0,
                None,
            );
            &p.text_primary
        } else {
            if pressed {
                ctx.brush.SetColor(&p.pill_hover);
                ctx.rt.FillRoundedRectangle(&seg_rr, ctx.brush);
            } else if hovered {
                ctx.brush.SetColor(&p.pill_bg);
                ctx.rt.FillRoundedRectangle(&seg_rr, ctx.brush);
            }
            if hovered {
                &p.text_primary
            } else {
                &p.text_secondary
            }
        };

        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            label,
            &ctx.fonts.center_fmt,
            text_color,
            seg_rect,
        );
        if ctx.focused_id == Some(&id) {
            draw_focus_ring(ctx, p, seg_rect, 6.0);
        }
        register_hit(ctx, id, seg_rect, action, true);
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_toggle_switch(
    ctx: &mut SceneInput<'_>,
    p: &PopupPalette,
    id: ControlId,
    row_rect: D2D_RECT_F,
    title_rect: D2D_RECT_F,
    subtitle_rect: D2D_RECT_F,
    track_rect: D2D_RECT_F,
    title: &str,
    subtitle: &str,
    checked: bool,
    action: PopupAction,
) {
    let hovered = ctx.hovered_id == Some(&id);
    let pressed = ctx.pressed_id == Some(&id) && hovered;

    if pressed || hovered {
        let row_hl = if pressed {
            composite_over(p.accent_soft, p.card_bg)
        } else {
            p.pill_bg
        };
        ctx.brush.SetColor(&row_hl);
        ctx.rt.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: row_rect,
                radiusX: 6.0,
                radiusY: 6.0,
            },
            ctx.brush,
        );
    }

    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        title,
        &ctx.fonts.body_bold_fmt,
        &p.text_primary,
        title_rect,
    );
    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        subtitle,
        &ctx.fonts.small_fmt,
        &p.text_muted,
        subtitle_rect,
    );

    let radius = (track_rect.bottom - track_rect.top) * 0.5;
    let track_rr = D2D1_ROUNDED_RECT {
        rect: track_rect,
        radiusX: radius,
        radiusY: radius,
    };
    let cy = (track_rect.top + track_rect.bottom) * 0.5;

    if checked {
        let track_col = if pressed {
            &p.accent_pressed
        } else if hovered {
            &p.accent_hover
        } else {
            &p.accent
        };
        ctx.brush.SetColor(track_col);
        ctx.rt.FillRoundedRectangle(&track_rr, ctx.brush);

        ctx.brush.SetColor(&p.accent_text);
        ctx.rt.FillEllipse(
            &D2D1_ELLIPSE {
                point: D2D_POINT_2F {
                    x: track_rect.right - 10.0,
                    y: cy,
                },
                radiusX: 7.0,
                radiusY: 7.0,
            },
            ctx.brush,
        );
    } else {
        let track_col = if pressed {
            &p.pill_pressed
        } else if hovered {
            &p.pill_hover
        } else {
            &p.pill_bg
        };
        ctx.brush.SetColor(track_col);
        ctx.rt.FillRoundedRectangle(&track_rr, ctx.brush);
        ctx.brush.SetColor(&p.card_border);
        ctx.rt.DrawRoundedRectangle(&track_rr, ctx.brush, 1.2, None);

        ctx.brush.SetColor(&p.text_secondary);
        ctx.rt.FillEllipse(
            &D2D1_ELLIPSE {
                point: D2D_POINT_2F {
                    x: track_rect.left + 10.0,
                    y: cy,
                },
                radiusX: 7.0,
                radiusY: 7.0,
            },
            ctx.brush,
        );
    }

    if ctx.focused_id == Some(&id) {
        draw_focus_ring(ctx, p, row_rect, 6.0);
    }
    register_hit(ctx, id, row_rect, action, true);
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_stepper(
    ctx: &mut SceneInput<'_>,
    p: &PopupPalette,
    rect: D2D_RECT_F,
    value_text: &str,
    dec_id: ControlId,
    dec_action: PopupAction,
    inc_id: ControlId,
    inc_action: PopupAction,
) {
    let outer_rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: 6.0,
        radiusY: 6.0,
    };
    ctx.brush.SetColor(&p.track_bg);
    ctx.rt.FillRoundedRectangle(&outer_rr, ctx.brush);
    ctx.brush.SetColor(&p.card_border);
    ctx.rt.DrawRoundedRectangle(&outer_rr, ctx.brush, 1.0, None);

    let pad = 2.0;
    let btn_w = 22.0;
    let dec_rect = D2D_RECT_F {
        left: rect.left + pad,
        top: rect.top + pad,
        right: rect.left + pad + btn_w,
        bottom: rect.bottom - pad,
    };
    let inc_rect = D2D_RECT_F {
        left: rect.right - pad - btn_w,
        top: rect.top + pad,
        right: rect.right - pad,
        bottom: rect.bottom - pad,
    };
    let val_rect = D2D_RECT_F {
        left: dec_rect.right + 2.0,
        top: rect.top,
        right: (inc_rect.left - 2.0).max(dec_rect.right + 3.0),
        bottom: rect.bottom,
    };

    for (id, btn_rect, label, action) in [
        (dec_id, dec_rect, "−", dec_action),
        (inc_id, inc_rect, "+", inc_action),
    ] {
        let hovered = ctx.hovered_id == Some(&id);
        let pressed = ctx.pressed_id == Some(&id) && hovered;
        let (bg, fg) = p.pill_colors(false, hovered, pressed);
        ctx.brush.SetColor(&bg);
        ctx.rt.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: btn_rect,
                radiusX: 4.0,
                radiusY: 4.0,
            },
            ctx.brush,
        );
        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            label,
            &ctx.fonts.center_fmt,
            &fg,
            btn_rect,
        );
        if ctx.focused_id == Some(&id) {
            draw_focus_ring(ctx, p, btn_rect, 4.0);
        }
        register_hit(ctx, id, btn_rect, action, true);
    }

    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        value_text,
        &ctx.fonts.mono_center_fmt,
        &p.text_primary,
        val_rect,
    );
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_action_pill(
    ctx: &mut SceneInput<'_>,
    p: &PopupPalette,
    id: ControlId,
    rect: D2D_RECT_F,
    label: &str,
    enabled: bool,
    danger: bool,
    action: PopupAction,
) {
    if !enabled {
        ctx.brush.SetColor(&p.track_bg);
        ctx.rt.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect,
                radiusX: 6.0,
                radiusY: 6.0,
            },
            ctx.brush,
        );
        let disabled_fg = windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
            a: 0.48,
            ..p.text_muted
        };
        draw_text_clipped(
            ctx.rt,
            ctx.brush,
            ctx.utf16_buf,
            label,
            &ctx.fonts.center_fmt,
            &disabled_fg,
            rect,
        );
        return;
    }

    let hovered = ctx.hovered_id == Some(&id);
    let pressed = ctx.pressed_id == Some(&id) && hovered;
    let (bg, fg) = if danger && (hovered || pressed) && !ctx.theme.is_high_contrast() {
        let danger_bg = composite_over(
            rgba(248, 113, 113, if pressed { 0.30 } else { 0.20 }),
            p.card_bg,
        );
        (danger_bg, p.red)
    } else {
        p.pill_colors(false, hovered, pressed)
    };
    draw_pill_visual(ctx, rect, label, bg, fg);
    if ctx.focused_id == Some(&id) {
        draw_focus_ring(ctx, p, rect, 6.0);
    }
    register_hit(ctx, id, rect, action, true);
}

unsafe fn draw_metric_chip(
    ctx: &mut SceneInput<'_>,
    p: &PopupPalette,
    rect: D2D_RECT_F,
    metric: HwMetric,
    on: bool,
    available: bool,
) {
    let id = ControlId::MetricChip(metric);
    let hovered = ctx.hovered_id == Some(&id);
    let pressed = ctx.pressed_id == Some(&id) && hovered;
    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: 6.0,
        radiusY: 6.0,
    };

    let fg = if on {
        let bg = if pressed {
            &p.accent_pressed
        } else if hovered {
            &p.accent_hover
        } else {
            &p.accent
        };
        ctx.brush.SetColor(bg);
        ctx.rt.FillRoundedRectangle(&rr, ctx.brush);
        p.accent_text
    } else if available {
        let bg = if pressed {
            &p.pill_pressed
        } else if hovered {
            &p.pill_hover
        } else {
            &p.pill_bg
        };
        ctx.brush.SetColor(bg);
        ctx.rt.FillRoundedRectangle(&rr, ctx.brush);
        if hovered && !ctx.theme.is_high_contrast() {
            p.text_primary
        } else {
            p.text_secondary
        }
    } else {
        if pressed {
            ctx.brush.SetColor(&p.pill_pressed);
            ctx.rt.FillRoundedRectangle(&rr, ctx.brush);
        } else if hovered {
            ctx.brush.SetColor(&p.pill_bg);
            ctx.rt.FillRoundedRectangle(&rr, ctx.brush);
        }
        ctx.brush.SetColor(&p.card_border);
        ctx.rt
            .DrawRoundedRectangle(&rr, ctx.brush, 1.0, Some(ctx.dash_stroke));
        if hovered {
            p.text_primary
        } else {
            p.text_muted
        }
    };

    let name_rect = D2D_RECT_F {
        left: rect.left + 8.0,
        top: rect.top,
        right: (rect.right - 20.0).max(rect.left + 9.0),
        bottom: rect.bottom,
    };
    let icon_rect = D2D_RECT_F {
        left: (rect.right - 20.0).max(rect.left + 8.0),
        top: rect.top,
        right: rect.right - 7.0,
        bottom: rect.bottom,
    };

    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        metric.display_name(),
        &ctx.fonts.small_fmt,
        &fg,
        name_rect,
    );
    let badge = if on {
        "✓"
    } else if available {
        "+"
    } else {
        "○"
    };
    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        badge,
        &ctx.fonts.small_right_fmt,
        &fg,
        icon_rect,
    );
    if ctx.focused_id == Some(&id) {
        draw_focus_ring(ctx, p, rect, 6.0);
    }
    register_hit(ctx, id, rect, PopupAction::ToggleMetric(metric), true);
}

unsafe fn draw_static_pill(
    ctx: &mut SceneInput<'_>,
    p: &PopupPalette,
    rect: D2D_RECT_F,
    label: &str,
    active: bool,
) {
    let (bg, fg) = p.pill_colors(active, false, false);
    draw_pill_visual(ctx, rect, label, bg, fg);
}

unsafe fn draw_pill_visual(
    ctx: &mut SceneInput<'_>,
    rect: D2D_RECT_F,
    label: &str,
    bg: windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F,
    fg: windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F,
) {
    ctx.brush.SetColor(&bg);
    ctx.rt.FillRoundedRectangle(
        &D2D1_ROUNDED_RECT {
            rect,
            radiusX: 6.0,
            radiusY: 6.0,
        },
        ctx.brush,
    );
    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        label,
        &ctx.fonts.center_fmt,
        &fg,
        rect,
    );
}

unsafe fn draw_focus_ring(
    ctx: &mut SceneInput<'_>,
    p: &PopupPalette,
    rect: D2D_RECT_F,
    radius: f32,
) {
    let inner_rr = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: rect.left - 0.5,
            top: rect.top - 0.5,
            right: rect.right + 0.5,
            bottom: rect.bottom + 0.5,
        },
        radiusX: radius + 0.5,
        radiusY: radius + 0.5,
    };
    ctx.brush.SetColor(&p.bg);
    ctx.rt.DrawRoundedRectangle(&inner_rr, ctx.brush, 1.5, None);

    let outer_rr = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: rect.left - 1.5,
            top: rect.top - 1.5,
            right: rect.right + 1.5,
            bottom: rect.bottom + 1.5,
        },
        radiusX: radius + 1.5,
        radiusY: radius + 1.5,
    };
    ctx.brush.SetColor(&p.text_primary);
    ctx.rt.DrawRoundedRectangle(&outer_rr, ctx.brush, 2.0, None);
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_pill_button(
    ctx: &mut SceneInput<'_>,
    p: &PopupPalette,
    id: ControlId,
    rect: D2D_RECT_F,
    label: &str,
    active: bool,
    action: PopupAction,
    in_viewport: bool,
) {
    let hovered = ctx.hovered_id == Some(&id);
    let pressed = ctx.pressed_id == Some(&id) && hovered;
    let (bg, fg) = p.pill_colors(active, hovered, pressed);
    draw_pill_visual(ctx, rect, label, bg, fg);
    if ctx.focused_id == Some(&id) {
        draw_focus_ring(ctx, p, rect, 6.0);
    }
    register_hit(ctx, id, rect, action, in_viewport);
}

fn register_hit(
    ctx: &mut SceneInput<'_>,
    id: ControlId,
    rect: D2D_RECT_F,
    action: PopupAction,
    in_viewport: bool,
) {
    ctx.focus_items.push(FocusItem {
        id: id.clone(),
        rect,
        action: action.clone(),
        in_viewport,
    });
    if in_viewport {
        let max_y = ctx.logical_h.max(HEADER_HEIGHT);
        let clipped = D2D_RECT_F {
            left: rect.left,
            top: rect.top.max(HEADER_HEIGHT),
            right: rect.right,
            bottom: rect.bottom.min(max_y),
        };
        if clipped.bottom <= clipped.top {
            return;
        }
        ctx.hit_boxes.push(HitBox {
            id,
            rect: clipped,
            action,
        });
    } else {
        ctx.hit_boxes.push(HitBox { id, rect, action });
    }
}

unsafe fn draw_card(
    rt: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    p: &PopupPalette,
    rect: D2D_RECT_F,
) {
    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: 8.0,
        radiusY: 8.0,
    };
    brush.SetColor(&p.card_bg);
    rt.FillRoundedRectangle(&rr, brush);
    brush.SetColor(&p.card_border);
    rt.DrawRoundedRectangle(&rr, brush, 1.0, None);
}

unsafe fn draw_kv_row(
    ctx: &mut SceneInput<'_>,
    p: &PopupPalette,
    range: (f32, f32),
    y: f32,
    key: &str,
    val: &str,
) {
    draw_kv_row_with_key_width(ctx, p, range, y, key, val, 46.0);
}

unsafe fn draw_kv_row_with_key_width(
    ctx: &mut SceneInput<'_>,
    p: &PopupPalette,
    (x0, x1): (f32, f32),
    y: f32,
    key: &str,
    val: &str,
    key_w: f32,
) {
    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        key,
        &ctx.fonts.body_fmt,
        &p.text_muted,
        D2D_RECT_F {
            left: x0,
            top: y,
            right: (x0 + key_w).min(x1),
            bottom: y + 18.0,
        },
    );
    draw_text_clipped(
        ctx.rt,
        ctx.brush,
        ctx.utf16_buf,
        val,
        &ctx.fonts.mono_fmt,
        &p.text_primary,
        D2D_RECT_F {
            left: (x0 + key_w - 4.0).min(x1),
            top: y,
            right: x1,
            bottom: y + 18.0,
        },
    );
}

fn format_sub_batteries(sub: &SubBatteries) -> String {
    let fmt_part = |label: &str, pct: Option<u8>, chg: bool| -> String {
        match (pct, chg) {
            (Some(p), true) => format!("{}: ⚡{}%", label, p),
            (Some(p), false) => format!("{}: {}%", label, p),
            (None, true) => format!("{}: ⚡", label),
            (None, false) => format!("{}: --", label),
        }
    };
    format!(
        "{}   {}   {}",
        fmt_part("L", sub.left_pct, sub.left_charging),
        fmt_part("R", sub.right_pct, sub.right_charging),
        fmt_part("盒", sub.case_pct, sub.case_charging),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_metrics_zero_when_unclipped_and_exact_when_clipped() {
        let exact = compute_scroll_metrics(534.0, 534.0);
        assert!((exact.max_scroll_y - 0.0).abs() < 0.001);
        assert!((exact.view_h - (534.0 - HEADER_HEIGHT)).abs() < 0.001);
        assert!((exact.body_content_h - (534.0 - HEADER_HEIGHT)).abs() < 0.001);
        assert!((exact.thumb_ratio - 1.0).abs() < 0.001);

        let taller_window = compute_scroll_metrics(364.0, 400.0);
        assert!((taller_window.max_scroll_y - 0.0).abs() < 0.001);
        assert!((taller_window.thumb_ratio - 1.0).abs() < 0.001);

        let clipped = compute_scroll_metrics(600.0, 540.0);
        assert!((clipped.max_scroll_y - (600.0 - 540.0)).abs() < 0.001);
        assert!(
            (clipped.thumb_ratio - ((540.0 - HEADER_HEIGHT) / (600.0 - HEADER_HEIGHT))).abs()
                < 0.001
        );
    }

    #[test]
    fn published_date_formats_first_ten_chars_and_skips_empty() {
        use crate::ui::popup::layout::format_published_date;
        assert_eq!(format_published_date(None), None);
        assert_eq!(format_published_date(Some("   ")), None);
        assert_eq!(
            format_published_date(Some("2026-11-20T12:34:56Z")),
            Some("2026-11-20".to_string())
        );
    }

    #[test]
    fn download_progress_formats_with_and_without_total_bytes() {
        let (text_known, ratio_known) =
            format_download_progress("0.2.0", 520 * 1024, Some(1_153_434));
        assert_eq!(text_known, "正在下载 v0.2.0... 46% (520 KB / 1.1 MB)");
        assert!((ratio_known - 0.4616).abs() < 0.01);

        let (text_zero, ratio_zero) = format_download_progress("0.2.0", 0, None);
        assert_eq!(text_zero, "正在下载 v0.2.0... 0 B");
        assert!((ratio_zero - 0.0).abs() < 0.001);

        let (text_unknown, ratio_unknown) = format_download_progress("0.2.0", 520 * 1024, None);
        assert_eq!(text_unknown, "正在下载 v0.2.0... 520 KB");
        assert!((0.08..=0.92).contains(&ratio_unknown));
    }

    #[test]
    fn popup_fonts_initialize_with_wrapped_error_format() {
        unsafe {
            let dwrite = crate::ui::renderer::shared_dwrite_factory().unwrap();
            let _fonts = PopupFonts::new(&dwrite).unwrap();
        }
    }

    #[test]
    fn settings_v2_renders_all_subtabs_and_themes_without_errors() {
        use crate::config::AppConfig;
        use crate::ui::popup::layout::{POPUP_FIXED_HEIGHT_LOGICAL, POPUP_WIDTH_LOGICAL};
        use crate::ui::renderer::{shared_d2d_factory, shared_dwrite_factory, WindowSurface};
        use crate::ui::theme::HighContrastColors;
        use windows::core::Interface;
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Direct2D::Common::{
            D2D1_ALPHA_MODE_IGNORE, D2D1_PIXEL_FORMAT,
        };
        use windows::Win32::Graphics::Direct2D::{
            D2D1_DASH_STYLE_DASH, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            D2D1_STROKE_STYLE_PROPERTIES,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

        unsafe {
            let d2d = shared_d2d_factory().unwrap();
            let dwrite = shared_dwrite_factory().unwrap();
            let fonts = PopupFonts::new(&dwrite).unwrap();
            let dash_stroke = d2d
                .CreateStrokeStyle(
                    &D2D1_STROKE_STYLE_PROPERTIES {
                        dashStyle: D2D1_DASH_STYLE_DASH,
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();

            let mut surface = WindowSurface::new();
            let w = POPUP_WIDTH_LOGICAL as i32;
            let h = POPUP_FIXED_HEIGHT_LOGICAL as i32;
            let hdc = surface.ensure_size(w, h).unwrap();

            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                ..Default::default()
            };
            let rt = d2d.CreateDCRenderTarget(&props).unwrap();
            let base_rt: ID2D1RenderTarget = rt.cast().unwrap();
            let brush = base_rt
                .CreateSolidColorBrush(&rgba(255, 255, 255, 1.0), None)
                .unwrap();
            let bind_rect = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };

            let hc_theme = UiTheme::HighContrast(HighContrastColors {
                dark: true,
                bg: (0, 0, 0),
                fg: (255, 255, 255),
                accent: (26, 235, 255),
                accent_fg: (0, 0, 0),
                muted: (180, 180, 180),
            });

            let mut snap = MonitorSnapshot::new(AppConfig::default());
            snap.config.hardware_items.push(HwMetric::CpuFreq); // odd (5 items) -> tests empty slot
            let mut hit_boxes = Vec::new();
            let mut focus_items = Vec::new();
            let mut slot_rects = Vec::new();
            let mut utf16_buf = Vec::new();
            let hovered_id = ControlId::Tab(PopupTab::Settings);
            let focused_id = ControlId::ResetDefaultMetrics;

            for theme in [UiTheme::Dark, UiTheme::Light, hc_theme] {
                for sub_tab in [
                    SettingsSubTab::Metrics,
                    SettingsSubTab::Layout,
                    SettingsSubTab::General,
                ] {
                    let mut scroll_y = 0.0;
                    let mut max_scroll_y = 0.0;
                    hit_boxes.clear();
                    focus_items.clear();
                    slot_rects.clear();
                    rt.BindDC(hdc, &bind_rect).unwrap();
                    rt.BeginDraw();
                    let mut scene = SceneInput {
                        rt: &base_rt,
                        brush: &brush,
                        d2d_factory: &d2d,
                        dash_stroke: &dash_stroke,
                        fonts: &fonts,
                        snapshot: &snap,
                        update_status: &UpdateStatus::Idle,
                        self_update_writable: false,
                        theme,
                        active_tab: PopupTab::Settings,
                        settings_sub_tab: sub_tab,
                        selected_metric_idx: Some(2),
                        drag_metric_from: Some(2),
                        drag_metric_active: sub_tab == SettingsSubTab::Metrics,
                        drag_metric_hover: Some(0),
                        cached_autostart: false,
                        has_dwm_border: true,
                        logical_w: POPUP_WIDTH_LOGICAL,
                        logical_h: POPUP_FIXED_HEIGHT_LOGICAL,
                        hovered_id: Some(&hovered_id),
                        pressed_id: None,
                        focused_id: Some(&focused_id),
                        scroll_y: &mut scroll_y,
                        max_scroll_y: &mut max_scroll_y,
                        hit_boxes: &mut hit_boxes,
                        focus_items: &mut focus_items,
                        metric_slot_rects: &mut slot_rects,
                        utf16_buf: &mut utf16_buf,
                    };
                    draw_popup_scene(&mut scene);
                    rt.EndDraw(None, None).unwrap();
                    assert!(!hit_boxes.is_empty());
                    assert!(!focus_items.is_empty());
                    if sub_tab == SettingsSubTab::Metrics {
                        assert_eq!(slot_rects.len(), 5);
                    }
                }
            }
        }
    }

    #[test]
    fn overview_and_about_states_render_cleanly_with_focus_rings() {
        use crate::config::AppConfig;
        use crate::state::HardwareMetrics;
        use crate::ui::popup::layout::{POPUP_FIXED_HEIGHT_LOGICAL, POPUP_WIDTH_LOGICAL};
        use crate::ui::renderer::{shared_d2d_factory, shared_dwrite_factory, WindowSurface};
        use crate::ui::theme::HighContrastColors;
        use windows::core::Interface;
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Direct2D::Common::{
            D2D1_ALPHA_MODE_IGNORE, D2D1_PIXEL_FORMAT,
        };
        use windows::Win32::Graphics::Direct2D::{
            D2D1_DASH_STYLE_DASH, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            D2D1_STROKE_STYLE_PROPERTIES,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

        unsafe {
            let d2d = shared_d2d_factory().unwrap();
            let dwrite = shared_dwrite_factory().unwrap();
            let fonts = PopupFonts::new(&dwrite).unwrap();
            let dash_stroke = d2d
                .CreateStrokeStyle(
                    &D2D1_STROKE_STYLE_PROPERTIES {
                        dashStyle: D2D1_DASH_STYLE_DASH,
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();

            let mut surface = WindowSurface::new();
            let w = POPUP_WIDTH_LOGICAL as i32;
            let h = POPUP_FIXED_HEIGHT_LOGICAL as i32;
            let hdc = surface.ensure_size(w, h).unwrap();

            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                ..Default::default()
            };
            let rt = d2d.CreateDCRenderTarget(&props).unwrap();
            let base_rt: ID2D1RenderTarget = rt.cast().unwrap();
            let brush = base_rt
                .CreateSolidColorBrush(&rgba(255, 255, 255, 1.0), None)
                .unwrap();
            let bind_rect = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            let hc_theme = UiTheme::HighContrast(HighContrastColors {
                dark: false,
                bg: (255, 255, 255),
                fg: (0, 0, 0),
                accent: (0, 90, 180),
                accent_fg: (255, 255, 255),
                muted: (80, 80, 80),
            });

            // 1. Overview with both zones disabled + focus ring on enable button.
            let mut disabled_snap = MonitorSnapshot::new(AppConfig::default());
            disabled_snap.config.set_hardware_enabled(false);
            disabled_snap.config.set_wireless_enabled(false);

            // 2. Overview with unavailable metrics + ACPI thermal.
            let mut acpi_snap = MonitorSnapshot::new(AppConfig::default());
            acpi_snap.hw = HardwareMetrics::unavailable();
            acpi_snap.hw.acpi_temp_c = Some(53.0);

            let mut hit_boxes = Vec::new();
            let mut focus_items = Vec::new();
            let mut slot_rects = Vec::new();
            let mut utf16_buf = Vec::new();

            for (snap, focus) in [
                (&disabled_snap, ControlId::OverviewToggleHardware),
                (&acpi_snap, ControlId::OverviewRefreshWireless),
            ] {
                for theme in [UiTheme::Dark, UiTheme::Light, hc_theme] {
                    let mut scroll_y = 0.0;
                    let mut max_scroll_y = 0.0;
                    hit_boxes.clear();
                    focus_items.clear();
                    slot_rects.clear();
                    rt.BindDC(hdc, &bind_rect).unwrap();
                    rt.BeginDraw();
                    let mut scene = SceneInput {
                        rt: &base_rt,
                        brush: &brush,
                        d2d_factory: &d2d,
                        dash_stroke: &dash_stroke,
                        fonts: &fonts,
                        snapshot: snap,
                        update_status: &UpdateStatus::Idle,
                        self_update_writable: true,
                        theme,
                        active_tab: PopupTab::Overview,
                        settings_sub_tab: SettingsSubTab::Metrics,
                        selected_metric_idx: None,
                        drag_metric_from: None,
                        drag_metric_active: false,
                        drag_metric_hover: None,
                        cached_autostart: false,
                        has_dwm_border: true,
                        logical_w: POPUP_WIDTH_LOGICAL,
                        logical_h: POPUP_FIXED_HEIGHT_LOGICAL,
                        hovered_id: None,
                        pressed_id: None,
                        focused_id: Some(&focus),
                        scroll_y: &mut scroll_y,
                        max_scroll_y: &mut max_scroll_y,
                        hit_boxes: &mut hit_boxes,
                        focus_items: &mut focus_items,
                        metric_slot_rects: &mut slot_rects,
                        utf16_buf: &mut utf16_buf,
                    };
                    draw_popup_scene(&mut scene);
                    rt.EndDraw(None, None).unwrap();
                    assert!(focus_items.iter().any(|item| item.id == focus));
                }
            }

            // 3. About tab: missing SHA-256 disables auto-update button; Downloading shows cancel button.
            let missing_sha = UpdateStatus::UpdateAvailable {
                current: "0.1.0".into(),
                latest: "0.2.0".into(),
                release_url: "https://github.com/AmlitaEnan/AirMonitor/releases/tag/v0.2.0".into(),
                published_at: None,
                download_url: Some(
                    "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/airmonitor.exe"
                        .into(),
                ),
                asset_name: Some("airmonitor.exe".into()),
                asset_size: Some(1_100_000),
                sha256_url: None,
            };
            let downloading = UpdateStatus::Downloading {
                latest: "0.2.0".into(),
                downloaded_bytes: 256 * 1024,
                total_bytes: Some(1024 * 1024),
            };
            for (status, expected_id, forbidden_id) in [
                (
                    &missing_sha,
                    ControlId::AboutManualDownload,
                    Some(ControlId::AboutAutoUpdate),
                ),
                (&downloading, ControlId::AboutCancelUpdate, None),
            ] {
                let mut scroll_y = 0.0;
                let mut max_scroll_y = 0.0;
                hit_boxes.clear();
                focus_items.clear();
                slot_rects.clear();
                rt.BindDC(hdc, &bind_rect).unwrap();
                rt.BeginDraw();
                let mut scene = SceneInput {
                    rt: &base_rt,
                    brush: &brush,
                    d2d_factory: &d2d,
                    dash_stroke: &dash_stroke,
                    fonts: &fonts,
                    snapshot: &acpi_snap,
                    update_status: status,
                    self_update_writable: true,
                    theme: UiTheme::Dark,
                    active_tab: PopupTab::About,
                    settings_sub_tab: SettingsSubTab::Metrics,
                    selected_metric_idx: None,
                    drag_metric_from: None,
                    drag_metric_active: false,
                    drag_metric_hover: None,
                    cached_autostart: false,
                    has_dwm_border: true,
                    logical_w: POPUP_WIDTH_LOGICAL,
                    logical_h: POPUP_FIXED_HEIGHT_LOGICAL,
                    hovered_id: None,
                    pressed_id: None,
                    focused_id: Some(&expected_id),
                    scroll_y: &mut scroll_y,
                    max_scroll_y: &mut max_scroll_y,
                    hit_boxes: &mut hit_boxes,
                    focus_items: &mut focus_items,
                    metric_slot_rects: &mut slot_rects,
                    utf16_buf: &mut utf16_buf,
                };
                draw_popup_scene(&mut scene);
                rt.EndDraw(None, None).unwrap();
                assert!(focus_items.iter().any(|item| item.id == expected_id));
                if let Some(forbidden) = forbidden_id {
                    assert!(!focus_items.iter().any(|item| item.id == forbidden));
                }
            }
        }
    }
}
