//! Regression tests for losing only the protocol output during live work.

use std::io::{BufReader, Read};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

use rune_testkit::{MockEndpoint, Script};
use rune_tools::contract::{Activity, Tool};

use super::*;

fn config(root: &tempfile::TempDir, endpoint: Endpoint) -> ServerConfig {
    let path = |name: &str| {
        Utf8PathBuf::from_path_buf(root.path().join(name)).expect("UTF-8 fixture path")
    };
    ServerConfig::new(
        Paths {
            config_root: path("config"),
            state_root: path("state"),
            data_root: path("data"),
        },
        Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("workspace"),
        endpoint,
        "test/model",
    )
    .expect("config")
    .with_mode(PermissionMode::FullAccess)
    .with_log_file(Some(path("diagnostics.log")))
}

fn open<W: Write + Send + 'static>(server: &Arc<Server<W>>) -> String {
    server.new_session(&json!({})).expect("session")["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned()
}

fn prompt<W: Write + Send + 'static>(server: &Arc<Server<W>>, session: &str, id: i64) {
    server
        .prompt(
            &json!({"sessionId": session, "prompt": [{"type": "text", "text": "work"}]}),
            Some(Id::Number(id)),
        )
        .expect("prompt");
}

fn assert_settled<W: Write + Send + 'static>(server: &Server<W>, started: Instant) {
    let remaining = SHUTDOWN_GRACE.saturating_sub(started.elapsed());
    let (active, _) = server
        .settled
        .wait_timeout_while(server.active.lock().expect("active"), remaining, |active| {
            active.cancellation.is_some()
        })
        .expect("settled wait");
    assert!(
        active.cancellation.is_none(),
        "turn exceeded shutdown grace"
    );
    assert!(
        active.queue.is_empty(),
        "disconnected prompts were retained"
    );
    assert!(started.elapsed() < SHUTDOWN_GRACE);
}

#[test]
fn output_loss_on_the_turn_thread_closes_a_silent_provider_and_drops_queued_work() {
    let root = tempfile::tempdir().expect("tempdir");
    let listener = TcpListener::bind("127.0.0.1:0").expect("provider listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = Endpoint::new(
        format!("http://{}/v1", listener.local_addr().expect("address")),
        "key",
    );
    let (client, output) = std::io::pipe().expect("protocol output pipe");
    let server = Arc::new(Server::new(config(&root, endpoint), output).expect("server"));
    let session = open(&server);
    prompt(&server, &session, 1);

    let accepted = Instant::now();
    let mut provider = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(accepted.elapsed() < SHUTDOWN_GRACE, "no provider request");
                std::thread::sleep(POLL);
            }
            Err(err) => panic!("accept provider: {err}"),
        }
    };
    provider
        .set_read_timeout(Some(SHUTDOWN_GRACE))
        .expect("timeout");
    let mut request = BufReader::new(provider.try_clone().expect("clone"));
    let mut length = 0;
    loop {
        let mut line = String::new();
        assert!(request.read_line(&mut line).expect("header") > 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().expect("content length");
        }
    }
    request.read_exact(&mut vec![0; length]).expect("body");
    provider
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .expect("response headers");
    prompt(&server, &session, 2);
    assert_eq!(server.active.lock().expect("active").queue.len(), 1);

    // Only output is closed. Input EOF must not be responsible for cancellation.
    drop(client);
    let started = Instant::now();
    provider
        .write_all(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"trigger\"}}]}\n\n")
        .expect("streamed delta");
    // Leave the provider body open and silent. Cancellation must close it.
    assert_eq!(provider.read(&mut [0]).expect("provider disconnected"), 0);
    assert_settled(&server, started);
    assert!(server.output_failed.load(Ordering::SeqCst));
    assert!(
        server
            .prompt(
                &json!({"sessionId": session, "prompt": [{"type": "text", "text": "later"}]}),
                Some(Id::Number(3))
            )
            .is_err()
    );
    assert!(matches!(listener.accept(), Err(err) if err.kind() == std::io::ErrorKind::WouldBlock));
}

struct WaitingTool {
    started: mpsc::Sender<()>,
    stopped: mpsc::Sender<()>,
}

impl Tool for WaitingTool {
    fn name(&self) -> &'static str {
        "wait_for_cancel"
    }
    fn description(&self) -> &'static str {
        "Wait until cancelled."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn activity(&self) -> Activity {
        Activity::Execute
    }
    fn call(&self, _: &Value, context: &ExecutionContext) -> Result<ToolOutput> {
        self.started.send(()).expect("started");
        let started = Instant::now();
        while context.check_cancelled().is_ok() {
            assert!(
                started.elapsed() < SHUTDOWN_GRACE.saturating_mul(2),
                "tool was not cancelled"
            );
            std::thread::sleep(POLL);
        }
        self.stopped.send(()).expect("stopped");
        context.check_cancelled()?;
        Ok(ToolOutput::success("finished"))
    }
}

#[test]
fn output_loss_on_the_reader_thread_cancels_running_tools() {
    let root = tempfile::tempdir().expect("tempdir");
    let mock = MockEndpoint::start(vec![Script::tool_call("call", "wait_for_cancel", "{}")]);
    let (started_tx, started_rx) = mpsc::channel();
    let (stopped_tx, stopped_rx) = mpsc::channel();
    let mut registry = Registry::new();
    registry
        .insert(Box::new(WaitingTool {
            started: started_tx,
            stopped: stopped_tx,
        }))
        .expect("tool");
    let (client, output) = std::io::pipe().expect("protocol output pipe");
    let server = Arc::new(
        Server::new(
            config(&root, Endpoint::new(mock.base_url(), "key")).with_registry(registry),
            output,
        )
        .expect("server"),
    );
    let session = open(&server);
    prompt(&server, &session, 1);
    started_rx
        .recv_timeout(SHUTDOWN_GRACE)
        .expect("tool running");
    prompt(&server, &session, 2);
    drop(client);
    let started = Instant::now();
    server.dispatch(Message::Request(Request::new(
        Id::Number(3),
        "initialize",
        json!({}),
    )));
    stopped_rx
        .recv_timeout(SHUTDOWN_GRACE)
        .expect("tool cancelled");
    assert_settled(&server, started);
    assert_eq!(mock.request_count(), 1);
    // Further messages on the still-open input must not start work.
    server.dispatch(Message::Request(Request::new(
        Id::Number(4),
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "later"}]}),
    )));
    assert!(server.active.lock().expect("active").cancellation.is_none());
}

#[test]
fn output_loss_releases_an_unanswered_permission_request() {
    let root = tempfile::tempdir().expect("tempdir");
    let mock = MockEndpoint::start(vec![Script::tool_call(
        "call",
        "read_file",
        r#"{"path":"file"}"#,
    )]);
    let (output, client) = UnixStream::pair().expect("protocol output");
    client
        .set_read_timeout(Some(SHUTDOWN_GRACE))
        .expect("timeout");
    let server = Arc::new(
        Server::new(
            config(&root, Endpoint::new(mock.base_url(), "key")).with_mode(PermissionMode::Ask),
            output,
        )
        .expect("server"),
    );
    let session = open(&server);
    prompt(&server, &session, 1);
    let mut reader =
        crate::jsonrpc::Reader::new(BufReader::new(client.try_clone().expect("clone")));
    loop {
        if let Some(Message::Request(request)) = reader.read_message().expect("frame")
            && request.method == "session/request_permission"
        {
            break;
        }
    }
    assert_eq!(server.pending.lock().expect("pending").len(), 1);
    client
        .shutdown(std::net::Shutdown::Read)
        .expect("close output");
    let started = Instant::now();
    server.respond(Id::Number(2), json!({}));
    assert_settled(&server, started);
    assert!(server.pending.lock().expect("pending").is_empty());
    assert_eq!(mock.request_count(), 1);
}

struct FailingFlush {
    writes: Arc<AtomicI64>,
}

impl Write for FailingFlush {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "output closed",
        ))
    }
}

#[test]
fn output_loss_during_flush_cancels_work_and_never_retries_output() {
    let root = tempfile::tempdir().expect("tempdir");
    let writes = Arc::new(AtomicI64::new(0));
    let server = Arc::new(
        Server::new(
            config(&root, Endpoint::new("http://127.0.0.1:1", "key")),
            FailingFlush {
                writes: Arc::clone(&writes),
            },
        )
        .expect("server"),
    );
    let cancellation = Cancellation::new();
    let context = ExecutionContext::new(server.config.workspace.clone());
    {
        let mut active = server.active.lock().expect("active");
        active.cancellation = Some(cancellation.clone());
        active.tools = Some(context.cancellation());
    }
    server.respond(Id::Number(1), json!({}));
    assert!(cancellation.is_cancelled());
    assert!(context.check_cancelled().is_err());
    server.respond(Id::Number(2), json!({}));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert!(server.output_failed.load(Ordering::SeqCst));
}

#[test]
fn output_loss_before_tool_registration_prevents_provider_and_tool_work() {
    let root = tempfile::tempdir().expect("tempdir");
    let mock = MockEndpoint::start(vec![Script::text("must not run")]);
    let (output, client) = UnixStream::pair().expect("protocol output");
    let server = Arc::new(
        Server::new(config(&root, Endpoint::new(mock.base_url(), "key")), output).expect("server"),
    );
    let session = open(&server);
    {
        let mut active = server.active.lock().expect("active");
        active.cancellation = Some(Cancellation::new());
        active.session = Some(session.clone());
    }
    client
        .shutdown(std::net::Shutdown::Read)
        .expect("close output");
    let started = Instant::now();
    server.respond(Id::Number(1), json!({}));
    run_queued(
        &server,
        Queued {
            id: Id::Number(2),
            session,
            prompt: Prompt::parse(
                &json!({"prompt": [{"type": "text", "text": "work"}]}),
                Dialect::ChatCompletions.provider(),
            )
            .expect("prompt"),
        },
    );
    assert_settled(&server, started);
    assert_eq!(mock.request_count(), 0);
}

#[test]
fn output_loss_cancels_a_silent_stdio_mcp_tool_and_terminates_its_child() {
    let root = tempfile::tempdir().expect("tempdir");
    let log = root.path().join("mcp");
    let mock = MockEndpoint::start(vec![Script::tool_call(
        "call",
        "mcp_fixture_greet",
        r#"{"who":"wait"}"#,
    )]);
    let (output, client) = UnixStream::pair().expect("protocol output");
    let server = Arc::new(
        Server::new(config(&root, Endpoint::new(mock.base_url(), "key")), output).expect("server"),
    );
    let created = server.new_session(&json!({"mcpServers": [{
        "name": "fixture",
        "command": "/bin/sh",
        "args": [concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stdio_mcp.sh"), log, "--fixture"],
        "env": [{"name": "ACP_MCP_GREETING", "value": "hello"}],
    }]})).expect("MCP session");
    let session = created["sessionId"].as_str().expect("session id");
    prompt(&server, session, 1);
    let waiting = Instant::now();
    while !std::fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("\"method\":\"tools/call\"")
    {
        assert!(waiting.elapsed() < SHUTDOWN_GRACE, "MCP tool did not start");
        std::thread::sleep(POLL);
    }
    prompt(&server, session, 2);
    client
        .shutdown(std::net::Shutdown::Read)
        .expect("close output");
    let started = Instant::now();
    server.respond(Id::Number(3), json!({}));
    assert_settled(&server, started);
    let pid = std::fs::read_to_string(log.with_extension("pid")).expect("child pid");
    assert!(
        !std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("check child")
            .success(),
        "MCP child survived output loss"
    );
    assert_eq!(mock.request_count(), 1);
}
