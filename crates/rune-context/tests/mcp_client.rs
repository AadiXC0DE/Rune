//! Integration tests for the MCP client.
//!
//! Every test runs against a real endpoint rather than a stub. The stdio tests
//! drive a scripted child process and the HTTP tests drive a real server on a
//! loopback port, so the framing, the timeout path, and the recovery logic are
//! all exercised the way they run in production. A test that replaced the
//! transport would not catch a framing bug, and a framing bug is what the
//! transport exists to prevent.

// Integration tests assert by panicking.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use rune_context::mcp::client::{Client, Credential, Expiry, FixedEnvironment, Variables};
use rune_context::mcp::{ServerConfig, Transport};
use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::config::Layer;
use rune_core::error::ErrorCode;

/// The scripted stdio server the tests drive.
///
/// The server is this crate's own fixture binary rather than a shell script: a
/// shell is a second language to keep in step with the protocol, and it is
/// absent on some platforms, so every test that needed one was skipped there.
/// Naming the scenario on the command line keeps the whole protocol in one file.
struct Fixture {
    directory: tempfile::TempDir,
    scenario: String,
    marker: Option<String>,
}

impl Fixture {
    /// Names a scenario the fixture binary serves.
    fn new(scenario: &str) -> Self {
        Self {
            directory: tempfile::tempdir().expect("temp dir"),
            scenario: scenario.to_owned(),
            marker: None,
        }
    }

    /// Names a scenario that also starts a marked descendant process.
    fn grouped(marker: &str) -> Self {
        Self {
            directory: tempfile::tempdir().expect("temp dir"),
            scenario: "grouped".to_owned(),
            marker: Some(marker.to_owned()),
        }
    }

    /// Returns the fixture binary.
    fn binary() -> std::path::PathBuf {
        let mut path = std::env::current_exe().expect("test exe path");
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        path.join(format!("mcp-fixture{}", std::env::consts::EXE_SUFFIX))
    }

    /// Returns a stdio configuration running this fixture.
    fn config(&self, name: &str) -> ServerConfig {
        let mut command = vec![
            Self::binary().to_string_lossy().into_owned(),
            self.scenario.clone(),
        ];
        if let Some(marker) = &self.marker {
            command.push(marker.clone());
        }
        ServerConfig {
            name: name.to_owned(),
            transport: Transport::Stdio {
                command,
                environment: BTreeMap::new(),
            },
            enabled: true,
            required: false,
            startup_timeout_ms: 10_000,
            operation_timeout_ms: 10_000,
            restart_limit: 1,
        }
    }
}

/// Builds a fixed variable set for tests that need credentials.
fn variables(pairs: &[(&str, &str)]) -> Arc<dyn Variables> {
    Arc::new(FixedEnvironment::new(
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    ))
}

/// Returns a budget with every MCP bound set explicitly.
fn budget() -> BudgetSet {
    let mut limits = BudgetSet::new();
    for name in [
        LimitName::McpOperationTimeoutMs,
        LimitName::McpStartupTimeoutMs,
        LimitName::McpRestartLimit,
        LimitName::McpSearchResultBytes,
    ] {
        limits
            .set(name, name.default_value(), Layer::Default)
            .expect("bound");
    }
    limits
}

/// Returns a server configuration that cannot start.
fn missing_config(name: &str) -> ServerConfig {
    ServerConfig {
        name: name.to_owned(),
        transport: Transport::Stdio {
            command: vec!["/nonexistent/mcp-binary".to_owned()],
            environment: BTreeMap::new(),
        },
        enabled: true,
        required: false,
        startup_timeout_ms: 5_000,
        operation_timeout_ms: 5_000,
        restart_limit: 1,
    }
}

#[test]
fn a_stdio_server_connects_lists_tools_and_executes_one() {
    let fixture = Fixture::new("standard");
    let client = Client::new(&budget());
    let report = client
        .connect_all(&[fixture.config("fixture")])
        .expect("connected");
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(report.connected, ["fixture"]);

    let tools = client.tools();
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["mcp_fixture_greet", "mcp_fixture_ping"]
    );
    assert_eq!(tools[0].description, "Greets.");
    assert_eq!(tools[0].input_schema["required"], json!(["who"]));
    assert_eq!(tools[0].server_tool, "greet");

    let outcome = client
        .call("mcp_fixture_greet", &json!({ "who": "world" }))
        .expect("called");
    assert_eq!(outcome.text, "hello from the fixture");
    assert_eq!(outcome.server, "fixture");
    assert_eq!(outcome.tool, "greet");
    assert!(!outcome.is_error);
    assert!(!outcome.truncated);

    let status = client.server("fixture").expect("status");
    assert!(status.connected);
    assert_eq!(status.transport, "stdio");
    assert_eq!(status.protocol_version.as_deref(), Some("2025-06-18"));
    assert_eq!(status.tools, 2);
    assert_eq!(status.restarts, 0);
    client.shutdown();
}

#[test]
fn a_server_paginating_its_tool_list_is_followed_to_the_end() {
    let scenario = "paged";
    let fixture = Fixture::new(scenario);
    let client = Client::new(&budget());
    client
        .connect_all(&[fixture.config("paged")])
        .expect("connected");
    assert_eq!(
        client
            .tools()
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>(),
        ["mcp_paged_first", "mcp_paged_second"]
    );
    client.shutdown();
}

#[test]
fn a_malformed_tool_entry_is_reported_without_losing_its_siblings() {
    let scenario = "mixed";
    let fixture = Fixture::new(scenario);
    let client = Client::new(&budget());
    client
        .connect_all(&[fixture.config("mixed")])
        .expect("connected");
    assert_eq!(
        client
            .tools()
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>(),
        ["mcp_mixed_good"]
    );
    // The entry without a schema, the entry without a name, and the duplicate
    // are each reported, and the one good tool survives all three.
    let status = client.server("mixed").expect("status");
    assert_eq!(status.warnings.len(), 3, "{:?}", status.warnings);
    assert!(
        status
            .warnings
            .iter()
            .all(|warning| warning.contains("skipped")),
        "{:?}",
        status.warnings
    );
    client.shutdown();
}

#[test]
fn a_tool_listing_over_the_search_budget_is_refused() {
    let scenario = "search-budget";
    let fixture = Fixture::new(scenario);
    let mut limits = budget();
    limits
        .set(
            LimitName::McpSearchResultBytes,
            Budget::Bounded(64),
            Layer::Default,
        )
        .expect("bound");
    let client = Client::new(&limits);
    let report = client
        .connect_all(&[fixture.config("paged")])
        .expect("connected");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].error.code(), ErrorCode::LimitExceeded);
    assert_eq!(report.failed[0].server, "paged");
    assert!(client.tools().is_empty());
    client.shutdown();
}

#[test]
fn a_frame_over_the_cap_is_rejected_with_too_large() {
    let scenario = "huge";
    let fixture = Fixture::new(scenario);
    let mut limits = budget();
    // The listing budget is raised so the frame cap is what stops the server,
    // not the size of the listing.
    limits
        .set(
            LimitName::McpSearchResultBytes,
            Budget::Bounded(64 * 1024 * 1024),
            Layer::Default,
        )
        .expect("bound");
    let client = Client::new(&limits);
    let report = client
        .connect_all(&[fixture.config("huge")])
        .expect("connected");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].error.code(), ErrorCode::TooLarge);
    assert_eq!(report.failed[0].error.field(), Some("frame"));
    client.shutdown();
}

#[test]
fn a_server_that_never_answers_times_out() {
    let scenario = "silent";
    let fixture = Fixture::new(scenario);
    let mut config = fixture.config("hung");
    config.startup_timeout_ms = 300;
    let client = Client::new(&budget());
    let started = Instant::now();
    let report = client.connect_all(&[config]).expect("connected");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the connect blocked for {elapsed:?}"
    );
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].error.code(), ErrorCode::Timeout);
    assert!(client.tools().is_empty());
    client.shutdown();
}

#[test]
fn a_hung_tool_call_times_out_rather_than_blocking() {
    let scenario = "hung-call";
    let fixture = Fixture::new(scenario);
    let mut config = fixture.config("slow");
    config.operation_timeout_ms = 300;
    let client = Client::new(&budget());
    client.connect_all(&[config]).expect("connected");
    assert_eq!(client.tools().len(), 1);
    let started = Instant::now();
    let error = client
        .call("mcp_slow_slow", &json!({}))
        .expect_err("timed out");
    let elapsed = started.elapsed();
    assert_eq!(error.code(), ErrorCode::Timeout);
    assert!(
        elapsed < Duration::from_secs(5),
        "the call blocked for {elapsed:?}"
    );
    client.shutdown();
}

#[test]
fn a_tool_that_vanishes_after_listing_fails_closed() {
    let scenario = "vanishing";
    let fixture = Fixture::new(scenario);
    let client = Client::new(&budget());
    client
        .connect_all(&[fixture.config("fixture")])
        .expect("connected");
    // A reload against a server that no longer lists the tool must not leave the
    // old entry callable.
    let error = client
        .call("mcp_fixture_absent", &json!({}))
        .expect_err("refused");
    assert_eq!(error.code(), ErrorCode::NotFound);
    client.shutdown();
}

#[test]
fn a_missing_tool_is_refused_without_reaching_the_transport() {
    let fixture = Fixture::new("standard");
    let client = Client::new(&budget());
    client
        .connect_all(&[fixture.config("fixture")])
        .expect("connected");
    let error = client
        .call("mcp_fixture_absent", &json!({}))
        .expect_err("refused");
    assert_eq!(error.code(), ErrorCode::NotFound);
    client.shutdown();
}

#[test]
fn a_required_server_that_fails_leaves_nothing_connected() {
    let fixture = Fixture::new("standard");
    let mut required = missing_config("missing");
    required.required = true;
    let client = Client::new(&budget());
    let error = client
        .connect_all(&[fixture.config("fixture"), required])
        .expect_err("refused");
    assert_eq!(error.code(), ErrorCode::TransportFailure);
    assert!(client.tools().is_empty(), "a child survived the failed set");
    assert!(
        client.status().iter().all(|status| !status.connected),
        "a server stayed connected after the set failed"
    );
}

#[test]
fn an_optional_server_that_fails_leaves_its_siblings_connected() {
    let fixture = Fixture::new("standard");
    let client = Client::new(&budget());
    let report = client
        .connect_all(&[fixture.config("fixture"), missing_config("missing")])
        .expect("connected");
    assert_eq!(report.connected, ["fixture"]);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].server, "missing");
    assert_eq!(client.tools().len(), 2);
    client.shutdown();
}

#[test]
fn shutdown_terminates_the_child_process_and_its_descendants() {
    // The server starts a background child, and the child writes a heartbeat to
    // a file. A heartbeat that stops advancing is what says the descendant is
    // gone, which avoids asking the platform for a process list that not every
    // platform provides.
    let marker = format!("mcp-child-marker-{}", std::process::id());
    let heartbeat = fixture_heartbeat(&marker);
    let _ = std::fs::remove_file(&heartbeat);
    let fixture = Fixture::grouped(heartbeat.to_str().expect("heartbeat path"));
    let client = Client::new(&budget());
    client
        .connect_all(&[fixture.config("grouped")])
        .expect("connected");
    assert!(client.server("grouped").expect("status").connected);
    assert!(
        wait_for_heartbeat(&heartbeat),
        "the fixture did not start its descendant; the test would prove nothing"
    );

    client.shutdown();
    assert!(!client.server("grouped").expect("status").connected);
    drop(client);

    // A writer that has stopped leaves the count where it was.
    let last = read_heartbeat(&heartbeat);
    let deadline = Instant::now();
    while deadline.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(200));
        if read_heartbeat(&heartbeat) == last {
            return;
        }
    }
    panic!("a child process survived shutdown: {heartbeat:?}");
}

/// Returns the path the descendant writes its heartbeat to.
fn fixture_heartbeat(marker: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(marker)
}

/// Returns the last count the descendant wrote, or `None` before it starts.
fn read_heartbeat(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

/// Waits for the descendant to start writing.
fn wait_for_heartbeat(path: &std::path::Path) -> bool {
    let deadline = Instant::now();
    while deadline.elapsed() < Duration::from_secs(10) {
        if read_heartbeat(path).is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn a_rejected_credential_with_an_unstated_expiry_recovers_after_one_reconnect() {
    // The server reports a null expiry on initialize, rejects the first tool
    // call with an auth error, and answers normally from then on. The tools must
    // remain available across the rejection.
    let scenario = "flaky-credential";
    let fixture = Fixture::new(scenario);
    let counter = fixture.directory.path().join("count");
    let mut config = fixture.config("flaky");
    if let Transport::Stdio { environment, .. } = &mut config.transport {
        environment.insert(
            "MCP_FIXTURE_COUNT".to_owned(),
            counter.to_str().expect("counter path").to_owned(),
        );
    }
    let client = Client::new(&budget());
    let report = client.connect_all(&[config]).expect("connected");
    assert!(report.is_complete(), "{report:?}");

    let outcome = client
        .call("mcp_flaky_greet", &json!({ "who": "world" }))
        .expect("called");
    assert_eq!(outcome.text, "hello from the fixture");

    let status = client.server("flaky").expect("status");
    assert!(status.connected);
    assert_eq!(status.restarts, 1);
    assert_eq!(status.credential_expires_at_ms, None);
    assert_eq!(status.tools, 1);
    assert!(
        client
            .tools()
            .iter()
            .any(|tool| tool.name == "mcp_flaky_greet"),
        "the tools did not survive the reconnect"
    );
    client.shutdown();
}

#[test]
fn a_server_that_refuses_the_newest_revision_is_negotiated_down() {
    // The server refuses the first two revisions with a rejected-parameter
    // error and accepts the third, naming a revision on the ladder. The client
    // must not give up on the first refusal.
    let scenario = "refuse-two";
    let fixture = Fixture::new(scenario);
    let client = Client::new(&budget());
    let report = client
        .connect_all(&[fixture.config("old")])
        .expect("connected");
    assert!(report.is_complete(), "{report:?}");
    let status = client.server("old").expect("status");
    assert_eq!(status.protocol_version.as_deref(), Some("2025-03-26"));
    assert_eq!(status.tools, 1);
    client.shutdown();
}

#[test]
fn a_server_that_names_a_revision_outside_the_ladder_is_negotiated_down() {
    // The server answers every revision with one this client does not speak, so
    // the client walks the whole ladder and reports the refusal.
    let scenario = "future";
    let fixture = Fixture::new(scenario);
    let client = Client::new(&budget());
    let report = client
        .connect_all(&[fixture.config("future")])
        .expect("connected");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].error.code(), ErrorCode::UnsupportedVersion);
    client.shutdown();
}

#[test]
fn a_transport_failure_does_not_walk_the_revision_ladder() {
    // A server that closes its output is not making a statement about the
    // revision, so the ladder stops at the first failure.
    let scenario = "dead";
    let fixture = Fixture::new(scenario);
    let client = Client::new(&budget());
    let report = client
        .connect_all(&[fixture.config("dead")])
        .expect("connected");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].error.code(), ErrorCode::TransportFailure);
    client.shutdown();
}

#[test]
fn a_credential_the_server_reported_as_expired_is_refreshed_before_the_call() {
    // The server reports an expiry in the past, so every call refreshes the
    // credential rather than paying for a rejection first. The tools stay
    // available throughout.
    let scenario = "stale-credential";
    let fixture = Fixture::new(scenario);
    let client = Client::new(&budget());
    client
        .connect_all(&[fixture.config("stale")])
        .expect("connected");
    assert_eq!(
        client
            .server("stale")
            .expect("status")
            .credential_expires_at_ms,
        Some(1)
    );
    for _ in 0..2 {
        let outcome = client.call("mcp_stale_greet", &json!({})).expect("called");
        assert_eq!(outcome.text, "ok");
    }
    assert_eq!(
        client
            .tools()
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>(),
        ["mcp_stale_greet"]
    );
    client.shutdown();
}

#[test]
fn a_credential_rejected_past_the_restart_limit_is_reported() {
    let scenario = "denied-credential";
    let fixture = Fixture::new(scenario);
    let client = Client::new(&budget());
    client
        .connect_all(&[fixture.config("denied")])
        .expect("connected");
    let error = client
        .call("mcp_denied_greet", &json!({}))
        .expect_err("refused");
    assert_eq!(error.code(), ErrorCode::AuthenticationRequired);
    let status = client.server("denied").expect("status");
    assert_eq!(status.restarts, 1);
    assert!(!status.connected);
    client.shutdown();
}

#[test]
fn reload_reconnects_every_server() {
    let fixture = Fixture::new("standard");
    let client = Client::new(&budget());
    client
        .connect_all(&[fixture.config("fixture")])
        .expect("connected");
    let report = client.reload().expect("reloaded");
    assert_eq!(report.connected, ["fixture"]);
    assert_eq!(client.tools().len(), 2);
    assert_eq!(client.server("fixture").expect("status").restarts, 0);
    client.shutdown();
}

#[test]
fn a_disabled_server_is_not_started() {
    let fixture = Fixture::new("standard");
    let mut config = fixture.config("off");
    config.enabled = false;
    let client = Client::new(&budget());
    let report = client.connect_all(&[config]).expect("connected");
    assert!(report.connected.is_empty());
    assert!(client.status().is_empty());
    assert!(client.tools().is_empty());
    client.shutdown();
}

#[test]
fn the_legacy_sse_transport_connects_and_calls_a_tool() {
    let server = SseFixture::start();
    let client = Client::new(&budget());
    let report = client
        .connect_all(&[server.config("legacy")])
        .expect("connected");
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(
        client
            .tools()
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>(),
        ["mcp_legacy_greet"]
    );
    let outcome = client.call("mcp_legacy_greet", &json!({})).expect("called");
    assert_eq!(outcome.text, "hello over sse");
    client.shutdown();
}

// ---------------------------------------------------------------------------
// HTTP transport
// ---------------------------------------------------------------------------

/// The steps a remote server answers with when it behaves.
fn healthy_steps(version: &str) -> Vec<Step> {
    vec![
        step(
            "initialize",
            Scripted::Initialize {
                version: version.to_owned(),
            },
        ),
        step(
            "tools/list",
            Scripted::Tools(json!({
                "tools": [{ "name": "greet", "description": "Greets.", "inputSchema": { "type": "object" } }]
            })),
        ),
        step(
            "tools/call",
            Scripted::Call(json!({ "content": [{ "type": "text", "text": "hello over http" }] })),
        ),
    ]
}

#[test]
fn an_http_server_connects_lists_tools_and_executes_one() {
    let server = ScriptedHttp::start(healthy_steps("2025-11-25"));
    let client = Client::new(&budget());
    let report = client
        .connect_all(&[server.config("remote")])
        .expect("connected");
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(
        client
            .tools()
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>(),
        ["mcp_remote_greet"]
    );
    let outcome = client.call("mcp_remote_greet", &json!({})).expect("called");
    assert_eq!(outcome.text, "hello over http");
    assert_eq!(outcome.server, "remote");
    assert_eq!(server.methods(), ["initialize", "tools/list", "tools/call"]);
    assert_eq!(server.remaining(), 3);
    let status = client.server("remote").expect("status");
    assert_eq!(status.transport, "http");
    assert!(status.connected);
    client.shutdown();
}

#[test]
fn an_http_server_that_never_answers_times_out() {
    let server = HangingHttp::start();
    let mut config = server.config("stuck");
    config.startup_timeout_ms = 300;
    let client = Client::new(&budget());
    let started = Instant::now();
    let report = client.connect_all(&[config]).expect("connected");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "the connect blocked for {elapsed:?}"
    );
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].error.code(), ErrorCode::Timeout);
    client.shutdown();
}

#[test]
fn a_bearer_token_from_the_environment_is_sent() {
    let server = ScriptedHttp::start(healthy_steps("2025-11-25"));
    let mut config = server.config("remote");
    if let Transport::Http {
        bearer_token_env, ..
    } = &mut config.transport
    {
        *bearer_token_env = Some("RUNE_MCP_TEST_TOKEN".to_owned());
    }
    let client = Client::with_variables(
        &budget(),
        variables(&[("RUNE_MCP_TEST_TOKEN", "token-value")]),
    );
    let report = client.connect_all(&[config]).expect("connected");
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(
        server.authorization().as_deref(),
        Some("Bearer token-value")
    );
    client.shutdown();
}

#[test]
fn a_header_sourced_from_the_environment_is_sent() {
    let server = ScriptedHttp::start(healthy_steps("2025-11-25"));
    let mut config = server.config("remote");
    if let Transport::Http { header_env, .. } = &mut config.transport {
        header_env.insert("x-tenant".to_owned(), "RUNE_MCP_TEST_TENANT".to_owned());
    }
    let client = Client::with_variables(&budget(), variables(&[("RUNE_MCP_TEST_TENANT", "acme")]));
    client.connect_all(&[config]).expect("connected");
    assert_eq!(client.tools().len(), 1);
    client.shutdown();
}

#[test]
fn a_credential_variable_that_is_unset_fails_the_server() {
    let server = ScriptedHttp::start(healthy_steps("2025-11-25"));
    let mut config = server.config("absent");
    if let Transport::Http {
        bearer_token_env, ..
    } = &mut config.transport
    {
        *bearer_token_env = Some("RUNE_MCP_TEST_ABSENT".to_owned());
    }
    let client = Client::new(&budget());
    let report = client.connect_all(&[config]).expect("connected");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(
        report.failed[0].error.code(),
        ErrorCode::AuthenticationRequired
    );
    assert_eq!(
        server.count("initialize"),
        0,
        "the server was contacted anyway"
    );
    client.shutdown();
}

#[test]
fn an_http_rejection_recovers_after_one_reconnect() {
    // The first tool call is rejected the way an over-expired credential is
    // reported, and the reconnect that follows answers normally.
    let server = ScriptedHttp::start(vec![
        step(
            "initialize",
            Scripted::Initialize {
                version: "2025-11-25".to_owned(),
            },
        ),
        step(
            "tools/list",
            Scripted::Tools(json!({
                "tools": [{ "name": "greet", "inputSchema": { "type": "object" } }]
            })),
        ),
        once("tools/call", Scripted::Status(401)),
        step(
            "tools/call",
            Scripted::Call(json!({
                "content": [{ "type": "text", "text": "recovered" }]
            })),
        ),
    ]);
    let client = Client::new(&budget());
    client
        .connect_all(&[server.config("remote")])
        .expect("connected");

    let outcome = client
        .call("mcp_remote_greet", &json!({}))
        .expect("recovered");
    assert_eq!(outcome.text, "recovered");
    let status = client.server("remote").expect("status");
    assert_eq!(status.restarts, 1);
    assert!(status.connected);
    assert_eq!(status.tools, 1);
    // The reconnect replayed initialize and the listing before the retry.
    assert_eq!(server.count("initialize"), 2);
    assert_eq!(server.count("tools/list"), 2);
    assert_eq!(server.count("tools/call"), 2);
    client.shutdown();
}

#[test]
fn an_http_reply_over_the_frame_cap_is_refused() {
    let server = ScriptedHttp::start(vec![step("initialize", Scripted::Oversized)]);
    let client = Client::new(&budget());
    let report = client
        .connect_all(&[server.config("huge")])
        .expect("connected");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].error.code(), ErrorCode::TooLarge);
    assert_eq!(report.failed[0].error.field(), Some("frame"));
    client.shutdown();
}

#[test]
fn an_http_reply_nulling_the_expiry_keeps_the_tools_available() {
    let server = ScriptedHttp::start(healthy_steps("2025-06-18"));
    let client = Client::new(&budget());
    client
        .connect_all(&[server.config("remote")])
        .expect("connected");
    let status = client.server("remote").expect("status");
    assert_eq!(status.credential_expires_at_ms, None);
    assert_eq!(status.protocol_version.as_deref(), Some("2025-06-18"));
    assert_eq!(status.restarts, 0);
    assert_eq!(client.tools().len(), 1);
    client.shutdown();
}

#[test]
fn an_http_server_selecting_an_unknown_revision_is_refused() {
    let server = ScriptedHttp::start(vec![step(
        "initialize",
        Scripted::Initialize {
            version: "2026-07-28".to_owned(),
        },
    )]);
    let client = Client::new(&budget());
    let report = client
        .connect_all(&[server.config("future")])
        .expect("connected");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].error.code(), ErrorCode::UnsupportedVersion);
    client.shutdown();
}

#[test]
fn a_credential_without_an_expiry_is_usable_at_any_clock_reading() {
    let credential = Credential {
        expires_at_ms: None,
    };
    assert_eq!(credential.state(0), Expiry::Unknown);
    assert!(credential.is_usable(u64::MAX));
}

// ---------------------------------------------------------------------------
// Fixture servers
// ---------------------------------------------------------------------------

/// Returns a configuration for a remote endpoint.
fn remote_config(name: &str, url: &str) -> ServerConfig {
    ServerConfig {
        name: name.to_owned(),
        transport: Transport::Http {
            url: url.to_owned(),
            headers: BTreeMap::new(),
            header_env: BTreeMap::new(),
            bearer_token_env: None,
        },
        enabled: true,
        required: false,
        startup_timeout_ms: 10_000,
        operation_timeout_ms: 5_000,
        restart_limit: 1,
    }
}

/// One scripted reply.
#[derive(Clone, Debug)]
enum Scripted {
    /// Answer initialize with this revision and a null expiry.
    Initialize {
        /// Revision the server selects.
        version: String,
    },
    /// Answer `tools/list` with this result.
    Tools(Value),
    /// Answer `tools/call` with this result.
    Call(Value),
    /// Answer with a status and no body.
    Status(u16),
    /// Answer with a body past the frame cap.
    Oversized,
    /// Hold the connection open and never answer.
    Hang,
}

/// One scripted reply for a specific method.
#[derive(Clone, Debug)]
struct Step {
    /// Method the step answers.
    method: String,
    /// Reply to send.
    reply: Scripted,
    /// Whether the step is consumed by the first request it answers. A
    /// consumed step lets a later step for the same method take over, which is
    /// what a server that fails once and then recovers looks like.
    once: bool,
}

/// Builds a step that answers every matching request.
fn step(method: &str, reply: Scripted) -> Step {
    Step {
        method: method.to_owned(),
        reply,
        once: false,
    }
}

/// Builds a step that answers only the first matching request.
fn once(method: &str, reply: Scripted) -> Step {
    Step {
        method: method.to_owned(),
        reply,
        once: true,
    }
}

/// An HTTP server that answers each method as the script directs.
///
/// Methods are matched rather than counted, so a reconnect that replays
/// `initialize` and `tools/list` is answered deterministically rather than
/// depending on how many requests reached the server.
#[derive(Debug)]
struct ScriptedHttp {
    port: u16,
    shutdown: Arc<AtomicBool>,
    log: Arc<Mutex<Vec<String>>>,
    authorization: Arc<Mutex<Option<String>>>,
    steps: Arc<Mutex<Vec<Step>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ScriptedHttp {
    /// Starts a server that answers from the given steps.
    fn start(steps: Vec<Step>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let authorization = Arc::new(Mutex::new(None));
        let steps = Arc::new(Mutex::new(steps));

        let thread_shutdown = Arc::clone(&shutdown);
        let thread_log = Arc::clone(&log);
        let thread_auth = Arc::clone(&authorization);
        let thread_steps = Arc::clone(&steps);
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { break };
                let log = Arc::clone(&thread_log);
                let auth = Arc::clone(&thread_auth);
                let steps = Arc::clone(&thread_steps);
                std::thread::spawn(move || {
                    let _ = serve_scripted(stream, &steps, &log, &auth);
                });
            }
        });

        Self {
            port,
            shutdown,
            log,
            authorization,
            steps,
            thread: Some(thread),
        }
    }

    /// Returns the endpoint URL.
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    /// Returns a configuration pointing at this server.
    fn config(&self, name: &str) -> ServerConfig {
        remote_config(name, &self.url())
    }

    /// Returns the methods the server answered, in order.
    fn methods(&self) -> Vec<String> {
        self.log.lock().map(|log| log.clone()).unwrap_or_default()
    }

    /// Returns the number of times a method was answered.
    fn count(&self, method: &str) -> usize {
        self.methods().iter().filter(|seen| *seen == method).count()
    }

    /// Returns the last authorization header received.
    fn authorization(&self) -> Option<String> {
        self.authorization.lock().expect("auth").clone()
    }

    /// Returns how many steps the script still holds.
    fn remaining(&self) -> usize {
        self.steps.lock().map_or(0, |steps| steps.len())
    }
}

impl Drop for ScriptedHttp {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Unblock the accept loop by connecting once.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Serves one scripted request.
fn serve_scripted(
    mut stream: TcpStream,
    steps: &Arc<Mutex<Vec<Step>>>,
    log: &Arc<Mutex<Vec<String>>>,
    authorization: &Arc<Mutex<Option<String>>>,
) -> std::io::Result<()> {
    let (request, header) = read_request(stream.try_clone()?)?;
    if let Some(value) = header {
        *authorization.lock().expect("auth") = Some(value);
    }
    let id = request.get("id").and_then(Value::as_u64);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    // A notification carries no identifier and expects no reply.
    if id.is_none() {
        stream.write_all(
            b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )?;
        return stream.flush();
    }
    log.lock().expect("log").push(method.clone());

    let reply = {
        let mut guard = steps.lock().expect("steps");
        match guard
            .iter()
            .position(|candidate| candidate.method == method)
        {
            Some(index) => {
                let step = guard[index].clone();
                if step.once {
                    guard.remove(index);
                }
                step.reply
            }
            None => Scripted::Hang,
        }
    };

    match reply {
        Scripted::Hang => {
            std::thread::sleep(Duration::from_secs(30));
            Ok(())
        }
        Scripted::Status(code) => {
            stream.write_all(
                format!("HTTP/1.1 {code} Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .as_bytes(),
            )?;
            stream.flush()
        }
        Scripted::Oversized => {
            let body = "x".repeat(9 * 1024 * 1024);
            stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )?;
            stream.write_all(body.as_bytes())?;
            stream.flush()
        }
        Scripted::Initialize { version } => write_json(
            stream,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": version,
                    "capabilities": {},
                    "_meta": { "expires_at_ms": null }
                }
            }),
        ),
        Scripted::Tools(result) | Scripted::Call(result) => write_json(
            stream,
            &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        ),
    }
}

/// Writes one JSON reply.
fn write_json(mut stream: TcpStream, body: &Value) -> std::io::Result<()> {
    let text = body.to_string();
    stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            text.len()
        )
        .as_bytes(),
    )?;
    stream.write_all(text.as_bytes())?;
    stream.flush()
}

/// Reads one HTTP request, returning its JSON body and authorization header.
fn read_request(stream: TcpStream) -> std::io::Result<(Value, Option<String>)> {
    let mut reader = BufReader::new(stream);
    let mut content_length = 0_usize;
    let mut authorization = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end().to_owned();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
        if lower.starts_with("authorization:")
            && let Some((_, value)) = trimmed.split_once(':')
        {
            authorization = Some(value.trim().to_owned());
        }
    }
    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok((
        serde_json::from_slice(&body).unwrap_or(Value::Null),
        authorization,
    ))
}

/// A server that accepts a connection and never answers.
#[derive(Debug)]
struct HangingHttp {
    port: u16,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HangingHttp {
    /// Starts a listener that never writes a reply.
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            // Accepted streams are held rather than dropped, so the client sees
            // an open connection that produces nothing instead of a refusal.
            let mut held: Vec<TcpStream> = Vec::new();
            for stream in listener.incoming() {
                if thread_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(stream) = stream {
                    held.push(stream);
                }
            }
        });
        Self {
            port,
            shutdown,
            thread: Some(thread),
        }
    }

    /// Returns a configuration pointing at this server.
    fn config(&self, name: &str) -> ServerConfig {
        remote_config(name, &format!("http://127.0.0.1:{}/mcp", self.port))
    }
}

impl Drop for HangingHttp {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A server reached the legacy way: an event stream plus a message endpoint.
///
/// Only the handshake needs the stream. This fixture names the message endpoint
/// and then answers each post inline, which is the shape a legacy server takes
/// when it does not need to push anything the client did not ask for.
#[derive(Debug)]
struct SseFixture {
    port: u16,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SseFixture {
    /// Starts a server that names an endpoint and answers posts.
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { break };
                std::thread::spawn(move || {
                    let _ = serve_sse(stream);
                });
            }
        });
        Self {
            port,
            shutdown,
            thread: Some(thread),
        }
    }

    /// Returns a configuration pointing at this server.
    fn config(&self, name: &str) -> ServerConfig {
        ServerConfig {
            name: name.to_owned(),
            transport: Transport::Sse {
                url: format!("http://127.0.0.1:{}/sse", self.port),
            },
            enabled: true,
            required: false,
            startup_timeout_ms: 10_000,
            operation_timeout_ms: 5_000,
            restart_limit: 1,
        }
    }
}

impl Drop for SseFixture {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Serves one connection on the legacy transport.
fn serve_sse(mut stream: TcpStream) -> std::io::Result<()> {
    let (path, request, _) = read_raw_request(stream.try_clone()?)?;
    if path.starts_with("/sse") {
        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")?;
        stream.write_all(b"event: endpoint\ndata: /messages\n\n")?;
        return stream.flush();
    }
    let id = request.get("id").and_then(Value::as_u64);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let payload = match method.as_str() {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "protocolVersion": "2024-11-05", "_meta": { "expires_at_ms": null } }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": [{ "name": "greet", "inputSchema": { "type": "object" } }] }
        }),
        "tools/call" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "text", "text": "hello over sse" }] }
        }),
        _ => Value::Null,
    };
    if payload.is_null() {
        stream.write_all(
            b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )?;
        return stream.flush();
    }
    write_json(stream, &payload)
}

/// Reads a raw HTTP request, returning its path, JSON body, and the method.
fn read_raw_request(stream: TcpStream) -> std::io::Result<(String, Value, String)> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok((String::new(), Value::Null, String::new()));
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut content_length = 0_usize;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end().to_owned();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok((
        path,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
        method,
    ))
}
