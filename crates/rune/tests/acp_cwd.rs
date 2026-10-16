//! ACP sessions use the client workspace even when the process starts elsewhere.

#![allow(clippy::expect_used, clippy::panic)]

use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::LazyLock;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(20);

/// One origin for every diagnostic, so the job log reads as a single timeline.
///
/// The test drives a child process over pipes and a provider fixture over a
/// socket. A deadline that fires only names the file and line, which cannot say
/// which side stalled or how far the other side had got. Stamping the client,
/// the reader, the fixture, and the child's stderr from one clock makes the
/// next failure self-describing.
static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Prints one timestamped, flushed diagnostic line to stderr.
///
/// stderr, not stdout: the test binary's stdout is libtest's, and stderr is
/// where the child's notes already land. Flushing keeps the line when the run
/// is killed at a deadline rather than unwinding on its own.
fn note(message: fmt::Arguments<'_>) {
    eprintln!(
        "[acp_cwd +{:8.3}s] {message}",
        START.elapsed().as_secs_f64()
    );
    let _ = std::io::stderr().flush();
}

/// Names a phase or frame on the shared timeline.
macro_rules! note {
    ($($arg:tt)*) => {
        note(format_args!($($arg)*))
    };
}

/// A value rendered for the log, trimmed so a large body cannot flood it.
fn brief(value: &Value) -> String {
    brief_text(&value.to_string())
}

/// Text rendered for the log, trimmed to a single readable line.
fn brief_text(text: &str) -> String {
    const LIMIT: usize = 800;
    let count = text.chars().count();
    if count > LIMIT {
        let head: String = text.chars().take(LIMIT).collect();
        format!("{head}… ({count} chars)")
    } else {
        text.to_owned()
    }
}

/// Owns the process so a failed assertion also stops the server.
struct Client {
    child: Child,
    input: ChildStdin,
    frames: Receiver<Value>,
}

impl Client {
    fn call(&mut self, id: u64, method: &str, params: &Value) -> Value {
        let sent = Instant::now();
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        note!("call {id} `{method}` -> {}", brief(params));
        writeln!(self.input, "{request}").expect("write request");
        self.input.flush().expect("flush request");
        loop {
            let frame = match self.frames.recv_timeout(TIMEOUT) {
                Ok(frame) => frame,
                Err(error) => {
                    let status = self.child.try_wait();
                    let waited = sent.elapsed().as_secs_f64();
                    panic!(
                        "call {id} `{method}` got no matching frame within {TIMEOUT:?} \
                         ({waited:.3}s elapsed): {error:?}. Sent {request}; the frames this \
                         call did receive are listed above. Child status {status:?}"
                    );
                }
            };
            let elapsed = sent.elapsed().as_secs_f64();
            if frame["id"] == id {
                note!(
                    "call {id} <- response after {elapsed:.3}s: {}",
                    brief(&frame)
                );
                assert!(frame.get("error").is_none(), "{frame}");
                return frame["result"].clone();
            }
            note!(
                "call {id} <- notification after {elapsed:.3}s: {}",
                brief(&frame)
            );
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A local provider returns two tool calls, then records their results.
fn provider(listener: &TcpListener) -> Vec<Value> {
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    note!("fixture: listening, waiting for the first request");
    let mut requests = Vec::new();
    for (offset, delta) in [
        json!({"tool_calls": [{"index": 0, "id": "read", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"notes.txt\"}"}}]}),
        json!({"tool_calls": [{"index": 0, "id": "list", "type": "function", "function": {"name": "glob_files", "arguments": "{\"pattern\":\"*.txt\"}"}}]}),
        json!({"content": "done"}),
    ]
    .into_iter()
    .enumerate()
    {
        let index = offset.saturating_add(1);
        let started = Instant::now();
        let deadline = started.checked_add(TIMEOUT).expect("deadline");
        note!("fixture: request {index} of 3: waiting up to {TIMEOUT:?} for a connection");
        let (mut stream, peer) = loop {
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "fixture: no provider request {index} of 3 arrived within {TIMEOUT:?}; \
                         waited {waited:.1}s; served {served} request(s) before this one; the \
                         child never opened a connection for this request",
                        waited = started.elapsed().as_secs_f64(),
                        served = requests.len(),
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fixture: accept request {index}: {error}"),
            }
        };
        note!("fixture: request {index}: accepted a connection from {peer}");
        // `accept` inherits `O_NONBLOCK` from a nonblocking listener on macOS,
        // unlike Linux; the read timeout below is the real bound, so the
        // accepted socket must be blocking before it is read.
        stream
            .set_nonblocking(false)
            .expect("blocking request socket");
        stream
            .set_read_timeout(Some(TIMEOUT))
            .expect("read timeout");
        stream
            .set_write_timeout(Some(TIMEOUT))
            .expect("write timeout");
        let mut reader = BufReader::new(&mut stream);
        let mut length = None;
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).expect("header");
            assert!(
                read > 0,
                "fixture: request {index}: the child closed the connection while sending its \
                 headers"
            );
            note!(
                "fixture: request {index} header {}",
                brief_text(line.trim_end())
            );
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = Some(value.trim().parse::<usize>().expect("content length"));
            }
        }
        let mut body = vec![0; length.expect("content length header")];
        reader.read_exact(&mut body).expect("request body");
        let request: Value = serde_json::from_slice(&body).expect("request JSON");
        note!(
            "fixture: request {index} body ({} bytes): {}",
            body.len(),
            brief(&request)
        );
        requests.push(request);
        drop(reader);

        let finish = if delta.get("tool_calls").is_some() {
            "tool_calls"
        } else {
            "stop"
        };
        let body = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        );
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("provider response");
        note!(
            "fixture: request {index}: wrote {} bytes of SSE (finish_reason {finish})",
            body.len()
        );
    }
    note!("fixture: served all {} requests", requests.len());
    requests
}

#[test]
fn client_cwd_roots_reads_listing_and_project_instructions() {
    let root = tempfile::tempdir().expect("isolated state");
    let workspace_a = root.path().join("workspace-a");
    let workspace_b = root.path().join("workspace-b");
    note!(
        "workspaces: A {}, B {}",
        workspace_a.display(),
        workspace_b.display()
    );
    for (workspace, label) in [(&workspace_a, "A"), (&workspace_b, "B")] {
        std::fs::create_dir(workspace).expect("workspace");
        std::fs::write(
            workspace.join("AGENTS.md"),
            format!("instructions from workspace {label}"),
        )
        .expect("instructions");
        std::fs::write(
            workspace.join("notes.txt"),
            format!("contents from workspace {label}"),
        )
        .expect("notes");
        std::fs::write(workspace.join(format!("only-{label}.txt")), label).expect("unique file");
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("provider listener");
    let address = listener.local_addr().expect("provider address");
    note!("provider listener bound to {address}");
    let provider = std::thread::spawn(move || provider(&listener));
    let mut command = Command::new(env!("CARGO_BIN_EXE_rune"));
    command.env_clear();
    // Windows loads Winsock providers using SystemRoot. Clearing it creates
    // an invalid environment for the first network request, even though the
    // ACP handshake over stdio can succeed. Keep this OS dependency while
    // still isolating Rune settings, credentials, and proxy variables.
    #[cfg(windows)]
    {
        let system_root = std::env::var_os("SystemRoot").expect("Windows SystemRoot");
        note!("child SystemRoot: {}", system_root.to_string_lossy());
        command.env("SystemRoot", system_root);
    }
    let mut child = command
        .env("HOME", root.path())
        .env("RUNE_HOME", root.path().join("state"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("RUNE_PROVIDER", "chat_completions")
        .env("RUNE_BASE_URL", format!("http://{address}/v1"))
        .env("RUNE_MODEL", "fixture-model")
        .env("RUNE_API_KEY_ENV", "RUNE_ACP_TEST_KEY")
        .env("RUNE_ACP_TEST_KEY", "fixture-key")
        .current_dir(&workspace_a)
        .arg("acp")
        // This fixture expects exactly three successful requests. Surface a
        // connection failure directly instead of spending the test deadline
        // in the product's default ten-attempt backoff.
        .args(["--limit", "provider_max_attempts=1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ACP process");
    note!("spawned the ACP child, pid {}", child.id());
    let input = child.stdin.take().expect("stdin");
    let output = child.stdout.take().expect("stdout");
    let errors = child.stderr.take().expect("stderr");
    // The child's notes reach the log on their own stream, stamped like the
    // rest, so a server-side refusal is attributed rather than interleaved.
    std::thread::spawn(move || {
        for line in BufReader::new(errors).lines() {
            match line {
                Ok(line) => note!("child stderr <- {}", brief_text(&line)),
                Err(error) => {
                    note!("child stderr: {error}");
                    break;
                }
            }
        }
        note!("child stderr closed (EOF)");
    });
    let (sender, frames) = channel();
    let reader = std::thread::spawn(move || {
        note!("reader: watching the child's stdout");
        let mut lines = BufReader::new(output).lines();
        loop {
            match lines.next() {
                Some(Ok(line)) => {
                    note!("reader <- {}", brief_text(&line));
                    let frame: Value = serde_json::from_str(&line).unwrap_or_else(|error| {
                        panic!("reader: protocol JSON from `{line}`: {error}")
                    });
                    if sender.send(frame).is_err() {
                        break;
                    }
                }
                Some(Err(error)) => panic!("reader: protocol line: {error}"),
                None => {
                    note!("reader: child closed stdout (EOF)");
                    break;
                }
            }
        }
    });
    let mut client = Client {
        child,
        input,
        frames,
    };
    note!("stage: initialize");
    client.call(1, "initialize", &json!({"protocolVersion": 1}));
    note!("stage: session/new, cwd is workspace B while the process started in workspace A");
    let created = client.call(
        2,
        "session/new",
        &json!({"cwd": workspace_b, "mcpServers": []}),
    );
    let session = created["sessionId"].as_str().expect("session identifier");
    note!("session: {session}");
    note!("stage: session/set_mode full_access");
    client.call(
        3,
        "session/set_mode",
        &json!({"sessionId": session, "modeId": "full_access"}),
    );
    note!("stage: session/list");
    let listed = client.call(4, "session/list", &json!({}));
    assert_eq!(listed["sessions"], json!([]));
    note!("stage: session/prompt (the step that makes provider requests)");
    let response = client.call(5, "session/prompt", &json!({"sessionId": session, "prompt": [{"type": "text", "text": "inspect the workspace"}]}));
    assert_eq!(response["stopReason"], "end_turn");

    note!("stage: prompt returned; joining the provider fixture");
    let requests = provider.join().expect("provider fixture");
    note!("fixture served {} request(s)", requests.len());
    assert_eq!(requests.len(), 3);
    for request in &requests {
        let instructions = request["messages"][0]["content"]
            .as_str()
            .expect("system instructions");
        assert!(
            instructions.contains("instructions from workspace B"),
            "{instructions}"
        );
        assert!(
            instructions.contains(workspace_b.to_str().expect("UTF-8 cwd")),
            "{instructions}"
        );
        assert!(
            !instructions.contains("instructions from workspace A"),
            "{instructions}"
        );
        assert!(
            !instructions.contains(workspace_a.to_str().expect("UTF-8 cwd")),
            "{instructions}"
        );
    }
    let messages = requests.last().expect("final request")["messages"]
        .as_array()
        .expect("messages");
    let result = |id| {
        messages
            .iter()
            .find(|message| message["tool_call_id"] == id)
            .expect("tool result")["content"]
            .as_str()
            .expect("tool content")
    };
    assert!(result("read").contains("contents from workspace B"));
    assert!(!result("read").contains("contents from workspace A"));
    assert!(result("list").contains("only-B.txt"));
    assert!(!result("list").contains("only-A.txt"));
    note!("stage: all assertions passed");
    drop(client);
    reader.join().expect("protocol reader");
}
