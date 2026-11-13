//! R-163: real interactive CLI, local model, and trusted stdio MCP fixtures.

#![cfg(unix)]
#![allow(clippy::expect_used)]

mod support;

#[test]
fn interactive_stdio_mcp_obeys_trust_policy_and_lifecycle_bounds() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(concat!(
                include_str!("fixture_support.py"),
                "\n",
                include_str!("mcp_stdio.py")
            )),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the interactive MCP fixture");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "stdio MCP regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
