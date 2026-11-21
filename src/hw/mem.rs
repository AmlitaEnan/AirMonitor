use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

pub fn sample_memory() -> Option<(f32, f32, f32)> {
    unsafe {
        let mut stat = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if GlobalMemoryStatusEx(&mut stat).is_ok() {
            compute_memory_metrics(stat.ullTotalPhys, stat.ullAvailPhys)
        } else {
            None
        }
    }
}

fn compute_memory_metrics(total_phys: u64, avail_phys: u64) -> Option<(f32, f32, f32)> {
    if total_phys == 0 {
        return None;
    }
    let total_gb = total_phys as f64 / BYTES_PER_GB;
    let used = total_phys.saturating_sub(avail_phys);
    let used_gb = used as f64 / BYTES_PER_GB;
    let pct = ((used as f64 / total_phys as f64) * 100.0).clamp(0.0, 100.0) as f32;
    Some((pct, used_gb as f32, total_gb as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_total_memory_returns_none() {
        assert_eq!(compute_memory_metrics(0, 0), None);
    }

    #[test]
    fn memory_metrics_calculates_gb_and_clamps_pct() {
        let gib = 1024_u64 * 1024 * 1024;
        let (pct, used_gb, total_gb) =
            compute_memory_metrics(32 * gib, 8 * gib).expect("valid total");
        assert!((pct - 75.0).abs() < 1e-4);
        assert!((used_gb - 24.0).abs() < 1e-4);
        assert!((total_gb - 32.0).abs() < 1e-4);

        let (pct_sat, used_sat, _) =
            compute_memory_metrics(16 * gib, 20 * gib).expect("valid total");
        assert_eq!(pct_sat, 0.0);
        assert_eq!(used_sat, 0.0);
    }

    #[test]
    fn live_sample_memory_returns_some_sane_values() {
        let (pct, used_gb, total_gb) = sample_memory().expect("live memory sample");
        assert!(pct.is_finite() && (0.0..=100.0).contains(&pct));
        assert!(total_gb.is_finite() && total_gb > 0.0);
        assert!(used_gb.is_finite() && used_gb >= 0.0 && used_gb <= total_gb + 0.01);
    }
}
