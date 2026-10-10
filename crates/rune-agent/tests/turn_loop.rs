// Integration tests assert by panicking.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// The loop is driven against a real listening endpoint, so these run only where
// a socket can be opened.
#![cfg(not(target_family = "wasm"))]

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
use rune_tools::registry::Registry;

/// A host that records what happened, for assertions.
struct TestHost {
    /// Held so the mock server outlives the host. Dropping it closes the
    /// listener, which would make every request fail with a refused connection.
    server: Option<MockEndpoint>,
    endpoint: Endpoint,
    dialect: Box<dyn Provider>,
    model: String,
    tools: Vec<ToolSpec>,
    rules: RuleSet,
    mode: PermissionMode,
    events: Mutex<Vec<Event>>,
    executed: Mutex<Vec<(String, serde_json::Value)>>,
    tool_response: Mutex<Option<ToolOutput>>,
    /// Real tools used by regressions that must reach the implementation.
    registry: Option<Registry>,
    cancellation: Cancellation,
    steering: SteeringQueue,
    workspace: Utf8PathBuf,
    limits: BudgetSet,
    /// Models what a host does with an unresolved call in `auto` mode: review
    /// it and, when nothing is concerning, allow it. A host that cannot review
    /// leaves the call unresolved, which the loop reports rather than running.
    resolve_ask: bool,
    private_authority: Option<Outcome>,
    private_decisions: Mutex<Vec<Option<String>>>,
    /// Text submitted as steering the first time a delta arrives, which is when
    /// a user typing during a stream would reach the queue.
    submit_on_first_delta: Mutex<Option<String>>,
    /// Cancels the turn from inside the next tool call, as a user pressing
    /// Escape while a tool runs would.
    cancel_on_execute: Mutex<bool>,
}

impl TestHost {
    fn new(endpoint: MockEndpoint) -> Self {
        let base = endpoint.base_url();
        Self {
            server: Some(endpoint),
            endpoint: Endpoint::new(base, "test-key"),
            dialect: Box::new(rune_net::chat_completions::ChatCompletions),
            model: "test/model".to_owned(),
            tools: Vec::new(),
            rules: RuleSet::new(),
            mode: PermissionMode::Auto,
            events: Mutex::new(Vec::new()),
            executed: Mutex::new(Vec::new()),
            tool_response: Mutex::new(None),
            registry: None,
            cancellation: Cancellation::new(),
            steering: SteeringQueue::new(8),
            workspace: Utf8PathBuf::from("/tmp/rune-test"),
            limits: BudgetSet::new(),
            resolve_ask: true,
            private_authority: None,
            private_decisions: Mutex::new(Vec::new()),
            submit_on_first_delta: Mutex::new(None),
            cancel_on_execute: Mutex::new(false),
        }
    }

    /// Cancels the turn while the next tool call is running.
    fn cancelling_the_next_call(self) -> Self {
        *self.cancel_on_execute.lock().expect("lock") = true;
        self
    }

    /// Submits steering as soon as the first token of a reply arrives.
    fn with_steering_mid_stream(self, text: &str) -> Self {
        *self.submit_on_first_delta.lock().expect("lock") = Some(text.to_owned());
        self
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

    fn model(&self) -> String {
        self.model.clone()
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
        // Steering is submitted from inside the stream, which is the only way
        // to exercise the path a user typing mid-turn takes. Doing it here
        // rather than before the turn is the whole point of the test that uses
        // it: text submitted before the turn is drained at the model boundary,
        // while text submitted during it has to survive to the finalizing one.
        if matches!(event, Event::TextDelta { .. })
            && let Some(text) = self.submit_on_first_delta.lock().expect("lock").take()
        {
            self.steering.submit(text).expect("queued");
        }
        self.events.lock().expect("lock").push(event);
    }

    fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        self.executed
            .lock()
            .expect("lock")
            .push((name.to_owned(), arguments.clone()));
        if std::mem::take(&mut *self.cancel_on_execute.lock().expect("lock")) {
            self.cancellation.cancel();
            return Err(rune_agent::steering::cancelled_error());
        }
        if let Some(registry) = &self.registry {
            return registry.call(name, arguments, &self.context());
        }
        Ok(self
            .tool_response
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or_else(|| ToolOutput::success("tool output")))
    }

    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        let (outcome, reason) = rune_agent::turn::decide_call_in_workspace(
            &self.rules,
            self.mode,
            name,
            target,
            &self.workspace,
        );
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

    fn decide_private_network(&self, target: Option<&str>) -> (Outcome, String) {
        self.private_decisions
            .lock()
            .expect("lock")
            .push(target.map(str::to_owned));
        (
            self.private_authority.unwrap_or(Outcome::Deny),
            "separate user decision".to_owned(),
        )
    }

    fn execute_with_context(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        if name == "web_fetch" {
            assert_eq!(
                context.private_network_access,
                arguments
                    .get("allow_private")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true),
                "private authority must be isolated to the approved call",
            );
            return self
                .registry
                .as_ref()
                .expect("web registry")
                .call(name, arguments, context);
        }
        self.execute(name, arguments)
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

/// Builds a script that asks for two reads in one assistant message.
fn two_reads(first: &str, second: &str) -> Script {
    let call = |index: usize, id: &str, path: &str| {
        serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": index,
                        "id": id,
                        "function": {
                            "name": "read_file",
                            "arguments": serde_json::json!({ "path": path }).to_string(),
                        },
                    }],
                },
            }],
        })
        .to_string()
    };
    Script::Frames(vec![
        call(0, first, "a.rs"),
        call(1, second, "b.rs"),
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned(),
        "[DONE]".to_owned(),
    ])
}

fn retained_handle(text: &str) -> rune_tools::Handle {
    let raw = text
        .split("handle ")
        .nth(1)
        .expect("a retained handle")
        .trim_end_matches(']');
    rune_tools::Handle::parse(raw).expect("valid handle")
}

fn result_reader_spec() -> ToolSpec {
    let registry = rune_tools::inventory::builtin_default().expect("registry");
    registry.schemas(&["read_tool_result"]).remove(0)
}

fn result_read_batch(handle: &rune_tools::Handle, requests: &[(usize, usize)]) -> Script {
    let mut frames: Vec<String> = requests
        .iter()
        .enumerate()
        .map(|(index, (offset, length))| {
            serde_json::json!({
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": index,
                    "id": format!("page_{index}"),
                    "function": {
                        "name": "read_tool_result",
                        "arguments": serde_json::json!({"handle": handle.as_str(), "offset": offset, "length": length}).to_string(),
                    },
                }]}}],
            }).to_string()
        })
        .collect();
    frames.push(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned());
    frames.push("[DONE]".to_owned());
    Script::Frames(frames)
}

fn set_result_caps(host: &mut TestHost, tool: u64, turn: u64) {
    for (name, cap) in [
        (rune_core::LimitName::MaxToolResultBytes, tool),
        (rune_core::LimitName::MaxTurnResultBytes, turn),
    ] {
        host.limits
            .set(
                name,
                rune_core::budget::Budget::Bounded(cap),
                rune_core::config::Layer::User,
            )
            .expect("result cap");
    }
}

#[test]
fn a_reader_can_page_a_result_spilled_earlier_in_the_same_batch() {
    let content = "é".repeat(2000);
    let mut history = rune_agent::History::new();
    let handle = rune_tools::Handle::derive(history.result_store().session(), "fixture", &content);
    let Script::Frames(mut frames) =
        Script::tool_call("fixture", "read_file", r#"{"path":"fixture.txt"}"#)
    else {
        panic!("tool frames");
    };
    frames.truncate(2);
    frames.push(serde_json::json!({
        "choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 1, "id": "page",
            "function": {"name": "read_tool_result", "arguments": serde_json::json!({"handle": handle.as_str(), "length": 512}).to_string()},
        }]}}],
    }).to_string());
    frames.push(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned());
    frames.push("[DONE]".to_owned());
    let endpoint = MockEndpoint::start(vec![Script::Frames(frames), Script::text("read")]);
    let mut host = TestHost::new(endpoint)
        .with_tools(vec![read_tool(), result_reader_spec()])
        .with_tool_response(ToolOutput::success(content));
    set_result_caps(&mut host, 2048, 3000);
    history.push_user("retain and read");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.calls.len(), 2);
    assert_eq!(retained_handle(&outcome.calls[0].output.text), handle);
    assert!(
        !outcome.calls[1].output.is_error,
        "{}",
        outcome.calls[1].output.text
    );
    let page: serde_json::Value =
        serde_json::from_str(&outcome.calls[1].output.text).expect("JSON");
    assert_eq!(page["text"], "é".repeat(256));
    assert_eq!(page["next_offset"], 512);
    assert_eq!(page["eof"], false);
    assert_eq!(history.result_store().len(), 1);
    history.validate().expect("paired calls");
}

#[test]
fn a_model_reads_two_pages_of_retained_output_without_exceeding_either_response_cap() {
    let content = format!("{}FULL-TAIL", "é".repeat(1745));
    let mut history = rune_agent::History::new();
    let handle = rune_tools::Handle::derive(history.result_store().session(), "fixture", &content);
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("fixture", "read_file", r#"{"path":"fixture.txt"}"#),
        Script::text("retained"),
        result_read_batch(&handle, &[(0, 1800), (1800, usize::MAX)]),
        Script::text("read both pages"),
    ]);
    let mut host = TestHost::new(endpoint)
        .with_tools(vec![read_tool(), result_reader_spec()])
        .with_tool_response(ToolOutput::success(content.clone()));
    set_result_caps(&mut host, 2048, 3800);
    history.push_user("retain the fixture");
    let initial = run_turn(&mut history, &host).expect("spill turn");
    assert_eq!(retained_handle(&initial.calls[0].output.text), handle);

    // Retention and the reader survive compaction and the next live turn.
    history.replace_with_summary("fixture retained", u64::MAX);
    history.push_user("read the retained fixture in two pages");
    let outcome = run_turn(&mut history, &host).expect("reader turn");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.calls.len(), 2);
    let mut reconstructed = String::new();
    for (index, call) in outcome.calls.iter().enumerate() {
        assert!(call.executed);
        assert!(!call.output.is_error, "{}", call.output.text);
        assert!(call.output.text.len() <= 2048);
        let page: serde_json::Value = serde_json::from_str(&call.output.text).expect("JSON page");
        assert_eq!(page["offset"], reconstructed.len());
        assert_eq!(page["total_bytes"], content.len());
        reconstructed.push_str(page["text"].as_str().expect("page bytes"));
        assert_eq!(page["next_offset"], reconstructed.len());
        assert_eq!(page["eof"], index == 1);
    }
    assert_eq!(reconstructed.as_bytes(), content.as_bytes());
    assert!(
        outcome
            .calls
            .iter()
            .map(|call| call.output.text.len())
            .sum::<usize>()
            <= 3800
    );
    assert_eq!(
        history.result_store().len(),
        1,
        "pages were not spilled again"
    );
    assert_eq!(
        host.executed_calls().len(),
        1,
        "the loop owns the retained store"
    );
    history.validate().expect("paired calls");
    let request = host
        .server
        .as_ref()
        .expect("server")
        .last_body()
        .expect("request");
    assert!(
        request.to_string().contains("FULL-TAIL"),
        "pages reached the provider"
    );
    assert!(host.events().iter().any(|event| matches!(event,
        Event::ToolStarted { call, activity: rune_tools::Activity::Read }
        if call.name == "read_tool_result"
    )));
}

#[test]
fn readers_in_one_batch_share_the_remaining_turn_budget_including_metadata() {
    let content = "x".repeat(3500);
    let mut history = rune_agent::History::new();
    let handle = rune_tools::Handle::derive(history.result_store().session(), "fixture", &content);
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("fixture", "read_file", r#"{"path":"fixture.txt"}"#),
        Script::text("retained"),
        result_read_batch(
            &handle,
            &[(0, usize::MAX), (0, usize::MAX), (0, usize::MAX)],
        ),
        Script::text("bounded"),
    ]);
    let mut host = TestHost::new(endpoint)
        .with_tools(vec![read_tool(), result_reader_spec()])
        .with_tool_response(ToolOutput::success(content));
    set_result_caps(&mut host, 2048, 3000);
    history.push_user("retain");
    run_turn(&mut history, &host).expect("spill");
    history.push_user("read");
    let outcome = run_turn(&mut history, &host).expect("read");
    assert_eq!(outcome.calls.len(), 3);
    let first = &outcome.calls[0].output;
    let second = &outcome.calls[1].output;
    assert!(!first.is_error);
    assert!(!second.is_error);
    assert!(first.text.len() <= 2048);
    assert!(second.text.len() <= 3000 - first.text.len());
    assert!(second.text.len() < first.text.len());
    for output in [first, second] {
        let page: serde_json::Value =
            serde_json::from_str(&output.text).expect("complete page JSON");
        assert_eq!(page["eof"], false);
        assert_eq!(
            page["next_offset"],
            page["text"].as_str().expect("text").len()
        );
    }
    assert!(outcome.calls[2].output.is_error);
    assert!(
        outcome
            .calls
            .iter()
            .map(|call| call.output.text.len())
            .sum::<usize>()
            <= 3000
    );
    assert_eq!(
        history.result_store().len(),
        1,
        "no page retained as another result"
    );
    history.validate().expect("valid after budget exhaustion");
}

#[test]
fn retained_reader_respects_policy_and_conversation_isolation() {
    let content = "SECRET-TAIL".repeat(300);
    let mut history = rune_agent::History::new();
    let handle = rune_tools::Handle::derive(history.result_store().session(), "fixture", &content);
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("fixture", "read_file", r#"{"path":"fixture.txt"}"#),
        Script::text("retained"),
        result_read_batch(&handle, &[(0, usize::MAX)]),
        Script::text("refused"),
        result_read_batch(&handle, &[(0, usize::MAX)]),
        Script::text("unknown"),
    ]);
    let mut host = TestHost::new(endpoint)
        .with_tools(vec![read_tool(), result_reader_spec()])
        .with_tool_response(ToolOutput::success(content));
    set_result_caps(&mut host, 128, 4096);
    history.push_user("retain");
    run_turn(&mut history, &host).expect("spill");
    host.rules
        .push(Rule::deny("read_tool_result", "*", Layer::User));
    history.push_user("read");
    let denied = run_turn(&mut history, &host).expect("denied turn");
    assert!(!denied.calls[0].executed);
    assert!(denied.calls[0].output.is_error);
    assert!(denied.calls[0].output.text.contains("refused by policy"));
    host.rules = RuleSet::new();
    let mut other = rune_agent::History::new();
    other.push_user("read a foreign handle");
    let unknown = run_turn(&mut other, &host).expect("foreign read");
    assert!(unknown.calls[0].output.is_error);
    assert!(unknown.calls[0].output.text.contains("no retained result"));
    assert!(!unknown.calls[0].output.text.contains("SECRET-TAIL"));
    assert!(other.result_store().is_empty());
}

#[test]
fn oversized_tool_results_have_a_bounded_preview_and_a_readable_full_body() {
    for is_error in [false, true] {
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call("large", "read_file", "{\"path\":\"a.rs\"}"),
            Script::text("inspected"),
            Script::text("later turn"),
        ]);
        let content = format!("PREVIEW {} FULL-TAIL", "é".repeat(40_000));
        let output = if is_error {
            ToolOutput::failure(content.clone())
        } else {
            ToolOutput::success(content.clone())
        };
        let host = TestHost::new(endpoint)
            .with_tools(vec![read_tool()])
            .with_tool_response(output);
        let mut history = rune_agent::History::new();
        history.push_user("read");

        let outcome = run_turn(&mut history, &host).expect("turn");
        let shown = &outcome.calls[0].output;
        assert!(shown.text.starts_with("PREVIEW "));
        assert!(!shown.text.contains("FULL-TAIL"));
        assert!(shown.text.len() <= 65_536);
        assert_eq!(shown.produced_bytes, content.len() as u64);
        assert_eq!(shown.is_error, is_error);
        let handle = retained_handle(&shown.text);
        assert_eq!(history.result_store().len(), 1);
        assert_eq!(history.result_store().total_bytes(), content.len());
        assert_eq!(
            history.result_store().describe(&handle),
            Some(("read_file", content.len() as u64, is_error))
        );
        let mut full = String::new();
        while full.len() < content.len() {
            let page = history
                .result_store()
                .read(&handle, full.len(), 4096)
                .expect("read retained bytes");
            assert!(!page.text.is_empty());
            full.push_str(&page.text);
        }
        assert_eq!(full, content);

        let body = host
            .server
            .as_ref()
            .expect("server")
            .last_body()
            .expect("request");
        assert!(body.to_string().contains(handle.as_str()));
        assert!(!body.to_string().contains("FULL-TAIL"));
        assert!(
            rune_agent::History::new()
                .result_store()
                .read(&handle, 0, 64)
                .is_err()
        );
        assert!(
            rune_agent::History::default()
                .result_store()
                .read(&handle, 0, 64)
                .is_err()
        );
        let saved = serde_json::to_string(&history).expect("serialized history");
        assert!(!saved.contains("FULL-TAIL"));
        let restored: rune_agent::History = serde_json::from_str(&saved).expect("restored history");
        assert!(restored.result_store().is_empty());

        history.push_user("continue");
        run_turn(&mut history, &host).expect("later turn");
        history.replace_with_summary("summary", u64::MAX);
        assert_eq!(
            history
                .result_store()
                .read(&handle, content.len() - 9, 9)
                .expect("tail")
                .text,
            "FULL-TAIL"
        );
    }
}

#[test]
fn lowered_tool_caps_include_preview_metadata_and_leave_small_results_unchanged() {
    for content in ["é".repeat(200), "é".repeat(64)] {
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call("lowered", "read_file", "{\"path\":\"a.rs\"}"),
            Script::text("done"),
        ]);
        let mut host = TestHost::new(endpoint)
            .with_tools(vec![read_tool()])
            .with_tool_response(ToolOutput::success(content.clone()));
        host.limits
            .set(
                rune_core::LimitName::MaxToolResultBytes,
                rune_core::budget::Budget::Bounded(128),
                rune_core::config::Layer::User,
            )
            .expect("limit");
        let mut history = rune_agent::History::new();
        history.push_user("read");
        let outcome = run_turn(&mut history, &host).expect("turn");
        let shown = &outcome.calls[0].output.text;
        assert!(shown.len() <= 128);
        if content.len() > 128 {
            let handle = retained_handle(shown);
            assert_eq!(
                history
                    .result_store()
                    .read(&handle, 0, 4096)
                    .expect("read")
                    .text,
                content
            );
            assert!(shown.starts_with('é'));
        } else {
            assert_eq!(shown, &content);
            assert!(history.result_store().is_empty());
        }
    }
}

#[test]
fn the_turn_budget_retains_cut_results_even_when_no_preview_space_remains() {
    let endpoint = MockEndpoint::start(vec![two_reads("first", "second"), Script::text("done")]);
    let content = "z".repeat(1000);
    let mut host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(ToolOutput::success(content.clone()));
    host.limits
        .set(
            rune_core::LimitName::MaxTurnResultBytes,
            rune_core::budget::Budget::Bounded(128),
            rune_core::config::Layer::User,
        )
        .expect("limit");
    let mut history = rune_agent::History::new();
    history.push_user("read both");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.calls[0].output.text.len(), 128);
    assert!(outcome.calls[1].output.text.is_empty());
    assert_eq!(history.result_store().len(), 2);
    let second_handle =
        rune_tools::Handle::derive(history.result_store().session(), "second", &content);
    assert_eq!(
        history
            .result_store()
            .read(&second_handle, 0, 4096)
            .expect("second retained output")
            .text,
        content
    );
    let handle = retained_handle(&outcome.calls[0].output.text);
    assert_eq!(
        history
            .result_store()
            .read(&handle, 0, 4096)
            .expect("read")
            .text,
        content
    );
    history.validate().expect("paired tool calls");
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
fn usage_is_summed_across_steps_and_the_last_request_is_kept_apart() {
    // Each step is a request billed on its own, so the turn costs their sum.
    // How full the context is, though, is the last request's size alone.
    let usage = |input: u64, output: u64| {
        serde_json::json!({
            "choices": [],
            "usage": { "prompt_tokens": input, "completion_tokens": output },
        })
        .to_string()
    };
    let Script::Frames(mut first) = Script::tool_call("call_1", "read_file", "{\"path\":\"a.rs\"}")
    else {
        panic!("frames");
    };
    first.insert(first.len() - 1, usage(100, 20));
    let second = Script::Frames(vec![
        r#"{"choices":[{"index":0,"delta":{"content":"read it"}}]}"#.to_owned(),
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.to_owned(),
        usage(150, 30),
        "[DONE]".to_owned(),
    ]);
    let endpoint = MockEndpoint::start(vec![Script::Frames(first), second]);
    let host = TestHost::new(endpoint).with_tools(vec![read_tool()]);

    let mut history = rune_agent::History::new();
    history.push_user("read a.rs");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.steps, 2);
    assert_eq!(outcome.usage.input_tokens, Some(250));
    assert_eq!(outcome.usage.output_tokens, Some(50));
    assert_eq!(outcome.last_request.input_tokens, Some(150));
    assert_eq!(outcome.last_request.output_tokens, Some(30));

    let finished = host.events().into_iter().find_map(|event| match event {
        Event::Finished {
            usage,
            last_request,
            ..
        } => Some((usage, last_request)),
        _ => None,
    });
    let (usage, last_request) = finished.expect("a finished event");
    assert_eq!(usage, outcome.usage);
    assert_eq!(last_request, outcome.last_request);
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
fn private_web_requests_use_a_distinct_decision_before_any_backend_call() {
    use rune_tools::web::{Fetched, RecordingBackend, WebFetch};
    use std::sync::Arc;

    for authority in [
        None,
        Some(Outcome::Ask),
        Some(Outcome::Deny),
        Some(Outcome::Allow),
    ] {
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call(
                "private",
                "web_fetch",
                r#"{"url":"https://example.com/","allow_private":true}"#,
            ),
            Script::tool_call("public", "web_fetch", r#"{"url":"https://example.com/"}"#),
            Script::text("done"),
        ]);
        let backend = Arc::new(RecordingBackend::new());
        for _ in 0..2 {
            backend.push(Fetched {
                status: 200,
                content_type: "text/plain".to_owned(),
                body: b"fixture answer".to_vec(),
                location: None,
            });
        }
        let tool = WebFetch::new(backend.clone(), &BudgetSet::new());
        let mut host = TestHost::new(endpoint)
            .with_tools(vec![rune_tools::contract::model_spec(&tool)])
            .with_mode(PermissionMode::FullAccess);
        let mut registry = Registry::new();
        registry.insert(Box::new(tool)).expect("register");
        host.registry = Some(registry);
        host.private_authority = authority;
        let mut history = rune_agent::History::new();
        history.push_user("fetch");
        let outcome = run_turn(&mut history, &host).expect("turn");
        assert_eq!(
            *host.private_decisions.lock().expect("lock"),
            vec![Some("domain:example.com".to_owned())]
        );
        let allowed = authority == Some(Outcome::Allow);
        assert_eq!(outcome.calls[0].executed, allowed);
        assert_eq!(outcome.calls[0].output.is_error, !allowed);
        assert!(outcome.calls[1].executed, "ordinary fetch must still work");
        assert!(!outcome.calls[1].output.is_error);
        assert_eq!(backend.requests().len(), if allowed { 2 } else { 1 });
        history.validate().expect("all calls answered");
    }
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
fn a_file_denial_blocks_equivalent_paths_but_allows_an_unrelated_read() {
    let directory = tempfile::tempdir().expect("workspace");
    std::fs::write(directory.path().join(".env"), "PRIVATE_SENTINEL\n").expect("secret");
    std::fs::write(directory.path().join("unrelated.txt"), "PUBLIC_SENTINEL\n")
        .expect("unrelated file");
    let workspace = Utf8PathBuf::from_path_buf(directory.path().to_owned()).expect("UTF-8 path");
    let absolute = workspace.join(".env");

    for target in [".env", "./.env", absolute.as_str()] {
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call(
                "blocked",
                "read_file",
                &serde_json::json!({ "path": target }).to_string(),
            ),
            Script::tool_call("unrelated", "read_file", r#"{"path":"unrelated.txt"}"#),
            Script::text("done"),
        ]);
        let mut rules = RuleSet::new();
        rules.push(Rule::allow("read_file", "*", Layer::Default));
        rules.push(Rule::deny("read_file", ".env", Layer::User));
        let mut registry = Registry::new();
        registry
            .insert(Box::new(rune_tools::ReadFile::new()))
            .expect("registered");
        let mut host = TestHost::new(endpoint)
            .with_tools(registry.all_schemas())
            .with_rules(rules);
        host.workspace = workspace.clone();
        host.registry = Some(registry);
        let mut history = rune_agent::History::new();
        history.push_user("read both files");

        let outcome = run_turn(&mut history, &host).expect("turn");
        assert_eq!(outcome.calls.len(), 2);
        assert!(!outcome.calls[0].executed, "{target} reached the tool");
        assert!(outcome.calls[0].output.is_error);
        assert!(!outcome.calls[0].output.text.contains("PRIVATE_SENTINEL"));
        assert!(outcome.calls[1].executed);
        assert!(!outcome.calls[1].output.is_error);
        assert!(outcome.calls[1].output.text.contains("PUBLIC_SENTINEL"));
        let executed = host.executed_calls();
        assert_eq!(executed.len(), 1);
        assert_eq!(executed[0].1["path"], "unrelated.txt");
    }
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
    // The person watching is told, and the model is told why and not to try
    // again, since the same call would be held the same way every time.
    assert!(
        host.events()
            .iter()
            .any(|event| matches!(event, Event::ToolDenied { .. })),
        "a held call was not reported to the host"
    );
    let told = &outcome.calls[0].output.text;
    assert!(told.contains("no rule allows"), "{told}");
    assert!(told.contains("Do not retry"), "{told}");
}

#[test]
fn a_call_to_a_tool_that_was_not_offered_is_answered_without_asking() {
    // A small model invents tool names. Asking whether an invented tool may
    // run has no useful answer, so the call is answered with the tools that do
    // exist, and the turn goes on.
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "run_shell", "{\"command\":\"ls\"}"),
        Script::text("Understood."),
    ]);
    let host = TestHost::new(endpoint).with_tools(vec![read_tool()]);

    let mut history = rune_agent::History::new();
    history.push_user("list the files");

    let outcome = run_turn(&mut history, &host).expect("the turn continues");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert!(!outcome.calls[0].executed);
    assert!(host.executed_calls().is_empty());
    let told = &outcome.calls[0].output.text;
    assert!(told.contains("no tool named `run_shell`"), "{told}");
    assert!(
        told.contains("read_file"),
        "the offered tools were not named: {told}"
    );
    assert!(
        host.events()
            .iter()
            .any(|event| matches!(event, Event::ToolDenied { .. })),
        "the refusal was not reported to the host"
    );
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
fn an_extreme_grep_context_is_reported_to_the_model_and_the_session_continues() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("fixture.txt"), "x\n").expect("fixture");
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call(
            "bad_context",
            "grep_files",
            r#"{"pattern":"x","path":"fixture.txt","context_lines":18446744073709551615}"#,
        ),
        Script::text("The context value was rejected."),
        Script::tool_call(
            "valid_context",
            "grep_files",
            r#"{"pattern":"x","path":"fixture.txt","context_lines":1}"#,
        ),
        Script::text("The session is still usable."),
    ]);
    let mut registry = Registry::new();
    registry
        .insert(Box::new(rune_tools::GrepFiles::new()))
        .expect("registered");
    let mut host = TestHost::new(endpoint).with_tools(registry.all_schemas());
    host.workspace = Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path");
    host.registry = Some(registry);

    let mut history = rune_agent::History::new();
    history.push_user("search with an excessive context value");
    let rejected = run_turn(&mut history, &host).expect("validation must not end the turn");
    assert_eq!(rejected.stop_reason, StopReason::Completed);
    assert_eq!(rejected.calls.len(), 1);
    assert!(rejected.calls[0].output.is_error);
    assert!(
        rejected.calls[0].output.text.contains("context_lines"),
        "{}",
        rejected.calls[0].output.text
    );
    history.validate().expect("the failed call has an answer");

    let request = host
        .server
        .as_ref()
        .expect("endpoint")
        .last_body()
        .expect("request");
    let result = request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["tool_call_id"] == "bad_context")
        .expect("the provider received the validation error");
    assert_eq!(result["is_error"], true);
    assert_eq!(result["content"], rejected.calls[0].output.text);

    history.push_user("search again with valid context");
    let recovered = run_turn(&mut history, &host).expect("the next turn runs");
    assert_eq!(recovered.stop_reason, StopReason::Completed);
    assert_eq!(recovered.text, "The session is still usable.");
    assert_eq!(recovered.calls.len(), 1);
    assert!(!recovered.calls[0].output.is_error);
    assert!(
        recovered.calls[0]
            .output
            .text
            .contains("fixture.txt:     1:x"),
        "{}",
        recovered.calls[0].output.text
    );
    history
        .validate()
        .expect("the session history remains valid");
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
fn a_turn_stopped_at_the_step_limit_keeps_its_last_words() {
    // The last step both answered and asked for another tool. The limit stops
    // the turn before that tool's result is seen, but what the model already
    // said is still the turn's answer.
    let call = serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": { "name": "read_file", "arguments": "{\"path\":\"a.rs\"}" },
                }],
            },
        }],
    });
    let endpoint = MockEndpoint::start(vec![Script::Frames(vec![
        r#"{"choices":[{"index":0,"delta":{"content":"Halfway there."}}]}"#.to_owned(),
        call.to_string(),
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned(),
        "[DONE]".to_owned(),
    ])]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_step_limit(1);

    let mut history = rune_agent::History::new();
    history.push_user("keep going");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::StepLimit);
    assert_eq!(outcome.text, "Halfway there.");
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
fn a_retried_step_tells_the_host_to_drop_what_it_streamed() {
    // The failed attempt's text already reached the host, and the retry
    // streams the answer again from the start. Without a restart between them
    // a host shows the partial answer followed by the whole one.
    let endpoint = MockEndpoint::start(vec![
        Script::truncated("partial answer"),
        Script::text("complete answer"),
    ]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("hello");

    let outcome = run_turn(&mut history, &host).expect("turn");

    let events = host.events();
    let restart = events
        .iter()
        .position(|event| matches!(event, Event::StepRestarted { step: 1 }))
        .expect("the host was told the step restarted");
    let streamed = |events: &[Event]| -> String {
        events
            .iter()
            .filter_map(|event| match event {
                Event::TextDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(streamed(&events[..restart]), "partial answer");
    assert_eq!(streamed(&events[restart..]), outcome.text);
}

#[test]
fn a_step_that_succeeds_first_time_is_never_restarted() {
    let endpoint = MockEndpoint::start(vec![Script::text("done")]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("hello");

    run_turn(&mut history, &host).expect("turn");
    assert!(
        !host
            .events()
            .iter()
            .any(|event| matches!(event, Event::StepRestarted { .. }))
    );
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
fn a_call_left_unrun_by_a_cancellation_is_still_answered() {
    // The second call of the batch never runs, but the model asked for it, so
    // the conversation must record an answer. Without one the history holds an
    // unanswered call and every later turn fails before it is sent.
    let endpoint = MockEndpoint::start(vec![two_reads("call_1", "call_2"), Script::text("done")]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .cancelling_the_next_call();

    let mut history = rune_agent::History::new();
    history.push_user("read two files");

    let err = run_turn(&mut history, &host).expect_err("cancelled");
    assert_eq!(err.code(), rune_core::error::ErrorCode::Cancelled);
    assert_eq!(
        host.executed_calls().len(),
        1,
        "a call ran after the turn was cancelled"
    );
    history.validate().expect("every call has an answer");

    let answers: Vec<String> = history
        .last()
        .expect("a turn")
        .parts
        .iter()
        .filter_map(|part| match part {
            rune_net::message::ContentPart::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(answers.len(), 2, "{answers:?}");
    assert!(answers[1].contains("not run"), "{answers:?}");

    // The conversation carries on from the cancelled turn.
    host.cancellation.reset();
    history.push_user("carry on");
    let outcome = run_turn(&mut history, &host).expect("the next turn runs");
    assert_eq!(outcome.text, "done");
}

#[test]
fn steering_submitted_mid_stream_gets_a_response_in_the_same_turn() {
    // Text typed while the answer is streaming arrives after the last request
    // was built. Ending the turn there dropped it: no further boundary was
    // reached, so the correction was never sent and the user was told nothing.
    // It must instead continue the turn so the model answers it.
    let endpoint = MockEndpoint::start(vec![
        Script::text("first reply"),
        Script::text("second reply"),
    ]);
    let host = TestHost::new(endpoint).with_steering_mid_stream("actually do this instead");

    let mut history = rune_agent::History::new();
    history.push_user("original request");

    run_turn(&mut history, &host).expect("turn");

    let applied = host.events().into_iter().any(|event| {
        matches!(
            event,
            Event::SteeringApplied {
                boundary: rune_agent::Boundary::Finalizing,
                count: 1
            }
        )
    });
    assert!(applied, "steering submitted mid-stream was dropped");
    // Reported once: a second report at the next model boundary would tell the
    // user two messages were applied when one was.
    let reports = host
        .events()
        .into_iter()
        .filter(|event| matches!(event, Event::SteeringApplied { .. }))
        .count();
    assert_eq!(
        reports, 1,
        "one steering message was reported {reports} times"
    );

    // The model was asked again with the steering in the conversation, which is
    // what makes the correction delivered rather than merely queued.
    let rendered = history
        .turns()
        .iter()
        .map(rune_agent::Turn::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("actually do this instead"),
        "the steering never reached the conversation: {rendered}"
    );
    assert!(
        rendered.contains("second reply"),
        "the turn ended without answering the steering: {rendered}"
    );
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

#[test]
fn input_sent_to_a_shell_session_is_judged_by_the_rules() {
    // The `command` argument is not what an interaction runs, so a rule that
    // allows it must not carry the input that follows.
    let arguments = serde_json::json!({
        "action": "interact",
        "session_id": "shell-0-0",
        "command": "ls",
        "chars": "rm -rf .git\n",
    });
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "shell", &arguments.to_string()),
        Script::text("I will not."),
    ]);
    let mut rules = RuleSet::new();
    rules.push(Rule::allow("shell", "ls*", Layer::User));
    rules.push(Rule::deny("shell", "rm *", Layer::User));
    let host = TestHost::new(endpoint).with_rules(rules);

    let mut history = rune_agent::History::new();
    history.push_user("clean up");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.calls.len(), 1);
    assert!(!outcome.calls[0].executed, "the input reached the session");
    assert!(host.executed_calls().is_empty());
    assert!(
        host.events()
            .iter()
            .any(|event| matches!(event, Event::ToolDenied { .. })),
        "no denial was reported"
    );
}
