//! Exercise session exports through the real CLI and its stdout stream.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;
use support::CommandExt as _;

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
            .isolated_env()
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
    use std::error::Error as _;

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
    assert_eq!(err.code(), ErrorCode::IoFailure);
    assert_eq!(
        err.source()
            .expect("output write cause")
            .downcast_ref::<std::io::Error>()
            .expect("typed output write cause")
            .kind(),
        std::io::ErrorKind::WriteZero
    );
}

/// A run killed mid-turn keeps every step it completed, because `ask` writes
/// the session as it goes.
#[cfg(unix)]
#[test]
fn a_killed_ask_run_keeps_its_completed_steps() {
    use std::io::BufRead as _;
    use std::process::Stdio;

    use rune_testkit::scripted::{ScriptedProvider, Turn};

    let temp = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("canonical root"))
        .expect("utf8");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("a.txt"), "alpha\n").expect("a.txt");
    std::fs::write(workspace.join("b.txt"), "beta\n").expect("b.txt");
    let mut slow = Turn::text("never");
    slow.delay_ms = 30_000;
    let provider = ScriptedProvider::start(vec![
        Turn::call("c1", "read_file", r#"{"path":"a.txt"}"#),
        Turn::call("c2", "read_file", r#"{"path":"b.txt"}"#),
        slow,
    ]);
    let command = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rune"));
        command
            .isolated_env()
            .env("HOME", root.join("home"))
            .env("RUNE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("xdg-state"))
            .env("RUNE_PROVIDER", "chat_completions")
            .env("RUNE_BASE_URL", provider.base_url())
            .env("RUNE_MODEL", "scripted")
            .env("RUNE_API_KEY_ENV", "RUNE_KILL_TEST_KEY")
            .env("RUNE_KILL_TEST_KEY", "kill-test-key")
            .current_dir(&workspace)
            .args(args);
        command
    };
    let mut child = command(&[
        "--permission-mode",
        "full-access",
        "--allow-unsandboxed",
        "ask",
        "--jsonl",
        "--",
        "Read a.txt and b.txt.",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .expect("ask");

    // Kept open until the kill: a closed pipe would fail the run's next write.
    let mut lines = std::io::BufReader::new(child.stdout.take().expect("stdout")).lines();
    let mut session_id = String::new();
    let mut results = 0;
    for line in lines.by_ref() {
        let record: serde_json::Value =
            serde_json::from_str(&line.expect("a line")).expect("a record");
        if record["type"] == "start" {
            session_id = record["session_id"].as_str().expect("an id").to_owned();
        }
        if record["type"] == "tool_result" {
            results += 1;
            if results == 2 {
                break;
            }
        }
    }
    assert_eq!(results, 2);
    assert!(!session_id.is_empty());
    // The stream reports a result as the tool finishes and the log records it
    // just after, so wait for the third request, which follows both.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while provider.request_count() < 3 {
        assert!(std::time::Instant::now() < deadline, "no third request");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // The third request waits 30 s; the run is killed while it does.
    child.kill().expect("kill");
    child.wait().expect("reaped");
    drop(lines);

    let export = command(&["session", "export", &session_id])
        .output()
        .expect("export");
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let text = String::from_utf8_lossy(&export.stdout);
    for expected in ["\"c1\"", "\"c2\"", "alpha", "beta"] {
        assert!(text.contains(expected), "{expected} missing from:\n{text}");
    }
    assert!(!text.contains("turn_failed"), "{text}");
}
