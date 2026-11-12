//! `?` shows the keys and `/help` opens in the viewer, through the real
//! binary and a PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

#[test]
fn the_key_overlay_closes_on_any_key_and_help_opens_in_the_viewer() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_help_overlay.py")),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix help overlay test");
    assert!(
        output.status.success(),
        "help overlay failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
