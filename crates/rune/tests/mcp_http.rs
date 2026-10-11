//! R-164: interactive HTTP MCP discovery, dispatch, trust and offline gating.

#![cfg(unix)]
#![allow(clippy::expect_used)]

#[test]
fn interactive_http_mcp_obeys_trust_policy_and_offline_mode() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("mcp_http.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the interactive MCP fixture");
    assert!(
        output.status.success(),
        "HTTP MCP regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
