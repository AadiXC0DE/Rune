//! R-153: /cost restores saved usage before making another provider request.
#![allow(clippy::expect_used)]

mod support;
use support::CommandExt as _;

use std::io::Write as _;
use std::process::{Command, Stdio};

use camino::Utf8PathBuf;
use rune_core::error::ErrorCode;
use rune_core::paths::Paths;
use rune_session::{EVENTS_FILE, SessionEvent, SessionStore};

fn saved_cost(events: Vec<SessionEvent>, selector: &str, expected: &str) {
    let dir = tempfile::tempdir().expect("workspace");
    let root = Utf8PathBuf::from_path_buf(dir.path().canonicalize().expect("root")).expect("utf8");
    let paths = Paths {
        state_root: root.join("state"),
        config_root: root.join("config/rune"),
        data_root: root.join("data/rune"),
    };
    let id = "savedcost001".parse().expect("id");
    let store = SessionStore::create(&paths, &id).expect("session");
    store
        .append(SessionEvent::WorkspaceSet {
            workspace: root.to_string(),
        })
        .expect("workspace");
    for event in events {
        store.append(event).expect("fixture event");
    }
    // Usage must come from the journal, even if its disposable projection is lost.
    std::fs::remove_file(store.metadata_path()).expect("remove projection");
    drop(store);
    let log = paths.session_dir(&id).join(EVENTS_FILE);
    let before = std::fs::read(&log).expect("log before");
    let config = root.join("fixture.toml");
    std::fs::write(&config,
        "provider = \"chat_completions\"\nbase_url = \"http://127.0.0.1:1/v1\"\n[models.chat_completions]\nid = \"fixture-model\"\n")
        .expect("config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
        .isolated_env()
        .env("HOME", &root)
        .env("RUNE_HOME", &paths.state_root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("RUNE_CONFIG", &config)
        .env("OPENAI_API_KEY", "fixture-key")
        .current_dir(&root)
        .args(["--offline", "--accessible", "resume", selector])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI");
    let mut input = child.stdin.take().expect("stdin");
    writeln!(input, "/cost\n/cost\n/quit").expect("commands");
    drop(input);
    let output = child.wait_with_output().expect("output");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout");
    for line in expected.lines() {
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

fn three_turns(legacy: bool) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    for turn in 1_u64..=3 {
        events.extend([
            SessionEvent::UserMessage {
                text: format!("question {turn}"),
            },
            SessionEvent::TurnStarted { turn },
            SessionEvent::AssistantMessage {
                turn,
                text: format!("answer {turn}"),
            },
            SessionEvent::UsageRecorded {
                input_tokens: turn.saturating_mul(100),
                output_tokens: turn.saturating_mul(10),
            },
        ]);
        if !legacy {
            events.push(SessionEvent::TurnFinished { turn });
        }
    }
    events
}

#[test]
fn resumed_three_turn_session_reports_saved_requests_and_tokens() {
    saved_cost(
        three_turns(false),
        "last",
        "Requests: 3\nTokens:   600 in, 60 out",
    );
}

#[test]
fn legacy_usage_survives_resume_and_compaction_without_finish_markers() {
    let mut events = three_turns(true);
    events.push(SessionEvent::Compaction {
        through: 9,
        summary: "Earlier exchanges.".into(),
    });
    saved_cost(
        events,
        "savedcost001",
        "Requests: 3\nTokens:   600 in, 60 out",
    );
}

#[test]
fn completed_requests_without_usage_count_but_failed_attempts_do_not() {
    let mut events = three_turns(false);
    events.extend([
        SessionEvent::TurnStarted { turn: 4 },
        SessionEvent::TurnFinished { turn: 4 },
        SessionEvent::TurnStarted { turn: 5 },
        SessionEvent::UsageRecorded {
            input_tokens: 0,
            output_tokens: 0,
        },
        SessionEvent::TurnFinished { turn: 5 },
        SessionEvent::TurnStarted { turn: 6 },
        SessionEvent::AssistantMessage {
            turn: 6,
            text: "cancelled partial".into(),
        },
        SessionEvent::TurnCancelled { turn: 6 },
        SessionEvent::TurnStarted { turn: 7 },
        SessionEvent::AssistantMessage {
            turn: 7,
            text: "failed partial".into(),
        },
        SessionEvent::TurnFailed {
            turn: 7,
            code: ErrorCode::IncompleteStream,
            message: "cut".into(),
        },
        SessionEvent::TurnStarted { turn: 8 },
        SessionEvent::AssistantDelta {
            turn: 8,
            text: "partial".into(),
        },
        SessionEvent::AssistantMessage {
            turn: 8,
            text: "died before finish".into(),
        },
    ]);
    saved_cost(events, "last", "Requests: 5\nTokens:   600 in, 60 out");
}

#[test]
fn empty_resumed_session_still_reports_no_requests() {
    saved_cost(
        Vec::new(),
        "last",
        "no requests have been made in this session yet",
    );
}

#[test]
fn legacy_completed_requests_without_usage_still_count() {
    let events = three_turns(true)
        .into_iter()
        .filter(|event| !matches!(event, SessionEvent::UsageRecorded { .. }))
        .collect();
    saved_cost(events, "last", "Requests: 3\nTokens:   0 in, 0 out");
}
