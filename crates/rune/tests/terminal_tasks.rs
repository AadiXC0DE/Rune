//! `/tasks` lists a command the model left running and stops it, and the
//! model's plan is pinned above the prompt until Ctrl+T hides it.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::process::Stdio;

use rune_testkit::scripted::{ScriptedProvider, Turn};

#[test]
fn tasks_lists_and_stops_a_command_and_the_plan_is_pinned() {
    let plan = serde_json::json!({
        "plan": [
            { "step": "read the code", "status": "completed" },
            { "step": "write the code", "status": "in_progress" },
            { "step": "run the tests", "status": "pending" },
        ],
    });
    let provider = ScriptedProvider::start(vec![
        Turn::call(
            "call-1",
            "shell",
            r#"{"action":"run","command":"sleep 30","yield_time_ms":100}"#,
        ),
        Turn::text("Started."),
        Turn::call("call-2", "update_plan", &plan.to_string()),
        Turn::text("Planned."),
    ]);
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_tasks.py")),
            env!("CARGO_BIN_EXE_rune"),
            &provider.base_url(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("python3 is required for the Unix terminal tasks test");
    assert!(
        output.status.success(),
        "terminal tasks failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(provider.requests().len(), 4);
}
