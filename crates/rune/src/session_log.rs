//! Recording a session as it runs, and reading one back.
//!
//! The log is the durable form of a conversation. It is written as the turn
//! runs, so a session that is interrupted can still be resumed, and the
//! conversation is rebuilt from the log rather than from a second copy kept in
//! memory.

use std::fmt::Write as _;

use camino::{Utf8Path, Utf8PathBuf};
use rune_agent::history::History;
use rune_agent::turn::TurnOutcome;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::SessionId;
use rune_core::paths::Paths;
use rune_net::message::ContentPart;
use rune_session::event::SessionEvent;
use rune_session::store::{SessionState, SessionStore, load_read_only};
use rune_session::tree::{Node, Tree};

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
    /// Last activity in milliseconds since the epoch, used for ordering.
    ///
    /// The rendered timestamp has second resolution, so ordering by it leaves
    /// two sessions started in the same second in an arbitrary order, and a
    /// listing can then name the wrong session as the most recent one.
    updated_at_ms: u64,
    /// Session title, when one was set.
    pub title: Option<String>,
    /// Workspace the session ran in, absent when none was recorded.
    pub workspace: Option<String>,
    /// Parent session, when this session is a child of another.
    pub parent: Option<String>,
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

    /// Marks this session as a child of another.
    ///
    /// A child is kept out of ordinary discovery and cannot be resumed on its
    /// own, because it ran with its parent's authority rather than its own.
    ///
    /// Called by the delegation path; present here because the record belongs
    /// with the other session marks.
    #[cfg(test)]
    pub fn set_parent(&self, parent: &SessionId) -> Result<()> {
        self.store.append(SessionEvent::ChildOf {
            parent: parent.to_string(),
        })?;
        Ok(())
    }

    /// Records the workspace the session runs in.
    ///
    /// Written once at the start, so a later listing can scope itself to a
    /// workspace without reading every session's contents.
    pub fn set_workspace(&self, workspace: &Utf8Path) -> Result<()> {
        self.store.append(SessionEvent::WorkspaceSet {
            workspace: rune_policy::trust::canonical_workspace(workspace).to_string(),
        })?;
        Ok(())
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

/// Page size used when a caller does not choose one.
pub const DEFAULT_PAGE: usize = 20;

/// Largest page a caller may ask for.
pub const MAX_PAGE: usize = 100;

/// One page of sessions.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Page {
    /// Rows in this page.
    pub rows: Vec<Summary>,
    /// Cursor to pass for the next page, absent when this is the last one.
    pub next: Option<String>,
}

/// Reads one page of sessions.
///
/// `scope` restricts the listing to one workspace, which is what `last` means:
/// the most recent session in the workspace the command ran in, not the most
/// recent anywhere. The cursor is the identifier of the last row already
/// returned, and paging resumes strictly after it. An identifier is used rather
/// than an offset because a session created between two pages would otherwise
/// shift every later row, so a session would be skipped or repeated.
pub fn page(
    paths: &Paths,
    scope: Option<&Utf8Path>,
    limit: usize,
    cursor: Option<&str>,
) -> Result<Page> {
    let limit = limit.clamp(1, MAX_PAGE);
    let all = list_scoped(paths, scope);

    let start = match cursor {
        None => 0,
        Some(cursor) => match all.iter().position(|row| row.id == cursor) {
            Some(position) => position.saturating_add(1),
            // A cursor naming a row that is gone cannot be resumed from, and
            // guessing a position would silently return the wrong rows.
            None => {
                return Err(RuneError::invalid_field(
                    "cursor",
                    format!("no session `{cursor}` is in the listing"),
                )
                .with_hint("restart the listing without a cursor"));
            }
        },
    };

    let rows: Vec<Summary> = all.into_iter().skip(start).take(limit).collect();
    let next = (rows.len() == limit)
        .then(|| rows.last().map(|row| row.id.clone()))
        .flatten();
    Ok(Page { rows, next })
}

/// Lists stored sessions, optionally restricted to one workspace.
///
/// A session that recorded no workspace is excluded from a scoped listing rather
/// than assumed to belong to it, because guessing would attribute a session to a
/// repository it may never have touched.
pub fn list_scoped(paths: &Paths, scope: Option<&Utf8Path>) -> Vec<Summary> {
    let wanted = scope.map(rune_policy::trust::canonical_workspace);
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
        if let Some(wanted) = &wanted
            && state.workspace.as_deref() != Some(wanted.as_str())
        {
            continue;
        }
        // A child session belongs to its parent's turn. Listing it would offer
        // a conversation the user never started, and resuming it directly would
        // run it without the authority its parent had.
        if state.parent.is_some() {
            continue;
        }
        out.push(summarize(&state, dir));
    }

    // An unreadable or absent timestamp sorts last rather than first, so a
    // damaged session never displaces a usable one.
    out.sort_by_key(|row| std::cmp::Reverse(row.updated_at_ms));
    out.truncate(LIST_LIMIT);
    out
}

/// Renders one session in detail.
///
/// Reports the log rather than the conversation, because the log is what is
/// stored: a count of events, the turns, the tokens, and any damage that was
/// found when the log was read.
#[must_use]
pub fn render_detail(state: &SessionState, dir: &Utf8Path) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "session   {}", state.id);
    let _ = writeln!(out, "directory {dir}");
    let _ = writeln!(
        out,
        "title     {}",
        state.title.as_deref().unwrap_or("untitled")
    );
    let _ = writeln!(out, "turns     {}", state.turns);
    let _ = writeln!(out, "events    {}", state.events.len());
    let _ = writeln!(
        out,
        "tokens    input {}, output {}",
        state.usage.input_tokens, state.usage.output_tokens
    );
    let _ = writeln!(
        out,
        "started   {}",
        state
            .events
            .first()
            .and_then(|frame| timestamp(frame.timestamp_ms))
            .as_deref()
            .unwrap_or("unknown")
    );
    let _ = writeln!(
        out,
        "updated   {}",
        state
            .events
            .last()
            .and_then(|frame| timestamp(frame.timestamp_ms))
            .as_deref()
            .unwrap_or("unknown")
    );
    match state.truncated_at {
        Some(offset) => {
            let _ = writeln!(
                out,
                "damaged   a torn final frame was dropped at byte {offset}"
            );
        }
        None => out.push_str("damaged   no"),
    }
    out.trim_end().to_owned()
}

/// Builds the branch tree of a stored session.
///
/// Every stored session is a single chain, so the tree is built by following the
/// log in order. It exists so the shape is reported from the log rather than
/// assumed, which is what lets a branch show up here when branching lands.
#[must_use]
pub fn tree_of(state: &SessionState) -> Tree {
    use rune_session::tree::Role;

    let mut tree = Tree::new();
    let mut parent: Option<u64> = None;
    for frame in &state.events {
        let (role, preview) = match &frame.event {
            SessionEvent::UserMessage { text } => (Role::User, text.as_str()),
            SessionEvent::AssistantMessage { text, .. } => (Role::Assistant, text.as_str()),
            SessionEvent::ToolResult { output, .. } => (Role::Tool, output.as_str()),
            SessionEvent::ToolCall { name, .. } => (Role::Assistant, name.as_str()),
            SessionEvent::TurnStarted { .. }
            | SessionEvent::Compaction { .. }
            | SessionEvent::UsageRecorded { .. }
            | SessionEvent::TitleSet { .. }
            | SessionEvent::WorkspaceSet { .. }
            | SessionEvent::ChildOf { .. } => continue,
        };
        let node = Node::new(
            role,
            preview.chars().take(PREVIEW_CHARS).collect::<String>(),
            preview.len(),
            i64::try_from(frame.timestamp_ms).unwrap_or(i64::MAX),
        );
        // A node that cannot be placed is skipped rather than failing the whole
        // report: the shape of the rest is still worth showing.
        if let Ok(seq) = tree.append(parent, node) {
            parent = Some(seq);
        }
    }
    tree
}

/// Characters of a turn shown as its preview.
pub const PREVIEW_CHARS: usize = 60;

/// Renders a branch tree for a terminal.
#[must_use]
pub fn render_tree(tree: &Tree, state: &SessionState) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "session {} ({} turns)", state.id, state.turns);

    for branch in tree.branches() {
        let _ = writeln!(
            out,
            "  branch {}  {} turn(s)  {}",
            branch.name, branch.turn_count, branch.summary
        );
    }

    // Nodes are visited by walking from each root through its children, which
    // is the only order the tree guarantees.
    let _ = writeln!(out, "\nturn  branch     role       preview");
    for root in roots(tree) {
        walk(tree, root, 0, &mut out);
    }
    out.trim_end().to_owned()
}

/// Returns the nodes that begin a path.
///
/// A node whose parent is absent is a root; a node whose parent is not in the
/// tree is treated as one too, so a repaired log still renders.
fn roots(tree: &Tree) -> Vec<u64> {
    let mut out: Vec<u64> = tree
        .path_to(tree.pointer().unwrap_or(1))
        .first()
        .copied()
        .into_iter()
        .collect();
    if out.is_empty() && !tree.is_empty() {
        out.push(1);
    }
    out
}

/// Walks one path, indenting by depth.
fn walk(tree: &Tree, seq: u64, depth: usize, out: &mut String) {
    let Some(node) = tree.node(seq) else {
        return;
    };
    let indent = "  ".repeat(depth);
    let _ = writeln!(
        out,
        "{indent}{:>4}  {:<10} {:<10} {}",
        node.seq,
        node.branch,
        role_name(node.role),
        node.preview
    );
    for child in tree.children(seq) {
        walk(tree, child, depth.saturating_add(1), out);
    }
}

/// Names a role for display.
fn role_name(role: rune_session::tree::Role) -> &'static str {
    use rune_session::tree::Role;
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Reports one session as JSON.
#[must_use]
pub fn detail_json(state: &SessionState) -> serde_json::Value {
    serde_json::json!({
        "id": state.id.to_string(),
        "title": state.title,
        "turns": state.turns,
        "events": state.events.len(),
        "usage": {
            "input_tokens": state.usage.input_tokens,
            "output_tokens": state.usage.output_tokens,
        },
        "truncated": state.truncated_at.is_some(),
    })
}

/// Reads one session's state.
pub fn inspect(paths: &Paths, id: &SessionId) -> Result<SessionState> {
    let dir = paths.session_dir(id);
    if !dir.exists() {
        return Err(
            RuneError::new(ErrorCode::NotFound, format!("no session `{id}` was found"))
                .with_hint("run `rune sessions` to see what is stored"),
        );
    }
    load_read_only(&dir)
}

/// Returns the most recent session in a workspace, when there is one.
pub fn latest_in(paths: &Paths, workspace: &Utf8Path) -> Option<Summary> {
    list_scoped(paths, Some(workspace)).into_iter().next()
}

/// Reads a stored session into a conversation.
///
/// A child session is refused: it ran with its parent's authority, and resuming
/// it on its own would run it with whatever authority this process has.
pub fn load(paths: &Paths, id: &SessionId) -> Result<(Recorder, History)> {
    let dir = paths.session_dir(id);
    if !dir.exists() {
        return Err(
            RuneError::new(ErrorCode::NotFound, format!("no session `{id}` was found"))
                .with_hint("list sessions to see what is stored"),
        );
    }

    let state = load_read_only(&dir)?;
    if let Some(parent) = &state.parent {
        return Err(RuneError::new(
            ErrorCode::Unsupported,
            format!("session `{id}` is a child of `{parent}` and cannot be resumed"),
        )
        .with_hint("resume the session that created it"));
    }
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
            | SessionEvent::TitleSet { .. }
            | SessionEvent::WorkspaceSet { .. }
            | SessionEvent::ChildOf { .. } => {}
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
        updated_at_ms: state.events.last().map_or(0, |frame| frame.timestamp_ms),
        title: state.title.clone(),
        workspace: state.workspace.clone(),
        parent: state.parent.clone(),
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
pub fn resolve_target(
    target: &crate::cli::ResumeTarget,
    paths: &Paths,
    workspace: &Utf8Path,
) -> Result<SessionId> {
    match target {
        crate::cli::ResumeTarget::Exact(raw) => raw.parse(),
        crate::cli::ResumeTarget::Latest => latest_in(paths, workspace)
            .ok_or_else(|| {
                RuneError::new(ErrorCode::NotFound, "no session has been saved yet")
                    .with_hint("run a session first, or start a new one")
            })
            .and_then(|row| row.id.parse()),
        crate::cli::ResumeTarget::Picker => list_scoped(paths, Some(workspace))
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

        // A sleep that only spans a few milliseconds would leave the two
        // sessions in the same whole second, which is what the ordering has to
        // survive rather than rely on.
        let mut second = Recorder::create(&paths, &id("sessionfffff")).expect("created");
        second.user_message("new").expect("wrote");
        second.turn(&outcome("new answer")).expect("wrote");
        drop(second);

        let rows = list_scoped(&paths, None);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "sessionfffff", "listing is not newest first");

        // The rendered timestamps may be equal, so the order has to come from
        // something finer than they show.
        assert!(
            rows[0].updated_at_ms >= rows[1].updated_at_ms,
            "the listing is ordered against its own timestamps"
        );
    }

    #[test]
    fn the_latest_session_is_the_newest_one() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);

        let mut only = Recorder::create(&paths, &id("sessionggggg")).expect("created");
        only.set_workspace(root).expect("workspace");
        only.user_message("hi").expect("wrote");
        only.turn(&outcome("hello")).expect("wrote");
        drop(only);

        let newest = latest_in(&paths, root).expect("a session");
        assert_eq!(newest.id, "sessionggggg");
        assert_eq!(newest.turns, 1);
    }

    #[test]
    fn paging_covers_every_session_without_a_gap_or_a_repeat() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let ids = [
            "sessionpg001",
            "sessionpg002",
            "sessionpg003",
            "sessionpg004",
            "sessionpg005",
        ];
        for (index, key) in ids.iter().enumerate() {
            let mut recorder = Recorder::create(&paths, &id(key)).expect("created");
            recorder.user_message(&format!("p{index}")).expect("wrote");
            recorder.turn(&outcome("a")).expect("wrote");
            drop(recorder);
            // Distinct timestamps, so the newest-first order is deterministic.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10 {
            let page = page(&paths, None, 2, cursor.as_deref()).expect("page");
            seen.extend(page.rows.iter().map(|row| row.id.clone()));
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        assert_eq!(seen.len(), ids.len(), "a page was skipped or repeated");
        let unique: std::collections::HashSet<_> = seen.iter().collect();
        assert_eq!(unique.len(), ids.len(), "a session appeared twice");
        // Every identifier must have been seen, since the walk ended.
        for key in ids {
            assert!(seen.iter().any(|seen| seen == key), "`{key}` was missed");
        }
    }

    #[test]
    fn the_last_page_reports_no_cursor() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut recorder = Recorder::create(&paths, &id("sessionpg006")).expect("created");
        recorder.user_message("only").expect("wrote");
        recorder.turn(&outcome("a")).expect("wrote");
        drop(recorder);

        let page = page(&paths, None, 10, None).expect("page");
        assert_eq!(page.rows.len(), 1);
        assert!(page.next.is_none(), "a final page offered a cursor");
    }

    #[test]
    fn an_unknown_cursor_is_refused_rather_than_guessed() {
        // Resuming from a position that no longer exists would silently return
        // the wrong rows, which is worse than reporting it.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let err = page(&paths, None, 2, Some("sessionmissing")).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.hint().is_some());
    }

    #[test]
    fn a_page_size_is_clamped_to_the_documented_bound() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        // A huge request is bounded rather than honoured, and a zero is raised
        // to one so a caller always makes progress.
        assert!(page(&paths, None, 0, None).is_ok());
        assert!(page(&paths, None, usize::MAX, None).is_ok());
        assert_eq!(MAX_PAGE, 100);
    }

    #[test]
    fn an_empty_directory_lists_nothing() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        assert!(list_scoped(&paths, None).is_empty());
        assert!(latest_in(&paths, root).is_none());
    }

    #[test]
    fn listing_renders_a_row_per_session() {
        let rows = vec![
            Summary {
                id: "sessionaaaaa".to_owned(),
                turns: 3,
                events: 9,
                updated_at: Some("2026-01-02T03:04:05Z".to_owned()),
                updated_at_ms: 1_704_164_645_000,
                title: Some("parser work".to_owned()),
                workspace: Some("/tmp/work".to_owned()),
                parent: None,
                dir: Utf8PathBuf::from("/tmp/a"),
            },
            Summary {
                id: "sessionbbbbb".to_owned(),
                turns: 0,
                events: 0,
                updated_at: None,
                updated_at_ms: 0,
                title: None,
                workspace: None,
                parent: None,
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
    fn a_child_session_is_absent_from_the_listing() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let parent = id("sessionpar01");
        let child = id("sessionchi01");

        let mut recorder = Recorder::create(&paths, &parent).expect("created");
        recorder.set_workspace(root).expect("workspace");
        recorder.user_message("hi").expect("wrote");
        recorder.turn(&outcome("yo")).expect("wrote");
        drop(recorder);

        let recorder = Recorder::create(&paths, &child).expect("created");
        recorder.set_workspace(root).expect("workspace");
        recorder.set_parent(&parent).expect("parent");
        drop(recorder);

        let listed = list_scoped(&paths, None);
        assert_eq!(listed.len(), 1, "{listed:#?}");
        assert_eq!(listed[0].id, parent.to_string());
    }

    #[test]
    fn a_child_session_cannot_be_resumed_directly() {
        // It ran with its parent's authority; resuming it alone would run it
        // with whatever authority this process happens to have.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let parent = id("sessionpar02");
        let child = id("sessionchi02");

        let recorder = Recorder::create(&paths, &child).expect("created");
        recorder.set_workspace(root).expect("workspace");
        recorder.set_parent(&parent).expect("parent");
        drop(recorder);

        let err = load(&paths, &child).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::Unsupported);
        assert!(err.message().contains("child"), "{}", err.message());
        assert!(err.hint().is_some());
    }

    #[test]
    fn a_child_is_still_inspectable_by_identifier() {
        // Kept out of discovery, but a caller that names it can read its log.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let parent = id("sessionpar03");
        let child = id("sessionchi03");
        let recorder = Recorder::create(&paths, &child).expect("created");
        recorder.set_workspace(root).expect("workspace");
        recorder.set_parent(&parent).expect("parent");
        drop(recorder);

        let state = inspect(&paths, &child).expect("inspected");
        assert_eq!(state.parent, Some(parent.to_string()));
    }

    #[test]
    fn a_session_tree_follows_its_log_in_order() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionqqqqq");
        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder.user_message("first").expect("wrote");
        recorder.turn(&outcome("answer")).expect("wrote");
        drop(recorder);

        let state = inspect(&paths, &key).expect("inspected");
        let tree = tree_of(&state);
        // The user message and the assistant reply.
        assert_eq!(tree.len(), 2);
        assert_eq!(tree.active_branch(), "main");
        assert_eq!(tree.path_to(2).len(), 2);
    }

    #[test]
    fn an_empty_session_has_an_empty_tree() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionrrrrr");
        let recorder = Recorder::create(&paths, &key).expect("created");
        drop(recorder);

        let state = inspect(&paths, &key).expect("inspected");
        assert!(tree_of(&state).is_empty());
    }

    #[test]
    fn the_rendered_tree_names_each_turn() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionsssss");
        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder.user_message("a question").expect("wrote");
        recorder.turn(&outcome("an answer")).expect("wrote");
        drop(recorder);

        let state = inspect(&paths, &key).expect("inspected");
        let rendered = render_tree(&tree_of(&state), &state);
        assert!(rendered.contains("branch main"), "{rendered}");
        assert!(rendered.contains("user"), "{rendered}");
        assert!(rendered.contains("assistant"), "{rendered}");
        assert!(rendered.contains("a question"), "{rendered}");
    }

    #[test]
    fn a_long_preview_is_truncated() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionttttt");
        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder
            .user_message(&"x".repeat(PREVIEW_CHARS * 3))
            .expect("wrote");
        recorder.turn(&outcome("y")).expect("wrote");
        drop(recorder);

        let tree = tree_of(&inspect(&paths, &key).expect("inspected"));
        let node = tree.node(1).expect("a user turn");
        assert_eq!(node.preview.chars().count(), PREVIEW_CHARS);
    }

    #[test]
    fn a_wide_preview_is_truncated_by_characters_not_bytes() {
        // A byte-based cut would split a character and produce invalid text.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionuuuuu");
        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder
            .user_message(&"書".repeat(PREVIEW_CHARS * 2))
            .expect("wrote");
        recorder.turn(&outcome("y")).expect("wrote");
        drop(recorder);

        let tree = tree_of(&inspect(&paths, &key).expect("inspected"));
        let node = tree.node(1).expect("a user turn");
        assert_eq!(node.preview.chars().count(), PREVIEW_CHARS);
        assert!(node.preview.is_char_boundary(node.preview.len()));
    }

    #[test]
    fn inspecting_a_session_reports_its_log() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionnnnnn");
        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder.user_message("hello").expect("wrote");
        recorder.turn(&outcome("hi")).expect("wrote");
        drop(recorder);

        let state = inspect(&paths, &key).expect("inspected");
        assert_eq!(state.turns, 1);
        assert_eq!(state.usage.input_tokens, 10);

        let rendered = render_detail(&state, &paths.session_dir(&key));
        assert!(rendered.contains("sessionnnnnn"), "{rendered}");
        assert!(rendered.contains("turns     1"), "{rendered}");
        assert!(rendered.contains("damaged   no"), "{rendered}");
    }

    #[test]
    fn inspecting_an_unknown_session_names_the_problem() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let err = inspect(&paths, &id("sessionzzzzz")).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.hint().is_some());
    }

    #[test]
    fn an_untitled_session_renders_as_untitled() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionooooo");
        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder.user_message("x").expect("wrote");
        recorder.turn(&outcome("y")).expect("wrote");
        drop(recorder);

        let state = inspect(&paths, &key).expect("inspected");
        assert!(render_detail(&state, &paths.session_dir(&key)).contains("untitled"));
    }

    #[test]
    fn the_session_detail_json_carries_the_totals() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionppppp");
        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder.user_message("x").expect("wrote");
        recorder.turn(&outcome("y")).expect("wrote");
        drop(recorder);

        let value = detail_json(&inspect(&paths, &key).expect("inspected"));
        assert_eq!(value["turns"], 1);
        assert_eq!(value["usage"]["input_tokens"], 10);
        assert_eq!(value["truncated"], false);
    }

    #[test]
    fn resuming_the_latest_session_works_when_one_exists() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut recorder = Recorder::create(&paths, &id("sessionhhhhh")).expect("created");
        recorder.set_workspace(root).expect("workspace");
        recorder.user_message("hi").expect("wrote");
        recorder.turn(&outcome("yo")).expect("wrote");
        drop(recorder);

        let resolved =
            resolve_target(&crate::cli::ResumeTarget::Latest, &paths, root).expect("resolved");
        assert_eq!(resolved.to_string(), "sessionhhhhh");
    }

    #[test]
    fn the_latest_session_is_scoped_to_its_workspace() {
        // Two repositories each have their own latest session, and `last` means
        // the one for the workspace the command ran in.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let first = root.join("one");
        let second = root.join("two");
        std::fs::create_dir_all(&first).expect("one");
        std::fs::create_dir_all(&second).expect("two");

        for (key, where_) in [("sessionws001", &first), ("sessionws002", &second)] {
            let mut recorder = Recorder::create(&paths, &id(key)).expect("created");
            recorder.set_workspace(where_).expect("workspace");
            recorder.user_message("hi").expect("wrote");
            recorder.turn(&outcome("yo")).expect("wrote");
            drop(recorder);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let one = latest_in(&paths, &first).expect("one");
        let two = latest_in(&paths, &second).expect("two");
        assert_eq!(one.id, "sessionws001");
        assert_eq!(two.id, "sessionws002");
        // An unscoped listing sees both.
        assert_eq!(list_scoped(&paths, None).len(), 2);
    }

    #[test]
    fn a_session_with_no_recorded_workspace_is_not_attributed_to_one() {
        // Guessing would attribute a session to a repository it may never have
        // touched, so it is excluded from a scoped listing.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut recorder = Recorder::create(&paths, &id("sessionws003")).expect("created");
        recorder.user_message("hi").expect("wrote");
        recorder.turn(&outcome("yo")).expect("wrote");
        drop(recorder);

        assert_eq!(list_scoped(&paths, None).len(), 1);
        assert!(latest_in(&paths, root).is_none());
    }

    #[test]
    fn resuming_with_no_saved_session_explains_why() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let err =
            resolve_target(&crate::cli::ResumeTarget::Latest, &paths, root).expect_err("refused");
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
            resolve_target(&target, &paths, Utf8Path::new("/w"))
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
        assert!(resolve_target(&target, &paths, Utf8Path::new("/w")).is_err());
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

        let rows = list_scoped(&paths, None);
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
