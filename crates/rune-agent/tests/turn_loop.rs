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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{Event, Host, StopReason, run_turn};
use rune_core::budget::BudgetSet;
use rune_core::config::{Effort, PermissionMode};
use rune_core::error::{Result, RuneError};
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
    /// Counts outbound client calls in the offline regression.
    offline_fetch_calls: Option<Arc<AtomicUsize>>,
    dialect: Box<dyn Provider>,
    model: String,
    tools: Vec<ToolSpec>,
    rules: RuleSet,
    mode: PermissionMode,
    events: Mutex<Vec<Event>>,
    executed: Mutex<Vec<(String, serde_json::Value)>>,
    decisions: Mutex<Vec<(String, Option<String>)>>,
    tool_response: Mutex<Option<ToolOutput>>,
    tool_error: Option<RuneError>,
    hook_veto: Option<rune_agent::turn::HookVeto>,
    hook_calls: Mutex<Vec<String>>,
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
            offline_fetch_calls: None,
            dialect: Box::new(rune_net::chat_completions::ChatCompletions),
            model: "test/model".to_owned(),
            tools: Vec::new(),
            rules: RuleSet::new(),
            mode: PermissionMode::Auto,
            events: Mutex::new(Vec::new()),
            executed: Mutex::new(Vec::new()),
            decisions: Mutex::new(Vec::new()),
            tool_response: Mutex::new(None),
            tool_error: None,
            hook_veto: None,
            hook_calls: Mutex::new(Vec::new()),
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

    fn tool_activity(&self, name: &str) -> Option<rune_tools::Activity> {
        self.registry.as_ref()?.activity(name)
    }

    fn tool_permission_target(&self, name: &str, arguments: &serde_json::Value) -> Option<String> {
        match &self.registry {
            Some(registry) => rune_agent::turn::permission_target_for(registry, name, arguments),
            None => rune_agent::turn::permission_target_for(
                &rune_tools::inventory::builtin_default().expect("built-in registry"),
                name,
                arguments,
            ),
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn concurrent_tool_host(&self, name: &str) -> Option<&(dyn Host + Sync)> {
        self.registry
            .as_ref()?
            .get(name)?
            .is_read_only()
            .then_some(self)
    }

    fn effort(&self) -> Effort {
        Effort::Auto
    }

    fn fetch(&self) -> Box<dyn rune_net::fetch::Fetch> {
        if let Some(calls) = &self.offline_fetch_calls {
            Box::new(CountingOfflineFetch(Arc::clone(calls)))
        } else {
            Box::new(rune_net::transport::UreqFetch::new())
        }
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

    fn pre_tool(
        &self,
        call: &rune_agent::turn::PreparedCall,
    ) -> Result<Option<rune_agent::turn::HookVeto>> {
        self.hook_calls
            .lock()
            .expect("hook calls")
            .push(call.id.clone());
        Ok(self.hook_veto.clone())
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
        if let Some(error) = &self.tool_error {
            return Err(error.clone());
        }
        Ok(self
            .tool_response
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or_else(|| ToolOutput::success("tool output")))
    }

    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        self.decisions
            .lock()
            .expect("lock")
            .push((name.to_owned(), target.map(str::to_owned)));
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
        if name == "update_plan" {
            return self
                .registry
                .as_ref()
                .expect("plan registry")
                .call(name, arguments, context);
        }
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

/// Records attempts at the Fetch seam without sending anything to a socket.
struct CountingOfflineFetch(Arc<AtomicUsize>);

impl rune_net::fetch::Fetch for CountingOfflineFetch {
    fn send(
        &self,
        _request: rune_net::fetch::FetchRequest,
    ) -> rune_net::error::NetResult<rune_net::fetch::FetchResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(rune_net::error::NetError::new(
            rune_net::error::FailureKind::Network,
            "offline turn must not call Fetch::send",
        ))
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
fn the_request_honors_the_parallel_tool_call_limit() {
    for limit in [None, Some(1), Some(2)] {
        let endpoint = MockEndpoint::start(vec![Script::text("reply")]);
        let mut host = TestHost::new(endpoint).with_tools(vec![read_tool()]);
        if let Some(limit) = limit {
            host.limits
                .set(
                    rune_core::LimitName::ParallelToolCalls,
                    rune_core::budget::Budget::Bounded(limit),
                    rune_core::config::Layer::User,
                )
                .expect("set parallel_tool_calls");
        }
        let mut history = rune_agent::History::new();
        history.push_user("hello");

        let outcome = run_turn(&mut history, &host).expect("turn");
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        let server = host.server.as_ref().expect("server");
        assert_eq!(server.request_count(), 1);
        let body = server.last_body().expect("request body");
        assert_eq!(body["parallel_tool_calls"], limit != Some(1));
    }
}

/// A bounded barrier for each fixture call. The test opens barriers explicitly,
/// so overlap is observed while calls are pending, without timing tool work.
#[derive(Default)]
struct BatchBarriers {
    state: Mutex<BatchBarrierState>,
    changed: std::sync::Condvar,
}

#[derive(Default)]
struct BatchBarrierState {
    started: Vec<String>,
    finished: Vec<String>,
    released: std::collections::BTreeSet<String>,
    reads: usize,
    mutations: usize,
    peak_reads: usize,
    overlap: bool,
}

impl BatchBarriers {
    fn wait_for(&self, keys: &[&str], finished: bool) {
        let state = self.state.lock().expect("barrier lock");
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(5), |state| {
                let observed = if finished {
                    &state.finished
                } else {
                    &state.started
                };
                keys.iter()
                    .any(|key| !observed.iter().any(|name| name == key))
            })
            .expect("barrier wait");
        assert!(!timeout.timed_out(), "pending calls: {:?}", state.started);
    }

    fn release(&self, key: &str) {
        self.state
            .lock()
            .expect("barrier lock")
            .released
            .insert(key.to_owned());
        self.changed.notify_all();
    }

    fn assert_started(&self, keys: &[&str]) {
        let state = self.state.lock().expect("barrier lock");
        assert_eq!(state.started.len(), keys.len(), "{:?}", state.started);
        assert!(
            keys.iter()
                .all(|key| state.started.iter().any(|name| name == key))
        );
    }
}

struct BarrierTool {
    name: &'static str,
    read_only: bool,
    barriers: Arc<BatchBarriers>,
    cancellation: Option<Cancellation>,
}

impl rune_tools::Tool for BarrierTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "Wait at a fixture barrier."
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]})
    }
    fn activity(&self) -> rune_tools::Activity {
        if self.name == "fixture_mutation" {
            rune_tools::Activity::Write
        } else {
            rune_tools::Activity::Read
        }
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
    fn call(
        &self,
        arguments: &serde_json::Value,
        _context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        let key = arguments["key"].as_str().expect("fixture key");
        let mutation = !self.read_only;
        let mut state = self.barriers.state.lock().expect("barrier lock");
        if mutation {
            state.overlap |= state.reads > 0 || state.mutations > 0;
            state.mutations = state.mutations.checked_add(1).expect("active calls");
        } else {
            state.overlap |= state.mutations > 0;
            state.reads = state.reads.checked_add(1).expect("active calls");
            state.peak_reads = state.peak_reads.max(state.reads);
        }
        state.started.push(key.to_owned());
        self.barriers.changed.notify_all();
        let (mut state, timeout) = self
            .barriers
            .changed
            .wait_timeout_while(state, Duration::from_secs(5), |state| {
                !state.released.contains(key)
            })
            .expect("fixture barrier");
        if mutation {
            state.mutations = state.mutations.checked_sub(1).expect("active calls");
        } else {
            state.reads = state.reads.checked_sub(1).expect("active calls");
        }
        state.finished.push(key.to_owned());
        self.barriers.changed.notify_all();
        if timeout.timed_out() {
            return Err(RuneError::new(
                rune_core::ErrorCode::Timeout,
                "fixture barrier was not released",
            ));
        }
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
            return Err(rune_agent::steering::cancelled_error());
        }
        Ok(ToolOutput::success(key.repeat(4096)))
    }
}

fn barrier_host(calls: &[(&str, &str)], cap: u64) -> (TestHost, Arc<BatchBarriers>) {
    let mut frames: Vec<_> = calls
        .iter()
        .enumerate()
        .map(|(index, (name, key))| {
            serde_json::json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": index, "id": key, "function": {"name": name,
                "arguments": serde_json::json!({"key": key}).to_string()}
            }]}}]})
            .to_string()
        })
        .collect();
    frames.push(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned());
    frames.push("[DONE]".to_owned());
    let endpoint = MockEndpoint::start(vec![Script::Frames(frames), Script::text("done")]);
    let barriers = Arc::new(BatchBarriers::default());
    let mut registry = Registry::new();
    for (name, read_only) in [
        ("fixture_read", true),
        ("fixture_mutation", false),
        ("fixture_unsafe_read", false),
    ] {
        registry
            .insert(Box::new(BarrierTool {
                name,
                read_only,
                barriers: Arc::clone(&barriers),
                cancellation: None,
            }))
            .expect("fixture registration");
    }
    let mut host = TestHost::new(endpoint).with_tools(registry.all_schemas());
    host.registry = Some(registry);
    host.limits
        .set(
            rune_core::LimitName::ParallelToolCalls,
            rune_core::budget::Budget::Bounded(cap),
            rune_core::config::Layer::User,
        )
        .expect("concurrency cap");
    (host, barriers)
}

#[test]
fn reads_overlap_at_cap_two_and_drain_before_a_pending_mutation() {
    let calls = [
        ("fixture_read", "a"),
        ("fixture_read", "b"),
        ("fixture_read", "c"),
        ("fixture_mutation", "mutation"),
        ("fixture_read", "d"),
        ("fixture_read", "e"),
    ];
    let (mut host, barriers) = barrier_host(&calls, 2);
    set_result_caps(&mut host, 2048, 3000);
    let mut history = rune_agent::History::new();
    history.push_user("read, mutate, then read");
    let outcome = std::thread::scope(|scope| {
        let worker = scope.spawn(|| run_turn(&mut history, &host));
        barriers.wait_for(&["a", "b"], false);
        barriers.assert_started(&["a", "b"]);
        // Finish the second call first. Results must still be retained in
        // request order, and the mutation must wait for every earlier read.
        barriers.release("b");
        barriers.wait_for(&["b"], true);
        barriers.release("a");
        barriers.wait_for(&["a"], true);
        barriers.wait_for(&["c"], false);
        barriers.assert_started(&["a", "b", "c"]);
        barriers.release("c");
        barriers.wait_for(&["mutation"], false);
        barriers.assert_started(&["a", "b", "c", "mutation"]);
        barriers.release("mutation");
        barriers.wait_for(&["d"], false);
        barriers.assert_started(&["a", "b", "c", "mutation", "d"]);
        barriers.release("d");
        barriers.wait_for(&["e"], false);
        barriers.release("e");
        worker.join().expect("turn worker").expect("turn")
    });
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(
        outcome
            .calls
            .iter()
            .map(|result| result.call.id.as_str())
            .collect::<Vec<_>>(),
        calls.iter().map(|(_, key)| *key).collect::<Vec<_>>()
    );
    assert!(
        outcome
            .calls
            .iter()
            .all(|result| result.executed && !result.output.is_error)
    );
    assert!(
        outcome
            .calls
            .iter()
            .all(|result| result.output.text.len() <= 2048)
    );
    assert!(
        outcome
            .calls
            .iter()
            .map(|result| result.output.text.len())
            .sum::<usize>()
            <= 3000
    );
    let first_handle = retained_handle(&outcome.calls[0].output.text);
    let second_handle = retained_handle(&outcome.calls[1].output.text);
    assert_ne!(first_handle, second_handle);
    let state = barriers.state.lock().expect("barrier lock");
    assert_eq!(state.peak_reads, 2);
    assert!(!state.overlap, "a read overlapped a mutation");
    assert_eq!(state.finished[..2], ["b", "a"]);
    history.validate().expect("paired history");
}

#[test]
fn one_call_mode_and_a_tool_overriding_read_only_keep_calls_sequential() {
    for (cap, first) in [(1, "fixture_read"), (2, "fixture_unsafe_read")] {
        let calls = [(first, "a"), ("fixture_read", "b"), ("fixture_read", "c")];
        let (host, barriers) = barrier_host(&calls, cap);
        let mut history = rune_agent::History::new();
        history.push_user("read in order");
        let outcome = std::thread::scope(|scope| {
            let worker = scope.spawn(|| run_turn(&mut history, &host));
            for (index, (_, key)) in calls.iter().enumerate() {
                barriers.wait_for(&[key], false);
                barriers.assert_started(
                    &calls[..=index]
                        .iter()
                        .map(|(_, key)| *key)
                        .collect::<Vec<_>>(),
                );
                barriers.release(key);
            }
            worker.join().expect("turn worker").expect("turn")
        });
        assert!(
            outcome
                .calls
                .iter()
                .all(|result| result.executed && !result.output.is_error)
        );
        let state = barriers.state.lock().expect("barrier lock");
        assert_eq!(state.peak_reads, 1);
        assert!(!state.overlap);
        assert_eq!(state.started, state.finished);
        history.validate().expect("paired history");
    }
}

#[test]
fn cancelling_concurrent_reads_answers_the_remaining_calls_without_running_them() {
    let calls = [
        ("fixture_read", "a"),
        ("fixture_read", "b"),
        ("fixture_read", "c"),
        ("fixture_mutation", "mutation"),
    ];
    let (mut host, barriers) = barrier_host(&calls, 2);
    let mut registry = Registry::new();
    registry
        .insert(Box::new(BarrierTool {
            name: "fixture_read",
            read_only: true,
            barriers: Arc::clone(&barriers),
            cancellation: Some(host.cancellation.clone()),
        }))
        .expect("fixture registration");
    registry
        .insert(Box::new(BarrierTool {
            name: "fixture_mutation",
            read_only: false,
            barriers: Arc::clone(&barriers),
            cancellation: None,
        }))
        .expect("fixture registration");
    host.registry = Some(registry);
    let mut history = rune_agent::History::new();
    history.push_user("cancel pending reads");
    let error = std::thread::scope(|scope| {
        let worker = scope.spawn(|| run_turn(&mut history, &host));
        barriers.wait_for(&["a", "b"], false);
        barriers.release("a");
        barriers.wait_for(&["a"], true);
        barriers.release("b");
        worker
            .join()
            .expect("turn worker")
            .expect_err("cancelled turn")
    });
    assert_eq!(error.code(), rune_core::ErrorCode::Cancelled);
    barriers.assert_started(&["a", "b"]);
    let answers = history
        .last()
        .expect("results")
        .parts
        .iter()
        .filter_map(|part| match part {
            rune_net::message::ContentPart::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(answers.len(), 4);
    assert!(answers[2].contains("not run"));
    assert!(answers[3].contains("not run"));
    history.validate().expect("cancelled calls are paired");
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
fn registered_permission_targets_reach_policy_and_explicit_denials_prevent_execution() {
    use rune_tools::{Activity, Tool};

    struct TargetTool {
        name: &'static str,
        activity: Activity,
        has_target: bool,
    }

    impl Tool for TargetTool {
        fn name(&self) -> &'static str {
            self.name
        }

        fn description(&self) -> &'static str {
            "Return fixture content for a declared target."
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn activity(&self) -> Activity {
            self.activity
        }

        fn permission_target(&self, arguments: &serde_json::Value) -> Option<String> {
            self.has_target
                .then(|| arguments["target"].as_str().expect("target").to_owned())
        }

        fn call(
            &self,
            _arguments: &serde_json::Value,
            _context: &ExecutionContext,
        ) -> Result<ToolOutput> {
            Ok(ToolOutput::success("fixture content"))
        }
    }

    // Cover both concurrent reads and sequential writes, replacement of a
    // built-in name, and an authoritative declaration of no target.
    for (name, activity, has_target) in [
        ("custom_read", Activity::Read, true),
        ("custom_write", Activity::Write, true),
        ("read_file", Activity::Read, true),
        ("read_file", Activity::Read, false),
    ] {
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call(
                "call_fixture",
                name,
                r#"{"target":"secret.txt","path":"public.txt"}"#,
            ),
            Script::text("Finished."),
        ]);
        let mut registry = Registry::new();
        registry
            .insert(Box::new(TargetTool {
                name,
                activity,
                has_target,
            }))
            .expect("register");
        let mut rules = RuleSet::new();
        rules.push(Rule::deny(name, "secret.txt", Layer::User));
        let mut host = TestHost::new(endpoint)
            .with_tools(registry.all_schemas())
            .with_rules(rules);
        host.registry = Some(registry);
        let mut history = rune_agent::History::new();
        history.push_user("read the fixture");

        let outcome = run_turn(&mut history, &host).expect("turn");
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(outcome.calls.len(), 1);
        assert_eq!(
            *host.decisions.lock().expect("lock"),
            vec![(name.to_owned(), has_target.then(|| "secret.txt".to_owned()))],
            "policy must see the registered tool's exact target declaration",
        );
        assert_eq!(outcome.calls[0].executed, !has_target);
        assert_eq!(outcome.calls[0].output.is_error, has_target);
        assert_eq!(host.executed_calls().len(), usize::from(!has_target));
        let denied: Vec<_> = host
            .events()
            .into_iter()
            .filter_map(|event| match event {
                Event::ToolDenied { reason, .. } => Some(reason),
                _ => None,
            })
            .collect();
        if has_target {
            assert_eq!(denied.len(), 1);
            assert!(denied[0].contains("secret.txt"), "{}", denied[0]);
            assert!(outcome.calls[0].output.text.contains("refused by policy"));
        } else {
            assert!(denied.is_empty());
        }
    }
}

#[test]
fn registered_tool_activity_overrides_name_inference_in_events() {
    use rune_tools::{Activity, Tool};

    struct FixtureTool {
        name: &'static str,
        activity: Activity,
    }

    impl Tool for FixtureTool {
        fn name(&self) -> &'static str {
            self.name
        }

        fn description(&self) -> &'static str {
            "Return fixture content."
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn activity(&self) -> Activity {
            self.activity
        }

        fn call(
            &self,
            _arguments: &serde_json::Value,
            _context: &ExecutionContext,
        ) -> Result<ToolOutput> {
            Ok(ToolOutput::success("fixture content"))
        }
    }

    for (name, declared, fallback) in [
        ("custom_read", Activity::Read, Activity::Execute),
        ("shell", Activity::Read, Activity::Execute),
        ("read_file", Activity::Search, Activity::Read),
    ] {
        for through_registry in [false, true] {
            let endpoint = MockEndpoint::start(vec![
                Script::tool_call("call_fixture", name, "{}"),
                Script::text("Finished reading."),
            ]);
            let mut registry = Registry::new();
            registry
                .insert(Box::new(FixtureTool {
                    name,
                    activity: declared,
                }))
                .expect("register");
            let mut host = TestHost::new(endpoint).with_tools(registry.all_schemas());
            if through_registry {
                host.registry = Some(registry);
            }
            let mut history = rune_agent::History::new();
            history.push_user("read the fixture");

            let outcome = run_turn(&mut history, &host).expect("turn");
            assert_eq!(outcome.stop_reason, StopReason::Completed);
            assert_eq!(outcome.calls.len(), 1);
            assert!(outcome.calls[0].executed);
            assert!(!outcome.calls[0].output.is_error);
            let activities: Vec<_> = host
                .events()
                .iter()
                .filter_map(|event| match event {
                    Event::ToolStarted { call, activity } => {
                        assert_eq!(call.name, name);
                        Some(*activity)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                activities,
                vec![if through_registry { declared } else { fallback }],
                "{name}, through_registry={through_registry}"
            );
        }
    }
}

#[test]
fn tool_failure_codes_and_hints_reach_the_provider_through_both_execution_paths() {
    use rune_core::error::ErrorCode;
    use rune_tools::{Activity, Tool};

    struct HintTool;

    impl Tool for HintTool {
        fn name(&self) -> &'static str {
            "hint"
        }

        fn description(&self) -> &'static str {
            "Return a missing file diagnostic."
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn activity(&self) -> Activity {
            Activity::Read
        }

        fn call(
            &self,
            _arguments: &serde_json::Value,
            _context: &ExecutionContext,
        ) -> Result<ToolOutput> {
            Err(RuneError::new(ErrorCode::NotFound, "missing").with_hint("try other.txt"))
        }
    }

    for through_registry in [false, true] {
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call("call_hint", "hint", "{}"),
            Script::text("I can try the suggested file."),
        ]);
        let mut registry = Registry::new();
        registry.insert(Box::new(HintTool)).expect("register");
        let mut host = TestHost::new(endpoint).with_tools(registry.all_schemas());
        if through_registry {
            host.registry = Some(registry);
        } else {
            host.tool_error =
                Some(RuneError::new(ErrorCode::NotFound, "missing").with_hint("try other.txt"));
        }
        let mut history = rune_agent::History::new();
        history.push_user("read missing.txt");
        let outcome = run_turn(&mut history, &host).expect("the turn continues");
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(outcome.calls.len(), 1);
        assert!(outcome.calls[0].executed);
        assert!(outcome.calls[0].output.is_error);
        history.validate().expect("the failure has an answer");

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
            .find(|message| message["tool_call_id"] == "call_hint")
            .expect("the provider received the failure");
        assert_eq!(result["is_error"], true);
        assert_eq!(result["content"], outcome.calls[0].output.text);
        let diagnostic: serde_json::Value =
            serde_json::from_str(result["content"].as_str().expect("result text"))
                .expect("diagnostic JSON");
        assert_eq!(diagnostic["code"], "not_found");
        assert_eq!(diagnostic["message"], "missing");
        assert_eq!(diagnostic["detail"]["hint"], "try other.txt");
    }
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
fn an_offline_turn_refuses_without_fetch_calls_or_retry_delay() {
    let mut host = TestHost::new(MockEndpoint::start(vec![Script::text("unused")]));
    host.endpoint.offline = true;
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    host.offline_fetch_calls = Some(Arc::clone(&fetch_calls));
    host.limits
        .set(
            rune_core::LimitName::ProviderMaxAttempts,
            rune_core::budget::Budget::Bounded(2),
            rune_core::config::Layer::User,
        )
        .expect("limit");
    let mut history = rune_agent::History::new();
    history.push_user("hello");

    let started = Instant::now();
    let error = run_turn(&mut history, &host).expect_err("offline refusal");
    let elapsed = started.elapsed();
    assert_eq!(fetch_calls.load(Ordering::SeqCst), 0);
    assert_eq!(host.server.as_ref().expect("server").request_count(), 0);
    assert!(
        !host.events().iter().any(|event| matches!(
            event,
            Event::ProviderRetry { .. }
                | Event::StepRestarted { .. }
                | Event::ProviderRetryFinished { .. }
        )),
        "offline refusal must not schedule a retry"
    );
    assert!(elapsed < Duration::from_millis(250), "elapsed: {elapsed:?}");
    assert_eq!(error.code(), rune_core::ErrorCode::PermissionDenied);
    assert_eq!(error.message(), "outbound requests are disabled");
    assert_eq!(
        error.detail().hint.as_deref(),
        Some("remove the offline setting to reach a provider")
    );
    history.validate().expect("valid history after refusal");
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
fn two_failures_report_the_next_attempt_and_backoff_before_each_restart() {
    let endpoint = MockEndpoint::start(vec![Script::text("recovered")]);
    endpoint.fail_first(2);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("hello");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.text, "recovered");
    assert_eq!(host.server.as_ref().expect("server").request_count(), 3);
    let retries: Vec<_> = host
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::ProviderRetry {
                step,
                next_attempt,
                max_attempts,
                delay,
            } => Some((step, next_attempt, max_attempts, delay.as_millis())),
            Event::StepRestarted { step } => Some((step, 0, 0, 0)),
            Event::ProviderRetryFinished { step } => Some((step, 0, 0, 1)),
            _ => None,
        })
        .collect();
    assert_eq!(
        retries,
        [
            (1, 2, 10, 250),
            (1, 0, 0, 0),
            (1, 3, 10, 500),
            (1, 0, 0, 0),
            (1, 0, 0, 1),
        ]
    );
}

#[test]
fn an_exhausted_budget_never_announces_a_pending_retry() {
    let endpoint = MockEndpoint::start(vec![Script::Status {
        code: 503,
        body: "unavailable".to_owned(),
    }]);
    let mut host = TestHost::new(endpoint);
    host.limits
        .set(
            rune_core::budget::LimitName::ProviderMaxAttempts,
            rune_core::budget::Budget::Bounded(1),
            rune_core::config::Layer::User,
        )
        .expect("limit");
    let mut history = rune_agent::History::new();
    history.push_user("hello");
    run_turn(&mut history, &host).expect_err("exhausted");
    assert!(
        !host
            .events()
            .iter()
            .any(|event| matches!(event, Event::ProviderRetry { .. }))
    );
    assert_eq!(host.server.as_ref().expect("server").request_count(), 1);
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
fn steering_during_a_failed_stream_reaches_the_retry_request() {
    let endpoint = MockEndpoint::start(vec![
        Script::truncated("partial answer"),
        Script::text("corrected answer"),
    ]);
    let host = TestHost::new(endpoint)
        .with_steering_mid_stream("actually do this instead")
        .with_step_limit(1);
    let mut history = rune_agent::History::new();
    history.push_user("original request");

    let outcome = run_turn(&mut history, &host).expect("turn");
    let server = host.server.as_ref().expect("server");
    assert_eq!(server.request_count(), 2);
    let body = server.last_body().expect("retry request");
    assert_eq!(
        body["messages"],
        serde_json::json!([
            { "role": "system", "content": "You are a test assistant." },
            { "role": "user", "content": "original request" },
            {
                "role": "user",
                "content": "<user_steering>\nactually do this instead\n</user_steering>\n"
            },
        ]),
        "the retry must include the correction without the failed attempt's partial answer"
    );
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.steps, 1);
    assert_eq!(outcome.text, "corrected answer");
    assert!(host.steering.is_empty());
    let applied: Vec<_> = host
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::SteeringApplied { boundary, count } => Some((boundary, count)),
            _ => None,
        })
        .collect();
    assert_eq!(applied, [(rune_agent::Boundary::AfterFailure, 1)]);
    history.validate().expect("valid history after retry");
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
fn a_file_rename_reports_one_mutation_and_refuses_an_outside_destination() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(workspace.path().join("before.bin"), [0, 255, 10]).expect("fixture");
    let outside_path = outside.path().join("escaped.bin");
    let escape = serde_json::json!({"path": "after.bin", "new_path": outside_path}).to_string();
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call(
            "rename",
            "rename_file",
            r#"{"path":"before.bin","new_path":"after.bin"}"#,
        ),
        Script::tool_call("escape", "rename_file", &escape),
        Script::text("done"),
    ]);
    let registry = rune_tools::inventory::builtin_default().expect("builtins");
    let mut rules = RuleSet::new();
    rules.push(Rule::allow("rename_file", "*", Layer::User));
    let mut host = TestHost::new(endpoint)
        .with_tools(registry.all_schemas())
        .with_rules(rules);
    host.workspace = Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path");
    host.registry = Some(registry);
    let mut history = rune_agent::History::new();
    history.push_user("rename the fixture");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.calls.len(), 2);
    assert!(!outcome.calls[0].output.is_error);
    assert!(outcome.calls[1].output.is_error);
    assert!(
        outcome.calls[1]
            .output
            .text
            .contains("path_outside_workspace")
    );
    assert!(!workspace.path().join("before.bin").exists());
    assert_eq!(
        std::fs::read(workspace.path().join("after.bin")).expect("read"),
        [0, 255, 10]
    );
    assert!(!outside_path.exists());
    let events = host.events();
    assert_eq!(events.iter().filter(|event| matches!(event,
        Event::ToolStarted { call, activity: rune_tools::Activity::Write } if call.id == "rename"
    )).count(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ToolFinished {
                    is_error: false,
                    ..
                }
            ))
            .count(),
        1
    );
    history.validate().expect("paired results");
}

#[test]
fn a_file_rename_requires_policy_to_allow_both_normalized_paths() {
    for (path, new_path, denied) in [
        ("./source", "destination", "source"),
        ("source", "./destination", "destination"),
    ] {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("source"), "keep").expect("fixture");
        let arguments = serde_json::json!({"path": path, "new_path": new_path}).to_string();
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call("rename", "rename_file", &arguments),
            Script::text("refused"),
        ]);
        let registry = rune_tools::inventory::builtin_default().expect("builtins");
        let mut rules = RuleSet::new();
        rules.push(Rule::allow("rename_file", "*", Layer::Default));
        rules.push(Rule::deny("rename_file", denied, Layer::User));
        let mut host = TestHost::new(endpoint)
            .with_tools(registry.all_schemas())
            .with_rules(rules);
        host.workspace =
            Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path");
        host.registry = Some(registry);
        let mut history = rune_agent::History::new();
        history.push_user("try a denied rename");
        let outcome = run_turn(&mut history, &host).expect("turn");
        assert!(outcome.calls[0].output.is_error);
        assert!(!outcome.calls[0].executed);
        assert!(host.executed_calls().is_empty());
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("source")).expect("read"),
            "keep"
        );
        assert!(!workspace.path().join("destination").exists());
        assert_eq!(
            host.events()
                .iter()
                .filter(|event| matches!(event, Event::ToolDenied { .. }))
                .count(),
            1
        );
        assert!(!host.events().iter().any(|event| matches!(
            event,
            Event::ToolStarted { .. } | Event::ToolFinished { .. }
        )));
    }
}

#[test]
fn directory_creation_is_advertised_idempotent_and_refuses_an_outside_path() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let outside_path = outside.path().join("one/two");
    let escape = serde_json::json!({"path": outside_path}).to_string();
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("create", "create_directory", r#"{"path":"one/two"}"#),
        Script::tool_call("repeat", "create_directory", r#"{"path":"one/two"}"#),
        Script::tool_call("escape", "create_directory", &escape),
        Script::text("done"),
    ]);
    let registry = rune_tools::inventory::builtin_default().expect("builtins");
    let mut rules = RuleSet::new();
    rules.push(Rule::allow("create_directory", "*", Layer::User));
    let mut host = TestHost::new(endpoint)
        .with_tools(rune_tools::inventory::advertisement(&registry))
        .with_rules(rules);
    host.workspace = Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path");
    host.registry = Some(registry);
    let mut history = rune_agent::History::new();
    history.push_user("create nested directories");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.calls.len(), 3);
    for (index, created) in [(0, 2), (1, 0)] {
        assert!(!outcome.calls[index].output.is_error);
        assert!(
            outcome.calls[index]
                .output
                .text
                .contains(&format!("Created {created} directories"))
        );
        assert!(outcome.calls[index].output.text.len() <= host.context().max_output_bytes);
    }
    assert!(outcome.calls[2].output.is_error);
    assert!(
        outcome.calls[2]
            .output
            .text
            .contains("path_outside_workspace")
    );
    assert!(workspace.path().join("one/two").is_dir());
    assert!(!outside.path().join("one").exists());
    assert_eq!(
        host.events()
            .iter()
            .filter(|event| matches!(
                event,
                Event::ToolStarted {
                    activity: rune_tools::Activity::Write,
                    ..
                }
            ))
            .count(),
        3
    );
    history.validate().expect("paired results");
}

#[test]
fn directory_creation_checks_policy_for_normalized_directory_targets() {
    for spelling in [
        "protected/nested",
        "./protected/nested",
        "unused/../protected/nested",
        "absolute",
    ] {
        let workspace = tempfile::tempdir().expect("workspace");
        let absolute = workspace.path().join("protected/nested");
        let path = if spelling == "absolute" {
            absolute.to_str().expect("UTF-8 path")
        } else {
            spelling
        };
        let arguments = serde_json::json!({"path": path}).to_string();
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call("denied", "create_directory", &arguments),
            Script::text("refused"),
        ]);
        let registry = rune_tools::inventory::builtin_default().expect("builtins");
        let mut rules = RuleSet::new();
        rules.push(Rule::allow("create_directory", "*", Layer::Default));
        rules.push(Rule::deny("create_directory", "protected/*", Layer::User));
        let mut host = TestHost::new(endpoint)
            .with_tools(registry.all_schemas())
            .with_rules(rules);
        host.workspace =
            Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path");
        host.registry = Some(registry);
        let mut history = rune_agent::History::new();
        history.push_user("try a denied directory");
        let outcome = run_turn(&mut history, &host).expect("turn");
        assert!(outcome.calls[0].output.is_error);
        assert!(!outcome.calls[0].executed);
        assert!(host.executed_calls().is_empty());
        assert_eq!(
            std::fs::read_dir(workspace.path())
                .expect("entries")
                .count(),
            0
        );
        assert_eq!(
            host.events()
                .iter()
                .filter(|event| matches!(event, Event::ToolDenied { .. }))
                .count(),
            1
        );
        assert!(
            !host
                .events()
                .iter()
                .any(|event| matches!(event, Event::ToolStarted { .. }))
        );
    }
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

#[test]
fn configured_vision_receives_only_permitted_images_from_the_host_inventory() {
    use rune_tools::vision::RecordingBackend;

    let directory = tempfile::tempdir().expect("workspace");
    let workspace = Utf8PathBuf::from_path_buf(directory.path().to_owned()).expect("UTF-8");
    for name in ["public.png", "secret.png"] {
        std::fs::write(workspace.join(name), b"\x89PNG\r\n\x1a\nfixture").expect("image");
    }
    for paths in [
        vec!["public.png"],
        vec!["secret.png"],
        vec!["public.png", "./secret.png"],
        vec!["./secret.png", "public.png"],
        vec![" secret.png "],
        vec!["public.png", " secret.png "],
    ] {
        let backend = Arc::new(RecordingBackend::new());
        backend.push_one(serde_json::json!({
            "image_id": 1, "status": "ok", "summary": "VISION_HOST_RESULT",
            "visible_text": [], "details": []
        }));
        let registry = rune_tools::inventory::builtin_with_vision(
            &rune_tools::FileLimits::default(),
            &BudgetSet::new(),
            &workspace.join("skills"),
            None,
            Arc::new(rune_tools::ask_user::Unavailable),
            Arc::new(rune_tools::skill::Empty),
            Some(backend.clone()),
        )
        .expect("inventory");
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call(
                "vision_call",
                "vision",
                &serde_json::json!({"paths": paths}).to_string(),
            ),
            Script::text("Finished."),
        ]);
        let mut rules = RuleSet::new();
        rules.push(Rule::allow("vision", "*", Layer::Default));
        rules.push(Rule::deny("vision", "secret.png", Layer::User));
        let mut host = TestHost::new(endpoint)
            .with_tools(rune_tools::inventory::advertisement(&registry))
            .with_rules(rules);
        host.workspace = workspace.clone();
        host.registry = Some(registry);
        let mut history = rune_agent::History::new();
        history.push_user("analyse the permitted image");
        let outcome = run_turn(&mut history, &host).expect("turn");
        let allowed = paths == ["public.png"];
        assert_eq!(outcome.calls.len(), 1);
        assert_eq!(outcome.calls[0].executed, allowed, "{paths:?}");
        assert_eq!(outcome.calls[0].output.is_error, !allowed, "{paths:?}");
        assert_eq!(backend.requests().len(), usize::from(allowed), "{paths:?}");
        if allowed {
            assert_eq!(backend.requests()[0].len(), 1);
            assert!(outcome.calls[0].output.text.contains("VISION_HOST_RESULT"));
            assert!(
                matches!(&backend.requests()[0][0], rune_tools::VisionSource::File { path, .. } if path == &workspace.join("public.png"))
            );
        }
    }
}

/// Builds one batch with independently chosen names, argument strings and IDs.
fn failure_batch(calls: &[(&str, &str, &str)]) -> Script {
    let mut frames: Vec<_> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            serde_json::json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": index, "id": id,
                "function": {"name": name, "arguments": arguments}
            }]}}]})
            .to_string()
        })
        .collect();
    frames.push(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned());
    frames.push("[DONE]".to_owned());
    Script::Frames(frames)
}

fn set_failure_cap(host: &mut TestHost, cap: rune_core::budget::Budget) {
    host.limits
        .set(
            rune_core::LimitName::MaxConsecutiveToolFailures,
            cap,
            rune_core::config::Layer::User,
        )
        .expect("failure cap");
}

fn assert_failure_stop(error: &RuneError, history: &rune_agent::History) {
    assert_eq!(error.code(), rune_core::ErrorCode::LimitExceeded);
    assert_eq!(
        error.detail().invariant.as_deref(),
        Some("max_consecutive_tool_failures")
    );
    history.validate().expect("all tool calls have results");
}

#[test]
fn identical_failures_stop_before_the_next_duplicate_across_steps() {
    let arguments = r#"{"path":"missing"}"#;
    for host_error in [false, true] {
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call("first", "read_file", arguments),
            Script::tool_call("second", "read_file", arguments),
            failure_batch(&[
                ("blocked", "read_file", arguments),
                ("also_blocked", "read_file", r#"{"path":"other"}"#),
            ]),
            Script::text("must not request this"),
        ]);
        let mut host = TestHost::new(endpoint)
            .with_tools(vec![read_tool()])
            .with_tool_response(ToolOutput::failure("missing file"));
        if host_error {
            host.tool_error = Some(RuneError::new(
                rune_core::ErrorCode::NotFound,
                "missing file",
            ));
        }
        set_failure_cap(&mut host, rune_core::budget::Budget::Bounded(2));
        let mut history = rune_agent::History::new();
        history.push_user("read a missing file");
        let error = run_turn(&mut history, &host).expect_err("duplicate must stop");
        assert_failure_stop(&error, &history);
        assert_eq!(host.executed_calls().len(), 2);
        assert_eq!(host.decisions.lock().expect("lock").len(), 2);
        assert_eq!(host.server.as_ref().expect("endpoint").request_count(), 3);
        assert_eq!(
            host.events().iter().filter(|event| matches!(event, Event::ToolDenied { reason, .. } if reason.contains("max_consecutive_tool_failures"))).count(),
            2
        );
    }
}

/// Read-only fixture that can recover on a later invocation of the same call.
struct FailureFixture {
    outputs: Mutex<std::collections::VecDeque<ToolOutput>>,
}

impl rune_tools::Tool for FailureFixture {
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> &'static str {
        "Return scripted success or failure."
    }
    fn input_schema(&self) -> serde_json::Value {
        read_tool().input_schema
    }
    fn activity(&self) -> rune_tools::Activity {
        rune_tools::Activity::Read
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn call(
        &self,
        _arguments: &serde_json::Value,
        _context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        Ok(self
            .outputs
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| ToolOutput::failure("missing file")))
    }
}

fn enable_concurrent_failure_fixture(host: &mut TestHost, outputs: Vec<ToolOutput>) {
    let mut registry = Registry::new();
    registry
        .insert(Box::new(FailureFixture {
            outputs: Mutex::new(outputs.into()),
        }))
        .expect("fixture");
    host.registry = Some(registry);
}

#[test]
fn identical_failures_in_a_batch_stop_at_the_cap_even_for_concurrent_reads() {
    let arguments = r#"{"path":"missing"}"#;
    for concurrent in [false, true] {
        for cap in [1, 2, 3] {
            let batch: Vec<_> = ["a", "b", "c", "d", "e"]
                .into_iter()
                .map(|id| (id, "read_file", arguments))
                .collect();
            let endpoint = MockEndpoint::start(vec![failure_batch(&batch)]);
            let mut host = TestHost::new(endpoint)
                .with_tools(vec![read_tool()])
                .with_tool_response(ToolOutput::failure("missing file"));
            if cap != 3 {
                set_failure_cap(&mut host, rune_core::budget::Budget::Bounded(cap));
            }
            if concurrent {
                enable_concurrent_failure_fixture(&mut host, Vec::new());
            }
            // The decision must use is_error, even when no output is retained.
            set_result_caps(&mut host, 0, 0);
            let mut history = rune_agent::History::new();
            history.push_user("repeat failing reads");
            let error = run_turn(&mut history, &host).expect_err("duplicate must stop");
            assert_failure_stop(&error, &history);
            assert_eq!(host.executed_calls().len(), usize::try_from(cap).unwrap());
            assert_eq!(host.server.as_ref().expect("endpoint").request_count(), 1);
        }
    }
}

#[test]
fn different_calls_reset_the_failure_streak_including_in_parallel_groups() {
    let a = r#"{"path":"a"}"#;
    let b = r#"{"path":"b"}"#;
    // Previous step ends at the cap for A. B resets it before the next A,
    // including when B and A share a parallel execution group.
    let endpoint = MockEndpoint::start(vec![
        failure_batch(&[("a1", "read_file", a), ("a2", "read_file", a)]),
        failure_batch(&[("b", "read_file", b), ("a3", "read_file", a)]),
        failure_batch(&[("other", "other_read", a), ("a4", "read_file", a)]),
        Script::text("done"),
    ]);
    let mut other = read_tool();
    other.name = "other_read".to_owned();
    let mut host = TestHost::new(endpoint)
        .with_tools(vec![read_tool(), other])
        .with_tool_response(ToolOutput::failure("missing file"));
    set_failure_cap(&mut host, rune_core::budget::Budget::Bounded(2));
    enable_concurrent_failure_fixture(&mut host, Vec::new());
    let mut history = rune_agent::History::new();
    history.push_user("try different calls");
    let outcome = run_turn(&mut history, &host).expect("different calls reset the streak");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.calls.len(), 6);
    assert_eq!(host.executed_calls().len(), 6);
    history.validate().expect("paired history");
}

#[test]
fn success_resets_the_failure_streak_for_the_same_call() {
    let arguments = r#"{"path":"missing"}"#;
    let endpoint = MockEndpoint::start(vec![
        failure_batch(&[
            ("a", "read_file", arguments),
            ("b", "read_file", arguments),
            ("c", "read_file", arguments),
            ("d", "read_file", arguments),
        ]),
        Script::text("done"),
    ]);
    let mut host = TestHost::new(endpoint).with_tools(vec![read_tool()]);
    set_failure_cap(&mut host, rune_core::budget::Budget::Bounded(2));
    enable_concurrent_failure_fixture(
        &mut host,
        vec![
            ToolOutput::failure("first failure"),
            ToolOutput::success("recovered"),
        ],
    );
    let mut history = rune_agent::History::new();
    history.push_user("retry a recovering tool");
    let outcome = run_turn(&mut history, &host).expect("success resets the streak");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(host.executed_calls().len(), 4);
    assert!(!outcome.calls[1].output.is_error);
    history.validate().expect("paired history");
}

#[test]
fn failure_cap_off_allows_more_identical_failures_than_the_default() {
    let arguments = r#"{"path":"missing"}"#;
    let batch: Vec<_> = ["a", "b", "c", "d", "e"]
        .into_iter()
        .map(|id| (id, "read_file", arguments))
        .collect();
    let endpoint = MockEndpoint::start(vec![failure_batch(&batch), Script::text("done")]);
    let mut host = TestHost::new(endpoint).with_tools(vec![read_tool()]);
    set_failure_cap(&mut host, rune_core::budget::Budget::Unbounded);
    enable_concurrent_failure_fixture(&mut host, Vec::new());
    let mut history = rune_agent::History::new();
    history.push_user("repeat reads with the failure cap off");
    let outcome = run_turn(&mut history, &host).expect("off disables the guard");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(host.executed_calls().len(), 5);
    history.validate().expect("paired history");
}

#[test]
fn the_failure_streak_is_local_to_a_turn_and_reaching_the_cap_allows_an_answer() {
    let arguments = r#"{"path":"missing"}"#;
    let endpoint = MockEndpoint::start(vec![
        failure_batch(&[("a", "read_file", arguments), ("b", "read_file", arguments)]),
        Script::text("first answer"),
        failure_batch(&[("c", "read_file", arguments), ("d", "read_file", arguments)]),
        Script::text("second answer"),
    ]);
    let mut host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(ToolOutput::failure("missing file"));
    set_failure_cap(&mut host, rune_core::budget::Budget::Bounded(2));
    let mut history = rune_agent::History::new();
    for prompt in ["first turn", "second turn"] {
        history.push_user(prompt);
        let outcome = run_turn(&mut history, &host).expect("each turn has its own streak");
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(outcome.calls.len(), 2);
        history.validate().expect("paired history");
    }
    assert_eq!(host.executed_calls().len(), 4);
}

#[test]
fn repeated_malformed_unoffered_and_denied_calls_stop_without_execution() {
    for scenario in ["malformed", "unoffered", "denied"] {
        let arguments = if scenario == "malformed" {
            "invalid JSON"
        } else {
            r#"{"path":"missing"}"#
        };
        let endpoint = MockEndpoint::start(vec![failure_batch(&[
            ("a", "read_file", arguments),
            ("b", "read_file", arguments),
            ("c", "read_file", arguments),
        ])]);
        let mut host = TestHost::new(endpoint);
        if scenario != "unoffered" {
            host.tools = vec![read_tool()];
        }
        if scenario == "denied" {
            host.rules.push(Rule::deny("read_file", "*", Layer::User));
        }
        set_failure_cap(&mut host, rune_core::budget::Budget::Bounded(2));
        let mut history = rune_agent::History::new();
        history.push_user("repeat an invalid call");
        let error = run_turn(&mut history, &host).expect_err("repeated errors must stop");
        assert_failure_stop(&error, &history);
        assert!(host.executed_calls().is_empty(), "{scenario}");
    }
}

#[test]
fn cancellation_of_identical_calls_preserves_the_cancellation_outcome() {
    let arguments = r#"{"path":"missing"}"#;
    let endpoint = MockEndpoint::start(vec![failure_batch(&[
        ("a", "read_file", arguments),
        ("b", "read_file", arguments),
        ("c", "read_file", arguments),
    ])]);
    let mut host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .cancelling_the_next_call();
    set_failure_cap(&mut host, rune_core::budget::Budget::Bounded(1));
    let mut history = rune_agent::History::new();
    history.push_user("cancel during the first call");
    let error = run_turn(&mut history, &host).expect_err("cancelled turn");
    assert_eq!(error.code(), rune_core::ErrorCode::Cancelled);
    assert_eq!(host.executed_calls().len(), 1);
    history.validate().expect("all cancelled calls answered");
}

#[test]
fn live_plans_survive_turns_and_compaction_but_are_isolated_from_new_histories() {
    let initial = serde_json::json!({"plan": [
        {"step": "Inspect", "status": "in_progress"},
        {"step": "Implement", "status": "pending"},
        {"step": "Verify", "status": "pending"}
    ]});
    let progress = serde_json::json!({"plan": [
        {"step": "Inspect", "status": "completed"},
        {"step": "Implement", "status": "in_progress"},
        {"step": "Verify", "status": "pending"}
    ]});
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("plan-1", "update_plan", &initial.to_string()),
        Script::text("Started."),
        Script::tool_call("plan-2", "update_plan", &progress.to_string()),
        Script::text("Progressed."),
        Script::tool_call("plan-retry", "update_plan", &progress.to_string()),
        Script::text("Unchanged."),
        Script::tool_call("plan-new", "update_plan", &initial.to_string()),
        Script::text("New plan."),
        Script::tool_call("plan-reopen", "update_plan", &initial.to_string()),
        Script::text("Reopened plan."),
    ]);
    let registry = rune_tools::inventory::builtin_default().expect("registry");
    let mut host = TestHost::new(endpoint)
        .with_tools(registry.schemas(&["update_plan"]))
        .with_mode(PermissionMode::FullAccess);
    host.registry = Some(registry);
    let mut history = rune_agent::History::new();
    history.push_user("Plan this work.");
    run_turn(&mut history, &host).expect("initial turn");
    // ACP uses a cloned history snapshot for the next turn.
    let mut history = history.clone();
    history.push_user("Update progress.");
    run_turn(&mut history, &host).expect("progress turn");
    history.replace_with_summary("Work progressed.", u64::MAX);
    history.push_user("Repeat the last plan.");
    run_turn(&mut history, &host).expect("retry after compaction");
    let mut separate = rune_agent::History::new();
    separate.push_user("Start a separate plan in the same registry.");
    run_turn(&mut separate, &host).expect("new conversation");
    let saved = serde_json::to_string(&history).expect("serialized history");
    let mut reopened: rune_agent::History = serde_json::from_str(&saved).expect("reopened history");
    reopened.push_user("Start a plan after reopening.");
    run_turn(&mut reopened, &host).expect("reopened conversation");
    let updates: Vec<_> = host
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::PlanUpdated { update } => Some(update),
            _ => None,
        })
        .collect();
    assert_eq!(
        updates
            .iter()
            .map(|update| update.version)
            .collect::<Vec<_>>(),
        [1, 2, 1, 1]
    );
    assert_eq!(updates[0].plan, updates[2].plan);
    assert!(updates.iter().all(|update| update.plan.len() == 3));
}

#[test]
fn pre_tool_veto_answers_sequential_and_concurrent_calls_without_execution() {
    for cap in [1, 2] {
        let calls = [
            ("fixture_read", "a"),
            ("fixture_read", "b"),
            ("fixture_mutation", "c"),
        ];
        let (mut host, barriers) = barrier_host(&calls, cap);
        host.mode = PermissionMode::FullAccess;
        host.hook_veto = Some(rune_agent::turn::HookVeto {
            name: "fixture guard".to_owned(),
            reason: "maintenance window".to_owned(),
        });
        let mut history = rune_agent::History::new();
        history.push_user("attempt the guarded calls");
        let outcome = run_turn(&mut history, &host).expect("turn");
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(*host.hook_calls.lock().expect("calls"), ["a", "b", "c"]);
        assert!(host.executed_calls().is_empty());
        barriers.assert_started(&[]);
        assert_eq!(outcome.calls.len(), 3);
        for result in outcome.calls {
            assert!(!result.executed);
            assert!(result.output.is_error);
            assert_eq!(
                result.output.text,
                "pre-tool hook `fixture guard` vetoed: maintenance window"
            );
        }
        assert_eq!(
            host.events()
                .iter()
                .filter(|event| matches!(event, Event::ToolDenied { reason, .. }
            if reason == "pre-tool hook `fixture guard` vetoed: maintenance window"))
                .count(),
            3
        );
        assert!(!host.events().iter().any(|event| matches!(
            event,
            Event::ToolStarted { .. } | Event::ToolFinished { .. }
        )));
        history.validate().expect("paired history");
        let request = host
            .server
            .as_ref()
            .expect("server")
            .last_body()
            .expect("request");
        let results = request["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 3);
        for (result, id) in results.iter().zip(["a", "b", "c"]) {
            assert_eq!(result["tool_call_id"], id);
            assert_eq!(
                result["content"],
                "pre-tool hook `fixture guard` vetoed: maintenance window"
            );
        }
    }
}

#[test]
fn pre_tool_hooks_cannot_override_a_policy_denial() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("blocked", "read_file", r#"{"path":"secret.txt"}"#),
        Script::text("done"),
    ]);
    let mut rules = RuleSet::new();
    rules.push(Rule::deny("read_file", "secret.txt", Layer::User));
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_rules(rules);
    let mut history = rune_agent::History::new();
    history.push_user("read secret");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert!(!outcome.calls[0].executed);
    assert!(host.hook_calls.lock().expect("hook calls").is_empty());
    history.validate().expect("paired history");
}
