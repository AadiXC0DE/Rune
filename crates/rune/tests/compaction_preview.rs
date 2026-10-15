//! Manual compaction previews through the real CLI leave saved records intact.
#![allow(clippy::expect_used)]

use std::io::Write as _;
use std::process::{Command, Stdio};

use camino::Utf8PathBuf;
use rune_core::paths::Paths;
use rune_session::{EVENTS_FILE, SessionEvent, SessionStore};

fn preview_fixture(exchanges: u64, previous_summary: bool, result_limit: usize, expected: &str) {
    let dir = tempfile::tempdir().expect("workspace");
    let root = Utf8PathBuf::from_path_buf(dir.path().canonicalize().expect("root")).expect("utf8");
    let paths = Paths {
        state_root: root.join("state"),
        config_root: root.join("config/rune"),
        data_root: root.join("data/rune"),
    };
    let id = "previewctx01".parse().expect("id");
    let store = SessionStore::create(&paths, &id).expect("session");
    store
        .append(SessionEvent::WorkspaceSet {
            workspace: root.to_string(),
        })
        .expect("workspace");
    if previous_summary {
        store
            .append(SessionEvent::UserMessage {
                text: "archived request".into(),
            })
            .expect("old prompt");
        store
            .append(SessionEvent::Compaction {
                through: 2,
                summary: "Previous conversation handoff summary.".into(),
            })
            .expect("previous compaction");
    }
    for turn in 1..=exchanges {
        store
            .append(SessionEvent::UserMessage {
                text: format!("question {turn}"),
            })
            .expect("prompt");
        store
            .append(SessionEvent::TurnStarted { turn })
            .expect("start");
        // Multiple calls and results form one assistant and one tool-result
        // record. Preview counts conversation records, not parts or log events.
        for tool in 1..=2 {
            store
                .append(SessionEvent::ToolCall {
                    call_id: format!("call_{turn}_{tool}"),
                    name: "read_file".into(),
                    arguments: "{\"path\":\"fixture.txt\"}".into(),
                })
                .expect("call");
        }
        for tool in 1..=2 {
            store
                .append(SessionEvent::ToolResult {
                    call_id: format!("call_{turn}_{tool}"),
                    ok: true,
                    output: format!("contents {turn}/{tool}"),
                })
                .expect("result");
        }
        store
            .append(SessionEvent::AssistantMessage {
                turn,
                text: format!("answer {turn}"),
            })
            .expect("answer");
        store
            .append(SessionEvent::TurnFinished { turn })
            .expect("finish");
    }
    drop(store);
    let log = paths.session_dir(&id).join(EVENTS_FILE);
    let before = std::fs::read(&log).expect("log before");
    let config = root.join("preview.toml");
    std::fs::write(&config, format!(
        "provider = \"chat_completions\"\nbase_url = \"http://127.0.0.1:1/v1\"\n[models.chat_completions]\nid = \"fixture-model\"\n[limits]\nmax_tool_result_bytes = {result_limit}\n"
    )).expect("config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
        .env_clear()
        .env("HOME", &root)
        .env("RUNE_HOME", &paths.state_root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("RUNE_CONFIG", &config)
        .env("OPENAI_API_KEY", "fixture-key")
        .current_dir(&root)
        .args(["--offline", "--accessible", "--resume", id.as_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI");
    let mut input = child.stdin.take().expect("stdin");
    // Repeating the preview must produce the same counts.
    writeln!(input, "/compact preview\n/compact preview\n/quit").expect("commands");
    drop(input);
    let output = child.wait_with_output().expect("output");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    for line in
        std::iter::once("compaction preview (history unchanged)").chain(expected.split("; "))
    {
        assert_eq!(
            stdout
                .lines()
                .filter(|actual| actual.strip_prefix("Notice: ") == Some(line))
                .count(),
            2,
            "{stdout}"
        );
    }
    assert_eq!(std::fs::read(log).expect("log after"), before);
}

#[test]
fn preview_counts_respect_tool_pairs_and_configured_retention() {
    preview_fixture(
        6,
        false,
        32_768,
        "20 record(s) would be summarized; 4 record(s) retained verbatim",
    );
    preview_fixture(
        6,
        false,
        131_072,
        "16 record(s) would be summarized; 8 record(s) retained verbatim",
    );
}

#[test]
fn preview_counts_an_existing_summary_without_changing_archived_records() {
    preview_fixture(
        6,
        true,
        32_768,
        "21 record(s) would be summarized; 4 record(s) retained verbatim",
    );
}

#[test]
fn empty_and_short_histories_report_zero_records_to_summarize() {
    preview_fixture(
        0,
        false,
        131_072,
        "0 record(s) would be summarized; 0 record(s) retained verbatim",
    );
    preview_fixture(
        1,
        false,
        131_072,
        "0 record(s) would be summarized; 4 record(s) retained verbatim",
    );
}
