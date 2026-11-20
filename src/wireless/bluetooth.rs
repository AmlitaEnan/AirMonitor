use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{w, GUID, PCWSTR};
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementBytePattern, BluetoothLEAdvertisementDataTypes,
    BluetoothLEAdvertisementFilter, BluetoothLEAdvertisementReceivedEventArgs,
    BluetoothLEAdvertisementWatcher, BluetoothLEAdvertisementWatcherStatus,
    BluetoothLEAdvertisementWatcherStoppedEventArgs, BluetoothLEScanningMode,
};
use windows::Foundation::{EventRegistrationToken, TypedEventHandler};
use windows::Storage::Streams::{DataReader, DataWriter};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDevicePropertyW, DIGCF_ALLCLASSES,
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA, SP_DEVINFO_DATA,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_FriendlyName, DEVPKEY_Device_IsPresent, DEVPKEY_NAME, DEVPROP_TYPE_BOOLEAN,
    DEVPROP_TYPE_BYTE, DEVPROP_TYPE_STRING, DEVPROP_TYPE_UINT32,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::state::{DeviceKind, SubBatteries, Transport, WirelessDevice};

// DEVPKEY_Bluetooth_Battery: {104EA319-6EE2-4701-BD47-8DDBF425BBE5}, pid 2
const GUID_BT_BATTERY: u128 = 0x104EA319_6EE2_4701_BD47_8DDBF425BBE5;
const PID_BT_BATTERY: u32 = 2;

// DEVPKEY_Bluetooth_DeviceAddress: {A35996AB-11CF-4935-8B61-A6761081ECDF}, pid 1
const GUID_BT_ADDRESS: u128 = 0xA35996AB_11CF_4935_8B61_A6761081ECDF;
const PID_BT_ADDRESS: u32 = 1;

// DEVPKEY_Device_IsConnected (Windows PnP): {83DA6326-97A6-4088-9453-A1923F573B29}, pid 15
const GUID_PNP_CONNECTED: u128 = 0x83DA6326_97A6_4088_9453_A1923F573B29;
const PID_PNP_CONNECTED: u32 = 15;

// GUID_BTHPORT_DEVICE_INTERFACE: {0850302A-B344-4FDA-9BE9-90576B8D46F0}
const GUID_BTHPORT_DEVICE_INTERFACE: u128 = 0x0850302A_B344_4FDA_9BE9_90576B8D46F0;
const NO_BT_RADIO_STATUS: &str = "No active Bluetooth radio detected (WinRT BLE skipped)";

const APPLE_COMPANY_ID: u16 = 0x004C;
const CONTINUITY_PROXIMITY_PAIRING: u8 = 0x07;
const PROXIMITY_PAIRING_LEN_FIELD: u8 = 0x19;
const AIRPODS_PACKET_LEN: usize = 27;
const AIRPODS_STALE_SECS: u64 = 90;
const AIRPODS_MIN_RSSI_DBM: i16 = -60;
const WATCHER_RETRY_COOLDOWN: Duration = Duration::from_secs(5);
const BT_OFFLINE_MISS_THRESHOLD: u32 = 3;
const BT_CACHE_GRACE_SECS: u64 = 180;

#[derive(Debug, Clone)]
struct CachedPnpBtEntry {
    device: WirelessDevice,
    last_seen: Instant,
    last_battery_seen: Option<Instant>,
    miss_streak: u32,
}

#[derive(Debug, Clone)]
struct RawPnpBtNode {
    instance_id: String,
    friendly: String,
    bt_mac: Option<String>,
    battery_pct: Option<u8>,
    connected: bool,
    is_present: bool,
    is_primary_node: bool,
}

#[derive(Debug, Clone)]
struct PnpBtRecord {
    key: String,
    instance_id: String,
    name: String,
    battery_pct: Option<u8>,
    battery_from_present: bool,
    connected: bool,
    is_present: bool,
    is_primary_node: bool,
    primary_connected: Option<bool>,
}

#[derive(Debug, Clone)]
struct AirPodsDecoded {
    model_id: u16,
    model_name: &'static str,
    sub_batteries: SubBatteries,
    overall_pct: Option<u8>,
    charging: bool,
}

#[derive(Debug, Clone)]
struct AirPodsCacheEntry {
    id: String,
    model_name: String,
    sub_batteries: SubBatteries,
    overall_pct: Option<u8>,
    charging: bool,
    rssi: i16,
    raw_hex: String,
    last_seen: Instant,
}

#[derive(Default)]
struct BleSharedState {
    airpods: HashMap<String, AirPodsCacheEntry>,
    apple_packets_seen: u64,
    proximity_packets_seen: u64,
    watcher_stopped: bool,
    last_error: Option<String>,
}

struct DevInfoGuard(HDEVINFO);

impl Drop for DevInfoGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = SetupDiDestroyDeviceInfoList(self.0);
            }
        }
    }
}

/// Pure Win32 PnP check for at least one PRESENT local Bluetooth radio interface
/// (`GUID_BTHPORT_DEVICE_INTERFACE`), without initializing COM or WinRT.
pub fn has_present_bluetooth_radio() -> bool {
    let bthport_guid = GUID::from_u128(GUID_BTHPORT_DEVICE_INTERFACE);
    unsafe {
        let Ok(dev_info) = SetupDiGetClassDevsW(
            Some(&bthport_guid),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        ) else {
            return false;
        };
        if dev_info.is_invalid() {
            return false;
        }
        let _guard = DevInfoGuard(dev_info);

        let mut iface_data = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        SetupDiEnumDeviceInterfaces(dev_info, None, &bthport_guid, 0, &mut iface_data).is_ok()
    }
}

pub struct BluetoothMonitor {
    ble_state: Arc<Mutex<BleSharedState>>,
    pnp_cache: HashMap<String, CachedPnpBtEntry>,
    watcher: Option<BluetoothLEAdvertisementWatcher>,
    watcher_token: Option<EventRegistrationToken>,
    watcher_stopped_token: Option<EventRegistrationToken>,
    watcher_started: bool,
    last_watcher_attempt: Option<Instant>,
    stop_flag: Arc<AtomicBool>,
}

// WinRT Agile watcher reference + Mutex-guarded shared cache.
unsafe impl Send for BluetoothMonitor {}
unsafe impl Sync for BluetoothMonitor {}

impl Default for BluetoothMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl BluetoothMonitor {
    pub fn new() -> Self {
        Self::with_stop_flag(Arc::new(AtomicBool::new(false)))
    }

    pub fn with_stop_flag(stop_flag: Arc<AtomicBool>) -> Self {
        let mut monitor = Self::new_unstarted(stop_flag);
        monitor.ensure_ble_watcher();
        monitor
    }

    fn new_unstarted(stop_flag: Arc<AtomicBool>) -> Self {
        let ble_state = Arc::new(Mutex::new(BleSharedState::default()));
        Self {
            ble_state,
            pnp_cache: HashMap::new(),
            watcher: None,
            watcher_token: None,
            watcher_stopped_token: None,
            watcher_started: false,
            last_watcher_attempt: None,
            stop_flag,
        }
    }

    pub fn poll(&mut self) -> Vec<WirelessDevice> {
        if self.stop_flag.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let radio_present = has_present_bluetooth_radio();
        self.ensure_ble_watcher_with_radio(radio_present);
        let now = Instant::now();

        // 1. PnP Bluetooth devices (classic BT & paired BLE battery via Windows PnP, never touching audio endpoints)
        let mut out = poll_pnp_bt_with_radio(
            &mut self.pnp_cache,
            radio_present,
            now,
            enumerate_pnp_bt_devices,
        );

        // 2. Passive Apple AirPods BLE advertisements
        if let Ok(mut guard) = self.ble_state.lock() {
            guard.airpods.retain(|_, v| {
                now.saturating_duration_since(v.last_seen).as_secs() <= AIRPODS_STALE_SECS
            });
            for entry in guard.airpods.values() {
                out.push(WirelessDevice {
                    id: entry.id.clone(),
                    name: entry.model_name.clone(),
                    kind: DeviceKind::Earbuds,
                    transport: Transport::BleBroadcast,
                    battery_pct: entry.overall_pct,
                    charging: entry.charging,
                    sub_batteries: Some(entry.sub_batteries),
                    online: true,
                    last_seen: entry.last_seen,
                    detail_note: Some(format!("RSSI {} dBm", entry.rssi)),
                });
            }
        }

        out
    }

    pub fn diagnostics(&mut self) -> String {
        let radio_present = has_present_bluetooth_radio();
        self.ensure_ble_watcher_with_radio(radio_present);
        let mut out = String::new();

        out.push_str(&format!(
            "[Bluetooth Radio] active_bthport_interface={}\n",
            radio_present
        ));
        out.push_str("[Bluetooth PnP Devices]\n");
        let raw_nodes = collect_raw_pnp_bt_nodes();
        if raw_nodes.is_empty() {
            out.push_str("  No paired Bluetooth PnP device nodes found.\n");
        } else {
            for r in &raw_nodes {
                let state = if r.connected && r.is_present {
                    "connected"
                } else {
                    "offline/cached"
                };
                out.push_str(&format!(
                    "  - {} [{}] | mac={:?} | connected={} | present={} | battery={:?} | id={}\n",
                    r.friendly,
                    state,
                    r.bt_mac,
                    r.connected,
                    r.is_present,
                    r.battery_pct,
                    r.instance_id
                ));
            }
        }

        // Brief ~2s passive BLE listen for Apple Continuity Proximity Pairing packets
        out.push_str("\n[BLE AirPods Passive Scan (~2s, Company 0x004C Continuity 0x07 0x19)]\n");
        if self.watcher_started {
            for _ in 0..20 {
                if self.stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        } else if !radio_present {
            out.push_str("  BLE watcher skipped (no active Bluetooth radio).\n");
        } else {
            out.push_str("  BLE watcher failed to start.\n");
        }

        if let Ok(guard) = self.ble_state.lock() {
            if let Some(err) = &guard.last_error {
                out.push_str(&format!("  Watcher status/error: {}\n", err));
            }
            out.push_str(&format!(
                "  Apple (0x004C) packets seen: {}, Continuity 0x07 packets decoded (RSSI >= {}dBm): {}\n",
                guard.apple_packets_seen, AIRPODS_MIN_RSSI_DBM, guard.proximity_packets_seen
            ));
            if guard.airpods.is_empty() {
                out.push_str("  No AirPods proximity advertisements captured during scan.\n");
            } else {
                for entry in guard.airpods.values() {
                    let sb = entry.sub_batteries;
                    out.push_str(&format!(
                        "  - {} [{}] L={:?}(chg={}) R={:?}(chg={}) Case={:?}(chg={}) RSSI={}dBm raw={}\n",
                        entry.model_name,
                        entry.id,
                        sb.left_pct,
                        sb.left_charging,
                        sb.right_pct,
                        sb.right_charging,
                        sb.case_pct,
                        sb.case_charging,
                        entry.rssi,
                        entry.raw_hex
                    ));
                }
            }
        }

        out
    }

    fn stop_ble_watcher(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            if self.watcher_started {
                let _ = watcher.Stop();
            }
            if let Some(token) = self.watcher_token.take() {
                let _ = watcher.RemoveReceived(token);
            }
            if let Some(token) = self.watcher_stopped_token.take() {
                let _ = watcher.RemoveStopped(token);
            }
        }
        self.watcher_token = None;
        self.watcher_stopped_token = None;
        self.watcher_started = false;
    }

    fn ensure_ble_watcher(&mut self) {
        let radio_present = has_present_bluetooth_radio();
        self.ensure_ble_watcher_with_radio(radio_present);
    }

    fn ensure_ble_watcher_with_radio(&mut self, radio_present: bool) {
        if self.stop_flag.load(Ordering::Relaxed) {
            return;
        }

        if !radio_present {
            self.stop_ble_watcher();
            self.last_watcher_attempt = None;
            if let Ok(mut guard) = self.ble_state.lock() {
                guard.watcher_stopped = false;
                guard.last_error = Some(NO_BT_RADIO_STATUS.to_string());
            }
            return;
        }

        if self.watcher_started {
            let stopped_by_event = self
                .ble_state
                .lock()
                .map(|g| g.watcher_stopped)
                .unwrap_or(false);
            let status_healthy = self
                .watcher
                .as_ref()
                .and_then(|w| w.Status().ok())
                .map(|s| s == BluetoothLEAdvertisementWatcherStatus::Started)
                .unwrap_or(false);

            if !stopped_by_event && status_healthy {
                return;
            }
            // Radio toggled off/on or watcher aborted: tear down and recreate below.
            self.stop_ble_watcher();
        }

        if let Some(last) = self.last_watcher_attempt {
            if last.elapsed() < WATCHER_RETRY_COOLDOWN {
                return;
            }
        }
        self.last_watcher_attempt = Some(Instant::now());

        if let Ok(mut guard) = self.ble_state.lock() {
            guard.watcher_stopped = false;
        }

        thread_local! {
            static COM_MTA_INITIALIZED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }
        COM_MTA_INITIALIZED.with(|init| {
            if !init.get() {
                unsafe {
                    if CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() {
                        init.set(true);
                    }
                }
            }
        });

        let watcher = match BluetoothLEAdvertisementWatcher::new() {
            Ok(w) => w,
            Err(e) => {
                if let Ok(mut guard) = self.ble_state.lock() {
                    guard.last_error = Some(format!("Create watcher failed: {}", e));
                }
                return;
            }
        };

        if let Err(e) = watcher.SetScanningMode(BluetoothLEScanningMode::Passive) {
            if let Ok(mut guard) = self.ble_state.lock() {
                guard.last_error = Some(format!("SetScanningMode failed: {}", e));
            }
        }

        apply_apple_proximity_filter(&watcher);

        let state_ref = Arc::clone(&self.ble_state);
        let rx_handler = TypedEventHandler::<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementReceivedEventArgs,
        >::new(move |_sender, args| {
            let Some(args) = args.as_ref() else {
                return Ok(());
            };
            let adv = match args.Advertisement() {
                Ok(a) => a,
                Err(_) => return Ok(()),
            };
            let mfg_list = match adv.ManufacturerData() {
                Ok(m) => m,
                Err(_) => return Ok(()),
            };
            let count = mfg_list.Size().unwrap_or(0);
            if count == 0 {
                return Ok(());
            }

            let rssi = args.RawSignalStrengthInDBm().unwrap_or(-127);
            let bt_addr = args.BluetoothAddress().unwrap_or(0);

            for i in 0..count {
                let Ok(mfg) = mfg_list.GetAt(i) else {
                    continue;
                };
                let Ok(company_id) = mfg.CompanyId() else {
                    continue;
                };
                if company_id != APPLE_COMPANY_ID {
                    continue;
                }
                let Ok(buf) = mfg.Data() else {
                    continue;
                };
                let Ok(reader) = DataReader::FromBuffer(&buf) else {
                    continue;
                };
                let len = reader.UnconsumedBufferLength().unwrap_or(0) as usize;
                if len == 0 {
                    continue;
                }
                let mut data = vec![0u8; len];
                if reader.ReadBytes(&mut data).is_err() {
                    continue;
                }

                if let Ok(mut guard) = state_ref.lock() {
                    guard.apple_packets_seen += 1;
                    if let Some(parsed) = parse_apple_proximity_pairing(&data, bt_addr, rssi) {
                        guard.proximity_packets_seen += 1;
                        guard.airpods.insert(parsed.id.clone(), parsed);
                    }
                }
            }
            Ok(())
        });

        let stop_state_ref = Arc::clone(&self.ble_state);
        let stopped_handler = TypedEventHandler::<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementWatcherStoppedEventArgs,
        >::new(move |_sender, args| {
            if let Ok(mut guard) = stop_state_ref.lock() {
                guard.watcher_stopped = true;
                let err_code = args.as_ref().and_then(|a| a.Error().ok());
                guard.last_error = Some(format!("Watcher stopped ({:?})", err_code));
            }
            Ok(())
        });

        let rx_token = match watcher.Received(&rx_handler) {
            Ok(token) => token,
            Err(e) => {
                if let Ok(mut guard) = self.ble_state.lock() {
                    guard.last_error = Some(format!("Attach Received failed: {}", e));
                }
                return;
            }
        };

        let stopped_token = watcher.Stopped(&stopped_handler).ok();

        if let Err(e) = watcher.Start() {
            let _ = watcher.RemoveReceived(rx_token);
            if let Some(t) = stopped_token {
                let _ = watcher.RemoveStopped(t);
            }
            if let Ok(mut guard) = self.ble_state.lock() {
                guard.last_error = Some(format!("Watcher Start failed: {}", e));
            }
            return;
        }

        if let Ok(mut guard) = self.ble_state.lock() {
            guard.last_error = None;
        }
        self.watcher = Some(watcher);
        self.watcher_token = Some(rx_token);
        self.watcher_stopped_token = stopped_token;
        self.watcher_started = true;
    }
}

impl Drop for BluetoothMonitor {
    fn drop(&mut self) {
        self.stop_ble_watcher();
    }
}

fn apply_apple_proximity_filter(watcher: &BluetoothLEAdvertisementWatcher) {
    let Ok(filter) = BluetoothLEAdvertisementFilter::new() else {
        return;
    };
    let Ok(writer) = DataWriter::new() else {
        return;
    };
    // AD Type 0xFF (ManufacturerSpecificData) payload starts with LE CompanyId (0x004C -> [0x4C, 0x00])
    // followed by Apple Continuity Proximity Pairing header [0x07, 0x19].
    let prefix = [
        (APPLE_COMPANY_ID & 0xFF) as u8,
        (APPLE_COMPANY_ID >> 8) as u8,
        CONTINUITY_PROXIMITY_PAIRING,
        PROXIMITY_PAIRING_LEN_FIELD,
    ];
    if writer.WriteBytes(&prefix).is_err() {
        return;
    }
    let Ok(buf) = writer.DetachBuffer() else {
        return;
    };
    let Ok(mfg_type) = BluetoothLEAdvertisementDataTypes::ManufacturerSpecificData() else {
        return;
    };
    let Ok(pattern) = BluetoothLEAdvertisementBytePattern::Create(mfg_type, 0, &buf) else {
        return;
    };
    if let Ok(patterns) = filter.BytePatterns() {
        if patterns.Append(&pattern).is_ok() {
            let _ = watcher.SetAdvertisementFilter(&filter);
        }
    }
}

fn collect_raw_pnp_bt_nodes() -> Vec<RawPnpBtNode> {
    const BT_ENUMERATORS: &[PCWSTR] = &[
        w!("BTHENUM"),
        w!("BTHLE"),
        w!("BTHLEDEVICE"),
        w!("BTHHFENUM"),
    ];

    let mut nodes = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    unsafe {
        for &enumerator in BT_ENUMERATORS {
            let Ok(dev_info) =
                SetupDiGetClassDevsW(None, enumerator, None, DIGCF_PRESENT | DIGCF_ALLCLASSES)
            else {
                continue;
            };
            if dev_info.is_invalid() {
                continue;
            }
            let _guard = DevInfoGuard(dev_info);

            let mut idx = 0u32;
            loop {
                let mut dev_data = SP_DEVINFO_DATA {
                    cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                    ..Default::default()
                };
                if SetupDiEnumDeviceInfo(dev_info, idx, &mut dev_data).is_err() {
                    break;
                }
                idx += 1;

                let instance_id = get_instance_id(dev_info, &dev_data);
                if instance_id.is_empty() {
                    continue;
                }
                let upper = instance_id.to_ascii_uppercase();
                if !seen_ids.insert(upper.clone()) {
                    continue;
                }

                let is_primary_node =
                    upper.starts_with("BTHENUM\\DEV_") || upper.starts_with("BTHLE\\DEV_");
                let battery_pct = read_prop_battery(dev_info, &dev_data);

                // Skip non-primary service/profile subnodes unless they carry a battery property
                if !is_primary_node && battery_pct.is_none() {
                    continue;
                }

                let friendly = read_prop_string(
                    dev_info,
                    &dev_data,
                    DEVPKEY_Device_FriendlyName.fmtid,
                    DEVPKEY_Device_FriendlyName.pid,
                )
                .or_else(|| {
                    read_prop_string(dev_info, &dev_data, DEVPKEY_NAME.fmtid, DEVPKEY_NAME.pid)
                })
                .unwrap_or_else(|| instance_id.clone());

                let bt_mac = extract_bt_mac(&upper).or_else(|| {
                    read_prop_string(
                        dev_info,
                        &dev_data,
                        GUID::from_u128(GUID_BT_ADDRESS),
                        PID_BT_ADDRESS,
                    )
                    .and_then(|s| normalize_mac(&s))
                });

                let connected = read_prop_bool(
                    dev_info,
                    &dev_data,
                    GUID::from_u128(GUID_PNP_CONNECTED),
                    PID_PNP_CONNECTED,
                )
                .unwrap_or(false);
                // Nodes are enumerated with DIGCF_PRESENT; if DEVPKEY_Device_IsPresent is absent
                // (None) on a connected node, avoid treating missing metadata as offline.
                let is_present = read_prop_bool(
                    dev_info,
                    &dev_data,
                    DEVPKEY_Device_IsPresent.fmtid,
                    DEVPKEY_Device_IsPresent.pid,
                )
                .unwrap_or(connected);

                nodes.push(RawPnpBtNode {
                    instance_id,
                    friendly,
                    bt_mac,
                    battery_pct,
                    connected,
                    is_present,
                    is_primary_node,
                });
            }
        }
    }

    nodes
}

fn enumerate_pnp_bt_devices() -> Vec<PnpBtRecord> {
    merge_pnp_bt_nodes(collect_raw_pnp_bt_nodes())
}

fn poll_pnp_bt_with_radio<F>(
    cache: &mut HashMap<String, CachedPnpBtEntry>,
    radio_present: bool,
    now: Instant,
    enumerate_fn: F,
) -> Vec<WirelessDevice>
where
    F: FnOnce() -> Vec<PnpBtRecord>,
{
    if !radio_present {
        if cache.is_empty() {
            return Vec::new();
        }
        return reconcile_pnp_bt_devices(cache, Vec::new(), now);
    }
    let online_records = filter_online_bt_records(enumerate_fn());
    reconcile_pnp_bt_devices(cache, online_records, now)
}

fn merge_pnp_bt_nodes(nodes: Vec<RawPnpBtNode>) -> Vec<PnpBtRecord> {
    let mut by_key: HashMap<String, PnpBtRecord> = HashMap::new();

    for node in nodes {
        let clean_name = clean_bt_friendly_name(&node.friendly);
        let mac_key = node
            .bt_mac
            .as_ref()
            .map(|m| format!("bt:{}", m))
            .unwrap_or_else(|| format!("bt:{}", clean_name.to_ascii_lowercase()));

        match by_key.get_mut(&mac_key) {
            Some(existing) => {
                if node.is_primary_node && !existing.is_primary_node {
                    existing.name = clean_name;
                    existing.instance_id = node.instance_id;
                    existing.is_primary_node = true;
                }
                if node.is_primary_node {
                    existing.primary_connected =
                        Some(existing.primary_connected.unwrap_or(false) || node.connected);
                }
                if let Some(pct) = node.battery_pct {
                    let should_replace = existing.battery_pct.is_none()
                        || (!existing.battery_from_present && node.is_present);
                    if should_replace {
                        existing.battery_pct = Some(pct);
                        existing.battery_from_present = node.is_present;
                    }
                }
                existing.connected |= node.connected;
                existing.is_present |= node.is_present;
            }
            None => {
                by_key.insert(
                    mac_key.clone(),
                    PnpBtRecord {
                        key: mac_key,
                        instance_id: node.instance_id,
                        name: clean_name,
                        battery_pct: node.battery_pct,
                        battery_from_present: node.battery_pct.is_some() && node.is_present,
                        connected: node.connected,
                        is_present: node.is_present,
                        is_primary_node: node.is_primary_node,
                        primary_connected: if node.is_primary_node {
                            Some(node.connected)
                        } else {
                            None
                        },
                    },
                );
            }
        }
    }

    let mut list: Vec<PnpBtRecord> = by_key.into_values().collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

fn filter_online_bt_records(records: Vec<PnpBtRecord>) -> Vec<PnpBtRecord> {
    records
        .into_iter()
        .filter(|rec| {
            // If parent DEV_<MAC> node exists and says connected=false, treat entire MAC as offline.
            if let Some(false) = rec.primary_connected {
                return false;
            }
            rec.connected && rec.is_present
        })
        .collect()
}

fn reconcile_pnp_bt_devices(
    cache: &mut HashMap<String, CachedPnpBtEntry>,
    online_records: Vec<PnpBtRecord>,
    now: Instant,
) -> Vec<WirelessDevice> {
    let mut seen_keys = HashSet::with_capacity(online_records.len());

    for rec in online_records {
        let kind = infer_bt_device_kind(&rec.name);
        let key = rec.key.clone();
        seen_keys.insert(key.clone());

        match cache.get_mut(&key) {
            Some(entry) => {
                entry.miss_streak = 0;
                entry.last_seen = now;
                entry.device.name = rec.name;
                entry.device.kind = kind;
                entry.device.online = true;
                entry.device.last_seen = now;

                if let Some(pct) = rec.battery_pct {
                    entry.device.battery_pct = Some(pct);
                    entry.device.detail_note = None;
                    entry.last_battery_seen = Some(now);
                } else {
                    let keep_cached_battery = entry.device.battery_pct.is_some()
                        && entry.last_battery_seen.is_some_and(|ts| {
                            now.saturating_duration_since(ts).as_secs() <= BT_CACHE_GRACE_SECS
                        });
                    if !keep_cached_battery {
                        entry.device.battery_pct = None;
                        entry.device.detail_note =
                            Some("Connected (PnP battery unavailable)".into());
                    }
                }
            }
            None => {
                let last_battery_seen = rec.battery_pct.map(|_| now);
                let device = WirelessDevice {
                    id: rec.key,
                    name: rec.name,
                    kind,
                    transport: Transport::Bluetooth,
                    battery_pct: rec.battery_pct,
                    charging: false,
                    sub_batteries: None,
                    online: true,
                    last_seen: now,
                    detail_note: if rec.battery_pct.is_none() {
                        Some("Connected (PnP battery unavailable)".into())
                    } else {
                        None
                    },
                };
                cache.insert(
                    key,
                    CachedPnpBtEntry {
                        device,
                        last_seen: now,
                        last_battery_seen,
                        miss_streak: 0,
                    },
                );
            }
        }
    }

    let keys: Vec<String> = cache.keys().cloned().collect();
    for key in keys {
        if !seen_keys.contains(&key) {
            let should_evict = match cache.get_mut(&key) {
                Some(entry) => {
                    entry.miss_streak = entry.miss_streak.saturating_add(1);
                    entry.miss_streak >= BT_OFFLINE_MISS_THRESHOLD
                        || now.saturating_duration_since(entry.last_seen).as_secs()
                            > BT_CACHE_GRACE_SECS
                }
                None => true,
            };
            if should_evict {
                cache.remove(&key);
            }
        }
    }

    let mut out: Vec<WirelessDevice> = cache.values().map(|e| e.device.clone()).collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

unsafe fn get_instance_id(dev_info: HDEVINFO, dev_data: &SP_DEVINFO_DATA) -> String {
    let mut buf = [0u16; 512];
    let mut req = 0u32;
    if SetupDiGetDeviceInstanceIdW(dev_info, dev_data, Some(&mut buf), Some(&mut req)).is_ok() {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        return String::from_utf16_lossy(&buf[..len]);
    }
    String::new()
}

unsafe fn read_prop_string(
    dev_info: HDEVINFO,
    dev_data: &SP_DEVINFO_DATA,
    fmtid: GUID,
    pid: u32,
) -> Option<String> {
    let mut key = DEVPKEY_NAME;
    key.fmtid = fmtid;
    key.pid = pid;
    let mut prop_type = Default::default();
    let mut buf = [0u8; 512];
    let mut req = 0u32;
    if SetupDiGetDevicePropertyW(
        dev_info,
        dev_data,
        &key,
        &mut prop_type,
        Some(&mut buf),
        Some(&mut req),
        0,
    )
    .is_err()
    {
        return None;
    }
    if prop_type != DEVPROP_TYPE_STRING {
        return None;
    }
    let byte_len = (req as usize).min(buf.len());
    let (chunks, _) = buf[..byte_len].as_chunks::<2>();
    let u16_slice: Vec<u16> = chunks
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .take_while(|&w| w != 0)
        .collect();
    if u16_slice.is_empty() {
        None
    } else {
        Some(String::from_utf16_lossy(&u16_slice))
    }
}

unsafe fn read_prop_bool(
    dev_info: HDEVINFO,
    dev_data: &SP_DEVINFO_DATA,
    fmtid: GUID,
    pid: u32,
) -> Option<bool> {
    let mut key = DEVPKEY_NAME;
    key.fmtid = fmtid;
    key.pid = pid;
    let mut prop_type = Default::default();
    let mut buf = [0u8; 8];
    let mut req = 0u32;
    if SetupDiGetDevicePropertyW(
        dev_info,
        dev_data,
        &key,
        &mut prop_type,
        Some(&mut buf),
        Some(&mut req),
        0,
    )
    .is_err()
    {
        return None;
    }
    if prop_type == DEVPROP_TYPE_BOOLEAN && req >= 1 {
        Some(buf[0] != 0)
    } else {
        None
    }
}

unsafe fn read_prop_battery(dev_info: HDEVINFO, dev_data: &SP_DEVINFO_DATA) -> Option<u8> {
    let mut key = DEVPKEY_NAME;
    key.fmtid = GUID::from_u128(GUID_BT_BATTERY);
    key.pid = PID_BT_BATTERY;

    let mut prop_type = Default::default();
    let mut buf = [0u8; 16];
    let mut req = 0u32;
    if SetupDiGetDevicePropertyW(
        dev_info,
        dev_data,
        &key,
        &mut prop_type,
        Some(&mut buf),
        Some(&mut req),
        0,
    )
    .is_err()
    {
        return None;
    }

    if prop_type == DEVPROP_TYPE_BYTE && req >= 1 {
        let pct = buf[0];
        if pct <= 100 {
            return Some(pct);
        }
    } else if prop_type == DEVPROP_TYPE_UINT32 && req >= 4 {
        let val = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if val <= 100 {
            return Some(val as u8);
        }
    }
    None
}

fn normalize_mac(s: &str) -> Option<String> {
    let hex: String = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if hex.len() == 12 && hex != "000000000000" {
        Some(hex)
    } else {
        None
    }
}

fn extract_bt_mac(upper_instance_id: &str) -> Option<String> {
    // 1. Canonical DEV_<12HEX> or BLUETOOTHDEVICE_<12HEX>
    for prefix in ["DEV_", "BLUETOOTHDEVICE_"] {
        if let Some(pos) = upper_instance_id.find(prefix) {
            let rest = &upper_instance_id[pos + prefix.len()..];
            let candidate = rest.split(['\\', '&', '_']).next().unwrap_or("");
            if candidate.len() == 12
                && candidate.chars().all(|c| c.is_ascii_hexdigit())
                && candidate != "000000000000"
            {
                return Some(candidate.to_string());
            }
        }
    }

    // 2. Child PnP node instance segment (after last '\'), e.g. 8&1234567&0&001122334455_C00000000
    // Skip any GUID braces {....} so Bluetooth Base UUID (00805F9B34FB) is never matched.
    let last_seg = upper_instance_id.rsplit('\\').next().unwrap_or("");
    if !last_seg.contains('{') {
        for tok in last_seg.split(['&', '_']) {
            if tok.len() == 12
                && tok.chars().all(|c| c.is_ascii_hexdigit())
                && tok != "000000000000"
            {
                return Some(tok.to_string());
            }
        }
    }

    None
}

fn clean_bt_friendly_name(name: &str) -> String {
    let mut s = name.trim();
    for suffix in [
        " Hands-Free AG",
        " Hands-Free",
        " Avrcp 传输",
        " Avrcp Transport",
        " Stereo",
    ] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            s = stripped.trim();
        }
    }
    s.to_string()
}

fn has_token_phrase(tokens: &[&str], phrase: &[&str]) -> bool {
    if phrase.is_empty() || tokens.len() < phrase.len() {
        return false;
    }
    tokens.windows(phrase.len()).any(|w| w == phrase)
}

fn infer_bt_device_kind(name: &str) -> DeviceKind {
    let lower = name.to_ascii_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();

    let is_earbuds = tokens.iter().any(|&t| {
        matches!(
            t,
            "buds"
                | "airpods"
                | "airpod"
                | "ear"
                | "earbud"
                | "earbuds"
                | "earphone"
                | "earphones"
                | "earpod"
                | "earpods"
                | "tws"
                | "pod"
                | "pods"
        ) || t.starts_with("buds")
            || t.starts_with("airpod")
            || t.starts_with("earbud")
            || t.starts_with("earpod")
            || t.starts_with("earphone")
            || t.starts_with("freebuds")
            || t.starts_with("powerbeats")
    });
    if is_earbuds {
        return DeviceKind::Earbuds;
    }

    let is_headset = tokens.iter().any(|&t| {
        matches!(
            t,
            "headset" | "headsets" | "headphone" | "headphones" | "qc"
        ) || t.starts_with("headset")
            || t.starts_with("headphone")
            || (t.starts_with("qc") && t[2..].chars().next().is_some_and(|c| c.is_ascii_digit()))
    }) || tokens
        .windows(2)
        .any(|w| w[0] == "wh" && w[1].chars().next().is_some_and(|c| c.is_ascii_digit()));
    if is_headset {
        return DeviceKind::Headset;
    }

    let is_mouse = tokens
        .iter()
        .any(|&t| matches!(t, "mouse" | "mice" | "viper"))
        || has_token_phrase(&tokens, &["mx", "master"]);
    if is_mouse {
        return DeviceKind::Mouse;
    }

    let is_keyboard = tokens
        .iter()
        .any(|&t| matches!(t, "keyboard" | "keyboards" | "keychron"))
        || has_token_phrase(&tokens, &["mx", "keys"]);
    if is_keyboard {
        return DeviceKind::Keyboard;
    }

    let is_gamepad = tokens.iter().any(|&t| {
        matches!(
            t,
            "xbox" | "controller" | "gamepad" | "dualsense" | "dualshock"
        )
    });
    if is_gamepad {
        return DeviceKind::Gamepad;
    }

    DeviceKind::Other
}

/// Pure decoder for Apple Continuity Proximity Pairing advertisement (`0x07 0x19`, 27 bytes)
/// aligned with OpenPods bit/nibble & `isFlipped` semantics.
fn decode_airpods_proximity_fields(data: &[u8]) -> Option<AirPodsDecoded> {
    if data.len() != AIRPODS_PACKET_LEN
        || data[0] != CONTINUITY_PROXIMITY_PAIRING
        || data[1] != PROXIMITY_PAIRING_LEN_FIELD
    {
        return None;
    }

    let model_id = ((data[3] as u16) << 8) | (data[4] as u16);
    let model_name = map_apple_model_name(model_id);
    let status_byte = data[5];
    // OpenPods: bit 1 of high nibble at data[5] (0x20) == 0 means left/right are flipped.
    let is_flipped = (status_byte & 0x20) == 0;

    let pods_byte = data[6];
    let hi_pod = (pods_byte >> 4) & 0x0F;
    let lo_pod = pods_byte & 0x0F;

    let case_and_chg = data[7];
    let case_nibble = case_and_chg & 0x0F;
    let chg_flags = (case_and_chg >> 4) & 0x0F;

    // OpenPods:
    //   flip == true  -> left = hi_pod (char 12), right = lo_pod (char 13), chargeL = bit 1 (0x02), chargeR = bit 0 (0x01)
    //   flip == false -> left = lo_pod (char 13), right = hi_pod (char 12), chargeL = bit 0 (0x01), chargeR = bit 1 (0x02)
    let (left_raw, right_raw, left_chg, right_chg) = if is_flipped {
        (
            hi_pod,
            lo_pod,
            (chg_flags & 0x02) != 0,
            (chg_flags & 0x01) != 0,
        )
    } else {
        (
            lo_pod,
            hi_pod,
            (chg_flags & 0x01) != 0,
            (chg_flags & 0x02) != 0,
        )
    };
    let case_chg = (chg_flags & 0x04) != 0;

    let left_pct = decode_airpods_nibble(left_raw);
    let right_pct = decode_airpods_nibble(right_raw);
    let case_pct = decode_airpods_nibble(case_nibble);

    let overall_pct = match (left_pct, right_pct) {
        (Some(l), Some(r)) => Some(l.min(r)),
        (Some(l), None) => Some(l),
        (None, Some(r)) => Some(r),
        (None, None) => case_pct,
    };

    let charging = left_chg || right_chg;

    Some(AirPodsDecoded {
        model_id,
        model_name,
        sub_batteries: SubBatteries {
            left_pct,
            left_charging: left_chg,
            right_pct,
            right_charging: right_chg,
            case_pct,
            case_charging: case_chg,
        },
        overall_pct,
        charging,
    })
}

/// Parse Apple Continuity Proximity Pairing advertisement (`0x07 0x19`, 27 bytes) with RSSI threshold.
fn parse_apple_proximity_pairing(
    data: &[u8],
    _bt_addr: u64,
    rssi: i16,
) -> Option<AirPodsCacheEntry> {
    if rssi < AIRPODS_MIN_RSSI_DBM {
        return None;
    }

    let decoded = decode_airpods_proximity_fields(data)?;
    let raw_hex = data
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(" ");

    Some(AirPodsCacheEntry {
        id: format!("ble:apple:{:04X}", decoded.model_id),
        model_name: decoded.model_name.to_string(),
        sub_batteries: decoded.sub_batteries,
        overall_pct: decoded.overall_pct,
        charging: decoded.charging,
        rssi,
        raw_hex,
        last_seen: Instant::now(),
    })
}

fn decode_airpods_nibble(nibble: u8) -> Option<u8> {
    match nibble {
        0..=9 => Some(nibble * 10 + 5),
        10 => Some(100),
        _ => None, // 0x0F = disconnected / unavailable
    }
}

fn map_apple_model_name(model_id: u16) -> &'static str {
    match model_id {
        0x0220 => "AirPods 1",
        0x0F20 => "AirPods 2",
        0x1320 => "AirPods 3",
        0x1920 => "AirPods 4",
        0x1B20 => "AirPods 4 (ANC)",
        0x0E20 => "AirPods Pro",
        0x1420 => "AirPods Pro 2",
        0x2420 => "AirPods Pro 2 (USB-C)",
        0x2620 | 0x2720 | 0x2820 => "AirPods Pro 3",
        0x0A20 => "AirPods Max",
        0x1F20 => "AirPods Max (USB-C)",
        0x0B20 => "Powerbeats Pro",
        0x1220 => "Beats Fit Pro",
        0x1720 => "Beats Studio Buds+",
        _ => "Apple AirPods",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_airpods_proximity_parse_unflipped_and_flipped_openpods() {
        // Unflipped sample: data[5]=0x20 (bit 0x20 set -> is_flipped=false),
        // data[6]=0x98 (hi=9, lo=8), data[7]=0x5A (chg=0b0101: bit0=L, bit2=Case; case=10 -> 100%)
        let mut unflipped = vec![0u8; 27];
        unflipped[0] = 0x07;
        unflipped[1] = 0x19;
        unflipped[2] = 0x01;
        unflipped[3] = 0x27;
        unflipped[4] = 0x20;
        unflipped[5] = 0x20;
        unflipped[6] = 0x98;
        unflipped[7] = 0x5A;

        let parsed =
            parse_apple_proximity_pairing(&unflipped, 0, -55).expect("unflipped should parse");
        assert_eq!(parsed.model_name, "AirPods Pro 3");
        assert_eq!(parsed.sub_batteries.left_pct, Some(85));
        assert_eq!(parsed.sub_batteries.right_pct, Some(95));
        assert_eq!(parsed.sub_batteries.case_pct, Some(100));
        assert!(parsed.sub_batteries.left_charging);
        assert!(!parsed.sub_batteries.right_charging);
        assert!(parsed.sub_batteries.case_charging);
        assert_eq!(parsed.overall_pct, Some(85));

        // Flipped sample: data[5]=0x00 (bit 0x20 clear -> is_flipped=true),
        // data[6]=0x98 (hi=9 -> L, lo=8 -> R), data[7]=0x5A (chg=0b0101 -> bit1=L(false), bit0=R(true))
        let mut flipped = unflipped.clone();
        flipped[5] = 0x00;
        let decoded_flipped =
            decode_airpods_proximity_fields(&flipped).expect("flipped should decode");
        assert_eq!(decoded_flipped.sub_batteries.left_pct, Some(95));
        assert_eq!(decoded_flipped.sub_batteries.right_pct, Some(85));
        assert_eq!(decoded_flipped.sub_batteries.case_pct, Some(100));
        assert!(!decoded_flipped.sub_batteries.left_charging);
        assert!(decoded_flipped.sub_batteries.right_charging);
        assert!(decoded_flipped.sub_batteries.case_charging);
    }

    #[test]
    fn test_airpods_strict_header_length_and_rssi_filter() {
        let mut pkt = vec![0u8; 27];
        pkt[0] = 0x07;
        pkt[1] = 0x19;
        pkt[3] = 0x0E;
        pkt[4] = 0x20;
        pkt[5] = 0x20;
        pkt[6] = 0xAA;
        pkt[7] = 0x0A;

        // Weak RSSI (< -60 dBm) must be rejected to avoid nearby strangers' AirPods
        assert!(parse_apple_proximity_pairing(&pkt, 0, -61).is_none());
        assert!(parse_apple_proximity_pairing(&pkt, 0, -60).is_some());

        // Non-27-byte length must be rejected
        let short = &pkt[..25];
        assert!(decode_airpods_proximity_fields(short).is_none());
        let mut long = pkt.clone();
        long.push(0x00);
        assert!(decode_airpods_proximity_fields(&long).is_none());

        // Wrong data[1] length marker (!= 0x19) must be rejected
        let mut bad_hdr = pkt.clone();
        bad_hdr[1] = 0x12;
        assert!(decode_airpods_proximity_fields(&bad_hdr).is_none());
    }

    #[test]
    fn test_infer_bt_device_kind_word_boundaries() {
        // False-positive traps: "ear" in Gear, "pod" in Tripod
        assert_eq!(infer_bt_device_kind("Samsung Gear S3"), DeviceKind::Other);
        assert_eq!(infer_bt_device_kind("Bluetooth Tripod"), DeviceKind::Other);
        assert_eq!(infer_bt_device_kind("HomePod Mini"), DeviceKind::Other);

        // True positives
        assert_eq!(infer_bt_device_kind("Nothing Ear (2)"), DeviceKind::Earbuds);
        assert_eq!(
            infer_bt_device_kind("Galaxy Buds2 Pro"),
            DeviceKind::Earbuds
        );
        assert_eq!(infer_bt_device_kind("AirPods Pro"), DeviceKind::Earbuds);
        assert_eq!(infer_bt_device_kind("WH-1000XM5"), DeviceKind::Headset);
        assert_eq!(infer_bt_device_kind("Bose QC45"), DeviceKind::Headset);
        assert_eq!(infer_bt_device_kind("MX Master 3S"), DeviceKind::Mouse);
        assert_eq!(
            infer_bt_device_kind("Keychron K8 Pro"),
            DeviceKind::Keyboard
        );
        assert_eq!(
            infer_bt_device_kind("Xbox Wireless Controller"),
            DeviceKind::Gamepad
        );
    }

    #[test]
    fn test_extract_bt_mac_parent_and_hfp_child() {
        let parent = r"BTHENUM\DEV_001122334455\8&1234567&0&BLUETOOTHDEVICE_001122334455";
        let hfp = r"BTHENUM\{0000111E-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&A013\8&1234567&0&001122334455_C00000000";
        assert_eq!(extract_bt_mac(parent).as_deref(), Some("001122334455"));
        assert_eq!(extract_bt_mac(hfp).as_deref(), Some("001122334455"));
    }

    #[test]
    fn test_merge_pnp_bt_nodes_connected_parent_with_child_battery() {
        let parent_id = r"BTHENUM\DEV_001122334455\8&1234567&0&BLUETOOTHDEVICE_001122334455";
        let hfp_id = r"BTHENUM\{0000111E-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&A013\8&1234567&0&001122334455_C00000000";
        let nodes = vec![
            RawPnpBtNode {
                instance_id: hfp_id.into(),
                friendly: "Galaxy Buds Pro (4455) Hands-Free AG".into(),
                bt_mac: extract_bt_mac(hfp_id),
                battery_pct: Some(85),
                connected: false,
                is_present: true,
                is_primary_node: false,
            },
            RawPnpBtNode {
                instance_id: parent_id.into(),
                friendly: "Galaxy Buds Pro (4455)".into(),
                bt_mac: extract_bt_mac(parent_id),
                battery_pct: None,
                connected: true,
                is_present: true,
                is_primary_node: true,
            },
        ];

        let online = filter_online_bt_records(merge_pnp_bt_nodes(nodes));
        assert_eq!(online.len(), 1);
        assert_eq!(online[0].key, "bt:001122334455");
        assert_eq!(online[0].name, "Galaxy Buds Pro (4455)");
        assert_eq!(online[0].battery_pct, Some(85));
        assert_eq!(infer_bt_device_kind(&online[0].name), DeviceKind::Earbuds);
    }

    #[test]
    fn test_merge_pnp_bt_nodes_offline_parent_excludes_cached_battery() {
        let parent_id = r"BTHENUM\DEV_001122334455\8&1234567&0&BLUETOOTHDEVICE_001122334455";
        let hfp_id = r"BTHENUM\{0000111E-0000-1000-8000-00805F9B34FB}_VID&00010075_PID&A013\8&1234567&0&001122334455_C00000000";
        let nodes = vec![
            RawPnpBtNode {
                instance_id: parent_id.into(),
                friendly: "Galaxy Buds Pro (4455)".into(),
                bt_mac: extract_bt_mac(parent_id),
                battery_pct: None,
                connected: false,
                is_present: false,
                is_primary_node: true,
            },
            RawPnpBtNode {
                instance_id: hfp_id.into(),
                friendly: "Galaxy Buds Pro (4455) Hands-Free AG".into(),
                bt_mac: extract_bt_mac(hfp_id),
                battery_pct: Some(100),
                connected: false,
                is_present: true,
                is_primary_node: false,
            },
        ];

        let online = filter_online_bt_records(merge_pnp_bt_nodes(nodes));
        assert!(online.is_empty());
    }

    #[test]
    fn test_merge_pnp_bt_nodes_dual_primary_preserves_connected_true() {
        let ble_primary = r"BTHLE\DEV_112233445566\8&1234&0&112233445566";
        let classic_primary = r"BTHENUM\DEV_112233445566\8&1234&0&BLUETOOTHDEVICE_112233445566";
        let nodes = vec![
            RawPnpBtNode {
                instance_id: ble_primary.into(),
                friendly: "MX Master 3S".into(),
                bt_mac: extract_bt_mac(ble_primary),
                battery_pct: Some(72),
                connected: true,
                is_present: true,
                is_primary_node: true,
            },
            RawPnpBtNode {
                instance_id: classic_primary.into(),
                friendly: "MX Master 3S".into(),
                bt_mac: extract_bt_mac(classic_primary),
                battery_pct: None,
                connected: false,
                is_present: false,
                is_primary_node: true,
            },
        ];

        let online = filter_online_bt_records(merge_pnp_bt_nodes(nodes));
        assert_eq!(online.len(), 1);
        assert_eq!(online[0].battery_pct, Some(72));
        assert_eq!(online[0].primary_connected, Some(true));
    }

    #[test]
    fn test_reconcile_pnp_bt_debounces_transient_disconnect_and_battery_drop() {
        let mut cache = HashMap::new();
        let t0 = Instant::now();
        let rec = |pct: Option<u8>| PnpBtRecord {
            key: "bt:AABBCCDDEEFF".into(),
            instance_id: r"BTHLE\DEV_AABBCCDDEEFF".into(),
            name: "WH-1000XM5".into(),
            battery_pct: pct,
            battery_from_present: pct.is_some(),
            connected: true,
            is_present: true,
            is_primary_node: true,
            primary_connected: Some(true),
        };

        let out0 = reconcile_pnp_bt_devices(&mut cache, vec![rec(Some(80))], t0);
        assert_eq!(out0.len(), 1);
        assert!(out0[0].online);
        assert_eq!(out0[0].battery_pct, Some(80));

        // Transient PnP battery property missing while still connected preserves 80%
        let t1 = t0 + Duration::from_secs(45);
        let out1 = reconcile_pnp_bt_devices(&mut cache, vec![rec(None)], t1);
        assert_eq!(out1.len(), 1);
        assert!(out1[0].online);
        assert_eq!(out1[0].battery_pct, Some(80));

        // 1~2 transient PnP refresh drops keep device online
        let t2 = t1 + Duration::from_secs(45);
        let out2 = reconcile_pnp_bt_devices(&mut cache, vec![], t2);
        assert_eq!(out2.len(), 1);
        assert!(out2[0].online);

        let t3 = t2 + Duration::from_secs(45);
        let out3 = reconcile_pnp_bt_devices(&mut cache, vec![], t3);
        assert_eq!(out3.len(), 1);
        assert!(out3[0].online);

        // 3rd consecutive miss -> genuinely disconnected, evicted cleanly
        let t4 = t3 + Duration::from_secs(45);
        let out4 = reconcile_pnp_bt_devices(&mut cache, vec![], t4);
        assert!(out4.is_empty());
    }

    #[test]
    fn test_ensure_ble_watcher_skips_when_no_radio_present() {
        let mut monitor = BluetoothMonitor::new_unstarted(Arc::new(AtomicBool::new(false)));
        monitor.last_watcher_attempt = Some(Instant::now());

        monitor.ensure_ble_watcher_with_radio(false);

        assert!(!monitor.watcher_started);
        assert!(monitor.watcher.is_none());
        assert!(monitor.last_watcher_attempt.is_none());
        let guard = monitor.ble_state.lock().unwrap();
        assert!(!guard.watcher_stopped);
        assert_eq!(guard.last_error.as_deref(), Some(NO_BT_RADIO_STATUS));
    }

    #[test]
    fn test_poll_pnp_bt_skips_setupdi_when_no_radio_and_empty_cache() {
        let mut cache = HashMap::new();
        let mut enumerated = false;

        let out = poll_pnp_bt_with_radio(&mut cache, false, Instant::now(), || {
            enumerated = true;
            Vec::new()
        });

        assert!(!enumerated);
        assert!(out.is_empty());
        assert!(cache.is_empty());
    }

    #[test]
    fn test_poll_pnp_bt_debounces_cached_devices_offline_when_radio_absent() {
        let mut cache = HashMap::new();
        let t0 = Instant::now();
        let sample_rec = || PnpBtRecord {
            key: "bt:AABBCCDDEEFF".into(),
            instance_id: r"BTHLE\DEV_AABBCCDDEEFF".into(),
            name: "WH-1000XM5".into(),
            battery_pct: Some(85),
            battery_from_present: true,
            connected: true,
            is_present: true,
            is_primary_node: true,
            primary_connected: Some(true),
        };

        let out0 = poll_pnp_bt_with_radio(&mut cache, true, t0, || vec![sample_rec()]);
        assert_eq!(out0.len(), 1);
        assert_eq!(out0[0].battery_pct, Some(85));

        // Radio disappears: SetupDi is skipped, cached device debounces smoothly offline
        let t1 = t0 + Duration::from_secs(45);
        let out1 = poll_pnp_bt_with_radio(&mut cache, false, t1, || {
            panic!("should skip SetupDi enumeration when radio is absent")
        });
        assert_eq!(out1.len(), 1);

        let t2 = t1 + Duration::from_secs(45);
        let out2 = poll_pnp_bt_with_radio(&mut cache, false, t2, || {
            panic!("should skip SetupDi enumeration when radio is absent")
        });
        assert_eq!(out2.len(), 1);

        // 3rd miss evicts cached entry cleanly
        let t3 = t2 + Duration::from_secs(45);
        let out3 = poll_pnp_bt_with_radio(&mut cache, false, t3, || {
            panic!("should skip SetupDi enumeration when radio is absent")
        });
        assert!(out3.is_empty());
        assert!(cache.is_empty());
    }
}
