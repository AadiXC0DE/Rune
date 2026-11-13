//! `/copy`, `/export` and `/diff` through the real binary and a PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

fn scenario(name: &str) {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_copy.py")),
            env!("CARGO_BIN_EXE_rune"),
            name,
        ])
        .output()
        .expect("python3 is required for the Unix copy test");
    assert!(
        output.status.success(),
        "copy scenario {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_code_block_is_copied_over_osc52() {
    scenario("copy");
}

#[test]
fn tmux_also_gets_the_passthrough_form() {
    scenario("tmux");
}

#[test]
fn export_writes_markdown_and_refuses_an_existing_file() {
    scenario("export");
}

#[test]
fn diff_opens_the_session_changes_in_the_viewer() {
    scenario("diff");
}
