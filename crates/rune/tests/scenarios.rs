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
    "unknown_window_no_compaction",
    "cut_text_continues",
    "cut_tool_call",
    "cut_three_times",
    "cut_empty_then_text",
    "failed_check_note",
    "verified_after_fix",
    "cut_mid_frame_then_ok",
    "prune_long_run",
    "overflow_ladder",
    "question_nudge",
    "question_nudge_off",
    "deepseek_thinking_tools",
    "kimi_thinking_tools",
    "glm_thinking_tools",
    "stall_same_failure",
    "stall_same_file",
    "checkpoint_same_prefix",
    "nested_rules",
    "goal_judge_continue",
    "goal_judge_same_reason",
    "goal_without_judge",
    "goal_experiment_judge",
    "goal_judge_spend",
    "context_report",
];

/// Scenarios built in Rust, which have no file.
const BUILT_IN: &[&str] = &["long_150_steps", "prune_long_run", "checkpoint_same_prefix"];

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
fn context_report() {
    let report = run_scenario("context_report");
    let result = report.result.expect("a result");
    let context = &result["context"];
    assert!(
        context["tool_results"]["read_file"].as_u64() > Some(0),
        "{result}"
    );
    let parts = context["parts"].as_object().expect("parts");
    assert_eq!(parts.len(), 11, "{context}");
    let sum: u64 = parts.values().filter_map(serde_json::Value::as_u64).sum();
    assert_eq!(Some(sum), context["total_tokens"].as_u64(), "{context}");
    assert_eq!(context["anchored"], true, "{context}");
    assert!(parts["base_prompt"].as_u64() > Some(0), "{context}");
    assert!(parts["tool_schemas"].as_u64() > Some(0), "{context}");
    // The key follows the cost on the wire.
    let keys: Vec<&String> = result.as_object().expect("object").keys().collect();
    let position = |key: &str| keys.iter().position(|name| *name == key);
    assert!(position("context") > position("narration"), "{result}");
}

#[test]
fn the_tool_list_is_byte_identical_across_runs() {
    let tools = |report: &scenario::Report| -> Vec<String> {
        report
            .requests
            .iter()
            .map(|request| {
                let body: serde_json::Value =
                    serde_json::from_str(&request.body).expect("request JSON");
                body["tools"].to_string()
            })
            .collect()
    };
    let first = tools(&run_scenario("fix_one_file"));
    let second = tools(&run_scenario("fix_one_file"));
    assert_eq!(first.len(), 4);
    assert!(first.iter().all(|list| *list == first[0]));
    assert_eq!(first, second);
}

#[test]
fn nested_rules() {
    let report = run_scenario("nested_rules");
    let third = &report.requests[2].body;
    assert_eq!(third.matches("Use tabs in a.").count(), 1, "{third}");
}

#[test]
fn deepseek_thinking_tools() {
    run_scenario("deepseek_thinking_tools");
}

#[test]
fn kimi_thinking_tools() {
    run_scenario("kimi_thinking_tools");
}

#[test]
fn glm_thinking_tools() {
    run_scenario("glm_thinking_tools");
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
fn cut_text_continues() {
    run_scenario("cut_text_continues");
}

#[test]
fn cut_tool_call() {
    run_scenario("cut_tool_call");
}

#[test]
fn cut_three_times() {
    run_scenario("cut_three_times");
}

#[test]
fn cut_empty_then_text() {
    run_scenario("cut_empty_then_text");
}

#[test]
fn failed_check_note() {
    // The model is told once that its last check failed, and the run reports it.
    let report = run_scenario("failed_check_note");
    let result = report.result.expect("a result");
    assert_eq!(result["verification"], "failed", "{result}");
    assert_eq!(result["tests_modified"], false, "{result}");
    assert_eq!(result["changed_files"], 1, "{result}");
}

#[test]
fn stall_same_failure() {
    // The same failure after a change asks once for competing explanations.
    let report = run_scenario("stall_same_failure");
    let result = report.result.expect("a result");
    assert_eq!(result["stall_notes"], 1, "{result}");
    assert_eq!(result["verification"], "failed", "{result}");
}

#[test]
fn stall_same_file() {
    // A fourth change to one file while the check fails asks once.
    let report = run_scenario("stall_same_file");
    let result = report.result.expect("a result");
    assert_eq!(result["stall_notes"], 1, "{result}");
}

#[test]
fn goal_judge_continue() {
    let report = run_scenario("goal_judge_continue");
    let result = report.result.as_ref().expect("a result");
    assert_eq!(result["judge"]["rounds"], 2, "{result}");
    assert_eq!(result["judge"]["status"], "done", "{result}");
    assert_eq!(result["judge"]["reason"], "the check passed.", "{result}");
    assert_eq!(result["steps"], 3, "{result}");

    // The judge request extends the conversation it was asked about, so the
    // cached prefix holds and the tools stay listed.
    let body = |index: usize| -> serde_json::Value {
        serde_json::from_str(&report.requests[index].body).expect("a JSON body")
    };
    let (asked, judged) = (body(0), body(1));
    let asked_messages = asked["messages"].as_array().expect("messages");
    let judged_messages = judged["messages"].as_array().expect("messages");
    assert_eq!(judged_messages[..asked_messages.len()], asked_messages[..]);
    assert_eq!(judged["tools"], asked["tools"]);
    assert_eq!(judged_messages.len(), asked_messages.len() + 2);
}

#[test]
fn goal_judge_same_reason() {
    // The third identical reason settles at once, without another note.
    let report = run_scenario("goal_judge_same_reason");
    let result = report.result.expect("a result");
    assert_eq!(result["judge"]["rounds"], 3, "{result}");
    assert_eq!(result["judge"]["status"], "continue", "{result}");
}

#[test]
fn goal_judge_spend() {
    // Only the judge reply is long enough to reach the limit.
    let report = run_scenario("goal_judge_spend");
    let result = report.result.expect("a result");
    assert_eq!(result["judge"]["status"], "continue", "{result}");
    assert_eq!(result["steps"], 1, "{result}");
}

#[test]
fn goal_without_judge() {
    let report = run_scenario("goal_without_judge");
    let result = report.result.expect("a result");
    assert!(result.get("judge").is_none(), "{result}");
}

#[test]
fn goal_experiment_judge() {
    let report = run_scenario("goal_experiment_judge");
    let result = report.result.expect("a result");
    assert_eq!(result["experiment"], "goal=judge", "{result}");
    assert_eq!(result["judge"]["rounds"], 1, "{result}");
    assert_eq!(result["judge"]["status"], "done", "{result}");
    assert!(result["judge"]["input_tokens"].is_u64(), "{result}");
}

#[test]
fn verified_after_fix() {
    let report = run_scenario("verified_after_fix");
    let result = report.result.expect("a result");
    assert_eq!(result["verification"], "verified", "{result}");
    assert_eq!(result["tests_modified"], false, "{result}");
}

#[test]
fn cut_mid_frame_then_ok() {
    run_scenario("cut_mid_frame_then_ok");
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
fn unknown_window_no_compaction() {
    // The model is in no catalog and nothing declares its window, so the
    // low trigger has nothing to measure against and every request is sent
    // as it is.
    run_scenario("unknown_window_no_compaction");
}

#[test]
fn context_overflow() {
    run_scenario("context_overflow");
}

#[test]
fn question_nudge() {
    // A headless reply that ends on a question is told once to decide.
    run_scenario("question_nudge");
}

#[test]
fn question_nudge_off() {
    run_scenario("question_nudge_off");
}

#[test]
fn overflow_ladder() {
    // Two rejections in a row: each compacts toward a smaller target and the
    // step is sent again.
    run_scenario("overflow_ladder");
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
        provider: None,
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
fn prune_long_run() {
    // Sixty reads of 8,000 bytes pass the ceiling of a 128,000 token window.
    // Old results become receipts, which is enough, so no summary is asked for.
    let line = format!("{}\n", "x".repeat(79));
    let workspace = (1..=60)
        .map(|n| (format!("f{n:02}.txt"), line.repeat(100)))
        .collect::<BTreeMap<_, _>>();
    let mut turns: Vec<Turn> = (1..=60)
        .map(|n| Turn {
            tool_calls: vec![ScriptedCall {
                id: format!("call_{n}"),
                name: "read_file".to_owned(),
                arguments: serde_json::json!({"path": format!("f{n:02}.txt")}),
            }],
            ..Turn::default()
        })
        .collect();
    turns.push(Turn {
        text: Some("Read them all.".to_owned()),
        ..Turn::default()
    });
    let long = Scenario {
        name: "prune_long_run".to_owned(),
        prompt: "Read f01.txt to f60.txt, one at a time.".to_owned(),
        workspace,
        global_args: Vec::new(),
        ask_args: vec!["--json".to_owned(), "--no-save".to_owned()],
        env: BTreeMap::from([("RUNE_CONTEXT_WINDOW".to_owned(), "128000".to_owned())]),
        provider: None,
        user_config: None,
        timeout_s: 120,
        turns,
        expect: Expect {
            exit_code: Some(0),
            requests: Some(61),
            max_request_tokens: Some(108_800),
            request_contains: vec![scenario::RequestContains {
                request: 61,
                text: "removed to save context".to_owned(),
            }],
            ..Expect::default()
        },
    };
    let report = scenario::run(Path::new(env!("CARGO_BIN_EXE_rune")), &long);
    let failures = scenario::check(&long, &report);
    assert!(
        failures.is_empty(),
        "prune_long_run failed:\n{}\nstderr:\n{}",
        failures.join("\n"),
        report.stderr
    );
}

#[test]
fn checkpoint_same_prefix() {
    // A ceiling of 6,400 tokens (10 percent of 64,000) with nothing kept
    // verbatim. Five reads of 3,000 bytes put request 6 over it, so request 6
    // is the summarizer's: the conversation so far plus one instruction.
    let file = format!(
        "{}{}\n",
        format!("{}\n", "x".repeat(79)).repeat(37),
        "y".repeat(39)
    );
    let names = ["a", "b", "c", "d", "e"];
    let workspace = names
        .iter()
        .map(|name| (format!("{name}.txt"), file.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut turns: Vec<Turn> = names
        .iter()
        .enumerate()
        .map(|(n, name)| Turn {
            tool_calls: vec![ScriptedCall {
                id: format!("call_{}", n + 1),
                name: "read_file".to_owned(),
                arguments: serde_json::json!({"path": format!("{name}.txt")}),
            }],
            ..Turn::default()
        })
        .collect();
    let checkpoint = [
        "## Goal",
        "## Constraints",
        "## Decisions",
        "## State",
        "### Done",
        "### In progress",
        "### Blocked",
        "## Next steps",
        "## Files that matter",
        "## Facts to keep",
    ]
    .iter()
    .map(|heading| format!("{heading}\n- read a.txt to e.txt, one at a time"))
    .collect::<Vec<_>>()
    .join("\n");
    turns.push(Turn {
        text: Some(checkpoint),
        match_text: Some("Write a checkpoint of the conversation above".to_owned()),
        ..Turn::default()
    });
    turns.push(Turn {
        text: Some("Done.".to_owned()),
        ..Turn::default()
    });
    let run = Scenario {
        name: "checkpoint_same_prefix".to_owned(),
        prompt: "Read a.txt, b.txt, c.txt, d.txt and e.txt, one at a time.".to_owned(),
        workspace,
        global_args: Vec::new(),
        ask_args: vec!["--json".to_owned(), "--no-save".to_owned()],
        env: BTreeMap::from([
            ("RUNE_CONTEXT_WINDOW".to_owned(), "64000".to_owned()),
            (
                "RUNE_LIMITS".to_owned(),
                "compaction_trigger_percent=10,compaction_keep_tokens=0".to_owned(),
            ),
        ]),
        provider: None,
        user_config: None,
        timeout_s: 120,
        turns,
        expect: Expect {
            exit_code: Some(0),
            request_contains: vec![
                scenario::RequestContains {
                    request: 6,
                    text: "Write a checkpoint of the conversation above".to_owned(),
                },
                scenario::RequestContains {
                    request: 6,
                    text: "\"tool_choice\":\"none\"".to_owned(),
                },
                scenario::RequestContains {
                    request: 7,
                    text: "<conversation-checkpoint>".to_owned(),
                },
            ],
            ..Expect::default()
        },
    };
    let report = scenario::run(Path::new(env!("CARGO_BIN_EXE_rune")), &run);
    let failures = scenario::check(&run, &report);
    assert!(
        failures.is_empty(),
        "checkpoint_same_prefix failed:\n{}\nstderr:\n{}",
        failures.join("\n"),
        report.stderr
    );
    let summarizer = &report.requests[5];
    assert!(
        summarizer.cached_tokens * 2 >= summarizer.prompt_tokens,
        "the summarizer missed the cached prefix: {} of {} tokens cached",
        summarizer.cached_tokens,
        summarizer.prompt_tokens
    );
    let result = report.result.as_ref().expect("the final result object");
    let billed: u64 = report.requests.iter().map(|r| r.prompt_tokens).sum();
    assert_eq!(result["usage"]["input_tokens"], billed, "{result}");
    assert_eq!(
        result["helper_usage"]["compaction"]["input_tokens"], summarizer.prompt_tokens,
        "{result}"
    );
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
