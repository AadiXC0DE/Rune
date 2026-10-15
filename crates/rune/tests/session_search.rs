//! Search stored sessions through the CLI with workspace scoping and pagination.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::process::{Command, Output};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::id::{EventSeq, SessionId};
use rune_core::paths::Paths;
use rune_session::{EventFrame, SessionEvent, SessionStore, write_log_atomic};

struct Fixture {
    _temp: tempfile::TempDir,
    root: Utf8PathBuf,
    paths: Paths,
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
        Self {
            _temp: temp,
            root,
            paths,
        }
    }

    fn session(
        &self,
        id: &str,
        workspace: Option<&Utf8Path>,
        events: Vec<SessionEvent>,
        time: u64,
    ) {
        let id: SessionId = id.parse().expect("id");
        drop(SessionStore::create(&self.paths, &id).expect("create"));
        let workspace = workspace.map(|workspace| SessionEvent::WorkspaceSet {
            workspace: workspace.to_string(),
        });
        let frames: Vec<_> = workspace
            .into_iter()
            .chain(events)
            .enumerate()
            .map(|(index, event)| {
                EventFrame::new(
                    EventSeq(u64::try_from(index).expect("index").saturating_add(1)),
                    time,
                    event,
                )
            })
            .collect();
        write_log_atomic(&self.paths.session_dir(&id), &frames).expect("fixture log");
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rune"))
            .env_clear()
            .env("HOME", &self.root)
            .env("RUNE_HOME", &self.paths.state_root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .current_dir(&self.root)
            .arg("sessions")
            .args(args)
            .output()
            .expect("sessions process")
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut args = args.to_vec();
        args.push("--json");
        let out = self.run(&args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        assert!(out.stderr.is_empty(), "{out:?}");
        serde_json::from_slice(&out.stdout).expect("listing JSON")
    }
}

fn title(text: &str) -> SessionEvent {
    SessionEvent::TitleSet { title: text.into() }
}

fn prompt(text: &str) -> SessionEvent {
    SessionEvent::UserMessage { text: text.into() }
}

fn ids(value: &serde_json::Value) -> Vec<&str> {
    value["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|row| row["id"].as_str().expect("session id"))
        .collect()
}

#[test]
fn title_and_later_prompt_fragments_preserve_workspace_and_child_exclusions() {
    let fixture = Fixture::new();
    let matching = || {
        vec![
            title("Café parser repair"),
            prompt("Opening request"),
            prompt("Later request: fix lexer [x].* here\nand keep the newline"),
        ]
    };
    fixture.session("searchlocal1", Some(&fixture.root), matching(), 1);
    fixture.session(
        "searchother1",
        Some(&fixture.root.join("elsewhere")),
        matching(),
        2,
    );
    fixture.session("searchlegacy", None, matching(), 3);
    let mut child = matching();
    child.push(SessionEvent::ChildOf {
        parent: "searchlocal1".into(),
    });
    fixture.session("searchchild1", Some(&fixture.root), child, 4);
    fixture.session(
        "searchreply1",
        Some(&fixture.root),
        vec![
            title("unrelated"),
            SessionEvent::AssistantMessage {
                turn: 1,
                text: "assistant-only Café [x].*".into(),
            },
            SessionEvent::ToolResult {
                call_id: "call_1".into(),
                ok: true,
                output: "tool-only Café [x].*".into(),
            },
        ],
        5,
    );
    fixture.session(
        "searchrename",
        Some(&fixture.root),
        vec![title("obsolete title"), title("current title")],
        6,
    );

    let local_id = "searchlocal1".parse().expect("local id");
    let dir = fixture.paths.session_dir(&local_id);
    let before = std::fs::read(dir.join("events.jsonl")).expect("original log");
    for query in ["CAFÉ", "lexer [x].*", "here\nand keep"] {
        assert_eq!(
            ids(&fixture.json(&["--search", query])),
            ["searchlocal1"],
            "query: {query}"
        );
        let all = fixture.json(&["--search", query, "--all"]);
        assert_eq!(ids(&all), ["searchlegacy", "searchother1", "searchlocal1"]);
        assert!(all["next"].is_null());
    }
    for query in [
        "assistant-only",
        "tool-only",
        "obsolete title",
        "no such prompt",
    ] {
        assert!(ids(&fixture.json(&["--search", query, "--all"])).is_empty());
    }
    assert_eq!(
        ids(&fixture.json(&["--search=current title"])),
        ["searchrename"]
    );
    let out = fixture.run(&["--search", "café"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).expect("text listing");
    assert!(text.contains("searchlocal1"));
    assert!(text.contains("Café parser repair"));
    assert!(!text.contains("searchother1"));
    assert_eq!(
        std::fs::read(dir.join("events.jsonl")).expect("unchanged log"),
        before
    );
}

#[test]
fn filtering_precedes_the_listing_cap_and_pages_matching_sessions_once_in_activity_order() {
    let fixture = Fixture::new();
    fixture.session(
        "searchpage01",
        Some(&fixture.root),
        vec![title("needle")],
        1,
    );
    fixture.session(
        "searchpage02",
        Some(&fixture.root),
        vec![prompt("needle")],
        2,
    );
    fixture.session(
        "searchpage03",
        Some(&fixture.root),
        vec![title("needle"), prompt("needle"), prompt("needle")],
        3,
    );
    for index in 0..55_u64 {
        fixture.session(
            &format!("search{index:06}"),
            Some(&fixture.root),
            vec![title("unrelated"), prompt("irrelevant")],
            index.saturating_add(100),
        );
    }
    let first = fixture.json(&["--search", "needle", "--limit", "2"]);
    assert_eq!(ids(&first), ["searchpage03", "searchpage02"]);
    let cursor = first["next"].as_str().expect("next cursor");
    assert_eq!(cursor, "searchpage02");
    let second = fixture.json(&["--search", "needle", "--limit", "2", "--cursor", cursor]);
    assert_eq!(ids(&second), ["searchpage01"]);
    assert!(second["next"].is_null());
    let ordinary = fixture.json(&["--limit", "100"]);
    assert_eq!(ids(&ordinary).len(), 50, "ordinary listing cap changed");
}

#[test]
fn missing_or_empty_search_text_and_cursors_outside_the_results_fail() {
    let fixture = Fixture::new();
    fixture.session(
        "searchvalid1",
        Some(&fixture.root),
        vec![title("needle")],
        1,
    );
    fixture.session("searchmiss01", Some(&fixture.root), vec![title("other")], 2);
    fixture.session(
        "searchother1",
        Some(&fixture.root.join("elsewhere")),
        vec![title("needle")],
        3,
    );
    for args in [
        vec!["--search"],
        vec!["--search", ""],
        vec!["--search", "   "],
        vec!["--search", "needle", "--cursor", "searchmiss01"],
        vec!["--search", "needle", "--cursor", "searchother1"],
    ] {
        let out = fixture.run(&args);
        assert!(!out.status.success(), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        assert!(!out.stderr.is_empty(), "{out:?}");
    }
}
