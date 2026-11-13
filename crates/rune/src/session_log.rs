//! Recording a session as it runs, and reading one back.
//!
//! The log is the durable form of a conversation. It is written as the turn
//! runs, so a session that is interrupted can still be resumed, and the
//! conversation is rebuilt from the log rather than from a second copy kept in
//! memory.

use std::fmt::Write as _;

use camino::{Utf8Path, Utf8PathBuf};
use rune_agent::history::History;
use rune_agent::history::JournalQueue;
use rune_agent::turn::{JournalEntry, NotePlacement, TurnOutcome};
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

/// The index and text of every user message in the log, in log order.
/// Corrections sent while a turn ran are user messages too.
pub fn user_messages(recorder: &Recorder) -> Result<Vec<(usize, String)>> {
    Ok(recorder
        .store
        .read()?
        .events
        .into_iter()
        .filter_map(|frame| match frame.event {
            SessionEvent::UserMessage { text } => Some(text),
            _ => None,
        })
        .enumerate()
        .collect())
}

/// The turn the `message`-th user message started, when it started one. A
/// correction sent while a turn ran started none.
pub fn turn_started_by(recorder: &Recorder, message: usize) -> Result<Option<u64>> {
    let events = recorder.store.read()?.events;
    let Some(position) = events
        .iter()
        .enumerate()
        .filter(|(_, frame)| matches!(frame.event, SessionEvent::UserMessage { .. }))
        .nth(message)
        .map(|(position, _)| position)
    else {
        return Ok(None);
    };
    for frame in events.iter().skip(position.saturating_add(1)) {
        match frame.event {
            SessionEvent::TurnStarted { turn } => return Ok(Some(turn)),
            SessionEvent::UserMessage { .. }
            | SessionEvent::AssistantMessage { .. }
            | SessionEvent::AssistantContent { .. }
            | SessionEvent::ToolCall { .. }
            | SessionEvent::ToolResult { .. }
            | SessionEvent::TurnFinished { .. }
            | SessionEvent::TurnCancelled { .. }
            | SessionEvent::TurnFailed { .. }
            | SessionEvent::TurnInterrupted { .. } => return Ok(None),
            _ => {}
        }
    }
    Ok(None)
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
    /// Model most recently selected in the log, when one was recorded.
    pub model: Option<String>,
    /// Text of the first user message, when there is one.
    pub first_prompt: Option<String>,
    /// Directory holding the log.
    pub dir: Utf8PathBuf,
}

impl Summary {
    /// Returns the last activity in milliseconds since the epoch.
    #[must_use]
    pub const fn updated_at_ms(&self) -> u64 {
        self.updated_at_ms
    }
}

/// An open session log.
#[derive(Debug)]
pub struct Recorder {
    store: SessionStore,
    objective: Option<String>,
    objective_budget: Option<u64>,
    objective_tokens: u64,
    turn: u64,
    active_turn: bool,
    responses: Vec<rune_agent::history::Turn>,
    journal_error: Option<RuneError>,
    metadata_warning: std::cell::RefCell<Option<RuneError>>,
    /// Initial estimate and its source, populated only when resuming.
    resumed_context: (u64, Option<&'static str>),
}

impl Recorder {
    /// Copies this session's journal, through its latest event, into a new
    /// session and returns the new id. This session is left as it is.
    pub fn fork(&self, paths: &Paths) -> Result<SessionId> {
        let last = self
            .store
            .read()?
            .events
            .last()
            .map(|frame| frame.seq)
            .ok_or_else(|| {
                RuneError::new(ErrorCode::NotFound, "the session has nothing to fork yet")
            })?;
        let destination = SessionId::generate();
        rune_session::fork::fork(paths, &self.store.id(), &destination, last)?;
        Ok(destination)
    }

    /// Copies this session's journal up to just before its `message`-th user
    /// message (0-based, in log order) into a new session and returns the new
    /// id. This session is left as it is.
    pub fn fork_before(&self, paths: &Paths, message: usize) -> Result<SessionId> {
        let events = self.store.read()?.events;
        let position = events
            .iter()
            .enumerate()
            .filter(|(_, frame)| matches!(frame.event, SessionEvent::UserMessage { .. }))
            .nth(message)
            .map(|(position, _)| position)
            .ok_or_else(|| RuneError::new(ErrorCode::NotFound, "no such message"))?;
        let before = position
            .checked_sub(1)
            .and_then(|previous| events.get(previous))
            .ok_or_else(|| {
                RuneError::new(
                    ErrorCode::NotFound,
                    "nothing comes before the first message",
                )
            })?;
        let destination = SessionId::generate();
        rune_session::fork::fork(paths, &self.store.id(), &destination, before.seq)?;
        Ok(destination)
    }

    /// Creates a new session and its log.
    pub fn create(paths: &Paths, id: &SessionId) -> Result<Self> {
        let store = SessionStore::create(paths, id)?;
        Ok(Self {
            store,
            objective: None,
            objective_budget: None,
            objective_tokens: 0,
            turn: 0,
            active_turn: false,
            responses: Vec::new(),
            journal_error: None,
            metadata_warning: std::cell::RefCell::new(None),
            resumed_context: (0, None),
        })
    }

    /// Records an event while keeping projection warnings out of journal errors.
    fn append(&self, event: SessionEvent) -> Result<()> {
        let report = self.store.append(event)?;
        // Keep one warning while a projection remains unavailable, even if
        // many streamed deltas were synced. A later successful refresh repairs
        // the summary and clears the pending warning.
        *self.metadata_warning.borrow_mut() = report.metadata_warning;
        Ok(())
    }

    /// Takes the pending metadata repair warning for display at a turn boundary.
    pub fn take_metadata_warning(&self) -> Option<RuneError> {
        self.metadata_warning.borrow_mut().take()
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
        self.append(SessionEvent::ChildOf {
            parent: parent.to_string(),
        })?;
        Ok(())
    }

    /// Records the workspace the session runs in.
    ///
    /// Written once at the start, so a later listing can scope itself to a
    /// workspace without reading every session's contents.
    pub fn set_workspace(&self, workspace: &Utf8Path) -> Result<()> {
        self.append(SessionEvent::WorkspaceSet {
            workspace: rune_policy::trust::canonical_workspace(workspace).to_string(),
        })?;
        Ok(())
    }

    /// Opens an existing session, preparing to append to it.
    pub fn open(paths: &Paths, id: &SessionId) -> Result<Self> {
        let store = SessionStore::open(&paths.session_dir(id))?;
        let turn = store.turns();
        let mut objective = None;
        let mut objective_budget = None;
        let mut objective_tokens = 0_u64;
        for frame in &store.read()?.events {
            match &frame.event {
                SessionEvent::ObjectiveSet { objective: text } => {
                    objective = Some(text.clone());
                    objective_budget = None;
                    objective_tokens = 0;
                }
                SessionEvent::ObjectiveBudgetSet {
                    objective: text,
                    token_budget,
                } => {
                    objective = Some(text.clone());
                    objective_budget = Some(*token_budget);
                    objective_tokens = 0;
                }
                SessionEvent::ObjectiveTokensRecorded { tokens } if objective_budget.is_some() => {
                    objective_tokens = objective_tokens.saturating_add(*tokens);
                }
                _ => {}
            }
        }
        Ok(Self {
            store,
            objective,
            objective_budget,
            objective_tokens,
            turn,
            active_turn: false,
            responses: Vec::new(),
            journal_error: None,
            metadata_warning: std::cell::RefCell::new(None),
            resumed_context: (0, None),
        })
    }

    /// Returns the initial context estimate and its source for a resumed log.
    pub fn resumed_context(&self) -> (u64, Option<&'static str>) {
        self.resumed_context
    }

    /// Returns the session identifier.
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.store.id()
    }

    /// Returns the explicitly set objective, absent for ordinary conversations.
    pub fn objective(&self) -> Option<&str> {
        self.objective.as_deref()
    }

    /// Persists an explicit objective before making it active in memory.
    pub fn set_objective(&mut self, objective: &str) -> Result<()> {
        if objective.trim().is_empty() {
            return Err(RuneError::invalid_field("objective", "objective is empty"));
        }
        self.append(SessionEvent::ObjectiveSet {
            objective: objective.to_owned(),
        })?;
        self.objective = Some(objective.to_owned());
        self.objective_budget = None;
        self.objective_tokens = 0;
        Ok(())
    }

    /// Atomically replaces the objective and its accounting with an explicit budget.
    pub fn set_budgeted_objective(&mut self, objective: &str, token_budget: u64) -> Result<()> {
        if objective.trim().is_empty() {
            return Err(RuneError::invalid_field("objective", "objective is empty"));
        }
        if token_budget == 0 {
            return Err(RuneError::invalid_field(
                "token_budget",
                "token budget must be positive",
            ));
        }
        self.append(SessionEvent::ObjectiveBudgetSet {
            objective: objective.to_owned(),
            token_budget,
        })?;
        self.objective = Some(objective.to_owned());
        self.objective_budget = Some(token_budget);
        self.objective_tokens = 0;
        Ok(())
    }

    /// Budget and accounted usage of the current objective, when explicitly set.
    pub fn objective_budget(&self) -> Option<(u64, u64)> {
        self.objective_budget
            .map(|budget| (budget, self.objective_tokens))
    }

    /// Persists a provider attempt before allowing another continuation.
    pub fn account_objective_tokens(&mut self, tokens: u64) -> Result<()> {
        if self.objective_budget.is_some() {
            self.append(SessionEvent::ObjectiveTokensRecorded { tokens })?;
            self.objective_tokens = self.objective_tokens.saturating_add(tokens);
        }
        Ok(())
    }

    /// Records the user's message.
    pub fn user_message(&self, text: &str) -> Result<()> {
        self.append(SessionEvent::UserMessage {
            text: text.to_owned(),
        })?;
        Ok(())
    }

    /// Persists an explicit user constraint before the live history adopts it.
    pub fn pin_constraint(&self, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Err(RuneError::invalid_field(
                "constraint",
                "constraint is empty",
            ));
        }
        self.append(SessionEvent::ConstraintPinned {
            text: text.to_owned(),
        })?;
        Ok(())
    }

    /// The number of the latest turn begun, zero before the first.
    pub fn current_turn(&self) -> u64 {
        self.turn
    }

    /// Records a workspace snapshot taken at a turn boundary, or a restore of one.
    pub fn workspace_checkpoint(&self, event: SessionEvent) -> Result<()> {
        if !matches!(
            event,
            SessionEvent::WorkspaceCheckpoint { .. } | SessionEvent::Restored { .. }
        ) {
            return Err(RuneError::new(
                ErrorCode::Internal,
                format!("expected a workspace checkpoint, got {}", event.kind()),
            ));
        }
        self.append(event)
    }

    /// Starts an exchange before the provider can produce visible text.
    /// The number of the current or last turn, zero before the first.
    pub const fn turn_number(&self) -> u64 {
        self.turn
    }

    pub fn begin_turn(&mut self) -> Result<()> {
        if !self.active_turn {
            let turn = self.turn.saturating_add(1);
            self.append(SessionEvent::TurnStarted { turn })?;
            self.turn = turn;
            self.active_turn = true;
            self.responses.clear();
        }
        Ok(())
    }

    /// Journals visible text in bounded frames before the terminal draws it.
    pub fn assistant_delta(&self, text: &str) -> Result<()> {
        // Bound encoded frames even when a provider sends a very large delta.
        // JSON escaping can expand each byte by at most six times.
        const CHUNK_BYTES: usize = 8 * 1024;
        if let Some(error) = &self.journal_error {
            return Err(error.clone());
        }
        let mut remaining = text;
        while !remaining.is_empty() {
            let mut end = remaining.len().min(CHUNK_BYTES);
            while !remaining.is_char_boundary(end) {
                end = end.saturating_sub(1);
            }
            self.append(SessionEvent::AssistantDelta {
                turn: self.turn,
                text: remaining[..end].to_owned(),
            })?;
            remaining = &remaining[end..];
        }
        Ok(())
    }

    /// Durably clears a failed request attempt's visible text before a retry.
    pub fn reset_assistant(&self) -> Result<()> {
        if let Some(error) = &self.journal_error {
            return Err(error.clone());
        }
        self.append(SessionEvent::AssistantReset { turn: self.turn })?;
        Ok(())
    }

    /// Remembers an observer write failure, since the observer cannot return it.
    pub fn journal_failed(&mut self, error: RuneError) {
        if self.journal_error.is_none() {
            self.journal_error = Some(error);
        }
    }

    /// Returns a journal failure to the input loop after its worker stops.
    pub fn check_journal(&mut self) -> Result<()> {
        self.journal_error.take().map_or(Ok(()), Err)
    }

    /// Retains completed provider responses until their tool results are saved.
    /// This is independent of model history, which can compact between steps.
    pub fn response(&mut self, outcome: &rune_net::transport::StreamOutcome) -> Result<()> {
        self.begin_turn()?;
        let (parts, _) = rune_agent::turn::assistant_parts(outcome);
        let mut message = rune_agent::history::Turn::assistant(0, parts);
        message.replay.clone_from(&outcome.replay);
        if !message.parts.is_empty() {
            self.responses.push(message);
        }
        Ok(())
    }

    /// Records a history change a resume must repeat, at its position.
    pub fn journal(&self, entry: &JournalEntry) -> Result<()> {
        self.append(journal_event(entry))
    }

    /// Records a turn and everything it produced.
    ///
    /// A streamed exchange keeps the number assigned before its request. A
    /// caller recording a finished outcome directly starts the exchange here,
    /// so even a turn with no output is numbered in the order it ran.
    pub fn turn(&mut self, outcome: &TurnOutcome) -> Result<()> {
        self.begin_turn()?;
        let turn = self.turn;

        if self.responses.is_empty() {
            // The calls of one step are a batch, and their results follow the whole
            // batch. Writing them interleaved would leave a log that cannot be
            // replayed into the conversation the model actually saw.
            for call in &outcome.calls {
                self.append(SessionEvent::ToolCall {
                    call_id: call.call.id.clone(),
                    name: call.call.name.clone(),
                    arguments: call.call.arguments.clone(),
                })?;
            }
            for repair in &outcome.repairs {
                self.append(repaired_event(repair))?;
            }
            for call in &outcome.calls {
                self.append(SessionEvent::ToolResult {
                    call_id: call.call.id.clone(),
                    ok: !call.output.is_error,
                    output: call.output.text.clone(),
                })?;
            }

            if !outcome.text.is_empty() {
                self.append(SessionEvent::AssistantMessage {
                    turn,
                    text: outcome.text.clone(),
                })?;
            }
        } else {
            let mut results = outcome.calls.iter();
            for message in std::mem::take(&mut self.responses) {
                for event in message_events(&message, turn)? {
                    self.append(event)?;
                }
                for (id, _, _) in message.tool_calls() {
                    if let Some(repair) = outcome
                        .repairs
                        .iter()
                        .find(|repair| repair.id == id.as_str())
                    {
                        self.append(repaired_event(repair))?;
                    }
                }
                for (id, _, _) in message.tool_calls() {
                    if let Some(result) = results
                        .next()
                        .filter(|result| result.call.id == id.as_str())
                    {
                        self.append(SessionEvent::ToolResult {
                            call_id: result.call.id.clone(),
                            ok: !result.output.is_error,
                            output: result.output.text.clone(),
                        })?;
                    }
                }
            }
        }

        if let (Some(input), Some(output)) =
            (outcome.usage.input_tokens, outcome.usage.output_tokens)
        {
            self.append(SessionEvent::UsageRecorded {
                input_tokens: input,
                output_tokens: output,
            })?;
        }
        self.append(SessionEvent::TurnFinished { turn })?;
        self.active_turn = false;
        Ok(())
    }

    /// Adds child usage without child messages or turn boundaries.
    pub fn child_usage(&self, usage: &rune_net::stream::Usage) -> Result<()> {
        if let (Some(input), Some(output)) = (usage.input_tokens, usage.output_tokens) {
            self.append(SessionEvent::UsageRecorded {
                input_tokens: input,
                output_tokens: output,
            })?;
        }
        Ok(())
    }

    /// Records the visible answer and boundary of a cancelled exchange.
    ///
    /// A cancellation is numbered even when no text arrived. Unknown usage is
    /// left absent rather than recorded as a completed request with zero usage.
    pub fn cancelled_turn(&mut self, partial: &str) -> Result<()> {
        self.begin_turn()?;
        let turn = self.turn;
        if !partial.trim().is_empty() {
            self.append(SessionEvent::AssistantMessage {
                turn,
                text: partial.to_owned(),
            })?;
        }
        self.append(SessionEvent::TurnCancelled { turn })?;
        self.active_turn = false;
        Ok(())
    }

    /// Records the visible answer and cause of a failed exchange.
    ///
    /// A failure is numbered even when no text arrived. Unknown usage is left
    /// absent rather than recorded as a completed request with zero usage.
    pub fn failed_turn(&mut self, partial: &str, error: &RuneError) -> Result<()> {
        self.begin_turn()?;
        let turn = self.turn;
        if !partial.trim().is_empty() {
            self.append(SessionEvent::AssistantMessage {
                turn,
                text: partial.to_owned(),
            })?;
        }
        self.append(SessionEvent::TurnFailed {
            turn,
            code: error.code(),
            message: error.message().to_owned(),
        })?;
        self.active_turn = false;
        Ok(())
    }

    /// Returns the session title, if it has one.
    #[must_use]
    pub fn title(&self) -> Option<String> {
        self.store.title()
    }

    /// The notice for a session another process left open and died holding,
    /// when the lock was taken over from it.
    #[must_use]
    pub fn takeover_notice(&self) -> Option<String> {
        self.store.took_over().map(|holder| {
            let on = holder
                .host
                .as_ref()
                .map(|host| format!(" on {host}"))
                .unwrap_or_default();
            format!(
                "session {} was left open by pid {}{on}, which is no longer running; continuing",
                self.store.id(),
                holder.pid
            )
        })
    }

    /// Writes the session's event log as JSON lines, as `rune session export`
    /// does.
    ///
    /// # Errors
    ///
    /// Fails when the log cannot be read or the writer refuses the bytes.
    pub fn export_jsonl(&self, out: impl std::io::Write) -> Result<()> {
        load_read_only(self.store.dir())?.export_jsonl(out)
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
        self.append(SessionEvent::TitleSet {
            title: title.to_owned(),
        })?;
        Ok(())
    }

    /// Archives original messages while holding the session's writer lock.
    pub fn archive_messages(&self, messages: &[rune_agent::history::Turn]) -> Result<()> {
        crate::compaction_archive::append(self.store.dir(), messages)
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
    paginate(list_scoped(paths, scope), limit, cursor)
}

/// Searches titles and recorded messages by a case-insensitive literal fragment.
///
/// Workspace and child-session exclusions are the same as for an ordinary
/// listing. Matching precedes the listing cap and pagination.
pub fn search_page(
    paths: &Paths,
    scope: Option<&Utf8Path>,
    query: &str,
    limit: usize,
    cursor: Option<&str>,
) -> Result<Page> {
    if query.trim().is_empty() {
        return Err(RuneError::invalid_field("search", "search text is empty")
            .with_hint("pass a title or user-message fragment"));
    }
    let query = query.to_lowercase();
    paginate(list_matching(paths, scope, Some(&query)), limit, cursor)
}

fn paginate(all: Vec<Summary>, limit: usize, cursor: Option<&str>) -> Result<Page> {
    let limit = limit.clamp(1, MAX_PAGE);

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
    list_matching(paths, scope, None)
}

fn list_matching(paths: &Paths, scope: Option<&Utf8Path>, query: Option<&str>) -> Vec<Summary> {
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
        if let Some(query) = query
            && !state
                .title
                .as_deref()
                .is_some_and(|title| title.to_lowercase().contains(query))
            && !state.events.iter().any(|frame| {
                message_text(&frame.event)
                    .is_some_and(|(_, text)| text.to_lowercase().contains(query))
            })
        {
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
/// Each saved branch has its own session log, so this view follows that log in
/// order. Fork provenance is available in the exported journal.
#[must_use]
pub fn tree_of(state: &SessionState) -> Tree {
    use rune_session::tree::Role;

    let mut tree = Tree::new();
    let mut parent: Option<u64> = None;
    for frame in rune_session::replay::replay_events(&state.events) {
        let failure;
        let (role, preview) = match &frame.event {
            SessionEvent::UserMessage { text } => (Role::User, text.as_str()),
            SessionEvent::AssistantMessage { text, .. } => (Role::Assistant, text.as_str()),
            SessionEvent::TurnCancelled { .. } => (Role::System, "[cancelled]"),
            SessionEvent::TurnInterrupted { .. } => (Role::System, "[interrupted]"),
            SessionEvent::TurnFailed { code, .. } => {
                failure = format!("[failed: {code}]");
                (Role::System, failure.as_str())
            }
            SessionEvent::ToolResult { output, .. } => (Role::Tool, output.as_str()),
            SessionEvent::ToolCall { name, .. } => (Role::Assistant, name.as_str()),
            SessionEvent::AssistantContent { .. }
            | SessionEvent::AssistantDelta { .. }
            | SessionEvent::AssistantReset { .. }
            | SessionEvent::TurnStarted { .. }
            | SessionEvent::TurnFinished { .. }
            | SessionEvent::Compaction { .. }
            | SessionEvent::UsageRecorded { .. }
            | SessionEvent::TitleSet { .. }
            | SessionEvent::ObjectiveSet { .. }
            | SessionEvent::ObjectiveBudgetSet { .. }
            | SessionEvent::ObjectiveTokensRecorded { .. }
            | SessionEvent::ConstraintPinned { .. }
            | SessionEvent::ModelSet { .. }
            | SessionEvent::WorkspaceSet { .. }
            | SessionEvent::WorkspaceCheckpoint { .. }
            | SessionEvent::Restored { .. }
            | SessionEvent::ForkedFrom { .. }
            | SessionEvent::ChildOf { .. }
            | SessionEvent::HarnessNote { .. }
            | SessionEvent::Checkpoint { .. }
            | SessionEvent::ResultReplaced { .. }
            | SessionEvent::ToolCallRepaired { .. } => continue,
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
        // An interrupted boundary has its own row below. Keep it out of the
        // branch summary so an exchange ending here reports the boundary once.
        let failure_boundary = branch
            .head_seq
            .and_then(|seq| tree.node(seq))
            .is_some_and(|node| {
                node.role == rune_session::tree::Role::System
                    && node.preview.starts_with("[failed: ")
            });
        let summary = if matches!(branch.summary.as_str(), "[cancelled]" | "[interrupted]")
            || failure_boundary
        {
            ""
        } else {
            &branch.summary
        };
        let _ = writeln!(
            out,
            "  branch {}  {} turn(s)  {}",
            branch.name, branch.turn_count, summary
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
    let mut recorder = Recorder::open(paths, id)?;
    // Usage is per turn and can include several requests. It is an estimate,
    // not an exact current request count. Never sum repeated conversation input.
    let saved = state
        .events
        .iter()
        .filter_map(|frame| match frame.event {
            SessionEvent::UsageRecorded {
                input_tokens,
                output_tokens,
            } => Some(input_tokens.saturating_add(output_tokens)),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let bytes = u64::try_from(history.byte_len()).unwrap_or(u64::MAX);
    let estimated = rune_agent::tokens::estimate_tokens(bytes).max(u64::from(!history.is_empty()));
    recorder.resumed_context = if saved > 0 && saved >= estimated {
        (saved, Some("saved usage"))
    } else if estimated > 0 {
        (estimated, Some("history bytes"))
    } else {
        (0, None)
    };
    Ok((recorder, history))
}

/// Encodes a history change for the journal.
#[must_use]
pub fn journal_event(entry: &JournalEntry) -> SessionEvent {
    match entry.clone() {
        JournalEntry::Note {
            at,
            placement,
            text,
        } => SessionEvent::HarnessNote {
            at,
            placement: match placement {
                NotePlacement::PromptPrefix => rune_session::NotePlacement::PromptPrefix,
                NotePlacement::PromptSuffix => rune_session::NotePlacement::PromptSuffix,
                NotePlacement::UserPart => rune_session::NotePlacement::UserPart,
                NotePlacement::UserTurn => rune_session::NotePlacement::UserTurn,
                NotePlacement::AssistantTurn => rune_session::NotePlacement::AssistantTurn,
            },
            text,
        },
        JournalEntry::Checkpoint {
            at,
            through,
            message,
        } => SessionEvent::Checkpoint {
            at,
            through,
            message,
        },
        JournalEntry::ResultReplaced {
            at,
            call_id,
            content,
        } => SessionEvent::ResultReplaced {
            at,
            call_id,
            content,
        },
    }
}

/// Decodes a journaled history change; `None` for any other event.
#[must_use]
pub fn journal_entry(event: &SessionEvent) -> Option<JournalEntry> {
    match event.clone() {
        SessionEvent::HarnessNote {
            at,
            placement,
            text,
        } => Some(JournalEntry::Note {
            at,
            placement: match placement {
                rune_session::NotePlacement::PromptPrefix => NotePlacement::PromptPrefix,
                rune_session::NotePlacement::PromptSuffix => NotePlacement::PromptSuffix,
                rune_session::NotePlacement::UserPart => NotePlacement::UserPart,
                rune_session::NotePlacement::UserTurn => NotePlacement::UserTurn,
                rune_session::NotePlacement::AssistantTurn => NotePlacement::AssistantTurn,
            },
            text,
        }),
        SessionEvent::Checkpoint {
            at,
            through,
            message,
        } => Some(JournalEntry::Checkpoint {
            at,
            through,
            message,
        }),
        SessionEvent::ResultReplaced {
            at,
            call_id,
            content,
        } => Some(JournalEntry::ResultReplaced {
            at,
            call_id,
            content,
        }),
        _ => None,
    }
}

/// Records that a call ran in a repaired form.
fn repaired_event(repair: &rune_agent::turn::CallRepair) -> SessionEvent {
    SessionEvent::ToolCallRepaired {
        call_id: repair.id.clone(),
        from: repair.from.clone(),
        to: repair.to.clone(),
        repairs: repair.repairs.clone(),
    }
}

/// Encodes one history message without flattening provider response boundaries.
pub fn message_events(message: &rune_agent::history::Turn, turn: u64) -> Result<Vec<SessionEvent>> {
    let mut events = Vec::new();
    if message.role == rune_net::message::Role::Assistant {
        events.push(SessionEvent::AssistantContent {
            message: serde_json::to_string(&rune_net::message::Message {
                role: message.role,
                parts: message.parts.clone(),
                replay: message.replay.clone(),
                origin: message.origin.clone(),
            })?,
        });
        let text = message.text();
        if !text.is_empty() {
            events.push(SessionEvent::AssistantMessage { turn, text });
        }
    } else if message.role == rune_net::message::Role::User {
        events.push(SessionEvent::UserMessage {
            text: message.text(),
        });
    }
    for part in &message.parts {
        match part {
            ContentPart::ToolCall {
                id,
                name,
                arguments,
            } => events.push(SessionEvent::ToolCall {
                call_id: id.to_string(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            ContentPart::ToolResult {
                id,
                content,
                is_error,
                ..
            } => events.push(SessionEvent::ToolResult {
                call_id: id.to_string(),
                ok: !is_error,
                output: content.clone(),
            }),
            _ => {}
        }
    }
    Ok(events)
}

/// The result a replayed call is given when the log holds none for it.
const INTERRUPTED_CALL: &str = rune_net::normalize::NOT_RUN;

/// Builds a conversation from a stored log.
#[must_use]
pub fn history_from(state: &SessionState) -> History {
    let mut history = History::new();
    let mut calls: Vec<ContentPart> = Vec::new();
    let mut results: Vec<ContentPart> = Vec::new();
    let mut assistant: Option<rune_net::message::Message> = None;
    let mut queue = JournalQueue::default();
    queue.apply_ready(&mut history);

    for frame in rune_session::replay::replay_events(&state.events) {
        if let Some(entry) = journal_entry(&frame.event) {
            queue.push(entry);
            queue.apply_ready(&mut history);
            continue;
        }
        match &frame.event {
            SessionEvent::ConstraintPinned { text } => {
                // Empty pins are refused at write time. Keep replay compatible
                // with a hand-edited log without changing its ordinary turns.
                let _ = history.pin_constraint(text.clone());
            }
            SessionEvent::UserMessage { text } => {
                flush(
                    &mut history,
                    &mut calls,
                    &mut results,
                    &mut assistant,
                    &mut queue,
                );
                history.push_user(text.clone());
                queue.apply_ready(&mut history);
            }
            SessionEvent::AssistantContent { message } => {
                flush(
                    &mut history,
                    &mut calls,
                    &mut results,
                    &mut assistant,
                    &mut queue,
                );
                assistant = serde_json::from_str(message).ok();
                if let Some(message) = &assistant {
                    calls.extend(
                        message
                            .parts
                            .iter()
                            .filter(|part| matches!(part, ContentPart::ToolCall { .. }))
                            .cloned(),
                    );
                }
            }
            SessionEvent::AssistantMessage { .. } if assistant.is_some() => {}
            SessionEvent::AssistantMessage { text, .. } => {
                flush(
                    &mut history,
                    &mut calls,
                    &mut results,
                    &mut assistant,
                    &mut queue,
                );
                if !text.is_empty() {
                    history.push_assistant(vec![ContentPart::Text { text: text.clone() }]);
                    queue.apply_ready(&mut history);
                }
            }
            SessionEvent::ToolCall { .. } if assistant.is_some() && results.is_empty() => {}
            SessionEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                // A tool result run always follows the calls it answers, so a
                // new call closes the previous group.
                if !results.is_empty() {
                    flush(
                        &mut history,
                        &mut calls,
                        &mut results,
                        &mut assistant,
                        &mut queue,
                    );
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
            SessionEvent::AssistantDelta { .. }
            | SessionEvent::AssistantReset { .. }
            | SessionEvent::TurnStarted { .. }
            | SessionEvent::TurnFinished { .. }
            | SessionEvent::Compaction { .. }
            | SessionEvent::UsageRecorded { .. }
            | SessionEvent::TitleSet { .. }
            | SessionEvent::ObjectiveSet { .. }
            | SessionEvent::ObjectiveBudgetSet { .. }
            | SessionEvent::ObjectiveTokensRecorded { .. }
            | SessionEvent::ModelSet { .. }
            | SessionEvent::WorkspaceSet { .. }
            | SessionEvent::WorkspaceCheckpoint { .. }
            | SessionEvent::Restored { .. }
            | SessionEvent::ForkedFrom { .. }
            | SessionEvent::ChildOf { .. }
            | SessionEvent::TurnInterrupted { .. }
            | SessionEvent::TurnCancelled { .. }
            | SessionEvent::TurnFailed { .. }
            | SessionEvent::HarnessNote { .. }
            | SessionEvent::Checkpoint { .. }
            | SessionEvent::ResultReplaced { .. }
            | SessionEvent::ToolCallRepaired { .. } => {}
        }
    }

    flush(
        &mut history,
        &mut calls,
        &mut results,
        &mut assistant,
        &mut queue,
    );
    queue.apply_ready(&mut history);
    mark_scoped_rules(&mut history);
    history
}

/// Reverses the attribute escaping of a rendered instruction file's path.
fn unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Marks every nested instruction file a rebuilt tool result carries as
/// delivered, so a resumed session does not send it twice.
fn mark_scoped_rules(history: &mut History) {
    const OPEN: &str = "<scoped-rules from=\"";
    let mut found = Vec::new();
    for turn in history.turns() {
        for part in &turn.parts {
            let ContentPart::ToolResult { content, .. } = part else {
                continue;
            };
            let mut rest = content.as_str();
            while let Some(start) = rest.find(OPEN) {
                rest = &rest[start.saturating_add(OPEN.len())..];
                let Some(end) = rest.find('"') else {
                    break;
                };
                found.push((format!("rules:{}", unescape(&rest[..end])), turn.seq));
                rest = &rest[end..];
            }
        }
    }
    for (key, seq) in found {
        history.mark_delivered(key, seq);
    }
}

/// Appends the pending calls, then the pending results.
///
/// A call with no result is given one saying it was interrupted. A turn's calls
/// are all written before any of its results, so a process that stopped between
/// the two leaves calls that were never answered, and a conversation holding
/// one is refused by every request that follows.
fn flush(
    history: &mut History,
    calls: &mut Vec<ContentPart>,
    results: &mut Vec<ContentPart>,
    assistant: &mut Option<rune_net::message::Message>,
    queue: &mut JournalQueue,
) {
    if !calls.is_empty() {
        for call in calls.iter() {
            let ContentPart::ToolCall { id, name, .. } = call else {
                continue;
            };
            let answered = results.iter().any(
                |result| matches!(result, ContentPart::ToolResult { id: answered, .. } if answered == id),
            );
            if !answered {
                results.push(ContentPart::ToolResult {
                    id: id.clone(),
                    name: name.clone(),
                    content: INTERRUPTED_CALL.to_owned(),
                    is_error: true,
                });
            }
        }
        for result in results.iter_mut() {
            if let ContentPart::ToolResult { id, name, .. } = result
                && let Some(ContentPart::ToolCall { name: called, .. }) = calls.iter().find(
                    |call| matches!(call, ContentPart::ToolCall { id: called, .. } if called == id),
                )
            {
                name.clone_from(called);
            }
        }
        if assistant.is_none() {
            history.push_assistant(std::mem::take(calls));
            queue.apply_ready(history);
        } else {
            calls.clear();
        }
    }
    if let Some(message) = assistant.take() {
        history.push_assistant_with_replay(message.parts, message.replay, message.origin);
        queue.apply_ready(history);
    }
    if !results.is_empty() {
        history.push_tool_results(std::mem::take(results));
        queue.apply_ready(history);
    }
}

/// Projects a stored session into a listing row.
fn summarize(state: &SessionState, dir: Utf8PathBuf) -> Summary {
    let model = state
        .events
        .iter()
        .rev()
        .find_map(|frame| match &frame.event {
            SessionEvent::ModelSet { model, .. } => Some(model.clone()),
            _ => None,
        });
    let first_prompt = state.events.iter().find_map(|frame| match &frame.event {
        SessionEvent::UserMessage { text } => Some(text.clone()),
        _ => None,
    });
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
        model,
        first_prompt,
        dir,
    }
}

/// Returns the role and text of a user or assistant message.
fn message_text(event: &SessionEvent) -> Option<(&'static str, &str)> {
    match event {
        SessionEvent::UserMessage { text } => Some(("user", text)),
        SessionEvent::AssistantMessage { text, .. } => Some(("assistant", text)),
        _ => None,
    }
}

/// Most search hits printed in one command.
pub const HIT_LIMIT: usize = 50;

/// Longest line a search hit shows, in characters.
pub const HIT_CHARS: usize = 100;

/// One line of a stored session that matched a search.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hit {
    /// Session identifier.
    pub id: String,
    /// Day of the session's last activity, as `YYYY-MM-DD`.
    pub date: String,
    /// `user`, `assistant`, or `title` when only the title matched.
    pub role: &'static str,
    /// The matching line, cut to [`HIT_CHARS`].
    pub line: String,
}

/// Finds the matching message lines of the listed sessions, newest session
/// first and in log order within a session.
///
/// Returns at most [`HIT_LIMIT`] hits and whether more were found.
#[must_use]
pub fn search_hits(rows: &[Summary], query: &str) -> (Vec<Hit>, bool) {
    let query = query.to_lowercase();
    let mut hits = Vec::new();
    for row in rows {
        let Ok(state) = load_read_only(&row.dir) else {
            continue;
        };
        let date = date_of(row.updated_at_ms).unwrap_or_else(|| "unknown".to_owned());
        let before = hits.len();
        for frame in &state.events {
            let Some((role, text)) = message_text(&frame.event) else {
                continue;
            };
            for line in matching_lines(text, &query) {
                hits.push(Hit {
                    id: row.id.clone(),
                    date: date.clone(),
                    role,
                    line: cut(line.trim(), HIT_CHARS),
                });
            }
        }
        if hits.len() == before
            && let Some(title) = &state.title
            && title.to_lowercase().contains(&query)
        {
            hits.push(Hit {
                id: row.id.clone(),
                date: date.clone(),
                role: "title",
                line: cut(title, HIT_CHARS),
            });
        }
        if hits.len() > HIT_LIMIT {
            break;
        }
    }
    let more = hits.len() > HIT_LIMIT;
    hits.truncate(HIT_LIMIT);
    (hits, more)
}

/// Returns the lines of `text` containing `query`, already lowercased.
///
/// A query that spans a line break matches no single line, so the line where
/// the match starts stands for it.
fn matching_lines<'a>(text: &'a str, query: &str) -> Vec<&'a str> {
    let lowered = text.to_lowercase();
    let lines: Vec<&str> = text.lines().collect();
    let hits: Vec<&str> = lowered
        .lines()
        .zip(&lines)
        .filter(|(folded, _)| folded.contains(query))
        .map(|(_, line)| *line)
        .collect();
    if !hits.is_empty() {
        return hits;
    }
    lowered
        .find(query)
        .and_then(|at| lines.get(lowered[..at].matches('\n').count()).copied())
        .into_iter()
        .collect()
}

/// Cuts `text` to at most `limit` characters.
fn cut(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Formats milliseconds since the epoch as a `YYYY-MM-DD` date.
fn date_of(millis: u64) -> Option<String> {
    let millis = i64::try_from(millis).ok()?;
    jiff::Timestamp::from_millisecond(millis)
        .ok()
        .map(|at| at.strftime("%Y-%m-%d").to_string())
}

/// Renders search hits for a terminal, one line each.
#[must_use]
pub fn render_hits(hits: &[Hit], more: bool) -> String {
    if hits.is_empty() {
        return "no sessions stored".to_owned();
    }
    let mut out = String::new();
    for hit in hits {
        let _ = writeln!(out, "{}  {}  {}", hit.id, hit.date, hit.line);
    }
    if more {
        out.push_str("more matches: narrow the search");
    }
    out.trim_end().to_owned()
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

    #[test]
    fn gemini_responses_survive_the_journal_resume_and_fork() {
        use rune_net::provider::{Provider, RequestPlan};
        use rune_net::stream::StreamReducer;
        use serde_json::json;
        let temp = tempfile::tempdir().expect("temp");
        let paths = paths(Utf8Path::from_path(temp.path()).expect("utf8"));
        let id = SessionId::generate();
        let mut recorder = Recorder::create(&paths, &id).expect("recorder");
        recorder.user_message("lookup twice").expect("prompt");
        let mut completed = outcome("done");
        let mut native_parts = Vec::new();
        for step in 0..3 {
            let parts = if step < 2 {
                json!([
                    {"text": "thinking", "thought": true, "thoughtSignature": format!("thought-{step}")},
                    {"functionCall": {"name": "lookup", "args": {"step": step}}, "thoughtSignature": format!("call-{step}")}
                ])
            } else {
                json!([{"text": "done", "thoughtSignature": "final-signature"}])
            };
            let mut reducer = rune_net::gemini::Reducer::default();
            let mut events = Vec::new();
            reducer.apply(Some(&json!({"candidates": [{"content": {"parts": parts}, "finishReason": "STOP"}]}).to_string()), &mut events).expect("frame");
            reducer.apply(None, &mut events).expect("finish");
            let response = rune_net::transport::StreamOutcome {
                events,
                replay: reducer.replay(),
                finish: Some(reducer.finish().expect("reason")),
                ..Default::default()
            };
            recorder.response(&response).expect("response");
            for (id, name, arguments) in response.tool_calls() {
                completed.calls.push(rune_agent::turn::CallResult {
                    call: PreparedCall {
                        id,
                        name,
                        arguments,
                    },
                    output: ToolOutput::success(format!("result-{step}")),
                    executed: true,
                });
            }
            native_parts.push(parts);
        }
        recorder.turn(&completed).expect("save");
        let mut interrupted = recorder.store.read().expect("state");
        let content = interrupted
            .events
            .iter()
            .position(|frame| matches!(frame.event, SessionEvent::AssistantContent { .. }))
            .expect("native content");
        interrupted.events.truncate(content + 1);
        history_from(&interrupted)
            .validate()
            .expect("unfinished calls receive interrupted results");
        assert!(
            rune_session::fork::fork(
                &paths,
                &id,
                &SessionId::generate(),
                interrupted.events[content].seq
            )
            .is_err()
        );
        let boundary = recorder
            .store
            .read()
            .expect("state")
            .events
            .last()
            .expect("last")
            .seq;
        drop(recorder);
        let branch = SessionId::generate();
        rune_session::fork::fork(&paths, &id, &branch, boundary).expect("fork");
        for saved in [id, branch] {
            let (_, history) = load(&paths, &saved).expect("resume");
            history.validate().expect("paired history");
            let mut plan = RequestPlan::new("gemini-fixture");
            plan.messages = history.to_messages();
            let request = rune_net::gemini::Gemini
                .build_request(&plan)
                .expect("request");
            assert_eq!(request["contents"].as_array().expect("contents").len(), 6);
            let state = load_read_only(&paths.session_dir(&saved)).expect("saved events");
            plan.messages = rune_acp::session::history_from_events(&state.events).to_messages();
            assert_eq!(
                rune_net::gemini::Gemini
                    .build_request(&plan)
                    .expect("ACP request"),
                request
            );
            for (step, parts) in native_parts.iter().enumerate() {
                assert_eq!(&request["contents"][step * 2 + 1]["parts"], parts);
                if step < 2 {
                    let response =
                        &request["contents"][step * 2 + 2]["parts"][0]["functionResponse"];
                    assert_eq!(response["name"], "lookup");
                    assert_eq!(response["response"]["output"], format!("result-{step}"));
                    assert!(response.get("id").is_none());
                }
            }
        }
    }

    #[test]
    fn legacy_tool_results_recover_their_function_names() {
        use rune_net::provider::{Provider, RequestPlan};
        let temp = tempfile::tempdir().expect("temp");
        let paths = paths(Utf8Path::from_path(temp.path()).expect("utf8"));
        let id = SessionId::generate();
        let mut recorder = Recorder::create(&paths, &id).expect("recorder");
        recorder.user_message("lookup").expect("prompt");
        let mut completed = outcome("done");
        completed.calls.push(rune_agent::turn::CallResult {
            call: PreparedCall {
                id: "legacy".into(),
                name: "lookup".into(),
                arguments: "{}".into(),
            },
            output: ToolOutput::success("found"),
            executed: true,
        });
        recorder.turn(&completed).expect("save legacy format");
        drop(recorder);
        let (_, history) = load(&paths, &id).expect("resume");
        let mut plan = RequestPlan::new("gemini-fixture");
        plan.messages = history.to_messages();
        let request = rune_net::gemini::Gemini
            .build_request(&plan)
            .expect("request");
        assert_eq!(
            request["contents"][2]["parts"][0]["functionResponse"]["name"],
            "lookup"
        );
    }

    #[test]
    fn fork_before_drops_the_chosen_message_and_after() {
        let temp = tempfile::tempdir().expect("home");
        let paths = paths(Utf8Path::from_path(temp.path()).expect("utf8"));
        let id = SessionId::generate();
        let store = SessionStore::create(&paths, &id).expect("store");
        store
            .append(SessionEvent::WorkspaceSet {
                workspace: "/work".into(),
            })
            .expect("workspace");
        for (turn, text) in [(1, "one"), (2, "two"), (3, "three")] {
            for event in [
                SessionEvent::UserMessage { text: text.into() },
                SessionEvent::TurnStarted { turn },
                SessionEvent::AssistantMessage {
                    turn,
                    text: format!("A{turn}"),
                },
                SessionEvent::TurnFinished { turn },
            ] {
                store.append(event).expect("event");
            }
        }
        drop(store);
        let recorder = Recorder::open(&paths, &id).expect("open");
        let listed = user_messages(&recorder).expect("messages");
        assert_eq!(
            listed,
            vec![
                (0, "one".to_owned()),
                (1, "two".to_owned()),
                (2, "three".to_owned())
            ]
        );
        assert_eq!(turn_started_by(&recorder, 1).expect("turn"), Some(2));
        assert_eq!(turn_started_by(&recorder, 5).expect("none"), None);
        let fork = recorder.fork_before(&paths, 1).expect("fork");
        let (forked, history) = load(&paths, &fork).expect("load the fork");
        assert_eq!(
            user_messages(&forked).expect("forked messages"),
            vec![(0, "one".to_owned())]
        );
        assert_eq!(history.turns().len(), 2);
        assert_eq!(user_messages(&recorder).expect("source").len(), 3);
        assert!(recorder.fork_before(&paths, 3).is_err());
        let first = Recorder::create(&paths, &SessionId::generate()).expect("create");
        first.user_message("alone").expect("message");
        let err = first.fork_before(&paths, 0).expect_err("first frame");
        assert_eq!(err.message(), "nothing comes before the first message");
    }

    #[test]
    fn a_recorder_forks_into_a_new_session_with_the_same_history() {
        let temp = tempfile::tempdir().expect("home");
        let paths = paths(Utf8Path::from_path(temp.path()).expect("utf8"));
        let id = SessionId::generate();
        let recorder = Recorder::create(&paths, &id).expect("create");
        assert!(recorder.fork(&paths).is_err());
        recorder.user_message("hello").expect("message");
        let branch = recorder.fork(&paths).expect("fork");
        assert_ne!(branch, id);
        let (_, history) = load(&paths, &branch).expect("resume the branch");
        assert_eq!(history.turns().len(), 1);
        Recorder::open(&paths, &branch).expect("the branch is writable");
    }

    #[test]
    fn forked_tool_pair_replays_as_a_valid_history_and_continues_with_the_next_turn() {
        let temp = tempfile::tempdir().expect("temp");
        let paths = paths(Utf8Path::from_path(temp.path()).expect("utf8"));
        let source = SessionId::generate();
        let branch = SessionId::generate();
        let store = SessionStore::create(&paths, &source).expect("store");
        for event in [
            SessionEvent::ConstraintPinned {
                text: "keep exact text".into(),
            },
            SessionEvent::TurnStarted { turn: 1 },
            SessionEvent::UserMessage {
                text: "read it".into(),
            },
            SessionEvent::ToolCall {
                call_id: "read".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            },
            SessionEvent::ToolResult {
                call_id: "read".into(),
                ok: true,
                output: "complete original result".into(),
            },
            SessionEvent::AssistantMessage {
                turn: 1,
                text: "later source answer".into(),
            },
            SessionEvent::TurnFinished { turn: 1 },
        ] {
            store.append(event).expect("event");
        }
        rune_session::fork::fork(&paths, &source, &branch, rune_core::id::EventSeq(5))
            .expect("fork");
        let (mut recorder, history) = load(&paths, &branch).expect("resume fork");
        history.validate().expect("tool pair is sendable");
        assert_eq!(history.pinned_constraints(), &["keep exact text"]);
        assert_eq!(history.len(), 3);
        assert!(
            matches!(&history.turns()[2].parts[0], ContentPart::ToolResult { content, is_error: false, .. } if content == "complete original result")
        );
        recorder.user_message("branch continuation").expect("user");
        recorder.turn(&outcome("branch answer")).expect("next turn");
        drop(recorder);
        let (recorder, history) = load(&paths, &branch).expect("reopen");
        history.validate().expect("continued history is sendable");
        assert_eq!(recorder.turn, 2);
        let source_state = store.read().expect("source");
        assert_eq!(source_state.events.len(), 7);
        assert_eq!(
            source_state.events[5].event,
            SessionEvent::AssistantMessage {
                turn: 1,
                text: "later source answer".into()
            }
        );
    }

    #[test]
    fn a_later_successful_refresh_clears_the_pending_metadata_warning() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(root.path()).expect("utf8");
        let recorder = Recorder::create(&paths(root), &id("sessionaaaaa")).expect("create");
        let metadata = recorder.store.metadata_path();
        std::fs::remove_file(&metadata).expect("remove projection");
        std::fs::create_dir(&metadata).expect("block projection");
        recorder
            .user_message("durable")
            .expect("record despite projection failure");
        assert!(recorder.metadata_warning.borrow().is_some());

        std::fs::remove_dir(&metadata).expect("unblock projection");
        recorder.set_title("repaired").expect("record and refresh");
        assert!(recorder.take_metadata_warning().is_none());
        assert_eq!(recorder.store.read().expect("replay").len(), 2);
        assert_eq!(
            serde_json::from_slice::<rune_session::Metadata>(
                &std::fs::read(metadata).expect("projection")
            )
            .expect("parse projection"),
            recorder.store.metadata()
        );
    }

    fn outcome(text: &str) -> TurnOutcome {
        TurnOutcome {
            error: None,
            judge: None,
            stop_reason: StopReason::Completed,
            text: text.to_owned(),
            narration: Vec::new(),
            reasoning: String::new(),
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(4),
                ..Usage::default()
            },
            last_request: Usage {
                input_tokens: Some(10),
                output_tokens: Some(4),
                ..Usage::default()
            },
            steps: 1,
            verification: rune_agent::turn::Verification::NotApplicable,
            checks: rune_agent::verify::CheckSummary::default(),
            calls: Vec::new(),
            repairs: Vec::new(),
            repaired_calls: 0,
        }
    }

    #[test]
    fn journal_frames_are_bounded_and_recover_unicode_before_a_torn_tail() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().expect("temp");
        let paths = paths(Utf8Path::from_path(dir.path()).expect("utf8"));
        let key = id("sessionjrnl1");
        let text = "aé🦀\n\t".repeat(5000);
        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder.user_message("slow").expect("prompt saved");
        recorder.begin_turn().expect("started");
        recorder.assistant_delta(&text).expect("journalled");
        let state = recorder.store.read().expect("read while streaming");
        let deltas: Vec<_> = state
            .events
            .iter()
            .filter_map(|frame| match &frame.event {
                SessionEvent::AssistantDelta { text, .. } => {
                    assert!(text.len() <= 8 * 1024);
                    assert!(frame.encode().expect("encoded").len() < 50 * 1024);
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect();
        assert!(deltas.len() > 1);
        assert_eq!(deltas.concat(), text);
        assert_eq!(state.usage, rune_session::store::UsageTotal::default());
        drop(recorder);
        std::fs::OpenOptions::new()
            .append(true)
            .open(paths.session_dir(&key).join("events.jsonl"))
            .expect("log")
            .write_all(b"{\"schema\":1,\"seq\":")
            .expect("torn frame");
        let (mut recorder, history) = load(&paths, &key).expect("recovered");
        assert_eq!(history.to_messages()[1].text(), text);
        recorder.user_message("continue").expect("continued prompt");
        recorder.begin_turn().expect("next turn");
        recorder.assistant_delta("done").expect("journalled");
        recorder.turn(&outcome("done")).expect("finished");
        let state = recorder.store.read().expect("read");
        assert_eq!(state.turns, 2);
        assert_eq!(
            state
                .events
                .iter()
                .filter(|frame| matches!(frame.event, SessionEvent::TurnFinished { turn: 2 }))
                .count(),
            1
        );
        let replay = render_tree(&tree_of(&state), &state);
        assert_eq!(replay.matches("[interrupted]").count(), 1);
        assert_eq!(history_from(&state).to_messages()[1].text(), text);
    }

    #[test]
    fn resume_estimates_context_without_summing_repeated_inputs() {
        for usage in [Some((1234, 0)), None, Some((u64::MAX, 1))] {
            let dir = tempfile::tempdir().expect("temp");
            let root = Utf8Path::from_path(dir.path()).expect("utf8");
            let paths = paths(root);
            let key = id("sessionctx01");
            let recorder = Recorder::create(&paths, &key).expect("created");
            assert_eq!(recorder.resumed_context(), (0, None));
            recorder.user_message("hello").expect("user");
            if let Some((input_tokens, output_tokens)) = usage {
                for _ in 0..2 {
                    recorder
                        .store
                        .append(SessionEvent::UsageRecorded {
                            input_tokens,
                            output_tokens,
                        })
                        .expect("usage");
                }
            }
            drop(recorder);
            let (recorder, _) = load(&paths, &key).expect("resumed");
            let expected = match usage {
                Some((input, output)) => (input.saturating_add(output), Some("saved usage")),
                None => (2, Some("history bytes")),
            };
            assert_eq!(recorder.resumed_context(), expected);
        }
    }

    #[test]
    fn resuming_an_empty_log_keeps_an_empty_context_meter() {
        let dir = tempfile::tempdir().expect("temp");
        let paths = paths(Utf8Path::from_path(dir.path()).expect("utf8"));
        let key = id("sessionctx02");
        drop(Recorder::create(&paths, &key).expect("created"));
        let (recorder, _) = load(&paths, &key).expect("resumed");
        assert_eq!(recorder.resumed_context(), (0, None));
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
    fn cancelled_exchanges_round_trip_without_duplicate_text_or_invented_usage() {
        for partial in ["", "STREAM-01\nSTREAM-02\nSTREAM-03\n"] {
            let dir = tempfile::tempdir().expect("temp");
            let root = Utf8Path::from_path(dir.path()).expect("utf8");
            let paths = paths(root);
            let key = id("sessionccccc");
            let mut recorder = Recorder::create(&paths, &key).expect("created");
            recorder.user_message("slow").expect("wrote");
            recorder.cancelled_turn(partial).expect("cancelled");
            drop(recorder);

            let (mut recorder, history) = load(&paths, &key).expect("resumed");
            history.validate().expect("valid replay");
            assert_eq!(history.len(), if partial.is_empty() { 1 } else { 2 });
            if !partial.is_empty() {
                assert_eq!(history.turns()[1].text(), partial);
            }
            let state = inspect(&paths, &key).expect("inspected");
            assert_eq!(state.turns, 1);
            assert_eq!(state.usage.total(), 0);
            assert!(
                !state
                    .events
                    .iter()
                    .any(|frame| matches!(frame.event, SessionEvent::UsageRecorded { .. }))
            );
            assert_eq!(
                state
                    .events
                    .iter()
                    .filter(|frame| matches!(frame.event, SessionEvent::TurnCancelled { turn: 1 }))
                    .count(),
                1
            );
            let rendered = render_tree(&tree_of(&state), &state);
            assert_eq!(rendered.matches("[cancelled]").count(), 1);
            for line in partial.lines() {
                assert_eq!(rendered.matches(line).count(), 1, "{rendered}");
            }

            recorder.user_message("continue").expect("wrote");
            recorder.turn(&outcome("continued")).expect("wrote");
            drop(recorder);
            let state = inspect(&paths, &key).expect("inspected");
            assert_eq!(state.turns, 2);
            assert!(state.events.iter().any(|frame| matches!(
                frame.event,
                SessionEvent::AssistantMessage { turn: 2, .. }
            )));
            assert_eq!(
                render_tree(&tree_of(&state), &state)
                    .matches("[cancelled]")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn failed_exchanges_round_trip_with_the_visible_answer_and_cause() {
        for partial in ["", "STREAM-01\nSTREAM-02\nSTREAM-03\n"] {
            let dir = tempfile::tempdir().expect("temp");
            let root = Utf8Path::from_path(dir.path()).expect("utf8");
            let paths = paths(root);
            let key = id("sessionfffff");
            let error = RuneError::new(
                ErrorCode::IncompleteStream,
                "the provider closed the stream",
            );
            let mut recorder = Recorder::create(&paths, &key).expect("created");
            recorder.user_message("die").expect("wrote");
            recorder.failed_turn(partial, &error).expect("failed");
            drop(recorder);

            let (mut recorder, history) = load(&paths, &key).expect("resumed");
            history.validate().expect("valid replay");
            assert_eq!(history.len(), if partial.is_empty() { 1 } else { 2 });
            if !partial.is_empty() {
                assert_eq!(history.turns()[1].text(), partial);
            }
            let state = inspect(&paths, &key).expect("inspected");
            assert_eq!(state.turns, 1);
            assert_eq!(state.usage.total(), 0);
            assert!(
                !state
                    .events
                    .iter()
                    .any(|frame| matches!(frame.event, SessionEvent::UsageRecorded { .. }))
            );
            let failures: Vec<_> = state
                .events
                .iter()
                .filter(|frame| matches!(frame.event, SessionEvent::TurnFailed { .. }))
                .map(|frame| &frame.event)
                .collect();
            assert_eq!(
                failures,
                [&SessionEvent::TurnFailed {
                    turn: 1,
                    code: ErrorCode::IncompleteStream,
                    message: error.message().to_owned(),
                }]
            );
            let rendered = render_tree(&tree_of(&state), &state);
            assert_eq!(rendered.matches("[failed: incomplete_stream]").count(), 1);
            for line in partial.lines() {
                assert_eq!(rendered.matches(line).count(), 1, "{rendered}");
            }

            recorder.user_message("continue").expect("wrote");
            recorder.turn(&outcome("continued")).expect("wrote");
            drop(recorder);
            let state = inspect(&paths, &key).expect("inspected");
            assert_eq!(state.turns, 2);
            assert!(state.events.iter().any(|frame| matches!(
                frame.event,
                SessionEvent::AssistantMessage { turn: 2, .. }
            )));
            assert_eq!(
                render_tree(&tree_of(&state), &state)
                    .matches("[failed: incomplete_stream]")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn ordinary_reply_text_does_not_become_a_failure_boundary() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionfffff");
        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder.user_message("quote this marker").expect("wrote");
        recorder
            .turn(&outcome("[failed: incomplete_stream]"))
            .expect("wrote");
        let state = inspect(&paths, &key).expect("inspected");
        let rendered = render_tree(&tree_of(&state), &state);
        assert!(
            rendered.lines().any(|line| {
                line.starts_with("  branch ") && line.ends_with("[failed: incomplete_stream]")
            }),
            "{rendered}"
        );
    }

    #[test]
    fn a_call_the_log_never_answered_is_replayed_as_interrupted() {
        // The recorder writes a turn's calls before its results, so a crash
        // between them leaves calls with no result. The replayed conversation
        // has to answer them, or every later turn fails validation.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionccccc");

        let recorder = Recorder::create(&paths, &key).expect("created");
        recorder.user_message("read both").expect("wrote");
        recorder
            .store
            .append(SessionEvent::TurnStarted { turn: 1 })
            .expect("wrote");
        for call_id in ["call_one", "call_two"] {
            recorder
                .store
                .append(SessionEvent::ToolCall {
                    call_id: call_id.to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{}".to_owned(),
                })
                .expect("wrote");
        }
        recorder
            .store
            .append(SessionEvent::ToolResult {
                call_id: "call_one".to_owned(),
                ok: true,
                output: "file text".to_owned(),
            })
            .expect("wrote");
        drop(recorder);

        let (resumed, history) = load(&paths, &key).expect("loaded");
        history
            .validate()
            .expect("the replayed conversation is sendable");
        resumed.user_message("carry on").expect("wrote");
        drop(resumed);

        let (_, history) = load(&paths, &key).expect("loaded again");
        history.validate().expect("later turns are still sendable");
        let results: Vec<(String, &str, bool)> = history.turns()[2]
            .parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::ToolResult {
                    id,
                    content,
                    is_error,
                    ..
                } => Some((id.to_string(), content.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            vec![
                ("call_one".to_owned(), "file text", false),
                ("call_two".to_owned(), INTERRUPTED_CALL, true),
            ]
        );
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
    fn registry_failure_code_and_hint_survive_session_recording_and_replay() {
        use rune_tools::{Activity, ExecutionContext, Registry, Tool};

        struct HintTool;

        impl Tool for HintTool {
            fn name(&self) -> &'static str {
                "hint"
            }

            fn description(&self) -> &'static str {
                "Return a missing file diagnostic."
            }

            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }

            fn activity(&self) -> Activity {
                Activity::Read
            }

            fn call(
                &self,
                _arguments: &serde_json::Value,
                _context: &ExecutionContext,
            ) -> Result<ToolOutput> {
                Err(RuneError::new(ErrorCode::NotFound, "missing").with_hint("try other.txt"))
            }
        }

        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionhint1");
        let mut registry = Registry::new();
        registry.insert(Box::new(HintTool)).expect("register");
        let output = registry
            .call(
                "hint",
                &serde_json::json!({}),
                &ExecutionContext::new(root.to_owned()),
            )
            .expect("repairable failure");
        let expected = output.render();
        let mut with_call = outcome("I can try the suggested file.");
        with_call.calls.push(rune_agent::turn::CallResult {
            call: PreparedCall {
                id: "call_hint".to_owned(),
                name: "hint".to_owned(),
                arguments: "{}".to_owned(),
            },
            output,
            executed: true,
        });
        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder.user_message("read missing.txt").expect("wrote");
        recorder.turn(&with_call).expect("recorded");
        drop(recorder);

        let state = load_read_only(&paths.session_dir(&key)).expect("read saved events");
        let saved = state
            .events
            .iter()
            .find_map(|frame| match &frame.event {
                SessionEvent::ToolResult {
                    call_id,
                    ok,
                    output,
                } if call_id == "call_hint" => {
                    assert!(!ok);
                    Some(output)
                }
                _ => None,
            })
            .expect("saved tool result");
        assert_eq!(saved, &expected);
        let diagnostic: serde_json::Value = serde_json::from_str(saved).expect("diagnostic JSON");
        assert_eq!(diagnostic["code"], "not_found");
        assert_eq!(diagnostic["message"], "missing");
        assert_eq!(diagnostic["detail"]["hint"], "try other.txt");

        let (_, history) = load(&paths, &key).expect("replay");
        history.validate().expect("replayed failure has an answer");
        assert!(history.turns().iter().flat_map(|turn| &turn.parts).any(|part| {
            matches!(part, ContentPart::ToolResult { content, is_error: true, .. } if content == &expected)
        }));
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
                model: None,
                first_prompt: None,
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
                model: None,
                first_prompt: None,
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
    fn tree_previews_escape_saved_newlines_and_controls_without_injecting_rows() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let key = id("sessionline1");
        let mut recorder = Recorder::create(&paths, &key).expect("created");
        recorder
            .user_message("question\n999 injected\r\t\0\u{1b}[2J")
            .expect("wrote");
        recorder
            .turn(&outcome("answer\r\nDONE\u{85}\u{2028}\u{2029} 世界"))
            .expect("wrote");
        drop(recorder);

        let state = inspect(&paths, &key).expect("inspected");
        let tree = tree_of(&state);
        let previews = [
            "question\\n999 injected\\r\\t\\u{0}\\u{1b}[2J",
            "answer\\r\\nDONE\\u{85}\\u{2028}\\u{2029} 世界",
        ];
        let rendered = render_tree(&tree, &state);
        let lines: Vec<_> = rendered.lines().collect();
        // Session heading, branch summary, blank line, table heading, two nodes.
        assert_eq!(lines.len(), 6, "{rendered}");
        assert!(lines[1].ends_with(previews[1]), "{rendered}");
        for (index, expected) in previews.into_iter().enumerate() {
            let seq = u64::try_from(index + 1).expect("seq");
            assert_eq!(tree.node(seq).expect("node").preview, expected);
            assert!(lines[index + 4].ends_with(expected), "{rendered}");
        }
        assert!(!rendered.chars().any(|ch| ch.is_control() && ch != '\n'));
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
    fn summaries_carry_model_and_first_prompt() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);

        let mut recorder = Recorder::create(&paths, &id("sessionmmmmm")).expect("created");
        for model in ["first-model", "second-model"] {
            recorder
                .store
                .append(SessionEvent::ModelSet {
                    model: model.to_owned(),
                    effort: rune_core::config::Effort::Max,
                })
                .expect("model");
        }
        recorder
            .user_message("opening prompt\nsecond line")
            .expect("wrote");
        recorder.turn(&outcome("answer")).expect("wrote");
        recorder.user_message("later prompt").expect("wrote");
        drop(recorder);
        drop(Recorder::create(&paths, &id("sessionnnnnn")).expect("created"));

        let rows = list_scoped(&paths, None);
        let full = rows
            .iter()
            .find(|row| row.id == "sessionmmmmm")
            .expect("row");
        assert_eq!(full.model.as_deref(), Some("second-model"));
        assert_eq!(
            full.first_prompt.as_deref(),
            Some("opening prompt\nsecond line")
        );
        let empty = rows
            .iter()
            .find(|row| row.id == "sessionnnnnn")
            .expect("row");
        assert_eq!(empty.model, None);
        assert_eq!(empty.first_prompt, None);
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

    /// A completed response with `text`, then one call to `read_file` when
    /// `call` is given.
    fn streamed(text: &str, call: Option<&str>) -> rune_net::transport::StreamOutcome {
        use rune_net::stream::ProviderEvent;
        let mut events = vec![ProviderEvent::TextDelta {
            delta: text.to_owned(),
        }];
        if let Some(call) = call {
            let id = rune_core::id::ToolCallId::new(call).expect("id");
            events.push(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: "read_file".to_owned(),
            });
            events.push(ProviderEvent::ToolCallEnd {
                id,
                arguments: "{\"path\":\"a.rs\"}".to_owned(),
            });
        }
        rune_net::transport::StreamOutcome {
            events,
            ..rune_net::transport::StreamOutcome::default()
        }
    }

    #[test]
    fn a_resumed_history_equals_the_one_in_memory() {
        use rune_agent::history::JournalQueue;
        let root = tempfile::tempdir().expect("tempdir");
        let paths = paths(Utf8Path::from_path(root.path()).expect("utf8"));
        let key = id("fidelity0001");
        let mut recorder = Recorder::create(&paths, &key).expect("create");
        let mut live = History::new();
        let at = |history: &History| u64::try_from(history.len()).expect("length");
        let respond = |recorder: &mut Recorder, live: &mut History, outcome| {
            recorder.response(&outcome).expect("response");
            let (parts, _) = rune_agent::turn::assistant_parts(&outcome);
            live.push_assistant(parts);
        };

        // A prompt with a note before it.
        recorder.user_message("read a.rs").expect("prompt");
        recorder
            .journal(&JournalEntry::Note {
                at: at(&live),
                placement: NotePlacement::PromptPrefix,
                text: "PLAN".to_owned(),
            })
            .expect("note");
        live.push_user("PLAN\n\nread a.rs");

        // A turn that reads a file, answers, is reminded, and answers again.
        respond(&mut recorder, &mut live, streamed("reading", Some("c1")));
        live.push_tool_results(vec![ContentPart::ToolResult {
            id: rune_core::id::ToolCallId::new("c1").expect("id"),
            name: "read_file".to_owned(),
            content: "file text".to_owned(),
            is_error: false,
        }]);
        respond(&mut recorder, &mut live, streamed("done", None));
        let reminder = JournalEntry::Note {
            at: at(&live),
            placement: NotePlacement::UserTurn,
            text: "REMINDER".to_owned(),
        };
        recorder.journal(&reminder).expect("reminder");
        live.push_user("REMINDER");
        respond(&mut recorder, &mut live, streamed("checked", None));
        let mut first = outcome("checked");
        first.calls = vec![rune_agent::turn::CallResult {
            call: PreparedCall {
                id: "c1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"a.rs\"}".to_owned(),
            },
            output: ToolOutput::success("file text"),
            executed: true,
        }];
        recorder.turn(&first).expect("turn");

        // The old result becomes a receipt, then a checkpoint replaces the
        // first exchange.
        let replaced = JournalEntry::ResultReplaced {
            at: at(&live),
            call_id: "c1".to_owned(),
            content: "receipt".to_owned(),
        };
        recorder.journal(&replaced).expect("replaced");
        let mut direct = JournalQueue::default();
        direct.push(replaced);
        direct.apply_ready(&mut live);
        let checkpoint_at = at(&live);
        live.replace_with_summary("<conversation-checkpoint>", 4);
        recorder
            .journal(&JournalEntry::Checkpoint {
                at: checkpoint_at,
                through: 3,
                message: "<conversation-checkpoint>".to_owned(),
            })
            .expect("checkpoint");

        // One more turn.
        recorder.user_message("next").expect("prompt");
        live.push_user("next");
        respond(&mut recorder, &mut live, streamed("fine", None));
        recorder.turn(&outcome("fine")).expect("turn");
        drop(recorder);

        let (_, resumed) = load(&paths, &key).expect("load");
        assert_eq!(resumed.to_messages(), live.to_messages());
        assert_eq!(
            serde_json::to_string(&resumed.to_messages()).expect("resumed"),
            serde_json::to_string(&live.to_messages()).expect("live")
        );
        assert_eq!(resumed.turns()[0].text(), "<conversation-checkpoint>");
        assert!(
            resumed
                .turns()
                .iter()
                .all(|turn| !turn.text().contains("file text"))
        );
    }

    #[test]
    fn rebuilt_scoped_rules_count_as_delivered() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = paths(Utf8Path::from_path(root.path()).expect("utf8"));
        let key = id("scopedrules1");
        let mut recorder = Recorder::create(&paths, &key).expect("create");
        recorder.user_message("read it").expect("prompt");
        let mut read = outcome("");
        read.calls = vec![rune_agent::turn::CallResult {
            call: PreparedCall {
                id: "c1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"a&b/x.rs\"}".to_owned(),
            },
            output: ToolOutput::success(
                "text\n\n<scoped-rules from=\"a&amp;b/AGENTS.md\" applies-to=\"a&amp;b/\">\nUse tabs.\n</scoped-rules>",
            ),
            executed: true,
        }];
        recorder.turn(&read).expect("turn");
        drop(recorder);

        let (_, history) = load(&paths, &key).expect("load");
        assert!(history.is_delivered("rules:a&b/AGENTS.md"));
        assert!(!history.is_delivered("rules:b/AGENTS.md"));
    }

    #[test]
    fn old_journals_ignore_compaction_records() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = paths(Utf8Path::from_path(root.path()).expect("utf8"));
        let key = id("oldjournal01");
        let mut recorder = Recorder::create(&paths, &key).expect("create");
        recorder.user_message("first").expect("prompt");
        recorder.turn(&outcome("one")).expect("turn");
        recorder
            .append(SessionEvent::Compaction {
                through: 2,
                summary: "a summary".to_owned(),
            })
            .expect("compaction");
        recorder.user_message("second").expect("prompt");
        recorder.turn(&outcome("two")).expect("turn");
        drop(recorder);

        let (_, history) = load(&paths, &key).expect("load");
        let texts: Vec<String> = history.turns().iter().map(rune_agent::Turn::text).collect();
        assert_eq!(texts, ["first", "one", "second", "two"]);
    }

    #[test]
    fn explicit_pins_survive_restart_and_compaction_events_verbatim() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = paths(Utf8Path::from_path(root.path()).expect("utf8"));
        let id = id("pinned000001");
        let recorder = Recorder::create(&paths, &id).expect("create");
        let constraints = [
            "Never commit.",
            "Preserve café and 日本語.\nKeep \"quotes\".",
            "  No dependencies.  ",
        ];
        assert!(recorder.pin_constraint(" \n ").is_err());
        recorder.user_message("an ordinary prompt").expect("prompt");
        for text in constraints {
            recorder.pin_constraint(text).expect("persist pin");
        }
        for _ in 0..3 {
            recorder
                .append(SessionEvent::Compaction {
                    through: 1,
                    summary: "A summary that omits every constraint.".to_owned(),
                })
                .expect("compaction event");
        }
        std::fs::remove_file(recorder.store.metadata_path()).expect("remove projection");
        drop(recorder);
        let (_, history) = load(&paths, &id).expect("restart");
        assert_eq!(history.pinned_constraints(), constraints);
        assert_eq!(history.len(), 1, "pins do not become summarizable turns");
        assert_eq!(history.turns()[0].text(), "an ordinary prompt");
        history.validate().expect("valid replay");
        for text in constraints {
            assert!(
                history
                    .to_messages()
                    .iter()
                    .any(|message| message.text().contains(text))
            );
        }
    }

    #[test]
    fn explicit_objectives_survive_reopen_and_compaction_without_entering_history() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(root.path()).expect("utf8");
        let paths = paths(root);
        let id = id("objective001");
        let mut recorder = Recorder::create(&paths, &id).expect("create");
        assert_eq!(recorder.objective(), None);
        recorder
            .user_message("Set a goal to fix this")
            .expect("prompt");
        assert_eq!(recorder.objective(), None);
        assert!(recorder.set_objective(" \n ").is_err());
        assert_eq!(recorder.store.read().expect("state").len(), 1);
        recorder.set_objective("superseded").expect("first goal");
        let objective = "Fix the café parser\nPreserve \"quoted\" inputs and 日本語";
        recorder.set_objective(objective).expect("goal");
        recorder
            .append(SessionEvent::Compaction {
                through: 3,
                summary: "Earlier conversation".to_owned(),
            })
            .expect("compaction");
        std::fs::remove_file(recorder.store.metadata_path()).expect("remove projection");
        drop(recorder);

        let (recorder, history) = load(&paths, &id).expect("restart");
        assert_eq!(recorder.objective(), Some(objective));
        assert_eq!(history.len(), 1);
        assert_eq!(history.turns()[0].text(), "Set a goal to fix this");
        assert_eq!(recorder.store.read().expect("state").turns, 0);
    }
    #[test]
    fn objective_budget_usage_survives_compaction_and_resets_only_on_replacement() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = paths(Utf8Path::from_path(root.path()).expect("utf8"));
        let id = id("budget000001");
        let mut recorder = Recorder::create(&paths, &id).expect("create");
        recorder
            .account_objective_tokens(99)
            .expect("ordinary prompt");
        assert_eq!(recorder.objective_budget(), None);
        assert!(recorder.set_budgeted_objective("work", 0).is_err());
        assert!(recorder.set_budgeted_objective(" ", 10).is_err());
        recorder.set_budgeted_objective("work", 30).expect("goal");
        recorder
            .account_objective_tokens(15)
            .expect("first request");
        recorder
            .append(SessionEvent::Compaction {
                through: 2,
                summary: "Earlier work".to_owned(),
            })
            .expect("compaction");
        drop(recorder);
        let mut recorder = Recorder::open(&paths, &id).expect("resume");
        assert_eq!(recorder.objective(), Some("work"));
        assert_eq!(recorder.objective_budget(), Some((30, 15)));
        recorder
            .account_objective_tokens(u64::MAX)
            .expect("saturating usage");
        drop(recorder);
        let mut recorder = Recorder::open(&paths, &id).expect("resume exhausted");
        assert_eq!(recorder.objective_budget(), Some((30, u64::MAX)));
        recorder
            .set_budgeted_objective("new work", 20)
            .expect("replace");
        assert_eq!(recorder.objective_budget(), Some((20, 0)));
        recorder.set_objective("unbudgeted work").expect("replace");
        recorder
            .account_objective_tokens(50)
            .expect("unbudgeted request");
        drop(recorder);
        let recorder = Recorder::open(&paths, &id).expect("resume unbudgeted");
        assert_eq!(recorder.objective(), Some("unbudgeted work"));
        assert_eq!(recorder.objective_budget(), None);
    }
}
