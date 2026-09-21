// Integration tests assert by panicking.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! End-to-end tests for the protocol server.
//!
//! A real server runs on its own thread against a real HTTP endpoint, and a
//! client drives it over a socket pair. The property most often under test is
//! that a second prompt arriving during a live turn is admitted rather than
//! refused, so the client sends frames while a turn is genuinely in flight.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use rune_acp::jsonrpc::MAX_FRAME_BYTES;
use rune_acp::server::{Dialect, PROTOCOL_VERSION, Server, ServerConfig};
use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::config::{Layer, PermissionMode};
use rune_core::paths::Paths;
use rune_net::transport::Endpoint;
use rune_testkit::{MockEndpoint, Script};
use serde_json::{Value, json};

/// Time a test waits for a frame before failing, so a defect reports rather
/// than hangs.
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// Builds a state layout rooted at a temporary directory.
fn paths_for(root: &Path) -> Paths {
    let base = |name: &str| {
        Utf8PathBuf::from_path_buf(root.join(name)).expect("the fixture path is UTF-8")
    };
    Paths {
        config_root: base("config"),
        state_root: base("state"),
        data_root: base("data"),
    }
}

/// How the client answers an approval request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Approval {
    /// Approve this call only.
    Once,
    /// Approve every call for the rest of the session.
    Always,
    /// Refuse the call.
    Refuse,
    /// Leave the request unanswered, so the turn stays blocked on it.
    Ignore,
}

/// A client driving the server over a socket.
struct Client {
    to_server: UnixStream,
    from_server: BufReader<UnixStream>,
    /// Responses that arrived before the test asked for them.
    responses: Vec<Value>,
    /// The order response identifiers were answered in.
    order: Vec<i64>,
    /// Every notification received.
    notifications: Vec<Value>,
    /// Every error response received.
    errors: Vec<Value>,
    /// Every approval request received.
    approvals: Vec<Value>,
    /// How to answer the next approval request.
    approval: Approval,
}

impl Client {
    fn new(stream: UnixStream) -> Self {
        let to_server = stream.try_clone().expect("clone");
        Self {
            to_server,
            from_server: BufReader::new(stream),
            responses: Vec::new(),
            order: Vec::new(),
            notifications: Vec::new(),
            errors: Vec::new(),
            approvals: Vec::new(),
            approval: Approval::Once,
        }
    }

    /// Writes one frame.
    fn send(&mut self, frame: &Value) {
        writeln!(self.to_server, "{frame}").expect("write a frame");
        self.to_server.flush().expect("flush a frame");
    }

    /// Sends a request.
    fn request(&mut self, id: i64, method: &str, params: &Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    /// Sends a notification.
    fn notify(&mut self, method: &str, params: &Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Reads one frame, answering any approval request the server issues.
    ///
    /// Answering here rather than in the test body is what keeps a turn from
    /// blocking while the test waits for an unrelated frame.
    fn next_frame(&mut self) -> Value {
        let deadline = Instant::now().checked_add(FRAME_TIMEOUT);
        loop {
            let expired = deadline.is_none_or(|deadline| Instant::now() >= deadline);
            assert!(
                !expired,
                "the server sent no frame within {FRAME_TIMEOUT:?}"
            );
            let mut line = String::new();
            match self.from_server.read_line(&mut line) {
                Ok(0) => panic!("the server closed the connection"),
                Ok(_) => {}
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(err) => panic!("could not read a frame: {err}"),
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let frame: Value = serde_json::from_str(trimmed).unwrap_or_else(|err| {
                panic!("the server wrote a non-frame line `{trimmed}`: {err}")
            });
            if let Some(answer) = self.answer_for(&frame) {
                self.send(&answer);
                continue;
            }
            return frame;
        }
    }

    /// Returns the answer to send for an incoming approval request.
    fn answer_for(&mut self, frame: &Value) -> Option<Value> {
        if frame.get("method").and_then(Value::as_str) != Some("session/request_permission") {
            return None;
        }
        let id = frame.get("id").cloned().expect("the request carries an id");
        let option = match self.approval {
            Approval::Once => "allow_once",
            Approval::Always => "allow_always",
            Approval::Refuse => "reject_once",
            Approval::Ignore => return None,
        };
        self.approvals.push(frame.clone());
        Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"outcome": {"outcome": "selected", "optionId": option}},
        }))
    }

    /// Files an incoming frame.
    fn record(&mut self, frame: Value) {
        if frame.get("method").is_none() {
            if frame.get("error").is_some_and(|error| !error.is_null()) {
                self.errors.push(frame.clone());
            }
            if let Some(id) = frame.get("id").and_then(Value::as_i64) {
                self.order.push(id);
            }
            self.responses.push(frame);
            return;
        }
        self.notifications.push(frame);
    }

    /// Reads until the response with a given identifier arrives.
    fn response(&mut self, id: i64) -> Value {
        if let Some(index) = self
            .responses
            .iter()
            .position(|frame| frame.get("id").and_then(Value::as_i64) == Some(id))
        {
            return self.responses.remove(index);
        }
        loop {
            let frame = self.next_frame();
            let matches = frame.get("id").and_then(Value::as_i64) == Some(id);
            self.record(frame.clone());
            if matches {
                return frame;
            }
        }
    }

    /// Sends a request and waits for its response.
    fn call(&mut self, id: i64, method: &str, params: &Value) -> Value {
        self.request(id, method, params);
        self.response(id)
    }

    /// Reads until one notification satisfies a predicate, returning it.
    fn update_until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        if let Some(found) = self.notifications.iter().find(|frame| predicate(frame)) {
            return found.clone();
        }
        loop {
            let frame = self.next_frame();
            if frame.get("method").is_none() {
                self.record(frame);
                continue;
            }
            let matches = predicate(&frame);
            self.record(frame.clone());
            if matches {
                return frame;
            }
        }
    }

    /// Reads until the server asks the client to approve a tool call.
    fn await_permission_request(&mut self) -> Value {
        self.update_until(|frame| {
            frame.get("method").and_then(Value::as_str) == Some("session/request_permission")
        })
    }

    /// Reads until a prompt of the given text is acknowledged.
    ///
    /// The acknowledgement is written before the turn reaches the model, so once
    /// it arrives the turn is provably in flight.
    fn await_acknowledgement(&mut self, text: &str) -> Value {
        self.update_until(|frame| {
            is_update(frame, "user_message_chunk")
                && frame["params"]["update"]["content"]["text"] == text
        })
    }

    /// Returns every update received whose sessionUpdate tag matches.
    fn updates(&self, tag: &str) -> Vec<Value> {
        self.notifications
            .iter()
            .filter(|frame| is_update(frame, tag))
            .cloned()
            .collect()
    }
}

/// Builds the prompt params for one user message.
fn prompt(session: &str, text: &str) -> Value {
    json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]})
}

/// Returns true when a frame is a session update carrying a given tag.
fn is_update(frame: &Value, tag: &str) -> bool {
    frame.get("method").and_then(Value::as_str) == Some("session/update")
        && frame["params"]["update"]["sessionUpdate"] == tag
}

/// A server running on its own thread with a client attached.
struct Harness {
    client: Option<Client>,
    join: Option<std::thread::JoinHandle<()>>,
    /// Held so the endpoint outlives every turn.
    mock: MockEndpoint,
    /// Where the server writes its diagnostics.
    log_file: Utf8PathBuf,
    /// Held so the fixture directory outlives the server.
    _root: tempfile::TempDir,
}

impl Harness {
    /// Starts a server against a scripted endpoint.
    fn start(scripts: Vec<Script>, mode: PermissionMode) -> Self {
        Self::start_with(scripts, mode, BudgetSet::new(), Vec::new())
    }

    /// Starts a server with explicit limits, rules, and fixture files.
    fn start_with(
        scripts: Vec<Script>,
        mode: PermissionMode,
        limits: BudgetSet,
        files: Vec<(&str, &str)>,
    ) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace =
            Utf8PathBuf::from_path_buf(root.path().join("workspace")).expect("utf8 workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        for (name, contents) in files {
            std::fs::write(workspace.join(name), contents).expect("fixture file");
        }

        let mock = MockEndpoint::start(scripts);
        let log_file =
            Utf8PathBuf::from_path_buf(root.path().join("diagnostics.log")).expect("utf8 log path");
        let config = ServerConfig::new(
            paths_for(root.path()),
            workspace,
            Endpoint::new(mock.base_url(), "test-key"),
            "test/model",
        )
        .expect("config")
        .with_dialect(Dialect::ChatCompletions)
        .with_mode(mode)
        .with_limits(limits)
        .expect("limits")
        .with_log_file(Some(log_file.clone()));

        let (server_side, client_side) = UnixStream::pair().expect("socket pair");
        let output = server_side.try_clone().expect("clone");
        let server = Arc::new(Server::new(config, output).expect("server"));
        let join = std::thread::spawn(move || {
            let _ = server.run(BufReader::new(server_side));
        });

        Self {
            client: Some(Client::new(client_side)),
            join: Some(join),
            mock,
            log_file,
            _root: root,
        }
    }

    /// Returns the diagnostics the server has written so far.
    fn logs(&self) -> String {
        std::fs::read_to_string(&self.log_file).unwrap_or_default()
    }

    /// Returns the client.
    fn client(&mut self) -> &mut Client {
        self.client.as_mut().expect("client")
    }

    /// Initializes the connection and opens a session, returning its identifier.
    fn open_session(&mut self) -> String {
        let response = self.client().call(
            1,
            "initialize",
            &json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {},
                "clientInfo": {"name": "test", "version": "0"},
            }),
        );
        assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSION);
        let response = self
            .client()
            .call(2, "session/new", &json!({"cwd": "/", "mcpServers": []}));
        response["result"]["sessionId"]
            .as_str()
            .expect("a session id")
            .to_owned()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            let _ = client.to_server.shutdown(std::net::Shutdown::Both);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[test]
fn initialize_advertises_this_protocol_version_and_capabilities() {
    let mut harness = Harness::start(vec![Script::text("unused")], PermissionMode::Auto);
    let response = harness.client().call(
        1,
        "initialize",
        &json!({"protocolVersion": PROTOCOL_VERSION, "clientInfo": {"name": "test"}}),
    );
    assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSION);
    assert_eq!(response["result"]["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        response["result"]["agentCapabilities"]["sessionCapabilities"]["resume"],
        json!({})
    );
    assert_eq!(response["result"]["agentInfo"]["name"], "rune");
    assert_eq!(response["result"]["authMethods"], json!([]));
}

#[test]
fn a_new_session_is_listed_with_its_identifier() {
    let mut harness = Harness::start(vec![Script::text("unused")], PermissionMode::Ask);
    let session = harness.open_session();
    assert_eq!(session.len(), 12, "a session id is twelve characters");
    let response = harness.client().call(3, "session/list", &json!({}));
    let listed = response["result"]["sessions"].as_array().expect("sessions");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["sessionId"], session.as_str());
}

#[test]
fn a_prompt_while_a_turn_is_running_is_queued_rather_than_refused() {
    // The first turn retries before it succeeds, which keeps it in flight while
    // the client sends the second prompt.
    let mut harness = Harness::start(
        vec![Script::text("one"), Script::text("two")],
        PermissionMode::Auto,
    );
    harness.mock.fail_first(2);
    let session = harness.open_session();

    harness
        .client()
        .request(3, "session/prompt", &prompt(&session, "first"));
    // The acknowledgement is written before the model is asked, so the turn is
    // provably running once it arrives.
    harness.client().await_acknowledgement("first");
    harness
        .client()
        .request(4, "session/prompt", &prompt(&session, "second"));

    let first = harness.client().response(3);
    let second = harness.client().response(4);
    assert_eq!(first["result"]["stopReason"], "end_turn");
    assert_eq!(second["result"]["stopReason"], "end_turn");

    let client = harness.client();
    assert!(
        client.errors.is_empty(),
        "a queued prompt produced an error: {:?}",
        client.errors
    );
    let position = |id: i64| client.order.iter().position(|seen| *seen == id);
    assert!(
        position(3) < position(4),
        "the first turn was answered after the second: {:?}",
        client.order
    );
    assert_eq!(
        client.updates("agent_message_chunk").len(),
        2,
        "both turns answered"
    );
    assert_eq!(
        harness.mock.request_count(),
        4,
        "two failures then two successes"
    );
    assert!(
        harness.logs().contains("queued a prompt"),
        "the second prompt did not take the queue path:\n{}",
        harness.logs()
    );
}

#[test]
fn the_prompt_queue_is_bounded() {
    let mut limits = BudgetSet::new();
    limits
        .set(
            LimitName::SteeringQueueDepth,
            Budget::Bounded(1),
            Layer::User,
        )
        .expect("set");
    let mut harness = Harness::start_with(
        vec![Script::text("one"), Script::text("two")],
        PermissionMode::Auto,
        limits,
        Vec::new(),
    );
    harness.mock.fail_first(3);
    let session = harness.open_session();

    harness
        .client()
        .request(3, "session/prompt", &prompt(&session, "first"));
    harness.client().await_acknowledgement("first");
    harness
        .client()
        .request(4, "session/prompt", &prompt(&session, "second"));
    harness
        .client()
        .request(5, "session/prompt", &prompt(&session, "third"));

    let refused = harness.client().response(5);
    assert_eq!(refused["error"]["code"], -32602);
    assert_eq!(refused["error"]["data"]["code"], "limit_exceeded");

    // The prompts that were admitted still run.
    let first = harness.client().response(3);
    let second = harness.client().response(4);
    assert_eq!(first["result"]["stopReason"], "end_turn");
    assert_eq!(second["result"]["stopReason"], "end_turn");
}

#[test]
fn cancelling_stops_the_active_turn_and_the_connection_stays_usable() {
    // The turn blocks on an approval the client never answers, so it is provably
    // in flight when the cancel arrives.
    let mut harness = Harness::start_with(
        vec![
            Script::tool_call("c1", "read_file", r#"{"path":"notes.txt"}"#),
            Script::text("after the cancel"),
        ],
        PermissionMode::Ask,
        BudgetSet::new(),
        vec![("notes.txt", "hello\n")],
    );
    harness.client().approval = Approval::Ignore;
    let session = harness.open_session();

    harness
        .client()
        .request(3, "session/prompt", &prompt(&session, "long"));
    let pending = harness.client().await_permission_request();
    assert_eq!(pending["params"]["sessionId"], session.as_str());
    harness
        .client()
        .notify("session/cancel", &json!({"sessionId": session}));

    let cancelled = harness.client().response(3);
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");

    // The same session accepts another turn, and the connection still answers.
    harness.client().call(
        4,
        "session/set_mode",
        &json!({"sessionId": session, "modeId": "full_access"}),
    );
    harness
        .client()
        .request(5, "session/prompt", &prompt(&session, "again"));
    let again = harness.client().response(5);
    assert_eq!(again["result"]["stopReason"], "end_turn");
    let alive = harness.client().call(6, "session/list", &json!({}));
    assert!(alive["result"]["sessions"].is_array());
}

#[test]
fn loading_replays_a_stored_conversation_while_resuming_does_not() {
    let mut harness = Harness::start(vec![Script::text("answer")], PermissionMode::Auto);
    let session = harness.open_session();
    harness
        .client()
        .request(3, "session/prompt", &prompt(&session, "question"));
    let answered = harness.client().response(3);
    assert_eq!(answered["result"]["stopReason"], "end_turn");
    harness
        .client()
        .call(4, "session/close", &json!({"sessionId": session}));

    let before = harness.client().notifications.len();
    harness
        .client()
        .call(5, "session/load", &json!({"sessionId": session}));
    let replayed: Vec<Value> = harness.client().notifications[before..].to_vec();
    assert_eq!(
        replayed
            .iter()
            .filter(|frame| is_update(frame, "user_message_chunk"))
            .count(),
        1,
        "loading replays what the user said"
    );
    assert_eq!(
        replayed
            .iter()
            .filter(|frame| is_update(frame, "agent_message_chunk"))
            .count(),
        1,
        "loading replays what the model answered"
    );
    harness
        .client()
        .call(6, "session/close", &json!({"sessionId": session}));

    let before = harness.client().notifications.len();
    harness
        .client()
        .call(7, "session/resume", &json!({"sessionId": session}));
    assert_eq!(
        harness.client().notifications.len(),
        before,
        "resuming replays nothing"
    );
}

#[test]
fn an_unresolved_tool_call_is_reported_to_the_client_for_approval() {
    let mut harness = Harness::start_with(
        vec![
            Script::tool_call("c1", "read_file", r#"{"path":"notes.txt"}"#),
            Script::text("done"),
        ],
        PermissionMode::Ask,
        BudgetSet::new(),
        vec![("notes.txt", "hello from the fixture\n")],
    );
    harness.client().approval = Approval::Always;
    let session = harness.open_session();
    harness
        .client()
        .request(3, "session/prompt", &prompt(&session, "read it"));
    let answered = harness.client().response(3);
    assert_eq!(answered["result"]["stopReason"], "end_turn");

    let approvals = harness.client().approvals.clone();
    assert_eq!(approvals.len(), 1, "the call needed one approval");
    let params = &approvals[0]["params"];
    assert_eq!(params["sessionId"], session.as_str());
    assert_eq!(params["toolCall"]["name"], "read_file");
    assert_eq!(params["options"][0]["kind"], "allow_once");
    assert_eq!(params["options"][1]["kind"], "allow_always");

    let calls = harness.client().updates("tool_call");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["params"]["update"]["toolCallId"], "c1");
    assert_eq!(calls[0]["params"]["update"]["kind"], "read");
    let finished = harness.client().updates("tool_call_update");
    assert_eq!(finished[0]["params"]["update"]["status"], "completed");
}

#[test]
fn a_refused_tool_call_is_reported_as_failed_and_the_turn_continues() {
    let mut harness = Harness::start_with(
        vec![
            Script::tool_call("c1", "read_file", r#"{"path":"notes.txt"}"#),
            Script::text("carried on"),
        ],
        PermissionMode::Ask,
        BudgetSet::new(),
        vec![("notes.txt", "secret\n")],
    );
    harness.client().approval = Approval::Refuse;
    let session = harness.open_session();
    harness
        .client()
        .request(3, "session/prompt", &prompt(&session, "read it"));
    let answered = harness.client().response(3);
    assert_eq!(answered["result"]["stopReason"], "end_turn");
    assert_eq!(
        harness.client().updates("tool_call").len(),
        0,
        "a refused call is never announced as running"
    );
    assert!(
        !harness.client().updates("agent_message_chunk").is_empty(),
        "the model was told the call was refused and carried on"
    );
}

#[test]
fn an_unknown_method_is_reported_and_the_connection_survives() {
    let mut harness = Harness::start(vec![Script::text("unused")], PermissionMode::Auto);
    let refused =
        harness
            .client()
            .call(1, "session/teleport", &json!({"sessionId": "aaaaaaaaaaaa"}));
    assert_eq!(refused["error"]["code"], -32601);
    assert!(
        refused["error"]["message"]
            .as_str()
            .expect("message")
            .contains("session/teleport")
    );

    let alive = harness.client().call(
        2,
        "initialize",
        &json!({"protocolVersion": PROTOCOL_VERSION}),
    );
    assert_eq!(alive["result"]["protocolVersion"], PROTOCOL_VERSION);
}

#[test]
fn a_frame_over_the_cap_is_rejected_and_the_connection_survives() {
    let mut harness = Harness::start(vec![Script::text("unused")], PermissionMode::Auto);
    let oversized = "x".repeat(MAX_FRAME_BYTES.saturating_add(1));
    harness.client().send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/prompt",
        "params": {"sessionId": oversized},
    }));

    let rejection = loop {
        let frame = harness.client().next_frame();
        if frame.get("error").is_some_and(|error| !error.is_null()) {
            break frame;
        }
        harness.client().record(frame);
    };
    assert_eq!(rejection["error"]["code"], -32600);
    assert_eq!(rejection["error"]["data"]["code"], "too_large");
    assert!(
        rejection["id"].is_null(),
        "the identifier was unrecoverable"
    );

    let alive = harness.client().call(
        2,
        "initialize",
        &json!({"protocolVersion": PROTOCOL_VERSION}),
    );
    assert_eq!(alive["result"]["protocolVersion"], PROTOCOL_VERSION);
}

#[test]
fn every_line_the_server_writes_is_a_protocol_frame() {
    let mut harness = Harness::start(vec![Script::text("answer")], PermissionMode::Auto);
    let session = harness.open_session();
    harness
        .client()
        .request(3, "session/prompt", &prompt(&session, "question"));
    harness.client().response(3);
    harness
        .client()
        .call(4, "session/close", &json!({"sessionId": session}));

    // Every frame the client read was parsed as JSON-RPC, so a stray diagnostic
    // line would already have failed the run. This asserts the traffic was not
    // trivially empty.
    let client = harness.client();
    assert!(!client.notifications.is_empty());
    for frame in &client.notifications {
        assert_eq!(frame["jsonrpc"], "2.0");
        assert!(frame["method"].is_string());
    }
}
