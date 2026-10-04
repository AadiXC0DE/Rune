//! Regressions for the text-only one-shot path through the real binary.

#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Serves one completion from an isolated local endpoint.
fn run_fixture(delta: &Value, finish: &str, json_output: bool) -> Output {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("fixture address");
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": delta, "finish_reason": null}]}),
        json!({
            "choices": [{"delta": {}, "finish_reason": finish}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 0}
        })
    );
    let server = std::thread::spawn(move || {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(10))
            .expect("fixture deadline");
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no provider request arrived");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept fixture request: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("request timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .expect("response timeout");

        // Consume the whole request before closing the connection, so unread
        // request bytes cannot cause a reset to discard the fixture response.
        let mut reader = std::io::BufReader::new(&mut stream);
        let mut content_length = None;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).expect("request header") > 0);
            if line == "\r\n" {
                break;
            }
            if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = Some(length.trim().parse::<usize>().expect("content length"));
            }
        }
        let mut request = vec![0; content_length.expect("request content length")];
        reader.read_exact(&mut request).expect("request body");
        drop(reader);

        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("fixture response");
    });

    let dir = tempfile::tempdir().expect("isolated state");
    let mut command = Command::new(env!("CARGO_BIN_EXE_rune"));
    command
        .env_clear()
        .env("HOME", dir.path())
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("RUNE_PROVIDER", "chat_completions")
        .env("RUNE_BASE_URL", format!("http://{address}/v1"))
        .env("RUNE_MODEL", "fixture-model")
        .env("RUNE_API_KEY_ENV", "RUNE_ASK_TEST_KEY")
        .env("RUNE_ASK_TEST_KEY", "fixture-key")
        .current_dir(dir.path())
        .arg("ask");
    if json_output {
        command.arg("--json");
    }
    let output = command
        .arg("run the fixture command")
        .output()
        .expect("ask");
    server.join().expect("fixture server");
    output
}

fn tool_call_delta() -> Value {
    json!({
        "content": "I will run the command.",
        "tool_calls": [
            {
                "index": 0, "id": "call-shell", "type": "function",
                "function": {
                    "name": "shell",
                    "arguments": "{\"action\":\"run\",\"command\":\"printf SHOULD_NOT_RUN\"}"
                }
            },
            {
                "index": 1, "id": "call-read", "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"private.txt\"}"}
            }
        ]
    })
}

#[test]
fn ask_json_rejects_tool_calls_regardless_of_finish_reason() {
    for finish in ["tool_calls", "stop", "length"] {
        let output = run_fixture(&tool_call_delta(), finish, true);
        assert_eq!(output.status.code(), Some(1), "finish: {finish}");
        let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
        assert_eq!(result["exit_code"], 1);
        assert_eq!(result["error_code"], "unsupported_tool_call");
        let error = result["error"].as_str().expect("failure detail");
        assert!(error.contains("shell"), "{error}");
        assert!(error.contains("read_file"), "{error}");
        assert_eq!(
            result["tool_calls"],
            json!([
                {"name": "shell", "status": "error"},
                {"name": "read_file", "status": "error"}
            ])
        );
        assert_eq!(result["output"], "I will run the command.");
        assert_eq!(result["final_output"], "");
        assert_eq!(result["steps"], 1);
        assert_eq!(
            result["usage"],
            json!({"input_tokens": 11, "output_tokens": 0})
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("SHOULD_NOT_RUN"), "{stdout}");
        assert!(!stdout.contains("private.txt"), "{stdout}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("shell"), "{stderr}");
        assert!(stderr.contains("read_file"), "{stderr}");
    }
}

#[test]
fn ask_plain_text_rejects_tool_calls_without_printing_a_final_answer() {
    let output = run_fixture(&tool_call_delta(), "tool_calls", false);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("tool calls"), "{stderr}");
    assert!(stderr.contains("shell"), "{stderr}");
    assert!(stderr.contains("read_file"), "{stderr}");
    assert!(!stderr.contains("SHOULD_NOT_RUN"), "{stderr}");
    assert!(!stderr.contains("private.txt"), "{stderr}");
}

#[test]
fn ask_rejects_a_tool_calls_finish_without_completed_calls() {
    let output = run_fixture(
        &json!({"content": "An unfinished tool request."}),
        "tool_calls",
        true,
    );
    assert_eq!(output.status.code(), Some(1));
    let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
    assert_eq!(result["error_code"], "unsupported_tool_call");
    assert_eq!(result["tool_calls"], json!([]));
    assert_eq!(result["final_output"], "");
}

#[test]
fn ask_text_only_completion_still_succeeds() {
    for json_output in [false, true] {
        let output = run_fixture(&json!({"content": "Fixture answer."}), "stop", json_output);
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        if json_output {
            let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
            assert_eq!(result["output"], "Fixture answer.");
            assert_eq!(result["final_output"], "Fixture answer.");
            assert_eq!(result["tool_calls"], json!([]));
            assert!(result.get("error").is_none());
            assert!(result.get("error_code").is_none());
        } else {
            assert_eq!(output.stdout, b"Fixture answer.\n");
        }
    }
}
