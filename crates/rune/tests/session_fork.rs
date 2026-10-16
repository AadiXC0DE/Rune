//! R-187: fork the saved journal through the actual CLI, then resume both sides.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;
use support::CommandExt as _;

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

use camino::Utf8PathBuf;
use rune_core::id::{EventSeq, SessionId};
use rune_core::paths::Paths;
use rune_session::{EVENTS_FILE, SessionEvent, SessionStore, load_read_only};

struct Fixture {
    _temp: tempfile::TempDir,
    root: Utf8PathBuf,
    paths: Paths,
    id: SessionId,
}

impl Fixture {
    fn new() -> Self {
        Self::with_id(SessionId::generate())
    }

    fn with_id(id: SessionId) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root =
            Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("root")).expect("utf8");
        let paths = Paths {
            state_root: root.join("state"),
            config_root: root.join("config/rune"),
            data_root: root.join("data/rune"),
        };
        let store = SessionStore::create(&paths, &id).expect("session");
        for event in [
            SessionEvent::WorkspaceSet {
                workspace: root.to_string(),
            },
            SessionEvent::TitleSet {
                title: "fork fixture".into(),
            },
            SessionEvent::ObjectiveSet {
                objective: "prefix goal".into(),
            },
            SessionEvent::ConstraintPinned {
                text: "preserve café".into(),
            },
            SessionEvent::TurnStarted { turn: 1 },
            SessionEvent::UserMessage {
                text: "read both files".into(),
            },
            SessionEvent::ToolCall {
                call_id: "a".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            },
            SessionEvent::ToolCall {
                call_id: "b".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            },
            SessionEvent::ToolResult {
                call_id: "a".into(),
                ok: true,
                output: "first file café".into(),
            },
            SessionEvent::ToolResult {
                call_id: "b".into(),
                ok: false,
                output: "second file absent".into(),
            },
            SessionEvent::AssistantMessage {
                turn: 1,
                text: "original future answer".into(),
            },
            SessionEvent::UsageRecorded {
                input_tokens: 30,
                output_tokens: 10,
            },
            SessionEvent::TurnFinished { turn: 1 },
            SessionEvent::ObjectiveSet {
                objective: "original future goal".into(),
            },
        ] {
            store.append(event).expect("fixture event");
        }
        std::fs::write(root.join("fixture.toml"),
            "provider = \"chat_completions\"\nbase_url = \"http://127.0.0.1:1/v1\"\n[models.chat_completions]\nid = \"fixture-model\"\n")
            .expect("config");
        Self {
            _temp: temp,
            root,
            paths,
            id,
        }
    }

    fn run(&self, args: &[&str], input: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
            .isolated_env()
            .env("HOME", &self.root)
            .env("RUNE_HOME", &self.paths.state_root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("RUNE_CONFIG", self.root.join("fixture.toml"))
            .env("OPENAI_API_KEY", "fixture-key")
            .current_dir(&self.root)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("CLI");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("input");
        child.wait_with_output().expect("output")
    }

    fn fork(&self, selector: &str, at: &str) -> Output {
        self.run(&["session", "fork", selector, "--at", at, "--json"], "")
    }

    fn files(&self, id: &SessionId) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(self.paths.session_dir(id))
            .expect("directory")
            .map(|entry| {
                let entry = entry.expect("entry");
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).expect("bytes"),
                )
            })
            .collect();
        files.sort();
        files
    }
}

fn fork_id(output: &Output) -> SessionId {
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("report");
    assert_eq!(report["through"], 10);
    report["id"]
        .as_str()
        .expect("id")
        .parse()
        .expect("session id")
}

#[test]
fn completed_tool_group_forks_and_both_sessions_resume_independently() {
    let fixture = Fixture::new();
    let workspace_file = fixture.root.join("source.txt");
    std::fs::write(&workspace_file, "original workspace bytes").expect("workspace file");
    rune_core::paths::write_private(
        &fixture
            .paths
            .session_dir(&fixture.id)
            .join("compaction-archive.json"),
        "[]",
    )
    .expect("source archive");
    // Whole-directory snapshots also read session.lock, which Windows refuses
    // while it is locked. Take both snapshots outside the writer's lifetime.
    let before = fixture.files(&fixture.id);
    // A reader can fork a durable prefix while a source writer holds its lock.
    let writer = SessionStore::open(&fixture.paths.session_dir(&fixture.id)).expect("writer");
    let source = load_read_only(&fixture.paths.session_dir(&fixture.id)).expect("source");
    let output = fixture.fork(fixture.id.as_str(), "10");
    let branch = fork_id(&output);
    assert_ne!(branch, fixture.id);
    drop(writer);
    assert_eq!(
        fixture.files(&fixture.id),
        before,
        "fork touched source files"
    );
    let copied = load_read_only(&fixture.paths.session_dir(&branch)).expect("branch");
    assert_eq!(&copied.events[..10], &source.events[..10]);
    assert_eq!(copied.events.len(), 11);
    assert_eq!(copied.events[10].seq, EventSeq(11));
    assert_eq!(
        copied.events[10].event,
        SessionEvent::ForkedFrom {
            session: fixture.id.to_string(),
            through: 10,
        }
    );
    assert_eq!(copied.title, source.title);
    assert_eq!(copied.workspace, source.workspace);
    assert!(
        !fixture
            .paths
            .session_dir(&branch)
            .join("compaction-archive.json")
            .exists()
    );
    assert_eq!(
        copied.usage.total(),
        0,
        "future usage must not be inherited"
    );

    let original_before_resume = fixture.files(&fixture.id);
    let resumed = fixture.run(
        &["--offline", "--accessible", "resume", branch.as_str()],
        "/goal\n/goal branch goal\n/quit\n",
    );
    assert!(resumed.status.success(), "{resumed:?}");
    let text = String::from_utf8(resumed.stdout).expect("stdout");
    assert!(text.contains("prefix goal"), "{text}");
    assert!(!text.contains("original future"), "{text}");
    assert!(text.contains("first file café"), "{text}");
    assert!(text.contains("second file absent"), "{text}");
    assert_eq!(fixture.files(&fixture.id), original_before_resume);
    let branch_before_resume = fixture.files(&branch);
    let original = fixture.run(
        &["--offline", "--accessible", "resume", fixture.id.as_str()],
        "/goal\n/goal main goal\n/quit\n",
    );
    assert!(original.status.success(), "{original:?}");
    let text = String::from_utf8(original.stdout).expect("stdout");
    assert!(text.contains("original future answer"), "{text}");
    assert!(text.contains("original future goal"), "{text}");
    assert_eq!(fixture.files(&branch), branch_before_resume);
    assert_eq!(
        std::fs::read_to_string(&workspace_file).expect("workspace bytes"),
        "original workspace bytes"
    );
    let main = load_read_only(&fixture.paths.session_dir(&fixture.id)).expect("main");
    let branch = load_read_only(&fixture.paths.session_dir(&branch)).expect("branch");
    assert_eq!(&main.events[..source.events.len()], source.events);
    assert_eq!(
        main.events.last().expect("main goal").event,
        SessionEvent::ObjectiveSet {
            objective: "main goal".into()
        }
    );
    assert_eq!(
        branch.events.last().expect("branch goal").event,
        SessionEvent::ObjectiveSet {
            objective: "branch goal".into()
        }
    );
}

#[test]
fn incomplete_tool_groups_and_invalid_positions_leave_no_new_session() {
    let fixture = Fixture::new();
    let before = fixture.files(&fixture.id);
    for at in ["7", "8", "9", "0", "15", "18446744073709551615", "nope"] {
        let output = fixture.fork(fixture.id.as_str(), at);
        assert!(!output.status.success(), "boundary {at}: {output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert_eq!(fixture.files(&fixture.id), before);
        assert_eq!(
            std::fs::read_dir(fixture.paths.sessions_dir())
                .expect("sessions")
                .count(),
            1
        );
    }
}

#[test]
fn last_and_exact_id_select_the_source_and_require_an_explicit_boundary() {
    let fixture = Fixture::new();
    fork_id(&fixture.fork("last", "10"));
    let exact = fixture.run(
        &[
            "session",
            "fork",
            "ignored",
            "--id",
            fixture.id.as_str(),
            "--at",
            "10",
            "--json",
        ],
        "",
    );
    fork_id(&exact);
    for args in [
        vec!["session", "fork", fixture.id.as_str()],
        vec!["session", fixture.id.as_str(), "--at", "10"],
        vec![
            "session",
            "fork",
            fixture.id.as_str(),
            "--at",
            "10",
            "--search",
            "file",
        ],
        vec![
            "session",
            "fork",
            fixture.id.as_str(),
            "--at",
            "10",
            "extra",
        ],
    ] {
        assert!(!fixture.run(&args, "").status.success(), "{args:?}");
    }
}

#[test]
fn child_sessions_and_existing_destinations_are_refused() {
    // Exercise both possible flag-shaped prefixes deterministically.
    for id in ["-fixedchild0", "--fixedchild"] {
        let fixture = Fixture::with_id(id.parse().expect("fixed id"));
        let before = fixture.files(&fixture.id);
        assert!(
            rune_session::fork::fork(&fixture.paths, &fixture.id, &fixture.id, EventSeq(10))
                .is_err()
        );
        assert_eq!(fixture.files(&fixture.id), before);
        let destination = SessionId::generate();
        std::fs::create_dir(fixture.paths.session_dir(&destination)).expect("reserve");
        assert!(
            rune_session::fork::fork(&fixture.paths, &fixture.id, &destination, EventSeq(10))
                .is_err()
        );
        assert_eq!(
            std::fs::read_dir(fixture.paths.session_dir(&destination))
                .expect("empty")
                .count(),
            0
        );
        let store = SessionStore::open(&fixture.paths.session_dir(&fixture.id)).expect("store");
        store
            .append(SessionEvent::ChildOf {
                parent: SessionId::generate().to_string(),
            })
            .expect("child");
        drop(store);
        let output = fixture.fork(fixture.id.as_str(), "10");
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8(output.stderr)
                .expect("error")
                .contains("child session")
        );
    }
}

#[test]
fn torn_tail_does_not_change_the_requested_durable_prefix_or_source_bytes() {
    let fixture = Fixture::new();
    let log = fixture.paths.session_dir(&fixture.id).join(EVENTS_FILE);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .expect("log")
        .write_all(b"{\"schema\":1,\"seq\":15")
        .expect("torn tail");
    let before = fixture.files(&fixture.id);
    fork_id(&fixture.fork(fixture.id.as_str(), "10"));
    assert_eq!(fixture.files(&fixture.id), before);
    assert!(!fixture.fork(fixture.id.as_str(), "15").status.success());
}

#[test]
fn malformed_tool_sequences_are_refused_before_a_destination_is_created() {
    use rune_session::{EventFrame, write_log_atomic};

    let fixture = Fixture::new();
    let call = |id: &str| SessionEvent::ToolCall {
        call_id: id.into(),
        name: "read_file".into(),
        arguments: "{}".into(),
    };
    let result = |id: &str| SessionEvent::ToolResult {
        call_id: id.into(),
        ok: true,
        output: "result".into(),
    };
    for events in [
        vec![result("orphan")],
        vec![call("a"), call("a"), result("a")],
        vec![
            call("a"),
            call("b"),
            result("a"),
            call("c"),
            result("b"),
            result("c"),
        ],
        vec![
            call("a"),
            SessionEvent::UserMessage {
                text: "interleaved".into(),
            },
            result("a"),
        ],
        vec![call(""), result("")],
    ] {
        let frames: Vec<_> = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| {
                EventFrame::new(EventSeq((index as u64).saturating_add(1)), 1, event)
            })
            .collect();
        write_log_atomic(&fixture.paths.session_dir(&fixture.id), &frames).expect("fixture log");
        let before = fixture.files(&fixture.id);
        let output = fixture.fork(fixture.id.as_str(), &frames.len().to_string());
        assert!(!output.status.success(), "{output:?}");
        assert_eq!(fixture.files(&fixture.id), before);
        assert_eq!(
            std::fs::read_dir(fixture.paths.sessions_dir())
                .expect("sessions")
                .count(),
            1
        );
    }
}
