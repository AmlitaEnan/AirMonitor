use std::time::{Duration, Instant};

pub(crate) const DEFAULT_PDH_BACKOFF_BASE: Duration = Duration::from_secs(10);
pub(crate) const DEFAULT_PDH_BACKOFF_MAX: Duration = Duration::from_secs(300);

/// Capped exponential backoff shared across PDH hardware monitors.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PdhBackoff {
    failures: u32,
    next_retry_at: Option<Instant>,
    base_delay: Duration,
    max_delay: Duration,
}

impl Default for PdhBackoff {
    fn default() -> Self {
        Self::new(DEFAULT_PDH_BACKOFF_BASE, DEFAULT_PDH_BACKOFF_MAX)
    }
}

impl PdhBackoff {
    pub(crate) fn new(base_delay: Duration, max_delay: Duration) -> Self {
        Self {
            failures: 0,
            next_retry_at: None,
            base_delay,
            max_delay,
        }
    }

    pub(crate) fn can_attempt(&self, now: Instant) -> bool {
        self.next_retry_at.is_none_or(|t| now >= t)
    }

    pub(crate) fn record_success(&mut self) {
        self.failures = 0;
        self.next_retry_at = None;
    }

    pub(crate) fn record_failure(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let shift = (self.failures - 1).min(10);
        let delay = self
            .base_delay
            .saturating_mul(1u32 << shift)
            .min(self.max_delay);
        self.next_retry_at = Some(now + delay);
    }

    pub(crate) fn reset(&mut self) -> bool {
        let had_state = self.failures > 0 || self.next_retry_at.is_some();
        self.failures = 0;
        self.next_retry_at = None;
        had_state
    }

    pub(crate) fn is_backing_off(&self) -> bool {
        self.failures > 0 || self.next_retry_at.is_some()
    }

    #[allow(dead_code)]
    pub(crate) fn failure_count(&self) -> u32 {
        self.failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdh_backoff_scales_exponentially_caps_and_resets() {
        let mut backoff = PdhBackoff::default();
        let t0 = Instant::now();
        assert!(backoff.can_attempt(t0));
        assert!(!backoff.is_backing_off());

        // 1st failure -> 10s
        backoff.record_failure(t0);
        assert!(backoff.is_backing_off());
        assert_eq!(backoff.failure_count(), 1);
        assert!(!backoff.can_attempt(t0 + Duration::from_secs(9)));
        assert!(backoff.can_attempt(t0 + Duration::from_secs(10)));

        // 2nd failure -> 20s
        let t1 = t0 + Duration::from_secs(10);
        backoff.record_failure(t1);
        assert!(!backoff.can_attempt(t1 + Duration::from_secs(19)));
        assert!(backoff.can_attempt(t1 + Duration::from_secs(20)));

        // Repeated failures cap at 300s
        let mut t = t1 + Duration::from_secs(20);
        for _ in 0..8 {
            backoff.record_failure(t);
            t += Duration::from_secs(300);
        }
        assert!(backoff.can_attempt(t));
        assert!(!backoff.can_attempt(t - Duration::from_secs(1)));

        // Success clears backoff
        backoff.record_success();
        assert!(!backoff.is_backing_off());
        assert_eq!(backoff.failure_count(), 0);
        assert!(backoff.can_attempt(t));

        // Reset is idempotent and reports whether state existed
        backoff.record_failure(t);
        assert!(backoff.reset());
        assert!(!backoff.reset());
        assert!(!backoff.is_backing_off());
    }
}
