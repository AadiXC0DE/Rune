//! The window title names the session and whether it is idle or working, the
//! progress indicator follows the turn, and both are put back on exit.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use rune_testkit::scripted::{ScriptedProvider, Turn};

fn run(title: &str) {
    let provider = ScriptedProvider::start(vec![Turn::text("First.")]);
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_title.py")),
            env!("CARGO_BIN_EXE_rune"),
            &provider.base_url(),
            title,
        ])
        .output()
        .expect("python3 is required for the Unix terminal title test");
    assert!(
        output.status.success(),
        "terminal title ({title}) failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_title_and_progress_follow_the_session() {
    run("on");
}

#[test]
fn a_disabled_title_is_never_written() {
    run("off");
}
