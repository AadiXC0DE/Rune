//! ACP sessions use the client workspace even when the process starts elsewhere.

#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(20);

/// Owns the process so a failed assertion also stops the server.
struct Client {
    child: Child,
    input: ChildStdin,
    frames: Receiver<Value>,
}

impl Client {
    fn call(&mut self, id: u64, method: &str, params: &Value) -> Value {
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .expect("write request");
        self.input.flush().expect("flush request");
        loop {
            let frame = self
                .frames
                .recv_timeout(TIMEOUT)
                .expect("protocol response");
            if frame["id"] == id {
                assert!(frame.get("error").is_none(), "{frame}");
                return frame["result"].clone();
            }
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
    let mut requests = Vec::new();
    for delta in [
        json!({"tool_calls": [{"index": 0, "id": "read", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"notes.txt\"}"}}]}),
        json!({"tool_calls": [{"index": 0, "id": "list", "type": "function", "function": {"name": "glob_files", "arguments": "{\"pattern\":\"*.txt\"}"}}]}),
        json!({"content": "done"}),
    ] {
        let deadline = Instant::now().checked_add(TIMEOUT).expect("deadline");
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no provider request arrived");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept request: {error}"),
            }
        };
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
            assert!(reader.read_line(&mut line).expect("header") > 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = Some(value.trim().parse::<usize>().expect("content length"));
            }
        }
        let mut body = vec![0; length.expect("content length header")];
        reader.read_exact(&mut body).expect("request body");
        requests.push(serde_json::from_slice(&body).expect("request JSON"));
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
    }
    requests
}

#[test]
fn client_cwd_roots_reads_listing_and_project_instructions() {
    let root = tempfile::tempdir().expect("isolated state");
    let workspace_a = root.path().join("workspace-a");
    let workspace_b = root.path().join("workspace-b");
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
    let provider = std::thread::spawn(move || provider(&listener));
    let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
        .env_clear()
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
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("ACP process");
    let input = child.stdin.take().expect("stdin");
    let output = child.stdout.take().expect("stdout");
    let (sender, frames) = channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let frame = serde_json::from_str(&line.expect("protocol line")).expect("protocol JSON");
            if sender.send(frame).is_err() {
                break;
            }
        }
    });
    let mut client = Client {
        child,
        input,
        frames,
    };
    client.call(1, "initialize", &json!({"protocolVersion": 1}));
    let created = client.call(
        2,
        "session/new",
        &json!({"cwd": workspace_b, "mcpServers": []}),
    );
    let session = created["sessionId"].as_str().expect("session identifier");
    client.call(
        3,
        "session/set_mode",
        &json!({"sessionId": session, "modeId": "full_access"}),
    );
    let listed = client.call(4, "session/list", &json!({}));
    assert_eq!(listed["sessions"][0]["sessionId"], session);
    assert_eq!(
        listed["sessions"][0]["cwd"],
        workspace_b.to_str().expect("UTF-8 cwd")
    );
    let response = client.call(5, "session/prompt", &json!({"sessionId": session, "prompt": [{"type": "text", "text": "inspect the workspace"}]}));
    assert_eq!(response["stopReason"], "end_turn");

    let requests = provider.join().expect("provider fixture");
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
    drop(client);
    reader.join().expect("protocol reader");
}
