//! R-188: reconcile the real CLI's usage export with the authoritative ledger.
#![allow(clippy::expect_used)]

mod support;
use support::CommandExt as _;

use std::process::{Command, Output};

use camino::Utf8PathBuf;
use rune_core::paths::{Paths, write_private};
use rune_session::report::{Period, summarize};
use rune_session::usage::{HelperKind, Ledger, UsageRecord, now_ms};

struct Fixture {
    _temp: tempfile::TempDir,
    root: Utf8PathBuf,
    paths: Paths,
    ledger: Ledger,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("root"))
            .expect("UTF-8 root");
        let paths = Paths {
            state_root: root.join("state"),
            config_root: root.join("config/rune"),
            data_root: root.join("data/rune"),
        };
        let ledger = Ledger::from_paths(&paths);
        Self {
            _temp: temp,
            root,
            paths,
            ledger,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rune"))
            .isolated_env()
            .env("HOME", &self.root)
            .env("RUNE_HOME", &self.paths.state_root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("ANTHROPIC_API_KEY", "environment-secret")
            .current_dir(&self.root)
            .args(args)
            .output()
            .expect("CLI process")
    }
}

fn decode(bytes: &[u8]) -> Vec<UsageRecord> {
    std::str::from_utf8(bytes)
        .expect("UTF-8 export")
        .lines()
        .map(|line| UsageRecord::decode(line).expect("versioned usage record"))
        .collect()
}

#[test]
fn export_reconciles_all_dimensions_and_preserves_missing_values_and_zeroes() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut main = UsageRecord::new(now - 3_000, "model café\n😀", HelperKind::Main);
    main.input_tokens = Some(100);
    main.output_tokens = Some(25);
    main.cache_read_tokens = Some(0);
    main.cache_write_tokens = Some(40);
    main.reasoning_tokens = Some(5);
    main.total_cost = Some("0.001230000000000000001".into());
    main.request_count = 3;
    let mut review = UsageRecord::new(now - 2_000, "review-model", HelperKind::PermissionReview);
    review.input_tokens = Some(12);
    review.output_tokens = Some(0);
    review.request_count = 2;
    let mut vision = UsageRecord::new(now - 1_000, "review-model", HelperKind::Vision);
    vision.output_tokens = Some(7);
    vision.cache_write_tokens = Some(0);
    vision.reasoning_tokens = Some(0);
    vision.total_cost = Some("0".into());
    vision.request_count = 3;
    let unknown = UsageRecord::new(now - 500, "model café\n😀", HelperKind::Main);
    for record in [&main, &review, &vision, &unknown] {
        fixture.ledger.append(record).expect("append fixture");
    }
    let original = fixture.ledger.read().expect("ledger snapshot");
    let before = std::fs::read(fixture.ledger.path()).expect("ledger bytes");
    let first = fixture.run(&["usage", "export"]);
    for args in [
        vec!["usage", "export"],
        vec!["usage", "export", "--json"],
        vec!["cost", "export"],
    ] {
        let output = fixture.run(&args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(output.stdout, first.stdout);
    }
    let exported = decode(&first.stdout);
    assert_eq!(exported, original.records);
    let expected = summarize(&original.records, Period::Last30Days, now);
    let actual = summarize(&exported, Period::Last30Days, now);
    assert_eq!(actual, expected);
    assert_eq!(actual.requests, 9);
    assert_eq!(actual.by_helper[&HelperKind::Main].requests, 4);
    assert_eq!(actual.by_helper[&HelperKind::PermissionReview].requests, 2);
    assert_eq!(actual.by_helper[&HelperKind::Vision].requests, 3);
    assert_eq!(
        actual.by_helper[&HelperKind::Vision].tokens.input_tokens,
        None
    );
    assert_eq!(actual.by_model["review-model"].requests, 5);
    let lines: Vec<serde_json::Value> = std::str::from_utf8(&first.stdout)
        .expect("UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON"))
        .collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["schema_version"], 1);
    assert_eq!(lines[0]["helper"], "main");
    assert_eq!(lines[1]["helper"], "permission_review");
    assert_eq!(lines[2]["helper"], "vision");
    assert_eq!(lines[0]["total_cost"], "0.001230000000000000001");
    assert_eq!(lines[0]["cache_read_tokens"], 0);
    assert_eq!(lines[1]["output_tokens"], 0);
    assert!(lines[1].get("total_cost").is_none());
    assert!(lines[2].get("input_tokens").is_none());
    assert_eq!(lines[3].as_object().expect("object").len(), 5);
    assert!(first.stdout.ends_with(b"\n"));
    assert_eq!(
        std::fs::read(fixture.ledger.path()).expect("ledger after"),
        before
    );
}

#[test]
fn explicit_period_filters_records_while_default_exports_all_retained_usage() {
    let fixture = Fixture::new();
    let now = now_ms();
    let day = 24 * 60 * 60 * 1_000;
    // Include records beyond the reporting windows and a future timestamp.
    let records: Vec<_> = [31 * day, 12 * day, 3 * day, day / 2, -day]
        .into_iter()
        .map(|age| UsageRecord::new(now - age, "fixture-model", HelperKind::Vision))
        .collect();
    for record in &records {
        fixture.ledger.append(record).expect("fixture append");
    }
    let all = fixture.run(&["usage", "export"]);
    assert!(all.status.success(), "{all:?}");
    assert_eq!(decode(&all.stdout), records);
    for (period, start) in [("24h", 3), ("7d", 2), ("30d", 1)] {
        let export = fixture.run(&["usage", "export", "--period", period]);
        assert!(export.status.success(), "{export:?}");
        assert!(export.stderr.is_empty(), "{export:?}");
        let selected = decode(&export.stdout);
        assert_eq!(selected, records[start..4]);
        let report = fixture.run(&["usage", "--period", period, "--json"]);
        assert!(report.status.success(), "{report:?}");
        let summary: serde_json::Value = serde_json::from_slice(&report.stdout).expect("summary");
        assert_eq!(summary["requests"], selected.len());
    }
}

#[test]
fn empty_export_writes_nothing_and_does_not_create_a_ledger_or_lock() {
    let fixture = Fixture::new();
    let output = fixture.run(&["usage", "export"]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(!fixture.ledger.path().exists());
    assert!(!fixture.ledger.lock_path().exists());
}

#[test]
fn torn_tail_warns_and_unknown_fields_are_excluded_without_rewriting_the_ledger() {
    let fixture = Fixture::new();
    let record = UsageRecord::new(now_ms() - 1_000, "fixture-model", HelperKind::Main);
    let mut value: serde_json::Value =
        serde_json::from_str(&record.encode().expect("encode")).expect("record JSON");
    value["credentials"] = serde_json::json!({"api_key": "stored-secret"});
    let raw = format!("{value}\n{{\"schema_version\":");
    write_private(fixture.ledger.path(), &raw).expect("torn ledger");
    let output = fixture.run(&["usage", "export"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(decode(&output.stdout), vec![record]);
    let text = std::str::from_utf8(&output.stdout).expect("UTF-8");
    assert!(!text.contains("secret"));
    assert!(!text.contains("credentials"));
    assert!(
        std::str::from_utf8(&output.stderr)
            .expect("stderr")
            .contains("omitted incomplete usage ledger tail")
    );
    assert_eq!(
        std::fs::read(fixture.ledger.path()).expect("ledger after"),
        raw.as_bytes()
    );
    assert!(!fixture.ledger.lock_path().exists());
}

#[test]
fn corrupt_ledgers_and_invalid_export_arguments_fail_without_partial_stdout() {
    let fixture = Fixture::new();
    let valid = UsageRecord::new(now_ms() - 1_000, "fixture-model", HelperKind::Main)
        .encode()
        .expect("encode");
    for damaged in [
        "not JSON".to_owned(),
        valid.replace("\"schema_version\":1", "\"schema_version\":99"),
        valid.replace("\"helper\":\"main\"", "\"helper\":\"unknown\""),
    ] {
        write_private(fixture.ledger.path(), &format!("{valid}\n{damaged}\n"))
            .expect("corrupt ledger");
        let output = fixture.run(&["usage", "export"]);
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(!output.stderr.is_empty(), "{output:?}");
    }
    write_private(fixture.ledger.path(), &format!("{valid}\n")).expect("valid ledger");
    for args in [
        vec!["usage", "export", "--period", "99y"],
        vec!["usage", "export", "7d"],
        vec!["usage", "export", "extra", "--period", "7d"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(!output.stderr.is_empty(), "{output:?}");
    }
}
