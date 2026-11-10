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
/// A hook run before each tool call of a [`TestHost`].
type OnExecute = Box<dyn FnMut(&str) + Send>;

/// Held by the tests that start shell background work or depend on there
/// being none, since that work is counted across the whole process.
static BACKGROUND_WORK: Mutex<()> = Mutex::new(());

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
    post_results: Mutex<Vec<(rune_agent::turn::CallResult, std::thread::ThreadId)>>,
    /// Real tools used by regressions that must reach the implementation.
    registry: Option<Registry>,
    cancellation: Cancellation,
    steering: SteeringQueue,
    workspace: Utf8PathBuf,
    limits: BudgetSet,
    objective_budget: Mutex<Option<(u64, u64)>>,
    /// Body handed to the loop in place of its own serialization, once.
    encoded_request: Mutex<Option<String>>,
    /// Whether `compact_now` compacts, through the shared helper.
    compacts: bool,
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
    /// Runs before each tool call with the tool's name.
    on_execute: Mutex<Option<OnExecute>>,
    /// Largest answer the model is described as producing.
    max_output: Option<u64>,
    /// Context window the model is described as having, in tokens.
    context_window: Option<u64>,
    /// Output ceilings of the plans that reached request preparation.
    planned_output: Mutex<Vec<Option<u64>>>,
    /// The cache key every request of the run is given.
    cache_key: Option<String>,
    /// Cache key and whether summaries were asked for, per planned request.
    planned_cache: Mutex<Vec<(Option<String>, bool)>>,
    /// Whether the model is described as seeing images tools attach.
    accepts_images: bool,
    /// Base64 data of the images tools attached, by image id.
    tool_images: Mutex<std::collections::BTreeMap<u64, Arc<str>>>,
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
            post_results: Mutex::new(Vec::new()),
            registry: None,
            cancellation: Cancellation::new(),
            steering: SteeringQueue::new(8),
            workspace: Utf8PathBuf::from("/tmp/rune-test"),
            limits: BudgetSet::new(),
            objective_budget: Mutex::new(None),
            encoded_request: Mutex::new(None),
            compacts: false,
            resolve_ask: true,
            private_authority: None,
            private_decisions: Mutex::new(Vec::new()),
            submit_on_first_delta: Mutex::new(None),
            cancel_on_execute: Mutex::new(false),
            on_execute: Mutex::new(None),
            max_output: None,
            context_window: None,
            planned_output: Mutex::new(Vec::new()),
            cache_key: None,
            planned_cache: Mutex::new(Vec::new()),
            accepts_images: false,
            tool_images: Mutex::new(std::collections::BTreeMap::new()),
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

    fn cache_key(&self) -> Option<String> {
        self.cache_key.clone()
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

    fn max_output_tokens(&self) -> Option<u64> {
        self.max_output
    }

    fn context_window(&self) -> Option<u64> {
        self.context_window
    }

    fn prepare_request(
        &self,
        _history: &mut rune_agent::history::History,
        plan: &mut rune_net::provider::RequestPlan,
        _client: &dyn rune_net::fetch::Fetch,
    ) -> Result<()> {
        self.planned_output
            .lock()
            .expect("planned output")
            .push(plan.max_output_tokens);
        self.planned_cache
            .lock()
            .expect("planned cache")
            .push((plan.cache_key.clone(), plan.reasoning_summary));
        plan.image_data
            .extend(self.tool_images.lock().expect("images").clone());
        Ok(())
    }

    fn accepts_images(&self) -> bool {
        self.accepts_images
    }

    fn tool_attachments(
        &self,
        result: &rune_agent::turn::CallResult,
    ) -> Vec<rune_net::message::ContentPart> {
        let Some(image) = result.output.image.as_ref().filter(|_| result.executed) else {
            return Vec::new();
        };
        let mut images = self.tool_images.lock().expect("images");
        let id = (1_u64 << 32).saturating_add(images.len() as u64);
        images.insert(id, Arc::from(base64(&image.bytes)));
        vec![rune_net::message::ContentPart::Image {
            image: rune_net::message::ImageRef {
                id,
                media_type: image.media_type.to_owned(),
                encoded_bytes: image.bytes.len() as u64,
            },
        }]
    }

    fn fetch(&self) -> Box<dyn rune_net::fetch::Fetch> {
        if let Some(calls) = &self.offline_fetch_calls {
            Box::new(CountingOfflineFetch(Arc::clone(calls)))
        } else {
            Box::new(rune_net::transport::UreqFetch::new())
        }
    }

    fn take_encoded_request(&self) -> Option<String> {
        self.encoded_request.lock().expect("encoded").take()
    }

    fn compact_now(
        &self,
        history: &mut rune_agent::History,
        client: &dyn rune_net::fetch::Fetch,
        _reason: rune_agent::turn::CompactReason,
    ) -> Result<bool> {
        if !self.compacts {
            return Ok(false);
        }
        rune_agent::compaction::compact_now(history, self, client)
    }

    fn objective_tokens_remaining(&self) -> Option<u64> {
        self.objective_budget
            .lock()
            .expect("budget")
            .map(|(budget, used)| budget.saturating_sub(used))
    }

    fn account_objective_tokens(&self, tokens: u64) -> Result<()> {
        if let Some((_, used)) = self.objective_budget.lock().expect("budget").as_mut() {
            *used = used.saturating_add(tokens);
        }
        Ok(())
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

    fn post_tool(&self, result: &rune_agent::turn::CallResult) {
        self.post_results
            .lock()
            .expect("post results")
            .push((result.clone(), std::thread::current().id()));
    }

    fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        self.executed
            .lock()
            .expect("lock")
            .push((name.to_owned(), arguments.clone()));
        if let Some(hook) = self.on_execute.lock().expect("lock").as_mut() {
            hook(name);
        }
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

    fn decide_quietly(&self, name: &str, target: Option<&str>) -> Outcome {
        rune_agent::turn::decide_call_in_workspace(
            &self.rules,
            self.mode,
            name,
            target,
            &self.workspace,
        )
        .0
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
        if name == "read_file"
            && self.accepts_images
            && let Some(registry) = &self.registry
        {
            // The per-call context carries whether the model can see an image.
            return registry.call(name, arguments, context);
        }
        if name == "update_plan" {
            return self
                .registry
                .as_ref()
                .expect("plan registry")
                .call(name, arguments, context);
        }
        if name == "edit_file"
            && let Some(registry) = &self.registry
        {
            // The per-call context carries whether the edit may quote the file.
            self.executed
                .lock()
                .expect("lock")
                .push((name.to_owned(), arguments.clone()));
            return registry.call(name, arguments, context);
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

    // The shortest jitter keeps the waits these tests observe exact.
    fn retry_policy(&self, max_attempts: usize) -> rune_agent::retry::RetryPolicy {
        rune_agent::retry::RetryPolicy::interactive(max_attempts).with_jitter(|| 0)
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
        // The middle is what a preview leaves out, so it stands for content
        // that only the store holds.
        let content = format!(
            "PREVIEW {} MIDDLE-ONLY {} FULL-TAIL",
            "é".repeat(20_000),
            "é".repeat(20_000)
        );
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
        // The preview keeps both ends and names the range left out between.
        assert!(shown.text.contains("FULL-TAIL"), "{}", shown.text);
        assert!(shown.text.contains("omitted; read_tool_result reads them"));
        assert!(!shown.text.contains("MIDDLE-ONLY"));
        assert!(shown.text.len() <= 32_768);
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
        assert!(!body.to_string().contains("MIDDLE-ONLY"));
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
        assert!(!saved.contains("MIDDLE-ONLY"));
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
fn a_small_context_window_lowers_the_result_cap() {
    let content = format!("START {} END", "x".repeat(20 * 1024));
    for (window, configured) in [
        (Some(16_000), None),
        (None, None),
        (Some(16_000), Some(2_048)),
    ] {
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call("large", "read_file", "{\"path\":\"a.rs\"}"),
            Script::text("done"),
        ]);
        let mut host = TestHost::new(endpoint)
            .with_tools(vec![read_tool()])
            .with_tool_response(ToolOutput::success(content.clone()));
        host.context_window = window;
        if let Some(cap) = configured {
            host.limits
                .set(
                    rune_core::LimitName::MaxToolResultBytes,
                    rune_core::budget::Budget::Bounded(cap),
                    rune_core::config::Layer::User,
                )
                .expect("limit");
        }
        let mut history = rune_agent::History::new();
        history.push_user("read");
        let outcome = run_turn(&mut history, &host).expect("turn");
        let shown = &outcome.calls[0].output.text;
        match (window, configured) {
            (Some(_), None) => {
                // A tenth of 16,000 tokens at four bytes a token, handle line included.
                assert!(shown.len() <= 6_400, "{}", shown.len());
                assert!(shown.len() > 4_096, "{}", shown.len());
                let handle = retained_handle(shown);
                assert_eq!(
                    history.result_store().describe(&handle),
                    Some(("read_file", content.len() as u64, false))
                );
            }
            (None, _) => assert_eq!(shown, &content),
            (Some(_), Some(cap)) => assert!(shown.len() <= cap as usize, "{}", shown.len()),
        }
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
fn a_completed_request_anchors_the_next_estimate() {
    let endpoint = MockEndpoint::start(vec![Script::text("hi")]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("hello");

    run_turn(&mut history, &host).expect("turn");
    let anchor = history.anchor().expect("anchor");
    assert_eq!(anchor.input_tokens, 10);
    assert_eq!(anchor.model, host.model);
    assert_eq!(anchor.dialect, host.dialect.name());
    assert!(anchor.measure.text_bytes > 0, "{anchor:?}");
}

#[test]
fn every_request_of_a_turn_carries_the_host_cache_key() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("call_1", "read_file", "{\"path\":\"a.rs\"}"),
        Script::text("done"),
    ]);
    let mut host = TestHost::new(endpoint);
    host.tools = vec![read_tool()];
    host.cache_key = Some("k".to_owned());
    let mut history = rune_agent::History::new();
    history.push_user("read a.rs");

    run_turn(&mut history, &host).expect("turn");
    let planned = host.planned_cache.lock().expect("planned").clone();
    assert_eq!(planned.len(), 2, "{planned:?}");
    assert!(
        planned.iter().all(|(key, _)| key.as_deref() == Some("k")),
        "{planned:?}"
    );
}

#[test]
fn a_refused_summary_is_dropped_and_the_step_continues() {
    let endpoint = MockEndpoint::start(vec![
        Script::Status {
            code: 400,
            body: r#"{"error":{"message":"Your organization must be verified to generate reasoning summaries"}}"#
                .to_owned(),
        },
        Script::text("hi"),
        Script::text("again"),
    ]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("hello");

    let outcome = run_turn(&mut history, &host).expect("the turn continues");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    history.push_user("hello again");
    run_turn(&mut history, &host).expect("the second turn");

    let planned = host.planned_cache.lock().expect("planned").clone();
    let asked: Vec<bool> = planned.iter().map(|(_, summary)| *summary).collect();
    assert_eq!(asked, vec![true, false, false]);
    let notices = host
        .events()
        .iter()
        .filter(|event| matches!(event, Event::Notice { text } if text == rune_agent::turn::SUMMARY_REFUSED_NOTICE))
        .count();
    assert_eq!(notices, 1);
    assert!(
        !host
            .events()
            .iter()
            .any(|event| matches!(event, Event::ProviderRetry { .. })),
        "the resend is not a provider retry"
    );
}

#[test]
fn a_failed_request_leaves_no_anchor() {
    let endpoint = MockEndpoint::start(vec![Script::Status {
        code: 400,
        body: "bad request".to_owned(),
    }]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("hello");

    run_turn(&mut history, &host).expect_err("refused");
    assert!(history.anchor().is_none());
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
            .and_then(rune_agent::turn::TurnOutcome::into_strict)
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
fn the_last_step_offers_tools_but_forbids_calls() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("first", "read_file", "{\"path\":\"a.rs\"}"),
        Script::tool_call("second", "read_file", "{\"path\":\"b.rs\"}"),
    ]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_step_limit(2);

    let mut history = rune_agent::History::new();
    history.push_user("read two files");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::StepLimit);
    assert_eq!(outcome.steps, 2);

    let body = host
        .server
        .as_ref()
        .expect("endpoint")
        .last_body()
        .expect("body");
    assert_eq!(body["tool_choice"], "none");
    assert!(!body["tools"].as_array().expect("tools").is_empty());
    assert!(body.to_string().contains(LAST_STEP_NOTE_TEXT));

    assert_eq!(outcome.calls.len(), 2);
    assert!(outcome.calls[0].executed);
    assert!(!outcome.calls[1].executed);
    assert_eq!(
        outcome.calls[1].output.text,
        "not run: the step limit was reached"
    );
    assert_eq!(host.executed_calls().len(), 1);
    history.validate().expect("valid history");
}

/// The part of the last-step note that survives JSON escaping.
const LAST_STEP_NOTE_TEXT: &str = "This is the last step allowed in this run.";

#[test]
fn the_last_step_skips_the_verification_reminder() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("w", "write_file", "{\"path\":\"a.txt\",\"content\":\"x\"}"),
        Script::text("Wrote it; nothing was run."),
    ]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![ToolSpec {
            name: "write_file".to_owned(),
            description: "Write a file.".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }])
        .with_tool_response(ToolOutput::success("Wrote `a.txt` (new file, 1 line)"))
        .with_step_limit(2);

    let mut history = rune_agent::History::new();
    history.push_user("write a file");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(host.server.as_ref().expect("endpoint").request_count(), 2);
    assert_eq!(outcome.stop_reason, StopReason::StepLimit);
    assert_eq!(outcome.text, "Wrote it; nothing was run.");
}

#[test]
fn an_uncapped_turn_runs_past_sixteen_steps() {
    let mut scripts: Vec<Script> = (0..20)
        .map(|index| {
            Script::tool_call(
                &format!("call_{index}"),
                "read_file",
                &format!("{{\"path\":\"f{index}.rs\"}}"),
            )
        })
        .collect();
    scripts.push(Script::text("All read."));
    let host = TestHost::new(MockEndpoint::start(scripts)).with_tools(vec![read_tool()]);

    let mut history = rune_agent::History::new();
    history.push_user("read many files");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.steps, 21);
}

#[test]
fn an_offline_turn_refuses_without_fetch_calls_or_retry_delay() {
    let mut host = TestHost::new(MockEndpoint::start(vec![Script::text("unused")]));
    // Offline still reaches a loopback server, so the refusal is shown on a
    // remote endpoint.
    host.endpoint.base_url = "https://api.example.com/v1".to_owned();
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
                ..
            } => Some((step, next_attempt, max_attempts, delay.as_millis())),
            Event::StepRestarted { step } => Some((step, 0, 0, 0)),
            Event::ProviderRetryFinished { step } => Some((step, 0, 0, 1)),
            _ => None,
        })
        .collect();
    assert_eq!(
        retries,
        [
            (1, 2, 10, 500),
            (1, 0, 0, 0),
            (1, 3, 10, 1000),
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

/// Six alternating turns, with a tail budget of zero so compaction keeps only
/// the last two.
fn compactable(endpoint: MockEndpoint) -> (TestHost, rune_agent::History) {
    let mut host = TestHost::new(endpoint);
    host.limits
        .set(
            rune_core::LimitName::CompactionKeepTokens,
            rune_core::budget::Budget::Bounded(0),
            rune_core::config::Layer::CommandLine,
        )
        .expect("tail budget");
    let mut history = rune_agent::History::new();
    for index in 0..3 {
        history.push_user(format!("question {index}"));
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: format!("answer {index}"),
        }]);
    }
    (host, history)
}

#[test]
fn compact_replaces_old_turns_and_returns_the_summary_usage() {
    let endpoint = MockEndpoint::start(vec![Script::text(
        "summary of the work so far, long enough to keep",
    )]);
    let (host, mut history) = compactable(endpoint);
    let plan = rune_agent::compaction::plan(&history, &host.limits).expect("a plan");
    let client = host.fetch();
    let report =
        rune_agent::compaction::compact(&mut history, &plan, &host, client.as_ref(), None, &|| {
            false
        })
        .expect("compaction");
    assert_eq!(report.removed_turns, plan.removed_turns);
    assert_eq!(report.remaining_turns, history.len());
    // The summary, then the newest question and its answer.
    assert_eq!(report.remaining_turns, 3);
    let first = format!("{:?}", history.turns()[0].parts);
    assert!(first.contains("<context_handoff>"), "{first}");
    assert_eq!(report.usage.input_tokens, Some(10));
    history.validate().expect("valid history");
}

#[test]
fn compact_leaves_history_unchanged_on_a_short_summary() {
    let endpoint = MockEndpoint::start(vec![Script::text("ok")]);
    let (host, mut history) = compactable(endpoint);
    let before = history.clone();
    let plan = rune_agent::compaction::plan(&history, &host.limits).expect("a plan");
    let client = host.fetch();
    let err =
        rune_agent::compaction::compact(&mut history, &plan, &host, client.as_ref(), None, &|| {
            false
        })
        .expect_err("a two-byte summary is refused");
    assert!(
        err.message().contains("nothing usable"),
        "{}",
        err.message()
    );
    assert_eq!(history.turns(), before.turns());
}

#[test]
fn a_host_encoded_request_is_sent_as_given() {
    let endpoint = MockEndpoint::start(vec![Script::text("done")]);
    let host = TestHost::new(endpoint);
    *host.encoded_request.lock().expect("encoded") = Some(
        r#"{"model":"test-model","stream":true,"messages":[{"role":"user","content":"hi"}],"marker":"from the host"}"#
            .to_owned(),
    );
    let mut history = rune_agent::History::new();
    history.push_user("hello");
    run_turn(&mut history, &host).expect("turn");
    let body = host
        .server
        .as_ref()
        .expect("endpoint")
        .last_body()
        .expect("body");
    assert_eq!(body["marker"], "from the host");
}

#[test]
fn an_active_objective_discards_the_host_encoded_request() {
    // The objective caps output tokens after the host serialized the plan,
    // so its text no longer matches and the loop serializes afresh.
    let endpoint = MockEndpoint::start(vec![Script::text("done")]);
    let host = TestHost::new(endpoint);
    *host.objective_budget.lock().expect("budget") = Some((100_000, 0));
    *host.encoded_request.lock().expect("encoded") =
        Some(r#"{"model":"test-model","stream":true,"messages":[],"marker":"stale"}"#.to_owned());
    let mut history = rune_agent::History::new();
    history.push_user("hello");
    run_turn(&mut history, &host).expect("turn");
    let body = host
        .server
        .as_ref()
        .expect("endpoint")
        .last_body()
        .expect("body");
    assert!(body.get("marker").is_none(), "{body}");
    assert!(body["messages"].to_string().contains("hello"), "{body}");
}

#[test]
fn encode_request_matches_the_transport_body() {
    let (host, history) = compactable(MockEndpoint::start(Vec::new()));
    let mut plan = rune_net::provider::RequestPlan::new("test-model");
    plan.instructions = "instructions".to_owned();
    plan.messages = history.to_messages();
    plan.tools = vec![read_tool()];
    let (encoded, estimate) = rune_agent::compaction::encode_request(
        host.dialect.as_ref(),
        &plan,
        rune_agent::compaction::Window(100_000),
        None,
    )
    .expect("encoded");
    let body = host.dialect.build_request(&plan).expect("body");
    assert_eq!(encoded, serde_json::to_string(&body).expect("json"));
    assert_eq!(estimate.serialized_bytes, encoded.len() as u64);
}

/// Five read steps, then a request the provider refuses as too long.
fn overflow_scripts(after: Vec<Script>) -> Vec<Script> {
    let mut scripts: Vec<Script> = (1..=5)
        .map(|n| {
            Script::tool_call(
                &format!("c{n}"),
                "read_file",
                &format!("{{\"path\":\"{n}.rs\"}}"),
            )
        })
        .collect();
    scripts.push(Script::Status {
        code: 400,
        body: "context length exceeded".to_owned(),
    });
    scripts.extend(after);
    scripts
}

#[test]
fn an_overflow_compacts_and_resends_the_step() {
    let endpoint = MockEndpoint::start(overflow_scripts(vec![
        Script::text("The user asked for five files to be read; all five were read."),
        Script::text("finished"),
    ]));
    let mut host = single_attempt(TestHost::new(endpoint).with_tools(vec![read_tool()]));
    host.compacts = true;
    host.limits
        .set(
            rune_core::LimitName::CompactionKeepTokens,
            rune_core::budget::Budget::Bounded(0),
            rune_core::config::Layer::CommandLine,
        )
        .expect("tail budget");
    let mut history = rune_agent::History::new();
    history.push_user("read five files");
    let outcome = run_turn(&mut history, &host).expect("the step is resent");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.text, "finished");
    let events = host.events();
    let compactions = events
        .iter()
        .filter(|event| matches!(event, Event::ContextCompacted { .. }))
        .count();
    assert_eq!(compactions, 1);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::StepRestarted { step: 6 })),
        "{events:?}"
    );
    let body = host
        .server
        .as_ref()
        .expect("endpoint")
        .last_body()
        .expect("body");
    assert!(body.to_string().contains("<context_handoff>"), "{body}");
    history.validate().expect("valid history");
}

#[test]
fn a_host_without_compaction_reports_the_overflow_hint() {
    let endpoint = MockEndpoint::start(overflow_scripts(Vec::new()));
    let host = single_attempt(TestHost::new(endpoint).with_tools(vec![read_tool()]));
    let mut history = rune_agent::History::new();
    history.push_user("read five files");
    let outcome = run_turn(&mut history, &host).expect("a partial outcome");
    let error = outcome.error.expect("the overflow error");
    assert_eq!(error.hint(), Some(rune_agent::turn::OVERFLOW_HINT));
    assert_eq!(outcome.steps, 5);
}

fn single_attempt(mut host: TestHost) -> TestHost {
    host.limits
        .set(
            rune_core::LimitName::ProviderMaxAttempts,
            rune_core::budget::Budget::Bounded(1),
            rune_core::config::Layer::User,
        )
        .expect("attempt cap");
    host
}

#[test]
fn a_failure_after_a_completed_request_returns_the_partial_outcome() {
    let billed = |id: &str, path: &str| {
        let Script::Frames(mut frames) =
            Script::tool_call(id, "read_file", &format!("{{\"path\":\"{path}\"}}"))
        else {
            panic!("frames");
        };
        frames.insert(
            frames.len() - 1,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.to_owned(),
        );
        Script::Frames(frames)
    };
    let endpoint = MockEndpoint::start(vec![
        billed("first", "a.rs"),
        billed("second", "b.rs"),
        Script::provider_error("bad"),
    ]);
    let host = single_attempt(TestHost::new(endpoint).with_tools(vec![read_tool()]));
    let mut history = rune_agent::History::new();
    history.push_user("read two files");

    let outcome = run_turn(&mut history, &host).expect("a partial outcome");
    assert_eq!(outcome.steps, 2);
    assert_eq!(outcome.usage.input_tokens, Some(20));
    assert!(outcome.error.is_some());
    assert_eq!(outcome.stop_reason, StopReason::ProviderFailure);
    assert_eq!(outcome.calls.len(), 2);
    assert!(outcome.calls.iter().all(|call| call.executed));
    assert_eq!(host.executed_calls().len(), 2);
    history.validate().expect("valid history");
}

#[test]
fn a_failure_before_any_request_completes_is_still_an_error() {
    let endpoint = MockEndpoint::start(vec![Script::provider_error("bad")]);
    let host = single_attempt(TestHost::new(endpoint));
    let mut history = rune_agent::History::new();
    history.push_user("hello");

    let err = run_turn(&mut history, &host).expect_err("no request completed");
    assert!(err.message().contains("bad"), "{}", err.message());
}

#[test]
fn into_strict_turns_a_partial_outcome_into_its_error() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("first", "read_file", "{\"path\":\"a.rs\"}"),
        Script::provider_error("bad"),
    ]);
    let host = single_attempt(TestHost::new(endpoint).with_tools(vec![read_tool()]));
    let mut history = rune_agent::History::new();
    history.push_user("read a file");

    let outcome = run_turn(&mut history, &host).expect("a partial outcome");
    let message = outcome
        .error
        .as_ref()
        .expect("an error")
        .message()
        .to_owned();
    let err = outcome
        .into_strict()
        .expect_err("strict hosts see the error");
    assert_eq!(err.message(), message);

    let endpoint = MockEndpoint::start(vec![Script::text("done")]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("hello");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert!(outcome.into_strict().is_ok());
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

    // The request completed before the cancellation, so the turn keeps it.
    let outcome = run_turn(&mut history, &host).expect("a partial outcome");
    assert_eq!(outcome.stop_reason, StopReason::Cancelled);
    assert_eq!(outcome.steps, 1);
    let err = outcome.into_strict().expect_err("cancelled");
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
                Event::ToolFinished { output, .. } if !output.is_error
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

/// Runs one failing `edit_file` call on a file holding a secret, with the
/// given read rule, and returns the call's result text.
fn missed_edit_result(read_rule: Rule) -> String {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(
        workspace.path().join("secret.txt"),
        "alpha\nsecret value 12345\nomega\n",
    )
    .expect("fixture");
    let arguments = serde_json::json!({
        "path": "secret.txt",
        "old_string": "secret value 99999\nomega\n",
        "new_string": "x\n",
    })
    .to_string();
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("edit", "edit_file", &arguments),
        Script::text("done"),
    ]);
    let registry = rune_tools::inventory::builtin_default().expect("builtins");
    let mut rules = RuleSet::new();
    rules.push(Rule::allow("edit_file", "*", Layer::User));
    rules.push(read_rule);
    let mut host = TestHost::new(endpoint)
        .with_tools(registry.all_schemas())
        .with_rules(rules);
    host.workspace = Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path");
    host.registry = Some(registry);
    let mut history = rune_agent::History::new();
    history.push_user("edit the secret file");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert!(
        outcome.calls[0].output.is_error,
        "{}",
        outcome.calls[0].output.text
    );
    outcome.calls[0].output.text.clone()
}

#[test]
fn an_edit_of_a_file_read_would_ask_for_quotes_nothing() {
    let quiet = missed_edit_result(Rule::ask("read_file", "*", Layer::User));
    assert!(!quiet.contains("12345"), "{quiet}");
    assert!(!quiet.contains("omega"), "{quiet}");
    assert!(quiet.contains("read the file again"), "{quiet}");
    // The same miss with reading allowed quotes the closest lines.
    let quoted = missed_edit_result(Rule::allow("read_file", "*", Layer::User));
    assert!(quoted.contains("secret value 12345"), "{quoted}");
}

#[test]
fn directory_creation_when_offered_is_idempotent_and_refuses_an_outside_path() {
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
    // Not advertised by default; a host may still offer it.
    let mut tools = rune_tools::inventory::advertisement(&registry);
    tools.push(rune_tools::contract::model_spec(
        registry.get("create_directory").expect("registered"),
    ));
    let mut host = TestHost::new(endpoint).with_tools(tools).with_rules(rules);
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

fn assert_stalled(outcome: &rune_agent::turn::TurnOutcome, history: &rune_agent::History) {
    assert_eq!(outcome.stop_reason, StopReason::Stalled);
    let error = outcome
        .error
        .as_ref()
        .expect("a stalled run carries its error");
    assert_eq!(error.code(), rune_core::ErrorCode::LimitExceeded);
    assert_eq!(
        error.detail().invariant.as_deref(),
        Some("max_consecutive_tool_failures")
    );
    history.validate().expect("all tool calls have results");
}

/// How a repeated-failure refusal begins.
const REFUSED: &str = "not run: this exact call has failed";
/// The answer to the call that stalls the run.
const STALLED: &str =
    "not run: the run stopped because this call was sent again after two refusals";

#[test]
fn identical_failures_are_refused_across_steps_and_the_turn_continues() {
    let arguments = r#"{"path":"missing"}"#;
    for host_error in [false, true] {
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call("first", "read_file", arguments),
            Script::tool_call("second", "read_file", arguments),
            failure_batch(&[
                ("refused", "read_file", arguments),
                ("different", "read_file", r#"{"path":"other"}"#),
            ]),
            Script::text("the file is missing"),
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
        let outcome = run_turn(&mut history, &host).expect("the turn goes on");
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(outcome.text, "the file is missing");
        assert!(outcome.error.is_none());
        // The refused call never ran; the different one did.
        assert_eq!(host.executed_calls().len(), 3);
        assert!(!outcome.calls[2].executed);
        assert!(outcome.calls[2].output.text.starts_with(REFUSED));
        assert!(outcome.calls[3].executed);
        assert_eq!(host.server.as_ref().expect("endpoint").request_count(), 4);
        assert_eq!(
            host.events()
                .iter()
                .filter(|event| matches!(event, Event::ToolDenied { reason, .. } if reason.contains("this exact call has failed")))
                .count(),
            1
        );
        history.validate().expect("paired history");
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
fn identical_failures_in_a_batch_stall_at_the_cap_even_for_concurrent_reads() {
    let arguments = r#"{"path":"missing"}"#;
    for concurrent in [false, true] {
        for cap in [1, 2, 3] {
            // `cap` failures, two refusals, the stall, then the rest unrun.
            let batch: Vec<_> = ["a", "b", "c", "d", "e", "f"]
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
            let outcome = run_turn(&mut history, &host).expect("a stalled outcome");
            assert_stalled(&outcome, &history);
            let cap = usize::try_from(cap).unwrap();
            assert_eq!(host.executed_calls().len(), cap);
            assert_eq!(outcome.calls.len(), 6);
            // A zero result budget cuts the answers' text, so only their
            // shape is checked here; the texts are checked elsewhere.
            assert!(
                outcome.calls[cap..]
                    .iter()
                    .all(|call| !call.executed && call.output.is_error)
            );
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
fn repeated_malformed_unoffered_and_denied_calls_stall_without_execution() {
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
            ("d", "read_file", arguments),
            ("e", "read_file", arguments),
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
        let outcome = run_turn(&mut history, &host).expect("a stalled outcome");
        assert_stalled(&outcome, &history);
        assert!(host.executed_calls().is_empty(), "{scenario}");
    }
}

#[test]
fn a_repeated_failure_is_refused_and_the_turn_continues() {
    let arguments = r#"{"path":"missing"}"#;
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("c1", "read_file", arguments),
        Script::tool_call("c2", "read_file", arguments),
        Script::tool_call("c3", "read_file", arguments),
        Script::tool_call("c4", "read_file", arguments),
        Script::text("the file does not exist"),
    ]);
    let mut host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(ToolOutput::failure("missing file"));
    set_failure_cap(&mut host, rune_core::budget::Budget::Bounded(3));
    let mut history = rune_agent::History::new();
    history.push_user("read a missing file");
    let outcome = run_turn(&mut history, &host).expect("the turn goes on");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.text, "the file does not exist");
    assert_eq!(host.executed_calls().len(), 3);
    assert!(!outcome.calls[3].executed);
    assert_eq!(
        outcome.calls[3].output.text,
        "not run: this exact call has failed 3 times in a row. Do not send it again. Read the \
         error above and change something: different arguments, another tool, or say what is \
         blocking you."
    );
    history.validate().expect("paired history");
}

#[test]
fn a_call_sent_after_two_refusals_stalls_the_run() {
    let arguments = r#"{"path":"missing"}"#;
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("c1", "read_file", arguments),
        Script::tool_call("c2", "read_file", arguments),
        Script::tool_call("c3", "read_file", arguments),
        Script::tool_call("c4", "read_file", arguments),
        Script::tool_call("c5", "read_file", arguments),
        Script::text("never sent"),
    ]);
    let mut host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(ToolOutput::failure("missing file"));
    set_failure_cap(&mut host, rune_core::budget::Budget::Bounded(2));
    let mut history = rune_agent::History::new();
    history.push_user("read a missing file");
    let outcome = run_turn(&mut history, &host).expect("a stalled outcome");
    assert_stalled(&outcome, &history);
    assert_eq!(outcome.steps, 5);
    assert_eq!(host.executed_calls().len(), 2);
    let refused = outcome
        .calls
        .iter()
        .filter(|call| call.output.text.starts_with(REFUSED))
        .count();
    assert_eq!(refused, 2);
    assert_eq!(outcome.calls[4].output.text, STALLED);
    assert_eq!(host.server.as_ref().expect("endpoint").request_count(), 5);
}

#[test]
fn different_arguments_are_never_refused() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("c1", "read_file", r#"{"path":"a"}"#),
        Script::tool_call("c2", "read_file", r#"{"path":"b"}"#),
        Script::tool_call("c3", "read_file", r#"{"path":"c"}"#),
        Script::tool_call("c4", "read_file", r#"{"path":"d"}"#),
        Script::text("none of them exist"),
    ]);
    let mut host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(ToolOutput::failure("missing file"));
    set_failure_cap(&mut host, rune_core::budget::Budget::Bounded(1));
    let mut history = rune_agent::History::new();
    history.push_user("read four missing files");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(host.executed_calls().len(), 4);
    history.validate().expect("paired history");
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
    let error = run_turn(&mut history, &host)
        .and_then(rune_agent::turn::TurnOutcome::into_strict)
        .expect_err("cancelled turn");
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

#[test]
fn post_tool_observes_finalized_concurrent_and_denied_results_once_in_order() {
    let calls = [
        ("fixture_read", "a"),
        ("fixture_read", "b"),
        ("fixture_mutation", "c"),
    ];
    let (mut host, barriers) = barrier_host(&calls, 2);
    host.mode = PermissionMode::Ask;
    host.rules
        .push(Rule::allow("fixture_read", "*", Layer::User));
    host.rules
        .push(Rule::deny("fixture_mutation", "*", Layer::User));
    // Exercise result retention before notification.
    set_result_caps(&mut host, 128, 256);
    let mut history = rune_agent::History::new();
    history.push_user("read and attempt a denied mutation");
    let (outcome, turn_thread) = std::thread::scope(|scope| {
        let worker = scope.spawn(|| (run_turn(&mut history, &host), std::thread::current().id()));
        barriers.wait_for(&["a", "b"], false);
        barriers.release("b");
        barriers.wait_for(&["b"], true);
        barriers.release("a");
        let (outcome, thread) = worker.join().expect("worker");
        (outcome.expect("turn"), thread)
    });
    let observed = host.post_results.lock().expect("post results");
    assert_eq!(observed.len(), 3);
    for ((result, thread), expected) in observed.iter().zip(&outcome.calls) {
        assert_eq!(*thread, turn_thread);
        assert_eq!(result.call.id, expected.call.id);
        assert_eq!(result.output, expected.output);
        assert_eq!(result.executed, expected.executed);
    }
    assert_eq!(observed[0].0.call.id, "a");
    assert_eq!(observed[1].0.call.id, "b");
    assert!(!observed[2].0.executed);
    assert!(observed[2].0.output.is_error);
    history.validate().expect("paired history");
}

#[test]
fn post_tool_observes_retained_result_reads_and_tool_errors_once() {
    let mut history = rune_agent::History::new();
    let content = "full output".repeat(300);
    let handle = rune_tools::Handle::derive(history.result_store().session(), "fixture", &content);
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("fixture", "read_file", r#"{"path":"fixture.txt"}"#),
        result_read_batch(&handle, &[(0, 100)]),
        Script::tool_call("error", "read_file", "malformed"),
        Script::text("done"),
    ]);
    let mut host = TestHost::new(endpoint)
        .with_tools(vec![read_tool(), result_reader_spec()])
        .with_tool_response(ToolOutput::success(content));
    set_result_caps(&mut host, 1024, 4096);
    history.push_user("retain and read");
    let outcome = run_turn(&mut history, &host).expect("turn");
    let observed = host.post_results.lock().expect("post results");
    assert_eq!(observed.len(), 3);
    assert_eq!(observed[1].0.call.name, "read_tool_result");
    assert!(observed[1].0.executed);
    assert!(observed[2].0.output.is_error);
    for ((result, _), expected) in observed.iter().zip(&outcome.calls) {
        assert_eq!(result.output, expected.output);
    }
    history.validate().expect("paired history");
}

#[test]
fn objective_budget_is_consumed_across_turns_and_blocks_further_requests() {
    let endpoint = MockEndpoint::start(vec![Script::text("answer")]);
    let host = TestHost::new(endpoint);
    *host.objective_budget.lock().expect("budget") = Some((30, 0));
    let mut history = rune_agent::History::new();
    for (index, remaining, reason) in [
        (1, 15, StopReason::Completed),
        (2, 0, StopReason::ObjectiveBudget),
        (3, 0, StopReason::ObjectiveBudget),
    ] {
        history.push_user(format!("turn {index}"));
        let outcome = run_turn(&mut history, &host).expect("turn");
        assert_eq!(outcome.stop_reason, reason);
        assert_eq!(host.objective_tokens_remaining(), Some(remaining));
        if index == 3 {
            assert_eq!(outcome.steps, 0);
            assert!(outcome.usage.is_empty());
            assert!(outcome.text.is_empty());
        }
    }
    let server = host.server.as_ref().expect("server");
    assert_eq!(server.request_count(), 2);
    assert_eq!(server.last_body().expect("body")["max_tokens"], 15);
    assert_eq!(
        *host.objective_budget.lock().expect("budget"),
        Some((30, 30))
    );
    assert_eq!(
        host.events()
            .iter()
            .filter(|event| matches!(event, Event::Finished { .. }))
            .count(),
        3
    );
}

#[test]
fn exhausted_objective_blocks_tool_and_steering_continuations() {
    for tools in [false, true] {
        let mut script = if tools {
            Script::tool_call("read", "read_file", r#"{"path":"x"}"#)
        } else {
            Script::text("answer")
        };
        if tools && let Script::Frames(frames) = &mut script {
            frames.insert(
                frames.len() - 1,
                r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.to_owned(),
            );
        }
        let endpoint = MockEndpoint::start(vec![script, Script::text("should not be requested")]);
        let host = TestHost::new(endpoint).with_tools(vec![read_tool()]);
        *host.objective_budget.lock().expect("budget") = Some((15, 0));
        if !tools {
            *host.submit_on_first_delta.lock().expect("steering") = Some("also do this".to_owned());
        }
        let mut history = rune_agent::History::new();
        history.push_user("work");
        let outcome = run_turn(&mut history, &host).expect("turn");
        assert_eq!(outcome.stop_reason, StopReason::ObjectiveBudget);
        assert_eq!(outcome.steps, 1);
        assert_eq!(host.server.as_ref().expect("server").request_count(), 1);
        history.validate().expect("paired tool results");
        if tools {
            assert_eq!(outcome.calls.len(), 1);
        } else {
            assert!(
                !host.steering.is_empty(),
                "unanswered steering must remain queued"
            );
        }
    }
}

#[test]
fn missing_usage_and_failed_attempts_cannot_bypass_an_objective_budget() {
    for script in [
        Script::truncated("partial response"),
        Script::Status {
            code: 503,
            body: "unavailable".to_owned(),
        },
        Script::Frames(vec![
            r#"{"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}"#.to_owned(),
            "[DONE]".to_owned(),
        ]),
    ] {
        let host = TestHost::new(MockEndpoint::start(vec![
            script,
            Script::text("unexpected retry"),
        ]));
        *host.objective_budget.lock().expect("budget") = Some((1, 0));
        let mut history = rune_agent::History::new();
        history.push_user("work");
        match run_turn(&mut history, &host) {
            Ok(outcome) => assert_eq!(outcome.stop_reason, StopReason::ObjectiveBudget),
            Err(error) => {
                assert_eq!(error.code(), rune_core::ErrorCode::LimitExceeded);
                assert_eq!(
                    error.detail().invariant.as_deref(),
                    Some("objective_token_budget")
                );
            }
        }
        assert_eq!(host.objective_tokens_remaining(), Some(0));
        assert_eq!(host.server.as_ref().expect("server").request_count(), 1);
        assert!(
            !host
                .events()
                .iter()
                .any(|event| matches!(event, Event::ProviderRetry { .. }))
        );
    }
}

#[test]
fn objective_accounting_preserves_zero_reports_and_does_not_double_count_usage() {
    use rune_agent::turn::ObjectiveAccounting;
    use rune_net::stream::{ProviderEvent, Usage};
    let host = TestHost::new(MockEndpoint::start(vec![Script::text("unused")]));
    *host.objective_budget.lock().expect("budget") = Some((u64::MAX, 0));
    let mut plan = rune_net::provider::RequestPlan::new("test");
    plan.max_output_tokens = Some(42);
    let mut accounting = ObjectiveAccounting::begin(&host, &mut plan).expect("begin");
    assert_eq!(plan.max_output_tokens, Some(42));
    let usage = Usage {
        input_tokens: Some(10),
        output_tokens: Some(5),
        cache_read_tokens: Some(3),
        reasoning_tokens: Some(2),
        ..Usage::default()
    };
    accounting.observe(&ProviderEvent::Usage(usage));
    accounting.observe(&ProviderEvent::Usage(usage));
    accounting.finish(&host, Some(usage)).expect("account");
    assert_eq!(
        *host.objective_budget.lock().expect("budget"),
        Some((u64::MAX, 15))
    );
    let mut accounting = ObjectiveAccounting::begin(&host, &mut plan).expect("begin");
    accounting.observe(&ProviderEvent::TextDelta {
        delta: "not an estimate".to_owned(),
    });
    accounting
        .finish(
            &host,
            Some(Usage {
                input_tokens: Some(0),
                output_tokens: Some(0),
                ..Usage::default()
            }),
        )
        .expect("zero usage");
    assert_eq!(
        *host.objective_budget.lock().expect("budget"),
        Some((u64::MAX, 15))
    );
    host.account_objective_tokens(u64::MAX)
        .expect("saturating charge");
    assert_eq!(host.objective_tokens_remaining(), Some(0));
}

#[test]
fn objective_estimates_count_complete_calls_and_anthropic_cache_tokens() {
    use rune_agent::turn::ObjectiveAccounting;
    use rune_net::stream::{ProviderEvent, Usage};
    let mut host = TestHost::new(MockEndpoint::start(vec![Script::text("unused")]));
    *host.objective_budget.lock().expect("budget") = Some((10_000, 0));
    let mut plan = rune_net::provider::RequestPlan::new("test");
    for streamed in [false, true] {
        let mut accounting = ObjectiveAccounting::begin(&host, &mut plan).expect("begin");
        let id = rune_core::id::ToolCallId::new("call").expect("id");
        accounting.observe(&ProviderEvent::ToolCallStart {
            id: id.clone(),
            name: "read".to_owned(),
        });
        if streamed {
            accounting.observe(&ProviderEvent::ToolCallDelta {
                id: id.clone(),
                delta: "12345678".to_owned(),
            });
        }
        accounting.observe(&ProviderEvent::ToolCallEnd {
            id,
            arguments: "12345678".to_owned(),
        });
        accounting
            .finish(
                &host,
                Some(Usage {
                    input_tokens: Some(10),
                    ..Usage::default()
                }),
            )
            .expect("charge");
    }
    assert_eq!(
        *host.objective_budget.lock().expect("budget"),
        Some((10_000, 26))
    );
    host.dialect = Box::new(rune_net::anthropic::Anthropic);
    plan.max_output_tokens = None;
    let accounting = ObjectiveAccounting::begin(&host, &mut plan).expect("anthropic begin");
    assert_eq!(
        plan.max_output_tokens,
        Some(rune_net::anthropic::DEFAULT_MAX_TOKENS),
        "retain the dialect's default ceiling"
    );
    accounting
        .finish(
            &host,
            Some(Usage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                cache_read_tokens: Some(3),
                cache_write_tokens: Some(7),
                reasoning_tokens: Some(2),
            }),
        )
        .expect("charge caches");
    // The input count already includes the cached parts, as the dialect
    // reports it, so only input and output are charged: 26 + 10 + 5.
    assert_eq!(
        *host.objective_budget.lock().expect("budget"),
        Some((10_000, 41))
    );
    assert_eq!(host.server.as_ref().expect("server").request_count(), 0);
}

#[test]
fn gemini_thinking_tokens_exhaust_the_objective_before_another_request() {
    use rune_agent::turn::ObjectiveAccounting;
    use rune_net::stream::{ProviderEvent, Usage};
    let mut host = TestHost::new(MockEndpoint::start(vec![Script::text("unused")]));
    host.dialect = Box::new(rune_net::gemini::Gemini);
    *host.objective_budget.lock().expect("budget") = Some((1000, 0));
    let mut plan = rune_net::provider::RequestPlan::new("test");
    let mut accounting = ObjectiveAccounting::begin(&host, &mut plan).expect("begin");
    let usage = Usage {
        input_tokens: Some(100),
        output_tokens: Some(10),
        reasoning_tokens: Some(1000),
        ..Usage::default()
    };
    accounting.observe(&ProviderEvent::Usage(usage));
    accounting.finish(&host, Some(usage)).expect("charge once");
    assert_eq!(
        *host.objective_budget.lock().expect("budget"),
        Some((1000, 1110))
    );
    let mut history = rune_agent::History::new();
    history.push_user("continue");
    let outcome = run_turn(&mut history, &host).expect("budget stop");
    assert_eq!(outcome.stop_reason, StopReason::ObjectiveBudget);
    assert_eq!(host.server.as_ref().expect("server").request_count(), 0);
}

#[test]
fn a_known_output_ceiling_is_requested_only_where_the_dialect_needs_one() {
    let cases = [
        (
            true,
            Some(200_000),
            Some(rune_agent::turn::MAX_REQUESTED_OUTPUT_TOKENS),
        ),
        (true, Some(16_000), Some(16_000)),
        (true, None, None),
        (false, Some(200_000), None),
    ];
    for (anthropic, known, expected) in cases {
        let mut host = TestHost::new(MockEndpoint::start(vec![Script::text("done")]));
        host.max_output = known;
        if anthropic {
            host.dialect = Box::new(rune_net::anthropic::Anthropic);
        }
        let mut history = rune_agent::history::History::new();
        history.push_user("hello");
        // The mock speaks one dialect, so the Anthropic attempt may fail after
        // preparation; only the planned ceiling is under test.
        let _ = run_turn(&mut history, &host);
        let planned = host.planned_output.lock().expect("planned").clone();
        assert_eq!(
            planned.first().copied(),
            Some(expected),
            "{anthropic} {known:?}"
        );
    }
}

#[test]
fn an_unchecked_change_is_sent_back_once_for_verification() {
    for (reminders, expected_requests) in [(1, 3), (0, 2)] {
        let endpoint = MockEndpoint::start(vec![
            Script::tool_call("w", "write_file", "{\"path\":\"a.txt\",\"content\":\"x\"}"),
            Script::text("Done."),
            Script::text("Nothing here can run a check."),
        ]);
        let mut host = TestHost::new(endpoint)
            .with_tools(vec![ToolSpec {
                name: "write_file".to_owned(),
                description: "Write a file.".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
            }])
            .with_tool_response(ToolOutput::success("Wrote `a.txt` (new file, 1 line)"));
        host.limits
            .set(
                rune_core::LimitName::VerificationReminders,
                rune_core::budget::Budget::Bounded(reminders),
                rune_core::config::Layer::User,
            )
            .expect("reminder limit");
        let mut history = rune_agent::History::new();
        history.push_user("write a file");
        let outcome = run_turn(&mut history, &host).expect("turn");

        let server = host.server.as_ref().expect("server");
        assert_eq!(server.request_count(), expected_requests, "{reminders}");
        let asked = host
            .events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| matches!(event, Event::VerificationRequested))
            .count();
        assert_eq!(asked, usize::try_from(reminders).expect("count"));
        // Nothing checked the change, and the reminder is not repeated.
        assert_eq!(
            outcome.verification,
            rune_agent::turn::Verification::Unverified
        );
    }
}

#[test]
fn a_command_after_the_last_change_counts_as_a_check() {
    use rune_agent::turn::{CallResult, PreparedCall, Verification};
    let call = |name: &str, arguments: &str| CallResult {
        call: PreparedCall {
            id: name.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        },
        output: ToolOutput::success("ok"),
        executed: true,
    };
    // A finished command reports how it ended; the verdict reads that.
    let finished = |mut result: CallResult| {
        result.output.check = Some(rune_tools::CheckRun {
            exit_code: Some(0),
            ..rune_tools::CheckRun::default()
        });
        result
    };
    let edit = call("edit_file", "{}");
    let test = finished(call("shell", r#"{"action":"run","command":"npm test"}"#));
    let read = finished(call("shell", r#"{"action":"run","command":"cat a.js"}"#));
    assert_eq!(
        Verification::of(std::slice::from_ref(&test)),
        Verification::NotApplicable
    );
    assert_eq!(
        Verification::of(&[edit.clone(), test.clone()]),
        Verification::Verified
    );
    assert_eq!(
        Verification::of(&[edit.clone(), read]),
        Verification::Unverified
    );
    assert_eq!(Verification::of(&[test, edit]), Verification::Unverified);
}

fn repeat_pointers(host: &TestHost) -> usize {
    host.events()
        .iter()
        .filter(|event| {
            matches!(event, Event::ToolFinished { output, .. }
                if output.text.starts_with(rune_agent::turn::REPEAT_MARKER))
        })
        .count()
}

#[test]
fn a_repeated_read_is_answered_from_the_earlier_result_once() {
    let directory = tempfile::tempdir().expect("workspace");
    let file = directory.path().join("lib.rs");
    std::fs::write(&file, "fn main() {}").expect("file");
    let arguments = &serde_json::json!({"path": file}).to_string();
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("first", "read_file", arguments),
        Script::tool_call("second", "read_file", arguments),
        Script::tool_call("third", "read_file", arguments),
        Script::text("done"),
    ]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(ToolOutput::success("1\tfn main() {}"));
    let mut history = rune_agent::History::new();
    history.push_user("read it");
    let outcome = run_turn(&mut history, &host).expect("turn");
    // The second call is answered by a pointer; a third look runs again.
    assert_eq!(host.executed_calls().len(), 2);
    assert_eq!(repeat_pointers(&host), 1);
    assert!(!outcome.calls[1].executed);
    assert!(outcome.calls[1].output.text.contains("`first`"));
}

#[test]
fn a_repeated_read_runs_again_after_its_result_was_pruned() {
    use std::fmt::Write as _;
    let directory = tempfile::tempdir().expect("workspace");
    let file = directory.path().join("lib.rs");
    std::fs::write(&file, "fn main() {}").expect("file");
    let arguments = &serde_json::json!({"path": file}).to_string();
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("first", "read_file", arguments),
        Script::text("read"),
        Script::tool_call("second", "read_file", arguments),
        Script::text("done"),
    ]);
    let mut numbered = String::new();
    for line in 1..=40 {
        let _ = writeln!(numbered, "{line:>6}\tfn main() {{}} // padding for size");
    }
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(ToolOutput::success(numbered));
    let mut history = rune_agent::History::new();
    history.push_user("read it");
    run_turn(&mut history, &host).expect("first turn");

    history.push_user(format!("now read it again {}", "x".repeat(130_000)));
    let report =
        rune_agent::prune::prune(&mut history, &rune_net::chat_completions::ChatCompletions);
    assert_eq!(report.results, 1);
    assert!(history.is_pruned("first"));

    let outcome = run_turn(&mut history, &host).expect("second turn");
    assert_eq!(host.executed_calls().len(), 2);
    assert_eq!(repeat_pointers(&host), 0);
    assert!(outcome.calls[0].executed);
}

#[test]
fn a_change_between_identical_searches_lets_the_second_run() {
    let arguments = r#"{"pattern":"fn main"}"#;
    let mut grep = read_tool();
    grep.name = "grep_files".to_owned();
    let mut edit = read_tool();
    edit.name = "edit_file".to_owned();
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("first", "grep_files", arguments),
        Script::tool_call("change", "edit_file", r#"{"path":"src/lib.rs"}"#),
        Script::tool_call("second", "grep_files", arguments),
        Script::text("done"),
    ]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![grep, edit])
        .with_tool_response(ToolOutput::success("ok"));
    let mut history = rune_agent::History::new();
    history.push_user("read, edit, read");
    run_turn(&mut history, &host).expect("turn");
    assert_eq!(host.executed_calls().len(), 3);
    assert_eq!(repeat_pointers(&host), 0);
}

#[test]
fn the_turn_loop_records_changes_in_the_ledger() {
    let test = r#"{"action":"run","command":"npm test"}"#;
    let mut edit = read_tool();
    edit.name = "edit_file".to_owned();
    let mut shell = read_tool();
    shell.name = "shell".to_owned();
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("change", "edit_file", r#"{"path":"src/lib.rs"}"#),
        Script::tool_call("check", "shell", test),
        Script::text("done"),
    ]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![edit, shell])
        .with_tool_response(ToolOutput::success("ok\n[exited with status 0]"));
    let mut history = rune_agent::History::new();
    history.push_user("edit, then test");
    run_turn(&mut history, &host).expect("turn");
    let ledger = history.ledger();
    assert_eq!(ledger.changed.len(), 1);
    assert_eq!(ledger.changed[0].path, "src/lib.rs");
    assert_eq!(ledger.checks.len(), 1);
    assert_eq!(ledger.checks[0].command, "npm test");
    assert_eq!(ledger.checks[0].exit, Some(0));
    assert!(ledger.settled);
    let check_turn = history
        .turns()
        .iter()
        .find(|turn| {
            turn.parts.iter().any(|part| {
                matches!(part, rune_net::message::ContentPart::ToolResult { id, .. } if id.as_str() == "check")
            })
        })
        .map(|turn| turn.seq);
    assert!(check_turn.is_some());
    assert_eq!(history.settled_turn(), check_turn);
}

#[test]
fn identical_reads_in_one_batch_run_once() {
    let directory = tempfile::tempdir().expect("workspace");
    let file = directory.path().join("lib.rs");
    std::fs::write(&file, "text").expect("file");
    let arguments = &serde_json::json!({"path": file}).to_string();
    let endpoint = MockEndpoint::start(vec![
        failure_batch(&[
            ("first", "read_file", arguments),
            ("other", "read_file", r#"{"path":"README.md"}"#),
            ("second", "read_file", arguments),
        ]),
        Script::text("done"),
    ]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(ToolOutput::success("text"));
    let mut history = rune_agent::History::new();
    history.push_user("read twice");
    run_turn(&mut history, &host).expect("turn");
    assert_eq!(host.executed_calls().len(), 2);
    assert_eq!(repeat_pointers(&host), 1);
}

#[test]
fn a_shortened_result_names_a_handle_to_its_full_text() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("run", "read_file", r#"{"path":"log"}"#),
        Script::text("done"),
    ]);
    let mut output = ToolOutput::success("short view");
    output.full_text = Some("the complete log".to_owned());
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(output);
    let mut history = rune_agent::History::new();
    history.push_user("run");
    let outcome = run_turn(&mut history, &host).expect("turn");
    let text = &outcome.calls[0].output.text;
    assert!(
        text.starts_with("short view\n[full log: read_tool_result handle "),
        "{text}"
    );
    assert_eq!(history.result_store().len(), 1);
}

fn cut_off_empty() -> Script {
    Script::Frames(vec![
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#.to_owned(),
        "[DONE]".to_owned(),
    ])
}

#[test]
fn an_empty_reply_cut_by_the_output_limit_is_asked_for_once_more() {
    let endpoint = MockEndpoint::start(vec![cut_off_empty(), Script::text("short answer")]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("answer");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(outcome.text, "short answer");
    assert_eq!(host.server.as_ref().expect("endpoint").request_count(), 2);
    history.validate().expect("roles alternate");
}

#[test]
fn a_second_empty_cut_off_reports_the_output_limit() {
    let endpoint = MockEndpoint::start(vec![cut_off_empty(), cut_off_empty()]);
    let host = TestHost::new(endpoint);
    let mut history = rune_agent::History::new();
    history.push_user("answer");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::OutputLimit);
    assert_eq!(host.server.as_ref().expect("endpoint").request_count(), 2);
}

#[test]
fn the_outcome_keeps_text_and_calls_in_the_order_they_happened() {
    let endpoint = MockEndpoint::start(vec![
        Script::Frames(vec![
            r#"{"choices":[{"index":0,"delta":{"content":"Let me look."}}]}"#.to_owned(),
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}]}}]}"#.to_owned(),
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned(),
            "[DONE]".to_owned(),
        ]),
        Script::text("It is fine."),
    ]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(ToolOutput::success("1\tok"));
    let mut history = rune_agent::History::new();
    history.push_user("look");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(
        outcome.narration,
        [
            rune_agent::turn::Narration::Text("Let me look.".to_owned()),
            rune_agent::turn::Narration::Call("c1".to_owned()),
            rune_agent::turn::Narration::Text("It is fine.".to_owned()),
        ]
    );
}

#[test]
fn a_read_in_a_later_turn_points_back_while_the_file_is_unchanged() {
    let directory = tempfile::tempdir().expect("workspace");
    let file = directory.path().join("notes.txt");
    std::fs::write(&file, "one").expect("file");
    let arguments = &serde_json::json!({"path": file}).to_string();
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("first", "read_file", arguments),
        Script::text("read"),
        Script::tool_call("again", "read_file", arguments),
        Script::text("same"),
        Script::tool_call("after_change", "read_file", arguments),
        Script::text("changed"),
    ]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![read_tool()])
        .with_tool_response(ToolOutput::success("1\tone"));
    let mut history = rune_agent::History::new();
    for prompt in ["read", "read again"] {
        history.push_user(prompt);
        run_turn(&mut history, &host).expect("turn");
    }
    assert_eq!(host.executed_calls().len(), 1);
    assert_eq!(repeat_pointers(&host), 1);
    // Someone edits the file between turns, so the next read runs.
    std::fs::write(&file, "two").expect("edit");
    history.push_user("read after the edit");
    run_turn(&mut history, &host).expect("turn");
    assert_eq!(host.executed_calls().len(), 2);
    assert_eq!(repeat_pointers(&host), 1);
}

#[test]
fn a_repeated_test_command_with_nothing_changed_is_answered_from_the_last_run() {
    let _quiet = BACKGROUND_WORK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let test = r#"{"action":"run","command":"cargo test -p core 2>&1 | tail -20"}"#;
    let sed = r#"{"action":"run","command":"sed -i 's/a/aa/' notes.txt"}"#;
    let mut shell = read_tool();
    shell.name = "shell".to_owned();
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("first", "shell", test),
        Script::tool_call("same", "shell", test),
        Script::tool_call("edit", "shell", sed),
        Script::tool_call("edit_again", "shell", sed),
        Script::tool_call("after_edit", "shell", test),
        Script::text("done"),
    ]);
    let host = TestHost::new(endpoint)
        .with_tools(vec![shell])
        .with_tool_response(ToolOutput::failure("[tests: 3 passed, 1 failed]"));
    let mut history = rune_agent::History::new();
    history.push_user("test");
    let outcome = run_turn(&mut history, &host).expect("turn");
    // Only `same` is skipped: the failing run is the answer. Writes always run,
    // and a test after a write runs again.
    assert_eq!(host.executed_calls().len(), 4);
    assert_eq!(repeat_pointers(&host), 1);
    assert!(!outcome.calls[1].executed);
}

/// Runs one `apply_patch` call with the real tools and the given rules.
fn run_patch(
    workspace: &tempfile::TempDir,
    patch: &str,
    rules: RuleSet,
) -> (TestHost, rune_agent::turn::TurnOutcome) {
    let arguments = serde_json::json!({ "input": patch }).to_string();
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("patch", "apply_patch", &arguments),
        Script::text("done"),
    ]);
    let registry = rune_tools::inventory::builtin_default().expect("builtins");
    let mut tools = registry.all_schemas();
    tools.push(rune_tools::contract::model_spec(
        &rune_tools::apply_patch::ApplyPatch::new(true),
    ));
    let mut host = TestHost::new(endpoint).with_tools(tools).with_rules(rules);
    host.workspace = Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path");
    host.registry = Some(registry);
    let mut history = rune_agent::History::new();
    history.push_user("apply the patch");
    let outcome = run_turn(&mut history, &host).expect("turn");
    (host, outcome)
}

#[test]
fn each_patch_path_is_decided_as_an_edit_or_a_write() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a.txt"), "one\n").expect("fixture");
    std::fs::write(workspace.path().join("b.txt"), "two\n").expect("fixture");
    let patch = "*** Begin Patch
*** Update File: a.txt
*** Move to: c.txt
-one
+1
*** Add File: d.txt
+new
*** Delete File: b.txt
*** End Patch";
    let mut rules = RuleSet::new();
    rules.push(Rule::allow("edit_file", "*", Layer::User));
    rules.push(Rule::allow("write_file", "*", Layer::User));
    let (host, outcome) = run_patch(&workspace, patch, rules);
    assert!(
        !outcome.calls[0].output.is_error,
        "{}",
        outcome.calls[0].output.text
    );
    assert_eq!(
        *host.decisions.lock().expect("lock"),
        [
            ("edit_file".to_owned(), Some("a.txt".to_owned())),
            ("write_file".to_owned(), Some("c.txt".to_owned())),
            ("write_file".to_owned(), Some("d.txt".to_owned())),
            ("edit_file".to_owned(), Some("b.txt".to_owned())),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("c.txt")).expect("moved"),
        "1\n"
    );
    assert!(!workspace.path().join("a.txt").exists());
    assert!(!workspace.path().join("b.txt").exists());
    assert!(workspace.path().join("d.txt").exists());

    // A deny rule on an edit refuses the whole patch before anything is written.
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir(workspace.path().join("secret")).expect("dir");
    std::fs::write(workspace.path().join("secret/a"), "keep\n").expect("fixture");
    std::fs::write(workspace.path().join("open.txt"), "x\n").expect("fixture");
    let patch = "*** Begin Patch
*** Update File: open.txt
-x
+y
*** Update File: secret/a
-keep
+lost
*** End Patch";
    let mut rules = RuleSet::new();
    rules.push(Rule::allow("edit_file", "*", Layer::Default));
    rules.push(Rule::deny("edit_file", "secret/*", Layer::User));
    let (host, outcome) = run_patch(&workspace, patch, rules);
    let result = &outcome.calls[0];
    assert!(result.output.is_error);
    assert!(!result.executed);
    assert!(
        result
            .output
            .text
            .starts_with("patch path `secret/a` was not approved:"),
        "{}",
        result.output.text
    );
    assert!(host.executed_calls().is_empty());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("open.txt")).expect("read"),
        "x\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("secret/a")).expect("read"),
        "keep\n"
    );
}

#[test]
fn a_patch_counts_as_a_change_for_verification() {
    use rune_agent::turn::{CallResult, PreparedCall, Verification};
    let call = |name: &str, arguments: &str| CallResult {
        call: PreparedCall {
            id: name.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        },
        output: ToolOutput::success("ok"),
        executed: true,
    };
    let patch = call("apply_patch", "{}");
    let mut test = call("shell", r#"{"action":"run","command":"npm test"}"#);
    test.output.check = Some(rune_tools::CheckRun {
        exit_code: Some(0),
        ..rune_tools::CheckRun::default()
    });
    assert_eq!(
        Verification::of(std::slice::from_ref(&patch)),
        Verification::Unverified
    );
    assert_eq!(Verification::of(&[patch, test]), Verification::Verified);
}

/// Standard base64 with padding, for comparing request bodies.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> shift) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[test]
fn an_image_read_reaches_the_next_request() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&[0x00, 0x00, 0x00, 0x0D]);
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&1280_u32.to_be_bytes());
    png.extend_from_slice(&720_u32.to_be_bytes());
    png.extend_from_slice(&[0x5A; 300]);
    std::fs::write(workspace.path().join("shot.png"), &png).expect("fixture");
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("look", "read_file", r#"{"path":"shot.png"}"#),
        Script::text("A wide screenshot."),
    ]);
    let mut registry = Registry::new();
    registry
        .insert(Box::new(rune_tools::ReadFile::new()))
        .expect("registered");
    let mut host = TestHost::new(endpoint).with_tools(registry.all_schemas());
    host.workspace = Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path");
    host.registry = Some(registry);
    host.accepts_images = true;

    let mut history = rune_agent::History::new();
    history.push_user("what is in shot.png");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    assert_eq!(
        outcome.calls[0].output.text,
        format!("image shot.png (1280x720, {} bytes) attached", png.len())
    );

    let server = host.server.as_ref().expect("server");
    assert_eq!(server.request_count(), 2);
    let body = server.last_body().expect("second request").to_string();
    assert_eq!(body.matches(&base64(&png)).count(), 1, "{body}");
    assert!(
        body.contains("Images from the tool results above."),
        "{body}"
    );
}

/// A host running the real file tools in `workspace` with reads and changes allowed.
fn file_host(workspace: &tempfile::TempDir, scripts: Vec<Script>) -> TestHost {
    let endpoint = MockEndpoint::start(scripts);
    let registry = rune_tools::inventory::builtin_default().expect("builtins");
    let mut rules = RuleSet::new();
    for tool in ["read_file", "edit_file", "write_file", "rename_file"] {
        rules.push(Rule::allow(tool, "*", Layer::User));
    }
    let mut host = TestHost::new(endpoint)
        .with_tools(registry.all_schemas())
        .with_rules(rules);
    host.workspace = Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path");
    host.registry = Some(registry);
    host
}

fn read_call(id: &str, path: &str) -> Script {
    Script::tool_call(
        id,
        "read_file",
        &serde_json::json!({ "path": path }).to_string(),
    )
}

fn edit_call(id: &str, path: &str, old: &str, new: &str) -> Script {
    Script::tool_call(
        id,
        "edit_file",
        &serde_json::json!({ "path": path, "old_string": old, "new_string": new }).to_string(),
    )
}

fn write_call(id: &str, path: &str, content: &str) -> Script {
    Script::tool_call(
        id,
        "write_file",
        &serde_json::json!({ "path": path, "content": content }).to_string(),
    )
}

const STALE_NOTICE: &str = "[the file had changed on disk since you last read it]";

#[test]
fn an_edit_after_an_outside_change_says_the_file_had_changed() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a.rs"), "one\ntwo\n").expect("fixture");
    let host = file_host(
        &workspace,
        vec![
            read_call("read", "a.rs"),
            Script::text("read"),
            edit_call("edit", "a.rs", "two", "2"),
            Script::text("done"),
        ],
    );
    let mut history = rune_agent::History::new();
    history.push_user("read it");
    run_turn(&mut history, &host).expect("first turn");
    std::fs::write(workspace.path().join("a.rs"), "ONE\ntwo\n").expect("outside change");
    history.push_user("now edit it");
    let outcome = run_turn(&mut history, &host).expect("second turn");
    let edit = &outcome.calls[0];
    assert!(!edit.output.is_error, "{}", edit.output.text);
    assert!(
        edit.output.text.ends_with(STALE_NOTICE),
        "{}",
        edit.output.text
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("a.rs")).expect("read"),
        "ONE\n2\n"
    );
}

#[test]
fn an_edit_of_an_unchanged_read_file_adds_no_notice() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a.rs"), "one\ntwo\n").expect("fixture");
    std::fs::write(workspace.path().join("b.rs"), "three\n").expect("fixture");
    let host = file_host(
        &workspace,
        vec![
            read_call("read", "a.rs"),
            edit_call("edit", "a.rs", "two", "2"),
            edit_call("again", "a.rs", "2", "II"),
            edit_call("unread", "b.rs", "three", "3"),
            Script::text("done"),
        ],
    );
    let mut history = rune_agent::History::new();
    history.push_user("read and edit");
    let outcome = run_turn(&mut history, &host).expect("turn");
    for call in &outcome.calls[1..] {
        assert!(!call.output.is_error, "{}", call.output.text);
        assert!(
            !call.output.text.contains("changed on disk"),
            "{}",
            call.output.text
        );
    }
}

#[test]
fn writing_over_an_unread_file_is_refused_once_then_allowed() {
    let workspace = tempfile::tempdir().expect("workspace");
    let original = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n";
    std::fs::write(workspace.path().join("notes.txt"), original).expect("fixture");
    let host = file_host(
        &workspace,
        vec![
            write_call("w1", "notes.txt", "new\n"),
            Script::text("refused"),
            write_call("w2", "notes.txt", "new\n"),
            Script::text("done"),
        ],
    );
    let mut history = rune_agent::History::new();
    history.push_user("replace the notes");
    let first = run_turn(&mut history, &host).expect("first turn");
    assert!(!first.calls[0].executed);
    assert_eq!(
        first.calls[0].output.text,
        "`notes.txt` already exists (12 lines) and has not been read in this session. Read it \
         first, or send this same call again to overwrite it."
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes.txt")).expect("read"),
        original
    );
    history.push_user("yes, overwrite it");
    let second = run_turn(&mut history, &host).expect("second turn");
    assert!(second.calls[0].executed);
    assert!(
        !second.calls[0].output.is_error,
        "{}",
        second.calls[0].output.text
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes.txt")).expect("read"),
        "new\n"
    );
}

#[test]
fn writing_over_a_file_the_agent_created_is_not_refused() {
    let workspace = tempfile::tempdir().expect("workspace");
    let host = file_host(
        &workspace,
        vec![
            write_call("create", "new.txt", "first\n"),
            write_call("replace", "new.txt", "second\n"),
            Script::text("done"),
        ],
    );
    let mut history = rune_agent::History::new();
    history.push_user("write twice");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert!(
        outcome
            .calls
            .iter()
            .all(|call| call.executed && !call.output.is_error)
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("new.txt")).expect("read"),
        "second\n"
    );
}

#[test]
fn writing_over_an_empty_file_is_not_refused() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("empty.txt"), "").expect("fixture");
    let host = file_host(
        &workspace,
        vec![write_call("w", "empty.txt", "text\n"), Script::text("done")],
    );
    let mut history = rune_agent::History::new();
    history.push_user("fill it");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert!(outcome.calls[0].executed && !outcome.calls[0].output.is_error);
}

#[test]
fn a_renamed_file_keeps_its_record() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a.rs"), "fn a() {}\n").expect("fixture");
    let host = file_host(
        &workspace,
        vec![
            read_call("read", "./a.rs"),
            Script::tool_call(
                "rename",
                "rename_file",
                &serde_json::json!({ "path": "a.rs", "new_path": "b.rs" }).to_string(),
            ),
            write_call("write", "b.rs", "fn b() {}\n"),
            Script::text("done"),
        ],
    );
    let mut history = rune_agent::History::new();
    history.push_user("read, rename, rewrite");
    let outcome = run_turn(&mut history, &host).expect("turn");
    for call in &outcome.calls {
        assert!(
            call.executed && !call.output.is_error,
            "{}",
            call.output.text
        );
    }
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("b.rs")).expect("read"),
        "fn b() {}\n"
    );
}

#[test]
fn a_write_policy_refuses_is_left_to_policy_by_the_overwrite_guard() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("notes.txt"), "keep\n").expect("fixture");
    let endpoint = MockEndpoint::start(vec![
        write_call("w", "notes.txt", "new\n"),
        Script::text("refused"),
    ]);
    let registry = rune_tools::inventory::builtin_default().expect("builtins");
    let mut rules = RuleSet::new();
    rules.push(Rule::deny("write_file", "*", Layer::User));
    let mut host = TestHost::new(endpoint)
        .with_tools(registry.all_schemas())
        .with_rules(rules);
    host.workspace = Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path");
    host.registry = Some(registry);
    let mut history = rune_agent::History::new();
    history.push_user("overwrite it");
    let outcome = run_turn(&mut history, &host).expect("turn");
    let text = &outcome.calls[0].output.text;
    assert!(text.contains("refused by policy"), "{text}");
    assert!(!text.contains("has not been read"), "{text}");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes.txt")).expect("read"),
        "keep\n"
    );
}

fn builtin_specs(names: &[&str]) -> Vec<ToolSpec> {
    rune_tools::inventory::builtin_default()
        .expect("registry")
        .schemas(names)
}

/// Returns the recorded tool calls and results as (kind, id, name, text).
fn recorded_tool_parts(
    history: &rune_agent::History,
) -> Vec<(&'static str, String, String, String)> {
    history
        .turns()
        .iter()
        .flat_map(|turn| turn.parts.iter())
        .filter_map(|part| match part {
            rune_net::message::ContentPart::ToolCall {
                id,
                name,
                arguments,
            } => Some(("call", id.to_string(), name.clone(), arguments.clone())),
            rune_net::message::ContentPart::ToolResult {
                id, name, content, ..
            } => Some(("result", id.to_string(), name.clone(), content.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_bash_call_runs_as_shell_and_says_so() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("c1", "bash", r#"{"command":"ls"}"#),
        Script::text("done"),
    ]);
    let host = TestHost::new(endpoint).with_tools(builtin_specs(&["shell", "read_file"]));
    let mut history = rune_agent::History::new();
    history.push_user("list the files");

    let outcome = run_turn(&mut history, &host).expect("turn");
    let note = "[note: there is no tool named `bash`; this ran as `shell`.]";
    assert_eq!(
        host.executed_calls(),
        vec![(
            "shell".to_owned(),
            serde_json::json!({ "command": "ls", "action": "run" })
        )]
    );
    assert_eq!(outcome.calls[0].call.name, "shell");
    assert!(outcome.calls[0].executed);
    assert_eq!(outcome.calls[0].output.text, format!("tool output\n{note}"));
    assert_eq!(outcome.repaired_calls, 1);
    assert_eq!(
        outcome.repairs,
        vec![rune_agent::turn::CallRepair {
            id: "c1".to_owned(),
            from: "bash".to_owned(),
            to: "shell".to_owned(),
            repairs: Vec::new(),
        }]
    );
    let events = host.events();
    let repaired = events
        .iter()
        .position(|event| {
            matches!(event, Event::CallRepaired { from, to, .. } if from == "bash" && to == "shell")
        })
        .expect("a repair event");
    let started = events
        .iter()
        .position(|event| matches!(event, Event::ToolStarted { .. }))
        .expect("a started event");
    assert!(repaired < started);

    // The next request shows the call as it ran, next to its result.
    let body = host
        .server
        .as_ref()
        .expect("server")
        .last_body()
        .expect("request body");
    let messages = body["messages"].as_array().expect("messages");
    let assistant = messages
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("assistant message");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "shell");
    let arguments: serde_json::Value = serde_json::from_str(
        assistant["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments"),
    )
    .expect("json");
    assert_eq!(
        arguments,
        serde_json::json!({ "command": "ls", "action": "run" })
    );
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("tool message");
    assert!(
        result["content"].as_str().expect("content").ends_with(note),
        "{result}"
    );
    history.validate().expect("paired history");
}

#[test]
fn a_repaired_call_is_judged_by_its_real_name() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("c1", "bash", r#"{"command":"rm -rf build"}"#),
        Script::text("it was refused"),
    ]);
    let mut rules = RuleSet::new();
    rules.push(Rule::deny("shell", "*", Layer::User));
    let host = TestHost::new(endpoint)
        .with_tools(builtin_specs(&["shell"]))
        .with_rules(rules);
    let mut history = rune_agent::History::new();
    history.push_user("clean the build");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert!(host.executed_calls().is_empty(), "a denied call ran");
    assert!(!outcome.calls[0].executed);
    assert_eq!(outcome.calls[0].call.name, "shell");
    let text = &outcome.calls[0].output.text;
    assert!(text.starts_with("`shell` was refused by policy"), "{text}");
    assert!(
        text.ends_with("[note: there is no tool named `bash`; this ran as `shell`.]"),
        "{text}"
    );
    let decisions = host.decisions.lock().expect("lock").clone();
    assert_eq!(decisions[0].0, "shell");
    let denied = host.events().into_iter().find_map(|event| match event {
        Event::ToolDenied { call, .. } => Some(call.name),
        _ => None,
    });
    assert_eq!(denied.as_deref(), Some("shell"));
}

#[test]
fn stringly_typed_arguments_are_coerced_and_reported() {
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call(
            "c1",
            "read_file",
            r#"{"path":"a.rs","start_line":"2","line_count":"3"}"#,
        ),
        Script::text("read"),
    ]);
    let host = TestHost::new(endpoint).with_tools(builtin_specs(&["read_file"]));
    let mut history = rune_agent::History::new();
    history.push_user("read part of a.rs");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(
        host.executed_calls(),
        vec![(
            "read_file".to_owned(),
            serde_json::json!({ "path": "a.rs", "start_line": 2, "line_count": 3 })
        )]
    );
    assert!(
        outcome.calls[0]
            .output
            .text
            .ends_with("[note: Repaired: `start_line` was a string; `line_count` was a string.]"),
        "{}",
        outcome.calls[0].output.text
    );
    assert_eq!(outcome.repaired_calls, 1);
    assert_eq!(outcome.repairs[0].from, "read_file");
    assert_eq!(outcome.repairs[0].to, "read_file");
    let recorded = recorded_tool_parts(&history);
    assert_eq!(
        recorded[0].3, r#"{"path":"a.rs","start_line":2,"line_count":3}"#,
        "the recorded call shows the arguments it ran with"
    );
}

#[test]
fn a_repaired_call_beside_signed_reasoning_is_not_rewritten() {
    let sent = r#"{"command":"ls"}"#;
    let tool_use = Script::Frames(vec![
        r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#.to_owned(),
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":"sig"}}"#.to_owned(),
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"list them"}}"#.to_owned(),
        r#"{"type":"content_block_stop","index":0}"#.to_owned(),
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash","input":{}}}"#.to_owned(),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": { "type": "input_json_delta", "partial_json": sent },
        })
        .to_string(),
        r#"{"type":"content_block_stop","index":1}"#.to_owned(),
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#.to_owned(),
        r#"{"type":"message_stop"}"#.to_owned(),
    ]);
    let answer = Script::Frames(vec![
        r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#.to_owned(),
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_owned(),
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}"#.to_owned(),
        r#"{"type":"content_block_stop","index":0}"#.to_owned(),
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#.to_owned(),
        r#"{"type":"message_stop"}"#.to_owned(),
    ]);
    let endpoint = MockEndpoint::start(vec![tool_use, answer]);
    let mut host = TestHost::new(endpoint).with_tools(builtin_specs(&["shell"]));
    host.dialect = Box::new(rune_net::anthropic::Anthropic);
    let mut history = rune_agent::History::new();
    history.push_user("list the files");

    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(outcome.text, "done");
    assert_eq!(host.executed_calls()[0].0, "shell");
    assert_eq!(outcome.calls[0].call.name, "shell");
    assert_eq!(outcome.repaired_calls, 1);
    let recorded = recorded_tool_parts(&history);
    assert_eq!(
        recorded[0],
        (
            "call",
            "toolu_1".to_owned(),
            "bash".to_owned(),
            sent.to_owned()
        )
    );
    assert_eq!(recorded[1].0, "result");
    assert_eq!(recorded[1].2, "bash", "the result keeps the recorded name");
    assert!(
        recorded[1]
            .3
            .ends_with("[note: there is no tool named `bash`; this ran as `shell`.]"),
        "{}",
        recorded[1].3
    );
    history.validate().expect("paired history");
}

#[test]
fn a_repeated_aliased_failure_is_refused_like_the_call_it_runs_as() {
    let arguments = r#"{"command":"make broken"}"#;
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("c1", "bash", arguments),
        Script::tool_call("c2", "bash", arguments),
        Script::tool_call("c3", "bash", arguments),
        Script::tool_call("c4", "bash", arguments),
        Script::text("the build is broken"),
    ]);
    let mut host = TestHost::new(endpoint)
        .with_tools(builtin_specs(&["shell"]))
        .with_tool_response(ToolOutput::failure("make: *** [broken] Error 2"));
    set_failure_cap(&mut host, rune_core::budget::Budget::Bounded(3));
    let mut history = rune_agent::History::new();
    history.push_user("build it");
    let outcome = run_turn(&mut history, &host).expect("the turn goes on");
    assert_eq!(host.executed_calls().len(), 3);
    assert!(!outcome.calls[3].executed);
    assert!(
        outcome.calls[3]
            .output
            .text
            .starts_with("not run: this exact call has failed 3 times in a row."),
        "{}",
        outcome.calls[3].output.text
    );
    history.validate().expect("paired history");
}

/// Runs one `shell` call whose command is an apply_patch heredoc, with the real tools.
fn heredoc_turn(
    workspace: &tempfile::TempDir,
    rules: RuleSet,
) -> (TestHost, rune_agent::turn::TurnOutcome) {
    let command = "apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: a.txt\n-old\n+new\n*** End Patch\nEOF";
    let arguments = serde_json::json!({ "action": "run", "command": command }).to_string();
    let endpoint = MockEndpoint::start(vec![
        Script::tool_call("p", "shell", &arguments),
        Script::text("done"),
    ]);
    let registry = rune_tools::inventory::builtin_default().expect("builtins");
    let mut host = TestHost::new(endpoint)
        .with_tools(registry.all_schemas())
        .with_rules(rules);
    host.workspace = Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path");
    host.registry = Some(registry);
    let mut history = rune_agent::History::new();
    history.push_user("patch it");
    let outcome = run_turn(&mut history, &host).expect("turn");
    history.validate().expect("paired history");
    (host, outcome)
}

#[test]
fn a_shell_heredoc_patch_runs_as_apply_patch() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a.txt"), "old\n").expect("fixture");
    let mut rules = RuleSet::new();
    rules.push(Rule::allow("edit_file", "*", Layer::User));
    let (host, outcome) = heredoc_turn(&workspace, rules);
    let result = &outcome.calls[0];
    assert!(
        result.executed && !result.output.is_error,
        "{}",
        result.output.text
    );
    assert_eq!(result.call.name, "apply_patch");
    assert!(
        result
            .output
            .text
            .ends_with("\n[ran as the apply_patch tool]"),
        "{}",
        result.output.text
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("a.txt")).expect("read"),
        "new\n"
    );
    assert_eq!(
        *host.decisions.lock().expect("lock"),
        [("edit_file".to_owned(), Some("a.txt".to_owned()))]
    );
    assert_eq!(outcome.repaired_calls, 0);
    // The model's call is recorded as it sent it, and its result keeps the name.
    let second = host
        .server
        .as_ref()
        .expect("server")
        .last_body()
        .expect("second request");
    let messages = second["messages"].as_array().expect("messages");
    assert!(
        messages
            .iter()
            .any(|message| message["tool_calls"][0]["function"]["name"] == "shell"),
        "{second}"
    );
}

#[test]
fn a_deny_rule_refuses_an_intercepted_patch() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a.txt"), "old\n").expect("fixture");
    let mut rules = RuleSet::new();
    rules.push(Rule::allow("edit_file", "*", Layer::Default));
    rules.push(Rule::deny("edit_file", "a.txt", Layer::User));
    let (_host, outcome) = heredoc_turn(&workspace, rules);
    let result = &outcome.calls[0];
    assert!(!result.executed);
    assert!(
        result
            .output
            .text
            .starts_with("patch path `a.txt` was not approved:"),
        "{}",
        result.output.text
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("a.txt")).expect("read"),
        "old\n"
    );
}

/// Starts `sleep 30` as a shell session and returns the tool and its id.
#[cfg(unix)]
fn background_session(workspace: &tempfile::TempDir) -> (Arc<rune_tools::shell::Shell>, String) {
    use rune_tools::Tool as _;
    let shell = Arc::new(rune_tools::shell::Shell::default());
    let context = ExecutionContext::new(
        Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path"),
    );
    let started = shell
        .call(
            &serde_json::json!({ "action": "run", "command": "sleep 30", "yield_time_ms": 100 }),
            &context,
        )
        .expect("started");
    let id = started
        .text
        .split_once("as session ")
        .and_then(|(_, rest)| rest.split_once(' '))
        .map_or_else(
            || panic!("no session in {}", started.text),
            |(id, _)| id.to_owned(),
        );
    (shell, id)
}

#[cfg(unix)]
fn stop_session(shell: &rune_tools::shell::Shell, id: &str, workspace: &camino::Utf8Path) {
    use rune_tools::Tool as _;
    let _ = shell.call(
        &serde_json::json!({ "action": "stop", "session_id": id }),
        &ExecutionContext::new(workspace.to_owned()),
    );
}

#[cfg(unix)]
fn check_turn(scripts: Vec<Script>) -> TestHost {
    let mut shell = read_tool();
    "shell".clone_into(&mut shell.name);
    TestHost::new(MockEndpoint::start(scripts))
        .with_tools(vec![shell, read_tool()])
        .with_tool_response(ToolOutput::failure("[tests: 3 passed, 1 failed]"))
}

#[cfg(unix)]
#[test]
fn a_check_repeated_while_a_session_runs_is_run_again() {
    let _quiet = BACKGROUND_WORK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let workspace = tempfile::tempdir().expect("workspace");
    let (shell, id) = background_session(&workspace);
    let test = r#"{"action":"run","command":"cargo test -p core"}"#;
    let host = check_turn(vec![
        Script::tool_call("first", "shell", test),
        Script::tool_call("same", "shell", test),
        Script::text("done"),
    ]);
    let mut history = rune_agent::History::new();
    history.push_user("test");
    let outcome = run_turn(&mut history, &host).expect("turn");
    stop_session(
        &shell,
        &id,
        camino::Utf8Path::from_path(workspace.path()).expect("UTF-8 path"),
    );
    assert_eq!(host.executed_calls().len(), 2);
    assert_eq!(repeat_pointers(&host), 0);
    assert!(outcome.calls[1].executed);
}

#[cfg(unix)]
#[test]
fn a_check_repeated_after_a_session_ended_is_run_again() {
    let _quiet = BACKGROUND_WORK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let workspace = tempfile::tempdir().expect("workspace");
    let root = Utf8PathBuf::from_path_buf(workspace.path().to_owned()).expect("UTF-8 path");
    let (shell, id) = background_session(&workspace);
    let test = r#"{"action":"run","command":"cargo test -p core"}"#;
    let host = check_turn(vec![
        Script::tool_call("first", "shell", test),
        Script::tool_call("look", "read_file", r#"{"path":"README.md"}"#),
        Script::tool_call("same", "shell", test),
        Script::text("done"),
    ]);
    // The session ends during the read, which changes no files, so only the
    // background record tells the second run apart from the first.
    *host.on_execute.lock().expect("lock") = Some(Box::new(move |name| {
        if name == "read_file" {
            stop_session(&shell, &id, &root);
            let deadline = Instant::now() + Duration::from_secs(5);
            while rune_tools::shell::background().0 > 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }));
    let mut history = rune_agent::History::new();
    history.push_user("test");
    let outcome = run_turn(&mut history, &host).expect("turn");
    assert_eq!(host.executed_calls().len(), 3);
    assert_eq!(repeat_pointers(&host), 0);
    assert!(outcome.calls[2].executed);
}
