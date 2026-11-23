//! A session that compacted and was resumed sends the same request as one that
//! never stopped.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use rune_testkit::scripted::{ScriptedProvider, Turn};

const SUMMARY: &str = "## Goal\n- read the files the user named\n## Constraints\n- none\n## Decisions\n- none\n## State\n### Done\n- the files were read\n### In progress\n- none\n### Blocked\n- none\n## Next steps\n- answer the last prompt\n## Files that matter\n- the .txt files in the workspace\n## Facts to keep\n- none";

fn turns() -> Vec<Turn> {
    let read = |index: usize| {
        Turn::call(
            &format!("c{index}"),
            "read_file",
            &format!("{{\"path\":\"f{index}.txt\"}}"),
        )
    };
    let mut turns = vec![read(1), read(2), Turn::text("Read two.")];
    turns.extend((3..=7).map(read));
    turns.push(Turn::text("Read five."));
    turns.push(Turn {
        match_text: Some("Write a checkpoint of the conversation above".to_owned()),
        ..Turn::text(SUMMARY)
    });
    turns.push(Turn::text("Last."));
    turns
}

/// Runs the session in `base` and returns the provider that served it.
fn run(base: &std::path::Path, mode: &str) -> ScriptedProvider {
    let provider = ScriptedProvider::start(turns());
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_resume_fidelity.py")),
            env!("CARGO_BIN_EXE_rune"),
            &provider.base_url(),
            base.to_str().expect("utf8 path"),
            mode,
        ])
        .output()
        .expect("python3 is required for the Unix terminal resume fidelity test");
    assert!(
        output.status.success(),
        "{mode} run failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    provider
}

/// The `messages` array of the last request, as compact JSON.
fn last_messages(provider: &ScriptedProvider) -> String {
    let requests = provider.requests();
    let last = requests.last().expect("a request");
    let body: serde_json::Value = serde_json::from_str(&last.body).expect("request JSON");
    serde_json::to_string(&body["messages"]).expect("messages")
}

#[test]
fn a_resumed_session_sends_the_request_the_live_one_would() {
    let root = tempfile::tempdir().expect("tempdir");
    // Both runs use the same paths, so the prompt that names them is the same.
    let base = root.path().join("session");
    let control = run(&base, "control");
    let resumed = run(&base, "resume");

    let requests = control.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.body.contains("<conversation-checkpoint>")),
        "the control run never compacted: request bytes {:?}",
        requests
            .iter()
            .map(|request| request.body.len())
            .collect::<Vec<_>>()
    );
    assert!(
        requests
            .iter()
            .any(|request| request.body.contains("<plan_mode>")),
        "the plan mode note never reached the model"
    );
    let summaries = |provider: &ScriptedProvider| {
        provider
            .requests()
            .iter()
            .filter(|request| {
                request
                    .body
                    .contains("Write a checkpoint of the conversation above")
            })
            .count()
    };
    assert_eq!(summaries(&control), 1);
    assert_eq!(summaries(&resumed), 1);
    assert_eq!(requests.len(), resumed.requests().len());
    let expected = last_messages(&control);
    assert!(expected.contains("\"last\""), "{expected}");
    assert_eq!(last_messages(&resumed), expected);
}
