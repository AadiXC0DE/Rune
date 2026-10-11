//! Exercise session exports through the real CLI and its stdout stream.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::process::{Command, Output};

use camino::Utf8PathBuf;
use rune_core::error::ErrorCode;
use rune_core::id::{EventSeq, SessionId};
use rune_core::paths::{Paths, write_private};
use rune_session::{EVENTS_FILE, EventFrame, SessionEvent, SessionStore, load_read_only};

struct Fixture {
    _temp: tempfile::TempDir,
    root: Utf8PathBuf,
    paths: Paths,
    id: SessionId,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonical root"))
            .expect("utf8");
        let paths = Paths {
            state_root: root.join("state"),
            config_root: root.join("config/rune"),
            data_root: root.join("data/rune"),
        };
        let id = "exporttest01".parse().expect("id");
        drop(SessionStore::create(&paths, &id).expect("create session"));
        Self {
            _temp: temp,
            root,
            paths,
            id,
        }
    }

    fn log(&self) -> Utf8PathBuf {
        self.paths.session_dir(&self.id).join(EVENTS_FILE)
    }

    fn export(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rune"))
            .env_clear()
            .env("HOME", &self.root)
            .env("RUNE_HOME", &self.paths.state_root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("ANTHROPIC_API_KEY", "environment-secret")
            .current_dir(&self.root)
            .args(["session", "export"])
            .args(args)
            .output()
            .expect("export process")
    }
}

fn frames(workspace: &str) -> Vec<EventFrame> {
    let events = vec![
        SessionEvent::WorkspaceSet {
            workspace: workspace.into(),
        },
        SessionEvent::TitleSet {
            title: "export fixture".into(),
        },
        SessionEvent::ChildOf {
            parent: "parentsess01".into(),
        },
        SessionEvent::TurnStarted { turn: 1 },
        SessionEvent::UserMessage {
            text: "line one\nline two: café 😀".into(),
        },
        SessionEvent::AssistantDelta {
            turn: 1,
            text: "retry text".into(),
        },
        SessionEvent::AssistantReset { turn: 1 },
        SessionEvent::ToolCall {
            call_id: "call_1".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"src/main.rs\"}".into(),
        },
        SessionEvent::ToolResult {
            call_id: "call_1".into(),
            ok: true,
            output: "first\nsecond\t\"quoted\"".into(),
        },
        SessionEvent::AssistantMessage {
            turn: 1,
            text: "saved answer".into(),
        },
        SessionEvent::UsageRecorded {
            input_tokens: 123,
            output_tokens: 45,
        },
        SessionEvent::TurnFinished { turn: 1 },
        SessionEvent::Compaction {
            through: 12,
            summary: "summary".into(),
        },
        SessionEvent::TurnStarted { turn: 2 },
        SessionEvent::TurnCancelled { turn: 2 },
        SessionEvent::TurnStarted { turn: 3 },
        SessionEvent::TurnFailed {
            turn: 3,
            code: ErrorCode::Internal,
            message: "fixture failure".into(),
        },
        SessionEvent::TurnStarted { turn: 4 },
        SessionEvent::TurnInterrupted { turn: 4 },
        SessionEvent::TurnStarted { turn: 5 },
        SessionEvent::AssistantDelta {
            turn: 5,
            text: "unfinished reply".into(),
        },
    ];
    events
        .into_iter()
        .enumerate()
        .map(|(index, event)| {
            let seq = u64::try_from(index).expect("index").saturating_add(1);
            EventFrame::new(EventSeq(seq), 1_000_u64.saturating_add(seq), event)
        })
        .collect()
}

#[test]
fn repeated_exports_preserve_every_saved_event_and_exclude_transport_and_credentials() {
    let fixture = Fixture::new();
    let expected = frames(fixture.root.as_str());
    let mut raw = String::new();
    for frame in &expected {
        let mut value = serde_json::to_value(frame).expect("frame JSON");
        value["credentials"] = serde_json::json!({ "api_key": "frame-secret" });
        value["headers"] = serde_json::json!({ "Authorization": "Bearer header-secret" });
        value["event"]["transport_headers"] = serde_json::json!({ "x-api-key": "event-secret" });
        raw.push_str(
            &serde_json::to_string_pretty(&value)
                .expect("pretty frame")
                .replace('\n', " "),
        );
        raw.push('\n');
    }
    write_private(&fixture.log(), &raw).expect("write fixture");
    rune_net::auth::store(&fixture.paths, "anthropic", "stored-secret").expect("store credential");
    // The export reads the authority, even when the disposable projection is corrupt.
    write_private(
        &fixture.paths.session_dir(&fixture.id).join("session.json"),
        "corrupt projection-secret",
    )
    .expect("metadata");

    let before = std::fs::read(fixture.log()).expect("original log");
    let first = fixture.export(&[fixture.id.as_str()]);
    let second = fixture.export(&[fixture.id.as_str(), "--json"]);
    for out in [&first, &second] {
        assert!(out.status.success(), "{out:?}");
        assert!(out.stderr.is_empty(), "{out:?}");
    }
    assert_eq!(first.stdout, second.stdout);
    let text = String::from_utf8(first.stdout).expect("utf8 export");
    let actual: Vec<_> = text
        .lines()
        .map(|line| EventFrame::decode(line).expect("valid exported frame"))
        .collect();
    assert_eq!(actual, expected);
    assert!(text.ends_with('\n'));
    assert!(!text.contains("secret"));
    assert!(!text.contains("headers"));
    assert!(!text.contains("credentials"));
    assert_eq!(
        text.lines().nth(3).expect("turn frame"),
        r#"{"schema":1,"seq":4,"timestamp_ms":1004,"event":{"kind":"turn_started","turn":1}}"#
    );
    assert_eq!(std::fs::read(fixture.log()).expect("unchanged log"), before);
}

#[test]
fn last_is_workspace_scoped_and_id_flag_takes_precedence() {
    let fixture = Fixture::new();
    let expected = [EventFrame::new(
        EventSeq::FIRST,
        1,
        SessionEvent::WorkspaceSet {
            workspace: fixture.root.to_string(),
        },
    )];
    rune_session::write_log_atomic(&fixture.paths.session_dir(&fixture.id), &expected)
        .expect("fixture log");
    let other = "exporttest02".parse().expect("other id");
    let store = SessionStore::create(&fixture.paths, &other).expect("other session");
    store
        .append(SessionEvent::WorkspaceSet {
            workspace: fixture.root.join("elsewhere").to_string(),
        })
        .expect("other workspace");
    let exact = fixture.export(&[fixture.id.as_str()]);
    for args in [
        vec!["last"],
        vec!["--id", fixture.id.as_str()],
        vec![other.as_str(), "--id", fixture.id.as_str()],
    ] {
        let out = fixture.export(&args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert!(out.stderr.is_empty(), "{out:?}");
        assert_eq!(out.stdout, exact.stdout, "{args:?}");
    }
}

#[test]
fn empty_export_is_empty_and_torn_tail_warns_without_inventing_events() {
    let fixture = Fixture::new();
    let empty = fixture.export(&[fixture.id.as_str()]);
    assert!(empty.status.success(), "{empty:?}");
    assert!(empty.stdout.is_empty());
    assert!(empty.stderr.is_empty());

    let line = "{\"schema\":1,\"seq\":1,\"timestamp_ms\":1,\"event\":{\"kind\":\"turn_started\",\"turn\":1}}\n";
    write_private(&fixture.log(), &format!("{line}{{\"schema\":")).expect("write torn log");
    let out = fixture.export(&[fixture.id.as_str()]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, line.as_bytes());
    assert!(
        String::from_utf8(out.stderr)
            .expect("stderr")
            .contains("omitted incomplete session tail")
    );
}

#[test]
fn invalid_selectors_and_corrupt_logs_fail_without_partial_stdout() {
    let fixture = Fixture::new();
    for args in [vec![], vec!["invalid"], vec!["missingtest1"], vec!["last"]] {
        let out = fixture.export(&args);
        assert!(!out.status.success(), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        assert!(!out.stderr.is_empty(), "{out:?}");
    }
    let first = EventFrame::new(EventSeq::FIRST, 1, SessionEvent::TurnStarted { turn: 1 });
    for second in [
        EventFrame::new(EventSeq(3), 2, SessionEvent::TurnFinished { turn: 1 }),
        EventFrame {
            schema: 99,
            seq: EventSeq(2),
            timestamp_ms: 2,
            event: SessionEvent::TurnFinished { turn: 1 },
        },
    ] {
        write_private(
            &fixture.log(),
            &format!(
                "{}\n{}\n",
                first.encode().expect("encode"),
                serde_json::to_string(&second).expect("JSON")
            ),
        )
        .expect("corrupt log");
        let out = fixture.export(&[fixture.id.as_str()]);
        assert!(!out.status.success(), "{out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        assert!(!out.stderr.is_empty(), "{out:?}");
    }
}

#[test]
fn export_reports_output_write_failures() {
    let fixture = Fixture::new();
    let store = SessionStore::open(&fixture.paths.session_dir(&fixture.id)).expect("open");
    store
        .append(SessionEvent::TurnStarted { turn: 1 })
        .expect("append");
    let state = load_read_only(store.dir()).expect("read");
    let mut output = [0_u8; 1];
    let err = state
        .export_jsonl(output.as_mut_slice())
        .expect_err("output too small");
    assert_eq!(err.code(), ErrorCode::TransportFailure);
}
