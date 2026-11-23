//! A `!` line runs a command whose output rides on the next prompt, and a
//! `#` line records a note in `AGENTS.md` that the model hears about.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::Stdio;

use rune_testkit::scripted::{ScriptedProvider, Turn};

fn last_user_message(body: &str) -> String {
    let body: serde_json::Value = serde_json::from_str(body).expect("request JSON");
    let messages = body["messages"].as_array().expect("messages");
    messages
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .expect("a user message")
        .to_string()
}

#[test]
fn bang_output_and_hash_notes_reach_the_next_prompt() {
    let provider = ScriptedProvider::start(vec![Turn::text("First."), Turn::text("Second.")]);
    let mut child = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_bang.py")),
            env!("CARGO_BIN_EXE_rune"),
            &provider.base_url(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 is required for the Unix terminal bang test");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("read the fixture's signal");
    if line.trim() == "ROW" {
        // The command ran without a request: nothing reaches the model yet.
        assert_eq!(provider.request_count(), 0);
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, "go").expect("signal the fixture");
    }
    let output = child.wait_with_output().expect("fixture finished");
    assert!(
        output.status.success() && line.trim() == "ROW",
        "terminal bang failed:\n{line}{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let first = last_user_message(&requests[0].body);
    for expected in [
        "$ echo BANG-OUT",
        "BANG-OUT\\n[exit 0]",
        "what did it print",
    ] {
        assert!(first.contains(expected), "missing {expected:?} in {first}");
    }
    let second = last_user_message(&requests[1].body);
    assert!(
        second.contains("The user added a note to AGENTS.md: always use tabs"),
        "{second}"
    );
    assert!(!second.contains("BANG-OUT"), "{second}");
}
