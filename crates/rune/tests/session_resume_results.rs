//! A saved `rune ask` run keeps its retained tool results in the session directory.
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

use std::process::{Command, Stdio};

use rune_testkit::scripted::{ScriptedProvider, Turn};

#[test]
fn a_spilled_shell_result_is_written_to_the_session_directory() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let workspace = std::fs::canonicalize(&workspace).expect("canonical workspace");
    let provider = ScriptedProvider::start(vec![
        Turn::call(
            "call_1",
            "shell",
            r#"{"action":"run","command":"head -c 20000 /dev/zero | tr '\\0' x"}"#,
        ),
        Turn::text("Done."),
    ]);

    let mut command = Command::new(env!("CARGO_BIN_EXE_rune"));
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    // The shell keeps the whole output, and the turn's smaller result budget
    // sends it to the store behind a handle.
    let output = command
        .env("HOME", root.join("home"))
        .env("RUNE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("RUNE_PROVIDER", "chat_completions")
        .env("RUNE_BASE_URL", provider.base_url())
        .env("RUNE_MODEL", "scripted")
        .env("RUNE_API_KEY_ENV", "RUNE_SCENARIO_KEY")
        .env("RUNE_SCENARIO_KEY", "scenario-key")
        .env("RUNE_LIMITS", "max_turn_result_bytes=4096")
        .args(["--permission-mode", "full-access", "--allow-unsandboxed"])
        .args(["ask", "--json", "--", "Print the output."])
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .output()
        .expect("the binary to run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr:\n{stderr}");
    let sent = provider.requests();
    let result = sent.last().map_or("", |request| request.body.as_str());
    assert!(result.contains("retained; handle result-"), "{result}");

    let sessions: Vec<_> = std::fs::read_dir(root.join("state").join("sessions"))
        .expect("a sessions directory")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let results = sessions[0].join("results");
    let index = std::fs::read_to_string(results.join("index.jsonl")).expect("an index");
    let line: serde_json::Value =
        serde_json::from_str(index.lines().next().expect("one line")).expect("a JSON line");
    assert_eq!(line["tool"], "shell", "{index}");
    let handle = line["handle"].as_str().expect("a handle");
    let stored = std::fs::read_to_string(results.join(format!("{handle}.txt"))).expect("content");
    assert!(
        stored.contains(&"x".repeat(20_000)),
        "{} bytes stored",
        stored.len()
    );
}
