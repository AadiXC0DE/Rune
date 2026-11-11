//! The startup terminal query through the real binary in a PTY: answered,
//! unanswered, typed over, and skipped.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

fn run(scenario: &str) -> serde_json::Value {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_probe.py")),
            env!("CARGO_BIN_EXE_rune"),
            scenario,
        ])
        .output()
        .expect("python3 is required for the Unix terminal probe test");
    assert!(
        output.status.success(),
        "terminal probe scenario {scenario} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("probe result")
}

fn first_frame(result: &serde_json::Value) -> Vec<u8> {
    result["first"]
        .as_array()
        .expect("first frame bytes")
        .iter()
        .map(|byte| u8::try_from(byte.as_u64().expect("byte")).expect("byte"))
        .collect()
}

#[test]
fn status_names_what_the_terminal_answered() {
    let result = run("answered");
    assert_eq!(
        result["status"],
        "terminal       FakeTerm(1.0); background #ffffff (light); synchronized output yes; \
         kitty keyboard 1; tmux no; ssh no"
    );
}

#[test]
fn a_silent_terminal_delays_the_first_frame_only_briefly() {
    let result = run("silent");
    let status = result["status"].as_str().expect("status");
    assert!(
        status.starts_with(
            "terminal       unknown; background unknown; synchronized output unknown; \
             kitty keyboard no"
        ),
        "{status}"
    );
    let frame = String::from_utf8_lossy(&first_frame(&result)).into_owned();
    assert!(frame.contains("ctrl-c cance"), "{frame}");
}

#[test]
fn text_typed_during_the_probe_is_the_first_draft() {
    let result = run("typed");
    let bytes = first_frame(&result);
    let mut grid = rune_term::Grid::new(160, 24).expect("grid");
    grid.feed(&bytes).expect("first frame");
    assert_eq!(grid.row_text(grid.cursor().row), "> hello");
    let prompt = bytes
        .windows(2)
        .position(|pair| pair == b"> ")
        .expect("a prompt row");
    assert!(bytes[prompt..].starts_with(b"> hello"), "{bytes:?}");
}

#[test]
fn nothing_is_asked_when_the_probe_is_off_or_cannot_be_answered() {
    for scenario in ["disabled", "accessible", "stdin-pipe", "stdout-pipe"] {
        let result = run(scenario);
        if let Some(status) = result["status"].as_str() {
            assert!(
                status.contains("terminal       unknown; "),
                "{scenario}: {status}"
            );
        }
    }
}
