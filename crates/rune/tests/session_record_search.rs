//! Search original session records, including archived compaction input.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::process::{Command, Output};

use camino::Utf8PathBuf;
use rune_core::id::{EventSeq, SessionId};
use rune_core::paths::{Paths, write_private};
use rune_session::{EVENTS_FILE, EventFrame, SessionEvent, SessionStore};
use serde_json::{Value, json};

struct Fixture {
    _temp: tempfile::TempDir,
    root: Utf8PathBuf,
    paths: Paths,
    id: SessionId,
    frames: Vec<EventFrame>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root =
            Utf8PathBuf::from_path_buf(temp.path().canonicalize().expect("root")).expect("utf8");
        let paths = Paths {
            state_root: root.join("state"),
            config_root: root.join("config/rune"),
            data_root: root.join("data/rune"),
        };
        let id = "recordsrch01".parse().expect("id");
        let store = SessionStore::create(&paths, &id).expect("session");
        let events = vec![
            SessionEvent::WorkspaceSet {
                workspace: root.to_string(),
            },
            SessionEvent::TitleSet {
                title: "metadata-only".into(),
            },
            SessionEvent::UserMessage {
                text: "original Café[x].*\nsecond line".into(),
            },
            SessionEvent::TurnStarted { turn: 1 },
            SessionEvent::ToolCall {
                call_id: "identifier-only".into(),
                name: "read_file".into(),
                arguments: "{\"path\":\"Café[x].*\"}".into(),
            },
            SessionEvent::ToolResult {
                call_id: "identifier-only".into(),
                ok: true,
                output: format!("Café[x].*\n{}\nEND", "full result\n".repeat(200)),
            },
            SessionEvent::AssistantMessage {
                turn: 1,
                text: "original Café[x].*\nsecond line".into(),
            },
            SessionEvent::TurnFinished { turn: 1 },
            SessionEvent::Compaction {
                through: 8,
                summary: "Earlier work was summarized.".into(),
            },
            SessionEvent::UserMessage {
                text: "retained request".into(),
            },
            SessionEvent::Compaction {
                through: 10,
                summary: "A later handoff summary.".into(),
            },
            SessionEvent::AssistantDelta {
                turn: 2,
                text: "stream fragment".into(),
            },
        ];
        for event in events {
            store.append(event).expect("append");
        }
        drop(store);
        let frames = rune_session::load_read_only(&paths.session_dir(&id))
            .expect("read")
            .events;
        Self {
            _temp: temp,
            root,
            paths,
            id,
            frames,
        }
    }

    fn log(&self) -> Utf8PathBuf {
        self.paths.session_dir(&self.id).join(EVENTS_FILE)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rune"))
            .env_clear()
            .env("HOME", &self.root)
            .env("RUNE_HOME", &self.paths.state_root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .current_dir(&self.root)
            .arg("session")
            .args(args)
            .output()
            .expect("search process")
    }

    fn json(&self, selector: &str, query: &str) -> Value {
        let out = self.run(&[selector, "--search", query, "--json"]);
        assert!(out.status.success(), "{out:?}");
        assert!(out.stderr.is_empty(), "{out:?}");
        serde_json::from_slice(&out.stdout).expect("search JSON")
    }
}

#[test]
fn original_records_and_positions_survive_repeated_compactions_without_mutation() {
    let fixture = Fixture::new();
    let before = std::fs::read(fixture.log()).expect("log");
    let expected: Vec<_> = [2, 4, 5, 6]
        .into_iter()
        .map(|i| &fixture.frames[i])
        .collect();
    let found = fixture.json(fixture.id.as_str(), "café[x].*");
    assert_eq!(found["session_id"], fixture.id.as_str());
    assert_eq!(found["query"], "café[x].*");
    assert_eq!(found["matches"], json!(expected));
    assert_eq!(found["truncated_at"], Value::Null);
    assert_eq!(fixture.json("last", "café[x].*"), found);
    let text = fixture.run(&[fixture.id.as_str(), "--search", "café[x].*"]);
    assert!(text.status.success(), "{text:?}");
    assert!(text.stderr.is_empty(), "{text:?}");
    let stdout = String::from_utf8(text.stdout).expect("text");
    assert!(stdout.contains("4 matching record(s)"));
    for frame in expected {
        assert!(stdout.contains(&format!(
            "record {}: {}",
            frame.seq.0,
            frame.encode().expect("encode")
        )));
    }
    assert_eq!(std::fs::read(fixture.log()).expect("unchanged"), before);
}

#[test]
fn search_covers_live_records_and_summaries_and_excludes_metadata_and_identifiers() {
    let fixture = Fixture::new();
    for (query, index) in [
        ("read_file", 4),
        ("retained request", 9),
        ("handoff", 10),
        ("stream fragment", 11),
    ] {
        assert_eq!(
            fixture.json("last", query)["matches"],
            json!([fixture.frames[index]])
        );
    }
    for query in ["absent symbol", "metadata-only", "identifier-only"] {
        assert_eq!(fixture.json("last", query)["matches"], json!([]));
    }
    let no_matches = fixture.run(&["last", "--search", "absent symbol"]);
    assert!(no_matches.status.success(), "{no_matches:?}");
    assert!(
        String::from_utf8(no_matches.stdout)
            .expect("text")
            .contains("0 matching record(s)")
    );
}

#[test]
fn last_is_workspace_scoped_and_exact_id_overrides_the_selector() {
    let fixture = Fixture::new();
    let other = "recordsrch02".parse().expect("id");
    let store = SessionStore::create(&fixture.paths, &other).expect("other session");
    store
        .append(SessionEvent::WorkspaceSet {
            workspace: fixture.root.join("other").to_string(),
        })
        .expect("workspace");
    store
        .append(SessionEvent::UserMessage {
            text: "Café[x].* elsewhere".into(),
        })
        .expect("prompt");
    let expected = fixture.json("last", "café[x].*");
    assert_eq!(expected["session_id"], fixture.id.as_str());
    let out = fixture.run(&[
        other.as_str(),
        "--id",
        fixture.id.as_str(),
        "--search",
        "café[x].*",
        "--json",
    ]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).expect("JSON"),
        expected
    );
}

#[test]
fn invalid_queries_and_corruption_fail_while_a_torn_tail_warns() {
    let fixture = Fixture::new();
    for query in ["", " \n\t "] {
        let out = fixture.run(&["last", "--search", query]);
        assert!(!out.status.success(), "{out:?}");
        assert!(
            String::from_utf8(out.stderr)
                .expect("error")
                .contains("search text is empty")
        );
    }
    let incompatible = fixture.run(&["export", "last", "--search", "symbol"]);
    assert!(!incompatible.status.success(), "{incompatible:?}");
    let original = std::fs::read_to_string(fixture.log()).expect("log");
    write_private(&fixture.log(), &format!("{original}{{\"schema\":")).expect("torn tail");
    let out = fixture.run(&[fixture.id.as_str(), "--search", "café[x].*", "--json"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8(out.stderr)
            .expect("warning")
            .contains("omitted incomplete session tail")
    );
    let found: Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(found["truncated_at"], original.len());
    assert_eq!(found["matches"].as_array().expect("matches").len(), 4);
    let corrupt = EventFrame::new(
        EventSeq(99),
        1,
        SessionEvent::UserMessage {
            text: "symbol".into(),
        },
    );
    write_private(
        &fixture.log(),
        &format!("{}\n", corrupt.encode().expect("frame")),
    )
    .expect("corrupt log");
    let out = fixture.run(&[fixture.id.as_str(), "--search", "symbol", "--json"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "partial results on corruption");
}
