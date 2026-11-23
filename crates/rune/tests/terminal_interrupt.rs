//! Interrupting a turn stops the command it is running, and the keys that
//! interrupt or clear never lose typed text.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

fn scenario(name: &str) {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_interrupt.py")),
            env!("CARGO_BIN_EXE_rune"),
            name,
        ])
        .output()
        .expect("python3 is required for the Unix terminal interrupt regression");
    assert!(
        output.status.success(),
        "terminal interrupt regression {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn interrupting_a_turn_stops_its_running_command() {
    scenario("command");
}

#[test]
fn one_escape_on_an_empty_draft_interrupts_and_keeps_the_partial_answer() {
    scenario("esc");
}

#[test]
fn escape_over_typed_text_needs_a_second_press_and_keeps_the_text() {
    scenario("esc-with-text");
}

#[test]
fn control_c_at_an_idle_prompt_needs_two_quick_presses_to_leave() {
    scenario("ctrl-c-idle");
}

#[test]
fn a_draft_cleared_with_control_c_comes_back_with_up() {
    scenario("recall");
}
