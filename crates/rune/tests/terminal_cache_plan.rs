//! Entering and leaving plan mode between prompts changes no earlier byte of
//! the conversation, so the provider's cached prefix survives both.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use rune_testkit::scripted::{ScriptedProvider, Turn};

#[test]
fn plan_mode_on_and_off_keeps_the_cached_prefix() {
    let provider = ScriptedProvider::start(vec![
        Turn::text("First."),
        Turn::text("Second."),
        Turn::text("Third."),
    ]);
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_cache_plan.py")),
            env!("CARGO_BIN_EXE_rune"),
            &provider.base_url(),
        ])
        .output()
        .expect("python3 is required for the Unix terminal plan cache test");
    assert!(
        output.status.success(),
        "terminal plan cache failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(provider.prefix_breaks(), 0);
    let messages = |index: usize| -> Vec<serde_json::Value> {
        let body: serde_json::Value =
            serde_json::from_str(&provider.requests()[index].body).expect("request JSON");
        body["messages"].as_array().expect("messages").clone()
    };
    for later in 1..3 {
        let (earlier, now) = (messages(later - 1), messages(later));
        assert_eq!(now[..earlier.len()], earlier[..], "request {}", later + 1);
    }
}
