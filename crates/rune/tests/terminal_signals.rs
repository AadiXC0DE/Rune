//! Every way out of an interactive session, and every pause and resume, puts
//! the terminal back as it was found.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

fn run(scenario: &str) -> serde_json::Value {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_signals.py")),
            env!("CARGO_BIN_EXE_rune"),
            scenario,
        ])
        .output()
        .expect("python3 is required for the Unix terminal signal test");
    assert!(
        output.status.success(),
        "terminal signal scenario {scenario} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("signal result")
}

fn ended_by(scenario: &str) {
    let result = run(scenario);
    let signal = result["signal"].as_i64().expect("signal");
    let status = result["status"].as_i64().expect("status");
    assert!(
        status == signal.saturating_neg() || status == signal.saturating_add(128),
        "{result}"
    );
}

#[test]
fn a_terminate_signal_restores_the_terminal() {
    ended_by("term");
}

#[test]
fn a_hang_up_restores_the_terminal() {
    ended_by("hup");
}

#[test]
fn an_interrupt_signal_restores_the_terminal() {
    ended_by("int");
}

#[test]
fn suspend_restores_and_resume_redraws() {
    let result = run("suspend");
    assert_eq!(result["status"], 0, "{result}");
}

#[test]
fn a_panic_restores_the_terminal_before_its_message() {
    run("panic");
}
