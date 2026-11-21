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
    hw_samples: u64,
    bar_commits: Option<u64>,
}

fn parse_smoke_field<'a>(report: &'a str, key: &str) -> Option<&'a str> {
    report
        .split('|')
        .map(str::trim)
        .find_map(|part| part.strip_prefix(key).map(str::trim))
}

fn parse_smoke_metric(report: &str, key: &str, suffix: &str) -> Option<f64> {
    let raw = parse_smoke_field(report, key)?;
    let num_str = raw.strip_suffix(suffix)?.trim();
    let val: f64 = num_str.parse().ok()?;
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
    if parse_smoke_field(report, "HwWorker=") != Some("ok") {
        return Err("hw-worker did not start cleanly (HwWorker != ok)".to_string());
    }
    if parse_smoke_field(report, "WirelessWorker=") != Some("ok") {
        return Err("wireless-worker did not start cleanly (WirelessWorker != ok)".to_string());
    }
    let hw_samples: u64 = parse_smoke_field(report, "HwSamples=")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| "missing or invalid HwSamples metric".to_string())?;
    if hw_samples < 2 {
        return Err(format!(
            "insufficient hardware samples: HwSamples={hw_samples} (expected >= 2)"
        ));
    }

    let bar_commits = match parse_smoke_field(report, "BarCommits=") {
        Some("headless_skip") => None,
        Some(raw) => {
            let commits: u64 = raw
                .parse()
                .map_err(|_| format!("invalid BarCommits metric: {raw}"))?;
            if commits < 1 {
                return Err("no taskbar render commits recorded (BarCommits=0)".to_string());
            }
            if parse_smoke_field(report, "ChildAttached=") != Some("true")
                || parse_smoke_field(report, "BarVisible=") != Some("true")
                || parse_smoke_field(report, "InTaskbarRect=") != Some("true")
            {
                return Err("taskbar child attachment/visibility/bounds check failed".to_string());
            }
            Some(commits)
        }
        None => return Err("missing BarCommits metric".to_string()),
    };

    if parse_smoke_field(report, "CpuTimes=") != Some("ok") {
        return Err("process CPU timing API failed (CpuTimes != ok)".to_string());
    }
    let working_set_mb = parse_smoke_metric(report, "WorkingSet=", "MB")
        .ok_or_else(|| "missing or invalid WorkingSet metric".to_string())?;
    let private_bytes_mb = parse_smoke_metric(report, "PrivateBytes=", "MB");
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
        hw_samples,
        bar_commits,
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
        airmonitor::updater::wait_for_old_pid_if_needed();
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
                let bar_str = metrics
                    .bar_commits
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "headless_skip".to_string());
                println!(
                    "AirMonitor smoke budget PASSED | WorkingSet={:.2}/{:.2} MB | PrivateBytes={} | IdleCpuPct={:.2}%/{:.2}% | HwSamples={} | BarCommits={} | GpuUmd=[{}]",
                    metrics.working_set_mb,
                    MAX_WORKING_SET_MB,
                    metrics
                        .private_bytes_mb
                        .map(|v| format!("{v:.2} MB"))
                        .unwrap_or_else(|| "N/A".to_string()),
                    metrics.idle_cpu_pct,
                    MAX_IDLE_CPU_PCT,
                    metrics.hw_samples,
                    bar_str,
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
        let line = "AirMonitor smoke test OK | Memory=ok | WorkingSet=9.42 MB | PrivateBytes=18.10 MB | CpuTimes=ok | IdleCpuPct=0.11% | HwWorker=ok | WirelessWorker=ok | HwSamples=5 | CPU=2.0% MEM=45.0% | Devices=1 | BarCommits=2 | ChildAttached=true | BarVisible=true | InTaskbarRect=true";
        let m = validate_smoke_report(line, 15.0, 0.50).expect("should pass budget");
        assert!((m.working_set_mb - 9.42).abs() < 1e-6);
        assert_eq!(m.private_bytes_mb, Some(18.10));
        assert!((m.idle_cpu_pct - 0.11).abs() < 1e-6);
        assert_eq!(m.hw_samples, 5);
        assert_eq!(m.bar_commits, Some(2));
    }

    #[test]
    fn smoke_budget_allows_headless_skip_for_shell_tray() {
        let line = "AirMonitor smoke test OK | Memory=ok | WorkingSet=9.42 MB | PrivateBytes=18.10 MB | CpuTimes=ok | IdleCpuPct=0.11% | HwWorker=ok | WirelessWorker=ok | HwSamples=3 | BarCommits=headless_skip | ChildAttached=headless_skip | BarVisible=headless_skip | InTaskbarRect=headless_skip";
        let m = validate_smoke_report(line, 15.0, 0.50).expect("headless skip should pass");
        assert_eq!(m.hw_samples, 3);
        assert_eq!(m.bar_commits, None);
    }

    #[test]
    fn smoke_budget_rejects_excess_working_set() {
        let line = "AirMonitor smoke test OK | Memory=ok | WorkingSet=16.70 MB | PrivateBytes=71.70 MB | CpuTimes=ok | IdleCpuPct=0.12% | HwWorker=ok | WirelessWorker=ok | HwSamples=3 | BarCommits=1 | ChildAttached=true | BarVisible=true | InTaskbarRect=true";
        let err = validate_smoke_report(line, 15.0, 0.50).unwrap_err();
        assert!(err.contains("WorkingSet"));
    }

    #[test]
    fn smoke_budget_rejects_excess_idle_cpu() {
        let line = "AirMonitor smoke test OK | Memory=ok | WorkingSet=10.00 MB | PrivateBytes=20.00 MB | CpuTimes=ok | IdleCpuPct=0.85% | HwWorker=ok | WirelessWorker=ok | HwSamples=3 | BarCommits=1 | ChildAttached=true | BarVisible=true | InTaskbarRect=true";
        let err = validate_smoke_report(line, 15.0, 0.50).unwrap_err();
        assert!(err.contains("IdleCpuPct"));
    }

    #[test]
    fn smoke_budget_rejects_missing_or_failed_structured_checks() {
        let na_line =
            "AirMonitor smoke test OK | Memory=err | WorkingSet=N/A | PrivateBytes=N/A | CpuTimes=err | IdleCpuPct=N/A | HwWorker=ok | WirelessWorker=ok | HwSamples=2 | BarCommits=headless_skip";
        assert!(validate_smoke_report(na_line, 15.0, 0.50).is_err());
        assert!(validate_smoke_report("", 15.0, 0.50).is_err());

        let one_sample = "AirMonitor smoke test OK | Memory=ok | WorkingSet=9.00 MB | CpuTimes=ok | IdleCpuPct=0.10% | HwWorker=ok | WirelessWorker=ok | HwSamples=1 | BarCommits=1 | ChildAttached=true | BarVisible=true | InTaskbarRect=true";
        assert!(validate_smoke_report(one_sample, 15.0, 0.50).is_err());

        let zero_commits = "AirMonitor smoke test OK | Memory=ok | WorkingSet=9.00 MB | CpuTimes=ok | IdleCpuPct=0.10% | HwWorker=ok | WirelessWorker=ok | HwSamples=3 | BarCommits=0 | ChildAttached=true | BarVisible=true | InTaskbarRect=true";
        assert!(validate_smoke_report(zero_commits, 15.0, 0.50).is_err());

        let bad_attach = "AirMonitor smoke test OK | Memory=ok | WorkingSet=9.00 MB | CpuTimes=ok | IdleCpuPct=0.10% | HwWorker=ok | WirelessWorker=ok | HwSamples=3 | BarCommits=1 | ChildAttached=false | BarVisible=true | InTaskbarRect=true";
        assert!(validate_smoke_report(bad_attach, 15.0, 0.50).is_err());

        let bad_worker = "AirMonitor smoke test OK | Memory=ok | WorkingSet=9.00 MB | CpuTimes=ok | IdleCpuPct=0.10% | HwWorker=err | WirelessWorker=ok | HwSamples=3 | BarCommits=headless_skip";
        assert!(validate_smoke_report(bad_worker, 15.0, 0.50).is_err());
    }
}
