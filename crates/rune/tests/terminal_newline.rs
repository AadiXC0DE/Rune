//! Newline keys and the keyboard protocol through the real binary in a PTY:
//! the protocol is turned on before the first frame and off on the way out.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

fn run(scenario: &str) -> serde_json::Value {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_newline.py")),
            env!("CARGO_BIN_EXE_rune"),
            scenario,
        ])
        .output()
        .expect("python3 is required for the Unix newline test");
    assert!(
        output.status.success(),
        "newline scenario {scenario} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("newline result")
}

#[test]
fn every_newline_key_reaches_the_provider_as_one_prompt() {
    let result = run("kitty");
    assert_eq!(result["prompts"], serde_json::json!(["a\nb\nc\nd\ne"]));
}

#[test]
fn tmux_gets_modify_other_keys_and_has_it_reset_on_exit() {
    run("tmux");
}

#[test]
fn terminal_setup_writes_the_binding_after_a_yes() {
    run("setup");
}
