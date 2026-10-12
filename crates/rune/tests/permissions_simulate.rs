//! R-194: replay recorded policy targets through the real CLI without writes.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Fixture {
    temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("sequence.json"),
            include_str!("fixtures/policy_sequence.json"),
        )
        .expect("recording");
        std::fs::write(temp.path().join("input.txt"), "preserve input").expect("input");
        std::fs::write(temp.path().join("config.toml"), "").expect("config");
        Self { temp }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rune"))
            .args(args)
            .env_clear()
            .env("HOME", self.temp.path())
            .env("RUNE_HOME", self.temp.path().join("state"))
            .env("RUNE_CONFIG", self.temp.path().join("config.toml"))
            .env("XDG_CONFIG_HOME", self.temp.path().join("config"))
            .env("XDG_DATA_HOME", self.temp.path().join("data"))
            .current_dir(self.temp.path())
            .stdin(Stdio::null())
            .output()
            .expect("run simulator")
    }

    fn snapshot(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
            for entry in std::fs::read_dir(dir).expect("read directory") {
                let path = entry.expect("entry").path();
                let name = path.strip_prefix(root).expect("relative").to_owned();
                if path.is_dir() {
                    out.insert(name, None);
                    visit(root, &path, out);
                } else {
                    out.insert(name, Some(std::fs::read(path).expect("read file")));
                }
            }
        }
        let mut out = BTreeMap::new();
        visit(self.temp.path(), self.temp.path(), &mut out);
        out
    }
}

#[test]
fn replay_reports_every_call_without_execution_or_approval_writes() {
    for mode in ["auto", "ask", "full-access"] {
        // Verify both absent state roots and existing approval/session files.
        for existing_state in [false, true] {
            let fixture = Fixture::new();
            if existing_state {
                let state = fixture.temp.path().join("state");
                std::fs::create_dir_all(&state).expect("state");
                std::fs::write(state.join("approvals.json"), "[]\n").expect("approvals");
                std::fs::write(state.join("events.jsonl"), "recorded history\n").expect("history");
            }
            let before = fixture.snapshot();
            let output = fixture.run(&[
                "permissions",
                "--simulate",
                "sequence.json",
                "--permission-mode",
                mode,
                "--json",
            ]);
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
            assert_eq!(report["dry_run"], true);
            assert_eq!(report["mode"], mode.replace('-', "_"));
            let calls = report["calls"].as_array().expect("calls");
            let recording: Vec<serde_json::Value> =
                serde_json::from_str(include_str!("fixtures/policy_sequence.json"))
                    .expect("fixture");
            assert_eq!(calls.len(), recording.len());
            for (position, (call, recorded)) in calls.iter().zip(&recording).enumerate() {
                assert_eq!(call["index"], position.saturating_add(1));
                assert_eq!(call["tool"], recorded["tool"]);
                assert_eq!(call["target"], recorded["target"]);
                let policy_outcome = match position {
                    4 | 5 if mode != "full-access" => "ask",
                    6 => "deny",
                    3 if mode == "ask" => "ask",
                    _ => "allow",
                };
                assert_eq!(call["decision"]["outcome"], policy_outcome);
                assert_eq!(
                    call["outcome"],
                    if mode == "full-access" {
                        "allow"
                    } else {
                        policy_outcome
                    }
                );
                assert_eq!(call["decision"]["layer"], "default");
                if matches!(position, 4 | 5) || (position == 3 && mode == "ask") {
                    assert_eq!(call["from_rule"], false);
                    assert!(
                        call["decision"]["rule"]
                            .as_str()
                            .expect("rule")
                            .contains("no rule matched")
                    );
                } else {
                    assert_eq!(call["from_rule"], true);
                    let pattern = if position == 3 { "pwd" } else { "*" };
                    assert_eq!(
                        call["decision"]["rule"],
                        format!("{} {pattern}", recorded["tool"].as_str().expect("tool"))
                    );
                    assert!(
                        call["decision"]["considered"]
                            .as_array()
                            .expect("rules")
                            .iter()
                            .any(|rule| rule["matched"] == true)
                    );
                }
            }
            assert_eq!(fixture.snapshot(), before, "simulator changed files");
        }
    }
}

#[test]
fn text_replay_reports_rules_and_an_empty_recording_is_valid() {
    let fixture = Fixture::new();
    let before = fixture.snapshot();
    let output = fixture.run(&["permissions", "--simulate=sequence.json"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).expect("text");
    assert_eq!(text.lines().count(), 9);
    assert!(text.contains(
        "1: read_file `input.txt`: allow (allow: matched `read_file *` at the default layer)"
    ));
    assert!(text.contains("7: web_fetch `https://example.invalid`: deny (deny: matched `web_fetch *` at the default layer)"));
    assert!(text.contains("5: shell `printf executed > executed.txt`: ask"));
    assert_eq!(fixture.snapshot(), before);
    std::fs::write(fixture.temp.path().join("sequence.json"), "[]").expect("empty recording");
    let output = fixture.run(&["permissions", "--simulate", "sequence.json", "--json"]);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(report["calls"], serde_json::json!([]));
    let output = fixture.run(&["permissions", "--simulate", "sequence.json"]);
    assert_eq!(
        String::from_utf8(output.stdout).expect("text"),
        "dry run: mode auto\nno calls\n"
    );
}

#[test]
fn invalid_recordings_and_conflicting_actions_fail_without_partial_output_or_writes() {
    let fixture = Fixture::new();
    for body in [
        "not JSON",
        "{}",
        r#"[{"tool":"shell"}]"#,
        r#"[{"tool":"shell","target":1}]"#,
        r#"[{"tool":"shell","target":"pwd","approval":"allow"}]"#,
        r#"[{"tool":"shell","target":"pwd"},{"tool":" ","target":""}]"#,
    ] {
        std::fs::write(fixture.temp.path().join("sequence.json"), body).expect("invalid fixture");
        let before = fixture.snapshot();
        let output = fixture.run(&["permissions", "--simulate", "sequence.json", "--json"]);
        assert!(!output.status.success(), "{body}: {output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid_field"));
        assert_eq!(fixture.snapshot(), before);
    }
    for args in [
        vec![
            "permissions",
            "--simulate",
            "sequence.json",
            "--explain",
            "shell:pwd",
        ],
        vec!["permissions", "--simulate", "sequence.json", "shell", "pwd"],
        vec!["permissions", "--simulate", "missing.json"],
        vec!["permissions", "--simulate", "."],
        vec!["permissions", "--simulate"],
    ] {
        let before = fixture.snapshot();
        let output = fixture.run(&args);
        assert!(!output.status.success(), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert_eq!(fixture.snapshot(), before);
    }
    std::fs::write(
        fixture.temp.path().join("sequence.json"),
        vec![b' '; 1024 * 1024 + 1],
    )
    .expect("oversized fixture");
    let before = fixture.snapshot();
    let output = fixture.run(&["permissions", "--simulate", "sequence.json"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("too_large"));
    assert!(output.stdout.is_empty());
    assert_eq!(fixture.snapshot(), before);
}
