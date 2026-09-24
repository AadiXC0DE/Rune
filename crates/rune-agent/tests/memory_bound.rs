// Integration tests assert by panicking.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! Bounds on what one turn accumulates.
//!
//! A turn appends a tool result per call, and a long turn makes many calls. The
//! bytes retained across those steps are what a runaway turn would otherwise
//! grow without limit, so these tests drive many steps against a real endpoint
//! and measure the bytes the history actually kept.

use std::sync::Mutex;

use camino::Utf8PathBuf;
use rune_agent::History;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{Event, Host, StopReason, run_turn};
use rune_core::budget::{Budget, BudgetSet};
use rune_core::config::Effort;
use rune_core::error::Result;
use rune_net::message::{ContentPart, ToolSpec};
use rune_net::provider::Provider;
use rune_net::transport::Endpoint;
use rune_policy::decision::Outcome;
use rune_testkit::{MockEndpoint, Script};
use rune_tools::contract::{ExecutionContext, ToolOutput};

/// Bytes one tool call returns, far above the per-turn bound so the bound is
/// what decides the retained total rather than the size of a single result.
const RESULT_BYTES: usize = 64 * 1024;

/// A host that answers every call with a large result and records its events.
struct BoundHost {
    /// Held so the mock server outlives the host; dropping it closes the
    /// listener and every request would then fail.
    _server: MockEndpoint,
    endpoint: Endpoint,
    dialect: Box<dyn Provider>,
    tools: Vec<ToolSpec>,
    events: Mutex<Vec<Event>>,
    cancellation: Cancellation,
    steering: SteeringQueue,
    workspace: Utf8PathBuf,
    limits: BudgetSet,
}

impl BoundHost {
    /// Builds a host whose tool returns a result of [`RESULT_BYTES`].
    fn new(endpoint: MockEndpoint, limits: BudgetSet) -> Self {
        let base = endpoint.base_url();
        Self {
            _server: endpoint,
            endpoint: Endpoint::new(base, "test-key"),
            dialect: Box::new(rune_net::chat_completions::ChatCompletions),
            tools: vec![read_tool()],
            events: Mutex::new(Vec::new()),
            cancellation: Cancellation::new(),
            steering: SteeringQueue::new(8),
            workspace: Utf8PathBuf::from("/tmp/rune-test"),
            limits,
        }
    }
}

impl Host for BoundHost {
    fn dialect(&self) -> &dyn Provider {
        self.dialect.as_ref()
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn model(&self) -> String {
        "test/model".to_owned()
    }

    fn instructions(&self) -> String {
        "You are a test assistant.".to_owned()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }

    fn effort(&self) -> Effort {
        Effort::Auto
    }

    fn emit(&self, event: Event) {
        self.events.lock().expect("lock").push(event);
    }

    fn execute(&self, _name: &str, _arguments: &serde_json::Value) -> Result<ToolOutput> {
        Ok(ToolOutput::success("x".repeat(RESULT_BYTES)))
    }

    fn decide(&self, _name: &str, _target: Option<&str>) -> (Outcome, String) {
        (Outcome::Allow, "allowed".to_owned())
    }

    fn context(&self) -> ExecutionContext {
        ExecutionContext::new(self.workspace.clone())
    }

    fn limits(&self) -> BudgetSet {
        self.limits.clone()
    }

    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    fn steering(&self) -> &SteeringQueue {
        &self.steering
    }
}

fn read_tool() -> ToolSpec {
    ToolSpec {
        name: "read_file".to_owned(),
        description: "Read a file.".to_owned(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        }),
    }
}

/// Builds the limit set a test runs a turn under.
fn limits(steps: u64, result_bytes: u64) -> BudgetSet {
    let mut limits = BudgetSet::new();
    limits
        .set(
            rune_core::LimitName::MaxAgentSteps,
            Budget::Bounded(steps),
            rune_core::config::Layer::User,
        )
        .expect("steps");
    limits
        .set(
            rune_core::LimitName::MaxTurnResultBytes,
            Budget::Bounded(result_bytes),
            rune_core::config::Layer::User,
        )
        .expect("result bytes");
    limits
}

/// Returns the endpoint a turn that always asks for another tool call.
///
/// The mock repeats its last script once the sequence is exhausted, so one
/// script drives every step.
fn endpoint() -> MockEndpoint {
    MockEndpoint::start(vec![Script::tool_call(
        "call_1",
        "read_file",
        "{\"path\":\"a.rs\"}",
    )])
}

/// Returns the tool-result bytes the history retained.
///
/// Measured from the public turn list, so it is the accumulation the history
/// actually holds rather than a figure the loop reports about itself.
fn retained_result_bytes(history: &History) -> usize {
    history
        .turns()
        .iter()
        .flat_map(|turn| turn.parts.iter())
        .filter_map(|part| match part {
            ContentPart::ToolResult { content, .. } => Some(content.len()),
            _ => None,
        })
        .fold(0_usize, usize::saturating_add)
}

/// Runs one turn that asks for a tool at every step.
fn run_bounded_turn(steps: u64, result_limit: u64) -> (History, usize) {
    let host = BoundHost::new(endpoint(), limits(steps, result_limit));
    let mut history = History::new();
    history.push_user("keep reading");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::StepLimit);
    assert_eq!(u64::from(outcome.steps), steps);
    // The bound must not break the conversation: every call is still answered.
    history.validate().expect("the history is still valid");

    let retained = retained_result_bytes(&history);
    (history, retained)
}

#[test]
fn a_turn_never_retains_more_result_bytes_than_its_bound() {
    let result_limit = 4_096_u64;
    let (_history, retained) = run_bounded_turn(40, result_limit);

    assert!(
        retained <= usize::try_from(result_limit).expect("fits"),
        "the turn retained {retained} bytes against a bound of {result_limit}"
    );
    // The bound is reached rather than ignored, which is what shows it is the
    // limit holding the total and not some smaller accident.
    assert_eq!(
        retained,
        usize::try_from(result_limit).expect("fits"),
        "the turn did not use its result budget"
    );
}

#[test]
fn accumulated_result_bytes_do_not_grow_with_the_step_count() {
    let result_limit = 4_096_u64;
    let (_short, few_steps) = run_bounded_turn(5, result_limit);
    let (_long, many_steps) = run_bounded_turn(50, result_limit);

    assert!(
        many_steps <= usize::try_from(result_limit).expect("fits"),
        "50 steps retained {many_steps} bytes"
    );
    assert_eq!(
        few_steps, many_steps,
        "a turn retained more as its step count grew: {few_steps} then {many_steps}"
    );
}
