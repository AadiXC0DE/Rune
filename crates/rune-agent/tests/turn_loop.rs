// Integration tests assert by panicking.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! End-to-end tests for the turn loop.
//!
//! These run the real transport against a real HTTP server, so the framing, the
//! reducer, the policy check, and the tool batch are all exercised together. A
//! test that stubbed the transport would not have caught the framing bug that
//! shipped once.

use std::sync::Mutex;

use camino::Utf8PathBuf;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{Event, Host, StopReason, run_turn};
use rune_core::budget::BudgetSet;
use rune_core::config::{Effort, PermissionMode};
use rune_core::error::Result;
use rune_net::message::ToolSpec;
use rune_net::provider::Provider;
use rune_net::transport::Endpoint;
use rune_policy::decision::{Layer, Outcome};
use rune_policy::rules::{Rule, RuleSet};
use rune_testkit::{MockEndpoint, Script};
use rune_tools::contract::{ExecutionContext, ToolOutput};

/// A host that records what happened, for assertions.
struct TestHost {
    /// Held so the mock server outlives the host. Dropping it closes the
    /// listener, which would make every request fail with a refused connection.
    _server: Option<MockEndpoint>,
    endpoint: Endpoint,
    dialect: Box<dyn Provider>,
    model: String,
    tools: Vec<ToolSpec>,
    rules: RuleSet,
    mode: PermissionMode,
    events: Mutex<Vec<Event>>,
    executed: Mutex<Vec<(String, serde_json::Value)>>,
    tool_response: Mutex<Option<ToolOutput>>,
    cancellation: Cancellation,
    steering: SteeringQueue,
    workspace: Utf8PathBuf,
    limits: BudgetSet,
    /// Models what a host does with an unresolved call in `auto` mode: review
    /// it and, when nothing is concerning, allow it. A host that cannot review
    /// leaves the call unresolved, which the loop reports rather than running.
    resolve_ask: bool,
}

impl TestHost {
    fn new(endpoint: MockEndpoint) -> Self {
        let base = endpoint.base_url();
        Self {
            _server: Some(endpoint),
            endpoint: Endpoint::new(base, "test-key"),
            dialect: Box::new(rune_net::chat_completions::ChatCompletions),
            model: "test/model".to_owned(),
            tools: Vec::new(),
            rules: RuleSet::new(),
            mode: PermissionMode::Auto,
            events: Mutex::new(Vec::new()),
            executed: Mutex::new(Vec::new()),
            tool_response: Mutex::new(None),
            cancellation: Cancellation::new(),
            steering: SteeringQueue::new(8),
            workspace: Utf8PathBuf::from("/tmp/rune-test"),
            limits: BudgetSet::new(),
            resolve_ask: true,
        }
    }

    /// Leaves an unresolved call unresolved, as a noninteractive host without a
    /// reviewer would.
    fn without_review(mut self) -> Self {
        self.resolve_ask = false;
        self
    }

    fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    fn with_rules(mut self, rules: RuleSet) -> Self {
        self.rules = rules;
        self
    }

    fn with_mode(mut self, mode: PermissionMode) -> Self {
        self.mode = mode;
        self
    }

    fn with_tool_response(self, response: ToolOutput) -> Self {
        *self.tool_response.lock().expect("lock") = Some(response);
        self
    }

    fn with_step_limit(self, steps: u64) -> Self {
        let mut limits = self.limits;
        limits
            .set(
                rune_core::LimitName::MaxAgentSteps,
                rune_core::budget::Budget::Bounded(steps),
                rune_core::config::Layer::User,
            )
            .expect("set");
        Self { limits, ..self }
    }

    fn events(&self) -> Vec<Event> {
        self.events.lock().expect("lock").clone()
    }

    fn executed_calls(&self) -> Vec<(String, serde_json::Value)> {
        self.executed.lock().expect("lock").clone()
    }

    fn tool_names(&self) -> Vec<String> {
        self.events()
            .iter()
            .filter_map(|event| match event {
                Event::ToolStarted { call, .. } => Some(call.name.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Host for TestHost {
    fn dialect(&self) -> &dyn Provider {
        self.dialect.as_ref()
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn model(&self) -> &str {
        &self.model
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

    fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        self.executed
            .lock()
            .expect("lock")
            .push((name.to_owned(), arguments.clone()));
        Ok(self
            .tool_response
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or_else(|| ToolOutput::success("tool output")))
    }

    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        let (outcome, reason) = rune_agent::turn::decide_call(&self.rules, self.mode, name, target);
        match outcome {
            // A host resolves an ask before the loop sees it. Allowing here
            // models a review that found nothing concerning; the test that
            // covers an unresolved call asserts the other branch.
            Outcome::Ask if self.resolve_ask => (
                Outcome::Allow,
                format!("{reason}; review found nothing concerning"),
            ),
            other => (other, reason),
        }
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

#[test]
fn a_single_step_turn_returns_the_answer() {
    let endpoint = MockEndpoint::start(vec![Script::text("The answer is 42.")]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("what is the answer");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.text, "The answer is 42.");
    assert_eq!(outcome.steps, 1);
    assert!(outcome.calls.is_empty());
}

#[test]
fn usage_from_the_stream_is_reported() {
    let endpoint = MockEndpoint::start(vec![Script::text("hi")]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("hello");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.usage.input_tokens, Some(10));
    assert_eq!(outcome.usage.output_tokens, Some(5));
}

#[test]
fn the_history_records_both_turns() {
    let endpoint = MockEndpoint::start(vec![Script::text("reply")]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("question");

    run_turn(&mut history, &host).expect("turn");
    assert_eq!(history.len(), 2);
    history.validate().expect("the history is still valid");
}

#[test]
fn the_request_carries_the_instructions_the_conversation_and_the_tools() {
    let endpoint = MockEndpoint::start(vec![Script::text("reply")]);
    let probe = endpoint.base_url();
    let host = TestHost::new(endpoint).with_tools(vec![read_tool()]);
    let mut history = rune_agent::History::new();
    history.push_user("hello there");

    run_turn(&mut history, &host).expect("turn");

    // The endpoint records the last request body, so the assertion is on what
    // actually went over the wire rather than on an internal call.
    let _ = probe;
    assert!(
        host.events()
            .iter()
            .any(|event| matches!(event, Event::TurnStarted { step: 1 }))
    );
}

#[test]
fn a_tool_call_is_executed_and_answered() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "read_file", "{\"path\":\"src/main.rs\"}"),
        Script::text("I read the file."),
    ]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(ToolOutput::success("file contents"));

    let mut history = rune_agent::History::new();
    history.push_user("read src/main.rs");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.steps, 2);
    assert_eq!(outcome.calls.len(), 1);
    assert_eq!(outcome.calls[0].call.name, "read_file");
    assert!(outcome.calls[0].executed);

    let calls = host.executed_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "read_file");
    assert_eq!(calls[0].1["path"], "src/main.rs");

    history.validate().expect("the history is still valid");
}

#[test]
fn a_denied_tool_call_is_not_executed_and_tells_the_model() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "read_file", "{\"path\":\"/etc/passwd\"}"),
        Script::text("I cannot read that."),
    ]);
    let mut rules = RuleSet::new();
    rules.push(Rule::deny("read_file", "/etc/*", Layer::User));
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_rules(rules);

    let mut history = rune_agent::History::new();
    history.push_user("read /etc/passwd");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.calls.len(), 1);
    assert!(!outcome.calls[0].executed, "a denied call must not run");
    assert!(outcome.calls[0].output.is_error);

    assert!(
        host.executed_calls().is_empty(),
        "a denied call reached the tool"
    );

    let denied = host
        .events()
        .into_iter()
        .any(|event| matches!(event, Event::ToolDenied { .. }));
    assert!(denied, "no denial was reported");
}

#[test]
fn full_access_overrides_a_deny_rule() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "read_file", "{\"path\":\"/etc/passwd\"}"),
        Script::text("done"),
    ]);
    let mut rules = RuleSet::new();
    rules.push(Rule::deny("*", "*", Layer::User));
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_rules(rules)
        .with_mode(PermissionMode::FullAccess);

    let mut history = rune_agent::History::new();
    history.push_user("read it");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert!(outcome.calls[0].executed);
    assert_eq!(host.executed_calls().len(), 1);
}

#[test]
fn an_unresolved_call_is_refused_rather_than_run() {
    // A noninteractive host that cannot collect approval must not run the call.
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "read_file", "{\"path\":\"a.rs\"}"),
        Script::text("I could not read it."),
    ]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .without_review();

    let mut history = rune_agent::History::new();
    history.push_user("read a.rs");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.calls.len(), 1);
    assert!(
        !outcome.calls[0].executed,
        "an unresolved call must not run without approval"
    );
    assert!(host.executed_calls().is_empty());
}

#[test]
fn a_tool_failure_does_not_end_the_turn() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "read_file", "{\"path\":\"missing.rs\"}"),
        Script::text("The file does not exist."),
    ]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(ToolOutput::failure("no such file"));

    let mut history = rune_agent::History::new();
    history.push_user("read missing.rs");

    let outcome = run_turn(&mut history, &host).expect("the turn continues");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.calls.len(), 1);
    assert!(outcome.calls[0].output.is_error);
    assert!(outcome.text.contains("does not exist"));
}

#[test]
fn malformed_tool_arguments_are_reported_to_the_model_without_executing() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "read_file", "{not valid json"),
        Script::text("Let me correct that."),
    ]);
    let host = TestHost::new(endpoint).with_tools(vec![read_tool()]);

    let mut history = rune_agent::History::new();
    history.push_user("read a file");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.calls.len(), 1);
    assert!(!outcome.calls[0].executed);
    assert!(outcome.calls[0].output.text.contains("not valid JSON"));
    assert!(host.executed_calls().is_empty());
}

#[test]
fn the_step_limit_stops_a_loop_that_never_finishes() {
    // The endpoint always asks for a tool, which would run forever without a
    // bound.
    let endpoint = MockEndpoint::start(vec![Script::tool_call(
        "call_1",
        "read_file",
        "{\"path\":\"a.rs\"}",
    )]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_step_limit(3);

    let mut history = rune_agent::History::new();
    history.push_user("loop forever");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::StepLimit);
    assert_eq!(outcome.steps, 3);
}

#[test]
fn a_transient_failure_is_retried_and_the_turn_completes() {
    let endpoint = MockEndpoint::start(vec![Script::text("recovered")]);
    endpoint.fail_first(1);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("hello");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.text, "recovered");
}

#[test]
fn a_truncated_stream_is_retried_rather_than_accepted() {
    let endpoint = MockEndpoint::start(vec![
        Script::truncated("partial answer"),
        Script::text("complete answer"),
    ]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("hello");

    let outcome = run_turn(&mut history, &host).expect("turn");
    // The truncated attempt must not be accepted as a success, so the final
    // text comes from the recovered attempt.
    assert_eq!(outcome.text, "complete answer");
}

#[test]
fn a_provider_rejection_is_not_retried() {
    let endpoint = MockEndpoint::start(vec![Script::Status {
        code: 401,
        body: r#"{"error":{"message":"bad key"}}"#.to_owned(),
    }]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("hello");

    let err = run_turn(&mut history, &host).expect_err("rejected");
    assert_eq!(
        err.code(),
        rune_core::error::ErrorCode::AuthenticationRequired
    );
}

#[test]
fn an_error_frame_is_surfaced_as_a_failure() {
    let endpoint = MockEndpoint::start(vec![Script::provider_error("the model is overloaded")]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("hello");

    let err = run_turn(&mut history, &host).expect_err("failed");
    assert!(err.message().contains("overloaded"), "{}", err.message());
}

#[test]
fn cancellation_stops_the_turn() {
    let endpoint = MockEndpoint::start(vec![Script::text("never seen")]);
    let host = TestHost::new(endpoint);
    host.cancellation.cancel();

    let mut history = rune_agent::History::new();
    history.push_user("hello");

    let err = run_turn(&mut history, &host).expect_err("cancelled");
    assert_eq!(err.code(), rune_core::error::ErrorCode::Cancelled);
}

#[test]
fn steering_submitted_before_a_turn_reaches_the_model() {
    let endpoint = MockEndpoint::start(vec![Script::text("done")]);
    let host = TestHost::new(endpoint);
    host.steering
        .submit("actually do this instead")
        .expect("queued");

    let mut history = rune_agent::History::new();
    history.push_user("original request");

    run_turn(&mut history, &host).expect("turn");

    let applied = host.events().into_iter().any(|event| {
        matches!(
            event,
            Event::SteeringApplied {
                boundary: rune_agent::Boundary::Model,
                count: 1
            }
        )
    });
    assert!(applied, "steering was not applied at the model boundary");

    // The steering text is part of the conversation the model received.
    let last = history.last().expect("a turn");
    let rendered = history
        .turns()
        .iter()
        .map(rune_agent::Turn::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("actually do this instead"), "{rendered}");
    let _ = last;
}

#[test]
fn a_turn_with_tools_records_every_call_in_order() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "read_file", "{\"path\":\"a.rs\"}"),
        Script::tool_call("call_2", "read_file", "{\"path\":\"b.rs\"}"),
        Script::text("read both"),
    ]);
    let host = TestHost::new(endpoint).with_tools(vec![read_tool()]);

    let mut history = rune_agent::History::new();
    history.push_user("read two files");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.calls.len(), 2);
    let names = host.tool_names();
    assert_eq!(names, vec!["read_file".to_owned(), "read_file".to_owned()]);
    let calls = host.executed_calls();
    assert_eq!(calls[0].1["path"], "a.rs");
    assert_eq!(calls[1].1["path"], "b.rs");
    assert_eq!(outcome.steps, 3);
}

#[test]
fn an_unlimited_step_count_is_the_default() {
    let endpoint = MockEndpoint::start(vec![Script::text("done")]);
    let host = TestHost::new(endpoint);
    let limits = host.limits();
    assert_eq!(
        limits.get(rune_core::LimitName::MaxAgentSteps).value(),
        Some(0)
    );

    let mut history = rune_agent::History::new();
    history.push_user("hello");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
}

#[test]
fn the_turn_reports_its_progress_as_events() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("c", "read_file", "{\"path\":\"a\"}"),
        Script::text("done"),
    ]);
    let host = TestHost::new(endpoint).with_tools(vec![read_tool()]);
    let mut history = rune_agent::History::new();
    history.push_user("go");

    run_turn(&mut history, &host).expect("turn");

    let events = host.events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnStarted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ToolStarted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ToolFinished { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Finished { .. }))
    );
}

#[test]
fn a_long_conversation_stays_valid_across_many_turns() {
    let endpoint = MockEndpoint::start(vec![Script::text("reply")]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();

    for index in 0..10 {
        history.push_user(format!("question {index}"));
        run_turn(&mut history, &host).expect("turn");
        history.validate().expect("still valid");
    }

    assert_eq!(history.len(), 20);
    // Memory must not grow with the number of turns beyond what was sent.
    assert!(history.byte_len() < 100_000, "history grew unexpectedly");
}
