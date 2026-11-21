use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::mem::{size_of, zeroed};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;
use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HANDLE, HMODULE, LUID};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
};
use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

use super::pdh::PdhBackoff;

const VENDOR_AMD: u32 = 0x1002;
const VENDOR_NVIDIA: u32 = 0x10DE;
const VENDOR_INTEL: u32 = 0x8086;
const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GpuDemand {
    pub usage: bool,
    pub temp: bool,
    pub hotspot: bool,
    pub power: bool,
    pub fan: bool,
    pub vram: bool,
}

impl GpuDemand {
    pub fn all() -> Self {
        Self {
            usage: true,
            temp: true,
            hotspot: true,
            power: true,
            fan: true,
            vram: true,
        }
    }

    pub fn none() -> Self {
        Self::default()
    }

    pub fn any(&self) -> bool {
        self.usage || self.temp || self.hotspot || self.power || self.fan || self.vram
    }
}

#[derive(Debug, Clone, Default)]
pub struct GpuSample {
    pub name: String,
    pub usage_pct: Option<f32>,
    pub temp_c: Option<f32>,
    pub hotspot_c: Option<f32>,
    pub power_w: Option<f32>,
    pub fan_rpm: Option<u32>,
    pub fan_pct: Option<u32>,
    pub vram_used_gb: Option<f32>,
    pub vram_total_gb: Option<f32>,
    pub provider: &'static str,
    pub(crate) multi_gpu: bool,
}

#[derive(Debug, Clone)]
struct DxgiGpuInfo {
    name: String,
    vendor_id: u32,
    device_id: u32,
    subsys_id: u32,
    luid: LUID,
    vram_total_gb: f32,
    multi_gpu: bool,
}

pub struct GpuMonitor {
    dxgi: Option<DxgiGpuInfo>,
    amd: Option<AmdAdl>,
    nvidia: Option<NvidiaNvml>,
    intel: Option<IntelIgcl>,
    amd_status: String,
    nvidia_status: String,
    intel_status: String,
    pdh_usage_query: isize,
    pdh_usage_counter: isize,
    pdh_usage_init_attempted: bool,
    pdh_usage_invalid_streak: u8,
    pdh_usage_backoff: PdhBackoff,
    pdh_vram_query: isize,
    pdh_vram_counter: isize,
    pdh_vram_init_attempted: bool,
    pdh_vram_invalid_streak: u8,
    pdh_vram_backoff: PdhBackoff,
}

impl GpuMonitor {
    pub fn new() -> Self {
        Self::new_with_demand(true, true)
    }

    pub fn new_with_demand(need_usage_pdh: bool, need_vram_pdh: bool) -> Self {
        let dxgi = select_primary_dxgi_gpu();

        let (amd, amd_status) = match dxgi.as_ref() {
            Some(info) if info.vendor_id == VENDOR_AMD => match AmdAdl::try_new(Some(info)) {
                Ok(a) => {
                    let s = format!("已加载 ({})", a.adapter_name);
                    (Some(a), s)
                }
                Err(reason) => (None, reason),
            },
            None => match AmdAdl::try_new(None) {
                Ok(a) => {
                    let s = format!("已加载 ({})", a.adapter_name);
                    (Some(a), s)
                }
                Err(reason) => (None, reason),
            },
            _ => (None, "未匹配厂商".to_string()),
        };

        let (nvidia, nvidia_status) = match dxgi.as_ref() {
            Some(info) if info.vendor_id == VENDOR_NVIDIA => {
                match NvidiaNvml::try_new(Some(info)) {
                    Ok(nv) => {
                        let s = format!("已加载 ({})", nv.device_name);
                        (Some(nv), s)
                    }
                    Err(reason) => (None, reason),
                }
            }
            None if amd.is_none() => match NvidiaNvml::try_new(None) {
                Ok(nv) => {
                    let s = format!("已加载 ({})", nv.device_name);
                    (Some(nv), s)
                }
                Err(reason) => (None, reason),
            },
            _ => (None, "未匹配厂商".to_string()),
        };

        let should_try_intel = matches!(dxgi.as_ref(), Some(info) if info.vendor_id == VENDOR_INTEL)
            || (dxgi.is_none() && amd.is_none() && nvidia.is_none());
        let (intel, intel_status) = if should_try_intel {
            match IntelIgcl::try_new(dxgi.as_ref()) {
                Ok(igcl) => {
                    let s = format!("已加载 ({})", igcl.device_name);
                    (Some(igcl), s)
                }
                Err(reason) => (None, reason),
            }
        } else {
            (None, "未匹配厂商".to_string())
        };

        let luid = dxgi.as_ref().map(|d| d.luid);
        let eager_vram_pdh = need_vram_pdh
            && luid.is_some()
            && !nvidia
                .as_ref()
                .is_some_and(|nv| nv.fn_get_mem_info.is_some());
        let mut pdh_vram_backoff = PdhBackoff::default();
        let (pdh_vram_query, pdh_vram_counter) = if eager_vram_pdh {
            let (q, c) = init_gpu_vram_pdh(luid);
            if q == 0 {
                pdh_vram_backoff.record_failure(Instant::now());
            }
            (q, c)
        } else {
            (0, 0)
        };

        // Only open the heavy per-process GPU Engine(*engtype_3D) PDH query upfront when
        // demanded and no vendor telemetry API is available; otherwise defer until needed.
        let has_vendor = amd.is_some() || nvidia.is_some() || intel.is_some();
        let eager_usage_pdh =
            need_usage_pdh && luid.is_some() && should_eagerly_init_usage_pdh(has_vendor, false);
        let mut pdh_usage_backoff = PdhBackoff::default();
        let (pdh_usage_query, pdh_usage_counter) = if eager_usage_pdh {
            let (q, c) = init_gpu_usage_pdh(luid);
            if q == 0 {
                pdh_usage_backoff.record_failure(Instant::now());
            }
            (q, c)
        } else {
            (0, 0)
        };

        Self {
            dxgi,
            amd,
            nvidia,
            intel,
            amd_status,
            nvidia_status,
            intel_status,
            pdh_usage_query,
            pdh_usage_counter,
            pdh_usage_init_attempted: eager_usage_pdh,
            pdh_usage_invalid_streak: 0,
            pdh_usage_backoff,
            pdh_vram_query,
            pdh_vram_counter,
            pdh_vram_init_attempted: eager_vram_pdh,
            pdh_vram_invalid_streak: 0,
            pdh_vram_backoff,
        }
    }

    pub fn sample(&mut self) -> GpuSample {
        self.sample_with_gpu_demand(GpuDemand::all())
    }

    pub fn sample_with_demand(&mut self, need_usage_pdh: bool, need_vram: bool) -> GpuSample {
        self.sample_with_gpu_demand(GpuDemand {
            usage: need_usage_pdh,
            temp: true,
            hotspot: true,
            power: true,
            fan: true,
            vram: need_vram,
        })
    }

    pub fn sample_with_gpu_demand(&mut self, demand: GpuDemand) -> GpuSample {
        let mut out = GpuSample::default();
        let now = Instant::now();

        if let Some(info) = &self.dxgi {
            out.name = info.name.clone();
            out.vram_total_gb = Some(info.vram_total_gb);
            out.multi_gpu = info.multi_gpu;
        }

        if !demand.any() {
            return out;
        }

        if let Some(amd) = &self.amd {
            let t = amd.sample_with_demand(demand);
            if out.name.is_empty() {
                out.name = amd.adapter_name.clone();
            }
            out.usage_pct = t.usage_pct;
            out.temp_c = t.temp_c;
            out.hotspot_c = t.hotspot_c;
            out.power_w = t.power_w;
            out.fan_rpm = t.fan_rpm;
            out.fan_pct = t.fan_pct;
            out.provider = "AMD ADL2";
        } else if let Some(nv) = &self.nvidia {
            let t = nv.sample_with_demand(demand);
            if out.name.is_empty() {
                out.name = nv.device_name.clone();
            }
            out.usage_pct = t.usage_pct;
            out.temp_c = t.temp_c;
            out.power_w = t.power_w;
            out.fan_rpm = t.fan_rpm;
            out.fan_pct = t.fan_pct;
            out.vram_used_gb = t.vram_used_gb;
            if t.vram_total_gb.is_some() {
                out.vram_total_gb = t.vram_total_gb;
            }
            out.provider = "NVIDIA NVML";
        } else if let Some(igcl) = &mut self.intel {
            let t = igcl.sample_with_demand(demand);
            if out.name.is_empty() {
                out.name = igcl.device_name.clone();
            }
            out.usage_pct = t.usage_pct;
            out.temp_c = t.temp_c;
            out.power_w = t.power_w;
            out.fan_rpm = t.fan_rpm;
            out.fan_pct = t.fan_pct;
            out.provider = "Intel IGCL";
        } else if self.dxgi.is_some() || self.pdh_usage_query != 0 || self.pdh_vram_query != 0 {
            out.provider = "DXGI/PDH";
        }

        let luid = self.dxgi.as_ref().map(|d| d.luid);
        let luid_filter = luid.map(format_pdh_luid_tag);

        if demand.usage && out.usage_pct.is_none() && luid.is_some() {
            out.usage_pct = self
                .sample_usage_pdh_at(luid, luid_filter.as_deref(), now)
                .map(|v| v.clamp(0.0, 100.0) as f32);
        }

        if demand.vram && out.vram_used_gb.is_none() && luid.is_some() {
            out.vram_used_gb = self
                .sample_vram_pdh_at(luid, luid_filter.as_deref(), now)
                .map(|bytes| (bytes / BYTES_PER_GB) as f32);
        }

        out
    }

    fn sample_usage_pdh_at(
        &mut self,
        luid: Option<LUID>,
        luid_tag: Option<&str>,
        now: Instant,
    ) -> Option<f64> {
        if self.pdh_usage_query == 0 || self.pdh_usage_counter == 0 {
            if !self.pdh_usage_backoff.can_attempt(now) {
                return None;
            }
            self.pdh_usage_init_attempted = true;
            let (q, c) = init_gpu_usage_pdh(luid);
            self.pdh_usage_query = q;
            self.pdh_usage_counter = c;
            if q == 0 || c == 0 {
                close_pdh_query_handle(
                    &mut self.pdh_usage_query,
                    &mut self.pdh_usage_counter,
                    &mut self.pdh_usage_invalid_streak,
                );
                self.pdh_usage_backoff.record_failure(now);
                return None;
            }
            // Initial collect happened in init_gpu_usage_pdh; wait for next sample tick.
            return None;
        }

        if unsafe { PdhCollectQueryData(self.pdh_usage_query) != 0 } {
            close_pdh_query_handle(
                &mut self.pdh_usage_query,
                &mut self.pdh_usage_counter,
                &mut self.pdh_usage_invalid_streak,
            );
            self.pdh_usage_backoff.record_failure(now);
            return None;
        }

        if let Some(val) = read_pdh_counter_sum(self.pdh_usage_counter, luid_tag) {
            self.pdh_usage_invalid_streak = 0;
            self.pdh_usage_backoff.record_success();
            Some(val)
        } else {
            self.pdh_usage_invalid_streak = self.pdh_usage_invalid_streak.saturating_add(1);
            if self.pdh_usage_invalid_streak >= 3 {
                close_pdh_query_handle(
                    &mut self.pdh_usage_query,
                    &mut self.pdh_usage_counter,
                    &mut self.pdh_usage_invalid_streak,
                );
                self.pdh_usage_backoff.record_failure(now);
            }
            None
        }
    }

    fn sample_vram_pdh_at(
        &mut self,
        luid: Option<LUID>,
        luid_tag: Option<&str>,
        now: Instant,
    ) -> Option<f64> {
        if self.pdh_vram_query == 0 || self.pdh_vram_counter == 0 {
            if !self.pdh_vram_backoff.can_attempt(now) {
                return None;
            }
            self.pdh_vram_init_attempted = true;
            let (q, c) = init_gpu_vram_pdh(luid);
            self.pdh_vram_query = q;
            self.pdh_vram_counter = c;
            if q == 0 || c == 0 {
                close_pdh_query_handle(
                    &mut self.pdh_vram_query,
                    &mut self.pdh_vram_counter,
                    &mut self.pdh_vram_invalid_streak,
                );
                self.pdh_vram_backoff.record_failure(now);
                return None;
            }
            return None;
        }

        if unsafe { PdhCollectQueryData(self.pdh_vram_query) != 0 } {
            close_pdh_query_handle(
                &mut self.pdh_vram_query,
                &mut self.pdh_vram_counter,
                &mut self.pdh_vram_invalid_streak,
            );
            self.pdh_vram_backoff.record_failure(now);
            return None;
        }

        if let Some(val) = read_pdh_counter_sum(self.pdh_vram_counter, luid_tag) {
            self.pdh_vram_invalid_streak = 0;
            self.pdh_vram_backoff.record_success();
            Some(val)
        } else {
            self.pdh_vram_invalid_streak = self.pdh_vram_invalid_streak.saturating_add(1);
            if self.pdh_vram_invalid_streak >= 3 {
                close_pdh_query_handle(
                    &mut self.pdh_vram_query,
                    &mut self.pdh_vram_counter,
                    &mut self.pdh_vram_invalid_streak,
                );
                self.pdh_vram_backoff.record_failure(now);
            }
            None
        }
    }

    pub fn release_usage_pdh(&mut self) -> bool {
        let was_open = close_pdh_query_handle(
            &mut self.pdh_usage_query,
            &mut self.pdh_usage_counter,
            &mut self.pdh_usage_invalid_streak,
        );
        let had_attempted = self.pdh_usage_init_attempted;
        self.pdh_usage_init_attempted = false;
        let had_backoff = self.pdh_usage_backoff.reset();
        was_open || had_attempted || had_backoff
    }

    pub fn release_vram_pdh(&mut self) -> bool {
        let was_open = close_pdh_query_handle(
            &mut self.pdh_vram_query,
            &mut self.pdh_vram_counter,
            &mut self.pdh_vram_invalid_streak,
        );
        let had_attempted = self.pdh_vram_init_attempted;
        self.pdh_vram_init_attempted = false;
        let had_backoff = self.pdh_vram_backoff.reset();
        was_open || had_attempted || had_backoff
    }

    pub fn release_pdh_queries(&mut self) -> bool {
        let u = self.release_usage_pdh();
        let v = self.release_vram_pdh();
        u || v
    }

    pub(crate) fn has_usage_pdh(&self) -> bool {
        self.pdh_usage_query != 0
    }

    pub(crate) fn has_vram_pdh(&self) -> bool {
        self.pdh_vram_query != 0
    }

    pub fn diagnostics(&self) -> String {
        let mut lines = Vec::new();
        match &self.dxgi {
            Some(info) => lines.push(format!(
                "  DXGI 主卡: {} (VID=0x{:04X}, DID=0x{:04X}, LUID=0x{:08X}_{:08X}, VRAM={:.2} GB, multi_gpu={})",
                info.name,
                info.vendor_id,
                info.device_id,
                info.luid.HighPart as u32,
                info.luid.LowPart,
                info.vram_total_gb,
                info.multi_gpu
            )),
            None => lines.push("  DXGI 主卡: (未检测到硬件显卡)".to_string()),
        }
        lines.push(format!("  AMD ADL2:    {}", self.amd_status));
        lines.push(format!("  NVIDIA NVML: {}", self.nvidia_status));
        lines.push(format!("  Intel IGCL:  {}", self.intel_status));

        let usage_pdh_status = if self.pdh_usage_query != 0 && self.pdh_usage_counter != 0 {
            "已初始化"
        } else if self.pdh_usage_backoff.is_backing_off() {
            "退避重试中"
        } else if !self.pdh_usage_init_attempted {
            "延迟未触发"
        } else {
            "不可用"
        };
        let vram_pdh_status = if self.pdh_vram_query != 0 && self.pdh_vram_counter != 0 {
            "已初始化"
        } else if self.pdh_vram_backoff.is_backing_off() {
            "退避重试中"
        } else if !self.pdh_vram_init_attempted {
            "延迟未触发 (优先使用 NVML)"
        } else {
            "不可用"
        };
        lines.push(format!(
            "  PDH 状态:    3D占用={}, 显存={}",
            usage_pdh_status, vram_pdh_status
        ));
        lines.join("\n")
    }
}

fn read_pdh_counter_sum(counter: isize, required_luid_tag: Option<&str>) -> Option<f64> {
    if counter == 0 {
        return None;
    }
    unsafe {
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
        let cap_elems = (buf_size as usize).div_ceil(elem_size);
        let mut buf: Vec<PDH_FMT_COUNTERVALUE_ITEM_W> = vec![zeroed(); cap_elems];
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

        let mut total = 0.0_f64;
        let mut any_valid = false;
        for item in buf.iter().take(item_count as usize) {
            if let Some(tag) = required_luid_tag {
                let sz = item.szName;
                if sz.is_null() {
                    continue;
                }
                let inst_name = sz.to_string().unwrap_or_default().to_ascii_lowercase();
                if !inst_name.contains(tag) {
                    continue;
                }
            }
            // PDH_CSTATUS_VALID_DATA (0) or PDH_CSTATUS_NEW_DATA (1)
            if item.FmtValue.CStatus <= 1 {
                let v = item.FmtValue.Anonymous.doubleValue;
                if v.is_finite() && v >= 0.0 {
                    total += v;
                    any_valid = true;
                }
            }
        }
        any_valid.then_some(total)
    }
}

impl Default for GpuMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for GpuMonitor {
    fn drop(&mut self) {
        let _ = self.release_pdh_queries();
    }
}

fn close_pdh_query_handle(query: &mut isize, counter: &mut isize, invalid_streak: &mut u8) -> bool {
    let was_open = *query != 0;
    if *query != 0 {
        unsafe {
            let _ = PdhCloseQuery(*query);
        }
        *query = 0;
    }
    *counter = 0;
    *invalid_streak = 0;
    was_open
}

fn is_virtual_or_software_gpu(name: &str, vendor_id: u32, flags: u32) -> bool {
    if vendor_id == 0 {
        return true;
    }
    if (flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
        return true;
    }
    let lower = name.to_lowercase();
    const BLOCKLIST: &[&str] = &[
        "microsoft basic",
        "oray",
        "virtual",
        "meta",
        "mumu",
        "parsec",
        "idd",
    ];
    BLOCKLIST.iter().any(|pat| lower.contains(pat))
}

fn select_primary_dxgi_gpu() -> Option<DxgiGpuInfo> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut best: Option<(usize, DxgiGpuInfo)> = None;
        let mut hw_count: usize = 0;
        let mut idx = 0_u32;

        while let Ok(adapter) = factory.EnumAdapters1(idx) {
            idx += 1;
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            let name = utf16_z_to_string(&desc.Description);
            if is_virtual_or_software_gpu(&name, desc.VendorId, desc.Flags) {
                continue;
            }
            hw_count += 1;
            let dedicated = desc.DedicatedVideoMemory;
            let info = DxgiGpuInfo {
                name,
                vendor_id: desc.VendorId,
                device_id: desc.DeviceId,
                subsys_id: desc.SubSysId,
                luid: desc.AdapterLuid,
                vram_total_gb: (dedicated as f64 / BYTES_PER_GB) as f32,
                multi_gpu: false,
            };
            let replace = match &best {
                None => true,
                Some((best_vram, _)) => dedicated > *best_vram,
            };
            if replace {
                best = Some((dedicated, info));
            }
        }

        best.map(|(_, mut info)| {
            info.multi_gpu = hw_count > 1;
            info
        })
    }
}

fn should_eagerly_init_usage_pdh(has_amd: bool, has_nvidia: bool) -> bool {
    !has_amd && !has_nvidia
}

fn format_pdh_luid_tag(luid: LUID) -> String {
    format!("luid_0x{:08x}_0x{:08x}", luid.HighPart as u32, luid.LowPart)
}

fn build_gpu_usage_pdh_path(luid: LUID) -> String {
    format!(
        "\\GPU Engine(*{}*engtype_3D)\\Utilization Percentage",
        format_pdh_luid_tag(luid)
    )
}

fn build_gpu_vram_pdh_path(luid: LUID) -> String {
    format!(
        "\\GPU Adapter Memory(*{}*)\\Dedicated Usage",
        format_pdh_luid_tag(luid)
    )
}

fn init_luid_counter_pdh(
    luid: Option<LUID>,
    luid_path_fn: impl FnOnce(LUID) -> String,
) -> (isize, isize) {
    let Some(l) = luid else {
        return (0, 0);
    };
    unsafe {
        let mut query: isize = 0;
        if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
            return (0, 0);
        }

        let Some(counter) = try_add_pdh_counter(query, &luid_path_fn(l)) else {
            let _ = PdhCloseQuery(query);
            return (0, 0);
        };

        let _ = PdhCollectQueryData(query);
        (query, counter)
    }
}

fn init_gpu_usage_pdh(luid: Option<LUID>) -> (isize, isize) {
    init_luid_counter_pdh(luid, build_gpu_usage_pdh_path)
}

fn init_gpu_vram_pdh(luid: Option<LUID>) -> (isize, isize) {
    init_luid_counter_pdh(luid, build_gpu_vram_pdh_path)
}

fn try_add_pdh_counter(query: isize, path: &str) -> Option<isize> {
    unsafe {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut counter: isize = 0;
        if PdhAddEnglishCounterW(query, PCWSTR(wide.as_ptr()), 0, &mut counter) == 0 && counter != 0
        {
            Some(counter)
        } else {
            None
        }
    }
}

fn normalize_gpu_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_space = true;
    for ch in name.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch.to_ascii_lowercase());
            prev_space = false;
        }
    }
    out.trim()
        .replace("(tm)", "")
        .replace("(r)", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn strict_gpu_name_eq(a: &str, b: &str) -> bool {
    let na = normalize_gpu_name(a);
    let nb = normalize_gpu_name(b);
    !na.is_empty() && na == nb
}

// ---------------------------------------------------------------------------
// AMD ADL2 (atiadlxx.dll)
// ---------------------------------------------------------------------------

const ADL_OK: c_int = 0;
const ADL_PMLOG_MAX_SENSORS: usize = 256;
const ADL_PMLOG_TEMPERATURE_EDGE: usize = 8;
const ADL_PMLOG_FAN_RPM: usize = 14;
const ADL_PMLOG_INFO_ACTIVITY_GFX: usize = 19;
const ADL_PMLOG_ASIC_POWER: usize = 23;
const ADL_PMLOG_TEMPERATURE_HOTSPOT: usize = 27;
const ADL_PMLOG_BOARD_POWER: usize = 73;

#[cfg(target_arch = "x86_64")]
type AdlMallocCallback = unsafe extern "C" fn(i_size: c_int) -> *mut c_void;
#[cfg(not(target_arch = "x86_64"))]
type AdlMallocCallback = unsafe extern "C" fn(i_size: c_int) -> *mut c_void;

type FnAdl2MainControlCreate =
    unsafe extern "C" fn(AdlMallocCallback, c_int, *mut *mut c_void) -> c_int;
type FnAdl2MainControlDestroy = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnAdl2AdapterNumberOfAdaptersGet = unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int;
type FnAdl2AdapterAdapterInfoGet =
    unsafe extern "C" fn(*mut c_void, *mut AdlAdapterInfo, c_int) -> c_int;
type FnAdl2NewQueryPmLogDataGet =
    unsafe extern "C" fn(*mut c_void, c_int, *mut AdlPmLogDataOutput) -> c_int;

#[link(name = "kernel32")]
extern "system" {
    fn GetProcessHeap() -> *mut c_void;
    fn HeapAlloc(h_heap: *mut c_void, dw_flags: u32, dw_bytes: usize) -> *mut c_void;
}

unsafe extern "C" fn adl_malloc_cb(i_size: c_int) -> *mut c_void {
    let bytes = i_size.max(1) as usize;
    let heap = GetProcessHeap();
    // HEAP_ZERO_MEMORY = 0x00000008
    HeapAlloc(heap, 0x0000_0008, bytes)
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct AdlAdapterInfo {
    i_size: c_int,
    i_adapter_index: c_int,
    str_udid: [c_char; 256],
    i_bus_number: c_int,
    i_device_number: c_int,
    i_function_number: c_int,
    i_vendor_id: c_int,
    str_adapter_name: [c_char; 256],
    str_display_name: [c_char; 256],
    i_present: c_int,
    i_exist: c_int,
    str_driver_path: [c_char; 256],
    str_driver_path_ext: [c_char; 256],
    str_pnp_string: [c_char; 256],
    i_os_display_index: c_int,
}

impl Default for AdlAdapterInfo {
    fn default() -> Self {
        unsafe { zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AdlSingleSensorData {
    supported: c_int,
    value: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct AdlPmLogDataOutput {
    size: c_int,
    sensors: [AdlSingleSensorData; ADL_PMLOG_MAX_SENSORS],
}

impl Default for AdlPmLogDataOutput {
    fn default() -> Self {
        unsafe { zeroed() }
    }
}

const _: () = assert!(size_of::<AdlAdapterInfo>() == 1572);
const _: () = assert!(size_of::<AdlSingleSensorData>() == 8);
const _: () = assert!(size_of::<AdlPmLogDataOutput>() == 2052);

#[derive(Default)]
struct VendorTelemetry {
    usage_pct: Option<f32>,
    temp_c: Option<f32>,
    hotspot_c: Option<f32>,
    power_w: Option<f32>,
    fan_rpm: Option<u32>,
    fan_pct: Option<u32>,
    vram_used_gb: Option<f32>,
    vram_total_gb: Option<f32>,
}

struct AmdAdl {
    module: isize,
    ctx: usize,
    adapter_index: c_int,
    adapter_name: String,
    fn_destroy: FnAdl2MainControlDestroy,
    fn_query_pmlog: FnAdl2NewQueryPmLogDataGet,
}

impl AmdAdl {
    fn try_new(dxgi: Option<&DxgiGpuInfo>) -> Result<Self, String> {
        unsafe {
            let Some(module) = load_dll("atiadlxx.dll") else {
                return Err("DLL 未安装 (atiadlxx.dll)".to_string());
            };
            let fn_create: FnAdl2MainControlCreate =
                match get_proc(module, b"ADL2_Main_Control_Create\0") {
                    Some(f) => f,
                    None => {
                        let _ = FreeLibrary(HMODULE(module as *mut c_void));
                        return Err("初始化失败 (缺少 ADL2_Main_Control_Create)".to_string());
                    }
                };
            let fn_destroy: FnAdl2MainControlDestroy =
                match get_proc(module, b"ADL2_Main_Control_Destroy\0") {
                    Some(f) => f,
                    None => {
                        let _ = FreeLibrary(HMODULE(module as *mut c_void));
                        return Err("初始化失败 (缺少 ADL2_Main_Control_Destroy)".to_string());
                    }
                };
            let fn_num_adapters: FnAdl2AdapterNumberOfAdaptersGet =
                match get_proc(module, b"ADL2_Adapter_NumberOfAdapters_Get\0") {
                    Some(f) => f,
                    None => {
                        let _ = FreeLibrary(HMODULE(module as *mut c_void));
                        return Err(
                            "初始化失败 (缺少 ADL2_Adapter_NumberOfAdapters_Get)".to_string()
                        );
                    }
                };
            let fn_adapter_info: FnAdl2AdapterAdapterInfoGet =
                match get_proc(module, b"ADL2_Adapter_AdapterInfo_Get\0") {
                    Some(f) => f,
                    None => {
                        let _ = FreeLibrary(HMODULE(module as *mut c_void));
                        return Err("初始化失败 (缺少 ADL2_Adapter_AdapterInfo_Get)".to_string());
                    }
                };
            let fn_query_pmlog: FnAdl2NewQueryPmLogDataGet =
                match get_proc(module, b"ADL2_New_QueryPMLogData_Get\0") {
                    Some(f) => f,
                    None => {
                        let _ = FreeLibrary(HMODULE(module as *mut c_void));
                        return Err("初始化失败 (缺少 ADL2_New_QueryPMLogData_Get)".to_string());
                    }
                };

            let mut ctx: *mut c_void = std::ptr::null_mut();
            if fn_create(adl_malloc_cb, 1, &mut ctx) != ADL_OK || ctx.is_null() {
                let _ = FreeLibrary(HMODULE(module as *mut c_void));
                return Err("初始化失败 (ADL2_Main_Control_Create)".to_string());
            }

            let mut count: c_int = 0;
            if fn_num_adapters(ctx, &mut count) != ADL_OK || count <= 0 {
                let _ = fn_destroy(ctx);
                let _ = FreeLibrary(HMODULE(module as *mut c_void));
                return Err("初始化失败 (无可用 ADL 适配器)".to_string());
            }

            let mut infos = vec![AdlAdapterInfo::default(); count as usize];
            let buf_bytes = (infos.len() * size_of::<AdlAdapterInfo>()) as c_int;
            if fn_adapter_info(ctx, infos.as_mut_ptr(), buf_bytes) != ADL_OK {
                let _ = fn_destroy(ctx);
                let _ = FreeLibrary(HMODULE(module as *mut c_void));
                return Err("初始化失败 (ADL2_Adapter_AdapterInfo_Get)".to_string());
            }

            let chosen = pick_adl_adapter(&infos, dxgi, ctx, fn_query_pmlog);
            let Some((adapter_index, adapter_name)) = chosen else {
                let _ = fn_destroy(ctx);
                let _ = FreeLibrary(HMODULE(module as *mut c_void));
                return Err("初始化失败 (未匹配到支持 PMLog 的适配器)".to_string());
            };

            Ok(Self {
                module,
                ctx: ctx as usize,
                adapter_index,
                adapter_name,
                fn_destroy,
                fn_query_pmlog,
            })
        }
    }

    fn sample_with_demand(&self, demand: GpuDemand) -> VendorTelemetry {
        if !(demand.usage || demand.temp || demand.hotspot || demand.power || demand.fan) {
            return VendorTelemetry::default();
        }
        unsafe {
            let mut out = AdlPmLogDataOutput {
                size: size_of::<AdlPmLogDataOutput>() as c_int,
                ..Default::default()
            };
            let res = (self.fn_query_pmlog)(self.ctx as *mut c_void, self.adapter_index, &mut out);
            if res != ADL_OK {
                return VendorTelemetry::default();
            }
            let sensor = |idx: usize| -> Option<c_int> {
                if idx < ADL_PMLOG_MAX_SENSORS && out.sensors[idx].supported != 0 {
                    Some(out.sensors[idx].value)
                } else {
                    None
                }
            };

            let usage_pct = demand
                .usage
                .then(|| sensor(ADL_PMLOG_INFO_ACTIVITY_GFX).map(|v| (v as f32).clamp(0.0, 100.0)))
                .flatten();
            let temp_c = demand
                .temp
                .then(|| {
                    sensor(ADL_PMLOG_TEMPERATURE_EDGE)
                        .map(|v| v as f32)
                        .filter(|v| (-20.0..=150.0).contains(v))
                })
                .flatten();
            let hotspot_c = demand
                .hotspot
                .then(|| {
                    sensor(ADL_PMLOG_TEMPERATURE_HOTSPOT)
                        .map(|v| v as f32)
                        .filter(|v| (-20.0..=150.0).contains(v))
                })
                .flatten();
            let power_w = demand
                .power
                .then(|| {
                    sensor(ADL_PMLOG_BOARD_POWER)
                        .or_else(|| sensor(ADL_PMLOG_ASIC_POWER))
                        .map(|v| v as f32)
                        .filter(|v| (0.0..=2000.0).contains(v))
                })
                .flatten();
            let fan_rpm = demand
                .fan
                .then(|| sensor(ADL_PMLOG_FAN_RPM).map(|v| v.max(0) as u32))
                .flatten();

            VendorTelemetry {
                usage_pct,
                temp_c,
                hotspot_c,
                power_w,
                fan_rpm,
                ..Default::default()
            }
        }
    }
}

impl Drop for AmdAdl {
    fn drop(&mut self) {
        unsafe {
            if self.ctx != 0 {
                let _ = (self.fn_destroy)(self.ctx as *mut c_void);
            }
            if self.module != 0 {
                let _ = FreeLibrary(HMODULE(self.module as *mut c_void));
            }
        }
    }
}

fn pick_adl_adapter(
    infos: &[AdlAdapterInfo],
    dxgi: Option<&DxgiGpuInfo>,
    ctx: *mut c_void,
    fn_query_pmlog: FnAdl2NewQueryPmLogDataGet,
) -> Option<(c_int, String)> {
    let is_amd_vendor = |v: c_int| v == 1002 || v == 0x1002;

    if let Some(d) = dxgi {
        if d.vendor_id != VENDOR_AMD {
            return None;
        }
        let dev_tag = format!("DEV_{:04X}", d.device_id);
        let mut pnp_matches: Vec<(c_int, String)> = Vec::new();
        let mut seen_bdf = std::collections::HashSet::new();
        for info in infos {
            if !is_amd_vendor(info.i_vendor_id) {
                continue;
            }
            let pnp = c_char_slice_to_string(&info.str_pnp_string);
            if pnp.to_ascii_uppercase().contains(&dev_tag)
                && adl_pmlog_works(ctx, info.i_adapter_index, fn_query_pmlog)
                && seen_bdf.insert((
                    info.i_bus_number,
                    info.i_device_number,
                    info.i_function_number,
                ))
            {
                pnp_matches.push((
                    info.i_adapter_index,
                    c_char_slice_to_string(&info.str_adapter_name),
                ));
            }
        }
        if pnp_matches.len() == 1 {
            return pnp_matches.into_iter().next();
        }
        if pnp_matches.len() > 1 {
            // Multiple AMD cards with same DID: only bind if strict name uniquely identifies one.
            let strict: Vec<_> = pnp_matches
                .into_iter()
                .filter(|(_, name)| strict_gpu_name_eq(name, &d.name))
                .collect();
            if strict.len() == 1 {
                return strict.into_iter().next();
            }
            return None;
        }

        let mut name_matches: Vec<(c_int, String)> = Vec::new();
        let mut seen_bdf_name = std::collections::HashSet::new();
        for info in infos {
            if !is_amd_vendor(info.i_vendor_id) {
                continue;
            }
            let name = c_char_slice_to_string(&info.str_adapter_name);
            if strict_gpu_name_eq(&name, &d.name)
                && adl_pmlog_works(ctx, info.i_adapter_index, fn_query_pmlog)
                && seen_bdf_name.insert((
                    info.i_bus_number,
                    info.i_device_number,
                    info.i_function_number,
                ))
            {
                name_matches.push((info.i_adapter_index, name));
            }
        }
        if name_matches.len() == 1 {
            return name_matches.into_iter().next();
        }
        return None;
    }

    let mut fallback: Vec<(c_int, String)> = Vec::new();
    let mut seen_bdf = std::collections::HashSet::new();
    for info in infos {
        if !is_amd_vendor(info.i_vendor_id) {
            continue;
        }
        if adl_pmlog_works(ctx, info.i_adapter_index, fn_query_pmlog)
            && seen_bdf.insert((
                info.i_bus_number,
                info.i_device_number,
                info.i_function_number,
            ))
        {
            fallback.push((
                info.i_adapter_index,
                c_char_slice_to_string(&info.str_adapter_name),
            ));
        }
    }
    (fallback.len() == 1)
        .then(|| fallback.into_iter().next())
        .flatten()
}

fn adl_pmlog_works(
    ctx: *mut c_void,
    adapter_index: c_int,
    fn_query_pmlog: FnAdl2NewQueryPmLogDataGet,
) -> bool {
    unsafe {
        let mut out = AdlPmLogDataOutput {
            size: size_of::<AdlPmLogDataOutput>() as c_int,
            ..Default::default()
        };
        if fn_query_pmlog(ctx, adapter_index, &mut out) != ADL_OK {
            return false;
        }
        out.sensors[ADL_PMLOG_TEMPERATURE_EDGE].supported != 0
            || out.sensors[ADL_PMLOG_INFO_ACTIVITY_GFX].supported != 0
    }
}

// ---------------------------------------------------------------------------
// NVIDIA NVML (nvml.dll)
// ---------------------------------------------------------------------------

const NVML_SUCCESS: c_int = 0;
const NVML_TEMPERATURE_GPU: c_uint = 0;

#[repr(C)]
#[derive(Clone, Copy, Default)]
#[allow(dead_code)]
struct NvmlUtilization {
    gpu: c_uint,
    memory: c_uint,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvmlFanSpeedInfo {
    version: u32,
    fan: u32,
    speed: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvmlPciInfoV2 {
    bus_id: [c_char; 16],
    domain: c_uint,
    bus: c_uint,
    device: c_uint,
    pci_device_id: c_uint,
    pci_sub_system_id: c_uint,
}

impl Default for NvmlPciInfoV2 {
    fn default() -> Self {
        unsafe { zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvmlPciInfoV3 {
    bus_id_legacy: [c_char; 16],
    domain: c_uint,
    bus: c_uint,
    device: c_uint,
    pci_device_id: c_uint,
    pci_sub_system_id: c_uint,
    bus_id: [c_char; 32],
}

impl Default for NvmlPciInfoV3 {
    fn default() -> Self {
        unsafe { zeroed() }
    }
}

const _: () = assert!(size_of::<NvmlUtilization>() == 8);
const _: () = assert!(size_of::<NvmlFanSpeedInfo>() == 12);
const _: () = assert!(size_of::<NvmlMemory>() == 24);
const _: () = assert!(size_of::<NvmlPciInfoV2>() == 36);
const _: () = assert!(size_of::<NvmlPciInfoV3>() == 68);

const NVML_FAN_SPEED_INFO_V1: u32 = (size_of::<NvmlFanSpeedInfo>() as u32) | (1u32 << 24);

type FnNvmlInit = unsafe extern "C" fn() -> c_int;
type FnNvmlShutdown = unsafe extern "C" fn() -> c_int;
type FnNvmlDeviceGetCount = unsafe extern "C" fn(*mut c_uint) -> c_int;
type FnNvmlDeviceGetHandleByIndex = unsafe extern "C" fn(c_uint, *mut *mut c_void) -> c_int;
type FnNvmlDeviceGetName = unsafe extern "C" fn(*mut c_void, *mut c_char, c_uint) -> c_int;
type FnNvmlDeviceGetPciInfoV2 = unsafe extern "C" fn(*mut c_void, *mut NvmlPciInfoV2) -> c_int;
type FnNvmlDeviceGetPciInfoV3 = unsafe extern "C" fn(*mut c_void, *mut NvmlPciInfoV3) -> c_int;
type FnNvmlDeviceGetUtilizationRates =
    unsafe extern "C" fn(*mut c_void, *mut NvmlUtilization) -> c_int;
type FnNvmlDeviceGetTemperature = unsafe extern "C" fn(*mut c_void, c_uint, *mut c_uint) -> c_int;
type FnNvmlDeviceGetPowerUsage = unsafe extern "C" fn(*mut c_void, *mut c_uint) -> c_int;
type FnNvmlDeviceGetFanSpeed = unsafe extern "C" fn(*mut c_void, *mut c_uint) -> c_int;
type FnNvmlDeviceGetFanSpeedRpm = unsafe extern "C" fn(*mut c_void, *mut NvmlFanSpeedInfo) -> c_int;
type FnNvmlDeviceGetMemoryInfo = unsafe extern "C" fn(*mut c_void, *mut NvmlMemory) -> c_int;

#[derive(Debug, Clone, PartialEq, Eq)]
struct NvmlDeviceCandidate {
    index: usize,
    name: String,
    pci_vendor_id: Option<u32>,
    pci_device_id: Option<u32>,
    pci_subsys_id: Option<u32>,
}

fn decode_nvml_pci_ids(
    pci_device_id_combined: u32,
    pci_subsys_id: u32,
) -> (Option<u32>, Option<u32>, Option<u32>) {
    let vendor_id = pci_device_id_combined & 0xFFFF;
    let device_id = (pci_device_id_combined >> 16) & 0xFFFF;
    (
        (vendor_id != 0).then_some(vendor_id),
        (device_id != 0).then_some(device_id),
        (pci_subsys_id != 0).then_some(pci_subsys_id),
    )
}

fn select_nvml_device_index(
    dxgi: Option<&DxgiGpuInfo>,
    candidates: &[NvmlDeviceCandidate],
) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }

    let Some(d) = dxgi else {
        // Without DXGI reference, only bind when there is a single unambiguous NVIDIA device.
        return (candidates.len() == 1).then(|| candidates[0].index);
    };

    if d.vendor_id != VENDOR_NVIDIA {
        return None;
    }

    // Priority 1: PCI Vendor/Device/SubSys ID match.
    let pci_matches: Vec<&NvmlDeviceCandidate> = candidates
        .iter()
        .filter(|c| c.pci_vendor_id == Some(VENDOR_NVIDIA) && c.pci_device_id == Some(d.device_id))
        .collect();
    if !pci_matches.is_empty() {
        let subsys_matches: Vec<&NvmlDeviceCandidate> = if d.subsys_id != 0 {
            pci_matches
                .iter()
                .copied()
                .filter(|c| c.pci_subsys_id == Some(d.subsys_id))
                .collect()
        } else {
            Vec::new()
        };
        let pool = if !subsys_matches.is_empty() {
            subsys_matches
        } else {
            pci_matches
        };
        if pool.len() == 1 {
            return Some(pool[0].index);
        }
        let name_narrowed: Vec<&NvmlDeviceCandidate> = pool
            .into_iter()
            .filter(|c| strict_gpu_name_eq(&c.name, &d.name))
            .collect();
        if name_narrowed.len() == 1 {
            return Some(name_narrowed[0].index);
        }
        // Multiple indistinguishable NVIDIA GPUs with identical PCI IDs and names:
        // cannot prove which matches DXGI LUID, so refuse to guess device 0.
        return None;
    }

    // Priority 2: Strict normalized name match ONLY for candidates lacking PCI info.
    let name_matches: Vec<&NvmlDeviceCandidate> = candidates
        .iter()
        .filter(|c| c.pci_device_id.is_none() && strict_gpu_name_eq(&c.name, &d.name))
        .collect();
    if name_matches.len() == 1 {
        return Some(name_matches[0].index);
    }

    None
}

struct NvidiaNvml {
    module: isize,
    device: usize,
    device_name: String,
    fn_shutdown: FnNvmlShutdown,
    fn_get_util: Option<FnNvmlDeviceGetUtilizationRates>,
    fn_get_temp: Option<FnNvmlDeviceGetTemperature>,
    fn_get_power: Option<FnNvmlDeviceGetPowerUsage>,
    fn_get_fan_pct: Option<FnNvmlDeviceGetFanSpeed>,
    fn_get_fan_rpm: Option<FnNvmlDeviceGetFanSpeedRpm>,
    fn_get_mem_info: Option<FnNvmlDeviceGetMemoryInfo>,
}

impl NvidiaNvml {
    fn try_new(dxgi: Option<&DxgiGpuInfo>) -> Result<Self, String> {
        if let Some(d) = dxgi {
            if d.vendor_id != VENDOR_NVIDIA {
                return Err("未匹配厂商".to_string());
            }
        }
        unsafe {
            let Some(module) = load_nvml_dll() else {
                return Err("DLL 未安装 (nvml.dll)".to_string());
            };
            let fn_init: FnNvmlInit = match get_proc(module, b"nvmlInit_v2\0")
                .or_else(|| get_proc(module, b"nvmlInit\0"))
            {
                Some(f) => f,
                None => {
                    let _ = FreeLibrary(HMODULE(module as *mut c_void));
                    return Err("初始化失败 (缺少 nvmlInit)".to_string());
                }
            };
            let fn_shutdown: FnNvmlShutdown = match get_proc(module, b"nvmlShutdown\0") {
                Some(f) => f,
                None => {
                    let _ = FreeLibrary(HMODULE(module as *mut c_void));
                    return Err("初始化失败 (缺少 nvmlShutdown)".to_string());
                }
            };
            let fn_get_handle: FnNvmlDeviceGetHandleByIndex =
                match get_proc(module, b"nvmlDeviceGetHandleByIndex_v2\0")
                    .or_else(|| get_proc(module, b"nvmlDeviceGetHandleByIndex\0"))
                {
                    Some(f) => f,
                    None => {
                        let _ = FreeLibrary(HMODULE(module as *mut c_void));
                        return Err("初始化失败 (缺少 nvmlDeviceGetHandleByIndex)".to_string());
                    }
                };

            if fn_init() != NVML_SUCCESS {
                let _ = FreeLibrary(HMODULE(module as *mut c_void));
                return Err("初始化失败 (nvmlInit)".to_string());
            }

            let fn_get_count: Option<FnNvmlDeviceGetCount> =
                get_proc(module, b"nvmlDeviceGetCount_v2\0")
                    .or_else(|| get_proc(module, b"nvmlDeviceGetCount\0"));
            let fn_get_name: Option<FnNvmlDeviceGetName> = get_proc(module, b"nvmlDeviceGetName\0");
            let fn_get_pci_v3: Option<FnNvmlDeviceGetPciInfoV3> =
                get_proc(module, b"nvmlDeviceGetPciInfo_v3\0");
            let fn_get_pci_v2: Option<FnNvmlDeviceGetPciInfoV2> =
                get_proc(module, b"nvmlDeviceGetPciInfo_v2\0")
                    .or_else(|| get_proc(module, b"nvmlDeviceGetPciInfo\0"));

            let mut count: c_uint = 1;
            if let Some(f) = fn_get_count {
                if f(&mut count) != NVML_SUCCESS {
                    count = 0;
                }
            }
            if count == 0 {
                let _ = fn_shutdown();
                let _ = FreeLibrary(HMODULE(module as *mut c_void));
                return Err("初始化失败 (未检测到 NVIDIA 设备)".to_string());
            }

            let mut handles: Vec<*mut c_void> = Vec::with_capacity(count as usize);
            let mut candidates: Vec<NvmlDeviceCandidate> = Vec::with_capacity(count as usize);

            for idx in 0..count {
                let mut dev: *mut c_void = std::ptr::null_mut();
                if fn_get_handle(idx, &mut dev) != NVML_SUCCESS || dev.is_null() {
                    continue;
                }
                let mut buf = [0 as c_char; 96];
                let name = if let Some(f) = fn_get_name {
                    if f(dev, buf.as_mut_ptr(), buf.len() as c_uint) == NVML_SUCCESS {
                        c_char_slice_to_string(&buf)
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };

                let (pci_vendor_id, pci_device_id, pci_subsys_id) = if let Some(f3) = fn_get_pci_v3
                {
                    let mut pci = NvmlPciInfoV3::default();
                    if f3(dev, &mut pci) == NVML_SUCCESS {
                        decode_nvml_pci_ids(pci.pci_device_id, pci.pci_sub_system_id)
                    } else {
                        (None, None, None)
                    }
                } else if let Some(f2) = fn_get_pci_v2 {
                    let mut pci = NvmlPciInfoV2::default();
                    if f2(dev, &mut pci) == NVML_SUCCESS {
                        decode_nvml_pci_ids(pci.pci_device_id, pci.pci_sub_system_id)
                    } else {
                        (None, None, None)
                    }
                } else {
                    (None, None, None)
                };

                let cand_idx = handles.len();
                handles.push(dev);
                candidates.push(NvmlDeviceCandidate {
                    index: cand_idx,
                    name,
                    pci_vendor_id,
                    pci_device_id,
                    pci_subsys_id,
                });
            }

            let Some(chosen_pos) = select_nvml_device_index(dxgi, &candidates) else {
                let _ = fn_shutdown();
                let _ = FreeLibrary(HMODULE(module as *mut c_void));
                return Err("初始化失败 (未能可靠匹配 DXGI 选定 NVIDIA 设备)".to_string());
            };

            let chosen_dev = handles[chosen_pos];
            let chosen_name = candidates[chosen_pos].name.clone();

            Ok(Self {
                module,
                device: chosen_dev as usize,
                device_name: chosen_name,
                fn_shutdown,
                fn_get_util: get_proc(module, b"nvmlDeviceGetUtilizationRates\0"),
                fn_get_temp: get_proc(module, b"nvmlDeviceGetTemperature\0"),
                fn_get_power: get_proc(module, b"nvmlDeviceGetPowerUsage\0"),
                fn_get_fan_pct: get_proc(module, b"nvmlDeviceGetFanSpeed\0"),
                fn_get_fan_rpm: get_proc(module, b"nvmlDeviceGetFanSpeedRPM\0"),
                fn_get_mem_info: get_proc(module, b"nvmlDeviceGetMemoryInfo\0"),
            })
        }
    }

    fn sample_with_demand(&self, demand: GpuDemand) -> VendorTelemetry {
        unsafe {
            let dev = self.device as *mut c_void;
            let usage_pct = if demand.usage {
                self.fn_get_util.and_then(|f| {
                    let mut u = NvmlUtilization::default();
                    (f(dev, &mut u) == NVML_SUCCESS).then(|| (u.gpu as f32).clamp(0.0, 100.0))
                })
            } else {
                None
            };
            let temp_c = if demand.temp {
                self.fn_get_temp.and_then(|f| {
                    let mut t: c_uint = 0;
                    (f(dev, NVML_TEMPERATURE_GPU, &mut t) == NVML_SUCCESS).then_some(t as f32)
                })
            } else {
                None
            };
            let power_w = if demand.power {
                self.fn_get_power.and_then(|f| {
                    let mut mw: c_uint = 0;
                    (f(dev, &mut mw) == NVML_SUCCESS).then(|| mw as f32 / 1000.0)
                })
            } else {
                None
            };
            let fan_pct = if demand.fan {
                self.fn_get_fan_pct.and_then(|f| {
                    let mut pct: c_uint = 0;
                    (f(dev, &mut pct) == NVML_SUCCESS && pct <= 100).then_some(pct)
                })
            } else {
                None
            };
            let fan_rpm = if demand.fan {
                self.fn_get_fan_rpm.and_then(|f| {
                    let mut info = NvmlFanSpeedInfo {
                        version: NVML_FAN_SPEED_INFO_V1,
                        fan: 0,
                        speed: 0,
                    };
                    (f(dev, &mut info) == NVML_SUCCESS).then_some(info.speed)
                })
            } else {
                None
            };
            let (vram_used_gb, vram_total_gb) = if demand.vram {
                self.fn_get_mem_info
                    .and_then(|f| {
                        let mut mem = NvmlMemory::default();
                        if f(dev, &mut mem) == NVML_SUCCESS && mem.total > 0 {
                            Some((
                                Some((mem.used as f64 / BYTES_PER_GB) as f32),
                                Some((mem.total as f64 / BYTES_PER_GB) as f32),
                            ))
                        } else {
                            None
                        }
                    })
                    .unwrap_or((None, None))
            } else {
                (None, None)
            };

            VendorTelemetry {
                usage_pct,
                temp_c,
                hotspot_c: None,
                power_w,
                fan_rpm,
                fan_pct,
                vram_used_gb,
                vram_total_gb,
            }
        }
    }
}

impl Drop for NvidiaNvml {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.fn_shutdown)();
            if self.module != 0 {
                let _ = FreeLibrary(HMODULE(self.module as *mut c_void));
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NvmlCandidate {
    System32(&'static str),
    ProtectedFallback(PathBuf),
}

fn known_program_files_dir() -> Option<PathBuf> {
    unsafe {
        let pwstr =
            SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, HANDLE::default())
                .ok()?;
        let res = if pwstr.is_null() {
            None
        } else {
            let s = pwstr.to_string().ok();
            s.filter(|p| !p.trim().is_empty()).map(PathBuf::from)
        };
        CoTaskMemFree(Some(pwstr.0 as *const c_void));
        res
    }
}

fn is_valid_nvml_fallback_policy_path(program_files: &Path, candidate: &Path) -> bool {
    if !program_files.is_absolute() || !candidate.is_absolute() {
        return false;
    }
    let has_unsafe_component = |p: &Path| {
        p.components().any(|c| {
            matches!(c, Component::ParentDir | Component::CurDir)
                || c.as_os_str().to_string_lossy().contains('\0')
        })
    };
    if has_unsafe_component(program_files) || has_unsafe_component(candidate) {
        return false;
    }
    let Ok(rel) = candidate.strip_prefix(program_files) else {
        return false;
    };
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    parts == ["nvidia corporation", "nvsmi", "nvml.dll"]
}

fn canonicalize_verified_nvml_path(program_files: &Path, candidate: &Path) -> Option<PathBuf> {
    if !is_valid_nvml_fallback_policy_path(program_files, candidate) {
        return None;
    }
    let canon_pf = std::fs::canonicalize(program_files).ok()?;
    let canon_cand = std::fs::canonicalize(candidate).ok()?;
    if !canon_cand.is_file() {
        return None;
    }
    let rel = canon_cand.strip_prefix(&canon_pf).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    if parts == ["nvidia corporation", "nvsmi", "nvml.dll"] {
        Some(canon_cand)
    } else {
        None
    }
}

fn nvml_load_candidates(program_files: Option<&Path>) -> Vec<NvmlCandidate> {
    let mut candidates = vec![NvmlCandidate::System32("nvml.dll")];
    if let Some(pf) = program_files {
        let fallback = pf.join("NVIDIA Corporation").join("NVSMI").join("nvml.dll");
        if is_valid_nvml_fallback_policy_path(pf, &fallback) {
            candidates.push(NvmlCandidate::ProtectedFallback(fallback));
        }
    }
    candidates
}

fn load_nvml_with<F>(
    program_files: Option<&Path>,
    mut try_load: F,
    verify_disk: bool,
) -> Option<isize>
where
    F: FnMut(&str) -> Option<isize>,
{
    for candidate in nvml_load_candidates(program_files) {
        match candidate {
            NvmlCandidate::System32(name) => {
                if let Some(m) = try_load(name) {
                    return Some(m);
                }
            }
            NvmlCandidate::ProtectedFallback(path) => {
                let verified = if verify_disk {
                    let Some(pf) = program_files else { continue };
                    let Some(canon) = canonicalize_verified_nvml_path(pf, &path) else {
                        continue;
                    };
                    canon
                } else {
                    path
                };
                if let Some(s) = verified.to_str() {
                    if let Some(m) = try_load(s) {
                        return Some(m);
                    }
                }
            }
        }
    }
    None
}

fn load_nvml_dll() -> Option<isize> {
    let pf = known_program_files_dir();
    load_nvml_with(pf.as_deref(), load_dll, true)
}

// ---------------------------------------------------------------------------
// Intel IGCL (ControlLib.dll / IntelControlLib.dll)
// ---------------------------------------------------------------------------

const CTL_RESULT_SUCCESS: u32 = 0x0000_0000;
const CTL_RESULT_ERROR_UNSUPPORTED_VERSION: u32 = 0x4000_0009;
const CTL_RESULT_ERROR_INVALID_SIZE: u32 = 0x4000_000F;
const CTL_INIT_FLAG_USE_LEVEL_ZERO: u32 = 1 << 0;
const CTL_DEVICE_TYPE_GRAPHICS: u32 = 1;
const CTL_MAX_DEVICE_NAME_LEN: usize = 100;
const CTL_PSU_COUNT: usize = 5;
const CTL_FAN_COUNT: usize = 5;

const CTL_UNITS_POWER_WATTS: u32 = 4;
const CTL_UNITS_TEMPERATURE_CELSIUS: u32 = 5;
const CTL_UNITS_ENERGY_JOULES: u32 = 6;
const CTL_UNITS_TIME_SECONDS: u32 = 7;
const CTL_UNITS_ANGULAR_SPEED_RPM: u32 = 9;
const CTL_UNITS_POWER_MILLIWATTS: u32 = 10;
const CTL_UNITS_PERCENT: u32 = 11;

const CTL_DATA_TYPE_INT8: u32 = 0;
const CTL_DATA_TYPE_UINT8: u32 = 1;
const CTL_DATA_TYPE_INT16: u32 = 2;
const CTL_DATA_TYPE_UINT16: u32 = 3;
const CTL_DATA_TYPE_INT32: u32 = 4;
const CTL_DATA_TYPE_UINT32: u32 = 5;
const CTL_DATA_TYPE_INT64: u32 = 6;
const CTL_DATA_TYPE_UINT64: u32 = 7;
const CTL_DATA_TYPE_FLOAT: u32 = 8;
const CTL_DATA_TYPE_DOUBLE: u32 = 9;

const fn ctl_make_version(major: u32, minor: u32) -> u32 {
    (major << 16) | (minor & 0x0000_FFFF)
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CtlApplicationId {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CtlInitArgs {
    size: u32,
    version: u8,
    app_version: u32,
    flags: u32,
    supported_version: u32,
    application_uid: CtlApplicationId,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CtlFirmwareVersion {
    major_version: u64,
    minor_version: u64,
    build_number: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CtlAdapterBdf {
    bus: u8,
    device: u8,
    function: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlDeviceAdapterProperties {
    size: u32,
    version: u8,
    p_device_id: *mut c_void,
    device_id_size: u32,
    device_type: u32,
    supported_subfunction_flags: u32,
    driver_version: u64,
    firmware_version: CtlFirmwareVersion,
    pci_vendor_id: u32,
    pci_device_id: u32,
    rev_id: u32,
    num_eus_per_sub_slice: u32,
    num_sub_slices_per_slice: u32,
    num_slices: u32,
    name: [c_char; CTL_MAX_DEVICE_NAME_LEN],
    graphics_adapter_properties: u32,
    frequency: u32,
    pci_subsys_id: u32,
    pci_subsys_vendor_id: u32,
    adapter_bdf: CtlAdapterBdf,
    num_xe_cores: u32,
    reserved: [u64; 7],
}

impl Default for CtlDeviceAdapterProperties {
    fn default() -> Self {
        unsafe { zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
union CtlDataValue {
    data8: i8,
    datau8: u8,
    data16: i16,
    datau16: u16,
    data32: i32,
    datau32: u32,
    data64: i64,
    datau64: u64,
    datafloat: f32,
    datadouble: f64,
}

impl Default for CtlDataValue {
    fn default() -> Self {
        unsafe { zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CtlOcTelemetryItem {
    b_supported: bool,
    units: u32,
    data_type: u32,
    value: CtlDataValue,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CtlPsuInfo {
    b_supported: bool,
    psu_type: u32,
    energy_counter: CtlOcTelemetryItem,
    voltage: CtlOcTelemetryItem,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlPowerTelemetryV0 {
    size: u32,
    version: u8,
    time_stamp: CtlOcTelemetryItem,
    gpu_energy_counter: CtlOcTelemetryItem,
    gpu_voltage: CtlOcTelemetryItem,
    gpu_current_clock_frequency: CtlOcTelemetryItem,
    gpu_current_temperature: CtlOcTelemetryItem,
    global_activity_counter: CtlOcTelemetryItem,
    render_compute_activity_counter: CtlOcTelemetryItem,
    media_activity_counter: CtlOcTelemetryItem,
    gpu_power_limited: bool,
    gpu_temperature_limited: bool,
    gpu_current_limited: bool,
    gpu_voltage_limited: bool,
    gpu_utilization_limited: bool,
    vram_energy_counter: CtlOcTelemetryItem,
    vram_voltage: CtlOcTelemetryItem,
    vram_current_clock_frequency: CtlOcTelemetryItem,
    vram_current_effective_frequency: CtlOcTelemetryItem,
    vram_read_bandwidth_counter: CtlOcTelemetryItem,
    vram_write_bandwidth_counter: CtlOcTelemetryItem,
    vram_current_temperature: CtlOcTelemetryItem,
    vram_power_limited: bool,
    vram_temperature_limited: bool,
    vram_current_limited: bool,
    vram_voltage_limited: bool,
    vram_utilization_limited: bool,
    total_card_energy_counter: CtlOcTelemetryItem,
    psu: [CtlPsuInfo; CTL_PSU_COUNT],
    fan_speed: [CtlOcTelemetryItem; CTL_FAN_COUNT],
}

impl Default for CtlPowerTelemetryV0 {
    fn default() -> Self {
        unsafe { zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlPowerTelemetryV1 {
    v0: CtlPowerTelemetryV0,
    gpu_vr_temp: CtlOcTelemetryItem,
    vram_vr_temp: CtlOcTelemetryItem,
    sa_vr_temp: CtlOcTelemetryItem,
    gpu_effective_clock: CtlOcTelemetryItem,
    gpu_over_voltage_percent: CtlOcTelemetryItem,
    gpu_power_percent: CtlOcTelemetryItem,
    gpu_temperature_percent: CtlOcTelemetryItem,
    vram_read_bandwidth: CtlOcTelemetryItem,
    vram_write_bandwidth: CtlOcTelemetryItem,
}

impl Default for CtlPowerTelemetryV1 {
    fn default() -> Self {
        unsafe { zeroed() }
    }
}

const _: () = assert!(size_of::<CtlApplicationId>() == 16);
const _: () = assert!(size_of::<CtlInitArgs>() == 36);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<CtlDeviceAdapterProperties>() == 272);
const _: () = assert!(size_of::<CtlOcTelemetryItem>() == 24);
const _: () = assert!(size_of::<CtlPsuInfo>() == 56);
const _: () = assert!(size_of::<CtlPowerTelemetryV0>() == 808);
const _: () = assert!(size_of::<CtlPowerTelemetryV1>() == 1024);

type FnCtlInit = unsafe extern "C" fn(*mut CtlInitArgs, *mut *mut c_void) -> u32;
type FnCtlClose = unsafe extern "C" fn(*mut c_void) -> u32;
type FnCtlEnumerateDevices = unsafe extern "C" fn(*mut c_void, *mut u32, *mut *mut c_void) -> u32;
type FnCtlGetDeviceProperties =
    unsafe extern "C" fn(*mut c_void, *mut CtlDeviceAdapterProperties) -> u32;
type FnCtlPowerTelemetryGet = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;

struct IntelIgcl {
    module: isize,
    api_handle: usize,
    device: usize,
    device_name: String,
    use_v0: bool,
    fn_close: FnCtlClose,
    fn_power_telemetry_get: FnCtlPowerTelemetryGet,
    prev_energy: Option<(f64, Option<f64>, Instant)>,
    prev_activity: Option<(f64, Option<f64>, Instant)>,
}

impl IntelIgcl {
    fn try_new(dxgi: Option<&DxgiGpuInfo>) -> Result<Self, String> {
        if let Some(d) = dxgi {
            if d.vendor_id != VENDOR_INTEL {
                return Err("未匹配厂商".to_string());
            }
        }
        unsafe {
            let Some(module) = load_igcl_dll() else {
                return Err("DLL 未安装 (ControlLib.dll)".to_string());
            };
            let fn_init: FnCtlInit = match get_proc(module, b"ctlInit\0") {
                Some(f) => f,
                None => {
                    let _ = FreeLibrary(HMODULE(module as *mut c_void));
                    return Err("初始化失败 (缺少 ctlInit)".to_string());
                }
            };
            let fn_close: FnCtlClose = match get_proc(module, b"ctlClose\0") {
                Some(f) => f,
                None => {
                    let _ = FreeLibrary(HMODULE(module as *mut c_void));
                    return Err("初始化失败 (缺少 ctlClose)".to_string());
                }
            };
            let fn_enum_devices: FnCtlEnumerateDevices =
                match get_proc(module, b"ctlEnumerateDevices\0") {
                    Some(f) => f,
                    None => {
                        let _ = FreeLibrary(HMODULE(module as *mut c_void));
                        return Err("初始化失败 (缺少 ctlEnumerateDevices)".to_string());
                    }
                };
            let fn_get_props: FnCtlGetDeviceProperties =
                match get_proc(module, b"ctlGetDeviceProperties\0") {
                    Some(f) => f,
                    None => {
                        let _ = FreeLibrary(HMODULE(module as *mut c_void));
                        return Err("初始化失败 (缺少 ctlGetDeviceProperties)".to_string());
                    }
                };
            let fn_power_telemetry_get: FnCtlPowerTelemetryGet =
                match get_proc(module, b"ctlPowerTelemetryGet\0") {
                    Some(f) => f,
                    None => {
                        let _ = FreeLibrary(HMODULE(module as *mut c_void));
                        return Err("初始化失败 (缺少 ctlPowerTelemetryGet)".to_string());
                    }
                };

            let mut init_args = CtlInitArgs {
                size: size_of::<CtlInitArgs>() as u32,
                version: 0,
                app_version: ctl_make_version(1, 0),
                flags: CTL_INIT_FLAG_USE_LEVEL_ZERO,
                supported_version: 0,
                application_uid: CtlApplicationId::default(),
            };
            let mut api_handle: *mut c_void = std::ptr::null_mut();
            if fn_init(&mut init_args, &mut api_handle) != CTL_RESULT_SUCCESS
                || api_handle.is_null()
            {
                let _ = FreeLibrary(HMODULE(module as *mut c_void));
                return Err("初始化失败 (ctlInit)".to_string());
            }

            let mut count: u32 = 0;
            if fn_enum_devices(api_handle, &mut count, std::ptr::null_mut()) != CTL_RESULT_SUCCESS
                || count == 0
            {
                let _ = fn_close(api_handle);
                let _ = FreeLibrary(HMODULE(module as *mut c_void));
                return Err("初始化失败 (未枚举到 IGCL 设备)".to_string());
            }

            let mut handles = vec![std::ptr::null_mut(); count as usize];
            if fn_enum_devices(api_handle, &mut count, handles.as_mut_ptr()) != CTL_RESULT_SUCCESS
                || count == 0
            {
                let _ = fn_close(api_handle);
                let _ = FreeLibrary(HMODULE(module as *mut c_void));
                return Err("初始化失败 (ctlEnumerateDevices)".to_string());
            }

            let mut luid_matches: Vec<(*mut c_void, String)> = Vec::new();
            let mut did_matches: Vec<(*mut c_void, String)> = Vec::new();
            let mut all_intel: Vec<(*mut c_void, String)> = Vec::new();

            for &dev in handles.iter().take(count as usize) {
                if dev.is_null() {
                    continue;
                }
                let mut luid = LUID::default();
                let mut props = CtlDeviceAdapterProperties {
                    size: size_of::<CtlDeviceAdapterProperties>() as u32,
                    version: 0,
                    p_device_id: &mut luid as *mut LUID as *mut c_void,
                    device_id_size: size_of::<LUID>() as u32,
                    ..Default::default()
                };
                if fn_get_props(dev, &mut props) != CTL_RESULT_SUCCESS {
                    continue;
                }
                if props.device_type != CTL_DEVICE_TYPE_GRAPHICS
                    || props.pci_vendor_id != VENDOR_INTEL
                {
                    continue;
                }
                let name = c_char_slice_to_string(&props.name);
                if let Some(d) = dxgi {
                    if luid.LowPart == d.luid.LowPart && luid.HighPart == d.luid.HighPart {
                        luid_matches.push((dev, name.clone()));
                    } else if d.device_id != 0 && props.pci_device_id == d.device_id {
                        did_matches.push((dev, name.clone()));
                    }
                }
                all_intel.push((dev, name));
            }

            let chosen = if dxgi.is_some() {
                if luid_matches.len() == 1 {
                    luid_matches.into_iter().next()
                } else if did_matches.len() == 1 {
                    did_matches.into_iter().next()
                } else {
                    None
                }
            } else if all_intel.len() == 1 {
                all_intel.into_iter().next()
            } else {
                None
            };

            let Some((device, device_name)) = chosen else {
                let _ = fn_close(api_handle);
                let _ = FreeLibrary(HMODULE(module as *mut c_void));
                return Err("初始化失败 (未匹配到 Intel 图形设备)".to_string());
            };

            Ok(Self {
                module,
                api_handle: api_handle as usize,
                device: device as usize,
                device_name,
                use_v0: false,
                fn_close,
                fn_power_telemetry_get,
                prev_energy: None,
                prev_activity: None,
            })
        }
    }

    fn sample_with_demand(&mut self, demand: GpuDemand) -> VendorTelemetry {
        if !(demand.usage || demand.temp || demand.power || demand.fan) {
            return VendorTelemetry::default();
        }
        unsafe {
            let dev = self.device as *mut c_void;
            let now = Instant::now();

            if !self.use_v0 {
                let mut v1 = CtlPowerTelemetryV1 {
                    v0: CtlPowerTelemetryV0 {
                        size: size_of::<CtlPowerTelemetryV1>() as u32,
                        version: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let res = (self.fn_power_telemetry_get)(
                    dev,
                    &mut v1 as *mut CtlPowerTelemetryV1 as *mut c_void,
                );
                if res == CTL_RESULT_SUCCESS {
                    return self.decode_telemetry_v0(&v1.v0, now, demand);
                }
                if res != CTL_RESULT_ERROR_INVALID_SIZE
                    && res != CTL_RESULT_ERROR_UNSUPPORTED_VERSION
                {
                    return VendorTelemetry::default();
                }
                self.use_v0 = true;
            }

            let mut v0 = CtlPowerTelemetryV0 {
                size: size_of::<CtlPowerTelemetryV0>() as u32,
                version: 0,
                ..Default::default()
            };
            let res = (self.fn_power_telemetry_get)(
                dev,
                &mut v0 as *mut CtlPowerTelemetryV0 as *mut c_void,
            );
            if res == CTL_RESULT_SUCCESS {
                self.decode_telemetry_v0(&v0, now, demand)
            } else {
                VendorTelemetry::default()
            }
        }
    }

    fn decode_telemetry_v0(
        &mut self,
        t: &CtlPowerTelemetryV0,
        now: Instant,
        demand: GpuDemand,
    ) -> VendorTelemetry {
        let ts_secs = if demand.power || demand.usage {
            extract_igcl_item(&t.time_stamp)
                .and_then(|(units, val)| (units == CTL_UNITS_TIME_SECONDS).then_some(val))
        } else {
            None
        };

        let temp_c = if demand.temp {
            extract_igcl_item(&t.gpu_current_temperature).and_then(|(units, val)| {
                if (units == CTL_UNITS_TEMPERATURE_CELSIUS || units > CTL_UNITS_PERCENT)
                    && (-20.0..=150.0).contains(&val)
                {
                    Some(val as f32)
                } else {
                    None
                }
            })
        } else {
            None
        };

        let power_w = if demand.power {
            let energy_item = if t.total_card_energy_counter.b_supported {
                &t.total_card_energy_counter
            } else {
                &t.gpu_energy_counter
            };
            compute_igcl_power_w(&mut self.prev_energy, energy_item, ts_secs, now)
        } else {
            self.prev_energy = None;
            None
        };

        let usage_pct = if demand.usage {
            let activity_item = if t.global_activity_counter.b_supported {
                &t.global_activity_counter
            } else {
                &t.render_compute_activity_counter
            };
            compute_igcl_usage_pct(&mut self.prev_activity, activity_item, ts_secs, now)
        } else {
            self.prev_activity = None;
            None
        };

        let mut fan_rpm: Option<u32> = None;
        let mut fan_pct: Option<u32> = None;
        if demand.fan {
            for item in &t.fan_speed {
                if let Some((units, val)) = extract_igcl_item(item) {
                    if units == CTL_UNITS_PERCENT {
                        if (0.0..=100.0).contains(&val) && fan_pct.is_none() {
                            fan_pct = Some(val.round() as u32);
                        }
                    } else if (units == CTL_UNITS_ANGULAR_SPEED_RPM || units > CTL_UNITS_PERCENT)
                        && (0.0..=15_000.0).contains(&val)
                        && fan_rpm.is_none()
                    {
                        fan_rpm = Some(val.round() as u32);
                    }
                }
            }
        }

        VendorTelemetry {
            usage_pct,
            temp_c,
            hotspot_c: None,
            power_w,
            fan_rpm,
            fan_pct,
            vram_used_gb: None,
            vram_total_gb: None,
        }
    }
}

impl Drop for IntelIgcl {
    fn drop(&mut self) {
        unsafe {
            if self.api_handle != 0 {
                let _ = (self.fn_close)(self.api_handle as *mut c_void);
            }
            if self.module != 0 {
                let _ = FreeLibrary(HMODULE(self.module as *mut c_void));
            }
        }
    }
}

fn load_igcl_dll() -> Option<isize> {
    load_dll("ControlLib.dll").or_else(|| load_dll("IntelControlLib.dll"))
}

fn extract_igcl_item(item: &CtlOcTelemetryItem) -> Option<(u32, f64)> {
    if !item.b_supported {
        return None;
    }
    let val = unsafe {
        match item.data_type {
            CTL_DATA_TYPE_INT8 => item.value.data8 as f64,
            CTL_DATA_TYPE_UINT8 => item.value.datau8 as f64,
            CTL_DATA_TYPE_INT16 => item.value.data16 as f64,
            CTL_DATA_TYPE_UINT16 => item.value.datau16 as f64,
            CTL_DATA_TYPE_INT32 => item.value.data32 as f64,
            CTL_DATA_TYPE_UINT32 => item.value.datau32 as f64,
            CTL_DATA_TYPE_INT64 => item.value.data64 as f64,
            CTL_DATA_TYPE_UINT64 => item.value.datau64 as f64,
            CTL_DATA_TYPE_FLOAT => item.value.datafloat as f64,
            CTL_DATA_TYPE_DOUBLE => item.value.datadouble,
            _ => return None,
        }
    };
    val.is_finite().then_some((item.units, val))
}

fn elapsed_igcl_secs(
    prev_ts: Option<f64>,
    curr_ts: Option<f64>,
    prev_instant: Instant,
    now: Instant,
) -> Option<f64> {
    if let (Some(p), Some(c)) = (prev_ts, curr_ts) {
        let dt = c - p;
        if dt.is_finite() && (0.05..=30.0).contains(&dt) {
            return Some(dt);
        }
    }
    let dt = now.saturating_duration_since(prev_instant).as_secs_f64();
    (dt.is_finite() && (0.05..=30.0).contains(&dt)).then_some(dt)
}

fn compute_igcl_power_w(
    prev: &mut Option<(f64, Option<f64>, Instant)>,
    item: &CtlOcTelemetryItem,
    ts_secs: Option<f64>,
    now: Instant,
) -> Option<f32> {
    let (units, val) = extract_igcl_item(item)?;
    if units == CTL_UNITS_POWER_WATTS {
        return (0.0..=2000.0).contains(&val).then_some(val as f32);
    }
    if units == CTL_UNITS_POWER_MILLIWATTS {
        let w = val / 1000.0;
        return (0.0..=2000.0).contains(&w).then_some(w as f32);
    }
    if units != CTL_UNITS_ENERGY_JOULES {
        return None;
    }
    let out = if let Some((prev_e, prev_ts, prev_inst)) = *prev {
        let de = val - prev_e;
        if de >= 0.0 {
            elapsed_igcl_secs(prev_ts, ts_secs, prev_inst, now).and_then(|dt| {
                let w = de / dt;
                (w.is_finite() && (0.0..=2000.0).contains(&w)).then_some(w as f32)
            })
        } else {
            None
        }
    } else {
        None
    };
    *prev = Some((val, ts_secs, now));
    out
}

fn compute_igcl_usage_pct(
    prev: &mut Option<(f64, Option<f64>, Instant)>,
    item: &CtlOcTelemetryItem,
    ts_secs: Option<f64>,
    now: Instant,
) -> Option<f32> {
    let (units, val) = extract_igcl_item(item)?;
    if units == CTL_UNITS_PERCENT {
        return (0.0..=100.0).contains(&val).then_some(val as f32);
    }
    if units != CTL_UNITS_TIME_SECONDS {
        return None;
    }
    let out = if let Some((prev_a, prev_ts, prev_inst)) = *prev {
        let da = val - prev_a;
        if da >= 0.0 {
            elapsed_igcl_secs(prev_ts, ts_secs, prev_inst, now)
                .map(|dt| ((da / dt) * 100.0).clamp(0.0, 100.0) as f32)
        } else {
            None
        }
    } else {
        None
    };
    *prev = Some((val, ts_secs, now));
    out
}

fn load_dll(path: &str) -> Option<isize> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let h = LoadLibraryExW(
            PCWSTR(wide.as_ptr()),
            HANDLE::default(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
        .ok()?;
        if h.0.is_null() {
            None
        } else {
            Some(h.0 as isize)
        }
    }
}

unsafe fn get_proc<T>(module: isize, name_nul: &[u8]) -> Option<T> {
    let proc = GetProcAddress(HMODULE(module as *mut c_void), PCSTR(name_nul.as_ptr()))?;
    Some(std::mem::transmute_copy(&proc))
}

fn utf16_z_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).trim().to_string()
}

fn c_char_slice_to_string(buf: &[c_char]) -> String {
    unsafe {
        let ptr = buf.as_ptr();
        if buf.contains(&0) {
            CStr::from_ptr(ptr).to_string_lossy().trim().to_string()
        } else {
            let bytes: Vec<u8> = buf.iter().map(|&c| c as u8).collect();
            String::from_utf8_lossy(&bytes).trim().to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn filters_virtual_and_software_adapters() {
        let sw_flag = DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32;
        assert!(is_virtual_or_software_gpu(
            "Microsoft Basic Render Driver",
            0x1414,
            sw_flag
        ));
        assert!(is_virtual_or_software_gpu("Unknown Adapter", 0, 0));
        assert!(is_virtual_or_software_gpu(
            "OrayIddDriver Device",
            0x1002,
            0
        ));
        assert!(is_virtual_or_software_gpu(
            "Virtual Desktop Monitor",
            0x1234,
            0
        ));
        assert!(is_virtual_or_software_gpu(
            "Meta Virtual Monitor",
            0x1234,
            0
        ));
        assert!(is_virtual_or_software_gpu(
            "MuMu Virtual Display Adapter",
            0x1234,
            0
        ));
        assert!(is_virtual_or_software_gpu(
            "Parsec Virtual Display Adapter",
            0x1234,
            0
        ));
        assert!(!is_virtual_or_software_gpu(
            "AMD Radeon RX 9070 XT",
            0x1002,
            0
        ));
        assert!(!is_virtual_or_software_gpu(
            "AMD Radeon(TM) Graphics",
            0x1002,
            0
        ));
    }

    #[test]
    fn ffi_struct_layouts_and_nvml_version() {
        assert_eq!(size_of::<NvmlFanSpeedInfo>(), 12);
        assert_eq!(NVML_FAN_SPEED_INFO_V1, 12 | (1 << 24));
        assert_eq!(size_of::<NvmlUtilization>(), 8);
        assert_eq!(size_of::<NvmlMemory>(), 24);
        assert_eq!(size_of::<NvmlPciInfoV2>(), 36);
        assert_eq!(size_of::<NvmlPciInfoV3>(), 68);
        assert_eq!(size_of::<AdlAdapterInfo>(), 1572);
        assert_eq!(size_of::<AdlSingleSensorData>(), 8);
        assert_eq!(size_of::<AdlPmLogDataOutput>(), 2052);

        assert_eq!(size_of::<CtlApplicationId>(), 16);
        assert_eq!(size_of::<CtlInitArgs>(), 36);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(size_of::<CtlDeviceAdapterProperties>(), 272);
        assert_eq!(size_of::<CtlOcTelemetryItem>(), 24);
        assert_eq!(size_of::<CtlPsuInfo>(), 56);
        assert_eq!(size_of::<CtlPowerTelemetryV0>(), 808);
        assert_eq!(size_of::<CtlPowerTelemetryV1>(), 1024);
        assert_eq!(ctl_make_version(1, 1), 0x0001_0001);

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GpuMonitor>();
    }

    #[test]
    fn nvml_load_policy_prefers_system32_and_validates_known_folder_fallback() {
        let pf = Path::new("C:\\Program Files");
        let valid = Path::new("C:\\Program Files\\NVIDIA Corporation\\NVSMI\\nvml.dll");
        assert!(is_valid_nvml_fallback_policy_path(pf, valid));

        // Reject relative, traversal, wrong prefix, wrong subpath
        assert!(!is_valid_nvml_fallback_policy_path(
            Path::new("Program Files"),
            valid
        ));
        assert!(!is_valid_nvml_fallback_policy_path(
            pf,
            Path::new("C:\\Program Files\\NVIDIA Corporation\\..\\evil\\nvml.dll")
        ));
        assert!(!is_valid_nvml_fallback_policy_path(
            pf,
            Path::new("C:\\Users\\Public\\NVIDIA Corporation\\NVSMI\\nvml.dll")
        ));
        assert!(!is_valid_nvml_fallback_policy_path(
            pf,
            Path::new("C:\\Program Files\\NVIDIA Corporation\\NVSMI\\evil.dll")
        ));

        // Mock loader: System32 succeeds -> fallback is never attempted
        let mut attempts = Vec::new();
        let loaded = load_nvml_with(
            Some(pf),
            |p| {
                attempts.push(p.to_string());
                Some(101)
            },
            false,
        );
        assert_eq!(loaded, Some(101));
        assert_eq!(attempts, vec!["nvml.dll"]);

        // Mock loader: System32 fails -> fallback attempted with protected ProgramFiles path
        let mut attempts2 = Vec::new();
        let loaded2 = load_nvml_with(
            Some(pf),
            |p| {
                attempts2.push(p.to_string());
                if p == "nvml.dll" {
                    None
                } else {
                    Some(202)
                }
            },
            false,
        );
        assert_eq!(loaded2, Some(202));
        assert_eq!(
            attempts2,
            vec![
                "nvml.dll",
                "C:\\Program Files\\NVIDIA Corporation\\NVSMI\\nvml.dll"
            ]
        );

        // Invalid ProgramFiles rejects fallback candidate completely
        let mut attempts3 = Vec::new();
        let loaded3 = load_nvml_with(
            Some(Path::new("relative\\dir")),
            |p| {
                attempts3.push(p.to_string());
                None
            },
            false,
        );
        assert_eq!(loaded3, None);
        assert_eq!(attempts3, vec!["nvml.dll"]);
    }

    #[test]
    fn known_program_files_dir_returns_valid_protected_path() {
        let pf = known_program_files_dir().expect("FOLDERID_ProgramFiles");
        assert!(pf.is_absolute());
        let cand = pf.join("NVIDIA Corporation").join("NVSMI").join("nvml.dll");
        assert!(is_valid_nvml_fallback_policy_path(&pf, &cand));
    }

    #[test]
    fn pdh_luid_paths_bind_to_selected_adapter_without_wildcard_fallback() {
        let luid = LUID {
            LowPart: 0x0001_2345,
            HighPart: 0x0000_0001,
        };
        assert_eq!(
            build_gpu_usage_pdh_path(luid),
            "\\GPU Engine(*luid_0x00000001_0x00012345*engtype_3D)\\Utilization Percentage"
        );
        assert_eq!(
            build_gpu_vram_pdh_path(luid),
            "\\GPU Adapter Memory(*luid_0x00000001_0x00012345*)\\Dedicated Usage"
        );
        assert_eq!(init_gpu_usage_pdh(None), (0, 0));
        assert_eq!(init_gpu_vram_pdh(None), (0, 0));
    }

    #[test]
    fn nvml_selection_requires_reliable_pci_or_strict_name_match_and_never_falls_back_blindly() {
        let dxgi_rtx4070 = DxgiGpuInfo {
            name: "NVIDIA GeForce RTX 4070".to_string(),
            vendor_id: VENDOR_NVIDIA,
            device_id: 0x2786,
            subsys_id: 0x1111_10DE,
            luid: LUID {
                LowPart: 1,
                HighPart: 0,
            },
            vram_total_gb: 12.0,
            multi_gpu: true,
        };

        // 1. Non-NVIDIA DXGI adapter must never bind to NVML
        let dxgi_amd = DxgiGpuInfo {
            vendor_id: VENDOR_AMD,
            ..dxgi_rtx4070.clone()
        };
        let cands = vec![NvmlDeviceCandidate {
            index: 0,
            name: "NVIDIA GeForce RTX 4070".to_string(),
            pci_vendor_id: Some(VENDOR_NVIDIA),
            pci_device_id: Some(0x2786),
            pci_subsys_id: Some(0x1111_10DE),
        }];
        assert_eq!(select_nvml_device_index(Some(&dxgi_amd), &cands), None);

        // 2. Mismatched PCI device_id and mismatched name -> None (never falls back to device 0!)
        let wrong_cands = vec![NvmlDeviceCandidate {
            index: 0,
            name: "NVIDIA GeForce RTX 4070 Ti".to_string(),
            pci_vendor_id: Some(VENDOR_NVIDIA),
            pci_device_id: Some(0x2782),
            pci_subsys_id: Some(0x2222_10DE),
        }];
        assert_eq!(
            select_nvml_device_index(Some(&dxgi_rtx4070), &wrong_cands),
            None
        );

        // 3. Multi-NVIDIA cards: PCI match picks index 1 even when index 0 is another NVIDIA card
        let multi_cands = vec![
            NvmlDeviceCandidate {
                index: 0,
                name: "NVIDIA GeForce RTX 4070 Ti".to_string(),
                pci_vendor_id: Some(VENDOR_NVIDIA),
                pci_device_id: Some(0x2782),
                pci_subsys_id: Some(0x2222_10DE),
            },
            NvmlDeviceCandidate {
                index: 1,
                name: "NVIDIA GeForce RTX 4070".to_string(),
                pci_vendor_id: Some(VENDOR_NVIDIA),
                pci_device_id: Some(0x2786),
                pci_subsys_id: Some(0x1111_10DE),
            },
        ];
        assert_eq!(
            select_nvml_device_index(Some(&dxgi_rtx4070), &multi_cands),
            Some(1)
        );

        // 4. Dual identical NVIDIA cards without LUID disambiguation -> None (cannot prove identity)
        let identical_cands = vec![
            NvmlDeviceCandidate {
                index: 0,
                name: "NVIDIA GeForce RTX 4070".to_string(),
                pci_vendor_id: Some(VENDOR_NVIDIA),
                pci_device_id: Some(0x2786),
                pci_subsys_id: Some(0x1111_10DE),
            },
            NvmlDeviceCandidate {
                index: 1,
                name: "NVIDIA GeForce RTX 4070".to_string(),
                pci_vendor_id: Some(VENDOR_NVIDIA),
                pci_device_id: Some(0x2786),
                pci_subsys_id: Some(0x1111_10DE),
            },
        ];
        assert_eq!(
            select_nvml_device_index(Some(&dxgi_rtx4070), &identical_cands),
            None
        );

        // 5. Fallback to strict name match only when PCI info unavailable (rejects "RTX 4070 Ti")
        let no_pci_cands = vec![
            NvmlDeviceCandidate {
                index: 0,
                name: "NVIDIA GeForce RTX 4070 Ti".to_string(),
                pci_vendor_id: None,
                pci_device_id: None,
                pci_subsys_id: None,
            },
            NvmlDeviceCandidate {
                index: 1,
                name: "NVIDIA GeForce RTX 4070".to_string(),
                pci_vendor_id: None,
                pci_device_id: None,
                pci_subsys_id: None,
            },
        ];
        assert_eq!(
            select_nvml_device_index(Some(&dxgi_rtx4070), &no_pci_cands),
            Some(1)
        );
    }

    #[test]
    fn usage_pdh_eager_init_only_without_vendor_api() {
        assert!(!should_eagerly_init_usage_pdh(true, false));
        assert!(!should_eagerly_init_usage_pdh(false, true));
        assert!(!should_eagerly_init_usage_pdh(true, true));
        assert!(should_eagerly_init_usage_pdh(false, false));
    }

    #[test]
    fn igcl_telemetry_item_decoding_and_delta_math() {
        let unsupported = CtlOcTelemetryItem {
            b_supported: false,
            units: CTL_UNITS_TEMPERATURE_CELSIUS,
            data_type: CTL_DATA_TYPE_DOUBLE,
            value: CtlDataValue { datadouble: 65.0 },
        };
        assert!(extract_igcl_item(&unsupported).is_none());

        let temp_item = CtlOcTelemetryItem {
            b_supported: true,
            units: CTL_UNITS_TEMPERATURE_CELSIUS,
            data_type: CTL_DATA_TYPE_DOUBLE,
            value: CtlDataValue { datadouble: 58.5 },
        };
        assert_eq!(
            extract_igcl_item(&temp_item),
            Some((CTL_UNITS_TEMPERATURE_CELSIUS, 58.5))
        );

        let rpm_item = CtlOcTelemetryItem {
            b_supported: true,
            units: CTL_UNITS_ANGULAR_SPEED_RPM,
            data_type: CTL_DATA_TYPE_UINT32,
            value: CtlDataValue { datau32: 1450 },
        };
        assert_eq!(
            extract_igcl_item(&rpm_item),
            Some((CTL_UNITS_ANGULAR_SPEED_RPM, 1450.0))
        );

        let mut prev_energy = None;
        let now = Instant::now();
        let e1 = CtlOcTelemetryItem {
            b_supported: true,
            units: CTL_UNITS_ENERGY_JOULES,
            data_type: CTL_DATA_TYPE_DOUBLE,
            value: CtlDataValue { datadouble: 100.0 },
        };
        assert!(compute_igcl_power_w(&mut prev_energy, &e1, Some(10.0), now).is_none());

        let e2 = CtlOcTelemetryItem {
            b_supported: true,
            units: CTL_UNITS_ENERGY_JOULES,
            data_type: CTL_DATA_TYPE_DOUBLE,
            value: CtlDataValue { datadouble: 175.0 },
        };
        let p = compute_igcl_power_w(&mut prev_energy, &e2, Some(11.0), now)
            .expect("delta power expected");
        assert!((p - 75.0).abs() < 0.01);

        let mut prev_act = None;
        let a1 = CtlOcTelemetryItem {
            b_supported: true,
            units: CTL_UNITS_TIME_SECONDS,
            data_type: CTL_DATA_TYPE_DOUBLE,
            value: CtlDataValue { datadouble: 2.0 },
        };
        assert!(compute_igcl_usage_pct(&mut prev_act, &a1, Some(10.0), now).is_none());
        let a2 = CtlOcTelemetryItem {
            b_supported: true,
            units: CTL_UNITS_TIME_SECONDS,
            data_type: CTL_DATA_TYPE_DOUBLE,
            value: CtlDataValue { datadouble: 2.5 },
        };
        let u = compute_igcl_usage_pct(&mut prev_act, &a2, Some(11.0), now)
            .expect("delta usage expected");
        assert!((u - 50.0).abs() < 0.01);
    }

    #[test]
    fn gpu_monitor_diagnostics_reports_all_backends() {
        let mon = GpuMonitor::new();
        let diag = mon.diagnostics();
        assert!(diag.contains("DXGI 主卡:"));
        assert!(diag.contains("AMD ADL2:"));
        assert!(diag.contains("NVIDIA NVML:"));
        assert!(diag.contains("Intel IGCL:"));
        assert!(diag.contains("PDH 状态:"));
    }

    #[test]
    fn gpu_monitor_demand_defers_and_releases_pdh_queries_and_backoff() {
        let mut mon = GpuMonitor::new_with_demand(false, false);
        assert!(!mon.has_usage_pdh());
        assert!(!mon.has_vram_pdh());
        assert!(!mon.pdh_usage_init_attempted);
        assert!(!mon.pdh_vram_init_attempted);

        let _ = mon.sample_with_demand(false, false);
        assert!(!mon.has_usage_pdh());
        assert!(!mon.has_vram_pdh());
        assert!(!mon.pdh_usage_init_attempted);
        assert!(!mon.pdh_vram_init_attempted);

        let t0 = Instant::now();
        mon.pdh_usage_backoff.record_failure(t0);
        mon.pdh_vram_backoff.record_failure(t0);
        assert!(!mon
            .pdh_usage_backoff
            .can_attempt(t0 + Duration::from_secs(1)));
        assert!(mon.release_pdh_queries());
        assert!(!mon.pdh_usage_backoff.is_backing_off());
        assert!(!mon.pdh_vram_backoff.is_backing_off());
        assert!(!mon.release_pdh_queries());
    }
}
