//! Cooperative host fetches must stop their work, not just SDK decoding.
#![allow(clippy::expect_used, clippy::panic)]

use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use rune_agent::turn::StopReason;
use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::config::Layer;
use rune_core::error::{ErrorCode, Result};
use rune_sdk::{
    Agent, AgentOptions, FetchContext, FetchRequest, FetchResponse, HostFetch, PromptOptions,
};

const POLL: Duration = Duration::from_millis(5);
// Allow scheduler latency on shared CI hosts in addition to the 5 ms poll.
const RELEASE_BOUND: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug)]
enum Silent {
    Send,
    Body(u16),
}

struct CooperativeFetch {
    silent: Silent,
    contexts: Sender<FetchContext>,
    reading: Sender<()>,
    released: Sender<()>,
}

impl HostFetch for CooperativeFetch {
    fn post(&self, _request: FetchRequest) -> Result<FetchResponse> {
        panic!("the SDK must call post_with_context")
    }

    fn post_with_context(
        &self,
        _request: FetchRequest,
        context: FetchContext,
    ) -> Result<(FetchResponse, Option<String>)> {
        self.contexts.send(context.clone()).expect("record context");
        match self.silent {
            Silent::Send => loop {
                context.check_head()?;
                std::thread::sleep(POLL);
            },
            Silent::Body(status) => Ok((
                FetchResponse {
                    status,
                    body: Box::new(SilentBody {
                        context,
                        reading: self.reading.clone(),
                        released: self.released.clone(),
                    }),
                },
                None,
            )),
        }
    }
}

struct SilentBody {
    context: FetchContext,
    reading: Sender<()>,
    released: Sender<()>,
}

impl Read for SilentBody {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        self.reading.send(()).expect("record blocked read");
        let watchdog = Instant::now();
        loop {
            if let Err(error) = self.context.check() {
                let kind = if error.code() == ErrorCode::Timeout {
                    io::ErrorKind::TimedOut
                } else {
                    io::ErrorKind::ConnectionAborted
                };
                return Err(io::Error::new(kind, error));
            }
            // A failed cancellation test must not leave an immortal reader.
            if watchdog.elapsed() > Duration::from_secs(5) {
                return Err(io::Error::other("fixture watchdog expired"));
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for SilentBody {
    fn drop(&mut self) {
        let _ = self.released.send(());
    }
}

fn limits(head_ms: u64, total: Budget, attempts: u64) -> BudgetSet {
    let mut limits = BudgetSet::new();
    for (name, budget) in [
        (LimitName::ProviderHeadTimeoutMs, Budget::Bounded(head_ms)),
        (LimitName::ProviderRequestTimeoutMs, total),
        (LimitName::ProviderMaxAttempts, Budget::Bounded(attempts)),
    ] {
        limits.set(name, budget, Layer::CommandLine).expect("limit");
    }
    limits
}

fn agent(fetch: Arc<dyn HostFetch>, limits: BudgetSet) -> Agent {
    Agent::new(AgentOptions {
        api_key: "fixture-key".to_owned(),
        model: Some("fixture-model".to_owned()),
        base_url: "https://example.invalid/v1".to_owned(),
        fetch: Some(fetch),
        instructions: None,
        tools: Vec::new(),
        dialect: rune_sdk::Dialect::ChatCompletions,
    })
    .expect("agent")
    .with_limits(limits)
}

fn fixture(
    silent: Silent,
    limits: BudgetSet,
) -> (Agent, Receiver<FetchContext>, Receiver<()>, Receiver<()>) {
    let (contexts, received) = mpsc::channel();
    let (reading, reads) = mpsc::channel();
    let (released, releases) = mpsc::channel();
    let agent = agent(
        Arc::new(CooperativeFetch {
            silent,
            contexts,
            reading,
            released,
        }),
        limits,
    );
    (agent, received, reads, releases)
}

#[test]
fn a_silent_body_receives_absolute_deadlines_and_releases_at_the_total_bound() {
    let (mut agent, contexts, reads, releases) =
        fixture(Silent::Body(200), limits(3000, Budget::Bounded(1000), 1));
    let started = Instant::now();
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    let context = contexts
        .recv_timeout(Duration::from_secs(1))
        .expect("context");
    let deadline = context.deadline.expect("total deadline");
    assert_eq!(
        context.head_deadline.duration_since(deadline),
        Duration::from_secs(2)
    );
    assert!(deadline.duration_since(started) >= Duration::from_secs(1));
    assert!(!context.is_cancelled());
    reads
        .recv_timeout(Duration::from_secs(1))
        .expect("read started");

    assert_eq!(
        turn.result().expect_err("deadline").code(),
        ErrorCode::Timeout
    );
    releases.recv_timeout(RELEASE_BOUND).expect("body released");
    assert!(Instant::now().saturating_duration_since(deadline) <= RELEASE_BOUND);
    assert!(context.is_cancelled(), "attempt stopped on exit");
    assert!(
        !context.signal.is_cancelled(),
        "deadline does not cancel the turn"
    );
}

#[test]
fn a_first_event_timeout_stops_a_silent_body_with_no_total_deadline() {
    let (mut agent, contexts, reads, releases) =
        fixture(Silent::Body(200), limits(1000, Budget::Unbounded, 1));
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    let context = contexts
        .recv_timeout(Duration::from_secs(1))
        .expect("context");
    assert!(context.deadline.is_none());
    reads
        .recv_timeout(Duration::from_secs(1))
        .expect("read started");
    assert_eq!(
        turn.result().expect_err("first event timeout").code(),
        ErrorCode::Timeout
    );
    let stopped = Instant::now();
    releases.recv_timeout(RELEASE_BOUND).expect("body released");
    assert!(stopped.elapsed() <= RELEASE_BOUND);
    assert!(context.is_cancelled());
    assert!(!context.signal.is_cancelled());
}

#[test]
fn closing_a_turn_interrupts_and_releases_a_silent_host_reader() {
    let (mut agent, contexts, reads, releases) =
        fixture(Silent::Body(200), limits(3000, Budget::Unbounded, 1));
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    let context = contexts
        .recv_timeout(Duration::from_secs(1))
        .expect("context");
    reads
        .recv_timeout(Duration::from_secs(1))
        .expect("read started");
    let started = Instant::now();
    agent.close().expect("close");
    assert_eq!(
        turn.result().expect("cancelled result").stop_reason,
        StopReason::Cancelled
    );
    releases.recv_timeout(RELEASE_BOUND).expect("body released");
    assert!(started.elapsed() <= RELEASE_BOUND);
    assert!(context.signal.is_cancelled());
}

#[test]
fn dropping_a_turn_interrupts_a_silent_host_send() {
    let (mut agent, contexts, _, _) = fixture(Silent::Send, limits(3000, Budget::Unbounded, 1));
    let turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    let context = contexts
        .recv_timeout(Duration::from_secs(1))
        .expect("send started");
    let started = Instant::now();
    drop(turn);
    assert!(started.elapsed() <= RELEASE_BOUND);
    assert!(context.signal.is_cancelled());
    agent.checkpoint().expect("worker settled");
}

#[test]
fn the_head_deadline_bounds_a_silent_host_send() {
    let (mut agent, contexts, _, _) = fixture(Silent::Send, limits(1000, Budget::Unbounded, 1));
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    let context = contexts
        .recv_timeout(Duration::from_secs(1))
        .expect("context");
    assert_eq!(
        turn.result().expect_err("head deadline").code(),
        ErrorCode::Timeout
    );
    assert!(Instant::now().saturating_duration_since(context.head_deadline) <= RELEASE_BOUND);
    assert!(!context.signal.is_cancelled());
}

#[test]
fn a_silent_error_body_obeys_the_total_deadline() {
    let (mut agent, _contexts, reads, releases) =
        fixture(Silent::Body(429), limits(3000, Budget::Bounded(1000), 1));
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    reads
        .recv_timeout(Duration::from_secs(1))
        .expect("error read started");
    assert_eq!(
        turn.result().expect_err("deadline").code(),
        ErrorCode::Timeout
    );
    releases
        .recv_timeout(RELEASE_BOUND)
        .expect("error body released");
}

#[test]
fn retries_receive_fresh_contexts_without_cancelling_the_turn() {
    struct RetryFetch {
        hits: AtomicUsize,
        contexts: Sender<FetchContext>,
    }
    impl HostFetch for RetryFetch {
        fn post(&self, _request: FetchRequest) -> Result<FetchResponse> {
            panic!("context method required")
        }

        fn post_with_context(
            &self,
            _request: FetchRequest,
            context: FetchContext,
        ) -> Result<(FetchResponse, Option<String>)> {
            context.check_head()?;
            self.contexts.send(context).expect("record attempt");
            if self.hits.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok((FetchResponse::new(429, "busy"), Some("0".to_owned())))
            } else {
                Ok((
                    FetchResponse::new(
                        200,
                        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                    ),
                    None,
                ))
            }
        }
    }
    let (sent, contexts) = mpsc::channel();
    let fetch = Arc::new(RetryFetch {
        hits: AtomicUsize::new(0),
        contexts: sent,
    });
    let mut agent = agent(fetch.clone(), limits(1000, Budget::Bounded(3000), 2));
    let mut turn = agent
        .prompt("work", PromptOptions::default())
        .expect("turn");
    assert_eq!(turn.result().expect("retried result").text, "ok");
    assert_eq!(fetch.hits.load(Ordering::SeqCst), 2);
    let first = contexts
        .recv_timeout(Duration::from_secs(1))
        .expect("first attempt");
    let second = contexts
        .recv_timeout(Duration::from_secs(1))
        .expect("second attempt");
    assert!(second.deadline > first.deadline);
    assert!(second.head_deadline > first.head_deadline);
    for context in [first, second] {
        assert!(
            context.is_cancelled(),
            "completed attempts stop retained readers"
        );
        assert!(!context.signal.is_cancelled(), "turn remains usable");
        assert_eq!(
            context.check().expect_err("ended attempt").code(),
            ErrorCode::Cancelled
        );
    }
    agent.checkpoint().expect("settled checkpoint");
}
