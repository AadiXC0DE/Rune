//! Bounded provider retry policy shared by native and SDK turns.
//!
//! Waits are jittered so parallel runs that failed together do not retry
//! together, a delay the endpoint asks for is honoured within a cap, and an
//! unreachable network is ridden out on a time budget of its own rather than
//! with the attempts meant for failures the endpoint reported.

use std::hash::BuildHasher as _;
use std::time::{Duration, Instant};

use rune_core::error::Result;
use rune_net::error::{FailureKind, NetError};

use crate::steering::Cancellation;

/// Longest delay an interactive run waits when the endpoint asks for more.
const INTERACTIVE_RETRY_AFTER_CAP: Duration = Duration::from_mins(15);

/// Longest delay a headless run without a deadline waits for.
const HEADLESS_RETRY_AFTER_CAP: Duration = Duration::from_mins(5);

/// How long an interactive run keeps trying an unreachable network.
const INTERACTIVE_UNREACHABLE_BUDGET: Duration = Duration::from_mins(10);

/// How long a headless run keeps trying an unreachable network.
const HEADLESS_UNREACHABLE_BUDGET: Duration = Duration::from_mins(2);

/// Retries after a timeout, each of which already waited a full timeout.
const MAX_TIMEOUT_RETRIES: u32 = 3;

/// Longest backoff step before jitter.
const MAX_BACKOFF_STEP: Duration = Duration::from_secs(10);

/// Longest wait between attempts on an unreachable network.
const MAX_UNREACHABLE_WAIT: Duration = Duration::from_secs(30);

/// What happens when the endpoint asks for a delay above the cap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OverCap {
    /// Wait for the cap instead.
    Clamp,
    /// Stop with the endpoint's error.
    Fail,
}

/// Retry decisions for one model step, counting the initial request as one.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Requests allowed for failures the endpoint reported.
    max_attempts: usize,
    /// Longest requested delay honoured.
    retry_after_cap: Duration,
    /// What a requested delay above the cap does.
    over_cap: OverCap,
    /// How long an unreachable network is retried, apart from the attempts.
    unreachable_budget: Duration,
    /// Random source for the backoff.
    jitter: fn() -> u64,
}

/// What one step has spent of its retry budgets.
#[derive(Clone, Copy, Debug)]
pub struct RetryState {
    attempts: usize,
    timeouts: u32,
    unreachable_since: Option<Instant>,
    unreachable_waits: u32,
    now: fn() -> Instant,
}

impl Default for RetryState {
    fn default() -> Self {
        Self {
            attempts: 0,
            timeouts: 0,
            unreachable_since: None,
            unreachable_waits: 0,
            now: Instant::now,
        }
    }
}

/// What to do after a failed request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Send the request again after this long.
    Wait(Duration),
    /// Stop with the failure.
    Stop,
    /// Stop because the endpoint asked for a delay, given here, longer than
    /// the run allows.
    TooLong(Duration),
}

impl RetryPolicy {
    /// Builds the policy for a run someone is watching, which waits out a long
    /// requested delay up to the cap.
    #[must_use]
    pub fn interactive(max_attempts: usize) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            retry_after_cap: INTERACTIVE_RETRY_AFTER_CAP,
            over_cap: OverCap::Clamp,
            unreachable_budget: INTERACTIVE_UNREACHABLE_BUDGET,
            jitter: default_jitter,
        }
    }

    /// Builds the policy for a headless run with `remaining` time left of its
    /// deadline, when it has one. A requested delay the run cannot afford ends
    /// it at once instead of waiting.
    #[must_use]
    pub fn headless(max_attempts: usize, remaining: Option<Duration>) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            retry_after_cap: remaining.unwrap_or(HEADLESS_RETRY_AFTER_CAP),
            over_cap: OverCap::Fail,
            unreachable_budget: HEADLESS_UNREACHABLE_BUDGET,
            jitter: default_jitter,
        }
    }

    /// Replaces the random source of the backoff.
    #[must_use]
    pub const fn with_jitter(mut self, jitter: fn() -> u64) -> Self {
        self.jitter = jitter;
        self
    }

    /// Decides whether to send a failed request again, and when.
    pub fn decide(&self, state: &mut RetryState, error: &NetError) -> Decision {
        let kind = error.kind();
        if matches!(kind, FailureKind::Cancelled | FailureKind::PolicyDenied) {
            return Decision::Stop;
        }
        match error.should_retry() {
            Some(false) => return Decision::Stop,
            Some(true) => {}
            None if !kind.is_retryable() => return Decision::Stop,
            None => {}
        }
        match kind {
            FailureKind::Unreachable => return self.unreachable(state),
            FailureKind::Timeout => {
                state.timeouts = state.timeouts.saturating_add(1);
                if state.timeouts > MAX_TIMEOUT_RETRIES {
                    return Decision::Stop;
                }
            }
            _ => {}
        }
        state.attempts = state.attempts.saturating_add(1);
        if state.attempts >= self.max_attempts {
            return Decision::Stop;
        }
        let mut requested = error
            .retry_after_ms()
            .map_or(Duration::ZERO, Duration::from_millis);
        if requested > self.retry_after_cap {
            match self.over_cap {
                OverCap::Clamp => requested = self.retry_after_cap,
                OverCap::Fail => return Decision::TooLong(requested),
            }
        }
        Decision::Wait(self.backoff(state.attempts).max(requested))
    }

    /// Waits on an unreachable network without spending an attempt, until its
    /// own budget runs out.
    fn unreachable(&self, state: &mut RetryState) -> Decision {
        let now = (state.now)();
        let since = *state.unreachable_since.get_or_insert(now);
        let elapsed = now.saturating_duration_since(since);
        if elapsed >= self.unreachable_budget {
            return Decision::Stop;
        }
        let factor = 1_u32
            .checked_shl(state.unreachable_waits)
            .unwrap_or(u32::MAX);
        let wait = Duration::from_secs(1)
            .saturating_mul(factor)
            .min(MAX_UNREACHABLE_WAIT)
            .min(self.unreachable_budget.saturating_sub(elapsed));
        state.unreachable_waits = state.unreachable_waits.saturating_add(1);
        Decision::Wait(wait)
    }

    /// Doubles a step from one second up to a cap, then waits a uniformly
    /// random time between half the step and the whole of it.
    fn backoff(&self, attempt: usize) -> Duration {
        let exponent = u32::try_from(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
        let factor = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
        let step = Duration::from_secs(1)
            .saturating_mul(factor)
            .min(MAX_BACKOFF_STEP);
        let half = u64::try_from(step.as_millis() / 2).unwrap_or(u64::MAX);
        let spread = (self.jitter)()
            .checked_rem(half.saturating_add(1))
            .unwrap_or(0);
        Duration::from_millis(half.saturating_add(spread))
    }
}

/// Hashes the current instant with a randomly keyed hasher, which is random
/// enough to spread retries apart without another dependency.
fn default_jitter() -> u64 {
    std::collections::hash_map::RandomState::new().hash_one(Instant::now())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn no_jitter() -> u64 {
        0
    }

    fn full_jitter() -> u64 {
        u64::MAX
    }

    fn rate_limited(retry_after_ms: Option<u64>) -> NetError {
        let error = NetError::new(FailureKind::RateLimited, "slow down");
        match retry_after_ms {
            Some(millis) => error.with_retry_after(millis),
            None => error,
        }
    }

    fn waits(policy: &RetryPolicy, state: &mut RetryState, error: &NetError) -> Vec<u128> {
        std::iter::from_fn(|| match policy.decide(state, error) {
            Decision::Wait(delay) => Some(delay.as_millis()),
            Decision::Stop | Decision::TooLong(_) => None,
        })
        .collect()
    }

    thread_local! {
        static BASE: Instant = Instant::now();
        static ELAPSED: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    }

    fn fake_now() -> Instant {
        BASE.with(|base| {
            base.checked_add(ELAPSED.with(Cell::get))
                .expect("fake time")
        })
    }

    fn advance(by: Duration) {
        ELAPSED.with(|elapsed| elapsed.set(elapsed.get().saturating_add(by)));
    }

    #[test]
    fn backoff_is_jittered_between_half_and_the_full_step() {
        let error = rate_limited(None);
        let low = RetryPolicy::interactive(8).with_jitter(no_jitter);
        assert_eq!(
            waits(&low, &mut RetryState::default(), &error),
            [500, 1000, 2000, 4000, 5000, 5000, 5000]
        );
        let high = RetryPolicy::interactive(8).with_jitter(full_jitter);
        for (wait, step) in waits(&high, &mut RetryState::default(), &error)
            .into_iter()
            .zip([1000, 2000, 4000, 8000, 10_000, 10_000, 10_000])
        {
            assert!(wait >= step / 2 && wait <= step, "{wait} for {step}");
        }
        let random = RetryPolicy::interactive(3);
        for _ in 0..50 {
            let Decision::Wait(delay) = random.decide(&mut RetryState::default(), &error) else {
                panic!("a first failure is retried");
            };
            assert!((500..=1000).contains(&delay.as_millis()), "{delay:?}");
        }
    }

    #[test]
    fn the_requested_delay_wins_when_longer() {
        let policy = RetryPolicy::interactive(10).with_jitter(no_jitter);
        assert_eq!(
            policy.decide(&mut RetryState::default(), &rate_limited(Some(20_000))),
            Decision::Wait(Duration::from_secs(20))
        );
        // A short requested delay still waits the backoff.
        assert_eq!(
            policy.decide(&mut RetryState::default(), &rate_limited(Some(0))),
            Decision::Wait(Duration::from_millis(500))
        );
        assert_eq!(
            policy.decide(&mut RetryState::default(), &rate_limited(Some(1500))),
            Decision::Wait(Duration::from_millis(1500))
        );
    }

    #[test]
    fn a_requested_delay_above_the_cap_is_clamped_or_fatal() {
        let hour = rate_limited(Some(3_600_000));
        let interactive = RetryPolicy::interactive(10).with_jitter(no_jitter);
        assert_eq!(
            interactive.decide(&mut RetryState::default(), &hour),
            Decision::Wait(Duration::from_mins(15))
        );
        let headless = RetryPolicy::headless(10, None).with_jitter(no_jitter);
        assert_eq!(
            headless.decide(&mut RetryState::default(), &hour),
            Decision::TooLong(Duration::from_secs(3600))
        );
        assert_eq!(
            headless.decide(&mut RetryState::default(), &rate_limited(Some(240_000))),
            Decision::Wait(Duration::from_secs(240))
        );
        let deadline = RetryPolicy::headless(10, Some(Duration::from_secs(10)));
        assert_eq!(
            deadline.decide(&mut RetryState::default(), &rate_limited(Some(20_000))),
            Decision::TooLong(Duration::from_secs(20))
        );
    }

    #[test]
    fn the_should_retry_header_overrides_the_kind() {
        let policy = RetryPolicy::interactive(10).with_jitter(no_jitter);
        let unavailable = NetError::new(FailureKind::Unavailable, "busy").with_status(503);
        assert!(matches!(
            policy.decide(&mut RetryState::default(), &unavailable),
            Decision::Wait(_)
        ));
        assert_eq!(
            policy.decide(
                &mut RetryState::default(),
                &unavailable.clone().with_should_retry(false)
            ),
            Decision::Stop
        );
        let invalid = NetError::new(FailureKind::InvalidRequest, "bad").with_status(400);
        assert_eq!(
            policy.decide(&mut RetryState::default(), &invalid),
            Decision::Stop
        );
        assert!(matches!(
            policy.decide(&mut RetryState::default(), &invalid.with_should_retry(true)),
            Decision::Wait(_)
        ));
        // A cancellation or a local refusal is never retried.
        for kind in [FailureKind::Cancelled, FailureKind::PolicyDenied] {
            let error = NetError::new(kind, "no").with_should_retry(true);
            assert_eq!(
                policy.decide(&mut RetryState::default(), &error),
                Decision::Stop
            );
        }
        // The attempt budget still applies.
        let single = RetryPolicy::interactive(1);
        assert_eq!(
            single.decide(&mut RetryState::default(), &rate_limited(None)),
            Decision::Stop
        );
    }

    #[test]
    fn timeouts_retry_at_most_three_times() {
        let policy = RetryPolicy::interactive(10).with_jitter(no_jitter);
        let timeout = NetError::new(FailureKind::Timeout, "slow");
        let mut state = RetryState::default();
        assert_eq!(waits(&policy, &mut state, &timeout), [500, 1000, 2000]);
        // The fourth timeout stopped, and the state stays stopped.
        assert_eq!(policy.decide(&mut state, &timeout), Decision::Stop);
        // A timeout also spends an attempt, so a single attempt never retries.
        let single = RetryPolicy::interactive(1);
        assert_eq!(
            single.decide(&mut RetryState::default(), &timeout),
            Decision::Stop
        );
    }

    #[test]
    fn an_unreachable_network_has_its_own_budget() {
        let unreachable = NetError::new(FailureKind::Unreachable, "dns");
        let policy = RetryPolicy::headless(10, None).with_jitter(no_jitter);

        // Ninety seconds of failures, then a success: no attempt is spent.
        let mut state = RetryState {
            now: fake_now,
            ..RetryState::default()
        };
        let mut waited = Vec::new();
        while ELAPSED.with(Cell::get) < Duration::from_secs(90) {
            let Decision::Wait(delay) = policy.decide(&mut state, &unreachable) else {
                panic!("stopped within the budget");
            };
            waited.push(delay.as_secs());
            advance(delay);
        }
        assert_eq!(waited, [1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(state.attempts, 0);
        // The ten attempts are all still there for other failures.
        let rate = rate_limited(None);
        assert_eq!(waits(&policy, &mut state, &rate).len(), 9);

        // Failures that go on stop at the two-minute budget.
        ELAPSED.with(|elapsed| elapsed.set(Duration::ZERO));
        let mut state = RetryState {
            now: fake_now,
            ..RetryState::default()
        };
        loop {
            match policy.decide(&mut state, &unreachable) {
                Decision::Wait(delay) => advance(delay),
                Decision::Stop => break,
                Decision::TooLong(_) => panic!("no delay was requested"),
            }
        }
        assert_eq!(ELAPSED.with(Cell::get), Duration::from_secs(120));

        // A refused connection is an ordinary network failure that spends attempts.
        let refused = NetError::new(FailureKind::Network, "refused");
        assert_eq!(
            waits(&policy, &mut RetryState::default(), &refused).len(),
            9
        );
    }
}
