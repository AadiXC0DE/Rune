//! R-003 regression through the real binary, a PTY, and a local provider.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

#[test]
fn terminal_permission_requests_are_displayed_and_resolved() {
    // Python's standard library supplies the PTY without adding a crate or
    // unsafe code. The fixture has deadlines and reaps its child on failure.
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_approval.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix terminal approval regression");
    assert!(
        output.status.success(),
        "terminal approval regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
