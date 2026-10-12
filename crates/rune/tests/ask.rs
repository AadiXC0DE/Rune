//! Regressions for the bounded one-shot tool loop through the real binary.

#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Serves one completion from an isolated local endpoint.
fn run_fixture(delta: &Value, finish: &str, json_output: bool) -> Output {
    run_fixture_with_options(delta, finish, json_output, false).0
}

fn isolated_command(dir: &tempfile::TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rune"));
    command
        .env_clear()
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
    )
}

fn run_response_fixture(
    response: String,
    json_output: bool,
    no_save: bool,
) -> (Output, tempfile::TempDir) {
    let (output, dir, _) = run_responses_fixture(vec![response], json_output, no_save, Some(1));
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
        setup,
    )
}

fn run_responses_fixture_command(
    responses: Vec<String>,
    output_flag: Option<&str>,
    no_save: bool,
    max_steps: Option<u64>,
    (arguments, images): (&[&str], &[&str]),
    setup: impl FnOnce(&tempfile::TempDir),
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
    setup(&dir);
    let mut command = isolated_command(&dir);
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
        let output = run_fixture(&tool_call_delta(), finish, true);
        assert_eq!(output.status.code(), Some(1), "finish: {finish}");
        let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
        assert_eq!(result["exit_code"], 1);
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
        assert!(stderr.contains("limit of 1 model steps"), "{stderr}");
    }
}

#[test]
fn ask_plain_text_step_limit_does_not_print_a_final_answer() {
    let output = run_fixture(&tool_call_delta(), "tool_calls", false);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
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
    let (output, dir) = run_fixture_with_options(&tool_call_delta(), "tool_calls", true, false);
    assert_eq!(output.status.code(), Some(1));
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
            "[limits]\nmax_agent_steps = 1\nprovider_max_attempts = {attempts}\n\
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
        Some(1),
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
            Some(3),
            (&["--permission-mode", mode, "review"], &[]),
            |dir| {
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
        assert_eq!(refusals, denied.len());
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
            Some(2),
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
                        "[limits]\nmax_agent_steps = 2\nproject_instructions_total_bytes = 512\n",
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
            assert_eq!(request["messages"][1]["content"], "run the fixture command");
        }
    }
}

#[test]
fn ask_refreshes_a_capped_repository_map_after_an_edit() {
    let edit = json!({"path": "src/main.rs", "old_string": "alpha();", "new_string": "beta(); "})
        .to_string();
    let (output, dir, requests) = run_responses_fixture_with_setup(
        vec![
            completion_response(&call_delta("edit_file", &edit), "tool_calls"),
            completion_response(&json!({"content": "Updated."}), "stop"),
        ],
        true,
        true,
        Some(2),
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
                "[limits]\nmax_agent_steps = 2\nrepository_map_bytes = 512\n",
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
        .expect("refreshed instructions");
    assert!(
        before.contains("src/lib.rs:1 fn alpha <- src/main.rs:1"),
        "{before}"
    );
    assert!(after.contains("src/lib.rs:1 fn alpha\n"), "{after}");
    assert!(
        after.contains("src/lib.rs:2 fn beta <- src/main.rs:1"),
        "{after}"
    );
    for instructions in [before, after] {
        let map = instructions
            .split("Repository symbol map")
            .nth(1)
            .expect("map section");
        assert!(map.len() + "Repository symbol map".len() <= 512);
        assert!(map.contains("map incomplete"));
        assert!(!instructions.contains("ignored_secret"));
        assert!(!instructions.contains("ignored.rs"));
    }
    let shown = isolated_command(&dir)
        .args(["prompt", "--show"])
        .output()
        .expect("prompt preview");
    assert!(shown.status.success(), "{shown:?}");
    assert_eq!(
        String::from_utf8(shown.stdout)
            .expect("UTF-8 preview")
            .trim_end(),
        after
    );
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
            Some(2),
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
            Some(2),
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
fn ask_bounds_repeated_tool_requests_with_configured_and_default_step_caps() {
    for configured_cap in [Some(2), Some(0), None] {
        let expected_steps = configured_cap.filter(|cap| *cap > 0).unwrap_or(16);
        let responses = (0..expected_steps)
            .map(|_| {
                completion_response(
                    &call_delta("read_file", r#"{"path":"fixture.txt"}"#),
                    "tool_calls",
                )
            })
            .collect();
        let (output, _, requests) = run_responses_fixture(responses, true, true, configured_cap);
        assert_eq!(output.status.code(), Some(1));
        let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
        assert_eq!(result["steps"], expected_steps);
        assert_eq!(requests.len() as u64, expected_steps);
        assert_eq!(result["error_code"], "limit_exceeded");
        assert_eq!(result["final_output"], "");
        assert_eq!(
            result["tool_calls"].as_array().expect("calls").len() as u64,
            expected_steps
        );
        assert_eq!(result["usage"]["input_tokens"], expected_steps * 11);
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
    assert_eq!(events[0], json!({"type": "start"}));
    assert_eq!(events[1], json!({"type": "step_start", "step": 1}));
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
        assert_eq!(events[0], json!({"type": "start"}));
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
    let partial = format!(
        "data: {}\n\n",
        json!({"choices": [{"delta": {"content": "Discard this attempt."}, "finish_reason": null}]})
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
        Some(1),
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
            "step_start",
            "text_delta",
            "step_restart",
            "text_delta",
            "final"
        ]
    );
    assert_eq!(events[2]["delta"], "Discard this attempt.");
    assert_eq!(events[3]["step"], 1);
    assert_eq!(events[4]["step"], 1);
    assert_eq!(events[5]["result"]["final_output"], "Recovered.");
    assert_eq!(events[5]["result"]["steps"], 1);
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
        Some(2),
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
            "step_start",
            "text_delta",
            "step_start",
            "text_delta",
            "final"
        ]
    );
    assert_eq!(events[2]["step"], 1);
    assert_eq!(events[2]["delta"], "Reading.");
    assert_eq!(events[4]["step"], 2);
    let result = &events[5]["result"];
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
        Some(2),
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
        assert_eq!(
            users[0]["content"],
            json!([
                {"type": "text", "text": "run the fixture command"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{ATTACHMENT_PNG_BASE64}")}}
            ])
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
        Some(1),
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
        run_responses_fixture_with_format(responses, Some("--jsonl"), true, Some(6), |_| {});
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
