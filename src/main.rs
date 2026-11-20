#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::ffi::c_void;
use std::io::Write;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Console::{
    AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleW, SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_ACCESS_RIGHTS};

const SINGLE_INSTANCE_MUTEX: PCWSTR = w!("Local\\AirMonitor_SingleInstance_v1");
const SYNCHRONIZE_ACCESS: SYNCHRONIZATION_ACCESS_RIGHTS =
    SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000);
const MAX_WORKING_SET_MB: f64 = 15.0;
const MAX_IDLE_CPU_PCT: f64 = 0.50;

#[link(name = "kernel32")]
extern "system" {
    fn CreatePipe(
        h_read_pipe: *mut HANDLE,
        h_write_pipe: *mut HANDLE,
        lp_pipe_attributes: *const c_void,
        n_size: u32,
    ) -> i32;
}

fn is_another_instance_running() -> bool {
    unsafe {
        if let Ok(handle) = OpenMutexW(SYNCHRONIZE_ACCESS, false, SINGLE_INSTANCE_MUTEX) {
            if !handle.is_invalid() {
                let _ = CloseHandle(handle);
            }
            return true;
        }
    }
    false
}

struct StdoutPipeCapture {
    orig_stdout: HANDLE,
    read_pipe: HANDLE,
    write_pipe: HANDLE,
}

impl StdoutPipeCapture {
    fn start() -> Option<Self> {
        unsafe {
            let orig_stdout = GetStdHandle(STD_OUTPUT_HANDLE).ok()?;
            let mut read_pipe = INVALID_HANDLE_VALUE;
            let mut write_pipe = INVALID_HANDLE_VALUE;
            if CreatePipe(&mut read_pipe, &mut write_pipe, std::ptr::null(), 64 * 1024) == 0 {
                return None;
            }
            if SetStdHandle(STD_OUTPUT_HANDLE, write_pipe).is_err() {
                let _ = CloseHandle(read_pipe);
                let _ = CloseHandle(write_pipe);
                return None;
            }
            Some(Self {
                orig_stdout,
                read_pipe,
                write_pipe,
            })
        }
    }

    fn finish(mut self) -> String {
        unsafe {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, self.orig_stdout);
            if !self.write_pipe.is_invalid() {
                let _ = CloseHandle(self.write_pipe);
                self.write_pipe = INVALID_HANDLE_VALUE;
            }
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let mut read = 0u32;
                if ReadFile(self.read_pipe, Some(&mut chunk), Some(&mut read), None).is_err()
                    || read == 0
                {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read as usize]);
            }
            if !self.read_pipe.is_invalid() {
                let _ = CloseHandle(self.read_pipe);
                self.read_pipe = INVALID_HANDLE_VALUE;
            }
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }
}

impl Drop for StdoutPipeCapture {
    fn drop(&mut self) {
        unsafe {
            if !self.write_pipe.is_invalid() {
                let _ = SetStdHandle(STD_OUTPUT_HANDLE, self.orig_stdout);
                let _ = CloseHandle(self.write_pipe);
            }
            if !self.read_pipe.is_invalid() {
                let _ = CloseHandle(self.read_pipe);
            }
        }
    }
}

#[derive(Debug, PartialEq)]
struct SmokeMetrics {
    working_set_mb: f64,
    private_bytes_mb: Option<f64>,
    idle_cpu_pct: f64,
}

fn parse_smoke_metric(report: &str, key: &str, suffix: &str) -> Option<f64> {
    let start = report.find(key)? + key.len();
    let rest = &report[start..];
    let end = rest.find(suffix)?;
    let val: f64 = rest[..end].trim().parse().ok()?;
    val.is_finite().then_some(val)
}

fn validate_smoke_report(
    report: &str,
    max_ws_mb: f64,
    max_idle_cpu_pct: f64,
) -> Result<SmokeMetrics, String> {
    if !report.contains("AirMonitor smoke test OK") {
        return Err("missing 'AirMonitor smoke test OK' line in smoke output".to_string());
    }
    let working_set_mb = parse_smoke_metric(report, "WorkingSet=", " MB")
        .ok_or_else(|| "missing or invalid WorkingSet metric".to_string())?;
    let private_bytes_mb = parse_smoke_metric(report, "PrivateBytes=", " MB");
    let idle_cpu_pct = parse_smoke_metric(report, "IdleCpuPct=", "%")
        .ok_or_else(|| "missing or invalid IdleCpuPct metric".to_string())?;

    if working_set_mb > max_ws_mb {
        return Err(format!(
            "WorkingSet {:.2} MB exceeds budget {:.2} MB",
            working_set_mb, max_ws_mb
        ));
    }
    if idle_cpu_pct > max_idle_cpu_pct {
        return Err(format!(
            "IdleCpuPct {:.2}% exceeds budget {:.2}%",
            idle_cpu_pct, max_idle_cpu_pct
        ));
    }

    Ok(SmokeMetrics {
        working_set_mb,
        private_bytes_mb,
        idle_cpu_pct,
    })
}

fn loaded_gpu_umds() -> Vec<&'static str> {
    const CANDIDATES: &[(&str, PCWSTR)] = &[
        ("amdxx64.dll", w!("amdxx64.dll")),
        ("aticfx64.dll", w!("aticfx64.dll")),
        ("nvwgf2umx.dll", w!("nvwgf2umx.dll")),
        ("nvldumdx.dll", w!("nvldumdx.dll")),
        ("igd10iumd64.dll", w!("igd10iumd64.dll")),
    ];
    let mut loaded = Vec::new();
    for &(name, wide) in CANDIDATES {
        if unsafe { GetModuleHandleW(wide).is_ok() } {
            loaded.push(name);
        }
    }
    loaded
}

fn main() {
    unsafe {
        let _ = SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
    }

    let smoke_test = std::env::args().any(|arg| arg == "--smoke-test");

    if smoke_test {
        airmonitor::config::set_read_only_mode(true);
        unsafe {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
        if is_another_instance_running() {
            eprintln!(
                "AirMonitor smoke test FAILED: another AirMonitor instance is already running."
            );
            std::process::exit(1);
        }
    } else {
        airmonitor::updater::handle_post_update_startup();
    }

    let capture = if smoke_test {
        StdoutPipeCapture::start()
    } else {
        None
    };

    let code = airmonitor::ui::run(smoke_test);
    airmonitor::ui::renderer::release_thread_local_d2d();
    let captured_report = capture.map(StdoutPipeCapture::finish);
    let gpu_umds = if smoke_test {
        loaded_gpu_umds()
    } else {
        Vec::new()
    };

    if let Some(ref text) = captured_report {
        if !text.is_empty() {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
    }

    if code != 0 {
        if smoke_test {
            eprintln!("AirMonitor smoke test FAILED: exited with code {code}.");
        }
        std::process::exit(code);
    }

    if smoke_test {
        let report = captured_report.as_deref().unwrap_or("");
        match validate_smoke_report(report, MAX_WORKING_SET_MB, MAX_IDLE_CPU_PCT) {
            Ok(metrics) => {
                let umd_str = if gpu_umds.is_empty() {
                    "none".to_string()
                } else {
                    gpu_umds.join(",")
                };
                println!(
                    "AirMonitor smoke budget PASSED | WorkingSet={:.2}/{:.2} MB | PrivateBytes={} | IdleCpuPct={:.2}%/{:.2}% | GpuUmd=[{}]",
                    metrics.working_set_mb,
                    MAX_WORKING_SET_MB,
                    metrics
                        .private_bytes_mb
                        .map(|v| format!("{v:.2} MB"))
                        .unwrap_or_else(|| "N/A".to_string()),
                    metrics.idle_cpu_pct,
                    MAX_IDLE_CPU_PCT,
                    umd_str
                );
                let _ = std::io::stdout().flush();
            }
            Err(err) => {
                eprintln!("AirMonitor smoke test FAILED: {err}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_budget_passes_when_within_limits() {
        let line = "AirMonitor smoke test OK | WorkingSet=9.42 MB | PrivateBytes=18.10 MB | IdleCpuPct=0.11% | CPU=2.0% MEM=45.0% | Devices=1 | ok";
        let m = validate_smoke_report(line, 15.0, 0.50).expect("should pass budget");
        assert!((m.working_set_mb - 9.42).abs() < 1e-6);
        assert_eq!(m.private_bytes_mb, Some(18.10));
        assert!((m.idle_cpu_pct - 0.11).abs() < 1e-6);
    }

    #[test]
    fn smoke_budget_rejects_excess_working_set() {
        let line = "AirMonitor smoke test OK | WorkingSet=16.70 MB | PrivateBytes=71.70 MB | IdleCpuPct=0.12%";
        let err = validate_smoke_report(line, 15.0, 0.50).unwrap_err();
        assert!(err.contains("WorkingSet"));
    }

    #[test]
    fn smoke_budget_rejects_excess_idle_cpu() {
        let line = "AirMonitor smoke test OK | WorkingSet=10.00 MB | PrivateBytes=20.00 MB | IdleCpuPct=0.85%";
        let err = validate_smoke_report(line, 15.0, 0.50).unwrap_err();
        assert!(err.contains("IdleCpuPct"));
    }

    #[test]
    fn smoke_budget_rejects_missing_or_na_metrics() {
        let na_line =
            "AirMonitor smoke test OK | WorkingSet=N/A | PrivateBytes=N/A | IdleCpuPct=N/A";
        assert!(validate_smoke_report(na_line, 15.0, 0.50).is_err());
        assert!(validate_smoke_report("", 15.0, 0.50).is_err());
    }
}
