//! Whole `rune ask` runs against a scripted provider.
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rune_testkit::scenario::{self, Expect, Scenario};
use rune_testkit::scripted::{ScriptedCall, Turn};

const SCENARIOS: &[&str] = &[
    "fix_one_file",
    "three_test_runs",
    "repeat_read",
    "unknown_tool",
    "malformed_arguments",
    "rate_limited_then_ok",
    "cut_stream_then_ok",
    "step_cap_graceful",
    "long_150_steps",
    "provider_fails_at_step_4",
    "same_failure_five_times",
    "ask_compacts",
    "context_overflow",
    "time_limit",
    "spend_limit",
    "max_cost_without_prices",
    "parallel_reads",
    "env_passthrough",
    "overwrite_guard",
];

/// Scenarios built in Rust, which have no file.
const BUILT_IN: &[&str] = &["long_150_steps"];

fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios")
}

fn run_scenario(name: &str) -> scenario::Report {
    let path = directory().join(format!("{name}.json"));
    let loaded = scenario::load(&path).unwrap_or_else(|err| panic!("{err}"));
    let report = scenario::run(Path::new(env!("CARGO_BIN_EXE_rune")), &loaded);
    let failures = scenario::check(&loaded, &report);
    assert!(
        failures.is_empty(),
        "scenario {name} failed:\n{}\nworkspace: {}\nstdout:\n{}\nstderr:\n{}",
        failures.join("\n"),
        report.workspace_dir,
        report.stdout,
        report.stderr
    );
    report
}

#[test]
fn fix_one_file() {
    run_scenario("fix_one_file");
}

#[test]
fn three_test_runs() {
    run_scenario("three_test_runs");
}

#[test]
fn repeat_read() {
    run_scenario("repeat_read");
}

#[test]
fn overwrite_guard() {
    run_scenario("overwrite_guard");
}

#[test]
fn unknown_tool() {
    run_scenario("unknown_tool");
}

#[test]
fn malformed_arguments() {
    run_scenario("malformed_arguments");
}

#[test]
fn rate_limited_then_ok() {
    run_scenario("rate_limited_then_ok");
}

#[test]
fn cut_stream_then_ok() {
    run_scenario("cut_stream_then_ok");
}

#[test]
fn step_cap_graceful() {
    run_scenario("step_cap_graceful");
}

#[test]
fn provider_fails_at_step_4() {
    let report = run_scenario("provider_fails_at_step_4");
    let result = report.result.expect("a result");
    assert_eq!(result["steps"], 3, "{result}");
    assert!(
        result["usage"]["input_tokens"].as_u64().unwrap_or(0) > 0,
        "{result}"
    );
    assert_eq!(
        result["tool_calls"].as_array().map(Vec::len),
        Some(3),
        "{result}"
    );
}

#[test]
fn same_failure_five_times() {
    // Two failures, two refusals, then the call that stalls the run.
    let report = run_scenario("same_failure_five_times");
    let result = report.result.expect("a result");
    assert_eq!(
        result["tool_calls"].as_array().map(Vec::len),
        Some(5),
        "{result}"
    );
    let statuses: Vec<&str> = result["narration"]
        .as_array()
        .expect("narration")
        .iter()
        .filter_map(|entry| entry["status"].as_str())
        .collect();
    assert_eq!(
        statuses,
        ["error", "error", "not_run", "not_run", "not_run"],
        "{result}"
    );
    assert!(
        result["usage"]["input_tokens"].as_u64().unwrap_or(0) > 0,
        "{result}"
    );
}

#[test]
fn ask_compacts() {
    // Five reads, the summary request, the sixth read, the answer. The
    // summary is billed, so the reported input counts every request.
    let report = run_scenario("ask_compacts");
    let billed: u64 = report
        .requests
        .iter()
        .map(|request| request.prompt_tokens)
        .sum();
    let result = report.result.expect("a result");
    assert_eq!(result["usage"]["input_tokens"], billed, "{result}");
}

#[test]
fn context_overflow() {
    run_scenario("context_overflow");
}

#[test]
fn time_limit() {
    // The second reply would take 20 s; the run stops at 2 s with what it did.
    let started = std::time::Instant::now();
    let report = run_scenario("time_limit");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(8),
        "took {:?}",
        started.elapsed()
    );
    let result = report.result.expect("a result");
    assert_eq!(result["steps"], 1, "{result}");
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|error| error.contains("time limit of 2 seconds")),
        "{result}"
    );
    assert!(
        result["usage"]["input_tokens"].as_u64().unwrap_or(0) > 0,
        "{result}"
    );
}

#[test]
fn spend_limit() {
    // At $100 per million tokens the first request alone passes $0.10.
    let report = run_scenario("spend_limit");
    let result = report.result.expect("a result");
    assert_eq!(result["steps"], 1, "{result}");
    assert!(result["cost_usd"].is_string(), "{result}");
}

#[test]
fn max_cost_without_prices() {
    let report = run_scenario("max_cost_without_prices");
    assert!(
        report.stdout.contains("needs prices for") || report.stderr.contains("needs prices for"),
        "stdout: {}\nstderr: {}",
        report.stdout,
        report.stderr
    );
}

/// Runs a scenario file with `--jsonl` and returns the parsed records.
fn jsonl_records(name: &str) -> Vec<serde_json::Value> {
    let path = directory().join(format!("{name}.json"));
    let mut loaded = scenario::load(&path).unwrap_or_else(|err| panic!("{err}"));
    loaded.ask_args = vec!["--jsonl".to_owned(), "--no-save".to_owned()];
    let report = scenario::run(Path::new(env!("CARGO_BIN_EXE_rune")), &loaded);
    let failures = scenario::check(&loaded, &report);
    assert!(
        failures.is_empty(),
        "{name} with --jsonl failed:\n{}\nstderr:\n{}",
        failures.join("\n"),
        report.stderr
    );
    report
        .stdout
        .lines()
        .map(|line| {
            assert!(line.len() < 1024 * 1024, "a record over 1 MiB");
            serde_json::from_str(line).unwrap_or_else(|err| panic!("{err}: {line}"))
        })
        .collect()
}

#[test]
fn fix_one_file_jsonl_records() {
    let records = jsonl_records("fix_one_file");
    let kind = |record: &serde_json::Value| record["type"].as_str().unwrap_or("").to_owned();
    assert_eq!(kind(&records[0]), "start");
    assert_eq!(records[0]["schema"], 2);
    assert_eq!(kind(&records[1]), "prompt");
    assert_eq!(kind(records.last().expect("records")), "final");

    // Steps open and close in order, and every call has one result.
    let mut open_step = None;
    let mut calls = BTreeMap::new();
    let mut input = 0;
    for record in &records[2..records.len() - 1] {
        match kind(record).as_str() {
            "step_start" => {
                assert!(open_step.is_none(), "a step started inside another");
                open_step = record["step"].as_u64();
                assert!(record["ts"].as_str().is_some_and(|ts| ts.ends_with('Z')));
            }
            "step_finish" => {
                assert_eq!(open_step, record["step"].as_u64(), "{record}");
                open_step = None;
                input += record["usage"]["input_tokens"].as_u64().expect("input");
            }
            "tool_call" => {
                assert!(calls.insert(record["id"].to_string(), false).is_none());
            }
            "tool_result" => {
                let seen = calls
                    .get_mut(&record["id"].to_string())
                    .expect("a call before its result");
                assert!(!*seen, "two results for one call");
                *seen = true;
            }
            "text_delta" | "reasoning" => {}
            other => panic!("unexpected record {other}: {record}"),
        }
    }
    assert!(open_step.is_none());
    assert!(!calls.is_empty());
    assert!(calls.values().all(|seen| *seen), "{calls:?}");
    let result = &records.last().expect("final")["result"];
    assert_eq!(result["usage"]["input_tokens"], input);
}

#[test]
fn rate_limited_then_ok_jsonl_reports_the_retry() {
    let records = jsonl_records("rate_limited_then_ok");
    let retries: Vec<_> = records
        .iter()
        .filter(|record| record["type"] == "retry")
        .collect();
    assert_eq!(retries.len(), 1, "{records:?}");
    assert_eq!(retries[0]["kind"], "rate_limited");
    assert_eq!(retries[0]["attempt"], 2);
}

/// The order of `tool_call` and `tool_result` records in a run of
/// `parallel_reads` with these global flags.
fn tool_record_order(global_args: &[&str]) -> Vec<String> {
    let path = directory().join("parallel_reads.json");
    let mut loaded = scenario::load(&path).unwrap_or_else(|err| panic!("{err}"));
    loaded.global_args = global_args.iter().map(|arg| (*arg).to_owned()).collect();
    let report = scenario::run(Path::new(env!("CARGO_BIN_EXE_rune")), &loaded);
    let failures = scenario::check(&loaded, &report);
    assert!(
        failures.is_empty(),
        "{}\n{}",
        failures.join("\n"),
        report.stderr
    );
    report
        .stdout
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a record"))
        .filter_map(|record| record["type"].as_str().map(str::to_owned))
        .filter(|kind| kind == "tool_call" || kind == "tool_result")
        .collect()
}

#[test]
fn parallel_reads() {
    // Every read starts before the first one is answered.
    let order = tool_record_order(&[]);
    assert_eq!(
        order,
        [
            "tool_call",
            "tool_call",
            "tool_call",
            "tool_call",
            "tool_result",
            "tool_result",
            "tool_result",
            "tool_result"
        ],
        "{order:?}"
    );
}

#[test]
fn parallel_reads_run_in_order_at_a_limit_of_one() {
    let order = tool_record_order(&["--limit", "parallel_tool_calls=1"]);
    assert_eq!(
        order,
        [
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result"
        ],
        "{order:?}"
    );
}

#[test]
fn run_context_is_not_in_the_system_message() {
    let path = directory().join("fix_one_file.json");
    let loaded = scenario::load(&path).unwrap_or_else(|err| panic!("{err}"));
    let report = scenario::run(Path::new(env!("CARGO_BIN_EXE_rune")), &loaded);
    let first: serde_json::Value =
        serde_json::from_str(&report.requests[0].body).expect("request 1 is JSON");
    let messages = first["messages"].as_array().expect("messages");
    let system = messages
        .iter()
        .find(|message| message["role"] == "system")
        .expect("a system message");
    assert!(!system.to_string().contains("run_context"), "{system}");
    let user = messages
        .iter()
        .find(|message| message["role"] == "user")
        .expect("a user message");
    assert!(user.to_string().contains("<run_context>"), "{user}");
}

/// Starts a run whose first step leaves a background command running and
/// whose second request never answers, signals it once the second request
/// is pending, and returns the exit code, stdout and the command's marker.
#[cfg(unix)]
fn signalled_run(signal: &str) -> (Option<i32>, String, String) {
    use rune_testkit::scripted::{ScriptedProvider, Turn};
    let marker = format!("600.{}{}", std::process::id(), signal.len());
    let mut shell = Turn::call(
        "s1",
        "shell",
        &serde_json::json!({
            "action": "run",
            "command": format!("sleep {marker} & wait"),
            "yield_time_ms": 200,
        })
        .to_string(),
    );
    shell.finish = Some("tool_calls".to_owned());
    let mut never = Turn::text("never");
    never.delay_ms = 30_000;
    let provider = ScriptedProvider::start(vec![shell, never]);
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rune"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("RUNE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("RUNE_PROVIDER", "chat_completions")
        .env("RUNE_BASE_URL", provider.base_url())
        .env("RUNE_MODEL", "scripted")
        .env("RUNE_API_KEY_ENV", "RUNE_SIGNAL_TEST_KEY")
        .env("RUNE_SIGNAL_TEST_KEY", "signal-test-key")
        .current_dir(&workspace)
        .args([
            "--permission-mode",
            "full-access",
            "--allow-unsandboxed",
            "ask",
            "--json",
            "--no-save",
            "--",
            "Start the command, then wait.",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("ask");
    let started = std::time::Instant::now();
    while provider.request_count() < 2 {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "no second request"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let status = std::process::Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(status.success());
    let signalled = std::time::Instant::now();
    let code = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status.code();
        }
        assert!(
            signalled.elapsed() < std::time::Duration::from_secs(10),
            "the run did not stop"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let output = child.wait_with_output().expect("output");
    (
        code,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        marker,
    )
}

/// Asserts that no process runs `sleep <marker>`.
#[cfg(unix)]
fn assert_no_survivor(marker: &str) {
    std::thread::sleep(std::time::Duration::from_secs(3));
    let found = std::process::Command::new("pgrep")
        .args(["-f", &format!("sleep {marker}")])
        .output()
        .expect("pgrep");
    assert_eq!(
        found.status.code(),
        Some(1),
        "a command survived: {}",
        String::from_utf8_lossy(&found.stdout)
    );
}

#[cfg(unix)]
#[test]
fn sigterm_stops_background_commands() {
    let (code, stdout, marker) = signalled_run("-TERM");
    assert_eq!(code, Some(143), "{stdout}");
    let result: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON object");
    assert_eq!(result["error_code"], "cancelled", "{result}");
    assert_eq!(result["steps"], 1, "{result}");
    assert_eq!(result["stop_reason"], "cancelled", "{result}");
    assert_eq!(result["exit_code"], 143, "{result}");
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|error| error.contains("SIGTERM")),
        "{result}"
    );
    assert_no_survivor(&marker);
}

#[cfg(unix)]
#[test]
fn sigint_exits_130() {
    let (code, stdout, marker) = signalled_run("-INT");
    assert_eq!(code, Some(130), "{stdout}");
    let result: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON object");
    assert_eq!(result["error_code"], "cancelled", "{result}");
    assert_eq!(result["stop_reason"], "cancelled", "{result}");
    assert_eq!(result["exit_code"], 130, "{result}");
    assert_no_survivor(&marker);
}

#[test]
fn env_passthrough() {
    run_scenario("env_passthrough");
}

#[test]
fn long_150_steps_completes() {
    let workspace = BTreeMap::from([(
        "a.txt".to_owned(),
        (1..=200)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )]);
    let mut turns: Vec<Turn> = (1..=150)
        .map(|n| Turn {
            tool_calls: vec![ScriptedCall {
                id: format!("call_{n}"),
                name: "read_file".to_owned(),
                arguments: serde_json::json!({"path": "a.txt", "start_line": n, "line_count": 1}),
            }],
            ..Turn::default()
        })
        .collect();
    turns.push(Turn {
        text: Some("Read every line.".to_owned()),
        ..Turn::default()
    });
    let long = Scenario {
        name: "long_150_steps".to_owned(),
        prompt: "Read a.txt one line at a time.".to_owned(),
        workspace,
        global_args: Vec::new(),
        ask_args: vec!["--json".to_owned(), "--no-save".to_owned()],
        env: BTreeMap::new(),
        user_config: None,
        timeout_s: 120,
        turns,
        expect: Expect {
            exit_code: Some(0),
            stop_reason: Some("end_turn".to_owned()),
            min_steps: Some(151),
            requests: Some(151),
            ..Expect::default()
        },
    };
    let report = scenario::run(Path::new(env!("CARGO_BIN_EXE_rune")), &long);
    let failures = scenario::check(&long, &report);
    assert!(
        failures.is_empty(),
        "long_150_steps failed:\n{}\nstderr:\n{}",
        failures.join("\n"),
        report.stderr
    );
    let result = report.result.expect("a result");
    assert_eq!(result["steps"], 151);
}

#[test]
fn every_scenario_file_has_a_test() {
    let mut found: Vec<String> = std::fs::read_dir(directory())
        .expect("the scenarios directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    found.sort();
    let mut listed: Vec<String> = SCENARIOS
        .iter()
        .filter(|name| !BUILT_IN.contains(name))
        .map(|s| (*s).to_owned())
        .collect();
    listed.sort();
    assert_eq!(found, listed);
}
