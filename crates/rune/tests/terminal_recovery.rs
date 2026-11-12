//! A failed turn offers its recovery: Enter retries, a rate limit counts
//! down, and a refused credential points at `/connect`, through the real
//! binary and a PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

fn scenario(name: &str) {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_recovery.py")),
            env!("CARGO_BIN_EXE_rune"),
            name,
        ])
        .output()
        .expect("python3 is required for the Unix recovery test");
    assert!(
        output.status.success(),
        "recovery scenario {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn enter_retries_a_failed_turn() {
    scenario("retry");
}

#[test]
fn a_rate_limit_counts_down_and_escape_interrupts() {
    scenario("rate");
}

#[test]
fn a_refused_credential_points_at_connect() {
    scenario("auth");
}
