//! Typing `@` offers ranked workspace files that respect `.gitignore`, and the
//! chosen path reaches the model as text.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use rune_testkit::scripted::{ScriptedProvider, Turn};

#[test]
fn an_at_mention_inserts_a_ranked_workspace_path() {
    let provider = ScriptedProvider::start(vec![Turn::text("Read it.")]);
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_mention.py")),
            env!("CARGO_BIN_EXE_rune"),
            &provider.base_url(),
        ])
        .output()
        .expect("python3 is required for the Unix terminal mention test");
    assert!(
        output.status.success(),
        "terminal mention failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).expect("request JSON");
    let user = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .expect("user message")
        .to_string();
    assert!(user.contains("look at @src/main.rs"), "{user}");
    assert!(
        !user.contains("fn main"),
        "the file must not be attached: {user}"
    );
}
