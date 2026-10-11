//! Bounded provider retry policy shared by native and SDK turns.

use std::time::{Duration, Instant};

use rune_core::error::Result;
use rune_net::error::FailureKind;

use crate::steering::Cancellation;

/// Retry decisions for one model step, counting the initial request as one.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    max_attempts: usize,
}

impl RetryPolicy {
    /// Builds a policy with at least one request attempt.
    #[must_use]
    pub const fn new(max_attempts: usize) -> Self {
        Self {
            max_attempts: if max_attempts == 0 { 1 } else { max_attempts },
        }
    }

    /// Returns the wait before another attempt, or stops on exhaustion or a
    /// permanent failure. An endpoint's delay takes precedence over backoff.
    #[must_use]
    pub fn delay(
        self,
        attempt: usize,
        kind: FailureKind,
        retry_after_ms: Option<u64>,
    ) -> Option<Duration> {
        if attempt >= self.max_attempts || !kind.is_retryable() {
            return None;
        }
        Some(retry_after_ms.map_or_else(|| backoff(attempt), Duration::from_millis))
    }
}

/// Waits in short slices so cancelling a turn interrupts its retry delay.
pub fn wait(delay: Duration, cancellation: &Cancellation) -> Result<()> {
    let started = Instant::now();
    loop {
        cancellation.check()?;
        let remaining = delay.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(remaining.min(Duration::from_millis(25)));
    }
}

/// Doubles from a small base and caps to avoid hammering an unavailable endpoint.
fn backoff(attempt: usize) -> Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1)).unwrap_or(0).min(5);
    let factor = 1_u64.checked_shl(exponent).unwrap_or(u64::MAX);
    let millis = 250_u64.saturating_mul(factor);
    Duration::from_millis(millis.min(8_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(1), Duration::from_millis(250));
        assert_eq!(backoff(2), Duration::from_millis(500));
        assert_eq!(backoff(3), Duration::from_millis(1000));
        assert_eq!(backoff(4), Duration::from_millis(2000));
        assert_eq!(backoff(5), Duration::from_millis(4000));
        assert_eq!(backoff(6), Duration::from_millis(8000));
        assert_eq!(backoff(20), Duration::from_millis(8000));
    }

    #[test]
    fn retry_after_wins_but_exhaustion_and_permanent_failures_stop() {
        let policy = RetryPolicy::new(2);
        assert_eq!(
            policy.delay(1, FailureKind::RateLimited, Some(1000)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(policy.delay(2, FailureKind::RateLimited, Some(1000)), None);
        assert_eq!(policy.delay(1, FailureKind::Unauthorized, None), None);
        assert_eq!(policy.delay(1, FailureKind::PolicyDenied, None), None);
    }
}
