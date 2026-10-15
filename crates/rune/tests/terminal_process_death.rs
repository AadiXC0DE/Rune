//! R-058 regression through SIGKILL of the real binary and interactive resume.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

#[test]
fn sigkill_preserves_a_replayable_partial_answer_without_completing_the_turn() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_process_death.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix process death regression");
    assert!(
        output.status.success(),
        "terminal process death regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
