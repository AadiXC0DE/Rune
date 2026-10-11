//! Manual compaction preserves exact originals for retrieval after restart.
#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use rune_agent::history::History;
use rune_core::paths::Paths;
use rune_net::message::ContentPart;
use rune_session::{EVENTS_FILE, SessionEvent, SessionStore};
use serde_json::{Value, json};

#[test]
fn compact_restart_and_retrieve_every_removed_message_and_complete_tool_batch() {
    let temp = tempfile::tempdir().expect("temp");
    let root = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("root")).expect("utf8");
    let paths = Paths {
        state_root: root.join("state"),
        config_root: root.join("config/rune"),
        data_root: root.join("data/rune"),
    };
    let id = "manualarch01".parse().expect("session ID");
    let store = SessionStore::create(&paths, &id).expect("session");
    store
        .append(SessionEvent::WorkspaceSet {
            workspace: root.to_string(),
        })
        .expect("workspace");
    let mut original = History::new();
    for turn in 1..=4 {
        let prompt = format!("question {turn}\nCafé");
        store
            .append(SessionEvent::UserMessage {
                text: prompt.clone(),
            })
            .expect("prompt");
        store
            .append(SessionEvent::TurnStarted { turn })
            .expect("started");
        original.push_user(prompt);
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for tool in 1..=2 {
            let call_id = format!("call_{turn}_{tool}");
            let arguments = format!("{{ \"path\": \"fixture{tool}.txt\" }}");
            store
                .append(SessionEvent::ToolCall {
                    call_id: call_id.clone(),
                    name: "read_file".into(),
                    arguments: arguments.clone(),
                })
                .expect("call");
            calls.push(ContentPart::ToolCall {
                id: rune_core::id::ToolCallId::new(call_id).expect("call ID"),
                name: "read_file".into(),
                arguments,
            });
        }
        for tool in 1..=2 {
            let call_id = format!("call_{turn}_{tool}");
            let output = format!(
                "original result {turn}/{tool}\n{}END",
                "contents\n".repeat(500)
            );
            store
                .append(SessionEvent::ToolResult {
                    call_id: call_id.clone(),
                    ok: tool == 1,
                    output: output.clone(),
                })
                .expect("result");
            results.push(ContentPart::ToolResult {
                id: rune_core::id::ToolCallId::new(call_id).expect("call ID"),
                name: String::new(),
                content: output,
                is_error: tool != 1,
            });
        }
        original.push_assistant(calls);
        original.push_tool_results(results);
        let answer = format!("answer {turn}\ncomplete");
        store
            .append(SessionEvent::AssistantMessage {
                turn,
                text: answer.clone(),
            })
            .expect("answer");
        store
            .append(SessionEvent::TurnFinished { turn })
            .expect("finished");
        original.push_assistant(vec![ContentPart::Text { text: answer }]);
    }
    drop(store);
    let log = paths.session_dir(&id).join(EVENTS_FILE);
    let before = std::fs::read(&log).expect("journal before");

    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut summaries = Vec::new();
        while summaries.len() < 2 {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "missing summary request");
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            let mut reader = std::io::BufReader::new(&mut stream);
            let mut first = String::new();
            reader.read_line(&mut first).expect("request");
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
            let (content_type, body) = if first.starts_with("GET ") {
                (
                    "application/json",
                    r#"{"data":[{"id":"fixture-model"}]}"#.to_owned(),
                )
            } else {
                let request: Value = serde_json::from_slice(&bytes).expect("JSON");
                assert_eq!(
                    request["messages"][0]["content"],
                    rune_agent::compaction::SUMMARY_INSTRUCTIONS
                );
                summaries.push(request);
                (
                    "text/event-stream",
                    format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({
                            "choices": [{"delta": {"content": "The earlier fixture established the task. Continue using the retained tool results."}, "finish_reason": "stop"}]
                        })
                    ),
                )
            };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("response");
        }
        summaries
    });
    let config = root.join("fixture.toml");
    std::fs::write(&config, format!(
        "provider = \"chat_completions\"\nbase_url = \"http://{address}/v1\"\n[models.chat_completions]\nid = \"fixture-model\"\n[limits]\nmax_tool_result_bytes = 32768\nprovider_request_timeout_ms = 5000\n"
    )).expect("config");
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rune"));
        command
            .env_clear()
            .env("HOME", &root)
            .env("RUNE_HOME", &paths.state_root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("RUNE_CONFIG", &config)
            .env("OPENAI_API_KEY", "fixture-key")
            .current_dir(&root);
        command
    };
    let mut child = command()
        .args(["--accessible", "--resume", id.as_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("interactive CLI");
    let mut input = child.stdin.take().expect("stdin");
    writeln!(input, "/compact\n/compact\n/quit").expect("commands");
    drop(input);
    let output = child.wait_with_output().expect("CLI output");
    let requests = server.join().expect("server");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    assert!(
        stdout.contains("compacted 12 earlier turn(s); 5 turn(s) remain"),
        "{stdout}"
    );
    assert!(
        stdout.contains("compacted 1 earlier turn(s); 5 turn(s) remain"),
        "{stdout}"
    );
    assert!(
        !requests[1].to_string().contains("question 1"),
        "original prefix was removed"
    );

    let run = |args: &[&str]| -> Output { command().args(args).output().expect("restarted CLI") };
    let all = run(&["session", "last", "--record", "all", "--json"]);
    assert!(all.status.success(), "{all:?}");
    assert!(all.stderr.is_empty(), "{all:?}");
    let all: Value = serde_json::from_slice(&all.stdout).expect("archive JSON");
    assert_eq!(all["session_id"], id.as_str());
    let records = all["records"].as_array().expect("records");
    assert_eq!(records.len(), 13);
    for (index, message) in original.turns()[..12].iter().enumerate() {
        let record_id = (index + 1).to_string();
        let record = run(&[
            "session",
            "--id",
            id.as_str(),
            "--record",
            &record_id,
            "--json",
        ]);
        assert!(record.status.success(), "{record:?}");
        let record: Value = serde_json::from_slice(&record.stdout).expect("record JSON");
        assert_eq!(
            record["records"],
            json!([{"id": index + 1, "message": message}])
        );
        assert_eq!(record["records"][0], records[index]);
    }
    assert!(
        records[12]["message"]["parts"][0]["text"]
            .as_str()
            .expect("summary")
            .contains("<context_handoff>")
    );
    for selector in ["0", "-1", "invalid", "18446744073709551616", "14"] {
        let output = run(&["session", id.as_str(), "--record", selector, "--json"]);
        assert!(!output.status.success(), "{selector}: {output:?}");
    }
    let conflict = run(&[
        "session",
        id.as_str(),
        "--record",
        "1",
        "--search",
        "question",
    ]);
    assert!(!conflict.status.success(), "{conflict:?}");
    assert_eq!(std::fs::read(log).expect("journal after"), before);
}
