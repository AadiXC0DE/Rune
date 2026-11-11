//! Keeps an idle Claude session's prompt cache from expiring.
//!
//! The provider keeps a cached prefix for five minutes after its last read. A
//! session left idle resends its last request with a one-token output ceiling
//! a little before that, a bounded number of times, so the next prompt reads
//! the cache instead of writing the whole conversation again.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rune_net::provider::{MarkerPolicy, Provider, RequestPlan};
use rune_net::stream::Usage;
use rune_net::transport::{Endpoint, RequestTimeouts, agent, stream_completion_observed};
use rune_session::usage::HelperKind;

use crate::helper_usage::{HelperUsage, note_usage};

/// Time between pings: under the provider's five-minute cache lifetime.
pub const WARM_INTERVAL: Duration = Duration::from_secs(270);

/// Pings sent in one idle period at most.
pub const MAX_PINGS: u32 = 6;

/// Longest sleep between checks that warming should stop.
const SLICE: Duration = Duration::from_secs(1);

/// Which idle period is current; a warming thread runs only while its own is.
#[derive(Debug, Default)]
pub struct WarmState {
    generation: AtomicU64,
}

impl WarmState {
    /// Ends any warming in progress and returns the new generation.
    pub fn stop(&self) -> u64 {
        self.generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }
}

/// What a warming thread sends and where its usage is counted.
pub struct Warmer {
    /// The last request of the turn, as it was sent.
    pub plan: RequestPlan,
    pub endpoint: Endpoint,
    pub dialect: Box<dyn Provider>,
    pub timeouts: RequestTimeouts,
    pub usage: HelperUsage,
    pub interval: Duration,
}

/// Starts a new idle period and warms it on its own thread.
///
/// Returns the number of pings the thread sent.
pub fn start(state: &Arc<WarmState>, warmer: Warmer) -> std::thread::JoinHandle<u32> {
    let generation = state.stop();
    let state = Arc::clone(state);
    std::thread::spawn(move || warmer.run(&state, generation))
}

impl Warmer {
    fn run(mut self, state: &WarmState, generation: u64) -> u32 {
        // Only the output ceiling changes, so the request reads the same cache.
        self.plan.max_output_tokens = Some(1);
        self.plan.cache_markers = MarkerPolicy::Full;
        let slice = SLICE.min(self.interval);
        let mut sent = 0;
        while sent < MAX_PINGS && idle_for(state, generation, self.interval, slice) {
            let mut reported = Usage::default();
            let outcome = stream_completion_observed(
                &agent(),
                &self.endpoint,
                self.dialect.as_ref(),
                &self.plan,
                self.timeouts,
                &|| !state.is_current(generation),
                &mut |event| note_usage(&mut reported, event),
            );
            sent = sent.saturating_add(1);
            self.usage.record_ended(
                HelperKind::CacheWarming,
                &self.plan.model,
                outcome.as_ref().ok().map(|done| done.usage),
                reported,
            );
            if outcome.is_err() {
                break;
            }
        }
        sent
    }
}

/// Waits `interval` in slices; false as soon as the idle period ends.
fn idle_for(state: &WarmState, generation: u64, interval: Duration, slice: Duration) -> bool {
    let started = Instant::now();
    loop {
        if !state.is_current(generation) {
            return false;
        }
        let elapsed = started.elapsed();
        if elapsed >= interval {
            return true;
        }
        std::thread::sleep(slice.min(interval.saturating_sub(elapsed)));
    }
}
