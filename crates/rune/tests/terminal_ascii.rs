//! R-072 acceptance through the real binary's status, menus, tools and transcript.

#![cfg(unix)]
#![allow(clippy::expect_used)]

#[test]
fn ascii_setting_uses_ascii_decorations_in_terminal_sessions() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_ascii.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix ASCII terminal test");
    assert!(
        output.status.success(),
        "ASCII terminal acceptance failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
