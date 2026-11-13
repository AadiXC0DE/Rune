//! A session resumed after its instruction file changed opens with the bytes
//! it started with, so the provider's cached prefix survives the resume.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use rune_testkit::scripted::{ScriptedProvider, Turn};

#[test]
fn a_resume_after_an_edit_keeps_the_cached_prefix() {
    let provider = ScriptedProvider::start(vec![Turn::text("First."), Turn::text("Second.")]);
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_cache_resume.py")),
            env!("CARGO_BIN_EXE_rune"),
            &provider.base_url(),
        ])
        .output()
        .expect("python3 is required for the Unix terminal cache resume test");
    assert!(
        output.status.success(),
        "terminal cache resume failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(provider.prefix_breaks(), 0);
    assert!(requests[1].body.contains("AGENTS.md changed on disk"));
    let messages = |index: usize| -> Vec<serde_json::Value> {
        let body: serde_json::Value =
            serde_json::from_str(&requests[index].body).expect("request JSON");
        body["messages"].as_array().expect("messages").clone()
    };
    let (first, second) = (messages(0), messages(1));
    assert!(second.len() > first.len());
    assert_eq!(second[..first.len()], first[..]);
}
