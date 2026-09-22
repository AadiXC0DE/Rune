//! Integration tests for the subprocess plugin protocol.
//!
//! Every test drives a real child process over real pipes. The properties under
//! test are the ones the protocol exists to guarantee: a denied call never
//! reaches the plugin, a crash loop is bounded and then disabled rather than
//! restarted forever, an incompatible protocol version is refused by name, and
//! the frame cap is enforced where the bytes arrive.

// Integration tests assert by panicking.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::config::Layer;
use rune_core::error::ErrorCode;
use rune_sdk::agent::{Agent, AgentOptions, Dialect, HostFetch};
use rune_sdk::plugin::{MAX_FRAME_BYTES, PluginHost, PluginManifest, PluginTool, SharedPlugin};

/// The scripted plugin the tests drive.
///
/// The plugin is this crate's own fixture binary rather than a shell script: a
/// shell is a second implementation of the protocol to keep in step with the
/// first, and it is absent on some platforms, so every test that needed one was
/// skipped there. Naming the scenario on the command line keeps the whole
/// protocol in one file.
struct Fixture {
    directory: tempfile::TempDir,
    /// Where the plugin records the frames it receives, when a test needs one.
    journal: Option<PathBuf>,
}

impl Fixture {
    /// Names a scenario the fixture binary serves.
    fn new(name: &str, scenario: &str, tools: &str) -> Self {
        Self::build(name, scenario, tools, false)
    }

    /// Names a scenario that also records the frames it receives.
    fn recording(name: &str, scenario: &str, tools: &str) -> Self {
        Self::build(name, scenario, tools, true)
    }

    /// Builds a manifest for one scenario.
    fn build(name: &str, scenario: &str, tools: &str, record: bool) -> Self {
        let directory = tempfile::tempdir().expect("temp dir");
        let journal = record.then(|| directory.path().join("journal.txt"));
        let commands = vec![
            Self::binary().to_string_lossy().into_owned(),
            scenario.to_owned(),
            journal
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ];
        let manifest = json!({
            "protocol_version": 1,
            "name": name,
            "tools": serde_json::from_str::<Value>(tools).expect("tool list"),
            "commands": commands,
        });
        std::fs::write(
            directory.path().join("plugin.json"),
            serde_json::to_vec_pretty(&manifest).expect("encode manifest"),
        )
        .expect("write manifest");
        Self { directory, journal }
    }

    /// Returns the fixture binary.
    fn binary() -> PathBuf {
        let mut path = std::env::current_exe().expect("test exe path");
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        path.join(format!("plugin-fixture{}", std::env::consts::EXE_SUFFIX))
    }

    /// Returns the plugin directory to start.
    fn path(&self) -> PathBuf {
        self.directory.path().to_path_buf()
    }

    /// Returns the path of the record file, when one was requested.
    fn journal(&self) -> &PathBuf {
        self.journal.as_ref().expect("a recording fixture")
    }
}

/// The two tools every fixture declares.
const TOOLS: &str = r#"[
    {"name": "echo", "description": "Echoes its argument.", "input_schema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "boom", "description": "Fails on purpose.", "input_schema": {"type": "object"}}
]"#;

/// Builds a limits set with tight plugin bounds.
fn limits(startup_ms: u64, restart_limit: u64) -> BudgetSet {
    let mut set = BudgetSet::new();
    for (name, value) in [
        (LimitName::McpStartupTimeoutMs, startup_ms),
        (LimitName::McpOperationTimeoutMs, 5_000),
        (LimitName::McpRestartLimit, restart_limit),
    ] {
        set.set(name, Budget::Bounded(value), Layer::CommandLine)
            .expect("a valid limit");
    }
    set
}

#[test]
fn a_manifest_declares_the_tools_the_host_lists() {
    let fixture = Fixture::new("echoer", "echoer", TOOLS);
    let mut host = PluginHost::start(fixture.path()).expect("start");

    assert_eq!(host.name(), "echoer");
    let tools = host.list_tools();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[0].description, "Echoes its argument.");
    assert_eq!(tools[0].input_schema["type"], "object");
    assert!(!host.is_disabled());

    host.shutdown().expect("shutdown");
}

#[test]
fn a_tool_call_returns_what_the_plugin_answered() {
    let fixture = Fixture::new("echoer", "echoer", TOOLS);
    let mut host = PluginHost::start(fixture.path()).expect("start");

    let output = host.call("echo", &json!({"text": "hello"})).expect("call");
    assert_eq!(output.text, "echoed");
    assert!(!output.is_error);

    // A plugin error is reported as a failed tool result, not an error of the
    // host: the model can read it and adapt.
    let err = host
        .call("absent", &json!({}))
        .expect_err("an unknown tool");
    assert_eq!(err.code(), ErrorCode::NotFound);

    host.shutdown().expect("shutdown");
}

#[test]
fn a_denied_tool_is_never_sent_to_the_plugin() {
    // The fixture writes every request it receives to its own stdout, so a call
    // that reached it would come back as a frame this host reads.
    let fixture = Fixture::new("echoer", "echoer", TOOLS);
    let mut host = PluginHost::start(fixture.path()).expect("start");

    let authorize = |name: &str, _arguments: &Value| name == "echo";
    let output = host
        .call_with_authorization("echo", &json!({"text": "allowed"}), &authorize)
        .expect("an allowed call");
    assert_eq!(output.text, "echoed");

    let err = host
        .call_with_authorization("boom", &json!({"secret": "do not send"}), &authorize)
        .expect_err("a denied call");
    assert_eq!(err.code(), ErrorCode::PermissionDenied);

    // The plugin is still healthy and still answering, which is what shows the
    // denial was the host's decision rather than a failure.
    let output = host
        .call("echo", &json!({"text": "still here"}))
        .expect("call");
    assert_eq!(output.text, "echoed");
    host.shutdown().expect("shutdown");
}

#[test]
fn a_frame_the_plugin_initiates_needs_no_answer() {
    // The echo fixture sends a notification back for every call it serves, so
    // the call that follows a notification still returns the right result.
    let fixture = Fixture::new("echoer", "echoer", TOOLS);
    let mut host = PluginHost::start(fixture.path()).expect("start");

    for _ in 0..3 {
        let output = host.call("echo", &json!({"text": "again"})).expect("call");
        assert_eq!(output.text, "echoed");
    }
    host.shutdown().expect("shutdown");
}

#[test]
fn a_denied_call_leaves_no_trace_in_the_plugin() {
    // The fixture records every request it receives, so a call that reached it
    // would be found in the record.
    let fixture = Fixture::recording("journaler", "journaler", TOOLS);
    let mut host = PluginHost::start(fixture.path()).expect("start");
    let authorize = |name: &str, _arguments: &Value| name == "echo";

    host.call_with_authorization("echo", &json!({"text": "allowed"}), &authorize)
        .expect("an allowed call");
    let err = host
        .call_with_authorization("boom", &json!({"secret": "do-not-send-31415"}), &authorize)
        .expect_err("a denied call");
    assert_eq!(err.code(), ErrorCode::PermissionDenied);

    host.shutdown().expect("shutdown");
    std::thread::sleep(Duration::from_millis(50));

    let recorded = std::fs::read_to_string(fixture.journal()).expect("journal");
    assert!(
        recorded.contains("\"echo\""),
        "the allowed call should have been sent: {recorded}"
    );
    assert!(
        !recorded.contains("boom"),
        "the denied call reached the plugin: {recorded}"
    );
    assert!(
        !recorded.contains("do-not-send-31415"),
        "the denied arguments reached the plugin: {recorded}"
    );
}

#[test]
fn a_crash_looping_plugin_is_disabled_after_its_restart_limit() {
    // Exits immediately, so every start fails.
    let fixture = Fixture::new("crasher", "crasher", TOOLS);
    let set = limits(2_000, 2);

    let err = PluginHost::start_with_limits(fixture.path(), set).expect_err("a crash loop");
    assert_eq!(err.code(), ErrorCode::LimitExceeded);
    assert!(err.message().contains("disabled"), "{}", err.message());
    assert!(err.message().contains("3 times"), "{}", err.message());
}

#[test]
fn a_plugin_that_never_answers_the_handshake_times_out() {
    let fixture = Fixture::new("silent", "silent", TOOLS);
    let set = limits(200, 0);

    let err = PluginHost::start_with_limits(fixture.path(), set).expect_err("a silent plugin");
    assert_eq!(err.code(), ErrorCode::LimitExceeded);
    assert!(err.message().contains("200 ms"), "{}", err.message());
}

#[test]
fn an_incompatible_manifest_version_is_refused_naming_both_versions() {
    let directory = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        directory.path().join("plugin.json"),
        serde_json::to_vec(&json!({
            "protocol_version": 4,
            "name": "futuristic",
            "tools": serde_json::from_str::<Value>(TOOLS).expect("tools"),
            "commands": ["/bin/true"],
        }))
        .expect("encode"),
    )
    .expect("write manifest");

    let err = PluginHost::start(directory.path()).expect_err("an unsupported version");
    assert_eq!(err.code(), ErrorCode::UnsupportedVersion);
    assert!(err.message().contains("version 4"), "{}", err.message());
    assert!(err.message().contains("version 1"), "{}", err.message());
    assert!(err.message().contains("futuristic"), "{}", err.message());
}

#[test]
fn an_incompatible_handshake_version_is_refused_naming_both_versions() {
    let fixture = Fixture::new("liar", "liar", TOOLS);

    let err = PluginHost::start_with_limits(fixture.path(), limits(2_000, 0))
        .expect_err("a handshake this build cannot speak");
    assert_eq!(err.code(), ErrorCode::LimitExceeded);
    assert!(err.message().contains("version 9"), "{}", err.message());
    assert!(err.message().contains("version 1"), "{}", err.message());
}

#[test]
fn a_frame_larger_than_the_cap_is_refused() {
    let fixture = Fixture::new("flooder", "flooder", TOOLS);
    let mut host = PluginHost::start_with_limits(fixture.path(), limits(5_000, 0)).expect("start");
    let err = host
        .call("echo", &json!({}))
        .expect_err("an oversized frame");
    assert_eq!(err.code(), ErrorCode::TooLarge);
    assert_eq!(err.field(), Some("plugin.frame"));
}

#[test]
fn a_plugin_that_dies_during_a_call_is_reported_rather_than_retried() {
    let fixture = Fixture::new("quitter", "quitter", TOOLS);
    let mut host = PluginHost::start_with_limits(fixture.path(), limits(2_000, 1)).expect("start");
    let err = host.call("echo", &json!({})).expect_err("the plugin died");
    assert_eq!(err.code(), ErrorCode::TransportFailure);
    assert!(!host.is_disabled(), "one restart remains");

    // The next call spends the remaining restart, and the one after that finds
    // the plugin disabled rather than starting it again.
    let err = host
        .call("echo", &json!({}))
        .expect_err("the plugin died again");
    assert_eq!(err.code(), ErrorCode::TransportFailure);
    let err = host
        .call("echo", &json!({}))
        .expect_err("the plugin is disabled");
    assert_eq!(err.code(), ErrorCode::LimitExceeded);
    assert!(host.is_disabled());
    assert!(host.disabled_reason().is_some());
}

#[test]
fn a_request_to_an_unknown_method_is_reported_by_the_plugin() {
    let fixture = Fixture::new("refuser", "refuser", TOOLS);
    let mut host = PluginHost::start(fixture.path()).expect("start");

    let err = host.call("echo", &json!({})).expect_err("a refusal");
    assert_eq!(err.code(), ErrorCode::Unsupported);
    assert!(err.message().contains("no such tool"), "{}", err.message());
}

#[test]
fn an_argument_too_large_for_one_frame_is_refused_without_touching_the_plugin() {
    let fixture = Fixture::new("echoer", "echoer", TOOLS);
    let mut host = PluginHost::start(fixture.path()).expect("start");

    let huge = "a".repeat(MAX_FRAME_BYTES + 64);
    let err = host
        .call("echo", &json!({ "text": huge }))
        .expect_err("an oversized argument");
    assert_eq!(err.code(), ErrorCode::TooLarge);
    assert_eq!(err.field(), Some("plugin.frame"));
    assert_eq!(host.restarts(), 0, "a refused argument costs no restart");

    // The plugin was never disturbed.
    let output = host.call("echo", &json!({"text": "fine"})).expect("call");
    assert_eq!(output.text, "echoed");
    host.shutdown().expect("shutdown");
}

#[test]
fn a_plugin_result_larger_than_its_bound_is_truncated() {
    let fixture = Fixture::new("verbose", "verbose", TOOLS);
    let mut host = PluginHost::start(fixture.path()).expect("start");

    let output = host.call("echo", &json!({})).expect("call");
    assert!(output.text.len() < 200 * 1024, "{}", output.text.len());
    assert!(
        output.text.ends_with("(truncated)"),
        "{}",
        output.text.len()
    );
    // The full size is still reported, so a host accounts for what was produced.
    assert_eq!(output.produced_bytes, 200 * 1024);
}

#[test]
fn a_plugin_that_died_between_calls_is_restarted_within_the_budget() {
    // Dies after the first call, so the next call needs a fresh process.
    let fixture = Fixture::new("flaky", "flaky", TOOLS);
    let mut host = PluginHost::start_with_limits(fixture.path(), limits(2_000, 3)).expect("start");
    assert_eq!(host.restarts(), 0);

    let output = host.call("echo", &json!({})).expect("the first call");
    assert_eq!(output.text, "echoed");
    assert_eq!(host.restarts(), 0, "a live plugin costs no restart");

    // The fixture exits as it answers, so the death lands just after the call.
    std::thread::sleep(Duration::from_millis(100));

    let output = host.call("echo", &json!({})).expect("restarted");
    assert_eq!(output.text, "echoed");
    assert!(!host.is_disabled());
    assert!(host.restarts() > 0, "the death spent a restart");
}

#[test]
fn shutdown_is_idempotent_and_refuses_further_calls() {
    let fixture = Fixture::new("echoer", "echoer", TOOLS);
    let mut host = PluginHost::start(fixture.path()).expect("start");
    host.shutdown().expect("shutdown");
    host.shutdown().expect("a second shutdown is a no-op");

    let err = host
        .call("echo", &json!({}))
        .expect_err("a shut down plugin");
    assert_eq!(err.code(), ErrorCode::InvalidState);
}

#[test]
fn a_manifest_without_a_command_is_refused() {
    let directory = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        directory.path().join("plugin.json"),
        serde_json::to_vec(&json!({
            "protocol_version": 1,
            "name": "empty",
            "tools": [],
        }))
        .expect("encode"),
    )
    .expect("write manifest");

    let err = PluginHost::start(directory.path()).expect_err("no command");
    assert_eq!(err.code(), ErrorCode::MissingField);
    assert_eq!(err.field(), Some("plugin.commands"));
}

#[test]
fn a_plugin_program_that_does_not_exist_is_reported_without_retrying() {
    let directory = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        directory.path().join("plugin.json"),
        serde_json::to_vec(&json!({
            "protocol_version": 1,
            "name": "ghost",
            "tools": serde_json::from_str::<Value>(TOOLS).expect("tools"),
            "commands": ["/nonexistent/plugin-program"],
        }))
        .expect("encode"),
    )
    .expect("write manifest");

    let err = PluginHost::start_with_limits(directory.path(), limits(2_000, 3))
        .expect_err("a missing program");
    assert_eq!(err.code(), ErrorCode::NotFound);
    assert!(
        err.message().contains("/nonexistent/plugin-program"),
        "{}",
        err.message()
    );
}

#[test]
fn a_manifest_declaring_more_tools_than_the_cap_is_refused() {
    let directory = tempfile::tempdir().expect("temp dir");
    let tools: Vec<Value> = (0..=rune_core::tool::MAX_TOOLS)
        .map(|index| {
            json!({
                "name": format!("tool_{index}"),
                "description": "A tool.",
                "input_schema": {"type": "object"},
            })
        })
        .collect();
    std::fs::write(
        directory.path().join("plugin.json"),
        serde_json::to_vec(&json!({
            "protocol_version": 1,
            "name": "greedy",
            "tools": tools,
            "commands": ["/bin/true"],
        }))
        .expect("encode"),
    )
    .expect("write manifest");

    let err = PluginHost::start(directory.path()).expect_err("too many tools");
    assert_eq!(err.code(), ErrorCode::TooLarge);
    assert_eq!(err.field(), Some("tools"));
}

#[test]
fn output_the_plugin_writes_for_itself_does_not_disturb_a_call() {
    // A plugin that logs to its own output is normal; the host reads past it.
    let fixture = Fixture::new("chatty", "chatty", TOOLS);
    let mut host = PluginHost::start(fixture.path()).expect("start");

    let output = host.call("echo", &json!({})).expect("call");
    assert_eq!(output.text, "echoed");
    host.shutdown().expect("shutdown");
}

#[test]
fn a_manifest_missing_altogether_is_reported_as_not_found() {
    let directory = tempfile::tempdir().expect("temp dir");
    let err = PluginHost::start(directory.path()).expect_err("no manifest");
    assert_eq!(err.code(), ErrorCode::NotFound);
}

#[test]
fn a_manifest_that_is_not_json_is_refused() {
    let directory = tempfile::tempdir().expect("temp dir");
    std::fs::write(directory.path().join("plugin.json"), b"{not json").expect("write manifest");
    let err = PluginHost::start(directory.path()).expect_err("a malformed manifest");
    assert_eq!(err.code(), ErrorCode::InvalidField);
    assert_eq!(err.field(), Some("plugin.json"));
}

#[test]
fn a_manifest_can_be_loaded_from_a_file_path() {
    let fixture = Fixture::new("echoer", "echoer", TOOLS);
    let manifest = PluginManifest::load(&fixture.path().join("plugin.json")).expect("manifest");
    assert_eq!(manifest.name, "echoer");
    assert_eq!(manifest.protocol_version, 1);
    assert_eq!(manifest.tools.len(), 2);
    assert_eq!(
        manifest.tools[0],
        PluginTool {
            name: "echo".to_owned(),
            description: "Echoes its argument.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"text": {"type": "string"}}
            }),
        }
    );
    assert_eq!(manifest.tools[1].name, "boom");
}

/// A fetch that forwards to a real endpoint, so a plugin tool can be driven
/// through a whole turn rather than called directly.
#[derive(Debug)]
struct ForwardingFetch;

impl HostFetch for ForwardingFetch {
    fn post(
        &self,
        request: rune_sdk::agent::FetchRequest,
    ) -> rune_core::error::Result<rune_sdk::agent::FetchResponse> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let response = agent
            .post(&request.url)
            .header("content-type", "application/json")
            .send(&request.body)
            .map_err(|err| {
                rune_core::error::RuneError::new(
                    ErrorCode::TransportFailure,
                    format!("the test fetch failed: {err}"),
                )
            })?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        response
            .into_body()
            .into_reader()
            .read_to_end(&mut body)
            .expect("read the response body");
        Ok(rune_sdk::agent::FetchResponse::new(status, body))
    }
}

#[test]
fn a_plugins_tools_drive_a_whole_turn() {
    let fixture = Fixture::new("echoer", "echoer", TOOLS);
    let host = PluginHost::start(fixture.path()).expect("start");
    let shared = Arc::new(SharedPlugin::new(host));

    let endpoint = rune_testkit::MockEndpoint::start(vec![
        rune_testkit::Script::tool_call("call_1", "echo", "{\"text\":\"from the model\"}"),
        rune_testkit::Script::text("the plugin answered"),
    ]);
    let mut agent = Agent::new(AgentOptions {
        api_key: "sk-plugin-credential".to_owned(),
        model: Some("mock-model".to_owned()),
        instructions: None,
        tools: shared.host_tools(),
        base_url: endpoint.base_url(),
        dialect: Dialect::ChatCompletions,
        fetch: Some(Arc::new(ForwardingFetch)),
    })
    .expect("agent");

    let mut turn = agent
        .prompt("use the plugin", rune_sdk::agent::PromptOptions::default())
        .expect("turn");
    let result = turn.result().expect("result");

    assert_eq!(result.calls.len(), 1);
    assert!(result.calls[0].executed, "{:?}", result.calls[0].output);
    assert_eq!(result.calls[0].output.text, "echoed");

    shared.shutdown().expect("shutdown");
}
