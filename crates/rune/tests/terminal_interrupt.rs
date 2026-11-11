//! Interrupting a turn stops the command it is running.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

#[test]
fn interrupting_a_turn_stops_its_running_command() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_interrupt.py")),
            env!("CARGO_BIN_EXE_rune"),
            "command",
        ])
        .output()
        .expect("python3 is required for the Unix terminal interrupt regression");
    assert!(
        output.status.success(),
        "terminal interrupt regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
