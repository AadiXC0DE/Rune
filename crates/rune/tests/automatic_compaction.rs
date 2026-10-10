//! Automatic context compaction through the real CLI, including tool steps.

#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[test]
fn fixture_turns_compact_before_the_next_cli_model_request() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        let mut normal_count = 0;
        let deadline = Instant::now() + Duration::from_secs(30);
        while normal_count < 7 {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "missing CLI request: {requests:?}"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .expect("write timeout");
            let mut reader = std::io::BufReader::new(&mut stream);
            let mut first = String::new();
            reader.read_line(&mut first).expect("request line");
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).expect("header") > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().expect("length");
                }
            }
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).expect("body");
            drop(reader);
            if first.starts_with("GET ") {
                let body = r#"{"data":[{"id":"fixture-model","context_window":40000}]}"#;
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("catalog response");
                continue;
            }
            let request: Value = serde_json::from_slice(&bytes).expect("request JSON");
            let summary = request["messages"][0]["content"].as_str()
                == Some(rune_agent::compaction::SUMMARY_INSTRUCTIONS);
            requests.push((summary, request));
            let (delta, finish) = if summary {
                (
                    json!({"content": "The fixture exchanges established the task. Continue using the recent conversation and tool results."}),
                    "stop",
                )
            } else {
                normal_count += 1;
                if normal_count == 3 {
                    (
                        json!({"tool_calls": [{"index": 0, "id": "fixture_call", "type": "function", "function": {
                            "name": "read_file", "arguments": "{\"path\":\"fixture.txt\"}"
                        }}]}),
                        "tool_calls",
                    )
                } else {
                    (
                        json!({"content": format!("fixture answer {normal_count}: {}", "x".repeat(20_000))}),
                        "stop",
                    )
                }
            };
            let body = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices": [{"delta": delta, "finish_reason": finish}]})
            );
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("completion response");
        }
        requests
    });
    let dir = tempfile::tempdir().expect("isolated workspace");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"
provider = "chat_completions"
base_url = "http://{address}/v1"
api_key_env = "RUNE_COMPACTION_TEST_KEY"
[models.chat_completions]
id = "fixture-model"
context_window = 40000
[limits]
compaction_trigger_percent = 50
max_tool_result_bytes = 32768
provider_request_timeout_ms = 5000
"#
        ),
    )
    .expect("config");
    std::fs::write(
        dir.path().join("fixture.txt"),
        ("tool fixture ".repeat(12) + "\n").repeat(180),
    )
    .expect("tool fixture");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
        .env_clear()
        .env("HOME", dir.path())
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("RUNE_CONFIG", &config)
        .env("RUNE_COMPACTION_TEST_KEY", "fixture-key")
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI");
    let mut input = child.stdin.take().expect("stdin");
    for turn in 1..=6 {
        writeln!(input, "fixture question {turn}").expect("prompt");
    }
    writeln!(input, "/quit").expect("quit");
    drop(input);
    let output = child.wait_with_output().expect("CLI output");
    let requests = server.join().expect("fixture server");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("compacted "),
        "no CLI compaction event; requests: {:?}; stdout tail: {}",
        requests
            .iter()
            .map(|(summary, body)| (*summary, body.to_string().len()))
            .collect::<Vec<_>>(),
        &stdout[stdout.len().saturating_sub(500)..]
    );
    let first_summary = requests
        .iter()
        .position(|(summary, _)| *summary)
        .expect("automatic summary request");
    let tool_step = requests
        .iter()
        .enumerate()
        .filter(|(_, (summary, _))| !summary)
        .nth(2)
        .expect("tool step")
        .0;
    assert!(
        requests[tool_step + 1].0,
        "the tool result must trigger compaction before its continuation request: {:?}; tool: {}",
        requests
            .iter()
            .map(|(summary, body)| (*summary, body.to_string().len()))
            .collect::<Vec<_>>(),
        requests[tool_step + 1].1["messages"]
            .as_array()
            .expect("messages")
            .last()
            .expect("last")["role"]
    );
    assert!(
        first_summary > 0,
        "a short initial request must not compact"
    );
    let mut compactions = 0;
    for pair in requests.windows(2) {
        if pair[0].0 {
            compactions += 1;
            assert!(!pair[1].0, "the next request should resume the task");
            assert!(pair[1].1.to_string().contains("<context_handoff>"));
            let bytes = serde_json::to_vec(&pair[1].1).expect("body").len() as u64;
            assert!(
                rune_agent::tokens::estimate_tokens(bytes) < 20_000,
                "compacted request still crosses the configured 50 percent trigger"
            );
        }
    }
    assert_eq!(stdout.matches("compacted ").count(), compactions);
    assert_eq!(requests.iter().filter(|(summary, _)| !summary).count(), 7);
    assert!(
        requests.iter().any(|(_, request)| request["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .any(|message| message["role"] == "tool")),
        "the tool continuation reached the provider"
    );
}
