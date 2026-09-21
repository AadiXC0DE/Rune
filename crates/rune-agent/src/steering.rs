//! Cancellation and mid-turn steering.
//!
//! Cancellation is a single flag every long operation checks. Steering is a
//! bounded queue of messages submitted while a turn is already running: they are
//! admitted rather than rejected, and drained at explicit boundaries so the model
//! sees them at a point where it can act on them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use rune_core::budget::{BudgetSet, LimitName};
use rune_core::error::{ErrorCode, Result, RuneError};

/// Where a turn can accept steering input.
///
/// These are the only points at which a running turn reconsiders its inputs.
/// Draining anywhere else would interleave a message into the middle of a tool
/// batch, which the model cannot interpret.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Boundary {
    /// Before building the next model request.
    Model,
    /// After a stream failure, before deciding whether to retry.
    AfterFailure,
    /// After a compaction is installed.
    AfterCompaction,
    /// Before the turn finalizes.
    Finalizing,
    /// After a cancellation was observed.
    Cancelled,
}

impl Boundary {
    /// Returns the wire representation, for tracing.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::AfterFailure => "after_failure",
            Self::AfterCompaction => "after_compaction",
            Self::Finalizing => "finalizing",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One message submitted while a turn may be running.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Steering {
    /// The submitted text.
    pub text: String,
    /// Boundary at which it was drained, once it has been.
    pub drained_at: Option<Boundary>,
}

/// A cancellation flag shared with everything working on a turn.
#[derive(Clone, Debug, Default)]
pub struct Cancellation {
    flag: Arc<AtomicBool>,
}

impl Cancellation {
    /// Returns a fresh flag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation.
    ///
    /// Idempotent, because a second interrupt while the first is being handled
    /// is normal.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Returns true when cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Clears the flag, for reuse across turns.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    /// Returns an error when cancellation was requested.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(cancelled_error());
        }
        Ok(())
    }
}

/// The bounded steering queue for one turn.
#[derive(Debug)]
pub struct SteeringQueue {
    inner: Mutex<Vec<Steering>>,
    signal: Condvar,
    capacity: usize,
}

impl SteeringQueue {
    /// Builds a queue with the configured depth.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
            signal: Condvar::new(),
            capacity: capacity.max(1),
        }
    }

    /// Builds a queue sized from the limits.
    #[must_use]
    pub fn from_limits(limits: &BudgetSet) -> Self {
        let capacity = limits.get_usize(LimitName::SteeringQueueDepth).max(1);
        Self::new(capacity)
    }

    /// Submits a message.
    ///
    /// Fails rather than growing without bound, because an unbounded queue turns
    /// a runaway producer into a memory exhaustion.
    pub fn submit(&self, text: impl Into<String>) -> Result<()> {
        let text = text.into();
        let mut guard = self.lock()?;
        if guard.len() >= self.capacity {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                format!("the steering queue is full at {} messages", self.capacity),
            )
            .with_hint("wait for the current turn to reach a boundary"));
        }
        guard.push(Steering {
            text,
            drained_at: None,
        });
        drop(guard);
        self.signal.notify_all();
        Ok(())
    }

    /// Returns the number of queued messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().map_or(0, |guard| guard.len())
    }

    /// Returns true when nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the queue capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Drains every queued message, recording the boundary.
    pub fn drain(&self, boundary: Boundary) -> Vec<Steering> {
        let Ok(mut guard) = self.lock() else {
            return Vec::new();
        };
        let mut drained: Vec<Steering> = guard.drain(..).collect();
        for message in &mut drained {
            message.drained_at = Some(boundary);
        }
        drained
    }

    /// Returns a copy of the queued messages without removing them.
    #[must_use]
    pub fn peek(&self) -> Vec<Steering> {
        self.lock()
            .map_or_else(|_| Vec::new(), |guard| guard.clone())
    }

    /// Waits until at least one message is queued, or the timeout elapses.
    ///
    /// Returns the drained messages, which may be empty on a timeout. The wait
    /// and the drain share one lock acquisition, so a message submitted while
    /// the caller is waking up cannot be lost between them.
    pub fn wait_and_drain(
        &self,
        boundary: Boundary,
        timeout: std::time::Duration,
    ) -> Vec<Steering> {
        let Ok(guard) = self.lock() else {
            return Vec::new();
        };
        // Sleeping while holding the guard is what makes the drain atomic: a
        // submitter cannot slip a message in after the predicate is checked.
        let Ok((mut guard, _)) = self
            .signal
            .wait_timeout_while(guard, timeout, |queue| queue.is_empty())
        else {
            return Vec::new();
        };

        let mut drained: Vec<Steering> = guard.drain(..).collect();
        drop(guard);
        for message in &mut drained {
            message.drained_at = Some(boundary);
        }
        drained
    }

    /// Locks the queue, converting a poisoned lock into an error.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Vec<Steering>>> {
        self.inner.lock().map_err(|_| {
            RuneError::new(
                ErrorCode::Internal,
                "the steering queue lock was poisoned by a panicking thread",
            )
        })
    }
}

/// Builds the error used for a cancellation.
#[must_use]
pub fn cancelled_error() -> RuneError {
    RuneError::new(ErrorCode::Cancelled, "the turn was cancelled")
}

/// Renders steering text as the message the model receives.
///
/// Delimited so the model can tell guidance submitted mid-turn from the original
/// request, which matters when the two conflict.
#[must_use]
pub fn render_steering(messages: &[Steering]) -> String {
    let mut out = String::new();
    for message in messages {
        out.push_str("<user_steering>\n");
        out.push_str(&message.text);
        out.push_str("\n</user_steering>\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn a_fresh_cancellation_is_not_requested() {
        let cancellation = Cancellation::new();
        assert!(!cancellation.is_cancelled());
        assert!(cancellation.check().is_ok());
    }

    #[test]
    fn cancellation_is_idempotent() {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        cancellation.cancel();
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn a_cancelled_turn_reports_the_cancelled_code() {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let err = cancellation.check().expect_err("cancelled");
        assert_eq!(err.code(), ErrorCode::Cancelled);
    }

    #[test]
    fn cancellation_is_shared_across_clones() {
        let cancellation = Cancellation::new();
        let handle = cancellation.clone();
        handle.cancel();
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn cancellation_can_be_reset_between_turns() {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        cancellation.reset();
        assert!(!cancellation.is_cancelled());
    }

    #[test]
    fn a_queue_starts_empty() {
        let queue = SteeringQueue::new(8);
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.capacity(), 8);
    }

    #[test]
    fn submitting_queues_a_message() {
        let queue = SteeringQueue::new(8);
        queue.submit("change of plan").expect("accepted");
        assert_eq!(queue.len(), 1);
        let peeked = queue.peek();
        assert_eq!(peeked[0].text, "change of plan");
        assert!(peeked[0].drained_at.is_none());
    }

    #[test]
    fn draining_returns_everything_and_records_the_boundary() {
        let queue = SteeringQueue::new(8);
        queue.submit("one").expect("accepted");
        queue.submit("two").expect("accepted");
        let drained = queue.drain(Boundary::Model);
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text, "one");
        assert_eq!(drained[0].drained_at, Some(Boundary::Model));
        assert!(queue.is_empty());
    }

    #[test]
    fn a_full_queue_refuses_rather_than_growing() {
        let queue = SteeringQueue::new(2);
        queue.submit("one").expect("accepted");
        queue.submit("two").expect("accepted");
        let err = queue.submit("three").expect_err("full");
        assert_eq!(err.code(), ErrorCode::LimitExceeded);
        assert!(err.detail().hint.is_some());
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn a_zero_capacity_queue_still_accepts_one_message() {
        // A capacity of zero would make the queue unusable, so it is raised.
        let queue = SteeringQueue::new(0);
        assert_eq!(queue.capacity(), 1);
        queue.submit("one").expect("accepted");
        assert!(queue.submit("two").is_err());
    }

    #[test]
    fn the_queue_depth_comes_from_the_limits() {
        let limits = BudgetSet::new();
        let queue = SteeringQueue::from_limits(&limits);
        assert_eq!(
            queue.capacity(),
            usize::try_from(
                LimitName::SteeringQueueDepth
                    .default_value()
                    .value()
                    .unwrap_or(64)
            )
            .unwrap_or(64)
        );
    }

    #[test]
    fn peeking_does_not_remove() {
        let queue = SteeringQueue::new(4);
        queue.submit("one").expect("accepted");
        assert_eq!(queue.peek().len(), 1);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn waiting_drains_a_queued_message() {
        let queue = Arc::new(SteeringQueue::new(4));
        let producer = Arc::clone(&queue);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            let _ = producer.submit("late guidance");
        });
        let drained = queue.wait_and_drain(Boundary::Model, std::time::Duration::from_secs(2));
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].text, "late guidance");
        assert!(queue.is_empty());
    }

    #[test]
    fn a_message_submitted_during_the_wait_is_not_lost() {
        // The wait and the drain share one lock acquisition, so a submitter
        // racing the reader must either be seen by this wait or remain queued
        // for the next one. Losing it would silently drop user guidance.
        //
        // The producer submits without sleeping, so the assertion does not
        // depend on scheduling: every message is either drained here or still
        // queued at the end, and the total must be exact either way.
        const TOTAL: usize = 200;

        let queue = Arc::new(SteeringQueue::new(TOTAL));
        let producer = Arc::clone(&queue);
        let finished = Arc::new(AtomicBool::new(false));
        let producer_done = Arc::clone(&finished);

        std::thread::spawn(move || {
            for index in 0..TOTAL {
                let _ = producer.submit(format!("message {index}"));
            }
            producer_done.store(true, Ordering::SeqCst);
        });

        let mut seen = 0_usize;
        // Wait until the producer is done, then drain whatever remains.
        while !finished.load(Ordering::SeqCst) {
            seen += queue
                .wait_and_drain(Boundary::Model, std::time::Duration::from_millis(5))
                .len();
        }
        seen += queue.drain(Boundary::Model).len();

        assert_eq!(
            seen, TOTAL,
            "messages were lost between the wait and the drain"
        );
    }

    #[test]
    fn waiting_returns_empty_on_a_timeout() {
        let queue = SteeringQueue::new(4);
        let drained = queue.wait_and_drain(Boundary::Model, std::time::Duration::from_millis(20));
        assert!(drained.is_empty());
    }

    #[test]
    fn steering_is_delimited_when_rendered() {
        let messages = vec![
            Steering {
                text: "first".to_owned(),
                drained_at: Some(Boundary::Model),
            },
            Steering {
                text: "second".to_owned(),
                drained_at: Some(Boundary::Model),
            },
        ];
        let rendered = render_steering(&messages);
        assert!(rendered.contains("<user_steering>\nfirst\n</user_steering>"));
        assert!(rendered.contains("<user_steering>\nsecond\n</user_steering>"));
    }

    #[test]
    fn rendering_nothing_produces_nothing() {
        assert!(render_steering(&[]).is_empty());
    }

    #[test]
    fn boundaries_have_distinct_names() {
        let boundaries = [
            Boundary::Model,
            Boundary::AfterFailure,
            Boundary::AfterCompaction,
            Boundary::Finalizing,
            Boundary::Cancelled,
        ];
        let mut seen = std::collections::HashSet::new();
        for boundary in boundaries {
            assert!(seen.insert(boundary.as_str()), "duplicate {boundary:?}");
        }
    }
}
