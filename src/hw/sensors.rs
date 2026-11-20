use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, HANDLE};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP_READ,
    MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
};

const SHM_PROBE_COOLDOWN: Duration = Duration::from_secs(15);
const STALE_FREEZE_TIMEOUT: Duration = Duration::from_secs(15);
const STALE_EPOCH_AGE_SECS: u64 = 30;

const HWINFO_SIG_ACTIVE: u32 = 0x5369_5748; // "HWiS"
const HWINFO_SIG_DEAD: u32 = 0x4441_4544; // "DEAD"
const HWINFO_HEADER_SIZE: usize = 44;
const HWINFO_MIN_SENSOR_SIZE: usize = 264;
const HWINFO_MIN_READING_SIZE: usize = 316;
const HWINFO_MAX_SENSORS: usize = 4096;
const HWINFO_MAX_READINGS: usize = 65536;

const HWINFO_READING_TEMP: u32 = 1;
const HWINFO_READING_FAN: u32 = 3;
const HWINFO_READING_POWER: u32 = 5;

const MAHM_SIG_ACTIVE: u32 = 0x4D41_484D; // "MAHM"
const MAHM_SIG_DEAD: u32 = 0x0000_DEAD;
const MAHM_MIN_HEADER_SIZE: usize = 20;
const MAHM_MIN_ENTRY_SIZE: usize = 1316;
const MAHM_MAX_ENTRIES: usize = 4096;

#[derive(Debug, Clone, Default)]
pub struct ExternalSensorSample {
    pub provider: &'static str,
    pub cpu_temp_c: Option<f32>,
    pub cpu_power_w: Option<f32>,
    pub cpu_fan_rpm: Option<u32>,
    pub disk_temp_c: Option<f32>,
    pub gpu_temp_c: Option<f32>,
    pub gpu_hotspot_c: Option<f32>,
    pub gpu_power_w: Option<f32>,
    pub gpu_fan_rpm: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShmSourceKind {
    HwInfo,
    Aida64,
    Afterburner,
}

#[derive(Debug, Clone)]
struct StaleTracker {
    last_ts: Option<i64>,
    last_changed: Instant,
}

impl Default for StaleTracker {
    fn default() -> Self {
        Self {
            last_ts: None,
            last_changed: Instant::now(),
        }
    }
}

impl StaleTracker {
    fn reset(&mut self) {
        self.last_ts = None;
        self.last_changed = Instant::now();
    }

    fn is_fresh(&mut self, ts: i64, now_unix_secs: u64, now: Instant) -> bool {
        if ts <= 0 {
            return false;
        }
        // Reject timestamps clearly frozen in the distant past when using Unix epoch seconds.
        if ts > 1_000_000_000 && now_unix_secs > (ts as u64).saturating_add(STALE_EPOCH_AGE_SECS) {
            return false;
        }
        match self.last_ts {
            Some(prev) if prev == ts => {
                now.saturating_duration_since(self.last_changed) <= STALE_FREEZE_TIMEOUT
            }
            _ => {
                self.last_ts = Some(ts);
                self.last_changed = now;
                true
            }
        }
    }
}

pub struct SensorHub {
    active_shm: Option<ShmSourceKind>,
    last_shm_probe: Option<Instant>,
    stale_tracker: StaleTracker,
    acpi_pdh_query: isize,
    acpi_pdh_counter: isize,
    acpi_pdh_init_attempted: bool,
    acpi_empty_streak: u8,
    acpi_unavailable: bool,
}

impl SensorHub {
    pub fn new() -> Self {
        Self {
            active_shm: None,
            last_shm_probe: None,
            stale_tracker: StaleTracker::default(),
            acpi_pdh_query: 0,
            acpi_pdh_counter: 0,
            acpi_pdh_init_attempted: false,
            acpi_empty_streak: 0,
            acpi_unavailable: false,
        }
    }

    pub fn sample(&mut self, enabled: bool) -> ExternalSensorSample {
        if !enabled {
            self.reset_sources();
            return ExternalSensorSample::default();
        }

        let now = Instant::now();
        let now_unix = current_unix_secs();

        if let Some(kind) = self.active_shm {
            if let Some(sample) = self.try_read_shm(kind, now_unix, now) {
                self.close_acpi_pdh();
                return sample;
            }
            // Active source stopped or became stale: immediately release and allow fallback.
            self.active_shm = None;
            self.stale_tracker.reset();
            self.last_shm_probe = None;
        }

        let should_probe_shm = self
            .last_shm_probe
            .is_none_or(|t| now.saturating_duration_since(t) >= SHM_PROBE_COOLDOWN);

        if should_probe_shm {
            self.last_shm_probe = Some(now);
            for kind in [
                ShmSourceKind::HwInfo,
                ShmSourceKind::Aida64,
                ShmSourceKind::Afterburner,
            ] {
                if let Some(sample) = self.try_read_shm(kind, now_unix, now) {
                    self.active_shm = Some(kind);
                    self.close_acpi_pdh();
                    return sample;
                }
            }
        }

        self.sample_acpi_thermal()
    }

    pub fn diagnostics(&mut self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("  HWiNFO64:        {}", diagnose_hwinfo()));
        lines.push(format!("  AIDA64:          {}", diagnose_aida64()));
        lines.push(format!("  MSI Afterburner: {}", diagnose_afterburner()));

        let acpi_status = match self.probe_acpi_temp_for_diag() {
            Some(temp) => {
                self.acpi_unavailable = false;
                self.acpi_empty_streak = 0;
                format!("可用 ({:.1} °C)", temp)
            }
            None => "无热区或不可用".to_string(),
        };
        lines.push(format!("  ACPI Thermal:    {}", acpi_status));

        let pawnio = if detect_pawnio_installed() {
            "已安装服务 (仅只读检测，未打开句柄)"
        } else {
            "未安装"
        };
        lines.push(format!("  PawnIO 服务:     {}", pawnio));
        lines.join("\n")
    }

    pub fn release(&mut self) -> bool {
        let was_active = self.is_active();
        self.reset_sources();
        was_active
    }

    pub(crate) fn is_active(&self) -> bool {
        self.acpi_pdh_query != 0
            || self.acpi_pdh_init_attempted
            || self.acpi_unavailable
            || self.active_shm.is_some()
            || self.last_shm_probe.is_some()
    }

    fn reset_sources(&mut self) {
        self.active_shm = None;
        self.last_shm_probe = None;
        self.stale_tracker.reset();
        self.close_acpi_pdh();
        self.acpi_empty_streak = 0;
        self.acpi_unavailable = false;
    }

    fn close_acpi_pdh(&mut self) {
        if self.acpi_pdh_query != 0 {
            unsafe {
                let _ = PdhCloseQuery(self.acpi_pdh_query);
            }
            self.acpi_pdh_query = 0;
            self.acpi_pdh_counter = 0;
        }
        self.acpi_pdh_init_attempted = false;
    }

    fn try_read_shm(
        &mut self,
        kind: ShmSourceKind,
        now_unix: u64,
        now: Instant,
    ) -> Option<ExternalSensorSample> {
        match kind {
            ShmSourceKind::HwInfo => {
                let map = open_hwinfo_mapping().ok()?;
                let parsed = parse_hwinfo_sm2(map.as_bytes()).ok()?;
                if !self.stale_tracker.is_fresh(parsed.poll_time, now_unix, now) {
                    return None;
                }
                has_any_metric(&parsed.sample).then_some(parsed.sample)
            }
            ShmSourceKind::Aida64 => {
                let map = ShmMapping::open(w!("AIDA64_SensorValues")).ok()?;
                parse_aida64_xml(map.as_bytes())
            }
            ShmSourceKind::Afterburner => {
                let map = ShmMapping::open(w!("MAHMSharedMemory")).ok()?;
                let parsed = parse_afterburner_mahm(map.as_bytes()).ok()?;
                if let Some(ts) = parsed.timestamp {
                    if ts > 0 && !self.stale_tracker.is_fresh(ts as i64, now_unix, now) {
                        return None;
                    }
                }
                has_any_metric(&parsed.sample).then_some(parsed.sample)
            }
        }
    }

    fn sample_acpi_thermal(&mut self) -> ExternalSensorSample {
        if self.acpi_unavailable {
            return ExternalSensorSample::default();
        }
        if !self.acpi_pdh_init_attempted {
            self.acpi_pdh_init_attempted = true;
            let (q, c) = init_acpi_thermal_pdh();
            self.acpi_pdh_query = q;
            self.acpi_pdh_counter = c;
            // First PDH sample may already have raw values on instant counters; try collecting once.
        }
        if self.acpi_pdh_query == 0 || self.acpi_pdh_counter == 0 {
            self.acpi_unavailable = true;
            return ExternalSensorSample::default();
        }
        let reading = read_acpi_thermal_max(self.acpi_pdh_query, self.acpi_pdh_counter);
        self.handle_acpi_reading(reading)
    }

    fn handle_acpi_reading(&mut self, reading: Option<f32>) -> ExternalSensorSample {
        if let Some(temp_c) = reading {
            self.acpi_empty_streak = 0;
            ExternalSensorSample {
                provider: "ACPI Thermal",
                cpu_temp_c: Some(temp_c),
                ..Default::default()
            }
        } else {
            self.acpi_empty_streak = self.acpi_empty_streak.saturating_add(1);
            if self.acpi_empty_streak >= 2 {
                self.close_acpi_pdh();
                self.acpi_unavailable = true;
            }
            ExternalSensorSample::default()
        }
    }

    fn probe_acpi_temp_for_diag(&mut self) -> Option<f32> {
        if self.acpi_pdh_query != 0 && self.acpi_pdh_counter != 0 {
            return read_acpi_thermal_max(self.acpi_pdh_query, self.acpi_pdh_counter);
        }
        let (q, c) = init_acpi_thermal_pdh();
        if q == 0 || c == 0 {
            return None;
        }
        let res = read_acpi_thermal_max(q, c);
        unsafe {
            let _ = PdhCloseQuery(q);
        }
        res
    }
}

impl Default for SensorHub {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SensorHub {
    fn drop(&mut self) {
        self.close_acpi_pdh();
    }
}

// ---------------------------------------------------------------------------
// Read-only shared memory mapping with VirtualQuery bounds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShmOpenError {
    NotFound,
    AccessDenied,
    Other,
}

struct ShmMapping {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    size: usize,
}

impl ShmMapping {
    fn open(name: PCWSTR) -> Result<Self, ShmOpenError> {
        unsafe {
            let handle = match OpenFileMappingW(FILE_MAP_READ.0, false, name) {
                Ok(h) => h,
                Err(e) => {
                    return if e.code() == ERROR_ACCESS_DENIED.to_hresult()
                        || GetLastError() == ERROR_ACCESS_DENIED
                    {
                        Err(ShmOpenError::AccessDenied)
                    } else {
                        Err(ShmOpenError::NotFound)
                    };
                }
            };
            if handle.is_invalid() || handle.0.is_null() {
                return Err(ShmOpenError::NotFound);
            }

            let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0);
            if view.Value.is_null() {
                let err = if GetLastError() == ERROR_ACCESS_DENIED {
                    ShmOpenError::AccessDenied
                } else {
                    ShmOpenError::Other
                };
                let _ = CloseHandle(handle);
                return Err(err);
            }

            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            let queried = VirtualQuery(
                Some(view.Value as *const c_void),
                &mut mbi,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            );
            if queried == 0 || mbi.RegionSize == 0 {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(handle);
                return Err(ShmOpenError::Other);
            }

            Ok(Self {
                handle,
                view,
                size: mbi.RegionSize,
            })
        }
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.view.Value as *const u8, self.size) }
    }
}

impl Drop for ShmMapping {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

fn open_hwinfo_mapping() -> Result<ShmMapping, ShmOpenError> {
    match ShmMapping::open(w!("Global\\HWiNFO_SENS_SM2")) {
        Ok(m) => Ok(m),
        Err(ShmOpenError::AccessDenied) => Err(ShmOpenError::AccessDenied),
        Err(_) => ShmMapping::open(w!("HWiNFO_SENS_SM2")),
    }
}

// ---------------------------------------------------------------------------
// HWiNFO64 SM2 parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShmParseError {
    TooSmall,
    InvalidSignature,
    DeadSignature,
    InvalidBounds,
    StaleTimestamp,
}

#[derive(Debug, Clone)]
struct HwinfoParsed {
    poll_time: i64,
    num_sensors: u32,
    num_readings: u32,
    sample: ExternalSensorSample,
}

fn parse_hwinfo_sm2(buf: &[u8]) -> Result<HwinfoParsed, ShmParseError> {
    if buf.len() < HWINFO_HEADER_SIZE {
        return Err(ShmParseError::TooSmall);
    }

    let sig = read_u32_le(buf, 0).ok_or(ShmParseError::TooSmall)?;
    if sig == HWINFO_SIG_DEAD {
        return Err(ShmParseError::DeadSignature);
    }
    if sig != HWINFO_SIG_ACTIVE {
        return Err(ShmParseError::InvalidSignature);
    }

    let poll_time = read_i64_le(buf, 12).ok_or(ShmParseError::TooSmall)?;
    if poll_time <= 0 {
        return Err(ShmParseError::StaleTimestamp);
    }

    let sensor_off = read_u32_le(buf, 20).ok_or(ShmParseError::TooSmall)? as usize;
    let sensor_elem_size = read_u32_le(buf, 24).ok_or(ShmParseError::TooSmall)? as usize;
    let num_sensors = read_u32_le(buf, 28).ok_or(ShmParseError::TooSmall)? as usize;

    let reading_off = read_u32_le(buf, 32).ok_or(ShmParseError::TooSmall)? as usize;
    let reading_elem_size = read_u32_le(buf, 36).ok_or(ShmParseError::TooSmall)? as usize;
    let num_readings = read_u32_le(buf, 40).ok_or(ShmParseError::TooSmall)? as usize;

    if num_sensors > HWINFO_MAX_SENSORS || num_readings > HWINFO_MAX_READINGS {
        return Err(ShmParseError::InvalidBounds);
    }

    if num_sensors > 0 {
        if sensor_off < HWINFO_HEADER_SIZE || sensor_elem_size < HWINFO_MIN_SENSOR_SIZE {
            return Err(ShmParseError::InvalidBounds);
        }
        let sensor_bytes = num_sensors
            .checked_mul(sensor_elem_size)
            .ok_or(ShmParseError::InvalidBounds)?;
        let sensor_end = sensor_off
            .checked_add(sensor_bytes)
            .ok_or(ShmParseError::InvalidBounds)?;
        if sensor_end > buf.len() {
            return Err(ShmParseError::InvalidBounds);
        }
    }

    if num_readings > 0 {
        if reading_off < HWINFO_HEADER_SIZE || reading_elem_size < HWINFO_MIN_READING_SIZE {
            return Err(ShmParseError::InvalidBounds);
        }
        let reading_bytes = num_readings
            .checked_mul(reading_elem_size)
            .ok_or(ShmParseError::InvalidBounds)?;
        let reading_end = reading_off
            .checked_add(reading_bytes)
            .ok_or(ShmParseError::InvalidBounds)?;
        if reading_end > buf.len() {
            return Err(ShmParseError::InvalidBounds);
        }
    }

    let mut sensors = Vec::with_capacity(num_sensors);
    for i in 0..num_sensors {
        let base = sensor_off + i * sensor_elem_size;
        let orig = read_c_string(&buf[base + 8..base + 136]);
        let user = read_c_string(&buf[base + 136..base + 264]);
        let name = if !orig.is_empty() { orig } else { user };
        sensors.push(name);
    }

    let mut sample = ExternalSensorSample {
        provider: "HWiNFO64",
        ..Default::default()
    };

    let mut cpu_temp_prio: u8 = u8::MAX;
    let mut cpu_power_prio: u8 = u8::MAX;
    let mut cpu_fan_prio: u8 = u8::MAX;
    let mut gpu_temp_prio: u8 = u8::MAX;
    let mut gpu_power_prio: u8 = u8::MAX;

    for i in 0..num_readings {
        let base = reading_off + i * reading_elem_size;
        let reading_type = read_u32_le(buf, base).unwrap_or(0);
        let sensor_idx = read_u32_le(buf, base + 4).unwrap_or(u32::MAX) as usize;
        let label_orig = read_c_string(&buf[base + 12..base + 140]);
        let label_user = read_c_string(&buf[base + 140..base + 268]);
        let unit = read_c_string(&buf[base + 268..base + 284]);
        let value = match read_f64_le(buf, base + 284) {
            Some(v) if v.is_finite() => v,
            _ => continue,
        };

        let sensor_name = sensors.get(sensor_idx).map(|s| s.as_str()).unwrap_or("");
        let sensor_l = sensor_name.to_ascii_lowercase();
        let label = if !label_orig.is_empty() {
            &label_orig
        } else {
            &label_user
        };
        let label_l = label.to_ascii_lowercase();
        let unit_l = unit.to_ascii_lowercase();

        match reading_type {
            HWINFO_READING_TEMP => {
                if !(-10.0..=140.0).contains(&value) {
                    continue;
                }
                let val_f32 = value as f32;

                if let Some(prio) = classify_hwinfo_cpu_temp(&sensor_l, &label_l) {
                    if prio < cpu_temp_prio {
                        cpu_temp_prio = prio;
                        sample.cpu_temp_c = Some(val_f32);
                    }
                } else if is_hwinfo_storage_temp(&sensor_l, &label_l) {
                    if (5.0..=110.0).contains(&value) {
                        sample.disk_temp_c =
                            Some(sample.disk_temp_c.map_or(val_f32, |prev| prev.max(val_f32)));
                    }
                } else if is_hwinfo_gpu_sensor(&sensor_l, &label_l) {
                    if label_l.contains("hot spot")
                        || label_l.contains("hotspot")
                        || label_l.contains("junction")
                    {
                        if sample.gpu_hotspot_c.is_none() {
                            sample.gpu_hotspot_c = Some(val_f32);
                        }
                    } else if let Some(prio) = classify_hwinfo_gpu_temp(&label_l) {
                        if prio < gpu_temp_prio {
                            gpu_temp_prio = prio;
                            sample.gpu_temp_c = Some(val_f32);
                        }
                    }
                }
            }
            HWINFO_READING_POWER => {
                if !(0.0..=2000.0).contains(&value) {
                    continue;
                }
                let val_f32 = value as f32;
                if let Some(prio) = classify_hwinfo_cpu_power(&sensor_l, &label_l) {
                    if prio < cpu_power_prio {
                        cpu_power_prio = prio;
                        sample.cpu_power_w = Some(val_f32);
                    }
                } else if is_hwinfo_gpu_sensor(&sensor_l, &label_l) {
                    if let Some(prio) = classify_hwinfo_gpu_power(&label_l) {
                        if prio < gpu_power_prio {
                            gpu_power_prio = prio;
                            sample.gpu_power_w = Some(val_f32);
                        }
                    }
                }
            }
            HWINFO_READING_FAN => {
                if !(0.0..=15_000.0).contains(&value) || unit_l.contains('%') {
                    continue;
                }
                let rpm = value.round() as u32;
                if is_hwinfo_gpu_sensor(&sensor_l, &label_l) {
                    if sample.gpu_fan_rpm.is_none() {
                        sample.gpu_fan_rpm = Some(rpm);
                    }
                } else if let Some(prio) = classify_hwinfo_cpu_fan(&label_l) {
                    if prio < cpu_fan_prio {
                        cpu_fan_prio = prio;
                        sample.cpu_fan_rpm = Some(rpm);
                    }
                }
            }
            _ => {}
        }
    }

    Ok(HwinfoParsed {
        poll_time,
        num_sensors: num_sensors as u32,
        num_readings: num_readings as u32,
        sample,
    })
}

fn classify_hwinfo_cpu_temp(sensor: &str, label: &str) -> Option<u8> {
    if label.contains("gpu")
        || label.contains("vrm")
        || label.contains("pch")
        || label.contains("chipset")
        || label.contains("dimm")
        || label.contains("drive")
        || label.contains("nvme")
        || label.contains("ssd")
    {
        return None;
    }
    if label.contains("cpu (tctl/tdie)") || label == "tctl/tdie" || label.contains("cpu package") {
        return Some(0);
    }
    if label.contains("tdie")
        || label.contains("cpu die (average)")
        || label.contains("package") && is_cpu_sensor(sensor)
    {
        return Some(1);
    }
    if label == "cpu"
        || label == "cpu temp"
        || label == "cpu temperature"
        || label.contains("core max")
    {
        return Some(2);
    }
    if is_cpu_sensor(sensor) && (label.contains("tctl") || label.contains("core temperatures")) {
        return Some(3);
    }
    None
}

fn classify_hwinfo_cpu_power(sensor: &str, label: &str) -> Option<u8> {
    if label.contains("gpu") || is_hwinfo_gpu_sensor(sensor, label) {
        return None;
    }
    if label.contains("cpu package power") || label == "cpu package" || label.contains("cpu ppt") {
        return Some(0);
    }
    if is_cpu_sensor(sensor)
        && (label.contains("package power") || label == "package" || label == "ppt")
    {
        return Some(1);
    }
    if label.contains("cpu") && label.contains("power") && !label.contains("limit") {
        return Some(2);
    }
    None
}

fn classify_hwinfo_cpu_fan(label: &str) -> Option<u8> {
    if label.contains("gpu") {
        return None;
    }
    if label == "cpu" || label == "cpu fan" || label == "cpu_fan" || label.contains("cpu fan") {
        return Some(0);
    }
    if label.contains("cpu_opt")
        || label.contains("cpu opt")
        || label.contains("aio")
        || label.contains("pump")
    {
        return Some(1);
    }
    None
}

fn is_cpu_sensor(sensor: &str) -> bool {
    sensor.contains("cpu")
        || sensor.contains("ryzen")
        || sensor.contains("intel core")
        || sensor.contains("xeon")
        || sensor.contains("epyc")
        || sensor.contains("threadripper")
}

fn is_hwinfo_storage_temp(sensor: &str, label: &str) -> bool {
    if sensor.contains("gpu")
        || sensor.contains("cpu")
        || label.contains("cpu")
        || label.contains("gpu")
    {
        return false;
    }
    if label.contains("vrm") || label.contains("pch") || label.contains("dimm") {
        return false;
    }
    let storage_sensor = sensor.contains("nvme")
        || sensor.contains("ssd")
        || sensor.contains("hdd")
        || sensor.contains("s.m.a.r.t")
        || sensor.contains("smart")
        || sensor.contains("drive")
        || sensor.contains("samsung")
        || sensor.contains("wd_black")
        || sensor.contains("western digital")
        || sensor.contains("crucial")
        || sensor.contains("kingston")
        || sensor.contains("kioxia")
        || sensor.contains("sk hynix")
        || sensor.contains("solidigm")
        || sensor.contains("seagate")
        || sensor.contains("toshiba")
        || sensor.contains("sabrent")
        || sensor.contains("micron");
    let storage_label = label.contains("drive temperature")
        || label.contains("airflow temperature")
        || label.contains("composite temperature")
        || label.contains("ssd temperature")
        || label.contains("nvme");
    (storage_sensor && (label.contains("temp") || label.contains("drive"))) || storage_label
}

fn is_hwinfo_gpu_sensor(sensor: &str, label: &str) -> bool {
    if is_cpu_sensor(sensor) && !label.starts_with("gpu") {
        return false;
    }
    sensor.contains("gpu")
        || sensor.contains("radeon")
        || sensor.contains("geforce")
        || sensor.contains("rtx")
        || sensor.contains("gtx")
        || sensor.contains("arc")
        || sensor.contains("nvidia")
        || label.starts_with("gpu ")
}

fn classify_hwinfo_gpu_temp(label: &str) -> Option<u8> {
    if label.contains("memory")
        || label.contains("vram")
        || label.contains("vr ")
        || label.contains("vrm")
    {
        return None;
    }
    if label == "gpu temperature" || label == "gpu core" || label == "gpu edge" {
        return Some(0);
    }
    if label.contains("gpu") && label.contains("temp") {
        return Some(1);
    }
    None
}

fn classify_hwinfo_gpu_power(label: &str) -> Option<u8> {
    if label.contains("limit") || label.contains("rail") || label.contains("pcie") {
        return None;
    }
    if label.contains("total board power") || label == "gpu power" || label == "tbp" {
        return Some(0);
    }
    if label.contains("gpu chip power")
        || label.contains("asic power")
        || label.contains("gpu core power")
    {
        return Some(1);
    }
    if label.contains("power") {
        return Some(2);
    }
    None
}

// ---------------------------------------------------------------------------
// AIDA64 pseudo-XML parser (zero external crate)
// ---------------------------------------------------------------------------

fn parse_aida64_xml(buf: &[u8]) -> Option<ExternalSensorSample> {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    if end == 0 {
        return None;
    }
    let text = String::from_utf8_lossy(&buf[..end]);
    let mut sample = ExternalSensorSample {
        provider: "AIDA64",
        ..Default::default()
    };

    let mut cpu_temp_prio = u8::MAX;
    let mut cpu_power_prio = u8::MAX;
    let mut cpu_fan_prio = u8::MAX;
    let mut gpu_temp_prio = u8::MAX;
    let mut gpu_power_prio = u8::MAX;

    for (tag, body) in iter_aida64_blocks(&text) {
        let id = extract_xml_tag(body, "id").unwrap_or("").trim();
        let label = extract_xml_tag(body, "label").unwrap_or("").trim();
        let val_str = extract_xml_tag(body, "value").unwrap_or("").trim();
        let Ok(value) = val_str.parse::<f32>() else {
            continue;
        };
        if !value.is_finite() {
            continue;
        }

        let id_u = id.to_ascii_uppercase();
        let label_l = label.to_ascii_lowercase();

        match tag {
            "temp" => {
                if !(-10.0..=140.0).contains(&value) {
                    continue;
                }
                if id_u == "TCPUPKG" || label_l == "cpu package" {
                    if 0 < cpu_temp_prio {
                        cpu_temp_prio = 0;
                        sample.cpu_temp_c = Some(value);
                    }
                } else if id_u == "TCPUDIODE" || label_l == "cpu diode" {
                    if 1 < cpu_temp_prio {
                        cpu_temp_prio = 1;
                        sample.cpu_temp_c = Some(value);
                    }
                } else if id_u == "TCPU" || label_l == "cpu" {
                    if 2 < cpu_temp_prio {
                        cpu_temp_prio = 2;
                        sample.cpu_temp_c = Some(value);
                    }
                } else if id_u.starts_with("THDD")
                    || id_u.starts_with("TSSD")
                    || id_u.starts_with("TNVME")
                    || label_l.contains("nvme")
                    || label_l.contains("ssd")
                    || label_l.contains("hdd")
                    || label_l.contains("samsung")
                    || label_l.contains("wdc")
                    || label_l.contains("kingston")
                    || label_l.contains("crucial")
                {
                    if (5.0..=110.0).contains(&value) {
                        sample.disk_temp_c =
                            Some(sample.disk_temp_c.map_or(value, |p| p.max(value)));
                    }
                } else if id_u == "TGPU1HOT"
                    || label_l.contains("gpu hotspot")
                    || label_l.contains("hot spot")
                {
                    if sample.gpu_hotspot_c.is_none() {
                        sample.gpu_hotspot_c = Some(value);
                    }
                } else if id_u == "TGPU1DIODE" || label_l == "gpu diode" {
                    if 0 < gpu_temp_prio {
                        gpu_temp_prio = 0;
                        sample.gpu_temp_c = Some(value);
                    }
                } else if (id_u == "TGPU1" || label_l == "gpu" || label_l == "gpu temperature")
                    && 1 < gpu_temp_prio
                {
                    gpu_temp_prio = 1;
                    sample.gpu_temp_c = Some(value);
                }
            }
            "pwr" => {
                if !(0.0..=2000.0).contains(&value) {
                    continue;
                }
                if id_u == "PCPUPKG" || label_l == "cpu package" {
                    if 0 < cpu_power_prio {
                        cpu_power_prio = 0;
                        sample.cpu_power_w = Some(value);
                    }
                } else if id_u == "PCPU" || label_l == "cpu" {
                    if 1 < cpu_power_prio {
                        cpu_power_prio = 1;
                        sample.cpu_power_w = Some(value);
                    }
                } else if id_u == "PGPU1" || label_l == "gpu" || label_l == "gpu total" {
                    if 0 < gpu_power_prio {
                        gpu_power_prio = 0;
                        sample.gpu_power_w = Some(value);
                    }
                } else if (id_u == "PGPU1CORE"
                    || label_l.contains("gpu chip")
                    || label_l.contains("gpu core"))
                    && 1 < gpu_power_prio
                {
                    gpu_power_prio = 1;
                    sample.gpu_power_w = Some(value);
                }
            }
            "fan" => {
                if !(0.0..=15_000.0).contains(&value) {
                    continue;
                }
                let rpm = value.round() as u32;
                if id_u == "FCPU" || label_l == "cpu" || label_l == "cpu fan" {
                    if 0 < cpu_fan_prio {
                        cpu_fan_prio = 0;
                        sample.cpu_fan_rpm = Some(rpm);
                    }
                } else if id_u == "FAIO"
                    || label_l.contains("aio")
                    || label_l.contains("pump")
                    || label_l.contains("cpu opt")
                {
                    if 1 < cpu_fan_prio {
                        cpu_fan_prio = 1;
                        sample.cpu_fan_rpm = Some(rpm);
                    }
                } else if (id_u.starts_with("FGPU") || label_l.contains("gpu"))
                    && sample.gpu_fan_rpm.is_none()
                {
                    sample.gpu_fan_rpm = Some(rpm);
                }
            }
            _ => {}
        }
    }

    has_any_metric(&sample).then_some(sample)
}

fn iter_aida64_blocks(input: &str) -> Vec<(&'static str, &str)> {
    const TAGS: [(&str, &str, &str); 3] = [
        ("temp", "<temp>", "</temp>"),
        ("pwr", "<pwr>", "</pwr>"),
        ("fan", "<fan>", "</fan>"),
    ];
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < input.len() {
        let rest = &input[pos..];
        let mut next_match: Option<(usize, &'static str, &str, &str)> = None;
        for &(tag, open, close) in &TAGS {
            if let Some(idx) = rest.find(open) {
                if next_match.is_none_or(|(best_idx, _, _, _)| idx < best_idx) {
                    next_match = Some((idx, tag, open, close));
                }
            }
        }
        let Some((rel_idx, tag, open, close)) = next_match else {
            break;
        };
        let content_start = pos + rel_idx + open.len();
        if let Some(end_rel) = input[content_start..].find(close) {
            let content_end = content_start + end_rel;
            out.push((tag, &input[content_start..content_end]));
            pos = content_end + close.len();
        } else {
            // Malformed unclosed tag: advance past opening tag so we don't loop forever.
            pos = content_start;
        }
    }
    out
}

fn extract_xml_tag<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(&body[start..end])
}

// ---------------------------------------------------------------------------
// MSI Afterburner MAHMSharedMemory parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct MahmParsed {
    timestamp: Option<u32>,
    num_entries: u32,
    sample: ExternalSensorSample,
}

fn parse_afterburner_mahm(buf: &[u8]) -> Result<MahmParsed, ShmParseError> {
    if buf.len() < MAHM_MIN_HEADER_SIZE {
        return Err(ShmParseError::TooSmall);
    }

    let sig = read_u32_le(buf, 0).ok_or(ShmParseError::TooSmall)?;
    if sig == MAHM_SIG_DEAD {
        return Err(ShmParseError::DeadSignature);
    }
    if sig != MAHM_SIG_ACTIVE {
        return Err(ShmParseError::InvalidSignature);
    }

    let header_size = read_u32_le(buf, 8).ok_or(ShmParseError::TooSmall)? as usize;
    let num_entries = read_u32_le(buf, 12).ok_or(ShmParseError::TooSmall)? as usize;
    let entry_size = read_u32_le(buf, 16).ok_or(ShmParseError::TooSmall)? as usize;

    if header_size < MAHM_MIN_HEADER_SIZE || header_size > buf.len() {
        return Err(ShmParseError::InvalidBounds);
    }
    if num_entries > MAHM_MAX_ENTRIES {
        return Err(ShmParseError::InvalidBounds);
    }
    if num_entries > 0 {
        if entry_size < MAHM_MIN_ENTRY_SIZE {
            return Err(ShmParseError::InvalidBounds);
        }
        let entries_bytes = num_entries
            .checked_mul(entry_size)
            .ok_or(ShmParseError::InvalidBounds)?;
        let entries_end = header_size
            .checked_add(entries_bytes)
            .ok_or(ShmParseError::InvalidBounds)?;
        if entries_end > buf.len() {
            return Err(ShmParseError::InvalidBounds);
        }
    }

    let timestamp = if header_size >= 24 && buf.len() >= 24 {
        read_u32_le(buf, 20)
    } else {
        None
    };

    let mut sample = ExternalSensorSample {
        provider: "MSI Afterburner",
        ..Default::default()
    };

    for i in 0..num_entries {
        let base = header_size + i * entry_size;
        let name = read_c_string(&buf[base..base + 260]);
        let units = read_c_string(&buf[base + 260..base + 520]);
        let data = match read_f32_le(buf, base + 1300) {
            Some(v) if v.is_finite() && v < 1e30 => v,
            _ => continue,
        };

        let name_l = name.to_ascii_lowercase();
        let units_l = units.to_ascii_lowercase();

        if name_l == "cpu temperature"
            || (name_l.starts_with("cpu") && name_l.contains("temperature"))
        {
            if (-10.0..=140.0).contains(&data) && sample.cpu_temp_c.is_none() {
                sample.cpu_temp_c = Some(data);
            }
        } else if name_l == "cpu power" || (name_l.starts_with("cpu") && name_l.contains("power")) {
            if (0.0..=1500.0).contains(&data) && sample.cpu_power_w.is_none() {
                sample.cpu_power_w = Some(data);
            }
        } else if name_l == "gpu temperature" || name_l == "gpu1 temperature" {
            if (-10.0..=140.0).contains(&data) && sample.gpu_temp_c.is_none() {
                sample.gpu_temp_c = Some(data);
            }
        } else if name_l == "gpu power" || name_l == "gpu1 power" {
            if (0.0..=2000.0).contains(&data) && sample.gpu_power_w.is_none() {
                sample.gpu_power_w = Some(data);
            }
        } else if (name_l.contains("fan tachometer")
            || ((name_l.contains("fan speed") || name_l == "gpu fan") && units_l.contains("rpm")))
            && !name_l.contains("cpu")
            && (0.0..=15_000.0).contains(&data)
            && sample.gpu_fan_rpm.is_none()
        {
            sample.gpu_fan_rpm = Some(data.round() as u32);
        }
    }

    Ok(MahmParsed {
        timestamp,
        num_entries: num_entries as u32,
        sample,
    })
}

// ---------------------------------------------------------------------------
// Windows ACPI Thermal Zone (PDH fallback)
// ---------------------------------------------------------------------------

fn init_acpi_thermal_pdh() -> (isize, isize) {
    unsafe {
        let mut query: isize = 0;
        if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
            return (0, 0);
        }
        let path = w!("\\Thermal Zone Information(*)\\Temperature");
        let mut counter: isize = 0;
        if PdhAddEnglishCounterW(query, path, 0, &mut counter) != 0 || counter == 0 {
            let _ = PdhCloseQuery(query);
            return (0, 0);
        }
        let _ = PdhCollectQueryData(query);
        (query, counter)
    }
}

fn read_acpi_thermal_max(query: isize, counter: isize) -> Option<f32> {
    if query == 0 || counter == 0 {
        return None;
    }
    unsafe {
        if PdhCollectQueryData(query) != 0 {
            return None;
        }
        let mut buf_size: u32 = 0;
        let mut item_count: u32 = 0;
        let status = PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_DOUBLE,
            &mut buf_size,
            &mut item_count,
            None,
        );
        // PDH_MORE_DATA = 0x800007D2
        if status != 0x8000_07D2 || buf_size == 0 || item_count == 0 {
            return None;
        }
        let elem_size = size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let cap = (buf_size as usize).div_ceil(elem_size);
        let mut buf: Vec<PDH_FMT_COUNTERVALUE_ITEM_W> = vec![zeroed(); cap];
        let status2 = PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_DOUBLE,
            &mut buf_size,
            &mut item_count,
            Some(buf.as_mut_ptr()),
        );
        if status2 != 0 || item_count == 0 {
            return None;
        }

        let mut max_temp: Option<f32> = None;
        for item in buf.iter().take(item_count as usize) {
            if item.FmtValue.CStatus <= 1 {
                if let Some(c) = normalize_acpi_temp_c(item.FmtValue.Anonymous.doubleValue) {
                    max_temp = Some(max_temp.map_or(c, |prev| prev.max(c)));
                }
            }
        }
        max_temp
    }
}

fn normalize_acpi_temp_c(raw: f64) -> Option<f32> {
    if !raw.is_finite() {
        return None;
    }
    let celsius = if (200.0..=450.0).contains(&raw) {
        raw - 273.15
    } else if (2000.0..=4500.0).contains(&raw) {
        (raw / 10.0) - 273.15
    } else {
        raw
    };
    (5.0..=120.0).contains(&celsius).then_some(celsius as f32)
}

// ---------------------------------------------------------------------------
// Diagnostics helpers
// ---------------------------------------------------------------------------

fn diagnose_hwinfo() -> String {
    match open_hwinfo_mapping() {
        Ok(map) => match parse_hwinfo_sm2(map.as_bytes()) {
            Ok(parsed) => format!(
                "可用 ({} 传感器, {} 读数)",
                parsed.num_sensors, parsed.num_readings
            ),
            Err(ShmParseError::DeadSignature) => "已停止 (DEAD 签名)".to_string(),
            Err(_) => "已运行但结构无效/已过期".to_string(),
        },
        Err(ShmOpenError::AccessDenied) => "无权限跳过".to_string(),
        Err(_) => "未运行".to_string(),
    }
}

fn diagnose_aida64() -> String {
    match ShmMapping::open(w!("AIDA64_SensorValues")) {
        Ok(map) => {
            let bytes = map.as_bytes();
            let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            if parse_aida64_xml(bytes).is_some() {
                format!("可用 ({} 字节)", len)
            } else {
                format!("共享内存存在但无有效传感器 ({} 字节)", len)
            }
        }
        Err(ShmOpenError::AccessDenied) => "无权限跳过".to_string(),
        Err(_) => "未运行".to_string(),
    }
}

fn diagnose_afterburner() -> String {
    match ShmMapping::open(w!("MAHMSharedMemory")) {
        Ok(map) => match parse_afterburner_mahm(map.as_bytes()) {
            Ok(parsed) => format!("可用 ({} 项)", parsed.num_entries),
            Err(ShmParseError::DeadSignature) => "已停止 (DEAD 签名)".to_string(),
            Err(_) => "已运行但结构无效".to_string(),
        },
        Err(ShmOpenError::AccessDenied) => "无权限跳过".to_string(),
        Err(_) => "未运行".to_string(),
    }
}

fn detect_pawnio_installed() -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        let status = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!("SYSTEM\\CurrentControlSet\\Services\\PawnIO"),
            0,
            KEY_READ,
            &mut hkey,
        );
        if status.is_ok() {
            let _ = RegCloseKey(hkey);
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Binary & general helpers
// ---------------------------------------------------------------------------

fn has_any_metric(s: &ExternalSensorSample) -> bool {
    s.cpu_temp_c.is_some()
        || s.cpu_power_w.is_some()
        || s.cpu_fan_rpm.is_some()
        || s.disk_temp_c.is_some()
        || s.gpu_temp_c.is_some()
        || s.gpu_hotspot_c.is_some()
        || s.gpu_power_w.is_some()
        || s.gpu_fan_rpm.is_some()
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_u32_le(buf: &[u8], off: usize) -> Option<u32> {
    let bytes: [u8; 4] = buf.get(off..off + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn read_i64_le(buf: &[u8], off: usize) -> Option<i64> {
    let bytes: [u8; 8] = buf.get(off..off + 8)?.try_into().ok()?;
    Some(i64::from_le_bytes(bytes))
}

fn read_f32_le(buf: &[u8], off: usize) -> Option<f32> {
    let bytes: [u8; 4] = buf.get(off..off + 4)?.try_into().ok()?;
    Some(f32::from_le_bytes(bytes))
}

fn read_f64_le(buf: &[u8], off: usize) -> Option<f64> {
    let bytes: [u8; 8] = buf.get(off..off + 8)?.try_into().ok()?;
    Some(f64::from_le_bytes(bytes))
}

fn read_c_string(slice: &[u8]) -> String {
    let len = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..len]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_c_str(dst: &mut [u8], s: &str) {
        let bytes = s.as_bytes();
        let n = bytes.len().min(dst.len().saturating_sub(1));
        dst[..n].copy_from_slice(&bytes[..n]);
        if n < dst.len() {
            dst[n] = 0;
        }
    }

    fn build_hwinfo_buf(
        sig: u32,
        poll_time: i64,
        sensors: &[&str],
        readings: &[(u32, u32, &str, &str, f64)],
    ) -> Vec<u8> {
        let sensor_off = HWINFO_HEADER_SIZE;
        let sensor_sz = HWINFO_MIN_SENSOR_SIZE;
        let reading_off = sensor_off + sensors.len() * sensor_sz;
        let reading_sz = HWINFO_MIN_READING_SIZE;
        let total = reading_off + readings.len() * reading_sz;

        let mut buf = vec![0u8; total];
        buf[0..4].copy_from_slice(&sig.to_le_bytes());
        buf[4..8].copy_from_slice(&1u32.to_le_bytes());
        buf[8..12].copy_from_slice(&0u32.to_le_bytes());
        buf[12..20].copy_from_slice(&poll_time.to_le_bytes());
        buf[20..24].copy_from_slice(&(sensor_off as u32).to_le_bytes());
        buf[24..28].copy_from_slice(&(sensor_sz as u32).to_le_bytes());
        buf[28..32].copy_from_slice(&(sensors.len() as u32).to_le_bytes());
        buf[32..36].copy_from_slice(&(reading_off as u32).to_le_bytes());
        buf[36..40].copy_from_slice(&(reading_sz as u32).to_le_bytes());
        buf[40..44].copy_from_slice(&(readings.len() as u32).to_le_bytes());

        for (i, name) in sensors.iter().enumerate() {
            let base = sensor_off + i * sensor_sz;
            write_c_str(&mut buf[base + 8..base + 136], name);
        }
        for (i, &(rtype, sidx, label, unit, val)) in readings.iter().enumerate() {
            let base = reading_off + i * reading_sz;
            buf[base..base + 4].copy_from_slice(&rtype.to_le_bytes());
            buf[base + 4..base + 8].copy_from_slice(&sidx.to_le_bytes());
            write_c_str(&mut buf[base + 12..base + 140], label);
            write_c_str(&mut buf[base + 268..base + 284], unit);
            buf[base + 284..base + 292].copy_from_slice(&val.to_le_bytes());
        }
        buf
    }

    #[test]
    fn sensor_hub_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SensorHub>();
    }

    #[test]
    fn sensor_hub_disabled_returns_empty_sample() {
        let mut hub = SensorHub::default();
        let sample = hub.sample(false);
        assert_eq!(sample.provider, "");
        assert!(sample.cpu_temp_c.is_none());
        assert!(sample.cpu_power_w.is_none());
        assert!(sample.cpu_fan_rpm.is_none());
        assert!(sample.disk_temp_c.is_none());
        assert!(sample.gpu_temp_c.is_none());
    }

    #[test]
    fn hwinfo_sm2_parses_valid_and_rejects_corrupt_buffers() {
        let buf = build_hwinfo_buf(
            HWINFO_SIG_ACTIVE,
            1_700_000_123,
            &[
                "CPU [#0]: AMD Ryzen 7 7800X3D",
                "ASUS ROG STRIX B650E",
                "S.M.A.R.T.: Samsung SSD 990 PRO 2TB",
                "S.M.A.R.T.: WD_BLACK SN850X",
                "GPU [#0]: AMD Radeon RX 7900 XT",
            ],
            &[
                (HWINFO_READING_TEMP, 0, "CPU (Tctl/Tdie)", "C", 64.5),
                (HWINFO_READING_POWER, 0, "CPU Package Power", "W", 58.2),
                (HWINFO_READING_FAN, 1, "CPU_FAN", "RPM", 1180.0),
                (HWINFO_READING_TEMP, 2, "Drive Temperature", "C", 41.0),
                (HWINFO_READING_TEMP, 3, "Drive Temperature", "C", 49.0),
                (HWINFO_READING_TEMP, 4, "GPU Temperature", "C", 55.0),
                (
                    HWINFO_READING_TEMP,
                    4,
                    "GPU Hot Spot Temperature",
                    "C",
                    71.0,
                ),
                (HWINFO_READING_POWER, 4, "Total Board Power", "W", 215.5),
                (HWINFO_READING_FAN, 4, "GPU Fan1", "RPM", 1620.0),
            ],
        );

        let parsed = parse_hwinfo_sm2(&buf).expect("valid HWiNFO buffer");
        assert_eq!(parsed.poll_time, 1_700_000_123);
        assert_eq!(parsed.sample.provider, "HWiNFO64");
        assert!((parsed.sample.cpu_temp_c.unwrap() - 64.5).abs() < 0.01);
        assert!((parsed.sample.cpu_power_w.unwrap() - 58.2).abs() < 0.01);
        assert_eq!(parsed.sample.cpu_fan_rpm, Some(1180));
        assert!((parsed.sample.disk_temp_c.unwrap() - 49.0).abs() < 0.01);
        assert!((parsed.sample.gpu_temp_c.unwrap() - 55.0).abs() < 0.01);
        assert!((parsed.sample.gpu_hotspot_c.unwrap() - 71.0).abs() < 0.01);
        assert!((parsed.sample.gpu_power_w.unwrap() - 215.5).abs() < 0.01);
        assert_eq!(parsed.sample.gpu_fan_rpm, Some(1620));

        // Truncated header
        assert_eq!(
            parse_hwinfo_sm2(&buf[..20]).unwrap_err(),
            ShmParseError::TooSmall
        );
        // Truncated readings section
        assert_eq!(
            parse_hwinfo_sm2(&buf[..buf.len() - 10]).unwrap_err(),
            ShmParseError::InvalidBounds
        );
        // Dead signature
        let mut dead = buf.clone();
        dead[0..4].copy_from_slice(&HWINFO_SIG_DEAD.to_le_bytes());
        assert_eq!(
            parse_hwinfo_sm2(&dead).unwrap_err(),
            ShmParseError::DeadSignature
        );
        // Wrong signature
        let mut bad_sig = buf.clone();
        bad_sig[0..4].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        assert_eq!(
            parse_hwinfo_sm2(&bad_sig).unwrap_err(),
            ShmParseError::InvalidSignature
        );
        // Zero poll_time rejected
        let mut stale = buf.clone();
        stale[12..20].copy_from_slice(&0i64.to_le_bytes());
        assert_eq!(
            parse_hwinfo_sm2(&stale).unwrap_err(),
            ShmParseError::StaleTimestamp
        );
        // Integer overflow in num_readings * reading_elem_size rejected safely
        let mut overflow = buf;
        overflow[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            parse_hwinfo_sm2(&overflow).unwrap_err(),
            ShmParseError::InvalidBounds
        );
    }

    #[test]
    fn hwinfo_stale_tracker_detects_frozen_and_old_epoch() {
        let mut tracker = StaleTracker::default();
        let now = Instant::now();
        let unix_now = 1_750_000_000;
        assert!(tracker.is_fresh(1_750_000_000, unix_now, now));
        assert!(!tracker.is_fresh(1_749_999_900, unix_now, now));
        assert!(tracker.is_fresh(1_750_000_000, unix_now, now + Duration::from_secs(5)));
        assert!(!tracker.is_fresh(1_750_000_000, unix_now, now + Duration::from_secs(20)));
    }

    #[test]
    fn aida64_xml_parses_valid_and_survives_malformed_fragments() {
        let xml = b"<temp><id>TCPUPKG</id><label>CPU Package</label><value>52.5</value></temp>\
            <temp><id>THDD1</id><label>Samsung SSD</label><value>38</value></temp>\
            <temp><id>THDD2</id><label>WD NVMe</label><value>46</value></temp>\
            <temp><id>TGPU1DIODE</id><label>GPU Diode</label><value>61</value></temp>\
            <pwr><id>PCPUPKG</id><label>CPU Package</label><value>42.3</value></pwr>\
            <pwr><id>PGPU1</id><label>GPU</label><value>180.5</value></pwr>\
            <fan><id>FCPU</id><label>CPU</label><value>1320</value></fan>\
            <fan><id>FGPU1</id><label>GPU</label><value>1500</value></fan>\0garbage_after_nul";
        let sample = parse_aida64_xml(xml).expect("valid AIDA64 XML");
        assert_eq!(sample.provider, "AIDA64");
        assert!((sample.cpu_temp_c.unwrap() - 52.5).abs() < 0.01);
        assert!((sample.disk_temp_c.unwrap() - 46.0).abs() < 0.01);
        assert!((sample.gpu_temp_c.unwrap() - 61.0).abs() < 0.01);
        assert!((sample.cpu_power_w.unwrap() - 42.3).abs() < 0.01);
        assert!((sample.gpu_power_w.unwrap() - 180.5).abs() < 0.01);
        assert_eq!(sample.cpu_fan_rpm, Some(1320));
        assert_eq!(sample.gpu_fan_rpm, Some(1500));

        // Malformed / unclosed / empty
        assert!(parse_aida64_xml(b"\0<temp><id>TCPU</id>").is_none());
        assert!(parse_aida64_xml(b"<temp><id>TCPU</id><value>NaN</value>").is_none());
        let partial = parse_aida64_xml(
            b"<temp><id>TCPU</id><value>bad</value></temp><temp><id>TCPU</id><value>49</value></temp>",
        )
        .expect("second valid temp parsed");
        assert!((partial.cpu_temp_c.unwrap() - 49.0).abs() < 0.01);
    }

    #[test]
    fn afterburner_mahm_parses_valid_and_rejects_oob() {
        let header_size = 32usize;
        let entry_size = MAHM_MIN_ENTRY_SIZE;
        let entries: [(&str, &str, f32); 6] = [
            ("CPU temperature", "C", 63.0),
            ("CPU power", "W", 55.5),
            ("GPU temperature", "C", 59.0),
            ("GPU power", "W", 190.2),
            ("Fan speed", "%", 45.0), // percentage must NOT populate gpu_fan_rpm
            ("Fan tachometer", "RPM", 1420.0), // RPM populates gpu_fan_rpm
        ];
        let mut buf = vec![0u8; header_size + entries.len() * entry_size];
        buf[0..4].copy_from_slice(&MAHM_SIG_ACTIVE.to_le_bytes());
        buf[4..8].copy_from_slice(&0x0002_0000u32.to_le_bytes());
        buf[8..12].copy_from_slice(&(header_size as u32).to_le_bytes());
        buf[12..16].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        buf[16..20].copy_from_slice(&(entry_size as u32).to_le_bytes());
        buf[20..24].copy_from_slice(&1_700_000_000u32.to_le_bytes());

        for (i, &(name, unit, val)) in entries.iter().enumerate() {
            let base = header_size + i * entry_size;
            write_c_str(&mut buf[base..base + 260], name);
            write_c_str(&mut buf[base + 260..base + 520], unit);
            buf[base + 1300..base + 1304].copy_from_slice(&val.to_le_bytes());
        }

        let parsed = parse_afterburner_mahm(&buf).expect("valid MAHM buffer");
        assert_eq!(parsed.sample.provider, "MSI Afterburner");
        assert!((parsed.sample.cpu_temp_c.unwrap() - 63.0).abs() < 0.01);
        assert!((parsed.sample.cpu_power_w.unwrap() - 55.5).abs() < 0.01);
        assert!((parsed.sample.gpu_temp_c.unwrap() - 59.0).abs() < 0.01);
        assert!((parsed.sample.gpu_power_w.unwrap() - 190.2).abs() < 0.01);
        assert_eq!(parsed.sample.gpu_fan_rpm, Some(1420));

        // Truncated entry buffer
        assert_eq!(
            parse_afterburner_mahm(&buf[..buf.len() - 4]).unwrap_err(),
            ShmParseError::InvalidBounds
        );
        // Wrong signature
        let mut bad = buf;
        bad[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        assert_eq!(
            parse_afterburner_mahm(&bad).unwrap_err(),
            ShmParseError::InvalidSignature
        );
    }

    #[test]
    fn acpi_thermal_kelvin_conversion_and_range_validation() {
        assert!((normalize_acpi_temp_c(315.15).unwrap() - 42.0).abs() < 0.01);
        assert!((normalize_acpi_temp_c(3181.5).unwrap() - 45.0).abs() < 0.01);
        assert!((normalize_acpi_temp_c(55.0).unwrap() - 55.0).abs() < 0.01);
        assert!(normalize_acpi_temp_c(0.0).is_none());
        assert!(normalize_acpi_temp_c(273.15).is_none()); // 0.0 C < 5.0
        assert!(normalize_acpi_temp_c(410.0).is_none()); // 136.85 C > 120.0
        assert!(normalize_acpi_temp_c(f64::NAN).is_none());
    }

    #[test]
    fn acpi_consecutive_empty_samples_disable_acpi_pdh_and_release_resets() {
        let mut hub = SensorHub::new();
        assert!(!hub.is_active());

        let empty1 = hub.handle_acpi_reading(None);
        assert_eq!(empty1.provider, "");
        assert_eq!(hub.acpi_empty_streak, 1);
        assert!(!hub.acpi_unavailable);

        // A valid reading resets the streak.
        let valid = hub.handle_acpi_reading(Some(44.5));
        assert_eq!(valid.provider, "ACPI Thermal");
        assert_eq!(valid.cpu_temp_c, Some(44.5));
        assert_eq!(hub.acpi_empty_streak, 0);
        assert!(!hub.acpi_unavailable);

        // Two consecutive empty readings disable ACPI PDH.
        let _ = hub.handle_acpi_reading(None);
        assert_eq!(hub.acpi_empty_streak, 1);
        assert!(!hub.acpi_unavailable);

        let _ = hub.handle_acpi_reading(None);
        assert_eq!(hub.acpi_empty_streak, 2);
        assert!(hub.acpi_unavailable);
        assert_eq!(hub.acpi_pdh_query, 0);
        assert!(!hub.acpi_pdh_init_attempted);

        // Once unavailable, sample_acpi_thermal skips PDH init/collect completely.
        let skipped = hub.sample_acpi_thermal();
        assert_eq!(skipped.provider, "");
        assert!(!hub.acpi_pdh_init_attempted);

        // release() resets ACPI unavailable state and is idempotent.
        assert!(hub.release());
        assert!(!hub.acpi_unavailable);
        assert_eq!(hub.acpi_empty_streak, 0);
        assert!(!hub.is_active());
        assert!(!hub.release());
    }
}
