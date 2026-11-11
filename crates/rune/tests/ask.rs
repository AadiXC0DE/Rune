//! Regressions for the bounded one-shot tool loop through the real binary.

#![allow(clippy::expect_used, clippy::panic)]

mod support;
use support::CommandExt as _;

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Serves one completion from an isolated local endpoint.
fn run_fixture(delta: &Value, finish: &str, json_output: bool) -> Output {
    run_fixture_with_options(delta, finish, json_output, false).0
}

/// Serves one completion to a run whose step cap is one.
fn run_capped_fixture(delta: &Value, finish: &str, json_output: bool) -> Output {
    run_fixture_with_cap(delta, finish, json_output, false, Some(1)).0
}

fn isolated_command(dir: &tempfile::TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rune"));
    command
        .isolated_env()
        .env("HOME", dir.path())
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .current_dir(dir.path());
    command
}

#[test]
fn ask_without_a_provider_reports_one_error_and_one_hint() {
    for args in [
        vec!["ask", "hello"],
        vec!["ask", "--json", "hello"],
        vec!["--json", "ask", "hello"],
    ] {
        let dir = tempfile::tempdir().expect("isolated state");
        let output = isolated_command(&dir).args(&args).output().expect("ask");

        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            stderr,
            "rune: authentication_required: no model provider is connected\n\
             hint: run `rune connect` to choose an endpoint and store a credential\n",
            "{args:?}"
        );
        if args.contains(&"--json") {
            let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
            assert_eq!(result["exit_code"], 1);
            assert_eq!(result["error_code"], "authentication_required");
            assert_eq!(result["error"], "no model provider is connected");
            assert_eq!(result["output"], "");
            assert_eq!(result["final_output"], "");
        } else {
            assert!(output.stdout.is_empty());
        }
        assert!(!dir.path().join("state").exists());
    }
}

#[test]
fn ask_json_reports_an_empty_prompt_as_one_failure_object() {
    for args in [
        vec!["ask", "--json"],
        vec!["--json", "ask"],
        vec!["ask", "--json", " \t "],
    ] {
        for input in ["", " \t\r\n"] {
            let dir = tempfile::tempdir().expect("isolated state");
            let mut child = isolated_command(&dir)
                .env("RUNE_MODEL", "fixture-model")
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("ask");
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(input.as_bytes())
                .expect("write prompt");
            let output = child.wait_with_output().expect("ask output");

            assert_eq!(output.status.code(), Some(1), "{args:?}, {input:?}");
            let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
            assert_eq!(result["exit_code"], 1);
            assert_eq!(result["error_code"], "missing_field");
            assert!(
                result["error"]
                    .as_str()
                    .expect("failure detail")
                    .contains("prompt")
            );
            assert_eq!(result["model"], "fixture-model");
            assert_eq!(result["output"], "");
            assert_eq!(result["final_output"], "");
            assert_eq!(result["steps"], 0);
            assert_eq!(result["usage"], json!({}));
            assert_eq!(result["tool_calls"], json!([]));
            assert_eq!(result["session_id"], "");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("missing_field"), "{stderr}");
            assert!(
                stderr.contains("pass the prompt as an argument"),
                "{stderr}"
            );
            assert!(!dir.path().join("state").exists());
        }
    }
}

#[test]
fn ask_plain_text_empty_prompt_keeps_stdout_empty() {
    let dir = tempfile::tempdir().expect("isolated state");
    let output = isolated_command(&dir)
        .arg("ask")
        .stdin(Stdio::null())
        .output()
        .expect("ask");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing_field"), "{stderr}");
    assert!(
        stderr.contains("pass the prompt as an argument"),
        "{stderr}"
    );
}

fn run_fixture_with_options(
    delta: &Value,
    finish: &str,
    json_output: bool,
    no_save: bool,
) -> (Output, tempfile::TempDir) {
    run_fixture_with_cap(delta, finish, json_output, no_save, None)
}

fn run_fixture_with_cap(
    delta: &Value,
    finish: &str,
    json_output: bool,
    no_save: bool,
    cap: Option<u64>,
) -> (Output, tempfile::TempDir) {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": delta, "finish_reason": null}]}),
        json!({
            "choices": [{"delta": {}, "finish_reason": finish}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 0}
        })
    );
    run_response_fixture(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        json_output,
        no_save,
        cap,
    )
}

fn run_response_fixture(
    response: String,
    json_output: bool,
    no_save: bool,
    cap: Option<u64>,
) -> (Output, tempfile::TempDir) {
    let (output, dir, _) = run_responses_fixture(vec![response], json_output, no_save, cap);
    (output, dir)
}

fn run_responses_fixture(
    responses: Vec<String>,
    json_output: bool,
    no_save: bool,
    max_steps: Option<u64>,
) -> (Output, tempfile::TempDir, Vec<Value>) {
    run_responses_fixture_with_setup(responses, json_output, no_save, max_steps, |_| {})
}

fn run_responses_fixture_with_setup(
    responses: Vec<String>,
    json_output: bool,
    no_save: bool,
    max_steps: Option<u64>,
    setup: impl FnOnce(&tempfile::TempDir),
) -> (Output, tempfile::TempDir, Vec<Value>) {
    run_responses_fixture_with_format(
        responses,
        json_output.then_some("--json"),
        no_save,
        max_steps,
        setup,
    )
}

fn run_responses_fixture_with_format(
    responses: Vec<String>,
    output_flag: Option<&str>,
    no_save: bool,
    max_steps: Option<u64>,
    setup: impl FnOnce(&tempfile::TempDir),
) -> (Output, tempfile::TempDir, Vec<Value>) {
    run_responses_fixture_with_images(responses, output_flag, no_save, max_steps, &[], setup)
}

fn run_responses_fixture_with_images(
    responses: Vec<String>,
    output_flag: Option<&str>,
    no_save: bool,
    max_steps: Option<u64>,
    images: &[&str],
    setup: impl FnOnce(&tempfile::TempDir),
) -> (Output, tempfile::TempDir, Vec<Value>) {
    run_responses_fixture_command(
        responses,
        output_flag,
        no_save,
        max_steps,
        (&["ask"], images),
        |dir, _| setup(dir),
    )
}

fn run_responses_fixture_command(
    responses: Vec<String>,
    output_flag: Option<&str>,
    no_save: bool,
    max_steps: Option<u64>,
    (arguments, images): (&[&str], &[&str]),
    setup: impl FnOnce(&tempfile::TempDir, &mut Command),
) -> (Output, tempfile::TempDir, Vec<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("fixture address");
    let (finished, completion) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(10))
            .expect("fixture deadline");
        let mut requests = Vec::new();
        loop {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if completion.try_recv().is_ok() {
                            return requests;
                        }
                        assert!(Instant::now() < deadline, "no provider request arrived");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept fixture request: {error}"),
                }
            };
            // `accept` inherits `O_NONBLOCK` from a nonblocking listener on
            // macOS, unlike Linux, so the socket must be put back to blocking
            // before it is read: otherwise the read below can return
            // `WouldBlock` (Darwin errno 35) instead of waiting for the request.
            // The read timeout set next is the real bound.
            stream
                .set_nonblocking(false)
                .expect("blocking fixture request");
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
            // Keep serving until the child exits so an unexpected retry is
            // counted, rather than hidden by a closed fixture endpoint.
            let response = responses
                .get(requests.len())
                .or_else(|| responses.last())
                .expect("fixture response");
            requests.push(serde_json::from_slice(&request).expect("request JSON"));

            stream
                .write_all(response.as_bytes())
                .expect("fixture response");
        }
    });

    let dir = tempfile::tempdir().expect("isolated state");
    std::fs::write(dir.path().join("fixture.txt"), "ALLOWED_READ_RESULT").expect("read fixture");
    if let Some(cap) = max_steps {
        rune_core::paths::create_dir_private(
            camino::Utf8Path::from_path(&dir.path().join("config/rune"))
                .expect("UTF-8 config path"),
        )
        .expect("configuration directory");
        std::fs::write(
            dir.path().join("config/rune/config.toml"),
            format!("[limits]\nmax_agent_steps = {cap}\n"),
        )
        .expect("step cap");
    }
    let mut command = isolated_command(&dir);
    setup(&dir, &mut command);
    command
        .env("RUNE_PROVIDER", "chat_completions")
        .env("RUNE_BASE_URL", format!("http://{address}/v1"))
        .env("RUNE_MODEL", "fixture-model")
        .env("RUNE_API_KEY_ENV", "RUNE_ASK_TEST_KEY")
        .env("RUNE_ASK_TEST_KEY", "fixture-key")
        .args(arguments);
    if let Some(flag) = output_flag {
        command.arg(flag);
    }
    if no_save {
        command.arg("--no-save");
    }
    for image in images {
        command.arg("--image").arg(image);
    }
    let output = command
        .arg("run the fixture command")
        .output()
        .expect("ask");
    finished.send(()).expect("stop fixture server");
    let requests = server.join().expect("fixture server");
    (output, dir, requests)
}

fn offered_tools(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn ask_adds_the_workspace_snapshot_only_when_switched_on() {
    for (switched_on, expected) in [(true, true), (false, false)] {
        let (output, _, requests) = run_responses_fixture_command(
            vec![completion_response(&json!({"content": "ok"}), "stop")],
            Some("--json"),
            true,
            None,
            (&["ask"], &[]),
            |_, command| {
                if switched_on {
                    command.env("RUNE_EXPERIMENT", "snapshot=on");
                }
            },
        );
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let body = requests[0].to_string();
        assert_eq!(body.contains("<workspace_snapshot>"), expected, "{body}");
        if expected {
            assert!(body.contains("top level: "), "{body}");
        }
    }
}

#[test]
fn ask_sends_tool_guidance_under_the_v2_experiment() {
    for switched_on in [true, false] {
        let (output, _, requests) = run_responses_fixture_command(
            vec![completion_response(&json!({"content": "ok"}), "stop")],
            Some("--json"),
            true,
            None,
            (&["ask"], &[]),
            |_, command| {
                if switched_on {
                    command.env("RUNE_EXPERIMENT", "prompt=v2-min");
                }
            },
        );
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert_eq!(requests[0]["messages"][0]["role"], "system");
        let system = requests[0]["messages"][0]["content"]
            .as_str()
            .expect("system text");
        assert_eq!(system.contains("Using the tools:"), switched_on, "{system}");
        assert_eq!(
            system.contains("edit_file needs old_string"),
            switched_on,
            "{system}"
        );
    }
}

#[test]
fn ask_sends_the_open_weight_overlay_when_switched_on() {
    for switched_on in [true, false] {
        let (output, _, requests) = run_responses_fixture_command(
            vec![completion_response(&json!({"content": "ok"}), "stop")],
            Some("--json"),
            true,
            None,
            (&["ask"], &[]),
            |_, command| {
                command.env("RUNE_MODEL", "deepseek-chat");
                if switched_on {
                    command.env("RUNE_EXPERIMENT", "overlay=on");
                }
            },
        );
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        let system = requests[0]["messages"][0]["content"]
            .as_str()
            .expect("system text");
        assert_eq!(
            system.contains("Never write a tool call as text"),
            switched_on,
            "{system}"
        );
        assert_eq!(
            system.contains("Notes for this model:"),
            switched_on,
            "{system}"
        );
    }
}

#[test]
fn ask_offers_the_subagent_tool() {
    let (output, _, requests) = run_responses_fixture_command(
        vec![completion_response(&json!({"content": "ok"}), "stop")],
        Some("--json"),
        true,
        None,
        (&["ask"], &[]),
        |_, _| {},
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(offered_tools(&requests[0]).contains(&"subagent".to_owned()));
}

#[test]
fn ask_review_does_not_offer_subagents() {
    let (output, _, requests) = run_responses_fixture_command(
        vec![completion_response(
            &json!({"content": "Review complete."}),
            "stop",
        )],
        Some("--json"),
        false,
        None,
        (&["review"], &[]),
        |dir, command| {
            command.with_external_programs();
            let git = Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .output()
                .expect("initialize review repository");
            assert!(git.status.success(), "review fixture setup failed: {git:?}");
        },
    );
    assert!(output.status.success(), "{output:?}");
    let tools = offered_tools(&requests[0]);
    assert!(tools.contains(&"read_file".to_owned()), "{tools:?}");
    assert!(!tools.contains(&"subagent".to_owned()), "{tools:?}");
}

#[test]
fn ask_json_reports_the_clamped_effort() {
    // Claude Sonnet 4.6 accepts every named level but xhigh.
    let (output, _, requests) = run_responses_fixture_command(
        vec![completion_response(&json!({"content": "ok"}), "stop")],
        Some("--json"),
        true,
        None,
        (
            &["ask", "--model", "claude-sonnet-4-6", "--effort", "xhigh"],
            &[],
        ),
        |_, _| {},
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
    assert_eq!(result["effort"], "max");
    assert_eq!(requests[0]["reasoning_effort"], "max");
}

#[test]
fn ask_json_preserves_provider_code_and_retry_delay_with_secrets_redacted() {
    for code in ["rate_limit_exceeded", "sk-fixture-secret-code"] {
        let secret = "sk-fixture-secret-message";
        let body =
            json!({"error": {"code": code, "message": format!("rejected {secret}")}}).to_string();
        let (output, _, requests) = run_responses_fixture_with_setup(
            vec![format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 3\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )],
            true,
            true,
            Some(1),
            |dir| write_retry_limits(dir, 1),
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(output.status.code(), Some(1));
        let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
        assert_eq!(result["error_code"], "rate_limited");
        let detail = &result["error_detail"];
        assert_eq!(
            detail["provider_code"],
            if code.starts_with("sk-") {
                "[redacted]"
            } else {
                code
            }
        );
        assert_eq!(detail["retry_after_ms"], 3_000);
        assert_eq!(detail["observed"], "http 429");
        assert!(
            detail["hint"]
                .as_str()
                .expect("hint")
                .contains("rejected the request")
        );
        for bytes in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(bytes);
            assert!(!text.contains(secret), "{text}");
            assert!(!text.contains("sk-fixture-secret-code"), "{text}");
            assert!(text.contains("[redacted]"), "{text}");
        }
    }
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
fn ask_json_reports_tool_errors_and_stops_at_the_configured_cap() {
    for finish in ["tool_calls", "stop", "length"] {
        let output = run_capped_fixture(&tool_call_delta(), finish, true);
        assert_eq!(output.status.code(), Some(3), "finish: {finish}");
        let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
        assert_eq!(result["exit_code"], 3);
        assert_eq!(result["error_code"], "limit_exceeded");
        let error = result["error"].as_str().expect("failure detail");
        assert!(error.contains("limit of 1 model steps"), "{error}");
        assert_eq!(
            result["tool_calls"],
            json!([
                {"name": "shell", "status": "error"},
                {"name": "read_file", "status": "error"}
            ])
        );
        assert_eq!(result["output"], "I will run the command.");
        assert_eq!(result["final_output"], "I will run the command.");
        assert_eq!(result["steps"], 1);
        assert_eq!(result["narration"][0]["text"], "I will run the command.");
        assert_eq!(result["narration"][1]["tool"], "shell");
        assert_eq!(result["narration"][2]["tool"], "read_file");
        assert_eq!(
            result["usage"],
            json!({"input_tokens": 11, "output_tokens": 0})
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("SHOULD_NOT_RUN"), "{stdout}");
        assert!(!stdout.contains("private.txt"), "{stdout}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("limit of 1 model steps"), "{stderr}");
    }
}

#[test]
fn ask_plain_text_step_limit_prints_the_final_text() {
    let output = run_capped_fixture(&tool_call_delta(), "tool_calls", false);
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(output.stdout, b"I will run the command.\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("limit of 1 model steps"), "{stderr}");
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
    assert_eq!(result["output"], "An unfinished tool request.");
    assert_eq!(result["steps"], 1);
    assert_eq!(
        result["usage"],
        json!({"input_tokens": 11, "output_tokens": 0})
    );
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

#[test]
fn ask_saves_a_resolvable_exchange_in_its_workspace() {
    for json_output in [false, true] {
        let (output, dir) = run_fixture_with_options(
            &json!({"content": "Fixture answer."}),
            "stop",
            json_output,
            false,
        );
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());

        let listing = isolated_command(&dir)
            .args(["sessions", "--json"])
            .output()
            .expect("list saved sessions");
        assert!(listing.status.success());
        let listing: Value = serde_json::from_slice(&listing.stdout).expect("session listing");
        let sessions = listing["sessions"].as_array().expect("sessions");
        assert_eq!(sessions.len(), 1);
        let session_id = sessions[0]["id"].as_str().expect("saved session id");
        assert!(!session_id.is_empty());
        if json_output {
            let result: Value = serde_json::from_slice(&output.stdout).expect("ask JSON");
            assert_eq!(result["session_id"], session_id);
        } else {
            assert_eq!(output.stdout, b"Fixture answer.\n");
        }

        let inspection = isolated_command(&dir)
            .args(["session", "--json", "--", session_id])
            .output()
            .expect("inspect saved session");
        assert!(
            inspection.status.success(),
            "{}",
            String::from_utf8_lossy(&inspection.stderr)
        );
        let inspection: Value =
            serde_json::from_slice(&inspection.stdout).expect("session inspection");
        assert_eq!(inspection["id"], session_id);
        assert_eq!(inspection["title"], "run the fixture command");
        assert_eq!(inspection["turns"], 1);
        assert_eq!(
            inspection["usage"],
            json!({"input_tokens": 11, "output_tokens": 0})
        );
        assert_eq!(inspection["truncated"], false);

        let session_dir = dir.path().join("state/sessions").join(session_id);
        let session_dir = camino::Utf8Path::from_path(&session_dir).expect("UTF-8 session path");
        let state = rune_session::load_read_only(session_dir).expect("read saved exchange");
        let workspace = dir.path().canonicalize().expect("canonical workspace");
        assert_eq!(state.workspace.as_deref(), workspace.to_str());
        let exchange: Vec<_> = state
            .events
            .iter()
            .filter_map(|frame| match &frame.event {
                rune_session::SessionEvent::UserMessage { text } => Some(("user", text.as_str())),
                rune_session::SessionEvent::AssistantMessage { turn, text } => {
                    assert_eq!(*turn, 1);
                    Some(("assistant", text.as_str()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            exchange,
            [
                ("user", "run the fixture command"),
                ("assistant", "Fixture answer.")
            ]
        );
    }
}

#[test]
fn ask_no_save_creates_no_session() {
    for json_output in [false, true] {
        let (output, dir) = run_fixture_with_options(
            &json!({"content": "Unsaved answer."}),
            "stop",
            json_output,
            true,
        );
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        assert!(!dir.path().join("state/sessions").exists());
        if json_output {
            let result: Value = serde_json::from_slice(&output.stdout).expect("ask JSON");
            assert_eq!(result["session_id"], "");
            assert_eq!(result["final_output"], "Unsaved answer.");
        } else {
            assert_eq!(output.stdout, b"Unsaved answer.\n");
        }
        let listing = isolated_command(&dir)
            .args(["sessions", "--json"])
            .output()
            .expect("list unsaved sessions");
        assert!(listing.status.success());
        let listing: Value = serde_json::from_slice(&listing.stdout).expect("session listing");
        assert_eq!(listing["sessions"], json!([]));
    }
}

#[test]
fn ask_saves_a_tool_exchange_stopped_at_the_step_cap() {
    let (output, dir) =
        run_fixture_with_cap(&tool_call_delta(), "tool_calls", true, false, Some(1));
    assert_eq!(output.status.code(), Some(3));
    let result: Value = serde_json::from_slice(&output.stdout).expect("ask JSON");
    let session_id = result["session_id"].as_str().expect("saved session id");
    assert!(!session_id.is_empty());
    let session_dir = dir.path().join("state/sessions").join(session_id);
    let session_dir = camino::Utf8Path::from_path(&session_dir).expect("UTF-8 session path");
    let state = rune_session::load_read_only(session_dir).expect("read rejected exchange");
    assert_eq!(state.turns, 1);
    assert_eq!(state.usage.input_tokens, 11);
    assert_eq!(state.usage.output_tokens, 0);
    let messages: Vec<_> = state
        .events
        .iter()
        .filter_map(|frame| match &frame.event {
            rune_session::SessionEvent::UserMessage { text }
            | rune_session::SessionEvent::AssistantMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        messages,
        ["run the fixture command", "I will run the command."]
    );
    assert!(matches!(
        state.events.last().map(|frame| &frame.event),
        Some(rune_session::SessionEvent::TurnFailed { turn: 1, code, message })
            if *code == rune_core::error::ErrorCode::LimitExceeded
                && Some(message.as_str()) == result["error"].as_str()
    ));
}

fn completion_response(delta: &Value, finish: &str) -> String {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": delta, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": finish}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 2}})
    );
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn write_retry_limits(dir: &tempfile::TempDir, attempts: u64) {
    std::fs::write(
        dir.path().join("config/rune/config.toml"),
        format!(
            "[limits]\nmax_agent_steps = 2\nprovider_max_attempts = {attempts}\n\
             provider_head_timeout_ms = 5000\nprovider_request_timeout_ms = 5000\n"
        ),
    )
    .expect("retry limits");
}

fn error_response(status: &str, retry_after: u64) -> String {
    let body = json!({"error": {"message": "Fixture rejection."}}).to_string();
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
         Retry-After: {retry_after}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn ask_retries_a_rate_limit_after_the_requested_delay_within_the_configured_deadline() {
    let started = Instant::now();
    let (output, _, requests) = run_responses_fixture_with_setup(
        vec![
            error_response("429 Too Many Requests", 1),
            completion_response(&json!({"content": "Recovered answer."}), "stop"),
        ],
        true,
        true,
        Some(2),
        |dir| write_retry_limits(dir, 2),
    );
    let elapsed = started.elapsed();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert!(elapsed >= Duration::from_secs(1), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    let result: Value = serde_json::from_slice(&output.stdout).expect("ask JSON");
    assert_eq!(result["final_output"], "Recovered answer.");
    assert_eq!(result["steps"], 1);
    assert_eq!(
        result["usage"],
        json!({"input_tokens": 11, "output_tokens": 2})
    );
}

#[test]
fn ask_stops_rate_limit_retries_at_the_configured_attempt_cap() {
    for attempts in [1, 2, 3] {
        let (output, _, requests) = run_responses_fixture_with_setup(
            vec![error_response("429 Too Many Requests", 0)],
            true,
            true,
            Some(1),
            |dir| write_retry_limits(dir, attempts),
        );
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(requests.len() as u64, attempts);
        let result: Value = serde_json::from_slice(&output.stdout).expect("ask JSON");
        assert_eq!(result["error_code"], "rate_limited");
        assert_eq!(result["final_output"], "");
    }
}

#[test]
fn ask_stops_at_once_when_the_requested_delay_is_longer_than_the_run_allows() {
    let started = Instant::now();
    let (output, _, requests) = run_responses_fixture_with_setup(
        vec![error_response("429 Too Many Requests", 3600)],
        true,
        true,
        Some(1),
        |dir| write_retry_limits(dir, 3),
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(requests.len(), 1);
    let result: Value = serde_json::from_slice(&output.stdout).expect("ask JSON");
    assert_eq!(result["error_code"], "rate_limited");
    assert_eq!(
        result["error_detail"]["hint"],
        "the provider asked to wait 3600 s, longer than this run allows"
    );
}

#[test]
fn ask_never_retries_an_invalid_credential_even_with_retry_after() {
    for status in ["401 Unauthorized", "403 Forbidden"] {
        let (output, _, requests) = run_responses_fixture_with_setup(
            vec![error_response(status, 1)],
            true,
            true,
            Some(1),
            |dir| write_retry_limits(dir, 3),
        );
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(requests.len(), 1, "{status}");
        let result: Value = serde_json::from_slice(&output.stdout).expect("ask JSON");
        assert_eq!(result["error_code"], "authentication_required");
        assert_eq!(result["final_output"], "");
    }
}

fn call_delta(name: &str, arguments: &str) -> Value {
    json!({"content": "Checking the fixture.", "tool_calls": [{
        "index": 0, "id": "call-fixture", "type": "function",
        "function": {"name": name, "arguments": arguments}
    }]})
}

#[test]
fn review_refuses_mutations_by_a_named_rule_and_still_reads_in_every_permission_mode() {
    let denied = [
        (
            "write_file",
            json!({"path": "fixture.txt", "content": "changed"}),
        ),
        (
            "edit_file",
            json!({"path": "fixture.txt", "old_string": "ALLOWED_READ_RESULT", "new_string": "changed"}),
        ),
        (
            "rename_file",
            json!({"path": "fixture.txt", "new_path": "renamed.txt"}),
        ),
        ("create_directory", json!({"path": "new-directory"})),
        (
            "shell",
            json!({"action": "run", "command": "printf changed > fixture.txt"}),
        ),
        (
            "install_skill",
            json!({"name": "fixture-skill", "content": "---\nname: fixture-skill\ndescription: Fixture\n---\nFixture body."}),
        ),
        ("web_fetch", json!({"url": "https://example.com"})),
        (
            "update_plan",
            json!({"steps": [{"step": "Change files", "status": "pending"}]}),
        ),
    ];
    for mode in ["ask", "auto", "full-access"] {
        let calls: Vec<Value> = denied
            .iter()
            .enumerate()
            .map(|(index, (name, arguments))| {
                json!({
                    "index": index, "id": format!("denied-{index}"), "type": "function",
                    "function": {"name": name, "arguments": arguments.to_string()}
                })
            })
            .collect();
        let (output, dir, requests) = run_responses_fixture_command(
            vec![
                completion_response(&json!({"tool_calls": calls}), "tool_calls"),
                completion_response(
                    &call_delta("read_file", r#"{"path":"fixture.txt"}"#),
                    "tool_calls",
                ),
                completion_response(&json!({"content": "Review complete."}), "stop"),
            ],
            Some("--json"),
            false,
            Some(4),
            (&["--permission-mode", mode, "review"], &[]),
            |dir, command| {
                command.with_external_programs();
                // Web tools are offered only when enabled, and the review must
                // refuse a network tool it was offered.
                command.env("RUNE_WEB_TOOLS", "true");
                let git = Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(dir.path())
                    .output()
                    .expect("initialize review repository");
                assert!(git.status.success(), "review fixture setup failed: {git:?}");
            },
        );
        assert!(output.status.success(), "{mode}: {output:?}");
        assert_eq!(requests.len(), 3, "{mode}");
        let messages = requests[1]["messages"].as_array().expect("tool feedback");
        let results: Vec<&Value> = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect();
        assert_eq!(results.len(), denied.len());
        for (index, result) in results.iter().enumerate() {
            assert_eq!(result["tool_call_id"], format!("denied-{index}"));
            let text = result["content"].as_str().expect("refusal");
            // A headless run does not offer these two, so they are refused
            // before policy is asked.
            if matches!(denied[index].0, "create_directory" | "install_skill") {
                assert!(
                    text.starts_with(&format!("there is no tool named `{}`", denied[index].0)),
                    "{text}"
                );
                continue;
            }
            assert!(text.contains("refused by policy"), "{text}");
            assert!(
                text.contains("matched `read-only review` at the session layer"),
                "{text}"
            );
        }
        let read = requests[2]["messages"]
            .as_array()
            .expect("read feedback")
            .last()
            .expect("read result");
        assert_eq!(read["role"], "tool");
        assert!(
            read["content"]
                .as_str()
                .expect("read content")
                .contains("ALLOWED_READ_RESULT")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("fixture.txt")).expect("unchanged file"),
            "ALLOWED_READ_RESULT"
        );
        assert!(!dir.path().join("renamed.txt").exists());
        assert!(!dir.path().join("new-directory").exists());
        assert!(!dir.path().join("config/rune/skills/fixture-skill").exists());
        let result: Value = serde_json::from_slice(&output.stdout).expect("review JSON");
        assert_eq!(result["final_output"], "Review complete.");
        let reports = result["tool_calls"].as_array().expect("call reports");
        for (report, (name, _)) in reports.iter().zip(&denied) {
            assert_eq!(report, &json!({"name": name, "status": "error"}));
        }
        assert_eq!(reports.len(), denied.len() + 1);
        assert_eq!(
            reports.last(),
            Some(&json!({"name": "read_file", "status": "success"}))
        );
        let session_dir = dir
            .path()
            .join("state/sessions")
            .join(result["session_id"].as_str().expect("session ID"));
        let saved = rune_session::load_read_only(
            camino::Utf8Path::from_path(&session_dir).expect("UTF-8 session path"),
        )
        .expect("saved review");
        let refusals = saved.events.iter().filter(|frame| matches!(&frame.event,
            rune_session::SessionEvent::ToolResult { ok: false, output, .. } if output.contains("read-only review"))).count();
        // The two tools a headless run does not offer never reach policy.
        assert_eq!(refusals, denied.len() - 2);
    }
}

#[test]
fn ask_sends_the_resolved_instructions_shown_by_prompt_show() {
    for variant in ["workspace", "override", "bounded"] {
        let (output, dir, requests) = run_responses_fixture_with_setup(
            vec![
                completion_response(
                    &call_delta("read_file", r#"{"path":"fixture.txt"}"#),
                    "tool_calls",
                ),
                completion_response(&json!({"content": "Fixture answer."}), "stop"),
            ],
            true,
            true,
            Some(3),
            |dir| {
                std::fs::write(
                    dir.path().join("AGENTS.md"),
                    format!(
                        "AUDIT_INSTRUCTION_MUST_BE_SENT\n{}",
                        "workspace rules\n".repeat(100)
                    ),
                )
                .expect("workspace instructions");
                if variant == "override" {
                    std::fs::write(
                        dir.path().join("config/rune/SYSTEM.md"),
                        "Fixture system override.",
                    )
                    .expect("system prompt override");
                } else if variant == "bounded" {
                    std::fs::write(
                        dir.path().join("config/rune/config.toml"),
                        "[limits]\nmax_agent_steps = 3\nproject_instructions_total_bytes = 512\n",
                    )
                    .expect("instruction limit");
                }
            },
        );
        assert!(output.status.success(), "{variant}: {output:?}");
        assert!(output.stderr.is_empty(), "{variant}: {output:?}");
        let shown = isolated_command(&dir)
            .args(["prompt", "--show"])
            .output()
            .expect("inspect resolved prompt");
        assert!(shown.status.success(), "{shown:?}");
        let shown = String::from_utf8(shown.stdout).expect("UTF-8 prompt");
        assert!(shown.contains("AUDIT_INSTRUCTION_MUST_BE_SENT"), "{shown}");
        if variant == "override" {
            assert!(shown.starts_with("Fixture system override."), "{shown}");
        } else if variant == "bounded" {
            assert!(
                shown.contains("truncated at the configured limit"),
                "{shown}"
            );
        }
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert_eq!(request["messages"][0]["role"], "system");
            assert_eq!(request["messages"][0]["content"], shown.trim_end());
            assert_eq!(request["messages"][1]["role"], "user");
            // The prompt, then the note that nobody can answer this run.
            let user = request["messages"][1]["content"].as_str().expect("text");
            assert!(
                user.starts_with("run the fixture command\n\n<run_context>"),
                "{user}"
            );
        }
    }
}

#[test]
fn ask_keeps_a_capped_repository_map_fixed_across_an_edit() {
    let edit = json!({"path": "src/main.rs", "old_string": "alpha();", "new_string": "beta(); "})
        .to_string();
    let (output, dir, requests) = run_responses_fixture_with_setup(
        vec![
            completion_response(&call_delta("edit_file", &edit), "tool_calls"),
            completion_response(&json!({"content": "Updated."}), "stop"),
        ],
        true,
        true,
        Some(3),
        |dir| {
            std::fs::create_dir(dir.path().join("src")).expect("sources");
            std::fs::write(
                dir.path().join("Cargo.toml"),
                "[package]\nname = 'fixture'\nversion = '0.1.0'\n",
            )
            .expect("manifest");
            std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").expect("ignore rule");
            std::fs::write(
                dir.path().join("ignored.rs"),
                "fn ignored_secret() { alpha(); }\n",
            )
            .expect("ignored source");
            let extra = (0..100).fold(String::new(), |mut text, index| {
                use std::fmt::Write as _;
                writeln!(text, "fn z{index:03}() {{}}").expect("format source");
                text
            });
            std::fs::write(
                dir.path().join("src/lib.rs"),
                format!("pub fn alpha() {{}}\npub fn beta() {{}}\n{extra}"),
            )
            .expect("definitions");
            std::fs::write(dir.path().join("src/main.rs"), "fn main() { alpha(); }\n")
                .expect("references");
            std::fs::write(
                dir.path().join("config/rune/config.toml"),
                "[limits]\nmax_agent_steps = 3\nrepository_map_bytes = 512\nverification_reminders = 0\n",
            )
            .expect("map cap");
        },
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(requests.len(), 2);
    let before = requests[0]["messages"][0]["content"]
        .as_str()
        .expect("initial instructions");
    let after = requests[1]["messages"][0]["content"]
        .as_str()
        .expect("instructions after the edit");
    assert!(
        before.contains("src/lib.rs:1 fn alpha <- src/main.rs:1"),
        "{before}"
    );
    // The edit moved the reference from alpha to beta, but the instructions
    // are byte-identical, so the cached prefix of the second request holds.
    assert_eq!(before, after);

    // A new run maps the workspace as it is now.
    let shown = isolated_command(&dir)
        .args(["prompt", "--show"])
        .output()
        .expect("prompt preview");
    assert!(shown.status.success(), "{shown:?}");
    let current = String::from_utf8(shown.stdout).expect("UTF-8 preview");
    assert!(current.contains("src/lib.rs:1 fn alpha\n"), "{current}");
    assert!(
        current.contains("src/lib.rs:2 fn beta <- src/main.rs:1"),
        "{current}"
    );
    for instructions in [before, current.as_str()] {
        let map = instructions
            .split("Repository symbol map")
            .nth(1)
            .expect("map section");
        assert!(map.trim_end().len() + "Repository symbol map".len() <= 512);
        assert!(map.contains("map incomplete"));
        assert!(!instructions.contains("ignored_secret"));
        assert!(!instructions.contains("ignored.rs"));
    }
    let disabled = isolated_command(&dir)
        .args(["--limit", "repository_map_bytes=0", "prompt", "--show"])
        .output()
        .expect("disable map");
    assert!(disabled.status.success(), "{disabled:?}");
    assert!(!String::from_utf8_lossy(&disabled.stdout).contains("Repository symbol map"));
}

#[test]
fn ask_reads_an_allowed_file_and_sends_its_result_before_the_final_answer() {
    for json_output in [true, false] {
        let (output, dir, requests) = run_responses_fixture(
            vec![
                completion_response(
                    &call_delta("read_file", r#"{"path":"fixture.txt"}"#),
                    "tool_calls",
                ),
                completion_response(&json!({"content": "Final fixture answer."}), "stop"),
            ],
            json_output,
            false,
            Some(3),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        assert_eq!(requests.len(), 2);
        let tools = requests[0]["tools"].as_array().expect("advertised tools");
        assert!(
            tools
                .iter()
                .any(|tool| tool["function"]["name"] == "read_file")
        );
        let messages = requests[1]["messages"]
            .as_array()
            .expect("second request messages");
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call-fixture");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call-fixture");
        assert!(
            messages[3]["content"]
                .as_str()
                .expect("read result")
                .contains("ALLOWED_READ_RESULT")
        );
        if json_output {
            let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
            assert_eq!(result["steps"], 2);
            assert_eq!(result["exit_code"], 0);
            assert_eq!(result["final_output"], "Final fixture answer.");
            assert_eq!(
                result["tool_calls"],
                json!([{"name": "read_file", "status": "success"}])
            );
            assert_eq!(
                result["usage"],
                json!({"input_tokens": 22, "output_tokens": 4})
            );
            let session_id = result["session_id"].as_str().expect("session ID");
            let session_dir = dir.path().join("state/sessions").join(session_id);
            let state = rune_session::load_read_only(
                camino::Utf8Path::from_path(&session_dir).expect("UTF-8 path"),
            )
            .expect("saved tool exchange");
            assert!(state.events.iter().any(|frame| matches!(&frame.event,
                rune_session::SessionEvent::ToolResult { ok: true, output, .. } if output.contains("ALLOWED_READ_RESULT"))));
            assert_eq!(state.usage.input_tokens, 22);
        } else {
            assert_eq!(output.stdout, b"Final fixture answer.\n");
        }
    }
}

#[test]
fn ask_returns_denied_unknown_and_malformed_calls_to_the_model_without_running_them() {
    for (name, arguments, expected) in [
        (
            "shell",
            r#"{"action":"run","command":"printf SHOULD_NOT_RUN > forbidden.txt"}"#,
            "no rule allows",
        ),
        ("read_file", r#"{"path":"../outside.txt"}"#, "outside"),
        ("nonexistent_tool", "{}", "there is no tool"),
        ("read_file", "{broken", "not valid JSON"),
    ] {
        let (output, dir, requests) = run_responses_fixture(
            vec![
                completion_response(&call_delta(name, arguments), "tool_calls"),
                completion_response(&json!({"content": "The tool failed."}), "stop"),
            ],
            true,
            true,
            Some(3),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
        assert_eq!(
            result["tool_calls"],
            json!([{"name": name, "status": "error"}])
        );
        let tool_result = requests[1]["messages"][3]["content"]
            .as_str()
            .expect("failure feedback");
        assert!(tool_result.contains(expected), "{tool_result}");
        assert!(!dir.path().join("forbidden.txt").exists());
        assert!(!dir.path().join("state/sessions").exists());
    }
}

#[test]
fn ask_stops_repeated_tool_requests_at_the_configured_step_cap() {
    let responses = (0..2)
        .map(|_| {
            completion_response(
                &call_delta("read_file", r#"{"path":"fixture.txt"}"#),
                "tool_calls",
            )
        })
        .collect();
    let (output, _, requests) = run_responses_fixture(responses, true, true, Some(2));
    assert_eq!(output.status.code(), Some(3));
    let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
    assert_eq!(result["steps"], 2);
    assert_eq!(requests.len(), 2);
    assert_eq!(result["error_code"], "limit_exceeded");
    assert_eq!(result["final_output"], "Checking the fixture.");
    assert_eq!(result["tool_calls"].as_array().expect("calls").len(), 2);
    assert_eq!(result["usage"]["input_tokens"], 22);
}

#[test]
fn ask_without_a_step_cap_runs_twenty_tool_steps() {
    let mut responses: Vec<String> = (0..20)
        .map(|index| {
            completion_response(
                &call_delta(
                    "read_file",
                    &format!(r#"{{"path":"fixture.txt","start_line":{}}}"#, index + 1),
                ),
                "tool_calls",
            )
        })
        .collect();
    responses.push(completion_response(
        &json!({"content": "All read."}),
        "stop",
    ));
    for configured_cap in [None, Some(0)] {
        let (output, _, requests) =
            run_responses_fixture(responses.clone(), true, true, configured_cap);
        assert_eq!(output.status.code(), Some(0), "{configured_cap:?}");
        let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
        assert_eq!(result["steps"], 21);
        assert_eq!(requests.len(), 21);
        assert_eq!(result["final_output"], "All read.");
    }
}

/// Holds the provider response open until stdout has delivered the early records.
fn streaming_jsonl_fixture(delta: &str, complete: bool) -> (Vec<Value>, Output) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("streaming fixture");
    let address = listener.local_addr().expect("address");
    let (release, gate) = std::sync::mpsc::channel();
    let first = format!(
        "data: {}\n\n",
        json!({"choices": [{"delta": {"content": delta}, "finish_reason": null}]})
    );
    let tail = if complete {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 11, "completion_tokens": 7}})
        )
    } else {
        String::new()
    };
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("request");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read deadline");
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .expect("write deadline");
        let mut reader = std::io::BufReader::new(&mut stream);
        let mut length = None;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).expect("header") > 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = Some(value.trim().parse::<usize>().expect("length"));
            }
        }
        let mut request = vec![0; length.expect("request length")];
        reader.read_exact(&mut request).expect("body");
        drop(reader);
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{first}", first.len().saturating_add(tail.len()).saturating_add(usize::from(!complete))).expect("early delta");
        stream.flush().expect("flush delta");
        gate.recv_timeout(Duration::from_secs(10))
            .expect("stdout observed before completion");
        stream.write_all(tail.as_bytes()).expect("finish response");
    });
    let dir = tempfile::tempdir().expect("isolated state");
    std::fs::create_dir_all(dir.path().join("config/rune")).expect("config directory");
    write_retry_limits(&dir, 1);
    let mut child = isolated_command(&dir)
        .env("RUNE_PROVIDER", "chat_completions")
        .env("RUNE_BASE_URL", format!("http://{address}/v1"))
        .env("RUNE_MODEL", "fixture-model")
        .env("RUNE_API_KEY_ENV", "RUNE_ASK_TEST_KEY")
        .env("RUNE_ASK_TEST_KEY", "fixture-key")
        .args(["ask", "--jsonl", "--json", "--no-save", "hello"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ask JSONL");
    let stdout = child.stdout.take().expect("stdout");
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            send.send(line.expect("stdout line"))
                .expect("line consumer");
        }
    });
    let mut events = Vec::new();
    loop {
        let line = receive
            .recv_timeout(Duration::from_secs(5))
            .expect("event before completion");
        assert!(line.len() < 1024 * 1024, "bounded JSONL record");
        let event: Value = serde_json::from_str(&line).expect("stdout contains only JSON objects");
        let got_text = event["type"] == "text_delta";
        events.push(event);
        if got_text {
            break;
        }
    }
    assert_eq!(events[0]["type"], "start");
    assert_eq!(events[0]["schema"], 2);
    assert_eq!(events[1]["type"], "prompt");
    assert_eq!(events[2]["type"], "step_start");
    assert_eq!(events[2]["step"], 1);
    assert!(child.try_wait().expect("running status").is_none());
    release.send(()).expect("release provider completion");
    while let Ok(line) = receive.recv_timeout(Duration::from_secs(5)) {
        assert!(line.len() < 1024 * 1024, "bounded JSONL record");
        events.push(serde_json::from_str(&line).expect("JSON event"));
    }
    reader.join().expect("stdout reader");
    let output = child.wait_with_output().expect("completion");
    server.join().expect("provider fixture");
    assert!(!dir.path().join("state/sessions").exists(), "no-save");
    (events, output)
}

#[test]
fn ask_jsonl_delivers_flushed_events_before_provider_completion() {
    let delta = "STREAMED 🌱\n\"\0".repeat(3000);
    let (events, output) = streaming_jsonl_fixture(&delta, true);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty());
    let chunks: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "text_delta")
        .collect();
    assert!(chunks.len() > 1);
    let joined: String = chunks
        .iter()
        .map(|event| event["delta"].as_str().expect("delta"))
        .collect();
    assert_eq!(joined, delta);
    assert!(chunks.iter().all(|event| event["step"] == 1));
    let final_event = events.last().expect("final event");
    assert_eq!(final_event["type"], "final");
    let result = &final_event["result"];
    assert_eq!(result["output"], delta);
    assert_eq!(result["final_output"], delta);
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["steps"], 1);
    assert_eq!(result["session_id"], "");
    assert_eq!(
        result["usage"],
        json!({"input_tokens": 11, "output_tokens": 7})
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "final")
            .count(),
        1
    );
}

#[test]
fn ask_jsonl_bounds_the_final_record_and_reports_size_failure_on_stderr() {
    let (events, output) = streaming_jsonl_fixture(&"a".repeat(600_000), true);
    assert_eq!(output.status.code(), Some(1));
    let final_event = events.last().expect("final");
    assert_eq!(final_event["type"], "final");
    assert_eq!(final_event["result"]["error_code"], "too_large");
    assert_eq!(final_event["result"]["exit_code"], 1);
    assert_eq!(final_event["result"]["output"], "");
    assert!(String::from_utf8_lossy(&output.stderr).contains("jsonl_event"));
}

#[test]
fn ask_jsonl_stream_failure_emits_a_final_failure_and_stderr_diagnostic() {
    let (events, output) = streaming_jsonl_fixture("partial", false);
    assert_eq!(output.status.code(), Some(1));
    let final_event = events.last().expect("final");
    assert_eq!(final_event["type"], "final");
    assert_eq!(final_event["result"]["exit_code"], 1);
    assert!(final_event["result"]["error_code"].is_string());
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("rune: "));
}

#[test]
fn ask_jsonl_validation_failures_emit_start_and_final_only() {
    for (args, code) in [
        (vec!["ask", "--jsonl", "hello"], "authentication_required"),
        (vec!["ask", "--jsonl"], "missing_field"),
    ] {
        let dir = tempfile::tempdir().expect("isolated state");
        let output = isolated_command(&dir)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("ask");
        assert_eq!(output.status.code(), Some(1));
        let events: Vec<Value> = String::from_utf8(output.stdout)
            .expect("UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON event"))
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "start");
        assert_eq!(events[0]["schema"], 2);
        assert_eq!(events[1]["type"], "final");
        assert_eq!(events[1]["result"]["error_code"], code);
        assert_eq!(
            String::from_utf8_lossy(&output.stderr)
                .matches("rune: ")
                .count(),
            1
        );
    }
}

#[test]
fn ask_jsonl_marks_retry_resets_and_keeps_the_recovered_final_result() {
    // Text followed by a transient failure is sent again in full; a stream
    // that is only cut keeps its text instead.
    let partial = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"choices": [{"delta": {"content": "Discard this attempt."}, "finish_reason": null}]}),
        json!({"error": {"message": "the model is overloaded", "type": "overloaded_error"}})
    );
    let incomplete = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{partial}",
        partial.len()
    );
    let (output, _, requests) = run_responses_fixture_with_format(
        vec![
            incomplete,
            completion_response(&json!({"content": "Recovered."}), "stop"),
        ],
        Some("--jsonl"),
        true,
        Some(2),
        |dir| write_retry_limits(dir, 2),
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty());
    assert_eq!(requests.len(), 2);
    let events: Vec<Value> = std::str::from_utf8(&output.stdout)
        .expect("UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON event"))
        .collect();
    assert_eq!(
        events
            .iter()
            .map(|event| event["type"].as_str().expect("type"))
            .collect::<Vec<_>>(),
        [
            "start",
            "prompt",
            "step_start",
            "text_delta",
            "retry",
            "step_restart",
            "text_delta",
            "step_finish",
            "final"
        ]
    );
    assert_eq!(events[3]["delta"], "Discard this attempt.");
    assert_eq!(events[4]["attempt"], 2);
    // The next attempt's time, in the same form as `ts` and no earlier.
    let retry_at = events[4]["retry_at"].as_str().expect("retry_at");
    let ts = events[4]["ts"].as_str().expect("ts");
    assert_eq!(retry_at.len(), ts.len(), "{retry_at}");
    assert!(retry_at.ends_with('Z') && retry_at >= ts, "{retry_at} {ts}");
    assert_eq!(events[5]["step"], 1);
    assert_eq!(events[6]["step"], 1);
    assert_eq!(events[7]["step"], 1);
    assert_eq!(events[8]["result"]["final_output"], "Recovered.");
    assert_eq!(events[8]["result"]["steps"], 1);
}

#[test]
fn ask_jsonl_preserves_tool_steps_and_saved_exchange() {
    let (output, dir, requests) = run_responses_fixture_with_format(
        vec![
            completion_response(
                &json!({"content": "Reading.", "tool_calls": call_delta("read_file", r#"{"path":"fixture.txt"}"#)["tool_calls"]}),
                "tool_calls",
            ),
            completion_response(&json!({"content": "Final answer."}), "stop"),
        ],
        Some("--jsonl"),
        false,
        Some(3),
        |_| {},
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty());
    assert_eq!(requests.len(), 2);
    let events: Vec<Value> = std::str::from_utf8(&output.stdout)
        .expect("UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON event"))
        .collect();
    assert_eq!(
        events
            .iter()
            .map(|event| event["type"].as_str().expect("type"))
            .collect::<Vec<_>>(),
        [
            "start",
            "prompt",
            "step_start",
            "text_delta",
            "step_finish",
            "tool_call",
            "tool_result",
            "step_start",
            "text_delta",
            "step_finish",
            "final"
        ]
    );
    assert_eq!(events[3]["step"], 1);
    assert_eq!(events[3]["delta"], "Reading.");
    assert_eq!(events[5]["id"], events[6]["id"]);
    assert_eq!(events[5]["arguments"], json!({"path": "fixture.txt"}));
    assert_eq!(events[6]["ok"], true);
    assert_eq!(events[8]["step"], 2);
    let result = &events[10]["result"];
    assert_eq!(result["final_output"], "Final answer.");
    assert_eq!(
        result["tool_calls"],
        json!([{"name": "read_file", "status": "success"}])
    );
    let session_dir = dir
        .path()
        .join("state/sessions")
        .join(result["session_id"].as_str().expect("saved ID"));
    let state = rune_session::load_read_only(
        camino::Utf8Path::from_path(&session_dir).expect("UTF-8 path"),
    )
    .expect("saved JSONL exchange");
    assert!(state.events.iter().any(|frame| matches!(&frame.event,
        rune_session::SessionEvent::ToolResult { ok: true, output, .. } if output.contains("ALLOWED_READ_RESULT"))));
}

const ATTACHMENT_PNG: &[u8] = include_bytes!("fixtures/attachment.png");
const ATTACHMENT_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

#[test]
fn ask_attaches_one_png_content_part_on_every_tool_step() {
    let (output, _, requests) = run_responses_fixture_with_images(
        vec![
            completion_response(
                &json!({"tool_calls": [{"index": 0, "id": "read-image-context", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"fixture.txt\"}"}}]}),
                "tool_calls",
            ),
            completion_response(&json!({"content": "image received"}), "stop"),
        ],
        Some("--json"),
        true,
        Some(3),
        &["attachment.png"],
        |dir| {
            std::fs::write(dir.path().join("attachment.png"), ATTACHMENT_PNG).expect("fixture PNG");
        },
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(requests.len(), 2);
    for request in requests {
        let users: Vec<_> = request["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|message| message["role"] == "user")
            .collect();
        assert_eq!(users.len(), 1);
        // The prompt and the run note come first, then the image.
        let content = users[0]["content"].as_array().expect("parts");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert!(
            content[0]["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("run the fixture command\n\n<run_context>")),
            "{content:?}"
        );
        assert_eq!(
            content[1],
            json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{ATTACHMENT_PNG_BASE64}")}})
        );
    }
    let result: Value = serde_json::from_slice(&output.stdout).expect("JSON result");
    assert_eq!(result["final_output"], "image received");
}

#[test]
fn ask_repeated_image_flags_preserve_attachment_order() {
    let (output, _, requests) = run_responses_fixture_with_images(
        vec![completion_response(
            &json!({"content": "two images"}),
            "stop",
        )],
        None,
        true,
        Some(2),
        &["first.png", "second.gif"],
        |dir| {
            std::fs::write(dir.path().join("first.png"), ATTACHMENT_PNG).expect("fixture PNG");
            std::fs::write(dir.path().join("second.gif"), b"GIF89a").expect("signature fixture");
        },
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(requests.len(), 1);
    let user = requests[0]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "user")
        .expect("user");
    assert_eq!(user["content"].as_array().expect("parts").len(), 3);
    assert_eq!(
        user["content"][1]["image_url"]["url"],
        format!("data:image/png;base64,{ATTACHMENT_PNG_BASE64}")
    );
    assert_eq!(
        user["content"][2]["image_url"]["url"],
        "data:image/gif;base64,R0lGODlh"
    );
}

#[test]
fn ask_oversized_image_and_batch_fail_before_any_provider_request() {
    const CAP: u64 = 8 * 1024 * 1024;
    for (images, size, format) in [
        (vec!["large.png"], CAP + 1, "--json"),
        (vec!["large.png"], CAP + 1, "--jsonl"),
        (vec!["attachment.png", "large.png"], CAP, "--json"),
    ] {
        let (output, dir, requests) = run_responses_fixture_with_images(
            vec![completion_response(
                &json!({"content": "must not be requested"}),
                "stop",
            )],
            Some(format),
            false,
            Some(1),
            &images,
            |dir| {
                std::fs::write(dir.path().join("attachment.png"), ATTACHMENT_PNG)
                    .expect("fixture PNG");
                let mut file =
                    std::fs::File::create(dir.path().join("large.png")).expect("oversized fixture");
                file.write_all(ATTACHMENT_PNG).expect("PNG header");
                file.set_len(size).expect("sparse oversized image");
            },
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(requests.is_empty(), "oversized input must not be uploaded");
        let result: Value = if format == "--jsonl" {
            let records: Vec<Value> = String::from_utf8(output.stdout)
                .expect("JSONL")
                .lines()
                .map(|line| serde_json::from_str(line).expect("record"))
                .collect();
            assert_eq!(records.len(), 2);
            assert_eq!(records[0]["type"], "start");
            assert_eq!(records[1]["type"], "final");
            records[1]["result"].clone()
        } else {
            serde_json::from_slice(&output.stdout).expect("JSON result")
        };
        assert_eq!(result["error_code"], "too_large");
        assert_eq!(result["error_detail"]["field"], "image");
        assert_eq!(result["steps"], 0);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("too_large") && stderr.contains("8388608"),
            "{stderr}"
        );
        assert!(!dir.path().join("state/sessions").exists());
    }
}

#[test]
fn model_plan_updates_emit_versioned_events_without_duplicates_or_workspace_mutations() {
    let initial = json!({"plan": [
        {"step": "Inspect", "status": "in_progress"},
        {"step": "Implement", "status": "pending"},
        {"step": "Verify", "status": "pending"}
    ]});
    let progress = json!({"explanation": "Inspection finished", "plan": [
        {"step": "Inspect", "status": "completed"},
        {"step": "Implement", "status": "in_progress"},
        {"step": "Verify", "status": "pending"}
    ]});
    let duplicate = json!({"plan": [
        {"step": "Inspect", "status": "completed"},
        {"step": " Inspect ", "status": "pending"}
    ]});
    let completed = json!({"plan": [
        {"step": "Inspect", "status": "completed"},
        {"step": "Implement", "status": "completed"},
        {"step": "Verify", "status": "completed"}
    ]});
    let mut responses: Vec<_> = [&initial, &progress, &progress, &duplicate, &completed]
        .into_iter()
        .enumerate()
        .map(|(index, arguments)| {
            let mut delta = call_delta("update_plan", &arguments.to_string());
            delta["tool_calls"][0]["id"] = json!(format!("plan-{index}"));
            completion_response(&delta, "tool_calls")
        })
        .collect();
    responses.push(completion_response(
        &json!({"content": "Plan finished."}),
        "stop",
    ));
    let (output, dir, requests) =
        run_responses_fixture_with_format(responses, Some("--jsonl"), true, Some(7), |_| {});
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty());
    assert_eq!(requests.len(), 6);
    assert!(
        requests[0]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["function"]["name"] == "update_plan")
    );
    let events: Vec<Value> = std::str::from_utf8(&output.stdout)
        .expect("UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON event"))
        .collect();
    let updates: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "plan_updated")
        .collect();
    assert_eq!(
        updates.len(),
        3,
        "retry and rejected update must emit no event"
    );
    for (index, update) in updates.iter().enumerate() {
        assert_eq!(update["update"]["version"], index + 1);
        assert_eq!(update["update"]["plan"].as_array().expect("steps").len(), 3);
        assert_eq!(update["update"]["plan"][0]["step"], "Inspect");
        assert_eq!(update["update"]["plan"][1]["step"], "Implement");
        assert_eq!(update["update"]["plan"][2]["step"], "Verify");
    }
    assert_eq!(updates[0]["update"]["plan"], initial["plan"]);
    assert_eq!(updates[1]["update"]["plan"], progress["plan"]);
    assert_eq!(updates[2]["update"]["plan"], completed["plan"]);
    for (index, version) in [(1, 1), (2, 2), (3, 2), (5, 3)] {
        let result = requests[index]["messages"]
            .as_array()
            .expect("messages")
            .last()
            .expect("tool result");
        assert_eq!(result["role"], "tool");
        let snapshot: Value =
            serde_json::from_str(result["content"].as_str().expect("result")).expect("snapshot");
        assert_eq!(snapshot["version"], version);
        assert_eq!(snapshot["plan"].as_array().expect("steps").len(), 3);
    }
    let rejected = requests[4]["messages"]
        .as_array()
        .expect("messages")
        .last()
        .expect("error");
    assert!(
        rejected["content"]
            .as_str()
            .expect("error text")
            .contains("distinct")
    );
    assert_eq!(
        events.last().expect("final")["result"]["final_output"],
        "Plan finished."
    );
    assert_eq!(
        std::fs::read(dir.path().join("fixture.txt")).expect("unchanged fixture"),
        b"ALLOWED_READ_RESULT"
    );
    let mut files: Vec<_> = std::fs::read_dir(dir.path())
        .expect("workspace")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    files.sort();
    assert_eq!(
        files,
        ["config", "fixture.txt"].map(std::ffi::OsString::from)
    );
}

#[cfg(unix)]
#[test]
fn user_pre_tool_hook_veto_preserves_the_pair_and_reports_its_named_reason() {
    // A new file, so the veto is the hook's and not the guard on overwriting
    // a file that was never read.
    let arguments = json!({"path": "protected.txt", "content": "MUST_NOT_WRITE"}).to_string();
    let (output, dir, requests) = run_responses_fixture_command(
        vec![
            completion_response(&call_delta("write_file", &arguments), "tool_calls"),
            completion_response(&json!({"content": "The guard refused the write."}), "stop"),
        ],
        Some("--json"),
        false,
        Some(3),
        (&["--permission-mode", "full-access", "ask"], &[]),
        |dir, command| {
            command.with_external_programs();
            std::fs::write(
                dir.path().join("guard.sh"),
                r#"printf '%s' "$RUNE_PRE_TOOL_CALL" >> hook-input.json
printf '%s' "${RUNE_ASK_TEST_KEY-unset}" > hook-key.txt
printf '%s' '{"decision":"veto","reason":"protected fixture"}'
"#,
            )
            .expect("hook script");
            let config = dir.path().join("config/rune/config.toml");
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(config)
                .expect("config");
            writeln!(
                file,
                "\n[pre_tool_hook]\nname = 'fixture guard'\nargv = ['/bin/sh', 'guard.sh']"
            )
            .expect("configure hook");
        },
    );
    assert!(output.status.success(), "{output:?}");
    assert!(!dir.path().join("protected.txt").exists());
    let input: Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("hook-input.json")).expect("hook input"),
    )
    .expect("exactly one invocation");
    assert_eq!(
        input,
        json!({"event":"pre_tool", "id":"call-fixture", "name":"write_file", "arguments":serde_json::from_str::<Value>(&arguments).expect("arguments")})
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hook-key.txt")).expect("environment"),
        "unset"
    );
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().expect("messages");
    let call = messages
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("assistant call");
    assert_eq!(call["tool_calls"][0]["id"], "call-fixture");
    let result = messages.last().expect("tool result");
    assert_eq!(result["role"], "tool");
    assert_eq!(result["tool_call_id"], "call-fixture");
    assert_eq!(
        result["content"],
        "pre-tool hook `fixture guard` vetoed: protected fixture"
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("ask JSON");
    assert_eq!(
        result["tool_calls"],
        json!([{"name":"write_file", "status":"error"}])
    );
    let session_dir = dir
        .path()
        .join("state/sessions")
        .join(result["session_id"].as_str().expect("session ID"));
    let saved = rune_session::load_read_only(
        camino::Utf8Path::from_path(&session_dir).expect("UTF-8 path"),
    )
    .expect("saved session");
    assert!(saved.events.iter().any(|frame| matches!(&frame.event,
        rune_session::SessionEvent::ToolResult { ok: false, output, .. }
            if output == "pre-tool hook `fixture guard` vetoed: protected fixture")));
}

#[cfg(unix)]
#[test]
fn pre_tool_allow_runs_the_tool_and_hook_failures_hold_it() {
    for (script, timeout_ms, expected) in [
        (
            r#"printf '%s' '{"decision":"allow"}'"#,
            5000,
            "ALLOWED_READ_RESULT",
        ),
        ("exit 1", 5000, "exited with status 1"),
        ("printf garbage", 5000, "hook failed"),
        ("sleep 10", 20, "exceeded its deadline"),
        ("head -c 5000 /dev/zero", 5000, "output exceeds 4096 bytes"),
    ] {
        let (output, dir, requests) = run_responses_fixture_command(
            vec![
                completion_response(
                    &call_delta("read_file", r#"{"path":"fixture.txt"}"#),
                    "tool_calls",
                ),
                completion_response(&json!({"content":"done"}), "stop"),
            ],
            Some("--json"),
            true,
            Some(3),
            (&["ask"], &[]),
            |dir, command| {
                command.with_external_programs();
                std::fs::write(dir.path().join("guard.sh"), script).expect("hook script");
                let config = dir.path().join("config/rune/config.toml");
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(config)
                    .expect("config");
                writeln!(file, "\n[pre_tool_hook]\nname = 'fixture guard'\nargv = ['/bin/sh', 'guard.sh']\ntimeout_ms = {timeout_ms}").expect("configure hook");
            },
        );
        assert!(output.status.success(), "{script}: {output:?}");
        let result = requests[1]["messages"]
            .as_array()
            .expect("messages")
            .last()
            .expect("tool result");
        let text = result["content"].as_str().expect("output");
        assert!(text.contains(expected), "{script}: {text}");
        if expected != "ALLOWED_READ_RESULT" {
            assert!(text.contains("pre-tool hook `fixture guard`"), "{text}");
            assert!(!text.contains("ALLOWED_READ_RESULT"), "tool ran: {text}");
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("fixture.txt")).expect("file"),
            "ALLOWED_READ_RESULT"
        );
    }
}

#[cfg(unix)]
#[test]
fn user_post_tool_hook_receives_one_redacted_result_without_changing_the_tool_pair() {
    let original = "ordinary result\nfixture-key\nsk-private-fixture\npassword=short\n";
    let (output, dir, requests) = run_responses_fixture_command(
        vec![
            completion_response(
                &call_delta("read_file", r#"{"path":"fixture.txt"}"#),
                "tool_calls",
            ),
            completion_response(&json!({"content": "done"}), "stop"),
        ],
        Some("--json"),
        false,
        Some(3),
        (&["ask"], &[]),
        |dir, command| {
            command.with_external_programs();
            std::fs::write(dir.path().join("fixture.txt"), original).expect("fixture");
            std::fs::write(
                dir.path().join("observer.sh"),
                r#"printf '%s' "$RUNE_POST_TOOL_RESULT" >> hook-input.json
printf '%s' "${RUNE_ASK_TEST_KEY-unset}" > hook-key.txt
printf '%s' 'ignored hook output'
"#,
            )
            .expect("script");
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(dir.path().join("config/rune/config.toml"))
                .expect("config");
            writeln!(
                file,
                "\n[post_tool_hook]\nname = 'fixture observer'\nargv = ['/bin/sh', 'observer.sh']"
            )
            .expect("configure hook");
        },
    );
    assert!(output.status.success(), "{output:?}");
    let event_bytes = std::fs::read(dir.path().join("hook-input.json")).expect("hook event");
    let event: Value = serde_json::from_slice(&event_bytes).expect("exactly one JSON event");
    assert_eq!(event["event"], "post_tool");
    assert_eq!(event["id"], "call-fixture");
    assert_eq!(event["name"], "read_file");
    assert_eq!(event["executed"], true);
    assert_eq!(event["is_error"], false);
    assert_eq!(event["output_truncated"], false);
    assert!(
        event["output"]
            .as_str()
            .expect("output")
            .contains("ordinary result")
    );
    let text = String::from_utf8(event_bytes).expect("UTF-8");
    for secret in ["fixture-key", "sk-private-fixture", "short"] {
        assert!(!text.contains(secret), "leaked {secret}: {text}");
    }
    assert!(!event.as_object().expect("event").contains_key("arguments"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hook-key.txt")).expect("environment"),
        "unset"
    );
    assert_eq!(requests.len(), 2);
    let result = requests[1]["messages"]
        .as_array()
        .expect("messages")
        .last()
        .expect("result");
    assert_eq!(result["role"], "tool");
    assert_eq!(result["tool_call_id"], "call-fixture");
    assert!(original.lines().all(|line| {
        result["content"]
            .as_str()
            .expect("tool text")
            .contains(line)
    }));
    assert!(
        !result["content"]
            .as_str()
            .expect("tool text")
            .contains("ignored hook output")
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("ask JSON");
    let session_dir = dir
        .path()
        .join("state/sessions")
        .join(result["session_id"].as_str().expect("session"));
    let saved =
        rune_session::load_read_only(camino::Utf8Path::from_path(&session_dir).expect("UTF-8"))
            .expect("session");
    assert_eq!(saved.events.iter().filter(|frame| matches!(&frame.event,
        rune_session::SessionEvent::ToolResult { ok: true, output, .. } if original.lines().all(|line| output.contains(line)))).count(), 1);
}

#[cfg(unix)]
fn configure_hooks(dir: &tempfile::TempDir, hooks: &[(&str, &str, &str)]) {
    let config = dir.path().join("config/rune/config.toml");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(config)
        .expect("config");
    for (index, (event, name, script)) in hooks.iter().enumerate() {
        let path = format!("hook-{index}.sh");
        std::fs::write(dir.path().join(&path), script).expect("hook script");
        writeln!(
            file,
            "\n[[hooks]]\nevent = '{event}'\nname = '{name}'\nargv = ['/bin/sh', '{path}']"
        )
        .expect("configure hook");
    }
}

#[cfg(unix)]
#[test]
fn session_hooks_run_in_order_and_add_prompt_context() {
    let log = "cat >> events.jsonl; echo >> events.jsonl\n";
    let context = "cat >> events.jsonl; echo >> events.jsonl\nprintf '%s' '{\"context\":\"branch is main\"}'\n";
    let (output, dir, requests) = run_responses_fixture_command(
        vec![completion_response(&json!({"content": "Done."}), "stop")],
        Some("--json"),
        false,
        Some(3),
        (&["ask"], &[]),
        |dir, command| {
            command.with_external_programs();
            configure_hooks(
                dir,
                &[
                    ("session_end", "end", log),
                    ("notification", "note", log),
                    ("user_prompt", "branch", context),
                    ("session_start", "start", log),
                ],
            );
        },
    );
    assert!(output.status.success(), "{output:?}");
    let events: Vec<Value> = std::fs::read_to_string(dir.path().join("events.jsonl"))
        .expect("events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event JSON"))
        .collect();
    let names: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().expect("event"))
        .collect();
    assert_eq!(
        names,
        [
            "session_start",
            "user_prompt",
            "notification",
            "session_end"
        ]
    );
    assert_eq!(events[0]["resumed"], false);
    assert_eq!(events[2]["kind"], "turn_finished");
    assert_eq!(events[2]["message"], "turn finished");
    assert_eq!(events[3]["reason"], "exit");
    let result: Value = serde_json::from_slice(&output.stdout).expect("ask JSON");
    assert_eq!(events[0]["session_id"], result["session_id"]);
    assert_eq!(requests.len(), 1);
    let prompt = requests[0]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "user")
        .expect("prompt")["content"]
        .to_string();
    assert!(
        prompt.contains(r#"<hook-context name=\"branch\">\nbranch is main\n</hook-context>"#),
        "{prompt}"
    );
}

#[cfg(unix)]
fn write_project_hooks(dir: &tempfile::TempDir) {
    std::fs::create_dir_all(dir.path().join(".rune")).expect("project directory");
    std::fs::write(
        dir.path().join(".rune/hooks.toml"),
        "[[hooks]]\nevent = 'session_start'\nname = 'marker'\n\
         argv = ['/bin/sh', '-c', 'echo project >> order.txt']\n",
    )
    .expect("hooks file");
}

#[cfg(unix)]
#[test]
fn unapproved_project_hooks_are_skipped_with_a_warning() {
    // Full access does not approve them either.
    let (output, dir, requests) = run_responses_fixture_command(
        vec![completion_response(&json!({"content": "Done."}), "stop")],
        Some("--json"),
        false,
        Some(3),
        (&["--permission-mode", "full-access", "ask"], &[]),
        |dir, command| {
            command.with_external_programs();
            write_project_hooks(dir);
        },
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(requests.len(), 1);
    assert!(!dir.path().join("order.txt").exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "warning: project hooks in .rune/hooks.toml are not approved and were skipped"
        ),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn approved_project_hooks_run() {
    let (output, dir, _) = run_responses_fixture_command(
        vec![completion_response(&json!({"content": "Done."}), "stop")],
        Some("--json"),
        false,
        Some(3),
        (&["ask"], &[]),
        |dir, command| {
            command.with_external_programs();
            configure_hooks(
                dir,
                &[("session_start", "user", "echo user >> order.txt\n")],
            );
            write_project_hooks(dir);
            let approval = isolated_command(dir)
                .args(["projects", "approve", "--yes"])
                .stdin(Stdio::null())
                .output()
                .expect("approve");
            assert!(approval.status.success(), "{approval:?}");
            assert!(!dir.path().join("order.txt").exists());
        },
    );
    assert!(output.status.success(), "{output:?}");
    // After the user's hooks of the same event.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("order.txt")).expect("hooks ran"),
        "user\nproject\n"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("project hooks"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn a_blocking_user_prompt_hook_stops_the_run() {
    let (output, dir, requests) = run_responses_fixture_command(
        Vec::new(),
        Some("--json"),
        false,
        Some(3),
        (&["ask"], &[]),
        |dir, command| {
            command.with_external_programs();
            configure_hooks(
                dir,
                &[
                    (
                        "user_prompt",
                        "guard",
                        "printf '%s' '{\"decision\":\"block\",\"reason\":\"not today\"}'",
                    ),
                    ("session_end", "end", "cat > end.json"),
                ],
            );
        },
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(requests.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("prompt blocked by hook guard: not today"),
        "{stderr}"
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("ask JSON");
    assert_eq!(result["error_code"], "permission_denied");
    assert_eq!(result["exit_code"], 2);
    let end: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("end.json")).expect("session end"))
            .expect("JSON");
    assert_eq!(end["reason"], "exit");
}

#[cfg(unix)]
#[test]
fn post_tool_timeout_and_nonzero_exit_preserve_completed_results_and_continue() {
    for (script, timeout_ms, expected) in [
        (
            "printf '%s' \"$RUNE_POST_TOOL_RESULT\" >> hook-input.json; sleep 30",
            100,
            "exceeded its deadline",
        ),
        (
            "printf '%s' \"$RUNE_POST_TOOL_RESULT\" >> hook-input.json; exit 7",
            5000,
            "exited with status 7",
        ),
    ] {
        let started = Instant::now();
        let (output, dir, requests) = run_responses_fixture_command(
            vec![
                completion_response(
                    &call_delta("read_file", r#"{"path":"fixture.txt"}"#),
                    "tool_calls",
                ),
                completion_response(&json!({"content":"done"}), "stop"),
            ],
            Some("--json"),
            true,
            Some(3),
            (&["ask"], &[]),
            |dir, command| {
                command.with_external_programs();
                std::fs::write(dir.path().join("observer.sh"), script).expect("script");
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(dir.path().join("config/rune/config.toml"))
                    .expect("config");
                writeln!(file, "\n[post_tool_hook]\nname = 'fixture observer'\nargv = ['/bin/sh', 'observer.sh']\ntimeout_ms = {timeout_ms}").expect("configure hook");
            },
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "hook was not bounded"
        );
        assert!(output.status.success(), "{output:?}");
        let event: Value = serde_json::from_slice(
            &std::fs::read(dir.path().join("hook-input.json")).expect("event"),
        )
        .expect("exactly once");
        assert_eq!(event["is_error"], false);
        let stderr = String::from_utf8(output.stderr).expect("stderr");
        assert!(
            stderr.contains("post-tool hook `fixture observer` failed"),
            "{stderr}"
        );
        assert!(stderr.contains(expected), "{stderr}");
        let result = requests[1]["messages"]
            .as_array()
            .expect("messages")
            .last()
            .expect("result");
        assert!(
            result["content"]
                .as_str()
                .expect("output")
                .contains("ALLOWED_READ_RESULT")
        );
    }
}

#[test]
fn ask_help_lists_the_exit_codes() {
    let output = Command::new(env!("CARGO_BIN_EXE_rune"))
        .args(["help", "ask"])
        .output()
        .expect("help");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Exit status:"), "{text}");
    assert!(text.contains("3    a limit ended the run"), "{text}");
    assert!(text.contains("143  stopped by SIGTERM or SIGHUP"), "{text}");
}

#[test]
fn ask_judge_needs_a_goal() {
    let dir = tempfile::tempdir().expect("isolated state");
    let output = isolated_command(&dir)
        .args(["ask", "--judge", "hello"])
        .output()
        .expect("ask");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("needs --goal"), "{stderr}");
    assert!(
        stderr.contains("pass --goal <objective> as well"),
        "{stderr}"
    );
}

#[test]
fn ask_context_report_needs_a_json_output() {
    let dir = tempfile::tempdir().expect("isolated state");
    let output = isolated_command(&dir)
        .args(["ask", "--context-report", "hello"])
        .output()
        .expect("ask");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--context-report needs --json or --jsonl"),
        "{stderr}"
    );
}

#[test]
fn ask_max_steps_rejects_a_non_number() {
    let dir = tempfile::tempdir().expect("isolated state");
    let output = isolated_command(&dir)
        .args(["ask", "--max-steps", "x", "hello"])
        .output()
        .expect("ask");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("is not a whole number"), "{stderr}");
}

#[test]
fn ask_refuses_an_unknown_experiment_key_before_any_request() {
    let (output, _dir, requests) = run_responses_fixture_command(
        Vec::new(),
        Some("--json"),
        true,
        None,
        (&["ask"], &[]),
        |_, command| {
            command.env("RUNE_EXPERIMENT", "unknown=on");
        },
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(requests.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stderr.contains("unknown experiment key") || stdout.contains("unknown experiment key"),
        "{output:?}"
    );
}

#[test]
fn ask_json_names_the_experiment_that_ran() {
    let (output, _dir, requests) = run_responses_fixture_command(
        vec![completion_response(&json!({"content": "Done."}), "stop")],
        Some("--json"),
        true,
        None,
        (&["ask"], &[]),
        |_, command| {
            command.env("RUNE_EXPERIMENT", "run_note=off");
        },
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
    assert_eq!(result["experiment"], "run_note=off");
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].to_string().contains("<run_context>"));
}

#[test]
fn rune_tool_text_changes_the_advertised_description() {
    let dir = tempfile::tempdir().expect("tempdir");
    let text = dir.path().join("tool-text.json");
    std::fs::write(&text, r#"{"shell":{"description":"Run a command."}}"#).expect("tool text");
    let output = Command::new(env!("CARGO_BIN_EXE_rune"))
        .args(["prompt", "--tools"])
        .current_dir(dir.path())
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("RUNE_TOOL_TEXT", &text)
        .output()
        .expect("run binary");
    assert!(output.status.success(), "{output:?}");
    let shell = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|spec| spec["name"] == "shell")
        .expect("shell advertised");
    assert_eq!(shell["description"], "Run a command.");
}

#[cfg(unix)]
#[test]
fn ask_counts_files_changed_by_commands() {
    let arguments = json!({"action": "run", "command": "echo x > made.txt"}).to_string();
    let (output, dir, _) = run_responses_fixture_command(
        vec![
            completion_response(&call_delta("shell", &arguments), "tool_calls"),
            completion_response(&json!({"content": "Made the file."}), "stop"),
        ],
        Some("--json"),
        true,
        Some(3),
        (&["--permission-mode", "full-access", "ask"], &[]),
        |dir, command| {
            command.with_external_programs();
            std::fs::write(dir.path().join(".gitignore"), "state/\nconfig/\ndata/\n")
                .expect("ignore file");
            let git = |arguments: &[&str]| {
                let status = Command::new("git")
                    .args([
                        "-c",
                        "user.name=Fixture",
                        "-c",
                        "user.email=fixture@example.com",
                    ])
                    .args(arguments)
                    .current_dir(dir.path())
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .expect("fixture git");
                assert!(status.success(), "git {arguments:?}");
            };
            git(&["init", "-q"]);
            git(&["add", "."]);
            git(&["commit", "-q", "-m", "fixture"]);
        },
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("made.txt")).expect("made by the command"),
        "x\n"
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
    assert_eq!(result["changed_files"], 1, "{result}");
    assert_eq!(result["tests_modified"], false, "{result}");
}
