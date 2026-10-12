//! R-170: only an explicit /goal creates a persistent session objective.
#![allow(clippy::expect_used)]

use std::io::Write as _;
use std::process::{Command, Stdio};

use camino::Utf8PathBuf;
use rune_core::id::SessionId;
use rune_core::paths::Paths;
use rune_session::{EVENTS_FILE, SessionEvent, SessionStore};

struct Fixture {
    _dir: tempfile::TempDir,
    root: Utf8PathBuf,
    paths: Paths,
    id: SessionId,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("workspace");
        let root =
            Utf8PathBuf::from_path_buf(dir.path().canonicalize().expect("root")).expect("utf8");
        let paths = Paths {
            state_root: root.join("state"),
            config_root: root.join("config/rune"),
            data_root: root.join("data/rune"),
        };
        let id = SessionId::generate();
        let store = SessionStore::create(&paths, &id).expect("session");
        store
            .append(SessionEvent::WorkspaceSet {
                workspace: root.to_string(),
            })
            .expect("workspace");
        std::fs::write(root.join("fixture.toml"),
            "provider = \"chat_completions\"\nbase_url = \"http://127.0.0.1:1/v1\"\n[models.chat_completions]\nid = \"fixture-model\"\n")
            .expect("config");
        Self {
            _dir: dir,
            root,
            paths,
            id,
        }
    }

    fn run(&self, input: &str) -> String {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
            .env_clear()
            .env("HOME", &self.root)
            .env("RUNE_HOME", &self.paths.state_root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("RUNE_CONFIG", self.root.join("fixture.toml"))
            .env("OPENAI_API_KEY", "fixture-key")
            .current_dir(&self.root)
            .args(["--offline", "--accessible", "resume", self.id.as_str()])
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
            .expect("commands");
        let output = child.wait_with_output().expect("output");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).expect("stdout")
    }

    fn log(&self) -> Vec<u8> {
        std::fs::read(self.paths.session_dir(&self.id).join(EVENTS_FILE)).expect("log")
    }
}

#[test]
fn an_explicit_goal_is_identical_after_process_restart_and_queries_do_not_write() {
    let fixture = Fixture::new();
    let objective = "Fix café parsing with \"quoted\" inputs and 日本語";
    let first = fixture.run(&format!("/goal {objective}\n/quit\n"));
    let expected = format!("Notice: active objective: {objective}");
    assert!(first.lines().any(|line| line == expected), "{first}");
    let state =
        rune_session::load_read_only(&fixture.paths.session_dir(&fixture.id)).expect("state");
    assert_eq!(state.turns, 0, "setting a goal must not run a model turn");
    assert_eq!(
        state.events.last().expect("goal event").event,
        SessionEvent::ObjectiveSet {
            objective: objective.to_owned()
        }
    );
    std::fs::remove_file(fixture.paths.session_dir(&fixture.id).join("session.json"))
        .expect("remove disposable metadata");
    let before = fixture.log();
    let restarted = fixture.run("/goal\n/goal\n/quit\n");
    assert_eq!(
        restarted.lines().filter(|line| *line == expected).count(),
        2,
        "{restarted}"
    );
    assert_eq!(fixture.log(), before, "inspection must not append events");

    let fresh = fixture.run("/new\n/goal\n/quit\n");
    assert!(fresh.contains("Notice: no active objective"), "{fresh}");
}

#[test]
fn an_ordinary_prompt_creates_no_objective_even_when_it_mentions_a_goal() {
    let fixture = Fixture::new();
    let output = fixture.run("Please set a goal to fix this parser\n/goal\n/quit\n");
    assert!(output.contains("Notice: no active objective"), "{output}");
    let state =
        rune_session::load_read_only(&fixture.paths.session_dir(&fixture.id)).expect("state");
    assert!(state.events.iter().any(|frame| matches!(&frame.event,
        SessionEvent::UserMessage { text } if text == "Please set a goal to fix this parser")));
    assert!(
        state
            .events
            .iter()
            .all(|frame| !matches!(frame.event, SessionEvent::ObjectiveSet { .. }))
    );
    let before = fixture.log();
    let restarted = fixture.run("/goal\n/quit\n");
    assert!(
        restarted.contains("Notice: no active objective"),
        "{restarted}"
    );
    assert_eq!(fixture.log(), before);
}

#[test]
fn an_explicit_budget_is_persisted_displayed_and_enforced_by_the_cli() {
    let fixture = Fixture::new();
    let first = fixture.run("/goal --budget 30 Fix café parsing\n/quit\n");
    assert!(
        first.contains("Notice: objective tokens: 0/30; 30 remaining"),
        "{first}"
    );
    let state =
        rune_session::load_read_only(&fixture.paths.session_dir(&fixture.id)).expect("state");
    assert_eq!(
        state.events.last().expect("budget event").event,
        SessionEvent::ObjectiveBudgetSet {
            objective: "Fix café parsing".to_owned(),
            token_budget: 30,
        }
    );
    let before = fixture.log();
    let invalid = fixture.run("/goal --budget 0 work\n/goal --budget 30\n/quit\n");
    assert!(
        invalid.contains("usage: /goal --budget <positive tokens> <objective>"),
        "{invalid}"
    );
    assert_eq!(
        fixture.log(),
        before,
        "invalid budgets must not replace the goal"
    );
    for used in [15, 30] {
        let store = SessionStore::open(&fixture.paths.session_dir(&fixture.id)).expect("store");
        store
            .append(SessionEvent::ObjectiveTokensRecorded { tokens: 15 })
            .expect("fixture usage");
        drop(store);
        let before = fixture.log();
        let restarted = fixture.run("/goal\n/quit\n");
        assert!(
            restarted.contains(&format!(
                "Notice: objective tokens: {used}/30; {} remaining",
                30 - used
            )),
            "{restarted}"
        );
        assert_eq!(fixture.log(), before, "queries must not change accounting");
    }
    let stopped = fixture.run("continue\n/quit\n");
    assert!(
        stopped.contains("exhausted the objective token budget"),
        "{stopped}"
    );
    let state =
        rune_session::load_read_only(&fixture.paths.session_dir(&fixture.id)).expect("state");
    assert!(
        state
            .events
            .iter()
            .all(|frame| !matches!(frame.event, SessionEvent::TurnFailed { .. })),
        "the budget must stop before the offline provider refusal"
    );
    let replaced = fixture.run("/goal --budget 20 New objective\n/quit\n");
    assert!(
        replaced.contains("Notice: objective tokens: 0/20; 20 remaining"),
        "{replaced}"
    );
}
