//! R-160 and R-199 acceptance through the real CLI and an isolated local provider.

#![allow(clippy::expect_used)]

#[test]
fn synchronous_children_return_reads_and_cannot_escape_their_allowlist() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("subagent.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the local subagent fixture");
    assert!(
        output.status.success(),
        "subagent regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
