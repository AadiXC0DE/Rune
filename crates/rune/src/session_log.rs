//! Recording a session as it runs, and reading one back.
//!
//! The log is the durable form of a conversation. It is written as the turn
//! runs, so a session that is interrupted can still be resumed, and the
//! conversation is rebuilt from the log rather than from a second copy kept in
//! memory.

use std::fmt::Write as _;

use camino::Utf8PathBuf;
use rune_agent::history::History;
use rune_agent::turn::TurnOutcome;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::SessionId;
use rune_core::paths::Paths;
use rune_net::message::ContentPart;
use rune_session::event::SessionEvent;
use rune_session::store::{SessionState, SessionStore, load_read_only};

/// Most sessions listed in one command.
pub const LIST_LIMIT: usize = 50;

/// Longest derived title, in characters.
pub const TITLE_CHARS: usize = 60;

/// Derives a title from the opening prompt.
///
/// The first line only, so a pasted block does not become the title.
#[must_use]
pub fn derive_title(prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "untitled".to_owned();
    }
    let mut title: String = first.chars().take(TITLE_CHARS).collect();
    if first.chars().count() > TITLE_CHARS {
        title.push_str("...");
    }
    title
}

/// One session as a listing shows it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Summary {
    /// Session identifier.
    pub id: String,
    /// Turn number of the last event, when the log holds one.
    pub turns: u64,
    /// Events recorded.
    pub events: usize,
    /// Last activity, as an ISO 8601 timestamp.
    pub updated_at: Option<String>,
    /// Session title, when one was set.
    pub title: Option<String>,
    /// Directory holding the log.
    pub dir: Utf8PathBuf,
}

/// An open session log.
#[derive(Debug)]
pub struct Recorder {
    store: SessionStore,
    turn: u64,
}

impl Recorder {
    /// Creates a new session and its log.
    pub fn create(paths: &Paths, id: &SessionId) -> Result<Self> {
        let store = SessionStore::create(paths, id)?;
        Ok(Self { store, turn: 0 })
    }

    /// Opens an existing session, preparing to append to it.
    pub fn open(paths: &Paths, id: &SessionId) -> Result<Self> {
        let store = SessionStore::open(&paths.session_dir(id))?;
        let turn = store.turns();
        Ok(Self { store, turn })
    }

    /// Returns the session identifier.
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.store.id()
    }

    /// Records the user's message.
    pub fn user_message(&self, text: &str) -> Result<()> {
        self.store.append(SessionEvent::UserMessage {
            text: text.to_owned(),
        })?;
        Ok(())
    }

    /// Records a turn and everything it produced.
    ///
    /// The turn number is assigned here rather than by the caller, so a turn
    /// that produced no output is still numbered in the order it ran.
    pub fn turn(&mut self, outcome: &TurnOutcome) -> Result<()> {
        self.turn = self.turn.saturating_add(1);
        let turn = self.turn;
        self.store.append(SessionEvent::TurnStarted { turn })?;

        // The calls of one step are a batch, and their results follow the whole
        // batch. Writing them interleaved would leave a log that cannot be
        // replayed into the conversation the model actually saw.
        for call in &outcome.calls {
            self.store.append(SessionEvent::ToolCall {
                call_id: call.call.id.clone(),
                name: call.call.name.clone(),
                arguments: call.call.arguments.clone(),
            })?;
        }
        for call in &outcome.calls {
            self.store.append(SessionEvent::ToolResult {
                call_id: call.call.id.clone(),
                ok: !call.output.is_error,
                output: call.output.text.clone(),
            })?;
        }

        if !outcome.text.is_empty() {
            self.store.append(SessionEvent::AssistantMessage {
                turn,
                text: outcome.text.clone(),
            })?;
        }

        if let (Some(input), Some(output)) =
            (outcome.usage.input_tokens, outcome.usage.output_tokens)
        {
            self.store.append(SessionEvent::UsageRecorded {
                input_tokens: input,
                output_tokens: output,
            })?;
        }
        Ok(())
    }

    /// Returns true when the session has no title yet.
    #[must_use]
    pub fn title_is_unset(&self) -> bool {
        self.store.title().is_none()
    }

    /// Records the session title.
    ///
    /// Called once, with the opening prompt, so a listing distinguishes
    /// sessions by what they were about rather than by identifier alone.
    pub fn set_title(&self, title: &str) -> Result<()> {
        self.store.append(SessionEvent::TitleSet {
            title: title.to_owned(),
        })?;
        Ok(())
    }
}

/// Lists stored sessions for a workspace, most recently active first.
pub fn list(paths: &Paths) -> Vec<Summary> {
    let Ok(entries) = std::fs::read_dir(paths.sessions_dir()) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(dir) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        let Ok(state) = load_read_only(&dir) else {
            continue;
        };
        out.push(summarize(&state, dir));
    }

    // An unreadable or absent timestamp sorts last rather than first, so a
    // damaged session never displaces a usable one.
    out.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    out.truncate(LIST_LIMIT);
    out
}

/// Returns the most recently active session, when there is one.
pub fn latest(paths: &Paths) -> Option<Summary> {
    list(paths).into_iter().next()
}

/// Reads a stored session into a conversation.
pub fn load(paths: &Paths, id: &SessionId) -> Result<(Recorder, History)> {
    let dir = paths.session_dir(id);
    if !dir.exists() {
        return Err(
            RuneError::new(ErrorCode::NotFound, format!("no session `{id}` was found"))
                .with_hint("list sessions to see what is stored"),
        );
    }

    let state = load_read_only(&dir)?;
    let history = history_from(&state);
    let recorder = Recorder::open(paths, id)?;
    Ok((recorder, history))
}

/// Builds a conversation from a stored log.
#[must_use]
pub fn history_from(state: &SessionState) -> History {
    let mut history = History::new();
    let mut calls: Vec<ContentPart> = Vec::new();
    let mut results: Vec<ContentPart> = Vec::new();

    for frame in &state.events {
        match &frame.event {
            SessionEvent::UserMessage { text } => {
                flush(&mut history, &mut calls, &mut results);
                history.push_user(text.clone());
            }
            SessionEvent::AssistantMessage { text, .. } => {
                flush(&mut history, &mut calls, &mut results);
                if !text.is_empty() {
                    history.push_assistant(vec![ContentPart::Text { text: text.clone() }]);
                }
            }
            SessionEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                // A tool result run always follows the calls it answers, so a
                // new call closes the previous group.
                if !results.is_empty() {
                    flush(&mut history, &mut calls, &mut results);
                }
                if let Ok(id) = rune_core::id::ToolCallId::new(call_id.clone()) {
                    calls.push(ContentPart::ToolCall {
                        id,
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                }
            }
            SessionEvent::ToolResult {
                call_id,
                ok,
                output,
            } => {
                if let Ok(id) = rune_core::id::ToolCallId::new(call_id.clone()) {
                    results.push(ContentPart::ToolResult {
                        id,
                        name: String::new(),
                        content: output.clone(),
                        is_error: !ok,
                    });
                }
            }
            SessionEvent::TurnStarted { .. }
            | SessionEvent::Compaction { .. }
            | SessionEvent::UsageRecorded { .. }
            | SessionEvent::TitleSet { .. } => {}
        }
    }

    flush(&mut history, &mut calls, &mut results);
    history
}

/// Appends the pending calls, then the pending results.
fn flush(history: &mut History, calls: &mut Vec<ContentPart>, results: &mut Vec<ContentPart>) {
    if !calls.is_empty() {
        history.push_assistant(std::mem::take(calls));
    }
    if !results.is_empty() {
        history.push_tool_results(std::mem::take(results));
    }
}

/// Projects a stored session into a listing row.
fn summarize(state: &SessionState, dir: Utf8PathBuf) -> Summary {
    Summary {
        id: state.id.to_string(),
        turns: state.turns,
        events: state.events.len(),
        updated_at: state
            .events
            .last()
            .and_then(|frame| timestamp(frame.timestamp_ms)),
        title: state.title.clone(),
        dir,
    }
}

/// Renders a listing for a terminal.
#[must_use]
pub fn render_listing(rows: &[Summary]) -> String {
    if rows.is_empty() {
        return "no sessions stored".to_owned();
    }

    let mut out = String::new();
    for row in rows {
        let title = row.title.as_deref().unwrap_or("untitled");
        let _ = writeln!(
            out,
            "{}  {:>3} turns  {}  {title}",
            row.id,
            row.turns,
            row.updated_at.as_deref().unwrap_or("unknown time"),
        );
    }
    out.trim_end().to_owned()
}

/// Formats milliseconds since the epoch as an ISO 8601 timestamp.
#[must_use]
pub fn timestamp(millis: u64) -> Option<String> {
    let millis = i64::try_from(millis).ok()?;
    jiff::Timestamp::from_millisecond(millis)
        .ok()
        .map(|at| at.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Returns the session identifier a resume target names.
pub fn resolve_target(target: &crate::cli::ResumeTarget, paths: &Paths) -> Result<SessionId> {
    match target {
        crate::cli::ResumeTarget::Exact(raw) => raw.parse(),
        crate::cli::ResumeTarget::Latest => latest(paths)
            .ok_or_else(|| {
                RuneError::new(ErrorCode::NotFound, "no session has been saved yet")
                    .with_hint("run a session first, or start a new one")
            })
            .and_then(|row| row.id.parse()),
        crate::cli::ResumeTarget::Picker => list(paths)
            .into_iter()
            .next()
            .map(|row| row.id)
            .ok_or_else(|| RuneError::new(ErrorCode::NotFound, "no session has been saved yet"))
            .and_then(|raw| raw.parse()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;
    use rune_agent::turn::{PreparedCall, StopReason};
    use rune_net::stream::Usage;
    use rune_tools::contract::ToolOutput;

    fn paths(root: &Utf8Path) -> Paths {
        let resolved = Paths::resolve(
            Some(root.as_str()),
            Some(root.as_str()),
            Some(root.as_str()),
            Some(root.as_str()),
            None,
        );
        resolved.ensure_roots().expect("roots");
        resolved
    }

    fn id(raw: &str) -> SessionId {
        raw.parse().expect("id")
    }

    fn outcome(text: &str) -> TurnOutcome {
        TurnOutcome {
            stop_reason: StopReason::Completed,
            text: text.to_owned(),
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(4),
                ..Usage::default()
            },
            steps: 1,
            calls: Vec::new(),
        }
    }

    #[test]
    fn a_turn_round_trips_through_the_log() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionaaaaa");

        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder.user_message("what changed?").expect("wrote");
        recorder.turn(&outcome("two files")).expect("wrote");
        drop(recorder);

        let (_, history) = load(&paths, &key).expect("loaded");
        assert_eq!(history.turns().len(), 2);
        assert_eq!(history.turns()[0].text(), "what changed?");
        assert_eq!(history.turns()[1].text(), "two files");
    }

    #[test]
    fn tool_calls_and_results_round_trip_in_order() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionbbbbb");

        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder.user_message("read it").expect("wrote");
        let mut with_call = outcome("");
        with_call.calls = vec![
            rune_agent::turn::CallResult {
                call: PreparedCall {
                    id: "call_one".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{\"path\":\"a.rs\"}".to_owned(),
                },
                output: ToolOutput::success("file text"),
                executed: true,
            },
            rune_agent::turn::CallResult {
                call: PreparedCall {
                    id: "call_two".to_owned(),
                    name: "grep_files".to_owned(),
                    arguments: "{}".to_owned(),
                },
                output: ToolOutput::failure("no matches"),
                executed: true,
            },
        ];
        recorder.turn(&with_call).expect("wrote");
        recorder
            .turn(&outcome("done"))
            .expect("wrote the second turn");
        drop(recorder);

        let (_, history) = load(&paths, &key).expect("loaded");
        let turns = history.turns();
        // The user message, the assistant turn holding both calls, the results,
        // and the final assistant message.
        assert_eq!(turns.len(), 4, "{turns:#?}");
        assert_eq!(turns[1].tool_calls().len(), 2);
        assert_eq!(turns[1].tool_calls()[0].1, "read_file");
        assert_eq!(turns[1].tool_calls()[1].1, "grep_files");

        // The results are one turn holding both, in the order the calls were
        // made, which is what the model is owed.
        let results: Vec<(&str, bool)> = turns[2]
            .parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::ToolResult {
                    content, is_error, ..
                } => Some((content.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            vec![("file text", false), ("no matches", true)],
            "{turns:#?}"
        );
    }

    #[test]
    fn a_resumed_session_continues_its_turn_numbering() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionccccc");

        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder.user_message("one").expect("wrote");
        recorder.turn(&outcome("first")).expect("wrote");
        drop(recorder);

        let (mut recorder, _) = load(&paths, &key).expect("loaded");
        recorder.user_message("two").expect("wrote");
        recorder.turn(&outcome("second")).expect("wrote");
        drop(recorder);

        let state = load_read_only(&paths.session_dir(&key)).expect("read");
        assert_eq!(state.turns, 2, "the second turn was numbered from zero");
    }

    #[test]
    fn an_absent_usage_count_is_not_recorded_as_zero() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionddddd");

        let mut recorder = Recorder::create(&paths, &key).expect("created");
        let mut silent = outcome("no usage reported");
        silent.usage = Usage::default();
        recorder.turn(&silent).expect("wrote");
        drop(recorder);

        let state = load_read_only(&paths.session_dir(&key)).expect("read");
        let recorded = state
            .events
            .iter()
            .any(|frame| matches!(frame.event, SessionEvent::UsageRecorded { .. }));
        assert!(
            !recorded,
            "an unreported count was recorded, which reads back as zero"
        );
    }

    #[test]
    fn a_reported_zero_is_recorded() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessioneeeee");

        let mut recorder = Recorder::create(&paths, &key).expect("created");
        let mut zero = outcome("zero usage");
        zero.usage = Usage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            ..Usage::default()
        };
        recorder.turn(&zero).expect("wrote");
        drop(recorder);

        let state = load_read_only(&paths.session_dir(&key)).expect("read");
        let recorded = state.events.iter().any(|frame| {
            matches!(
                frame.event,
                SessionEvent::UsageRecorded {
                    input_tokens: 0,
                    output_tokens: 0
                }
            )
        });
        assert!(recorded, "a reported zero was dropped");
    }

    #[test]
    fn listing_puts_the_most_recent_first() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);

        let mut first = Recorder::create(&paths, &id("sessionaaaaa")).expect("created");
        first.user_message("old").expect("wrote");
        first.turn(&outcome("old answer")).expect("wrote");
        drop(first);

        std::thread::sleep(std::time::Duration::from_millis(5));

        let mut second = Recorder::create(&paths, &id("sessionfffff")).expect("created");
        second.user_message("new").expect("wrote");
        second.turn(&outcome("new answer")).expect("wrote");
        drop(second);

        let rows = list(&paths);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "sessionfffff", "listing is not newest first");
    }

    #[test]
    fn the_latest_session_is_the_newest_one() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);

        let mut only = Recorder::create(&paths, &id("sessionggggg")).expect("created");
        only.user_message("hi").expect("wrote");
        only.turn(&outcome("hello")).expect("wrote");
        drop(only);

        let newest = latest(&paths).expect("a session");
        assert_eq!(newest.id, "sessionggggg");
        assert_eq!(newest.turns, 1);
    }

    #[test]
    fn an_empty_directory_lists_nothing() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        assert!(list(&paths).is_empty());
        assert!(latest(&paths).is_none());
    }

    #[test]
    fn listing_renders_a_row_per_session() {
        let rows = vec![
            Summary {
                id: "sessionaaaaa".to_owned(),
                turns: 3,
                events: 9,
                updated_at: Some("2026-01-02T03:04:05Z".to_owned()),
                title: Some("parser work".to_owned()),
                dir: Utf8PathBuf::from("/tmp/a"),
            },
            Summary {
                id: "sessionbbbbb".to_owned(),
                turns: 0,
                events: 0,
                updated_at: None,
                title: None,
                dir: Utf8PathBuf::from("/tmp/b"),
            },
        ];
        let rendered = render_listing(&rows);
        assert!(rendered.contains("sessionaaaaa"));
        assert!(rendered.contains("parser work"));
        assert!(rendered.contains("untitled"));
        assert!(rendered.contains("unknown time"));
    }

    #[test]
    fn listing_nothing_says_so() {
        assert_eq!(render_listing(&[]), "no sessions stored");
    }

    #[test]
    fn loading_an_unknown_session_names_the_problem() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let err = load(&paths, &id("sessionzzzzz")).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.hint().is_some(), "the failure does not say what to do");
    }

    #[test]
    fn resuming_the_latest_session_works_when_one_exists() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut recorder = Recorder::create(&paths, &id("sessionhhhhh")).expect("created");
        recorder.user_message("hi").expect("wrote");
        recorder.turn(&outcome("yo")).expect("wrote");
        drop(recorder);

        let resolved = resolve_target(&crate::cli::ResumeTarget::Latest, &paths).expect("resolved");
        assert_eq!(resolved.to_string(), "sessionhhhhh");
    }

    #[test]
    fn resuming_with_no_saved_session_explains_why() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let err = resolve_target(&crate::cli::ResumeTarget::Latest, &paths).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.hint().is_some());
    }

    #[test]
    fn an_exact_resume_target_is_parsed_rather_than_looked_up() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let target = crate::cli::ResumeTarget::Exact("sessionjjjjj".to_owned());
        assert_eq!(
            resolve_target(&target, &paths)
                .expect("resolved")
                .to_string(),
            "sessionjjjjj"
        );
    }

    #[test]
    fn a_malformed_exact_resume_target_is_refused() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let target = crate::cli::ResumeTarget::Exact("not valid!".to_owned());
        assert!(resolve_target(&target, &paths).is_err());
    }

    #[test]
    fn a_title_comes_from_the_opening_line() {
        assert_eq!(derive_title("fix the parser"), "fix the parser");
        assert_eq!(derive_title("first line\nsecond line"), "first line");
        assert_eq!(derive_title("   padded   "), "padded");
    }

    #[test]
    fn an_empty_prompt_has_no_title_to_derive() {
        assert_eq!(derive_title(""), "untitled");
        assert_eq!(derive_title("\n\n"), "untitled");
    }

    #[test]
    fn a_long_title_is_truncated() {
        let title = derive_title(&"x".repeat(TITLE_CHARS + 10));
        assert_eq!(title.chars().count(), TITLE_CHARS + 3);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn a_title_set_once_is_read_back_by_the_recorder() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionkkkkk");
        let recorder = Recorder::create(&paths, &key).expect("created");
        assert!(recorder.title_is_unset(), "a new session had a title");
        recorder.set_title("cache rewrite").expect("wrote");
        assert!(!recorder.title_is_unset());
        drop(recorder);

        let rows = list(&paths);
        assert_eq!(rows[0].title.as_deref(), Some("cache rewrite"));
    }

    #[test]
    fn creating_a_session_that_already_exists_is_refused() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionlllll");
        let first = Recorder::create(&paths, &key).expect("created");
        drop(first);
        let err = Recorder::create(&paths, &key).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::AlreadyExists);
    }

    #[test]
    fn a_timestamp_renders_for_a_plausible_instant() {
        assert_eq!(
            timestamp(1_704_164_645_000).as_deref(),
            Some("2024-01-02T03:04:05Z")
        );
    }
}
