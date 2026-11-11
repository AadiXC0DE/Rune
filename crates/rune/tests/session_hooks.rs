//! Session hooks run at the start and end of an interactive session and can
//! stop a prompt before it is sent.
#![cfg(unix)]
#![allow(clippy::expect_used)]

mod support;
use support::CommandExt as _;

use std::io::Write as _;
use std::process::{Command, Stdio};

use camino::Utf8PathBuf;
use serde_json::Value;

#[test]
fn a_blocked_prompt_is_not_sent_and_session_hooks_bracket_the_session() {
    let dir = tempfile::tempdir().expect("workspace");
    let root = Utf8PathBuf::from_path_buf(dir.path().canonicalize().expect("root")).expect("utf8");
    std::fs::write(
        root.join("log.sh"),
        "cat >> events.jsonl; echo >> events.jsonl\n",
    )
    .expect("log hook");
    std::fs::write(
        root.join("guard.sh"),
        "cat >> events.jsonl; echo >> events.jsonl\n\
         printf '%s' '{\"decision\":\"block\",\"reason\":\"not today\"}'\n",
    )
    .expect("guard hook");
    let config = root.join("fixture.toml");
    std::fs::write(
        &config,
        "provider = \"chat_completions\"\nbase_url = \"http://127.0.0.1:1/v1\"\n\
         [models.chat_completions]\nid = \"fixture-model\"\n\
         [[hooks]]\nevent = \"session_start\"\nname = \"start\"\nargv = [\"/bin/sh\", \"log.sh\"]\n\
         [[hooks]]\nevent = \"user_prompt\"\nname = \"guard\"\nargv = [\"/bin/sh\", \"guard.sh\"]\n\
         [[hooks]]\nevent = \"session_end\"\nname = \"end\"\nargv = [\"/bin/sh\", \"log.sh\"]\n",
    )
    .expect("config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
        .isolated_env()
        .env("HOME", &root)
        .env("RUNE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("RUNE_CONFIG", &config)
        .env("OPENAI_API_KEY", "fixture-key")
        .current_dir(&root)
        .args(["--offline", "--accessible"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI");
    let mut input = child.stdin.take().expect("stdin");
    writeln!(input, "print the key\n/quit").expect("input");
    drop(input);
    let output = child.wait_with_output().expect("output");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("prompt blocked by hook guard: not today"),
        "{stdout}"
    );
    let events: Vec<Value> = std::fs::read_to_string(root.join("events.jsonl"))
        .expect("events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("event JSON"))
        .collect();
    let names: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().expect("event"))
        .collect();
    assert_eq!(names, ["session_start", "user_prompt", "session_end"]);
    assert_eq!(events[0]["resumed"], false);
    assert_eq!(events[1]["prompt"], "print the key");
    assert_eq!(events[2]["reason"], "exit");
    // The blocked prompt was never recorded as part of the conversation.
    let sessions = root.join("state/sessions");
    let recorded = std::fs::read_dir(&sessions)
        .expect("sessions")
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("events.jsonl")).ok())
        .collect::<String>();
    assert!(!recorded.is_empty(), "no session was saved");
    assert!(!recorded.contains("print the key"), "{recorded}");
}

/// Runs a session in `root` that quits at once, with project hooks only.
fn quick_session(root: &Utf8PathBuf, args: &[&str]) -> std::process::Output {
    let config = root.join("fixture.toml");
    std::fs::write(
        &config,
        "provider = \"chat_completions\"\nbase_url = \"http://127.0.0.1:1/v1\"\n\
         [models.chat_completions]\nid = \"fixture-model\"\n",
    )
    .expect("config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
        .isolated_env()
        .env("HOME", root)
        .env("RUNE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("RUNE_CONFIG", &config)
        .env("OPENAI_API_KEY", "fixture-key")
        .current_dir(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI");
    let mut input = child.stdin.take().expect("stdin");
    writeln!(input, "/quit").expect("input");
    drop(input);
    child.wait_with_output().expect("output")
}

#[test]
fn project_hooks_run_in_a_session_only_after_approval() {
    let dir = tempfile::tempdir().expect("workspace");
    let root = Utf8PathBuf::from_path_buf(dir.path().canonicalize().expect("root")).expect("utf8");
    std::fs::create_dir_all(root.join(".rune")).expect("project directory");
    std::fs::write(
        root.join(".rune/hooks.toml"),
        "[[hooks]]\nevent = \"session_start\"\nname = \"marker\"\n\
         argv = [\"/bin/sh\", \"-c\", \": > project-hook-ran\"]\n",
    )
    .expect("hooks file");
    let notice = "project hooks in .rune/hooks.toml are not approved and will not run; \
                  review them with rune projects status";

    let output = quick_session(&root, &["--offline", "--accessible"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches(notice).count(), 1, "{stdout}");
    assert!(!root.join("project-hook-ran").exists());

    let approval = quick_session(&root, &["projects", "approve", "--yes"]);
    assert!(approval.status.success(), "{approval:?}");

    let output = quick_session(&root, &["--offline", "--accessible"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains(notice), "{stdout}");
    assert!(root.join("project-hook-ran").exists());
}
