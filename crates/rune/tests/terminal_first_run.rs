//! The start banner's tips and `/init`, through the real binary and a PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

fn scenario(name: &str) {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_first_run.py")),
            env!("CARGO_BIN_EXE_rune"),
            name,
        ])
        .output()
        .expect("python3 is required for the Unix first run test");
    assert!(
        output.status.success(),
        "first run scenario {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn each_start_shows_the_next_tip() {
    scenario("tips");
}

#[test]
fn init_sends_the_fixed_prompt_and_echoes_the_command() {
    scenario("init");
}
