//! R-163: real interactive CLI, local model, and trusted stdio MCP fixtures.

#![cfg(unix)]
#![allow(clippy::expect_used)]

#[test]
fn interactive_stdio_mcp_obeys_trust_policy_and_lifecycle_bounds() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("mcp_stdio.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the interactive MCP fixture");
    assert!(
        output.status.success(),
        "stdio MCP regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
