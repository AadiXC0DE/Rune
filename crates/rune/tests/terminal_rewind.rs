//! Rewinding to before an earlier message, through the real binary and a PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

fn scenario(name: &str) {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_rewind.py")),
            env!("CARGO_BIN_EXE_rune"),
            name,
        ])
        .output()
        .expect("python3 is required for the Unix rewind test");
    assert!(
        output.status.success(),
        "rewind scenario {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_double_escape_rewinds_and_the_edited_message_is_resent() {
    scenario("edit");
}

#[test]
fn rewinding_with_the_files_restores_them_after_a_preview() {
    scenario("files");
}
