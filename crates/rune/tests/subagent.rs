//! R-160 and R-199 acceptance through the real CLI and an isolated local provider.

#![allow(clippy::expect_used)]

mod support;

#[test]
fn synchronous_children_return_reads_and_cannot_escape_their_allowlist() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(concat!(
                include_str!("fixture_support.py"),
                "\n",
                include_str!("subagent.py")
            )),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the local subagent fixture");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "subagent regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
