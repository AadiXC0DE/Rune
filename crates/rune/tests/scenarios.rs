//! Whole `rune ask` runs against a scripted provider.
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use rune_testkit::scenario;

const SCENARIOS: &[&str] = &[
    "fix_one_file",
    "three_test_runs",
    "repeat_read",
    "unknown_tool",
    "malformed_arguments",
    "rate_limited_then_ok",
    "cut_stream_then_ok",
];

fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios")
}

fn run_scenario(name: &str) {
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
fn every_scenario_file_has_a_test() {
    let mut found: Vec<String> = std::fs::read_dir(directory())
        .expect("the scenarios directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    found.sort();
    let mut listed: Vec<String> = SCENARIOS.iter().map(|s| (*s).to_owned()).collect();
    listed.sort();
    assert_eq!(found, listed);
}
