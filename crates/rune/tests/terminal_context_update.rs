//! A permission switch between prompts reaches the model as an appended
//! update, so the provider's cached prefix survives it.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use rune_testkit::scripted::{ScriptedProvider, Turn};

#[test]
fn a_permission_switch_rides_on_the_next_prompt_without_a_prefix_break() {
    let provider = ScriptedProvider::start(vec![Turn::text("First."), Turn::text("Second.")]);
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_context_update.py")),
            env!("CARGO_BIN_EXE_rune"),
            &provider.base_url(),
        ])
        .output()
        .expect("python3 is required for the Unix terminal context update test");
    assert!(
        output.status.success(),
        "terminal context update failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(provider.prefix_breaks(), 0);
    assert!(requests[1].body.contains("Permission mode is now auto."));
    let messages = |index: usize| -> Vec<serde_json::Value> {
        let body: serde_json::Value =
            serde_json::from_str(&requests[index].body).expect("request JSON");
        body["messages"].as_array().expect("messages").clone()
    };
    let (first, second) = (messages(0), messages(1));
    assert!(second.len() > first.len());
    assert_eq!(second[..first.len()], first[..]);
}
