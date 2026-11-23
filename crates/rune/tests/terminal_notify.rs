//! A turn that ends while the terminal is unfocused raises a notification
//! through the terminal's own channel; a focused terminal gets none, and the
//! focus reports are turned on at start and off on exit.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use rune_testkit::scripted::{ScriptedProvider, Turn};

fn run(setting: &str) {
    let provider = ScriptedProvider::start(vec![Turn::text("First."), Turn::text("Second.")]);
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_notify.py")),
            env!("CARGO_BIN_EXE_rune"),
            &provider.base_url(),
            setting,
        ])
        .output()
        .expect("python3 is required for the Unix terminal notify test");
    assert!(
        output.status.success(),
        "terminal notify ({setting}) failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_unfocused_terminal_is_notified_and_a_focused_one_is_not() {
    run("auto");
}

#[test]
fn notify_off_sends_nothing_even_unfocused() {
    run("off");
}
