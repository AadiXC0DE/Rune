//! Integration tests for the command-line surface.
//!
//! These run the built binary as a subprocess, so they exercise the real
//! dispatch path including exit codes and stream separation rather than calling
//! into the library.

// Integration tests assert by panicking. The guards that forbid panicking
// apply to shipped code, where a panic on user input is a defect.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::process::Command;

/// Path of the binary under test.
fn binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test exe path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("rune")
}

/// Runs the binary with an isolated state directory.
fn run(args: &[&str]) -> Output {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let output = Command::new(binary())
        .args(args)
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .output()
        .expect("run binary");

    Output {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Captured result of a run.
struct Output {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

#[test]
fn version_prints_one_line_and_exits_zero() {
    let out = run(&["--version"]);
    assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
    assert_eq!(out.stdout.lines().count(), 1, "{}", out.stdout);
    assert!(out.stdout.starts_with("rune "), "{}", out.stdout);
    assert!(out.stderr.is_empty(), "{}", out.stderr);
}

#[test]
fn version_short_flag_matches_long_flag() {
    assert_eq!(run(&["-v"]).stdout, run(&["--version"]).stdout);
    assert_eq!(run(&["version"]).stdout, run(&["--version"]).stdout);
}

#[test]
fn help_lists_every_command_group() {
    let out = run(&["--help"]);
    assert_eq!(out.status, Some(0));
    for group in [
        "Run:",
        "Sessions and local records:",
        "Account and configuration:",
        "Diagnostics and maintenance:",
    ] {
        assert!(out.stdout.contains(group), "missing group {group}");
    }
    assert!(out.stdout.contains("Global flags:"));
}

#[test]
fn help_for_one_command_shows_its_usage() {
    let out = run(&["help", "sessions"]);
    assert_eq!(out.status, Some(0));
    assert!(out.stdout.contains("rune sessions"));
    assert!(out.stdout.contains("--limit"));
}

#[test]
fn help_for_an_unknown_command_says_so() {
    let out = run(&["help", "frobnicate"]);
    assert!(out.stdout.contains("frobnicate"));
    assert!(out.stdout.contains("rune help"));
}

#[test]
fn unknown_command_fails_with_a_hint() {
    let out = run(&["frobnicate"]);
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.contains("not a command"), "{}", out.stderr);
    assert!(out.stderr.contains("hint:"), "{}", out.stderr);
}

#[test]
fn unknown_global_flag_fails_before_loading_configuration() {
    let out = run(&["--nonsense"]);
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.contains("--nonsense"), "{}", out.stderr);
    assert!(out.stdout.is_empty());
}

#[test]
fn missing_flag_value_is_reported_as_a_missing_field() {
    let out = run(&["--model"]);
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.contains("requires a value"), "{}", out.stderr);
}

#[test]
fn session_last_inspects_the_saved_session_in_the_current_workspace() {
    use camino::Utf8PathBuf;
    use rune_core::{id::SessionId, paths::Paths};
    use rune_session::{SessionEvent, SessionStore};

    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().canonicalize().expect("canonical tempdir"))
        .expect("utf-8 path");
    let paths = Paths {
        state_root: root.join("state"),
        config_root: root.join("config/rune"),
        data_root: root.join("data/rune"),
    };
    let workspaces = [root.join("one"), root.join("two")];
    let ids = ["sessionws001", "sessionws002"];
    for (workspace, raw) in workspaces.iter().zip(ids) {
        std::fs::create_dir(workspace).expect("workspace");
        let id: SessionId = raw.parse().expect("session id");
        let store = SessionStore::create(&paths, &id).expect("create session");
        store
            .append(SessionEvent::WorkspaceSet {
                workspace: workspace.to_string(),
            })
            .expect("save workspace");
        store
            .append(SessionEvent::UserMessage {
                text: "saved prompt".into(),
            })
            .expect("save message");
    }

    for (workspace, raw) in workspaces.iter().zip(ids) {
        let command = || {
            let mut command = Command::new(binary());
            command
                .current_dir(workspace)
                .env("RUNE_HOME", &paths.state_root)
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_DATA_HOME", root.join("data"));
            command
        };
        for selector in ["last", ids[0], ids[1]] {
            let out = command()
                .args(["session", selector, "--json"])
                .output()
                .expect("run binary");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.stderr.is_empty());
            let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json");
            assert_eq!(value["id"], if selector == "last" { raw } else { selector });
            assert_eq!(value["events"], 2);
        }
        let out = command()
            .args(["session", "last"])
            .output()
            .expect("run binary");
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        assert!(String::from_utf8_lossy(&out.stdout).contains(raw));
    }
}

#[test]
fn session_last_without_saved_sessions_reports_not_found() {
    let out = run(&["session", "last", "--json"]);
    assert_eq!(out.status, Some(1));
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert!(out.stderr.contains("not_found"), "{}", out.stderr);
    assert!(
        out.stderr.contains("no session has been saved yet"),
        "{}",
        out.stderr
    );
}

#[test]
fn session_inspection_still_requires_a_valid_selector() {
    for (args, error) in [
        (vec!["session", "--json"], "missing_field"),
        (vec!["session", "bad", "--json"], "invalid_field"),
    ] {
        let out = run(&args);
        assert_eq!(out.status, Some(1));
        assert!(out.stdout.is_empty(), "{}", out.stdout);
        assert!(out.stderr.contains(error), "{}", out.stderr);
    }
}

#[test]
fn tree_last_reports_the_saved_branch_structure_in_the_current_workspace() {
    use camino::Utf8PathBuf;
    use rune_core::{id::SessionId, paths::Paths};
    use rune_session::{SessionEvent, SessionStore};

    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().canonicalize().expect("canonical tempdir"))
        .expect("utf-8 path");
    let paths = Paths {
        state_root: root.join("state"),
        config_root: root.join("config/rune"),
        data_root: root.join("data/rune"),
    };
    let workspaces = [root.join("one"), root.join("two")];
    let ids = ["sessionws001", "sessionws002"];
    for ((workspace, raw), turns) in workspaces.iter().zip(ids).zip([1_u64, 2]) {
        std::fs::create_dir(workspace).expect("workspace");
        let id: SessionId = raw.parse().expect("session id");
        let store = SessionStore::create(&paths, &id).expect("create session");
        store
            .append(SessionEvent::WorkspaceSet {
                workspace: workspace.to_string(),
            })
            .expect("save workspace");
        for turn in 1..=turns {
            store
                .append(SessionEvent::UserMessage {
                    text: "saved prompt".into(),
                })
                .expect("save prompt");
            store
                .append(SessionEvent::AssistantMessage {
                    turn,
                    text: "saved reply".into(),
                })
                .expect("save reply");
        }
    }

    for (workspace, raw) in workspaces.iter().zip(ids) {
        let command = || {
            let mut command = Command::new(binary());
            command
                .current_dir(workspace)
                .env("RUNE_HOME", &paths.state_root)
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_DATA_HOME", root.join("data"));
            command
        };
        for selector in [None, Some("last"), Some(ids[0]), Some(ids[1])] {
            let out = command()
                .arg("tree")
                .args(selector)
                .arg("--json")
                .output()
                .expect("run binary");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.stderr.is_empty());
            let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json");
            let expected_id = match selector {
                None | Some("last") => raw,
                Some(id) => id,
            };
            let nodes = if expected_id == ids[0] { 2 } else { 4 };
            assert_eq!(
                value,
                serde_json::json!({
                    "session": expected_id,
                    "active_branch": "main",
                    "branches": [{
                        "name": "main",
                        "turns": nodes,
                        "head": nodes,
                        "diverges_at": null,
                    }],
                    "turns": nodes,
                })
            );
        }
        let out = command()
            .args(["tree", "last"])
            .output()
            .expect("run binary");
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        let text = String::from_utf8_lossy(&out.stdout);
        for expected in [raw, "main", "saved prompt", "saved reply"] {
            assert!(text.contains(expected), "{text}");
        }
    }
}

#[test]
fn tree_last_without_saved_sessions_reports_not_found() {
    for args in [vec!["tree", "last", "--json"], vec!["tree", "--json"]] {
        let out = run(&args);
        assert_eq!(out.status, Some(1));
        assert!(out.stdout.is_empty(), "{}", out.stdout);
        assert!(out.stderr.contains("not_found"), "{}", out.stderr);
        assert!(
            out.stderr.contains("no session has been saved yet"),
            "{}",
            out.stderr
        );
    }
}

#[test]
fn tree_still_rejects_an_invalid_exact_id() {
    let out = run(&["tree", "bad", "--json"]);
    assert_eq!(out.status, Some(1));
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert!(out.stderr.contains("invalid_field"), "{}", out.stderr);
}

#[test]
fn auth_status_json_matches_the_default_inspection() {
    let default = run(&["auth", "--json"]);
    let status = run(&["auth", "status", "--json"]);
    for out in [&default, &status] {
        assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
        assert!(out.stderr.is_empty(), "{}", out.stderr);
    }
    let default: serde_json::Value = serde_json::from_str(&default.stdout).expect("valid json");
    let status: serde_json::Value = serde_json::from_str(&status.stdout).expect("valid json");
    assert!(status.is_object());
    assert!(status["provider"].is_string());
    assert!(status["model"].is_string());
    assert!(status.get("base_url").is_some());
    assert_eq!(status, default);
}

#[test]
fn auth_status_text_matches_the_default_inspection() {
    let default = run(&["auth"]);
    let status = run(&["auth", "status"]);
    for out in [&default, &status] {
        assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
        assert!(out.stderr.is_empty(), "{}", out.stderr);
        assert!(!out.stdout.is_empty());
    }
    assert_eq!(status.stdout, default.stdout);
}

#[test]
fn usage_reports_the_selected_period_and_interval() {
    let day_ms = 24 * 60 * 60 * 1_000;
    for (args, period, days) in [
        (vec!["usage", "--json"], "24h", 1),
        (vec!["usage", "7d", "--json"], "7d", 7),
        (vec!["usage", "--period", "24h", "--json"], "24h", 1),
        (vec!["usage", "--period", "7d", "--json"], "7d", 7),
        (vec!["usage", "--period", "30d", "--json"], "30d", 30),
        (vec!["usage", "--period=7d", "--json"], "7d", 7),
        (vec!["usage", "24h", "--period", "7d", "--json"], "7d", 7),
    ] {
        let out = run(&args);
        assert_eq!(out.status, Some(0), "{args:?}: {}", out.stderr);
        let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
        assert_eq!(value["period"], period, "{args:?}");
        let now_ms = value["now_ms"].as_i64().expect("now_ms");
        let since_ms = value["since_ms"].as_i64().expect("since_ms");
        assert_eq!(now_ms - since_ms, days * day_ms, "{args:?}");
    }
}

#[test]
fn usage_rejects_an_invalid_period_flag() {
    let out = run(&["usage", "--period", "99y", "--json"]);
    assert_eq!(out.status, Some(1), "{}", out.stdout);
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert!(out.stderr.contains("invalid_field"), "{}", out.stderr);
    assert!(out.stderr.contains("99y"), "{}", out.stderr);
    assert!(out.stderr.contains("24h, 7d, or 30d"), "{}", out.stderr);
}

#[test]
fn usage_period_flag_filters_the_ledger_to_seven_days() {
    use rune_session::usage::{HelperKind, Ledger, UsageRecord, now_ms};

    let dir = tempfile::TempDir::new().expect("tempdir");
    let state = dir.path().join("state");
    let path = camino::Utf8PathBuf::from_path_buf(state.join("usage.jsonl")).expect("utf-8 path");
    let ledger = Ledger::new(path);
    let now = now_ms();
    let day_ms = 24 * 60 * 60 * 1_000;
    for (days_ago, tokens) in [(0, 11), (3, 22), (8, 44)] {
        let mut record = UsageRecord::new(now - days_ago * day_ms, "test-model", HelperKind::Main);
        record.input_tokens = Some(tokens);
        ledger.append(&record).expect("append usage");
    }

    let out = Command::new(binary())
        .args(["usage", "--period", "7d", "--json"])
        .env("RUNE_HOME", &state)
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .output()
        .expect("run binary");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json");
    assert_eq!(value["period"], "7d");
    assert_eq!(value["requests"], 2);
    assert_eq!(value["tokens"]["input_tokens"], 33);
}

#[test]
fn doctor_passes_on_a_clean_setup() {
    let out = run(&["doctor"]);
    assert_eq!(
        out.status,
        Some(0),
        "stdout: {}\nstderr: {}",
        out.stdout,
        out.stderr
    );
    assert!(out.stdout.contains("ok "), "{}", out.stdout);
}

#[test]
fn doctor_json_is_parseable_and_names_every_check() {
    let out = run(&["doctor", "--json"]);
    assert_eq!(out.status, Some(0));
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    let checks = value["checks"].as_array().expect("checks array");
    assert!(!checks.is_empty());
    for check in checks {
        assert!(check["name"].is_string());
        assert!(check["outcome"].is_string());
        assert!(check["detail"].is_string());
    }
    assert!(value["version"].is_string());
    assert!(value["state_root"].is_string());
}

#[test]
fn doctor_reports_an_unconfigured_provider_as_unknown_not_failed() {
    let out = run(&["doctor", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    let checks = value["checks"].as_array().expect("checks array");
    let provider = checks
        .iter()
        .find(|check| check["name"] == "provider")
        .expect("provider check");
    assert_eq!(provider["outcome"], "unknown");
    assert!(
        provider["hint"]
            .as_str()
            .expect("hint")
            .contains("rune connect")
    );
}

#[test]
fn a_fresh_install_has_no_provider_and_says_how_to_connect_one() {
    let out = run(&["status", "--json"]);
    assert_eq!(out.status, Some(0));
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    assert_eq!(value["provider"], "unconfigured");
    assert_eq!(value["provider_connected"], false);
}

#[test]
fn limits_json_lists_every_limit_with_its_source() {
    let out = run(&["limits", "--json"]);
    assert_eq!(out.status, Some(0));
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    let limits = value["limits"].as_array().expect("limits array");
    assert!(
        limits.len() > 20,
        "expected the full set, got {}",
        limits.len()
    );
    for row in limits {
        assert!(row["name"].is_string());
        assert!(!row["value"].is_null() || row["value"] == "off");
        assert!(!row["default"].is_null() || row["default"] == "off");
        assert!(row["unit"].is_string());
        assert!(row["min"].is_number());
        assert!(row["description"].is_string());
    }
}

#[test]
fn limits_text_shows_defaults() {
    let out = run(&["limits"]);
    assert_eq!(out.status, Some(0));
    assert!(out.stdout.contains("max_tool_result_bytes"));
    assert!(out.stdout.contains("provider_head_timeout_ms"));
    assert!(out.stdout.contains("default"));
}

#[test]
fn a_limit_override_is_visible_in_limits_output() {
    let out = run(&["--limit", "list_entries=42", "limits", "--json"]);
    assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    let limits = value["limits"].as_array().expect("limits array");
    let row = limits
        .iter()
        .find(|row| row["name"] == "list_entries")
        .expect("list_entries");
    assert_eq!(row["value"], 42);
    assert_eq!(row["source"], "command_line");
}

#[test]
fn a_limit_outside_its_range_is_rejected() {
    let out = run(&["--limit", "compaction_trigger_percent=5", "limits"]);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.contains("compaction_trigger_percent"),
        "{}",
        out.stderr
    );
}

#[test]
fn an_unknown_limit_name_is_rejected() {
    let out = run(&["--limit", "nonsense=5", "limits"]);
    assert_eq!(out.status, Some(1));
    assert!(out.stderr.contains("not a known limit"), "{}", out.stderr);
}

#[test]
fn permissions_explain_flag_reports_one_structured_decision() {
    for args in [
        vec!["permissions", "--explain", "shell:pwd", "--json"],
        vec!["permissions", "--explain=shell:pwd", "--json"],
    ] {
        let out = run(&args);
        assert_eq!(out.status, Some(0), "{}", out.stderr);
        assert!(out.stderr.is_empty(), "{}", out.stderr);
        let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("one JSON object");
        assert_eq!(value["tool"], "shell");
        assert_eq!(value["target"], "pwd");
        assert_eq!(value["mode"], "auto");
        assert_eq!(value["outcome"], "allow");
        assert_eq!(value["rule"], "shell pwd");
        assert_eq!(value["layer"], "default");
        assert!(value.get("rules").is_none(), "{value}");
    }
}

#[test]
fn permissions_explanations_preserve_positional_and_text_output() {
    let positional = run(&["permissions", "shell", "pwd"]);
    let flagged = run(&["permissions", "--explain", "shell:pwd"]);
    assert_eq!(positional.status, Some(0), "{}", positional.stderr);
    assert_eq!(flagged.status, Some(0), "{}", flagged.stderr);
    assert_eq!(flagged.stdout, positional.stdout);
    assert_eq!(
        flagged.stdout,
        "shell `pwd`: allowed (allow: matched `shell pwd` at the default layer)\n"
    );

    let json = run(&["permissions", "shell", "pwd", "--json"]);
    assert_eq!(json.status, Some(0), "{}", json.stderr);
    let value: serde_json::Value = serde_json::from_str(&json.stdout).expect("one JSON object");
    assert_eq!(value["outcome"], "allow");
    assert_eq!(value["rule"], "shell pwd");
}

#[test]
fn permissions_explain_preserves_colons_in_the_target() {
    let out = run(&[
        "permissions",
        "--explain",
        "read_file:path:segment",
        "--json",
    ]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("one JSON object");
    assert_eq!(value["tool"], "read_file");
    assert_eq!(value["target"], "path:segment");
    assert_eq!(value["outcome"], "allow");
    assert_eq!(value["rule"], "read_file *");
}

#[test]
fn permissions_explain_rejects_malformed_actions() {
    for action in ["pwd", ":pwd", "shell:", ""] {
        let out = run(&["permissions", "--explain", action]);
        assert_eq!(out.status, Some(1), "{action}: {}", out.stderr);
        assert!(out.stdout.is_empty(), "{}", out.stdout);
        assert!(out.stderr.contains("invalid_field"), "{}", out.stderr);
        assert!(out.stderr.contains("tool:target"), "{}", out.stderr);
        assert!(out.stderr.contains("shell:pwd"), "{}", out.stderr);
    }
}

#[test]
fn permissions_explain_reports_mode_defaults_and_effective_outcomes() {
    for (mode, action, outcome, rule, from_rule) in [
        ("ask", "shell:pwd", "ask", "no rule matched `pwd`", false),
        (
            "auto",
            "web_fetch:https://example.com",
            "deny",
            "web_fetch *",
            true,
        ),
        (
            "full-access",
            "web_fetch:https://example.com",
            "allow",
            "web_fetch *",
            true,
        ),
    ] {
        let out = run(&[
            "permissions",
            "--permission-mode",
            mode,
            "--explain",
            action,
            "--json",
        ]);
        assert_eq!(out.status, Some(0), "{}", out.stderr);
        let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("one JSON object");
        assert_eq!(value["outcome"], outcome, "{mode}: {value}");
        assert_eq!(value["rule"], rule, "{mode}: {value}");
        assert_eq!(value["from_rule"], from_rule, "{mode}: {value}");
        assert_eq!(value["layer"], "default");
    }
}

#[test]
fn permissions_without_an_action_still_lists_rules() {
    let out = run(&["permissions", "--json"]);
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("one JSON object");
    assert_eq!(value["mode"], "auto");
    let rules = value["rules"].as_array().expect("rules array");
    assert!(
        rules
            .iter()
            .any(|rule| rule["tool"] == "shell" && rule["pattern"] == "pwd")
    );
    assert!(value.get("outcome").is_none(), "{value}");

    let text = run(&["permissions"]);
    assert_eq!(text.status, Some(0), "{}", text.stderr);
    assert!(text.stdout.contains("mode: auto"), "{}", text.stdout);
    assert!(text.stdout.contains("pattern"), "{}", text.stdout);
}

#[test]
fn config_explain_reports_the_source_layer() {
    let out = run(&["config", "--json"]);
    assert_eq!(out.status, Some(0));
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    let values = value["values"].as_array().expect("values array");
    let provider = values
        .iter()
        .find(|row| row["key"] == "provider")
        .expect("provider row");
    assert_eq!(provider["source"], "default");
    assert_eq!(value["layers"]["command_line"], "command_line");
}

#[test]
fn cli_provider_and_model_overrides_resolve_only_their_own_settings() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config = dir.path().join("user.toml");
    std::fs::write(
        &config,
        r#"
provider = "anthropic"
base_url = "https://previous-provider.invalid"
api_key_env = "PREVIOUS_PROVIDER_KEY"
[models.anthropic]
id = "large-model"
context_window = 2000000
[models.chat_completions]
id = "chat-model"
context_window = 64000
[models.responses]
id = "large-model"
context_window = 32000
[models]
opencode = "named-model"
"#,
    )
    .expect("write config");

    for (args, provider, provider_source, model, model_source, capacity, capacity_source, reset) in [
        (
            vec!["--provider", "openai", "config"],
            "chat_completions",
            "command_line",
            "chat-model",
            "user",
            "64000",
            "user",
            true,
        ),
        (
            vec!["config", "--provider", "responses"],
            "responses",
            "command_line",
            "large-model",
            "user",
            "32000",
            "user",
            true,
        ),
        (
            vec!["--provider", "opencode", "config"],
            "opencode",
            "command_line",
            "named-model",
            "user",
            "128000",
            "default",
            true,
        ),
        (
            vec!["--provider", "unknown-provider", "config"],
            "unknown-provider",
            "command_line",
            "",
            "default",
            "128000",
            "default",
            true,
        ),
        (
            vec!["--provider", "ANTHROPIC", "config"],
            "anthropic",
            "command_line",
            "large-model",
            "user",
            "2000000",
            "user",
            false,
        ),
        (
            vec![
                "--provider",
                "openai",
                "--model",
                "explicit-model",
                "config",
            ],
            "chat_completions",
            "command_line",
            "explicit-model",
            "command_line",
            "128000",
            "default",
            true,
        ),
        (
            vec![
                "--model",
                "explicit-model",
                "config",
                "--provider",
                "openai",
            ],
            "chat_completions",
            "command_line",
            "explicit-model",
            "command_line",
            "128000",
            "default",
            true,
        ),
        (
            vec!["--provider", "openai", "--model", "chat-model", "config"],
            "chat_completions",
            "command_line",
            "chat-model",
            "command_line",
            "64000",
            "user",
            true,
        ),
        (
            vec!["--model", "small-model", "config"],
            "anthropic",
            "user",
            "small-model",
            "command_line",
            "128000",
            "default",
            false,
        ),
        (
            vec!["config", "--model", "large-model"],
            "anthropic",
            "user",
            "large-model",
            "command_line",
            "2000000",
            "user",
            false,
        ),
    ] {
        let mut command = Command::new(binary());
        // Keep the fixture independent of the developer's provider environment.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("RUNE_") {
                command.env_remove(key);
            }
        }
        let out = command
            .args(&args)
            .arg("--json")
            .current_dir(dir.path())
            .env("RUNE_CONFIG", &config)
            .env("RUNE_STATE", dir.path().join("state"))
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_DATA_HOME", dir.path().join("data"))
            .output()
            .expect("run config");
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert!(out.stderr.is_empty(), "{args:?}: {out:?}");
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json");
        assert_eq!(value["diagnostics"], serde_json::json!([]), "{args:?}");
        let values = value["values"].as_array().expect("values array");
        for (key, expected, source) in [
            ("provider", provider, provider_source),
            ("model", model, model_source),
            ("context_window", capacity, capacity_source),
            (
                "base_url",
                if reset {
                    "default"
                } else {
                    "https://previous-provider.invalid"
                },
                if reset { "default" } else { "user" },
            ),
            (
                "api_key_env",
                if reset {
                    "default"
                } else {
                    "PREVIOUS_PROVIDER_KEY"
                },
                if reset { "default" } else { "user" },
            ),
        ] {
            let row = values
                .iter()
                .find(|row| row["key"] == key)
                .expect("config row");
            assert_eq!(row["value"], expected, "{args:?}: {key}");
            assert_eq!(row["source"], source, "{args:?}: {key}");
        }
    }
}

#[test]
fn a_project_file_cannot_set_the_model() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join(".rune.toml"), "model = \"sneaky\"\n").expect("write");

    let output = Command::new(binary())
        .args(["config", "--json"])
        .current_dir(dir.path())
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .output()
        .expect("run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    let values = value["values"].as_array().expect("values array");
    let model = values
        .iter()
        .find(|row| row["key"] == "model")
        .expect("model row");
    assert_eq!(model["value"], "", "project file set the model");

    let diagnostics = value["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["code"], "key_not_allowed_in_scope");
}

#[test]
fn a_malformed_user_config_warns_without_failing() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config_dir = dir.path().join("config").join("rune");
    std::fs::create_dir_all(&config_dir).expect("mkdir");
    std::fs::write(config_dir.join("config.toml"), "this is not toml =").expect("write");

    let output = Command::new(binary())
        .args(["config", "--json"])
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .output()
        .expect("run");

    assert!(
        output.status.success(),
        "a warning must not fail the command"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    let diagnostics = value["diagnostics"].as_array().expect("diagnostics");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["layer"], "user");
    assert_eq!(diagnostics[0]["code"], "invalid_configuration");
}

#[test]
fn workspace_list_is_empty_on_a_fresh_install() {
    let out = run(&["workspace", "list", "--json"]);
    assert_eq!(out.status, Some(0));
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    assert_eq!(value["directories"].as_array().expect("array").len(), 0);
}

/// Isolates configuration mutations from the caller's Rune settings.
fn config_mutation_command(root: &camino::Utf8Path) -> Command {
    let mut command = Command::new(binary());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RUNE_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(root)
        .env("RUNE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"));
    command
}

#[test]
fn workspace_mutations_read_and_write_the_active_config() {
    for override_path in [
        None,
        Some(""),
        Some("settings.toml"),
        Some("custom/settings.toml"),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
                .expect("private scratch directory");
        }
        let default_path = root.join("config/rune/config.toml");
        let default_text = "effort = 'low'\n";
        rune_core::paths::create_dir_private(default_path.parent().expect("default parent"))
            .expect("private default parent");
        rune_core::paths::write_private(&default_path, default_text).expect("default config");
        let active_path = override_path
            .filter(|path| !path.is_empty())
            .map_or_else(|| default_path.clone(), |path| root.join(path));
        let existing = root.join("existing");
        let added = root.join("added");
        std::fs::create_dir(&existing).expect("existing directory");
        std::fs::create_dir(&added).expect("added directory");
        let existing = existing.canonicalize_utf8().expect("canonical existing");
        let added = added.canonicalize_utf8().expect("canonical added");
        let mut original: toml::Table =
            toml::from_str("effort = 'high'\n[models]\nanthropic = 'saved-model'\n")
                .expect("original config");
        original.insert(
            "additional_directories".to_owned(),
            toml::Value::Array(vec![toml::Value::String(existing.to_string())]),
        );
        rune_core::paths::create_dir_private(active_path.parent().expect("active parent"))
            .expect("private active parent");
        rune_core::paths::write_private(
            &active_path,
            &toml::to_string(&original).expect("render config"),
        )
        .expect("active config");
        let mut both = vec![existing.as_str(), added.as_str()];
        both.sort_unstable();

        for (args, expected) in [
            (vec!["add", added.as_str()], both.clone()),
            (vec!["add", added.as_str()], both),
            (vec!["remove", added.as_str()], vec![existing.as_str()]),
            (vec!["clear"], vec![]),
        ] {
            let mut command = config_mutation_command(root);
            if let Some(path) = override_path {
                command.env("RUNE_CONFIG", path);
            }
            let out = command
                .arg("workspace")
                .args(&args)
                .arg("--json")
                .output()
                .expect("workspace mutation");
            assert!(out.status.success(), "{override_path:?} {args:?}: {out:?}");
            let stored: toml::Table =
                toml::from_str(&std::fs::read_to_string(&active_path).expect("read active config"))
                    .expect("parse active config");
            assert_eq!(stored["effort"], original["effort"]);
            assert_eq!(stored["models"], original["models"]);
            let directories = stored
                .get("additional_directories")
                .map_or_else(Vec::new, |value| {
                    value
                        .as_array()
                        .expect("directory array")
                        .iter()
                        .map(|value| value.as_str().expect("directory string"))
                        .collect::<Vec<_>>()
                });
            assert_eq!(directories, expected, "{override_path:?} {args:?}");
            if active_path != default_path {
                assert_eq!(
                    std::fs::read_to_string(&default_path).expect("default"),
                    default_text
                );
            }
            // Reload in a fresh process, including after clear.
            let mut command = config_mutation_command(root);
            if let Some(path) = override_path {
                command.env("RUNE_CONFIG", path);
            }
            let out = command
                .args(["workspace", "list", "--json"])
                .output()
                .expect("list");
            assert!(out.status.success(), "{out:?}");
            let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
            assert_eq!(value["directories"], serde_json::json!(expected));
        }
    }
}

#[test]
fn config_mutations_create_an_override_without_creating_the_default() {
    for (args, override_path) in [
        (
            vec!["workspace", "add", ".", "--json"],
            "custom/settings.toml",
        ),
        (
            vec!["connect", "anthropic", "--model", "saved-model", "--json"],
            "custom/settings.toml",
        ),
        (vec!["workspace", "add", ".", "--json"], "settings.toml"),
        (
            vec!["connect", "anthropic", "--model", "saved-model", "--json"],
            "settings.toml",
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let path = root.join(override_path);
        let out = config_mutation_command(root)
            .args(&args)
            .env("RUNE_CONFIG", override_path)
            .env("ANTHROPIC_API_KEY", "test-key")
            .output()
            .expect("config mutation");
        assert!(out.status.success(), "{args:?}: {out:?}");
        let text = rune_core::paths::read_private(&path, 4096)
            .expect("private config")
            .expect("created config");
        let stored: toml::Table = toml::from_str(&text).expect("parse config");
        if args[0] == "workspace" {
            assert_eq!(
                stored["additional_directories"]
                    .as_array()
                    .expect("array")
                    .len(),
                1
            );
        } else {
            assert_eq!(stored["provider"].as_str(), Some("anthropic"));
            assert_eq!(stored["models"]["anthropic"].as_str(), Some("saved-model"));
        }
        assert!(
            !root.join("config").exists(),
            "default config directory was created"
        );
    }
}

#[test]
fn provider_selection_preserves_the_override_and_leaves_the_default_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
    let default_path = root.join("config/rune/config.toml");
    let default_text = "effort = 'low'\n";
    rune_core::paths::create_dir_private(default_path.parent().expect("default parent"))
        .expect("private default parent");
    rune_core::paths::write_private(&default_path, default_text).expect("default config");
    let path = root.join("custom/settings.toml");
    rune_core::paths::create_dir_private(path.parent().expect("active parent"))
        .expect("private active parent");
    rune_core::paths::write_private(
        &path,
        "effort = 'high'\n[models]\nresponses = 'other-model'\n",
    )
    .expect("active config");
    let out = config_mutation_command(root)
        .args(["connect", "anthropic", "--model", "saved-model", "--json"])
        .env("RUNE_CONFIG", &path)
        .env("ANTHROPIC_API_KEY", "test-key")
        .output()
        .expect("connect");
    assert!(out.status.success(), "{out:?}");
    let stored: toml::Table =
        toml::from_str(&std::fs::read_to_string(&path).expect("active config"))
            .expect("parse config");
    assert_eq!(stored["provider"].as_str(), Some("anthropic"));
    assert_eq!(
        stored["base_url"].as_str(),
        Some("https://api.anthropic.com")
    );
    assert_eq!(stored["models"]["anthropic"].as_str(), Some("saved-model"));
    assert_eq!(stored["models"]["responses"].as_str(), Some("other-model"));
    assert_eq!(stored["effort"].as_str(), Some("high"));
    assert_eq!(
        std::fs::read_to_string(&default_path).expect("default config"),
        default_text
    );
}

#[test]
fn an_unknown_workspace_subcommand_is_rejected() {
    let out = run(&["workspace", "frobnicate"]);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.contains("list, add, remove, or clear"),
        "{}",
        out.stderr
    );
}

#[test]
fn prompt_reports_the_built_in_source() {
    let out = run(&["prompt"]);
    assert_eq!(out.status, Some(0));
    assert!(out.stdout.contains("built in"), "{}", out.stdout);
}

#[test]
fn an_unavailable_surface_says_so_explicitly() {
    // Fetching a release over the network is genuinely unsupported: the
    // installer only takes an artifact the caller supplies.
    let out = run(&[
        "upgrade",
        "--from",
        "https://example.test/rune",
        "--checksum",
        "00",
    ]);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.contains("not available in this build"),
        "{}",
        out.stderr
    );
}

#[test]
fn stdout_stays_clean_of_diagnostics() {
    // Machine consumers read stdout, so an error must not write there.
    let out = run(&["frobnicate"]);
    assert!(
        out.stdout.is_empty(),
        "stdout was not empty: {}",
        out.stdout
    );
    assert!(!out.stderr.is_empty());
}

#[test]
fn benchmark_short_circuits_before_configuration() {
    // With the benchmark variable set the process must exit after dispatch, so
    // a broken configuration cannot make the startup measurement fail.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config_dir = dir.path().join("config").join("rune");
    std::fs::create_dir_all(&config_dir).expect("mkdir");
    std::fs::write(config_dir.join("config.toml"), "broken =").expect("write");

    let output = Command::new(binary())
        .args(["doctor"])
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("RUNE_BENCH", "1")
        .output()
        .expect("run");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn benchmark_does_not_suppress_help_or_version() {
    // Help and version must still answer when the benchmark variable is set,
    // otherwise a startup measurement could not cover those paths.
    for args in [vec!["--help"], vec!["--version"]] {
        let output = Command::new(binary())
            .args(&args)
            .env("RUNE_BENCH", "1")
            .output()
            .expect("run");
        assert!(output.status.success(), "{args:?}");
        assert!(!output.stdout.is_empty(), "{args:?} printed nothing");
    }
}

/// A listener that counts the connections it accepts and answers none.
struct Listener {
    port: u16,
    accepts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Listener {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accepts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&accepts);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stream.is_err() {
                    break;
                }
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        Self { port, accepts }
    }

    fn count(&self) -> usize {
        self.accepts.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Runs the binary against a provider served at the listener's port.
fn run_against(
    listener: &Listener,
    model: Option<&str>,
    extra: &[(&str, &str)],
    args: &[&str],
) -> Output {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let mut command = Command::new(binary());
    command
        .args(args)
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("RUNE_PROVIDER", "chat_completions")
        .env(
            "RUNE_BASE_URL",
            format!("http://127.0.0.1:{}/v1", listener.port),
        )
        .env("RUNE_API_KEY_ENV", "RUNE_CLI_TEST_KEY")
        .env("RUNE_CLI_TEST_KEY", "sk-test")
        .env_remove("RUNE_MODEL")
        .env_remove("RUNE_OFFLINE");
    if let Some(model) = model {
        command.env("RUNE_MODEL", model);
    }
    for (key, value) in extra {
        command.env(key, value);
    }
    let output = command.output().expect("run binary");
    Output {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn offline_after_the_command_refuses_the_request() {
    // Offline after the command refuses the request just as it does before it.
    let listener = Listener::start();
    for args in [
        vec!["ask", "--offline", "hi"],
        vec!["ask", "hi", "--offline"],
    ] {
        let out = run_against(&listener, Some("m"), &[], &args);
        assert_eq!(out.status, Some(1), "{args:?}: {}", out.stderr);
        assert!(out.stderr.contains("disabled"), "{args:?}: {}", out.stderr);
    }
    assert_eq!(listener.count(), 0, "an offline run reached the endpoint");
}

#[test]
fn offline_in_the_user_file_preserves_the_model_and_refuses_requests() {
    let listener = Listener::start();
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config_dir = dir.path().join("config").join("rune");
    std::fs::create_dir_all(&config_dir).expect("mkdir");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            r#"
offline = true
web_tools = true
provider = "chat_completions"
base_url = "http://127.0.0.1:{}/v1"
api_key_env = "RUNE_CLI_TEST_KEY"
[models]
chat_completions = "configured-model"
[limits]
provider_request_timeout_ms = 1000
"#,
            listener.port
        ),
    )
    .expect("write config");

    let run_with_config = |args: &[&str]| {
        Command::new(binary())
            .env_clear()
            .env("HOME", dir.path())
            .env("RUNE_HOME", dir.path().join("state"))
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_DATA_HOME", dir.path().join("data"))
            .env("RUNE_CLI_TEST_KEY", "sk-test")
            .current_dir(dir.path())
            .args(args)
            .output()
            .expect("run binary")
    };

    let models = run_with_config(&["models"]);
    assert!(models.status.success(), "{models:?}");
    assert!(
        String::from_utf8_lossy(&models.stdout).contains("configured-model"),
        "{models:?}"
    );
    assert!(models.stderr.is_empty(), "{models:?}");

    let ask = run_with_config(&["ask", "hi"]);
    assert_eq!(ask.status.code(), Some(1), "{ask:?}");
    let stderr = String::from_utf8_lossy(&ask.stderr);
    assert!(stderr.contains("outbound requests are disabled"), "{ask:?}");
    assert!(!stderr.contains("could not parse"), "{ask:?}");
    assert_eq!(
        listener.count(),
        0,
        "the offline user file allowed a request"
    );
}

#[test]
fn a_model_named_after_the_command_is_used() {
    let listener = Listener::start();
    let out = run_against(
        &listener,
        None,
        &[],
        &["ask", "--model", "m", "--offline", "hi"],
    );
    assert!(
        !out.stderr.contains("no model is selected"),
        "the model after the command was ignored: {}",
        out.stderr
    );
}

#[test]
fn a_limit_after_the_command_is_applied() {
    let out = run(&["limits", "--limit", "max_agent_steps=5", "--json"]);
    assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    let row = value["limits"]
        .as_array()
        .expect("limits array")
        .iter()
        .find(|row| row["name"] == "max_agent_steps")
        .cloned()
        .expect("max_agent_steps");
    assert_eq!(row["value"], 5);
    assert_eq!(row["source"], "command_line");
}

#[test]
fn listing_models_offline_reports_the_configured_model_without_a_request() {
    // Offline is honoured however it is given: the global flag, the flag after
    // the command, or the environment.
    let listener = Listener::start();
    for (env, args) in [
        (vec![], vec!["--offline", "models"]),
        (vec![], vec!["models", "--offline"]),
        (vec![("RUNE_OFFLINE", "1")], vec!["models"]),
    ] {
        let out = run_against(&listener, Some("configured-model"), &env, &args);
        assert_eq!(out.status, Some(0), "{args:?} {env:?}: {}", out.stderr);
        assert!(
            out.stdout.contains("configured-model"),
            "{args:?} {env:?}: {}",
            out.stdout
        );
    }
    assert_eq!(
        listener.count(),
        0,
        "an offline listing reached the endpoint"
    );
}

/// Runs `rune connect` for a machine caller, with or without the key exported.
fn connect_json(key: Option<&str>) -> (Output, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let mut command = Command::new(binary());
    command
        .args(["connect", "anthropic", "--json"])
        .env("RUNE_HOME", dir.path().join("state"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("RUNE_API_KEY_ENV")
        .env_remove("RUNE_PROVIDER")
        .env_remove("RUNE_BASE_URL")
        .env_remove("RUNE_MODEL");
    if let Some(key) = key {
        command.env("ANTHROPIC_API_KEY", key);
    }
    let output = command.output().expect("run binary");
    let out = Output {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    (out, dir)
}

#[test]
fn connecting_without_a_credential_fails_and_saves_nothing() {
    // A machine caller with no key is refused, rather than told it connected
    // and left with a selection that no request can use.
    let (out, dir) = connect_json(None);
    assert_eq!(out.status, Some(1), "stdout: {}", out.stdout);
    assert!(out.stderr.contains("no credential"), "{}", out.stderr);
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert!(
        !dir.path()
            .join("config")
            .join("rune")
            .join("config.toml")
            .exists(),
        "the selection was saved without a credential"
    );
}

#[test]
fn connecting_with_the_key_exported_succeeds() {
    let (out, _dir) = connect_json(Some("sk-ant-test"));
    assert_eq!(out.status, Some(0), "stderr: {}", out.stderr);
    let value: serde_json::Value = serde_json::from_str(&out.stdout).expect("valid json");
    assert_eq!(value["provider"], "anthropic");
    assert!(!out.stdout.contains("sk-ant-test"), "{}", out.stdout);
}

#[test]
fn concurrent_connect_processes_preserve_both_credentials_after_reopening() {
    use rune_core::paths::{self, Paths};
    use std::process::Stdio;
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("scratch home");
    let home = dir.path().to_str().expect("utf8");
    let state = dir.path().join("state");
    let resolved = Paths::resolve(
        Some(home),
        None,
        None,
        None,
        Some(state.to_str().expect("utf8")),
    );
    paths::create_dir_private(&resolved.state_root).expect("state");
    let lock = paths::lock_private(&resolved.credentials_lock()).expect("hold lock");

    let mut children = Vec::new();
    for (provider, variable, value) in [
        ("anthropic", "ANTHROPIC_API_KEY", "scratch-anthropic"),
        ("opencode", "OPENCODE_API_KEY", "scratch-opencode"),
    ] {
        // Each profile has its own selection config; credentials share the
        // state root. This isolates credential concurrency from config writes.
        children.push(
            Command::new(binary())
                .args(["connect", provider, "--json"])
                .env_clear()
                .env("HOME", dir.path())
                .env("RUNE_HOME", &state)
                .env("XDG_CONFIG_HOME", dir.path().join(provider))
                .env(variable, value)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("connect process"),
        );
    }
    std::thread::sleep(Duration::from_millis(200));
    let blocked: Vec<_> = children
        .iter_mut()
        .map(|child| child.try_wait().expect("status").is_none())
        .collect();
    // A writer must read only after acquiring the lock, or this entry is lost.
    let body = serde_json::json!({
        "version": 1,
        "entries": { "holder": { "value": "scratch-holder" } }
    });
    paths::write_private_atomic(&resolved.credentials_file(), &body.to_string())
        .expect("holder update");
    drop(lock);
    for child in children {
        let output = child.wait_with_output().expect("connect result");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        blocked.into_iter().all(|waiting| waiting),
        "connect bypassed the held lock"
    );

    let reopened = Paths::resolve(
        Some(home),
        None,
        None,
        None,
        Some(state.to_str().expect("utf8")),
    );
    let providers = rune_net::auth::stored_providers(&reopened).expect("reopen");
    assert_eq!(providers, ["anthropic", "holder", "opencode"]);
    let stored: serde_json::Value = serde_json::from_str(
        &paths::read_private(
            &reopened.credentials_file(),
            rune_net::auth::MAX_CREDENTIAL_FILE_BYTES,
        )
        .expect("read")
        .expect("file"),
    )
    .expect("complete JSON");
    for (provider, value) in [
        ("anthropic", "scratch-anthropic"),
        ("opencode", "scratch-opencode"),
        ("holder", "scratch-holder"),
    ] {
        assert_eq!(stored["entries"][provider]["value"], value);
    }
}

#[cfg(unix)]
#[test]
fn connecting_in_a_fresh_home_creates_private_state_and_opens_a_session() {
    use std::os::unix::fs::PermissionsExt as _;

    // Set the umask in a child shell so parallel tests keep their own mask.
    for mask in ["022", "002"] {
        for xdg in [false, true] {
            let dir = tempfile::tempdir().expect("fresh home");
            let state = dir.path().join(if xdg {
                "state/rune"
            } else {
                ".local/state/rune"
            });
            let command = || {
                let mut command = Command::new("sh");
                command
                    .args(["-c", r#"umask "$1"; shift; exec "$@""#, "rune-test", mask])
                    .arg(binary())
                    .env_clear()
                    .env("HOME", dir.path())
                    .current_dir(dir.path());
                if xdg {
                    command.env("XDG_STATE_HOME", dir.path().join("state"));
                }
                command
            };
            assert!(!state.exists(), "the home already has Rune state");

            let connected = command()
                .args(["connect", "anthropic", "--json"])
                .env("ANTHROPIC_API_KEY", "sk-ant-test")
                .output()
                .expect("connect");
            assert!(
                connected.status.success(),
                "umask {mask}, xdg {xdg}: {}",
                String::from_utf8_lossy(&connected.stderr)
            );
            for (path, mode) in [
                (state.clone(), 0o700),
                (state.join("credentials.json"), 0o600),
            ] {
                assert_eq!(
                    std::fs::metadata(&path)
                        .expect("metadata")
                        .permissions()
                        .mode()
                        & 0o777,
                    mode,
                    "{} with umask {mask}, xdg {xdg}",
                    path.display()
                );
            }

            // No key in the environment: the session must use the stored one.
            // EOF ends it after startup, without needing a live provider.
            let session = command()
                .args(["--offline", "--model", "claude-test"])
                .stdin(std::process::Stdio::null())
                .output()
                .expect("open session");
            assert!(
                session.status.success(),
                "umask {mask}, xdg {xdg}: {}",
                String::from_utf8_lossy(&session.stderr)
            );
            assert!(
                String::from_utf8_lossy(&session.stdout).contains("session "),
                "the session was not announced"
            );
            let sessions: Vec<_> = std::fs::read_dir(state.join("sessions"))
                .expect("sessions directory")
                .collect::<Result<_, _>>()
                .expect("session entries");
            assert_eq!(sessions.len(), 1, "one session must have opened");
            let session_dir = camino::Utf8PathBuf::from_path_buf(sessions[0].path()).expect("utf8");
            let saved = rune_session::store::load_read_only(&session_dir).expect("session log");
            assert!(
                saved.events.iter().any(|frame| matches!(
                    &frame.event,
                    rune_session::event::SessionEvent::WorkspaceSet { .. }
                )),
                "the session did not record its workspace"
            );
        }
    }
}

#[test]
fn a_flag_the_command_does_not_take_is_refused() {
    let out = run(&["models", "--nonsense"]);
    assert_eq!(out.status, Some(1));
    assert!(
        out.stderr.contains("not a flag of `rune models`"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("hint:"), "{}", out.stderr);
}
