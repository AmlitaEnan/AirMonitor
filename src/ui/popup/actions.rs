use crate::config::{AppConfig, HwMetric};
use crate::ui::popup::layout::{PopupAction, PopupTab, SettingsSubTab};
use crate::ui::tray::{is_autostart_enabled, set_autostart};
use crate::ui::SnapshotStore;
use crate::updater::UpdateStatus;

pub(crate) enum ActionEffect {
    None,
    SwitchTab(PopupTab),
    SwitchSettingsSubTab(SettingsSubTab),
    SelectMetricSlot(usize),
    RequestWirelessRefresh,
    CheckForUpdates,
    StartDownloadUpdate,
    OpenUrl(String),
    OpenConfig,
    ConfigMutated {
        save_error: Option<String>,
        autostart_state: Option<bool>,
        select_metric_idx: Option<usize>,
    },
    ErrorOnly(String),
}

pub(crate) fn is_update_in_progress(status: &UpdateStatus) -> bool {
    matches!(
        status,
        UpdateStatus::Checking
            | UpdateStatus::Downloading { .. }
            | UpdateStatus::Verifying { .. }
            | UpdateStatus::Applying { .. }
    )
}

pub(crate) fn try_begin_update_check(status: &mut UpdateStatus) -> bool {
    if is_update_in_progress(status) {
        return false;
    }
    *status = UpdateStatus::Checking;
    true
}

pub(crate) struct DownloadTarget {
    pub latest: String,
    pub download_url: String,
    pub asset_name: Option<String>,
    pub sha256_url: Option<String>,
}

pub(crate) fn try_begin_download_update(status: &mut UpdateStatus) -> Option<DownloadTarget> {
    match status {
        UpdateStatus::UpdateAvailable {
            latest,
            download_url: Some(url),
            asset_name,
            asset_size,
            sha256_url,
            ..
        } => {
            let target = DownloadTarget {
                latest: latest.clone(),
                download_url: url.clone(),
                asset_name: asset_name.clone(),
                sha256_url: sha256_url.clone(),
            };
            *status = UpdateStatus::Downloading {
                latest: target.latest.clone(),
                downloaded_bytes: 0,
                total_bytes: *asset_size,
            };
            Some(target)
        }
        _ => None,
    }
}

pub(crate) fn apply_popup_action(action: PopupAction, state: &SnapshotStore) -> ActionEffect {
    match action {
        PopupAction::SwitchTab(tab) => ActionEffect::SwitchTab(tab),
        PopupAction::SwitchSettingsSubTab(sub_tab) => ActionEffect::SwitchSettingsSubTab(sub_tab),
        PopupAction::SelectMetricSlot(idx) => ActionEffect::SelectMetricSlot(idx),
        PopupAction::RefreshWireless => ActionEffect::RequestWirelessRefresh,
        PopupAction::CheckForUpdates => ActionEffect::CheckForUpdates,
        PopupAction::StartDownloadUpdate => ActionEffect::StartDownloadUpdate,
        PopupAction::OpenUrl(url) => ActionEffect::OpenUrl(url),
        PopupAction::OpenConfig => ActionEffect::OpenConfig,
        PopupAction::ToggleHiddenDevice(id) => mutate_and_save(state, |cfg| {
            toggle_hidden_device(&mut cfg.hidden_devices, &id);
            (true, None)
        }),
        PopupAction::ToggleMetric(metric) => mutate_and_save(state, |cfg| {
            let before_len = cfg.hardware_items.len();
            toggle_hw_metric(&mut cfg.hardware_items, metric);
            let select = if cfg.hardware_items.len() > before_len {
                Some(cfg.hardware_items.len() - 1)
            } else {
                None
            };
            (true, select)
        }),
        PopupAction::SwapMetricSlots(a, b) => mutate_and_save(state, |cfg| {
            if swap_metric_slots(&mut cfg.hardware_items, a, b) {
                (true, Some(b))
            } else {
                (false, None)
            }
        }),
        PopupAction::MoveSelectedMetricLeft(idx) => mutate_and_save(state, |cfg| {
            if let Some(target) = move_metric_left(&mut cfg.hardware_items, idx) {
                (true, Some(target))
            } else {
                (false, None)
            }
        }),
        PopupAction::SwapSelectedMetricRow(idx) => mutate_and_save(state, |cfg| {
            if let Some(target) = swap_metric_row(&mut cfg.hardware_items, idx) {
                (true, Some(target))
            } else {
                (false, None)
            }
        }),
        PopupAction::MoveSelectedMetricRight(idx) => mutate_and_save(state, |cfg| {
            if let Some(target) = move_metric_right(&mut cfg.hardware_items, idx) {
                (true, Some(target))
            } else {
                (false, None)
            }
        }),
        PopupAction::RemoveMetricSlot(idx) => mutate_and_save(state, |cfg| {
            if remove_metric_at(&mut cfg.hardware_items, idx) {
                let next_sel = if cfg.hardware_items.is_empty() {
                    None
                } else {
                    Some(idx.min(cfg.hardware_items.len() - 1))
                };
                (true, next_sel)
            } else {
                (false, None)
            }
        }),
        PopupAction::ResetDefaultMetrics => mutate_and_save(state, |cfg| {
            let changed = reset_default_metrics(&mut cfg.hardware_items);
            (changed, Some(2))
        }),
        PopupAction::SetLayout(mode, order) => mutate_and_save(state, |cfg| {
            cfg.layout_mode = mode;
            if let Some(o) = order {
                cfg.combined_order = o;
            }
            (true, None)
        }),
        PopupAction::SetAnchor {
            is_devices,
            is_combined,
            anchor,
        } => mutate_and_save(state, |cfg| {
            if is_combined {
                cfg.combined_zone.anchor = anchor;
            } else if is_devices {
                cfg.devices_zone.anchor = anchor;
            } else {
                cfg.hardware_zone.anchor = anchor;
            }
            (true, None)
        }),
        PopupAction::StepOffset {
            is_devices,
            is_combined,
            delta,
        } => mutate_and_save(state, |cfg| {
            let zone = if is_combined {
                &mut cfg.combined_zone
            } else if is_devices {
                &mut cfg.devices_zone
            } else {
                &mut cfg.hardware_zone
            };
            zone.offset_x = step_offset_x(zone.offset_x, delta);
            (true, None)
        }),
        PopupAction::ResetZoneOffset {
            is_devices,
            is_combined,
        } => mutate_and_save(state, |cfg| {
            let zone = if is_combined {
                &mut cfg.combined_zone
            } else if is_devices {
                &mut cfg.devices_zone
            } else {
                &mut cfg.hardware_zone
            };
            if zone.offset_x != 0 {
                zone.offset_x = 0;
                (true, None)
            } else {
                (false, None)
            }
        }),
        PopupAction::StepLowThreshold(delta) => mutate_and_save(state, |cfg| {
            cfg.low_battery_threshold =
                step_low_battery_threshold(cfg.low_battery_threshold, delta);
            (true, None)
        }),
        PopupAction::ToggleHideUnknownBatteryDevices => mutate_and_save(state, |cfg| {
            cfg.hide_unknown_battery_devices = !cfg.hide_unknown_battery_devices;
            (true, None)
        }),
        PopupAction::StepHwPollInterval(delta_step) => mutate_and_save(state, |cfg| {
            let next = step_hw_poll_interval_ms(cfg.hw_poll_interval_ms, delta_step);
            if next != cfg.hw_poll_interval_ms {
                cfg.hw_poll_interval_ms = next;
                (true, None)
            } else {
                (false, None)
            }
        }),
        PopupAction::ToggleEnableExternalSensors => mutate_and_save(state, |cfg| {
            cfg.enable_external_sensors = !cfg.enable_external_sensors;
            (true, None)
        }),
        PopupAction::ToggleHideUnavailableMetrics => mutate_and_save(state, |cfg| {
            cfg.hide_unavailable_metrics = !cfg.hide_unavailable_metrics;
            (true, None)
        }),
        PopupAction::ToggleAutostart => {
            let target = !is_autostart_enabled();
            if let Err(err) = set_autostart(target) {
                return ActionEffect::ErrorOnly(err);
            }
            let (changed, save_error) = state.update_and_save_config(|cfg| {
                cfg.autostart = target;
                true
            });
            if changed {
                ActionEffect::ConfigMutated {
                    save_error,
                    autostart_state: Some(target),
                    select_metric_idx: None,
                }
            } else if let Some(err) = save_error {
                ActionEffect::ErrorOnly(err)
            } else {
                ActionEffect::None
            }
        }
    }
}

fn mutate_and_save<F>(state: &SnapshotStore, f: F) -> ActionEffect
where
    F: FnOnce(&mut AppConfig) -> (bool, Option<usize>),
{
    let mut select_metric_idx = None;
    let (changed, save_error) = state.update_and_save_config(|cfg| {
        let (did_change, sel) = f(cfg);
        select_metric_idx = sel;
        did_change
    });
    if changed {
        ActionEffect::ConfigMutated {
            save_error,
            autostart_state: None,
            select_metric_idx,
        }
    } else if let Some(err) = save_error {
        ActionEffect::ErrorOnly(err)
    } else if let Some(idx) = select_metric_idx {
        ActionEffect::SelectMetricSlot(idx)
    } else {
        ActionEffect::None
    }
}

pub fn toggle_hw_metric(items: &mut Vec<HwMetric>, metric: HwMetric) {
    if let Some(pos) = items.iter().position(|&m| m == metric) {
        items.remove(pos);
    } else {
        items.push(metric);
    }
}

pub fn swap_metric_slots(items: &mut [HwMetric], a: usize, b: usize) -> bool {
    if a >= items.len() || b >= items.len() || a == b {
        return false;
    }
    items.swap(a, b);
    true
}

pub fn move_metric_left(items: &mut [HwMetric], idx: usize) -> Option<usize> {
    if idx >= items.len() || idx < 2 {
        return None;
    }
    let target = idx - 2;
    items.swap(idx, target);
    Some(target)
}

pub fn swap_metric_row(items: &mut [HwMetric], idx: usize) -> Option<usize> {
    if idx >= items.len() {
        return None;
    }
    let target = idx ^ 1;
    if target >= items.len() {
        return None;
    }
    items.swap(idx, target);
    Some(target)
}

pub fn move_metric_right(items: &mut [HwMetric], idx: usize) -> Option<usize> {
    let len = items.len();
    if idx >= len {
        return None;
    }
    let target = if idx + 2 < len {
        idx + 2
    } else if idx % 2 == 1 && idx + 1 < len {
        idx + 1
    } else {
        return None;
    };
    items.swap(idx, target);
    Some(target)
}

pub fn remove_metric_at(items: &mut Vec<HwMetric>, idx: usize) -> bool {
    if idx >= items.len() {
        return false;
    }
    items.remove(idx);
    true
}

pub fn reset_default_metrics(items: &mut Vec<HwMetric>) -> bool {
    let defaults = [
        HwMetric::NetUp,
        HwMetric::NetDown,
        HwMetric::CpuUsage,
        HwMetric::MemUsage,
    ];
    if items.as_slice() == defaults {
        return false;
    }
    items.clear();
    items.extend_from_slice(&defaults);
    true
}

pub fn toggle_hidden_device(hidden: &mut Vec<String>, device_id: &str) -> bool {
    if let Some(pos) = hidden.iter().position(|id| id == device_id) {
        hidden.remove(pos);
        false
    } else {
        hidden.push(device_id.to_string());
        true
    }
}

pub fn step_offset_x(offset_x: i32, delta: i32) -> i32 {
    offset_x
        .saturating_add(delta)
        .clamp(0, crate::config::MAX_OFFSET_X)
}

pub fn step_low_battery_threshold(threshold: u8, delta: i8) -> u8 {
    let next = (threshold as i16 + delta as i16).clamp(5, 50);
    next as u8
}

pub const HW_POLL_INTERVAL_STEPS_MS: &[u64] = &[500, 1000, 2000, 3000, 5000];

pub fn step_hw_poll_interval_ms(current_ms: u64, delta_step: i8) -> u64 {
    if delta_step > 0 {
        HW_POLL_INTERVAL_STEPS_MS
            .iter()
            .copied()
            .find(|&step| step > current_ms)
            .unwrap_or(*HW_POLL_INTERVAL_STEPS_MS.last().unwrap())
    } else if delta_step < 0 {
        HW_POLL_INTERVAL_STEPS_MS
            .iter()
            .rev()
            .copied()
            .find(|&step| step < current_ms)
            .unwrap_or(HW_POLL_INTERVAL_STEPS_MS[0])
    } else {
        current_ms.clamp(
            HW_POLL_INTERVAL_STEPS_MS[0],
            *HW_POLL_INTERVAL_STEPS_MS.last().unwrap(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hw_metric_toggle_and_2d_reorder() {
        let mut items = vec![
            HwMetric::NetUp,
            HwMetric::NetDown,
            HwMetric::CpuUsage,
            HwMetric::MemUsage,
            HwMetric::CpuFreq,
        ];

        // Swap two slots directly (drag-and-drop).
        assert!(swap_metric_slots(&mut items, 0, 3));
        assert_eq!(items[0], HwMetric::MemUsage);
        assert_eq!(items[3], HwMetric::NetUp);
        assert!(!swap_metric_slots(&mut items, 2, 2));
        assert!(!swap_metric_slots(&mut items, 0, 99));

        // Reset defaults restores [NetUp, NetDown, CpuUsage, MemUsage].
        assert!(reset_default_metrics(&mut items));
        assert!(!reset_default_metrics(&mut items));
        assert_eq!(
            items,
            vec![
                HwMetric::NetUp,
                HwMetric::NetDown,
                HwMetric::CpuUsage,
                HwMetric::MemUsage,
            ]
        );

        // 2D left/right/row swap on 5 items:
        // col0: [0:NetUp, 1:NetDown], col1: [2:CpuUsage, 3:MemUsage], col2: [4:CpuFreq, empty]
        toggle_hw_metric(&mut items, HwMetric::CpuFreq);
        assert_eq!(move_metric_left(&mut items, 0), None);
        assert_eq!(move_metric_left(&mut items, 1), None);
        assert_eq!(move_metric_left(&mut items, 2), Some(0));
        assert_eq!(items[0], HwMetric::CpuUsage);
        assert_eq!(items[2], HwMetric::NetUp);

        // Row swap: 0 <-> 1, 2 <-> 3, 4 has no partner when len == 5.
        assert_eq!(swap_metric_row(&mut items, 0), Some(1));
        assert_eq!(items[1], HwMetric::CpuUsage);
        assert_eq!(swap_metric_row(&mut items, 4), None);

        // Right move: 1 -> 3 (idx + 2 < 5), 3 -> 4 (odd idx % 2 == 1 && idx + 1 < 5), 4 -> None.
        assert_eq!(move_metric_right(&mut items, 1), Some(3));
        assert_eq!(items[3], HwMetric::CpuUsage);
        assert_eq!(move_metric_right(&mut items, 3), Some(4));
        assert_eq!(items[4], HwMetric::CpuUsage);
        assert_eq!(move_metric_right(&mut items, 4), None);
        assert_eq!(move_metric_right(&mut items, 2), Some(4));

        // Remove slot at index.
        assert!(!remove_metric_at(&mut items, 10));
        assert!(remove_metric_at(&mut items, 4));
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn settings_v2_actions_mutate_snapshot_store() {
        crate::config::set_read_only_mode(true);
        let store = SnapshotStore::new(AppConfig::default());

        assert!(matches!(
            apply_popup_action(
                PopupAction::SwitchSettingsSubTab(SettingsSubTab::Layout),
                &store
            ),
            ActionEffect::SwitchSettingsSubTab(SettingsSubTab::Layout)
        ));
        assert!(matches!(
            apply_popup_action(PopupAction::SelectMetricSlot(1), &store),
            ActionEffect::SelectMetricSlot(1)
        ));

        assert!(matches!(
            apply_popup_action(PopupAction::SwapMetricSlots(0, 2), &store),
            ActionEffect::ConfigMutated {
                select_metric_idx: Some(2),
                ..
            }
        ));
        assert_eq!(
            store.read_snapshot().unwrap().config.hardware_items,
            vec![
                HwMetric::CpuUsage,
                HwMetric::NetDown,
                HwMetric::NetUp,
                HwMetric::MemUsage,
            ]
        );

        assert!(matches!(
            apply_popup_action(PopupAction::MoveSelectedMetricLeft(2), &store),
            ActionEffect::ConfigMutated {
                select_metric_idx: Some(0),
                ..
            }
        ));
        assert!(matches!(
            apply_popup_action(PopupAction::SwapSelectedMetricRow(0), &store),
            ActionEffect::ConfigMutated {
                select_metric_idx: Some(1),
                ..
            }
        ));
        assert!(matches!(
            apply_popup_action(PopupAction::MoveSelectedMetricRight(1), &store),
            ActionEffect::ConfigMutated {
                select_metric_idx: Some(3),
                ..
            }
        ));
        assert!(matches!(
            apply_popup_action(PopupAction::RemoveMetricSlot(3), &store),
            ActionEffect::ConfigMutated {
                select_metric_idx: Some(2),
                ..
            }
        ));
        assert_eq!(
            store.read_snapshot().unwrap().config.hardware_items.len(),
            3
        );

        assert!(matches!(
            apply_popup_action(PopupAction::ResetDefaultMetrics, &store),
            ActionEffect::ConfigMutated {
                select_metric_idx: Some(2),
                ..
            }
        ));
        assert_eq!(
            store.read_snapshot().unwrap().config.hardware_items,
            vec![
                HwMetric::NetUp,
                HwMetric::NetDown,
                HwMetric::CpuUsage,
                HwMetric::MemUsage,
            ]
        );

        assert!(matches!(
            apply_popup_action(
                PopupAction::ResetZoneOffset {
                    is_devices: false,
                    is_combined: true
                },
                &store
            ),
            ActionEffect::ConfigMutated { .. }
        ));
        assert_eq!(
            store.read_snapshot().unwrap().config.combined_zone.offset_x,
            0
        );
        // Resetting when already 0 returns None without redundant save.
        assert!(matches!(
            apply_popup_action(
                PopupAction::ResetZoneOffset {
                    is_devices: false,
                    is_combined: true
                },
                &store
            ),
            ActionEffect::None
        ));
    }

    #[test]
    fn hidden_device_and_steppers() {
        let mut hidden = Vec::new();
        assert!(toggle_hidden_device(&mut hidden, "dev1"));
        assert_eq!(hidden, vec!["dev1"]);
        assert!(!toggle_hidden_device(&mut hidden, "dev1"));
        assert!(hidden.is_empty());

        assert_eq!(step_offset_x(2, -4), 0);
        assert_eq!(step_offset_x(8, 4), 12);
        assert_eq!(step_low_battery_threshold(5, -5), 5);
        assert_eq!(step_low_battery_threshold(50, 5), 50);
        assert_eq!(step_low_battery_threshold(20, 5), 25);

        assert_eq!(step_hw_poll_interval_ms(500, -1), 500);
        assert_eq!(step_hw_poll_interval_ms(500, 1), 1000);
        assert_eq!(step_hw_poll_interval_ms(1000, 1), 2000);
        assert_eq!(step_hw_poll_interval_ms(2000, 1), 3000);
        assert_eq!(step_hw_poll_interval_ms(3000, 1), 5000);
        assert_eq!(step_hw_poll_interval_ms(5000, 1), 5000);
        assert_eq!(step_hw_poll_interval_ms(5000, -1), 3000);
        assert_eq!(step_hw_poll_interval_ms(1500, 1), 2000);
        assert_eq!(step_hw_poll_interval_ms(1500, -1), 1000);
    }

    #[test]
    fn new_general_settings_actions_mutate_snapshot_store() {
        crate::config::set_read_only_mode(true);
        let store = SnapshotStore::new(AppConfig::default());

        assert!(matches!(
            apply_popup_action(PopupAction::StepHwPollInterval(1), &store),
            ActionEffect::ConfigMutated { .. }
        ));
        assert_eq!(
            store.read_snapshot().unwrap().config.hw_poll_interval_ms,
            2000
        );

        assert!(matches!(
            apply_popup_action(PopupAction::StepHwPollInterval(-1), &store),
            ActionEffect::ConfigMutated { .. }
        ));
        assert_eq!(
            store.read_snapshot().unwrap().config.hw_poll_interval_ms,
            1000
        );

        assert!(matches!(
            apply_popup_action(PopupAction::ToggleEnableExternalSensors, &store),
            ActionEffect::ConfigMutated { .. }
        ));
        assert!(
            !store
                .read_snapshot()
                .unwrap()
                .config
                .enable_external_sensors
        );

        assert!(matches!(
            apply_popup_action(PopupAction::ToggleHideUnavailableMetrics, &store),
            ActionEffect::ConfigMutated { .. }
        ));
        assert!(
            store
                .read_snapshot()
                .unwrap()
                .config
                .hide_unavailable_metrics
        );
    }

    #[test]
    fn about_actions_map_without_mutating_config() {
        crate::config::set_read_only_mode(true);
        let store = SnapshotStore::new(AppConfig::default());
        let rev_before = store.read_versioned().unwrap().config_rev;

        assert!(matches!(
            apply_popup_action(PopupAction::SwitchTab(PopupTab::About), &store),
            ActionEffect::SwitchTab(PopupTab::About)
        ));
        assert!(matches!(
            apply_popup_action(PopupAction::CheckForUpdates, &store),
            ActionEffect::CheckForUpdates
        ));
        assert!(matches!(
            apply_popup_action(PopupAction::StartDownloadUpdate, &store),
            ActionEffect::StartDownloadUpdate
        ));
        assert!(matches!(
            apply_popup_action(
                PopupAction::OpenUrl(crate::updater::REPO_URL.to_string()),
                &store
            ),
            ActionEffect::OpenUrl(ref u) if u == crate::updater::REPO_URL
        ));
        assert!(matches!(
            apply_popup_action(PopupAction::OpenConfig, &store),
            ActionEffect::OpenConfig
        ));
        assert_eq!(store.read_versioned().unwrap().config_rev, rev_before);
    }

    #[test]
    fn update_transitions_guard_against_reentrancy() {
        for busy in [
            UpdateStatus::Checking,
            UpdateStatus::Downloading {
                latest: "0.2.0".into(),
                downloaded_bytes: 100,
                total_bytes: Some(1000),
            },
            UpdateStatus::Verifying {
                latest: "0.2.0".into(),
            },
            UpdateStatus::Applying {
                latest: "0.2.0".into(),
            },
        ] {
            assert!(is_update_in_progress(&busy));
            let mut s1 = busy.clone();
            assert!(!try_begin_update_check(&mut s1));
            assert_eq!(s1, busy);

            let mut s2 = busy.clone();
            assert!(try_begin_download_update(&mut s2).is_none());
            assert_eq!(s2, busy);
        }

        let mut idle = UpdateStatus::Idle;
        assert!(try_begin_download_update(&mut idle).is_none());
        assert!(try_begin_update_check(&mut idle));
        assert_eq!(idle, UpdateStatus::Checking);

        let mut avail_no_url = UpdateStatus::UpdateAvailable {
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            release_url: crate::updater::RELEASES_URL.into(),
            published_at: None,
            download_url: None,
            asset_name: None,
            asset_size: None,
            sha256_url: None,
        };
        assert!(try_begin_download_update(&mut avail_no_url).is_none());
        assert!(try_begin_update_check(&mut avail_no_url));
        assert_eq!(avail_no_url, UpdateStatus::Checking);

        let mut avail_with_url = UpdateStatus::UpdateAvailable {
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            release_url: crate::updater::RELEASES_URL.into(),
            published_at: Some("2026-11-20T12:00:00Z".into()),
            download_url: Some(
                "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/airmonitor.exe"
                    .into(),
            ),
            asset_name: Some("airmonitor.exe".into()),
            asset_size: Some(1_100_000),
            sha256_url: Some(
                "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/SHA256SUMS.txt"
                    .into(),
            ),
        };
        let target = try_begin_download_update(&mut avail_with_url).unwrap();
        assert_eq!(target.latest, "0.2.0");
        assert_eq!(target.asset_name.as_deref(), Some("airmonitor.exe"));
        assert_eq!(
            avail_with_url,
            UpdateStatus::Downloading {
                latest: "0.2.0".into(),
                downloaded_bytes: 0,
                total_bytes: Some(1_100_000),
            }
        );
        // Once Downloading, neither check nor download can re-enter.
        assert!(!try_begin_update_check(&mut avail_with_url));
        assert!(try_begin_download_update(&mut avail_with_url).is_none());
    }
}
