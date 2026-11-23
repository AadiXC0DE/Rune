//! `/undo` takes back a file a command created in the last turn, `/undo
//! preview` lists it first, and `/redo` puts it back.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use rune_testkit::scripted::{ScriptedProvider, Turn};

#[test]
fn undo_and_redo_restore_a_command_made_file() {
    let git = std::process::Command::new("git").arg("--version").output();
    if !git.is_ok_and(|output| output.status.success()) {
        eprintln!("skipped: git is not installed");
        return;
    }
    let provider = ScriptedProvider::start(vec![
        Turn::call(
            "call_1",
            "shell",
            r#"{"action": "run", "command": "echo x > made-by-command"}"#,
        ),
        Turn::text("Made."),
    ]);
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_undo.py")),
            env!("CARGO_BIN_EXE_rune"),
            &provider.base_url(),
        ])
        .output()
        .expect("python3 is required for the Unix terminal undo test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("RESTORED"),
        "terminal undo failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(provider.request_count(), 2);
}
