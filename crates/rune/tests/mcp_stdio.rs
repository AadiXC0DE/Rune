//! R-163: real interactive CLI, local model, and trusted stdio MCP fixtures.

#![cfg(unix)]
#![allow(clippy::expect_used)]

mod support;

/// Runs one selection of the Python fixture against the built binary.
fn run_fixture(selection: &str) {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("mcp_stdio.py")),
            env!("CARGO_BIN_EXE_rune"),
            selection,
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

#[test]
fn interactive_stdio_mcp_obeys_trust_policy_and_lifecycle_bounds() {
    run_fixture("interactive");
}

#[test]
fn a_large_server_is_deferred_behind_mcp_call() {
    run_fixture("large");
}

#[test]
fn ask_connects_trusted_mcp_servers() {
    run_fixture("ask");
}
