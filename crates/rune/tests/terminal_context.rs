//! After one turn, `/context` shows the window by part and `/cost` names the
//! model that served it.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use rune_testkit::scripted::{ScriptedProvider, Turn};

#[test]
fn context_and_cost_views_after_one_turn() {
    let provider = ScriptedProvider::start(vec![Turn::text("First.")]);
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_context.py")),
            env!("CARGO_BIN_EXE_rune"),
            &provider.base_url(),
        ])
        .output()
        .expect("python3 is required for the Unix terminal context test");
    assert!(
        output.status.success(),
        "terminal context failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(provider.requests().len(), 1);
}
