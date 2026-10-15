//! R-024 regression through the real binary, a PTY, and a local provider.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

#[test]
fn cancellation_saves_the_visible_answer_and_one_boundary_across_resume() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_cancellation.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix terminal cancellation regression");
    assert!(
        output.status.success(),
        "terminal cancellation regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
