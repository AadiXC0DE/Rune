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
