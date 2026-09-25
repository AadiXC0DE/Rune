//! The interactive session.
//!
//! Connects the terminal shell loop to the agent. One turn runs at a time; a
//! line typed while a turn is running is queued rather than refused, which is
//! what makes the shell usable while the model is working.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, Write as _};
use std::sync::{Arc, Mutex};

use crate::session_log::{self, Recorder};
use camino::{Utf8Path, Utf8PathBuf};
use rune_agent::history::History;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{self, Event, Host, StopReason};
use rune_context::prompt::{self, Prompt};
use rune_core::budget::BudgetSet;
use rune_core::config::{Effort, PermissionMode, Settings};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::SessionId;
use rune_core::paths::Paths;
use rune_net::message::ToolSpec;
use rune_net::provider::Provider;
use rune_net::transport::Endpoint;
use rune_policy::decision::Outcome;
use rune_policy::review::{ReviewOutcome, ReviewRequest, ReviewSession, Reviewer};
use rune_policy::rules::RuleSet;
use rune_session::usage::{HelperKind, Ledger, UsageRecord, now_ms};
use rune_term::footer::{self, FooterState};
use rune_term::input::KeyAction;
use rune_term::shell::ExitReason;
use rune_term::shell::{Action, Input, Shell};
use rune_term::theme::{Slot, Theme};
use rune_term::transcript::{self, Display, Entry};
use rune_tools::contract::{ExecutionContext, ToolOutput};
use rune_tools::inventory;
use rune_tools::registry::Registry;

/// Everything a session needs to run.
pub struct SessionConfig {
    /// Resolved settings.
    pub settings: Settings,
    /// State paths, used for the session log.
    pub paths: Paths,
    /// Session to resume, when the launch asked to resume one.
    pub resume: Option<SessionId>,
    /// Primary workspace.
    pub workspace: Utf8PathBuf,
    /// Endpoint for model requests.
    pub endpoint: Endpoint,
    /// Dialect the endpoint speaks.
    pub dialect: Box<dyn Provider>,
    /// Tool registry.
    pub registry: Registry,
    /// Rules in force.
    pub rules: RuleSet,
}

/// A writer that locks the shared stream for the length of one write.
///
/// Holding the lock across a turn would deadlock: the turn draws streamed text
/// through the same stream, so it would wait on a lock its own caller holds.
/// Locking per write keeps the bytes ordered without ever holding it that long.
struct LockedSink {
    stream: LiveSink,
}

impl std::io::Write for LockedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| std::io::Error::other("the output lock was poisoned"))?;
        stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| std::io::Error::other("the output lock was poisoned"))?;
        stream.flush()
    }
}

/// Text that has streamed in but is not yet a finished line.
///
/// Held apart from the transcript because it is redrawn on every delta: the
/// transcript is printed once, while this is replaced in place until the step
/// ends and the text becomes final.
#[derive(Default)]
struct StreamingText {
    /// Assistant text so far in the current step.
    answer: String,
    /// Reasoning text so far, shown while the model is thinking.
    reasoning: String,
    /// Wrapped rows for each lane, and how much of the source they cover.
    ///
    /// Kept so a delta wraps only the text that arrived, rather than the whole
    /// answer again. Re-wrapping everything per delta is quadratic in the length
    /// of the response, which is what made a long answer stutter.
    answer_rows: LazyRows,
    reasoning_rows: LazyRows,
}

/// Wrapped rows derived from a growing string.
///
/// Every complete line is wrapped once and kept. Only the line being written is
/// wrapped again on the next delta, so the cost of a delta is bounded by one
/// line rather than by the whole answer. Rewrapping everything per delta is
/// quadratic in the length of the response, which is what made a long answer
/// stutter: a twenty thousand character answer was re-wrapped twenty million
/// characters' worth.
///
/// The boundary is a newline because that is a hard break the wrapper already
/// respects, so a line that is complete can never be wrapped differently by
/// later text.
#[derive(Default)]
struct LazyRows {
    /// Rows for every line that is finished, ending in a newline.
    finished: Vec<String>,
    /// Bytes of the source those rows cover.
    covered: usize,
}

/// Returns the rows for a growing string, wrapping only what is new.
fn rows_for(lazy: &mut LazyRows, text: &str, width: usize) -> Vec<String> {
    // Text that shrank means the lane was cleared, so the cached rows describe
    // a response that is gone.
    if text.len() < lazy.covered {
        lazy.finished.clear();
        lazy.covered = 0;
    }

    // Everything up to the last newline is final. A newline is a hard break, so
    // no later text can change how the text before it wraps, and the wrap for
    // that prefix is computed once no matter how many deltas follow it.
    let sealed = text.rfind('\n').map_or(0, |at| at.saturating_add(1));
    if sealed != lazy.covered {
        lazy.finished = rune_term::width::wrap(text.get(..sealed).unwrap_or_default(), width);
        lazy.covered = sealed;
    }

    let tail = text.get(lazy.covered..).unwrap_or_default();
    if tail.is_empty() {
        return lazy.finished.clone();
    }

    // The tail continues the row the sealed prefix ended on. A prefix ending in
    // a newline wraps to a trailing empty row, which the tail fills rather than
    // sitting under, so that row is dropped before the tail is appended.
    let mut rows = lazy.finished.clone();
    rows.pop();
    rows.extend(rune_term::width::wrap(tail, width));
    rows
}

/// The stream a session draws on.
///
/// Shared rather than borrowed because the host draws from inside a turn, where
/// the caller holds no lock. Every write takes the lock for its own duration
/// only, so a delta arriving mid-turn can never wait on the turn that is
/// producing it.
type LiveSink = Arc<Mutex<dyn std::io::Write + Send>>;

/// Host state for a turn.
struct SessionHost {
    endpoint: Endpoint,
    dialect: Box<dyn Provider>,
    /// Model the session sends to, changeable while the session runs.
    ///
    /// Behind a lock because a slash command changes it from the input loop
    /// while a turn reads it on the turn's own thread. It is read once per
    /// request attempt, so the lock is not on the streaming path.
    model: Mutex<String>,
    instructions: String,
    tools: Vec<ToolSpec>,
    rules: RuleSet,
    mode: PermissionMode,
    effort: Effort,
    fast_mode: bool,
    limits: BudgetSet,
    context: ExecutionContext,
    registry: Registry,
    cancellation: Cancellation,
    steering: SteeringQueue,
    events: Arc<Mutex<Vec<Event>>>,
    /// Tokens spent from the context window, summed across turns.
    context_used: std::sync::atomic::AtomicU64,
    /// Size of the context window, zero when the provider stated none.
    ///
    /// Atomic because choosing another model mid-session changes the window the
    /// status line is measured against, and the status line only has `&self`.
    context_limit: std::sync::atomic::AtomicU64,
    /// Theme the status line is drawn with.
    theme: Theme,
    /// Session identifier, shown shortened in the status line.
    ///
    /// Behind a lock because `/new` replaces it while the session runs, and the
    /// status line reads it through `&self`.
    session_id: Mutex<String>,
    /// Workspace, shown as given.
    workspace: String,
    /// Whether the terminal can render direct color.
    truecolor: bool,
    /// Terminal width the live region is drawn for.
    ///
    /// Held behind an atomic because a resize is noticed while the region is
    /// being drawn, and the width the status line was measured against has to
    /// move with it.
    width: std::sync::atomic::AtomicU16,
    /// Terminal height, kept for the layout the status line is solved in.
    height: std::sync::atomic::AtomicU16,
    /// Draws the live region and owns every write to the terminal.
    inline: Mutex<rune_term::inline::Inline>,
    /// Ordered upstream provider preference.
    provider_order: Vec<String>,
    /// Whether requests are restricted to that preference.
    provider_strict: bool,
    /// Escape that opens a reasoning line, resolved from the theme once.
    reasoning: Mutex<String>,
    /// Text streamed so far in the current step, drawn below the input.
    ///
    /// Shared and mutable because a delta arrives on the turn's own thread
    /// while the renderer needs to read what has accumulated.
    streaming: Arc<Mutex<StreamingText>>,
    /// Where the live region is written, so a delta can be shown as it lands.
    ///
    /// Absent until a session attaches one, which keeps every other caller of
    /// this host, including the tests, free of a terminal.
    live_out: Arc<Mutex<Option<LiveSink>>>,
    /// Reviewer for unresolved actions, absent when none is configured.
    reviewer: Option<Box<dyn Reviewer>>,
    /// What this session has spent, for `/cost`.
    totals: Mutex<Totals>,
    /// The bytes each file held before this session first wrote to it, for
    /// `/undo`.
    ///
    /// Keyed by path and written once per file, so the entry is the state
    /// before the session's first change rather than before its latest: putting
    /// a file back to where a chain of edits began is what a single undo
    /// command can honestly do. A file the session created is held as absent,
    /// so undoing removes it.
    undo: Mutex<BTreeMap<Utf8PathBuf, Option<Vec<u8>>>>,
    /// Review activity for the current turn.
    review_session: Arc<Mutex<ReviewSession>>,
}

/// Locks the model, recovering from a poisoned lock.
///
/// A panic elsewhere must not make the session unable to name its own model,
/// so the value is taken as it stands.
fn lock_model(model: &Mutex<String>) -> std::sync::MutexGuard<'_, String> {
    model
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Locks the review session, recovering from a poisoned lock.
///
/// A panic while holding this lock must not make every later tool call fail, so
/// the state is taken as it stands.
fn lock_review(session: &Mutex<ReviewSession>) -> std::sync::MutexGuard<'_, ReviewSession> {
    session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Host for SessionHost {
    fn dialect(&self) -> &dyn Provider {
        self.dialect.as_ref()
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn model(&self) -> String {
        lock_model(&self.model).clone()
    }

    fn instructions(&self) -> String {
        self.instructions.clone()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }

    fn effort(&self) -> Effort {
        self.effort
    }

    fn fast_mode(&self) -> bool {
        self.fast_mode
    }

    fn emit(&self, event: Event) {
        // Text is accumulated as it arrives and drawn straight away, which is
        // what makes an answer appear while it is being written rather than
        // after the whole response has been received.
        match &event {
            Event::TextDelta { delta } => {
                if let Ok(mut streaming) = self.streaming.lock() {
                    streaming.answer.push_str(delta);
                }
                self.draw_stream();
            }
            Event::ReasoningDelta { delta } => {
                if let Ok(mut streaming) = self.streaming.lock() {
                    streaming.reasoning.push_str(delta);
                }
                self.draw_stream();
            }
            _ => {}
        }
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }

    fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        // Captured before the call so the bytes that were there can be put
        // back. A capture failure is not fatal: the call still runs, and `/undo`
        // reports that it had nothing to restore for this file.
        if let Some(path) = mutating_path(name, arguments) {
            self.remember(path);
        }
        self.registry.call(name, arguments, &self.context)
    }

    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        let (outcome, reason) = turn::decide_call(&self.rules, self.mode, name, target);
        if outcome != Outcome::Ask {
            return (outcome, reason);
        }

        // In automatic mode an unresolved action gets one narrow review rather
        // than a person's attention. The review never opens a prompt and never
        // ends the turn: a caution or an unreachable reviewer holds the action
        // and hands the agent guidance.
        if self.mode == PermissionMode::Auto
            && let Some(reviewer) = &self.reviewer
        {
            let action = target.unwrap_or(name).to_owned();
            let request = ReviewRequest::new(action.clone(), vec![action.clone()], name, "");
            let mut session = lock_review(&self.review_session);
            return match session.review(reviewer.as_ref(), &request) {
                ReviewOutcome::Clear { .. } => {
                    (Outcome::Allow, format!("{reason}; cleared by review"))
                }
                other => (
                    Outcome::Deny,
                    other
                        .reason()
                        .map_or_else(|| String::from("review held the action"), str::to_owned),
                ),
            };
        }

        // Nothing has judged the action, so it stays unresolved. Being
        // interactive is not a judgment: approving here would authorize an
        // action that no rule allowed and no reviewer saw.
        (outcome, reason)
    }

    fn context(&self) -> ExecutionContext {
        // Sharing the cancellation flag is what lets a cancel reach every tool
        // in the turn rather than only the one running when it arrived.
        self.context.fork()
    }

    fn limits(&self) -> BudgetSet {
        self.limits.clone()
    }

    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    fn steering(&self) -> &SteeringQueue {
        &self.steering
    }

    fn provider_order(&self) -> Vec<String> {
        self.provider_order.clone()
    }

    fn provider_strict(&self) -> bool {
        self.provider_strict
    }
}

impl SessionHost {
    /// Points the session at another model.
    ///
    /// The next request uses it. A turn already running keeps the model it
    /// started with, because its request has already been sent.
    ///
    /// The context window moves with the model only when it is known: a window
    /// left over from a larger model would understate how full the smaller one
    /// is, and a window invented for a model nothing describes would be a guess
    /// presented as a fact.
    fn set_model(&self, model: &str, context_window: Option<u64>) {
        {
            let mut current = lock_model(&self.model);
            model.clone_into(&mut current);
        }
        if let Some(window) = context_window.filter(|window| *window > 0) {
            self.context_limit
                .store(window, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Returns the theme the interface is drawn with.
    fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Returns whether the terminal renders direct color.
    const fn truecolor(&self) -> bool {
        self.truecolor
    }

    /// Returns the model the session is sending to.
    fn model_name(&self) -> String {
        lock_model(&self.model).clone()
    }

    /// Returns whether a model has been chosen.
    fn has_model(&self) -> bool {
        !lock_model(&self.model).trim().is_empty()
    }

    /// Adopts a model's real context window, when the endpoint reports one.
    ///
    /// The configured window wins, because a user who declared one is describing
    /// the model they selected and the endpoint may advertise a number that
    /// counts only part of the conversation. When nothing is configured the
    /// endpoint's figure is far better than the compiled default, which
    /// understates a large model so badly that a session looks nearly full
    /// before it has said anything.
    fn adopt_context_window(&self, reported: Option<u64>, configured: bool) {
        if configured {
            return;
        }
        if let Some(window) = reported.filter(|window| *window > 0) {
            self.context_limit
                .store(window, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Reports the running state for the read-only commands.
    ///
    /// Borrowed from live state rather than collected at the start, so a status
    /// line read after `/model` names the model now in effect.
    fn info<'a>(&'a self, provider: &'a str, endpoint: &'a str) -> SessionInfo<'a> {
        SessionInfo {
            model: self.model_name(),
            provider,
            endpoint,
            mode: self.mode,
            effort: self.effort,
            session_id: self.session_id_name(),
            workspace: &self.workspace,
            context_used: self.context_used.load(std::sync::atomic::Ordering::Relaxed),
            context_limit: self
                .context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            totals: self
                .totals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    /// Adds one turn's usage to the session total.
    fn record_usage(&self, usage: &rune_net::stream::Usage) {
        self.totals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(usage);
    }

    /// Remembers a file's contents before this session changes it.
    ///
    /// Only the first capture for a path is kept, so an undo returns the file
    /// to the state it was in before the session touched it rather than to some
    /// intermediate state that only makes sense in the middle of a chain.
    fn remember(&self, path: &Utf8Path) {
        let mut undo = self
            .undo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if undo.contains_key(path) {
            return;
        }
        // A file that is absent or unreadable is recorded as absent, so undoing
        // a creation removes the file and an unreadable file is not silently
        // reported as restored.
        undo.insert(path.to_owned(), std::fs::read(path).ok());
    }

    /// Returns whether anything is available to undo.
    fn can_undo(&self) -> bool {
        !self
            .undo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Puts every changed file back and forgets the record.
    ///
    /// Returns one line per file, describing what was done, or a failure
    /// naming the file that could not be restored. Files are restored before
    /// the record is cleared, so a failure leaves the rest still undoable.
    fn undo(&self) -> Result<Vec<String>> {
        let mut undo = self
            .undo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut report = Vec::with_capacity(undo.len());
        let mut done: Vec<Utf8PathBuf> = Vec::with_capacity(undo.len());
        for (path, before) in undo.iter() {
            match before {
                Some(bytes) => {
                    std::fs::write(path, bytes).map_err(|err| {
                        RuneError::new(
                            ErrorCode::Internal,
                            format!("`{path}` could not be restored: {err}"),
                        )
                    })?;
                    report.push(format!("restored {path}"));
                }
                None => {
                    // A file that did not exist before is removed, which is what
                    // undoing its creation means.
                    match std::fs::remove_file(path) {
                        Ok(()) => report.push(format!("removed {path}")),
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                            report.push(format!("{path} was already gone"));
                        }
                        Err(err) => {
                            return Err(RuneError::new(
                                ErrorCode::Internal,
                                format!("`{path}` could not be removed: {err}"),
                            ));
                        }
                    }
                }
            }
            done.push(path.clone());
        }
        for path in done {
            undo.remove(&path);
        }
        Ok(report)
    }

    /// Records how large the conversation has become.
    ///
    /// Takes the largest reading rather than summing them. Each turn resends
    /// the whole conversation, so an input count already includes every earlier
    /// turn: adding them counts the same history once per turn, which is what
    /// made a session appear to fill its window several times over.
    fn record_context_size(&self, used: u64) {
        self.context_used
            .fetch_max(used, std::sync::atomic::Ordering::Relaxed);
    }

    /// Forgets the context reading, for a conversation that has been replaced.
    fn forget_context(&self) {
        self.context_used
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns the identifier of the session now running.
    fn session_id_name(&self) -> String {
        self.session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Points the status line at a new session.
    fn set_session_id(&self, id: &str) {
        let mut slot = self
            .session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        id.clone_into(&mut slot);
    }

    /// Returns the status line for the current state.
    fn status_line(&self, width: usize) -> String {
        let state = FooterState {
            model: lock_model(&self.model).clone(),
            permission_mode: self.mode,
            workspace: self.workspace.clone(),
            context_used: self.context_used.load(std::sync::atomic::Ordering::Relaxed),
            context_limit: self
                .context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            session_id: self
                .session_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        };
        let layout = footer::solve(
            (
                u16::try_from(width).unwrap_or(80),
                self.height.load(std::sync::atomic::Ordering::Relaxed),
            ),
            1,
            false,
            footer::DEFAULT_MINIMUM_ROWS,
        );
        footer::render(&state, &layout, &self.theme, width, self.truecolor).join("\n")
    }

    /// Returns the line the activity line should show for a finished turn.
    #[must_use]
    pub fn activity_line(outcome: &turn::TurnOutcome) -> Option<String> {
        match outcome.stop_reason {
            StopReason::StepLimit => Some(String::from("reached the step limit")),
            StopReason::Cancelled => Some(String::from("cancelled")),
            _ => None,
        }
    }

    /// Returns the current terminal width.
    fn width(&self) -> u16 {
        self.width.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Takes the terminal's current size, keeping the last known one when the
    /// terminal cannot report it.
    ///
    /// Read every frame rather than once, because a resized window would
    /// otherwise keep a layout measured for the old width.
    fn refresh_size(&self) {
        let Some((cols, rows)) = rune_term::shell::terminal_size() else {
            return;
        };
        if cols == 0 || rows == 0 {
            return;
        }
        self.width.store(cols, std::sync::atomic::Ordering::Relaxed);
        self.height
            .store(rows, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut inline) = self.inline.lock() {
            inline.set_width(cols);
            // One row of slack, so a region that fills the screen still has
            // somewhere to move the caret and never writes into the terminal's
            // last row, which is what makes it scroll.
            inline.set_max_rows(rows.saturating_sub(1));
        }
    }

    /// Draws the live region and returns the bytes the terminal must receive.
    ///
    /// This is the only method that produces output for the interactive path. A
    /// line printed beside it would move the rows the region is drawn at, so
    /// there is exactly one writer: finished lines are handed here and printed
    /// once, and everything after them is redrawn in place.
    fn paint(
        &self,
        settled: &[String],
        activity: Option<&str>,
        prompt: &[String],
        below: &[String],
        caret: (u16, u16),
    ) -> Result<Vec<u8>> {
        self.refresh_size();
        let footer_rows = self.status_rows();
        let mut inline = self
            .inline
            .lock()
            .map_err(|_| RuneError::new(ErrorCode::Internal, "the renderer lock was poisoned"))?;
        Ok(inline.frame(settled, activity, &footer_rows, prompt, below, caret))
    }

    /// Removes the live region, for a clean exit.
    fn clear_region(&self) -> Result<Vec<u8>> {
        let mut inline = self
            .inline
            .lock()
            .map_err(|_| RuneError::new(ErrorCode::Internal, "the renderer lock was poisoned"))?;
        Ok(inline.clear())
    }

    /// Returns the footer rows for the current state.
    fn status_rows(&self) -> Vec<String> {
        self.status_line(usize::from(self.width()))
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Writes bytes to the session's stream.
    ///
    /// Silently ignores a failure. Presentation must never be able to end a
    /// turn, and a closed stream is not a reason to lose a conversation.
    fn show(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Ok(slot) = self.live_out.lock() else {
            return;
        };
        let Some(sink) = slot.as_ref() else {
            return;
        };
        if let Ok(mut sink) = sink.lock() {
            let _ = sink.write_all(bytes);
            let _ = sink.flush();
        }
    }

    /// Draws the text that has streamed in so far.
    ///
    /// A failure is ignored: not being able to present a delta must never end a
    /// turn or fail a request that is otherwise fine.
    fn draw_stream(&self) {
        let rows = self.streaming_rows();
        if rows.is_empty() {
            return;
        }
        let (prompt_row, caret) = self.idle_prompt();
        let Ok(painted) = self.paint(&[], None, &prompt_row, &rows, caret) else {
            return;
        };
        self.show(&painted);
    }

    /// Returns the empty input row, with the caret at its start.
    ///
    /// Every frame draws this, so the region is the same shape whether text is
    /// arriving or not and the caret always has a row of its own.
    fn idle_prompt(&self) -> (Vec<String>, (u16, u16)) {
        let marker = rune_term::shell::prompt();
        let row = transcript::render_prompt(marker, "", usize::from(self.width()));
        let caret = rune_term::width::str_width(marker);
        (vec![row], (0, u16::try_from(caret).unwrap_or(u16::MAX)))
    }

    /// Returns the escape that opens the reasoning lane.
    ///
    /// Resolved from the theme once per call and cached on the host, because
    /// `Lanes` holds a `&'static str` and the renderer must not allocate per
    /// line.
    fn reasoning_style(&self) -> String {
        self.reasoning
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Returns the rows the streamed text occupies, newest last.
    fn streaming_rows(&self) -> Vec<String> {
        let Ok(mut streaming) = self.streaming.lock() else {
            return Vec::new();
        };
        let width = usize::from(self.width());
        let dim = self.theme.sgr(Slot::Dim, self.truecolor);
        let reset = rune_term::engine::Style::RESET;

        // The fields are borrowed separately, because each lane's text and the
        // rows derived from it are written together.
        let StreamingText {
            answer,
            reasoning,
            answer_rows,
            reasoning_rows,
        } = &mut *streaming;

        // Reasoning comes first, drawn apart from the answer, in a secondary
        // colour and indented, so a reader can tell thinking from the reply.
        let mut rows: Vec<String> = Vec::new();
        if !reasoning.trim().is_empty() {
            for line in rows_for(reasoning_rows, reasoning, width) {
                rows.push(format!("{dim}  {line}{reset}"));
            }
        }
        if !answer.trim().is_empty() {
            for line in rows_for(answer_rows, answer, width) {
                rows.push(line.clone());
            }
        }

        // Every row of the answer is handed over. The renderer owns the region
        // height, because only it knows how tall the region it drew was and
        // therefore which rows it can paint over. Trimming here as well would
        // mean two components each holding a different idea of the limit, and
        // the rows dropped here could not be erased by the frame that follows.
        rows
    }

    /// Clears the streamed text, which a finished step has taken over.
    fn clear_streaming(&self) {
        if let Ok(mut streaming) = self.streaming.lock() {
            streaming.answer.clear();
            streaming.reasoning.clear();
            streaming.answer_rows = LazyRows::default();
            streaming.reasoning_rows = LazyRows::default();
        }
    }

    /// Drops the events of the turn that just finished.
    fn clear_events(&self) {
        if let Ok(mut events) = self.events.lock() {
            events.clear();
        }
    }
}

/// Runs an interactive session.
///
/// Returns the exit code the process should use.
pub fn run<R: BufRead, W: std::io::Write + Send + 'static>(
    config: SessionConfig,
    input: R,
    output: W,
) -> Result<u8> {
    let limits = config.settings.limits.clone();
    let prompt = build_prompt(&config.workspace, &config.paths.config_root, &limits);

    // A resumed session continues its stored conversation; a new one starts
    // empty and writes a fresh log.
    let (mut recorder, mut history) = if let Some(id) = &config.resume {
        session_log::load(&config.paths, id)?
    } else {
        let id = SessionId::generate();
        let recorder = Recorder::create(&config.paths, &id)?;
        // Recorded before the first turn, so the session is attributable to its
        // workspace even if the run ends immediately.
        recorder.set_workspace(&config.workspace)?;
        (recorder, History::new())
    };

    // Opened once. A history that failed to load is not fatal: recall is a
    // convenience, and losing it must not cost the session.
    let mut history_file = crate::prompt_history::History::open(&config.paths).ok();

    // Discovered once, because a command file that changes mid-session would
    // otherwise make an invocation mean two different things.
    let commands =
        rune_context::commands::discover(&config.workspace, &Paths::from_process().config_root)
            .unwrap_or_default();

    // Resolved before the host literal because the registry is moved into it,
    // and because the reasoning lane's colour has to be read off the theme
    // before the theme itself moves into the host.
    let theme = resolve_theme(&config);
    let reasoning_escape = theme.sgr(Slot::Dim, truecolor_supported());

    // Copied before the endpoint moves into the host, so `/status` and the
    // picker can name the address and the provider without the credential that
    // travels beside the endpoint.
    let endpoint_url = config.endpoint.base_url.clone();
    let provider_name = config.settings.provider.to_string();

    let host = SessionHost {
        endpoint: config.endpoint,
        dialect: config.dialect,
        model: Mutex::new(config.settings.model.clone()),
        instructions: prompt.instructions,
        tools: inventory::advertisement(&config.registry),
        rules: config.rules,
        mode: config.settings.permission_mode,
        effort: config.settings.effort,
        fast_mode: config.settings.fast_mode,
        limits: limits.clone(),
        context: ExecutionContext::new(config.workspace.clone())
            .with_allow_unsandboxed(config.settings.allow_unsandboxed),
        registry: config.registry,
        cancellation: Cancellation::new(),
        steering: SteeringQueue::from_limits(&limits),
        events: Arc::new(Mutex::new(Vec::new())),
        context_used: std::sync::atomic::AtomicU64::new(0),
        context_limit: std::sync::atomic::AtomicU64::new(context_limit(&config.settings, &limits)),
        theme,
        session_id: Mutex::new(recorder.id().to_string()),
        workspace: config.workspace.to_string(),
        truecolor: truecolor_supported(),
        // Seeded from the terminal and refreshed on every frame, so a window
        // resized while the session runs is picked up.
        width: std::sync::atomic::AtomicU16::new(terminal_width()),
        height: std::sync::atomic::AtomicU16::new(terminal_height()),
        inline: Mutex::new(rune_term::inline::Inline::new(terminal_width())),
        provider_order: config.settings.provider_order.clone(),
        provider_strict: config.settings.provider_strict,
        streaming: Arc::new(Mutex::new(StreamingText::default())),
        live_out: Arc::new(Mutex::new(None)),
        reasoning: Mutex::new(reasoning_escape),
        reviewer: crate::auto_review::build(&config.settings, &config.paths)?,
        totals: Mutex::new(Totals::default()),
        undo: Mutex::new(BTreeMap::new()),
        review_session: Arc::new(Mutex::new(ReviewSession::new(&limits))),
    };

    // The stream is shared: the loop writes to it, and the host writes to it
    // while a turn is running so streamed text appears as it arrives. Sharing
    // one lock rather than nesting two is what keeps those writes ordered.
    let out: LiveSink = Arc::new(Mutex::new(output));
    if let Ok(mut slot) = host.live_out.lock() {
        *slot = Some(Arc::clone(&out));
    }

    // The context window is resolved before anything is drawn. The session used
    // to announce itself first and correct the figure a moment later, which
    // meant the status line showed the compiled default for as long as the
    // lookup took and then changed under the reader. A wrong number that
    // corrects itself is worse than a number that arrives late: it is
    // indistinguishable from a right one while it is on screen.
    if !config.settings.offline {
        resolve_context_window(&host, &config.settings, &config.paths);
    }

    // The session identifier is announced up front so a resumed-or-new session
    // can be named later without consulting the listing. It goes through the
    // renderer like everything else, so the rows it occupies are known to the
    // component that later redraws over them.
    {
        let banner = format!("session {}", recorder.id());
        let (prompt_row, caret) = host.idle_prompt();
        let painted = host.paint(std::slice::from_ref(&banner), None, &prompt_row, &[], caret)?;
        if let Ok(mut sink) = out.lock() {
            sink.write_all(&painted)?;
            sink.flush()?;
        }
    }

    // A resumed session keeps the title it was given.
    let mut is_first_prompt = config.resume.is_none() && recorder.title_is_unset();

    // Keys are read directly when a terminal is attached, so the line being
    // typed is drawn by this program with a cursor placed where it belongs.
    // Without that, the terminal echoes each key at a position this program
    // does not know, and the two disagree about what is on the line.
    let mut reader = rune_term::input::KeyReader::new();
    let keyed = reader.is_active();

    // A session with no model asks for one now, before the first turn, because
    // a turn cannot be sent without an identifier. A terminal drives the picker,
    // and a caller that cannot answer is refused here rather than at the
    // endpoint, where an empty model comes back as an opaque protocol error.
    if !config.settings.has_model() {
        if !keyed {
            return Err(rune_core::config::unconfigured_model_error());
        }
        choose_model(&mut reader, &host, &out, &config.settings, &config.paths)?;
        if !host.has_model() {
            // The picker was cancelled, so there is still nothing to send to.
            return Err(rune_core::config::unconfigured_model_error());
        }
    }

    // Held rather than read off the config at the point of use, because the
    // input closure borrows the config and a model chosen mid-session has to be
    // able to build a catalog from the same settings the session started with.
    let settings = config.settings.clone();

    // The prompts already recorded in this workspace, oldest first, which is
    // the order the up arrow walks backwards through. Refreshed after each
    // turn so a prompt typed now is recallable next, and read from the file
    // rather than kept beside it so there is one source of truth.
    let mut recall = recorded_prompts(history_file.as_ref(), &config.workspace);

    let mut source = rune_term::shell::StdinSource::new(input);
    let mut shell = Shell::new(&mut source);

    // One handler for both input paths, so a keystroke and a piped line mean
    // exactly the same thing.
    //
    // The model commands are reported to the caller rather than acted on here,
    // because choosing one needs the input reader to run a picker and the
    // status line to be redrawn, both of which live outside this closure.
    //
    // The recorded prompts are passed in rather than captured, so the loop that
    // owns them can re-read them between submissions for the up arrow.
    let mut handle_input = |input: Input,
                            sink: &mut LockedSink,
                            history_file: &mut Option<crate::prompt_history::History>|
     -> Result<Step> {
        match input {
            Input::Command { name, arguments } => {
                // Built here rather than captured, so the status a command
                // reports is the state at the moment it runs.
                let info = host.info(&provider_name, &endpoint_url);
                // Command output is buffered rather than written straight out,
                // because it has to be committed through the renderer: text
                // written directly lands at the caret, inside the region the
                // session repaints, and the screen then shows a mix of the two.
                let mut captured: Vec<u8> = Vec::new();
                let handled = handle_command(
                    &name,
                    &arguments,
                    &commands,
                    history_file.as_ref(),
                    &config.workspace,
                    &info,
                    &mut captured,
                )?;
                let mut note = |text: String| captured.extend_from_slice(text.as_bytes());
                match handled {
                    Handled::Exit => return Ok(Step::Exit),
                    Handled::PickModel => return Ok(Step::PickModel),
                    Handled::Copy => {
                        // The reply is read from the conversation rather than
                        // from the screen, because what is on screen has been
                        // wrapped and styled and is no longer the text.
                        match last_reply(&history) {
                            Some(reply) => match copy_to_clipboard(&reply) {
                                Ok(()) => note(format!(
                                    "copied {} character(s) to the clipboard",
                                    reply.chars().count()
                                )),
                                Err(err) => note(format!("could not copy: {err}")),
                            },
                            None => note("there is no reply to copy yet".to_owned()),
                        }
                    }
                    Handled::NewSession => {
                        // Handled here because the recorder and the conversation
                        // both live in this closure, and starting a fresh
                        // conversation means replacing both together.
                        let id = SessionId::generate();
                        let fresh = Recorder::create(&config.paths, &id)?;
                        fresh.set_workspace(&config.workspace)?;
                        recorder = fresh;
                        history = History::new();
                        host.set_session_id(id.as_str());
                        // The context reading belongs to the conversation that
                        // just ended, so it is cleared rather than carried into
                        // a session that has sent nothing.
                        host.forget_context();
                        note(format!("started session {id}"));
                    }
                    Handled::Undo => return Ok(Step::Undo),
                    Handled::Compact => {
                        // Handled here because this closure already mutably
                        // borrows the conversation, and a second borrower would
                        // not compile for the right reason.
                        return compact_history(&host, &mut history, sink);
                    }
                    Handled::Rename(title) => {
                        // Handled here because this closure already holds the
                        // recorder, and giving the loop a second way to reach
                        // it would be two writers for one file.
                        recorder.set_title(&session_log::derive_title(&title))?;
                        note(format!("renamed this session to {title}"));
                    }
                    Handled::SetModel(model) => {
                        // Named inline, so no catalog entry describes it and the
                        // window already in force is kept rather than guessed.
                        host.set_model(&model, None);
                        note(format!("model set to {}", host.model_name()));
                    }
                    Handled::ClearHistory => {
                        // Forgetting is reported, because a silent success would
                        // leave the user unsure whether anything was removed.
                        match history_file.as_mut() {
                            Some(history) => {
                                let _ = history.clear();
                                note("forgot every recorded prompt".to_owned());
                            }
                            None => note("no prompt history is available".to_owned()),
                        }
                    }
                    // Expanded text goes to the composer for review, never
                    // straight to the model: a template with a wrong argument
                    // should be visible before it is sent.
                    Handled::Expand(prompt) => {
                        note(format!(
                            "-- /{name} expanded; edit before sending --\n{prompt}"
                        ));
                    }
                    Handled::Continue => {}
                }
                let lines = output_lines(&captured);
                flush_lines(&host, sink, &lines)?;
                Ok(Step::Continue)
            }
            Input::Prompt(text) => {
                // The submitted line is committed to the flow before the turn
                // runs, so what the user typed stays on screen once the reply
                // replaces the region it was typed in.
                let echo = transcript::render_prompt(
                    rune_term::shell::prompt(),
                    &text,
                    usize::from(host.width()),
                );
                // The typed line is committed and an empty input row is drawn
                // under it, so the caret has its own row from the moment the
                // line is submitted rather than only once streaming starts.
                let (prompt_row, caret) = host.idle_prompt();
                let painted =
                    host.paint(std::slice::from_ref(&echo), None, &prompt_row, &[], caret)?;
                sink.write_all(&painted)?;
                sink.flush()?;

                if is_first_prompt {
                    recorder.set_title(&session_log::derive_title(&text))?;
                    is_first_prompt = false;
                }
                // Recorded before the turn runs, so a prompt that is interrupted
                // is still recallable.
                if let Some(history) = history_file.as_mut() {
                    let entry = crate::prompt_history::Entry::new(text.clone())
                        .located(&config.workspace, recorder.id().as_str());
                    let _ = history.record(entry);
                }
                recorder.user_message(&text)?;
                history.push_user(text);
                let outcome = turn::run_turn(&mut history, &host)?;
                // The turn is recorded before it is reported, so a session that
                // dies while rendering still has its exchange on disk.
                recorder.turn(&outcome)?;
                // The model is read back rather than captured at start, so a
                // turn that ran after `/model` is billed to the model that ran.
                record_usage(&config.paths, &host.model(), &outcome);
                host.record_usage(&outcome.usage);
                host.record_context_size(
                    outcome
                        .usage
                        .input_tokens
                        .unwrap_or(0)
                        .saturating_add(outcome.usage.output_tokens.unwrap_or(0)),
                );
                host.clear_events();
                // The streamed text has been superseded by the finished turn,
                // so it is dropped before the render that replaces it.
                host.clear_streaming();
                // A finished turn is printed once and becomes part of the
                // terminal's own scrollback, so it stays readable with the
                // terminal's search and copy. Nothing else writes to the
                // terminal: the region below is redrawn in place.
                let lines = report_turn(&outcome, &host)?;
                let activity = SessionHost::activity_line(&outcome);
                // The prompt row is included even though it is empty, so the
                // region the next keystroke redraws is the same shape as this
                // one and nothing has to be drawn twice.
                let prompt_row = transcript::render_prompt(
                    rune_term::shell::prompt(),
                    "",
                    usize::from(host.width()),
                );
                let caret = rune_term::width::str_width(rune_term::shell::prompt());
                let painted = host.paint(
                    &lines,
                    activity.as_deref(),
                    std::slice::from_ref(&prompt_row),
                    &[],
                    (0, u16::try_from(caret).unwrap_or(u16::MAX)),
                )?;
                if !painted.is_empty() {
                    sink.write_all(&painted)?;
                    sink.flush()?;
                }
                Ok(Step::Continue)
            }
            Input::Empty => Ok(Step::Continue),
        }
    };

    let reason = if keyed {
        // Each keystroke redraws the prompt, so the line and the cursor follow
        // what was typed rather than waiting for the terminal to decide the
        // line is finished.
        let mut reason = ExitReason::EndOfInput;
        while let Some(input) = await_submission(&mut reader, &host, &out, &recall)? {
            let mut sink = LockedSink {
                stream: Arc::clone(&out),
            };
            match handle_input(input, &mut sink, &mut history_file)? {
                Step::Exit => {
                    reason = ExitReason::Requested;
                    break;
                }
                Step::PickModel => {
                    choose_model(&mut reader, &host, &out, &settings, &config.paths)?;
                }
                Step::Undo => {
                    report_undo(&host, &mut sink)?;
                }
                Step::Continue => {}
            }
            // Re-read so a prompt submitted above is recallable with the up
            // arrow. The file is the source of truth, so it is read rather than
            // appended to a copy that could drift from it.
            recall = recorded_prompts(history_file.as_ref(), &config.workspace);
        }
        reason
    } else {
        // A pipe has no keystrokes to drive a picker, so a model change is
        // reported instead of silently doing nothing.
        shell.run(|input| {
            let mut sink = LockedSink {
                stream: Arc::clone(&out),
            };
            match handle_input(input, &mut sink, &mut history_file)? {
                Step::Exit => Ok(Action::Exit),
                Step::Continue => Ok(Action::Continue),
                Step::Undo => {
                    report_undo(&host, &mut sink)?;
                    Ok(Action::Continue)
                }
                Step::PickModel => {
                    let _ = writeln!(
                        sink,
                        "a model picker needs a terminal; use /model <id> and run `rune models` to list ids"
                    );
                    Ok(Action::Continue)
                }
            }
        })?
    };

    // The region is removed before the session ends, so the shell that started
    // it continues on a screen that is not half a frame.
    let cleared = host.clear_region()?;
    if !cleared.is_empty()
        && let Ok(mut sink) = out.lock()
    {
        let _ = sink.write_all(&cleared);
        let _ = sink.flush();
    }

    Ok(reason.exit_code())
}

/// Waits for a line to be submitted, drawing the prompt as it is typed.
///
/// Returns the submitted input, or `None` when the user asked to leave. The
/// cursor is placed after the text typed so far, which is the whole reason the
/// line is drawn here rather than by the terminal.
///
/// `recall` supplies earlier prompts for the up and down arrows. Passing an
/// empty slice leaves those keys doing nothing, which is what a session with no
/// recorded history wants.
fn await_submission(
    reader: &mut rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
    recall: &[String],
) -> Result<Option<Input>> {
    let marker = rune_term::shell::prompt();
    // Which completion row is highlighted. While the dropdown is open the
    // arrows move it rather than walking the prompt history, because the list is
    // what the user is looking at.
    let mut selected = 0_usize;

    loop {
        let rows = completion_rows(reader.line(), selected, host.theme(), host.truecolor());
        draw_prompt(reader, host, out, &rows, marker)?;

        match reader.read_key() {
            KeyAction::Submit => {
                // Enter takes the highlighted completion when the list is open
                // and the command is not yet complete, so a half-typed name is
                // never run. Once the name is complete, Enter runs it.
                if let Some(chosen) = open_completion(reader.line(), &rows, selected)
                    && chosen.name != reader.line().trim_start_matches('/')
                {
                    reader.replace(&format!("/{}", chosen.name));
                    selected = 0;
                    continue;
                }
                let text = reader.line().trim().to_owned();
                reader.clear();
                if text.is_empty() {
                    continue;
                }
                return Ok(Some(Input::parse(&text)));
            }
            // An empty line is the only thing there is to leave behind, so
            // interrupting it means leaving the session.
            KeyAction::Interrupt => return Ok(None),
            KeyAction::Cancel => {
                // Escape clears a partly typed line first, which is what a
                // reader expects of a key that also leaves.
                if reader.line().is_empty() {
                    return Ok(None);
                }
                reader.clear();
                selected = 0;
            }
            response @ (KeyAction::Up | KeyAction::Down) => {
                // The dropdown owns the arrows while it is open.
                if !rows.is_empty() {
                    let count =
                        completion_rows(reader.line(), selected, host.theme(), host.truecolor())
                            .len()
                            .max(1);
                    selected = if response == KeyAction::Up {
                        selected.saturating_sub(1)
                    } else {
                        selected.saturating_add(1).min(count.saturating_sub(1))
                    };
                } else if response == KeyAction::Up {
                    reader.recall_previous(recall);
                } else {
                    reader.recall_next(recall);
                }
            }
            // Tab completes the highlighted command, which is what every other
            // shell does and what a reader reaches for first.
            KeyAction::Complete => {
                if let Some(chosen) = open_completion(reader.line(), &rows, selected) {
                    reader.replace(&format!("/{}", chosen.name));
                    selected = 0;
                }
            }
            KeyAction::Ignored => {
                // Typing narrows the list, so the highlight returns to the top
                // rather than pointing at a row that may no longer exist.
                selected = 0;
            }
        }
    }
}

/// Returns the command the dropdown is offering, when it is open.
///
/// The rows are already rendered, so the count comes from the table rather than
/// from the text of a row: a row carries styling and a description, and parsing
/// either back out would be the wrong way round.
fn open_completion(
    line: &str,
    rows: &[String],
    selected: usize,
) -> Option<&'static rune_term::commands::Builtin> {
    if rows.is_empty() {
        return None;
    }
    let word = line.strip_prefix('/').unwrap_or_default();
    let matches = rune_term::commands::matching(word);
    matches.get(selected).copied()
}

/// Draws the prompt row, with any rows that belong below it.
///
/// A caret is placed at the end of the typed text so the terminal's cursor is
/// where the next character will go.
fn draw_prompt(
    reader: &rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
    below: &[String],
    marker: &str,
) -> Result<()> {
    use rune_term::width::str_width;

    let mut sink = out
        .lock()
        .map_err(|_| RuneError::new(ErrorCode::Internal, "the output lock was poisoned"))?;
    let row = transcript::render_prompt(marker, reader.line(), usize::from(host.width()));
    let caret = str_width(marker).saturating_add(reader.column());
    let painted = host.paint(
        &[],
        None,
        std::slice::from_ref(&row),
        below,
        (0, u16::try_from(caret).unwrap_or(u16::MAX)),
    )?;
    if !painted.is_empty() {
        sink.write_all(&painted)?;
        sink.flush()?;
    }
    Ok(())
}

/// Runs an inline picker and returns the chosen entry.
///
/// The list is drawn under the input line, so the transcript above stays
/// readable while a choice is made. Typing narrows the list, the vertical
/// arrows move the highlight, Enter accepts, and Escape abandons the choice
/// without changing anything.
///
/// Returns `None` when the user cancelled or asked to leave.
fn run_picker(
    reader: &mut rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
    mut picker: rune_term::picker::Picker,
) -> Result<Option<String>> {
    let marker = rune_term::shell::prompt();
    // Whatever was on the line is put back afterwards, so a half-typed prompt
    // is not lost to a look at the model list.
    let draft = reader.line().to_owned();
    reader.clear();

    let chosen = loop {
        let mut below = vec![picker.title().to_owned()];
        below.extend(picker.rows(&host.theme, host.truecolor));
        below.push(rune_term::picker::Picker::hint().to_owned());
        draw_prompt(reader, host, out, &below, marker)?;

        match reader.read_key() {
            // Tab and Enter both accept: the picker is the only thing on
            // screen, so there is no typed argument for Enter to mean.
            KeyAction::Submit | KeyAction::Complete => {
                break picker.selected().map(str::to_owned);
            }
            KeyAction::Interrupt | KeyAction::Cancel => break None,
            // Both branches fall through to the redraw at the top of the loop.
            response @ (KeyAction::Up | KeyAction::Down) => {
                if response == KeyAction::Up {
                    picker.up();
                } else {
                    picker.down();
                }
            }
            KeyAction::Ignored => {
                picker.set_query(reader.line());
            }
        }
    };

    reader.replace(&draft);
    Ok(chosen)
}

/// Summarizes older turns and installs the summary.
///
/// The summary is produced by the model, because nothing else can compress a
/// conversation without discarding what makes it useful. When the request fails
/// the history is left exactly as it was, so a failed compaction costs nothing
/// but the attempt.
fn compact_history(
    host: &SessionHost,
    history: &mut History,
    sink: &mut LockedSink,
) -> Result<Step> {
    let Some(plan) = rune_agent::compaction::plan(history, &host.limits) else {
        return report(
            host,
            sink,
            "nothing to compact: the conversation is short enough to leave as it is",
        );
    };

    if host.endpoint.offline {
        return report(
            host,
            sink,
            "cannot compact while outbound requests are disabled",
        );
    }

    let request = rune_agent::compaction::render_summary_request(history, &plan);
    let mut request_plan = rune_net::provider::RequestPlan::new(host.model());
    rune_agent::compaction::SUMMARY_INSTRUCTIONS.clone_into(&mut request_plan.instructions);
    request_plan.messages = rune_net::transport::one_shot_messages(&request);

    let outcome = match rune_net::transport::stream_completion(
        &rune_net::transport::agent(),
        &host.endpoint,
        host.dialect.as_ref(),
        &request_plan,
        compaction_timeout(host),
        &|| false,
    ) {
        Ok(outcome) => outcome,
        Err(err) => {
            return report(
                host,
                sink,
                &format!("compaction failed, nothing was changed: {err}"),
            );
        }
    };

    let summary = outcome.text();
    // A summary that is empty or trivial would replace the conversation it was
    // meant to compress with nothing, so it is refused and the history stands.
    if let Err(err) = rune_agent::compaction::validate_summary(&summary) {
        return report(
            host,
            sink,
            &format!("compaction produced nothing usable: {err}"),
        );
    }

    let removed = rune_agent::compaction::apply(
        history,
        &plan,
        rune_agent::compaction::wrap_summary(&summary),
    );
    host.record_usage(&outcome.usage);
    report(
        host,
        sink,
        &format!(
            "compacted {removed} earlier turn(s); {} turn(s) remain",
            history.len()
        ),
    )
}

/// Commits one line of command output through the renderer.
fn report(host: &SessionHost, sink: &mut LockedSink, line: &str) -> Result<Step> {
    flush_lines(host, sink, &output_lines(line.as_bytes()))?;
    Ok(Step::Continue)
}

/// How long a compaction request waits.
///
/// Taken from the configured head timeout, so a slow endpoint that has been
/// given more room to answer is not cut off here at a smaller number.
fn compaction_timeout(host: &SessionHost) -> std::time::Duration {
    host.limits
        .get_usize(rune_core::budget::LimitName::ProviderHeadTimeoutMs)
        .try_into()
        .map_or(DEFAULT_COMPACTION_TIMEOUT, std::time::Duration::from_millis)
}

/// How long a compaction request waits when no limit is configured.
const DEFAULT_COMPACTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Puts back what the session changed and reports each file.
///
/// A file that could not be restored is reported and the rest are still
/// attempted, because leaving the user with half their files back and no
/// explanation is worse than a partial undo they can see.
fn report_undo(host: &SessionHost, sink: &mut LockedSink) -> Result<()> {
    if !host.can_undo() {
        flush_lines(
            host,
            sink,
            &["nothing to undo: this session has not changed any file".to_owned()],
        )?;
        return Ok(());
    }
    match host.undo() {
        Ok(lines) => flush_lines(host, sink, &lines)?,
        Err(err) => {
            let mut lines = vec![err.to_string()];
            if let Some(hint) = err.hint() {
                lines.push(format!("hint: {hint}"));
            }
            flush_lines(host, sink, &lines)?;
        }
    }
    Ok(())
}

/// Returns the model's most recent reply, as plain text.
///
/// Read from the conversation rather than from the screen: what is on screen has
/// been wrapped, indented, and styled, so copying it would paste back the
/// renderer's layout rather than what the model said.
#[must_use]
fn last_reply(history: &History) -> Option<String> {
    history
        .turns()
        .iter()
        .rev()
        .find(|turn| turn.role == rune_net::message::Role::Assistant)
        .map(|turn| {
            // Only text parts are copied. A tool call carries arguments the user
            // did not write and cannot use, so including it would paste JSON
            // into whatever they are working on.
            turn.parts
                .iter()
                .filter_map(|part| match part {
                    rune_net::message::ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<&str>>()
                .join("")
        })
        .filter(|reply| !reply.trim().is_empty())
}

/// Copies text to the terminal's clipboard.
///
/// Uses OSC 52, which the terminal emulator handles itself. Shelling out to a
/// platform clipboard tool would work on a desktop and fail over SSH, where the
/// terminal is the only thing that can reach the user's clipboard. A terminal
/// that does not implement OSC 52 ignores it rather than showing it, so the
/// escape never becomes visible text.
fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let encoded = base64_encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    write!(stdout, "\u{1b}]52;c;{encoded}\u{7}")?;
    stdout.flush()
}

/// Encodes bytes as standard base64 with padding.
///
/// Written here rather than pulled from a crate: OSC 52 is the only base64 this
/// program needs, and a dependency to replace twelve lines is not worth the
/// build time or the supply chain.
#[must_use]
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let b0 = chunk.first().copied().unwrap_or(0);
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let triple = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        let indexes = [
            (triple >> 18) & 0x3f,
            (triple >> 12) & 0x3f,
            (triple >> 6) & 0x3f,
            triple & 0x3f,
        ];
        for (position, index) in indexes.iter().enumerate() {
            // A short final chunk emits padding instead of the bytes it does
            // not have, which is what makes the encoding decodable.
            if position > chunk.len() {
                out.push('=');
            } else {
                out.push(char::from(ALPHABET[*index as usize]));
            }
        }
    }
    out
}

/// Closes a styled row.
const RESET: &str = "\u{1b}[0m";

/// Returns the rows showing what can be typed next.
///
/// Drawn while a slash command is being typed, from the same table the help
/// text and the dispatcher use, so a command that is offered is one that works.
/// The row under the cursor is marked and coloured; every row carries the line
/// that describes the command, which is what makes the list usable without
/// trying each name.
///
/// Returns an empty list when the line is not a command being typed, so a
/// caller can pass every keystroke without checking first.
#[must_use]
pub fn completion_rows(line: &str, selected: usize, theme: &Theme, truecolor: bool) -> Vec<String> {
    let Some(word) = slash_word(line) else {
        return Vec::new();
    };
    let matches = rune_term::commands::matching(word);
    if matches.is_empty() {
        return Vec::new();
    }

    let accent = theme.sgr(Slot::Accent, truecolor);
    let dim = theme.sgr(Slot::Dim, truecolor);
    // Wide enough for the longest name in the list, so the descriptions line up.
    let width = matches
        .iter()
        .map(|entry| {
            entry
                .name
                .len()
                .saturating_add(entry.arguments.len())
                .saturating_add(if entry.arguments.is_empty() { 1 } else { 2 })
        })
        .max()
        .unwrap_or(0);

    matches
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let left = if entry.arguments.is_empty() {
                format!("/{}", entry.name)
            } else {
                format!("/{} {}", entry.name, entry.arguments)
            };
            let body = format!("{left:<width$}  {}", entry.summary);
            if index == selected {
                styled(&accent, &format!("> {body}"))
            } else {
                styled(&dim, &format!("  {body}"))
            }
        })
        .collect()
}

/// Returns the word after a leading slash, when the line is one.
///
/// A line with a space in it is a command already being given its arguments, so
/// there is nothing left to complete.
fn slash_word(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('/')?;
    if rest.chars().any(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// Wraps `text` in `open`, closing it again.
///
/// A theme without color yields an empty sequence, and a row surrounded by two
/// empty strings would carry escapes the terminal has nothing to do with.
fn styled(open: &str, text: &str) -> String {
    if open.is_empty() {
        return text.to_owned();
    }
    format!("{open}{text}{RESET}")
}

/// Prints finished lines and leaves the prompt under them.
///
/// Every writer must go through the renderer: text written straight to the
/// stream lands at the caret, which is inside the region the session repaints,
/// and the two then disagree about what is on screen. Lines committed here
/// become part of the terminal's own scrollback.
fn flush_lines(host: &SessionHost, sink: &mut LockedSink, lines: &[String]) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let (prompt_row, caret) = host.idle_prompt();
    let painted = host.paint(lines, None, &prompt_row, &[], caret)?;
    if !painted.is_empty() {
        sink.write_all(&painted)?;
        sink.flush()?;
    }
    Ok(())
}

/// Splits captured command output into the lines the renderer commits.
///
/// A trailing newline produces an empty final line, which would commit a blank
/// row and move the prompt down for no reason, so it is dropped.
fn output_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.strip_suffix('\n').unwrap_or(&text);
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.lines().map(str::to_owned).collect()
}

/// Runs the model picker and applies the choice.
///
/// The list comes from the endpoint when it can be reached and from
/// configuration when it cannot, so a picker still offers something on an
/// offline machine. A chosen model is written back to the configuration, so the
/// next session starts on it, and is applied to this session immediately.
fn choose_model(
    reader: &mut rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
    settings: &Settings,
    paths: &Paths,
) -> Result<()> {
    let catalog = match crate::provider_setup::fetch_catalog(settings, paths, MODEL_PICKER_TIMEOUT)
    {
        Ok(catalog) => catalog,
        // A picker that refuses to open because the endpoint is unreachable
        // leaves the user with nothing to do, so the configured model is
        // offered instead and the reason is shown. It is still enriched from the
        // cached catalog, so the window it reports is the model's own.
        Err(err) => {
            let mut sink = LockedSink {
                stream: Arc::clone(out),
            };
            let _ = writeln!(sink, "could not list models: {}", err.message());
            if let Some(hint) = err.hint() {
                let _ = writeln!(sink, "hint: {hint}");
            }
            let mut catalog = crate::provider_setup::catalog_for(settings);
            crate::provider_setup::enrich_with_capacity(settings, paths, &mut catalog);
            catalog
        }
    };

    let items: Vec<String> = catalog
        .models
        .iter()
        .map(|model| model.id.clone())
        .collect();
    let current = host.model_name();
    let picker = rune_term::picker::Picker::new(
        format!("models from {}", catalog.provider),
        items,
        rune_term::picker::DEFAULT_WINDOW,
    )
    .with_current(Some(current.as_str()));

    let Some(chosen) = run_picker(reader, host, out, picker)? else {
        // Cancelling reports nothing: the status line still names the model in
        // effect, which is the answer to the question that was asked.
        return Ok(());
    };

    // The catalog is where a model's capacity is declared, so the window that
    // travels with the choice is the one the endpoint reported for it rather
    // than a number invented for it.
    let window = catalog
        .models
        .iter()
        .find(|model| model.id == chosen)
        .and_then(|model| model.context_window);
    host.set_model(&chosen, window);
    // Written after the change takes effect, so a failed write cannot leave the
    // session on a model the file does not name.
    let selection = crate::provider_setup::Selection {
        provider: rune_core::config::provider_key(&settings.provider),
        model: Some(chosen.clone()),
        base_url: None,
    };
    let mut sink = LockedSink {
        stream: Arc::clone(out),
    };
    match crate::provider_setup::save_selection(paths, &selection) {
        Ok(()) => {
            let _ = writeln!(sink, "model set to {chosen}");
        }
        Err(err) => {
            let _ = writeln!(
                sink,
                "model set to {chosen} for this session; it could not be saved: {}",
                err.message()
            );
        }
    }
    Ok(())
}

/// Returns the prompts recorded for a workspace, oldest first.
///
/// A missing history is not an error: recall is a convenience, and a session
/// without it is still usable.
fn recorded_prompts(
    history: Option<&crate::prompt_history::History>,
    workspace: &Utf8Path,
) -> Vec<String> {
    history
        .map(|history| {
            history
                .for_workspace(workspace)
                .iter()
                .map(|entry| entry.text.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Renders the session's current state.
///
/// Reads nothing from disk, because the point of the command is to answer
/// questions about the running session rather than about the machine.
#[must_use]
fn render_status(info: &SessionInfo<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "model          {}", info.model);
    let _ = writeln!(out, "provider       {}", info.provider);
    let _ = writeln!(out, "endpoint       {}", info.endpoint);
    let _ = writeln!(out, "permissions    {}", info.mode.label());
    let _ = writeln!(out, "effort         {}", info.effort);
    let _ = writeln!(out, "session        {}", info.session_id);
    let _ = writeln!(out, "workspace      {}", info.workspace);
    let _ = write!(
        out,
        "context        {}",
        footer::format_tokens(info.context_used)
    );
    if info.context_limit == 0 {
        // A window that is not known is left unnamed rather than shown as a
        // share of a number this program does not have.
        let _ = writeln!(out, " (window unknown)");
    } else {
        let percent = info
            .context_used
            .saturating_mul(100)
            .checked_div(info.context_limit)
            .unwrap_or(0)
            .min(100);
        let _ = writeln!(
            out,
            " of {} ({percent}%)",
            footer::format_tokens(info.context_limit)
        );
    }
    out.trim_end().to_owned()
}

/// Renders what this session has spent.
///
/// Counted from the turns this session has run rather than read back from the
/// ledger, because the ledger records which model served a request but not
/// which session asked for it, and a command that reported the machine's whole
/// spend would be answering a different question.
///
/// Only tokens are reported. No provider in this build reports a monetary cost,
/// so a currency figure here would be one this program invented.
#[must_use]
fn render_cost(info: &SessionInfo<'_>) -> String {
    let totals = &info.totals;
    if totals.requests == 0 {
        return "no requests have been made in this session yet".to_owned();
    }
    let mut out = String::new();
    let _ = writeln!(out, "Requests: {}", totals.requests);
    let _ = writeln!(
        out,
        "Tokens:   {} in, {} out",
        footer::format_tokens(totals.input_tokens),
        footer::format_tokens(totals.output_tokens)
    );
    if totals.cache_read_tokens > 0 || totals.cache_write_tokens > 0 {
        let _ = writeln!(
            out,
            "Cache:    {} read, {} written",
            footer::format_tokens(totals.cache_read_tokens),
            footer::format_tokens(totals.cache_write_tokens)
        );
    }
    if totals.reasoning_tokens > 0 {
        let _ = writeln!(
            out,
            "Reasoning: {}",
            footer::format_tokens(totals.reasoning_tokens)
        );
    }
    out.trim_end().to_owned()
}

/// Renders the settings that govern the running session.
#[must_use]
fn render_settings(info: &SessionInfo<'_>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "model          {}", info.model);
    let _ = writeln!(out, "provider       {}", info.provider);
    let _ = writeln!(out, "permissions    {}", info.mode.label());
    let _ = writeln!(out, "effort         {}", info.effort);
    let _ = write!(
        out,
        "context window {}",
        if info.context_limit == 0 {
            "unknown".to_owned()
        } else {
            footer::format_tokens(info.context_limit)
        }
    );
    let _ = writeln!(out);
    out.trim_end().to_owned()
}

/// Adopts the selected model's real context window.
///
/// The endpoint is asked what it serves and the published catalog fills in the
/// capacity it does not state. A failure is not reported and does not stop the
/// session: the window already in force, from the configuration or the compiled
/// default, is what applies, and reminding a user of a lookup they cannot act
/// on would only be noise at startup.
fn resolve_context_window(host: &SessionHost, settings: &Settings, paths: &Paths) {
    let Ok(catalog) = crate::provider_setup::fetch_catalog(settings, paths, CONTEXT_LOOKUP_TIMEOUT)
    else {
        return;
    };
    let current = host.model_name();
    let reported = catalog
        .models
        .iter()
        .find(|model| model.id == current)
        .and_then(|model| model.context_window);
    host.adopt_context_window(reported, settings.context_window.is_some());
}

/// How long the session waits for the endpoint's model list at startup.
///
/// Short, because a user is waiting for a prompt: the figure is an improvement
/// on the compiled default rather than something the session cannot run without.
const CONTEXT_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How long the model picker waits for the endpoint's list.
///
/// Shorter than the command line's wait, because a user is watching a list
/// that has not appeared yet and configuration already offers a usable entry.
const MODEL_PICKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Resolves the theme for a session.
///
/// A terminal that accepts no color gets the colorless theme whatever the
/// configuration names, because the setting expresses a preference and the
/// terminal states a capability.
fn resolve_theme(config: &SessionConfig) -> Theme {
    resolve_theme_for(config, std::env::var_os("NO_COLOR").is_some())
}

/// Resolves the theme for a stated terminal capability.
///
/// The capability is a parameter because reading it from the environment makes a
/// test that asserts the colorless path pass or fail depending on the shell it
/// happens to run in.
fn resolve_theme_for(config: &SessionConfig, accepts_no_color: bool) -> Theme {
    if accepts_no_color {
        return Theme::no_color();
    }
    Theme::resolve(
        config.settings.theme.as_deref(),
        true,
        &config.paths.themes_dir(),
    )
}

/// Returns the terminal width to compose for.
///
/// Read from the terminal rather than assumed. A width that disagrees with the
/// real one makes every row either wrap onto a row the renderer did not count,
/// or leave a gap, and both put later content in the wrong place.
fn terminal_width() -> u16 {
    rune_term::shell::terminal_size().map_or(80, |size| size.0)
}

/// Returns the terminal height to compose for.
///
/// A terminal that does not report a size gets a conventional height rather than
/// zero, which would compose a frame with no room for anything.
fn terminal_height() -> u16 {
    rune_term::shell::terminal_size().map_or(24, |size| size.1)
}

/// Returns the context window for the configured model.
///
/// Configuration carries no per-model window, so the compiled default applies
/// until a provider reports one. A zero would make every request look oversized.
fn context_limit(settings: &Settings, _limits: &BudgetSet) -> u64 {
    // The configured window wins, because it describes the model the user
    // selected. The compiled default is a guess that suits a small model and
    // understates a large one, which is what made a million-token model report
    // a hundred and twenty-eight thousand.
    settings
        .context_window
        .unwrap_or(rune_net::catalog::DEFAULT_CONTEXT_WINDOW)
}

/// Returns whether the terminal can render direct color.
///
/// Reads the environment for the two variables that advertise it. Anything else
/// gets the indexed fallback, which every terminal renders.
fn truecolor_supported() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    colorterm.eq_ignore_ascii_case("truecolor") || colorterm.eq_ignore_ascii_case("24bit")
}

/// Adds a finished turn to the usage ledger.
///
/// A ledger write must never end a session, so a failure is reported and the
/// session continues: losing an accounting record is better than losing the
/// conversation, which is already on disk.
fn record_usage(paths: &Paths, model: &str, outcome: &turn::TurnOutcome) {
    let mut record = UsageRecord::new(now_ms(), model, HelperKind::Main);
    record.input_tokens = outcome.usage.input_tokens;
    record.output_tokens = outcome.usage.output_tokens;
    record.cache_read_tokens = outcome.usage.cache_read_tokens;
    record.cache_write_tokens = outcome.usage.cache_write_tokens;
    record.reasoning_tokens = outcome.usage.reasoning_tokens;
    let ledger = Ledger::from_paths(paths);
    if let Err(err) = ledger.append(&record) {
        // Reported on the error stream so a machine consumer reading stdout
        // still sees a clean conversation.
        let _ = writeln!(std::io::stderr(), "usage was not recorded: {err}");
    }
}

/// What handling a slash command decided.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Handled {
    /// Carry on with the session.
    Continue,
    /// End the session.
    Exit,
    /// Place this text in the composer for review.
    Expand(String),
    /// Forget every recorded prompt.
    ClearHistory,
    /// Send later turns to this model.
    SetModel(String),
    /// Ask the user to choose a model from a list.
    PickModel,
    /// Put back the files the session changed.
    Undo,
    /// Summarize older turns to free the context window.
    Compact,
    /// Put the last reply on the terminal's clipboard.
    Copy,
    /// Start a fresh conversation in this terminal.
    NewSession,
    /// Give the session a new title.
    Rename(String),
}

/// Token and request totals for one session.
///
/// Summed here rather than read back from the ledger, which does not record
/// which session a request belonged to.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Totals {
    /// Requests the session has made.
    requests: u64,
    /// Input tokens reported across those requests.
    input_tokens: u64,
    /// Output tokens reported across those requests.
    output_tokens: u64,
    /// Prompt-cache reads reported across those requests.
    cache_read_tokens: u64,
    /// Prompt-cache writes reported across those requests.
    cache_write_tokens: u64,
    /// Reasoning tokens reported across those requests.
    reasoning_tokens: u64,
}

impl Totals {
    /// Adds one turn's usage.
    ///
    /// A provider that reports no count leaves that field as it was rather than
    /// adding zero, so an absent figure does not look like a measured zero.
    fn record(&mut self, usage: &rune_net::stream::Usage) {
        self.requests = self.requests.saturating_add(1);
        if let Some(value) = usage.input_tokens {
            self.input_tokens = self.input_tokens.saturating_add(value);
        }
        if let Some(value) = usage.output_tokens {
            self.output_tokens = self.output_tokens.saturating_add(value);
        }
        if let Some(value) = usage.cache_read_tokens {
            self.cache_read_tokens = self.cache_read_tokens.saturating_add(value);
        }
        if let Some(value) = usage.cache_write_tokens {
            self.cache_write_tokens = self.cache_write_tokens.saturating_add(value);
        }
        if let Some(value) = usage.reasoning_tokens {
            self.reasoning_tokens = self.reasoning_tokens.saturating_add(value);
        }
    }
}

/// Returns the file a tool call will change, when it changes one.
///
/// Only the two tools that rewrite a file wholesale are treated as mutations.
/// A tool that changes something else, such as the shell, is not tracked: what
/// a shell command changed is not knowable from its arguments, and pretending
/// otherwise would offer an undo that does not undo.
fn mutating_path<'a>(name: &str, arguments: &'a serde_json::Value) -> Option<&'a Utf8Path> {
    if name != "write_file" && name != "edit_file" {
        return None;
    }
    arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(Utf8Path::new)
}

/// What the read-only commands report about the running session.
///
/// A struct rather than a list of arguments so adding a field does not change
/// every call site, and so a test can build one without a session.
struct SessionInfo<'a> {
    /// Model the session is sending to.
    ///
    /// Owned because it lives behind a lock that can change while the session
    /// runs, so it cannot be handed out as a reference.
    model: String,
    /// Provider name as written in configuration.
    provider: &'a str,
    /// Endpoint requests are sent to.
    endpoint: &'a str,
    /// Permission mode in force.
    mode: PermissionMode,
    /// Reasoning effort in force.
    effort: Effort,
    /// Session identifier.
    ///
    /// Owned because `/new` replaces it while the session runs, so it cannot be
    /// handed out as a reference.
    session_id: String,
    /// Workspace the session runs in.
    workspace: &'a str,
    /// Tokens spent from the context window.
    context_used: u64,
    /// Size of the context window, zero when none is known.
    context_limit: u64,
    /// What this session has spent so far.
    totals: Totals,
}

/// What the input loop does after handling one line.
enum Step {
    /// Carry on reading input.
    Continue,
    /// Leave the session.
    Exit,
    /// Ask the user to choose a model before continuing.
    PickModel,
    /// Put back the files the session changed.
    Undo,
}

/// Handles a slash command.
///
/// An unknown command is reported and the loop continues, because a typo should
/// not end a session.
fn handle_command<W: std::io::Write>(
    name: &str,
    arguments: &str,
    commands: &rune_context::commands::Discovery,
    history: Option<&crate::prompt_history::History>,
    self_workspace: &Utf8Path,
    info: &SessionInfo<'_>,
    output: &mut W,
) -> Result<Handled> {
    match name {
        "quit" | "exit" => Ok(Handled::Exit),
        // A model named inline is taken as given rather than checked against a
        // catalog, because an endpoint may serve a model it does not advertise
        // and a typo is visible in the status line right after.
        // `/models` is accepted as a spelling of `/model`, because reaching for
        // the plural is what most people do first and refusing it teaches
        // nothing.
        "model" | "models" => {
            let requested = arguments.trim();
            if requested.is_empty() {
                return Ok(Handled::PickModel);
            }
            Ok(Handled::SetModel(requested.to_owned()))
        }
        "compact" => Ok(Handled::Compact),
        // Read-only: the session log has no branch concept, so there is nothing
        // to fork or switch to. Reporting the shape is what can be done
        // truthfully.
        "tree" => {
            let state = session_log::inspect(&Paths::from_process(), &info.session_id.parse()?)?;
            let tree = session_log::tree_of(&state);
            let _ = writeln!(output, "{}", session_log::render_tree(&tree, &state));
            Ok(Handled::Continue)
        }
        "copy" => Ok(Handled::Copy),
        "new" => Ok(Handled::NewSession),
        "undo" => Ok(Handled::Undo),
        "rename" => {
            let title = arguments.trim();
            if title.is_empty() {
                let _ = writeln!(output, "usage: /rename <title>");
                return Ok(Handled::Continue);
            }
            Ok(Handled::Rename(title.to_owned()))
        }
        "status" => {
            let _ = writeln!(output, "{}", render_status(info));
            Ok(Handled::Continue)
        }
        "cost" => {
            let _ = writeln!(output, "{}", render_cost(info));
            Ok(Handled::Continue)
        }
        "settings" => {
            let _ = writeln!(output, "{}", render_settings(info));
            Ok(Handled::Continue)
        }
        "history" => {
            let Some(history) = history else {
                let _ = writeln!(output, "no prompt history is available");
                return Ok(Handled::Continue);
            };
            // `here` scopes recall to the workspace the session is running in,
            // which is what a composer offers; anything else is read as a
            // session identifier.
            let entries = match arguments.trim() {
                "" => history.entries(),
                "here" => history.for_workspace(self_workspace),
                "clear" => {
                    // Clearing needs the mutable handle, so it is handled by the
                    // caller, which owns it.
                    return Ok(Handled::ClearHistory);
                }
                session => history.for_session(session),
            };
            if entries.is_empty() {
                // The path is named so a user can find or remove the file, and
                // so an empty result is distinguishable from a missing file.
                let _ = writeln!(output, "no prompts recorded in {}", history.path());
            }
            for (index, entry) in entries.iter().enumerate() {
                let _ = writeln!(output, "{:>4}  {}", index.saturating_add(1), entry.text);
            }
            if !entries.is_empty() {
                let _ = writeln!(
                    output,
                    "\n{} prompt(s) recorded in {}",
                    history.len(),
                    history.path()
                );
            }
            Ok(Handled::Continue)
        }
        "help" => {
            // Read from the same table the dropdown offers, so a command that is
            // listed is one that works and the two cannot drift apart. The table
            // is longer than the summary the dropdown shows, so it is printed as
            // a whole rather than as one line.
            let _ = writeln!(output, "{}", rune_term::commands::render_help());
            // `clear` and the session argument are forms of `/history`, which
            // the table lists once; they are named here so they are findable.
            let _ = writeln!(output, "/history clear  forget every recorded prompt");
            let listing = rune_context::commands::render_listing(&commands.commands);
            let _ = writeln!(output, "{listing}");
            // A command file that was refused is invisible otherwise, and a
            // silent skip looks like a file that was never read.
            for warning in &commands.warnings {
                let _ = writeln!(output, "skipped {}: {}", warning.path, warning.reason);
            }
            Ok(Handled::Continue)
        }
        other => {
            let Some(command) = commands.commands.iter().find(|c| c.name == other) else {
                let _ = writeln!(output, "unknown command `/{other}`; try /help");
                return Ok(Handled::Continue);
            };
            let parts: Vec<String> = arguments.split_whitespace().map(str::to_owned).collect();
            match command.expand(&parts) {
                Ok(text) => Ok(Handled::Expand(text)),
                Err(err) => {
                    let _ = writeln!(output, "{err}");
                    if let Some(hint) = err.hint() {
                        let _ = writeln!(output, "hint: {hint}");
                    }
                    Ok(Handled::Continue)
                }
            }
        }
    }
}

/// Returns the one line that stands for a finished tool call.
///
/// A tool result already opens with a headline naming what it found, because
/// that is what the model reads first. That line is the whole of what a reader
/// needs while skimming, and the body stays available to the model and in the
/// session log.
fn tool_summary(name: &str, output: &ToolOutput) -> String {
    let headline = output
        .text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    // A failure is stated in full, because the reason is the result and a
    // reader needs it to act.
    if output.is_error {
        return format!("{name}: {headline}");
    }
    if headline.is_empty() {
        return format!("{name}: done");
    }
    format!("{name}: {headline}")
}

/// Renders what a turn produced, as the lines to print.
///
/// Returns the lines rather than writing them, because the renderer is the only
/// component that writes to the terminal. A line printed beside it would move
/// the rows the live region is drawn at, and in raw mode a bare newline moves
/// down without returning to the first column.
fn report_turn(outcome: &turn::TurnOutcome, host: &SessionHost) -> Result<Vec<String>> {
    let events = host
        .events
        .lock()
        .map(|events| events.clone())
        .unwrap_or_default();

    // Model and tool output both reach a terminal, so every entry is rendered
    // through the transcript, which strips the control sequences a terminal
    // would act on.
    let mut entries = Vec::new();
    for event in &events {
        match event {
            Event::ToolStarted { call, activity } => {
                entries.push(Entry::tool(format!(
                    "{} {}",
                    activity.running_label(),
                    call.name
                )));
            }
            Event::ToolDenied { call, reason } => {
                entries.push(Entry::notice(format!("refused {}: {reason}", call.name)));
            }
            Event::SteeringApplied { count, .. } => {
                entries.push(Entry::notice(format!("applied {count} queued message(s)")));
            }
            _ => {}
        }
    }

    for call in &outcome.calls {
        if call.executed {
            // Only the result's headline reaches the conversation. The body is
            // what the model asked for and what it already has; printing it
            // again buries the answer under the material it was drawn from.
            entries.push(Entry::tool(tool_summary(&call.call.name, &call.output)));
        }
    }

    // Reasoning is shown above the answer and in its own lane, so a reader can
    // tell what the model thought from what it concluded.
    if !outcome.reasoning.trim().is_empty() {
        entries.push(Entry::reasoning(outcome.reasoning.clone()));
    }

    if !outcome.text.is_empty() {
        entries.push(Entry::assistant(outcome.text.clone()));
    }

    match outcome.stop_reason {
        StopReason::StepLimit => entries.push(Entry::notice("reached the model step limit")),
        StopReason::Cancelled => entries.push(Entry::notice("cancelled")),
        _ => {}
    }

    // Measured against the terminal rather than a fixed width, so a line is
    // wrapped where the reader's own window wraps it instead of mid-word at a
    // column that has nothing to do with this terminal.
    let display = Display {
        width: usize::from(host.width()),
        ..Display::default()
    };
    let lanes = transcript::Lanes {
        reasoning: host.reasoning_style(),
        reset: rune_term::engine::Style::RESET.to_owned(),
    };
    let rendered = transcript::render_lanes(&entries, display, &lanes);
    Ok(rendered.lines().map(str::to_owned).collect())
}

/// Builds the prompt for a session.
fn build_prompt(workspace: &Utf8Path, config_root: &Utf8Path, limits: &BudgetSet) -> Prompt {
    let instructions = prompt::instructions_for(workspace, config_root, limits);
    Prompt {
        instructions,
        included: Vec::new(),
        omissions: Vec::new(),
    }
}

/// Builds the runtime configuration for an interactive session.
///
/// Split from the loop so the command surface can report a configuration problem
/// before a terminal is taken over.
pub fn prepare(
    settings: &Settings,
    paths: &Paths,
    workspace: &Utf8Path,
    resume: Option<SessionId>,
) -> Result<SessionConfig> {
    // A model is not required here. A session that has a terminal can ask for
    // one before its first turn, which is what makes `rune` alone work on a
    // connection that was made without naming a model. The refusal still
    // happens for a caller that cannot be asked, because a turn sent with an
    // empty identifier fails at the endpoint with nothing to act on.
    settings.require_provider()?;

    let provider_name = settings.provider.to_string();
    let base_url = settings.base_url.clone().ok_or_else(|| {
        RuneError::new(
            ErrorCode::InvalidConfiguration,
            format!("no endpoint is configured for provider `{provider_name}`"),
        )
        .with_hint("set `base_url` in the user config, or run `rune connect`")
    })?;
    rune_net::transport::validate_url(&base_url)?;

    let credential =
        rune_net::auth::resolve(paths, &provider_name, settings.api_key_env.as_deref())?
            .ok_or_else(|| {
                rune_net::auth::missing_credential_error(
                    &provider_name,
                    settings.api_key_env.as_deref(),
                )
            })?;

    let dialect: Box<dyn Provider> = match settings.provider {
        rune_core::config::Provider::Anthropic => Box::new(rune_net::anthropic::Anthropic),
        rune_core::config::Provider::Responses => Box::new(rune_net::responses::Responses),
        _ => Box::new(rune_net::chat_completions::ChatCompletions),
    };

    let mut registry = inventory::builtin_with_web(
        &rune_tools::workspace::FileLimits::from_budget(&settings.limits),
        &settings.limits,
        &paths.managed_skills_dir(),
        crate::web_client::backends(settings),
    )?;
    // The delegation tool lives with the authority model it enforces, and the
    // tool registry cannot depend on that crate, so it is added here where both
    // are visible.
    registry.insert(Box::new(rune_agent::Subagent::unsupported(
        rune_agent::Authority {
            mode: settings.permission_mode,
            rules: RuleSet::new(),
            workspace: workspace.to_owned(),
            roots: std::iter::once(workspace.to_owned())
                .chain(settings.additional_directories.iter().cloned())
                .collect(),
            tools: registry.names().into_iter().map(str::to_owned).collect(),
            mcp_view: None,
            generation: 0,
        },
    )))?;

    Ok(SessionConfig {
        settings: settings.clone(),
        paths: paths.clone(),
        resume,
        workspace: workspace.to_owned(),
        endpoint: crate::provider_setup::endpoint(
            &settings.provider,
            &base_url,
            credential.expose(),
            rune_net::transport::AuthStyle::Bearer,
            settings.offline,
        ),
        dialect,
        registry,
        // The rules that ship with the harness, so a fresh install has a usable
        // starting point: reads and in-workspace edits proceed, outbound traffic
        // is refused, and an unknown command resolves to the mode's default
        // rather than to nothing.
        rules: crate::permissions::validated(settings)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_term::shell::{ScriptedSource, Shell};

    /// A writer that appends to a shared buffer, for asserting on what a draw
    /// actually wrote.
    struct SharedSink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_streamed_delta_is_drawn_before_the_turn_ends() {
        // The point of streaming: text appears while it is being produced. A
        // delta that only reached the screen once the turn finished would look
        // identical on the final screen, so the assertion is that a write
        // happened while the turn was still running.
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        if let Ok(mut slot) = host.live_out.lock() {
            *slot = Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&sink)))));
        }

        host.emit(Event::TextDelta {
            delta: "streamed".to_owned(),
        });

        let written = String::from_utf8_lossy(&sink.lock().expect("lock")).into_owned();
        assert!(
            written.contains("streamed"),
            "the delta was not drawn when it arrived: {written:?}"
        );
    }

    #[test]
    fn streamed_text_is_drawn_above_the_status_and_input() {
        // The answer sits directly above the status block, under the question it
        // answers, and the input stays at the bottom. Putting it below the input
        // made the input rise as the answer grew, because the region is anchored
        // at the bottom of the screen.
        let host = test_host();
        // Accumulated without drawing, so this test sees one frame rather than
        // the deltas that produced it overlap on the same screen.
        if let Ok(mut streaming) = host.streaming.lock() {
            streaming.answer.push_str("answer");
        }
        let prompt_row = transcript::render_prompt(rune_term::shell::prompt(), "", 80);
        let rows = host.streaming_rows();
        let bytes = host
            .paint(&[], None, std::slice::from_ref(&prompt_row), &rows, (0, 2))
            .expect("painted");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let input = text.find("> ").expect("the input row");
        let answer = text.find("answer").expect("the streamed answer");
        assert!(
            answer < input,
            "the answer was not drawn above the input: {text:?}"
        );
    }

    #[test]
    fn the_status_line_is_drawn_above_the_input() {
        // Where a reader looks for it, and where it does not move as an answer
        // arrives.
        let host = test_host();
        let bytes = host
            .paint(&[], None, &["> ".to_owned()], &[], (0, 2))
            .expect("painted");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let status = text.find("auto").expect("the status line");
        let input = text.find("> ").expect("the input row");
        assert!(
            status < input,
            "the status line was drawn below the input: {text:?}"
        );
    }

    #[test]
    fn a_finished_turn_is_printed_once_above_the_live_region() {
        // Finished lines join the terminal's own scrollback, so they are
        // written in the flow rather than positioned. Everything after them is
        // the region that gets redrawn in place.
        let host = test_host();
        let lines = vec!["a line".to_owned()];
        let bytes = host
            .paint(&lines, None, &[], &[], (0, 0))
            .expect("painted the first screen");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            text.contains("a line\r\n"),
            "the finished line was not printed in the flow: {text:?}"
        );
    }

    #[test]
    fn the_live_region_carries_the_status_line() {
        let host = test_host();
        let bytes = host.paint(&[], None, &[], &[], (0, 0)).expect("painted");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            text.contains("test"),
            "the status line was not drawn: {text:?}"
        );
        assert!(text.contains("auto"), "{text:?}");
    }

    #[test]
    fn the_prompt_and_its_cursor_are_drawn_where_the_caret_is() {
        // This is the whole point of the composer: the caret sits inside what
        // was typed, rather than wherever the terminal would have left it.
        let host = test_host();
        let row = vec!["> hel".to_owned()];
        let bytes = host.paint(&[], None, &row, &[], (0, 5)).expect("painted");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(text.contains("> hel"), "{text:?}");
        // A one-based column, so a caret after five columns is column six.
        assert!(
            text.contains("\u{1b}[6G"),
            "the caret was not placed at the end of the line: {text:?}"
        );
    }

    #[test]
    fn every_frame_hides_the_cursor_while_it_writes() {
        // A cursor visible mid-frame is seen jumping between rows.
        let host = test_host();
        let bytes = host.paint(&[], None, &[], &[], (0, 0)).expect("painted");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(text.contains(rune_term::inline::HIDE_CURSOR), "{text:?}");
        assert!(text.contains(rune_term::inline::SHOW_CURSOR), "{text:?}");
    }

    #[test]
    fn leaving_removes_the_live_region() {
        let host = test_host();
        host.paint(&[], None, &[], &[], (0, 0)).expect("painted");
        let cleared = host.clear_region().expect("cleared");
        assert!(!cleared.is_empty(), "the region was left on the screen");
    }

    #[test]
    fn the_width_comes_from_the_terminal_rather_than_a_constant() {
        // A width that disagrees with the real one makes every row wrap or
        // leave a gap, and both put later content in the wrong place.
        let host = test_host();
        let before = host.width();
        host.width.store(132, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(host.width(), 132);
        assert_ne!(before, 132, "the test did not change the width");
        // The status line is measured against the width it now reports.
        let line = host.status_line(usize::from(host.width()));
        assert!(!line.is_empty());
    }

    #[test]
    fn a_prompt_override_replaces_the_built_in_text() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::write(
            root.join(rune_core::paths::names::SYSTEM_PROMPT_FILE),
            "You are a terse reviewer.\n",
        )
        .expect("write");

        let prompt = build_prompt(root, root, &BudgetSet::new());
        assert_eq!(prompt.instructions, "You are a terse reviewer.");
        assert!(
            !prompt.instructions.contains("coding agent"),
            "the built-in text was kept alongside the override"
        );
    }

    #[test]
    fn no_override_leaves_the_built_in_text_alone() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let prompt = build_prompt(root, root, &BudgetSet::new());
        assert!(prompt.instructions.starts_with(prompt::SYSTEM_PROMPT));
    }

    #[test]
    fn an_empty_override_file_is_ignored() {
        // A truncated or blanked file would otherwise leave the model with no
        // instructions at all.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::write(
            root.join(rune_core::paths::names::SYSTEM_PROMPT_FILE),
            "   \n\n",
        )
        .expect("write");

        let prompt = build_prompt(root, root, &BudgetSet::new());
        assert!(prompt.instructions.starts_with(prompt::SYSTEM_PROMPT));
    }

    /// A reviewer that answers with whatever it was given.
    struct Scripted(ReviewOutcome);

    impl Reviewer for Scripted {
        fn review(&self, _request: &ReviewRequest) -> ReviewOutcome {
            self.0.clone()
        }
    }

    /// Builds a host in automatic mode with the given reviewer.
    fn host_with_reviewer(outcome: ReviewOutcome) -> SessionHost {
        let mut host = test_host();
        host.mode = PermissionMode::Auto;
        host.reviewer = Some(Box::new(Scripted(outcome)));
        host
    }

    #[test]
    fn an_unresolved_action_is_cleared_by_review_in_automatic_mode() {
        // Automatic mode reviews instead of prompting, so an action the rules
        // do not decide is allowed when the reviewer clears it.
        let host = host_with_reviewer(ReviewOutcome::Clear {
            reviewed_action: "rm -rf /tmp/x".to_owned(),
        });
        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Allow, "{reason}");
        assert!(reason.contains("cleared by review"), "{reason}");
    }

    #[test]
    fn a_cautioning_review_holds_the_action_with_its_reason() {
        let host = host_with_reviewer(ReviewOutcome::Caution {
            reason: "broader than the request".to_owned(),
        });
        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Deny, "{reason}");
        assert_eq!(reason, "broader than the request");
    }

    #[test]
    fn a_review_that_cannot_complete_holds_the_action_without_ending_the_turn() {
        // An unavailable reviewer is not a judgment about the action, and it
        // must not become an approval.
        let host = host_with_reviewer(ReviewOutcome::Unavailable {
            reason: "the reviewer could not be reached".to_owned(),
        });
        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Deny, "{reason}");
        assert!(reason.contains("could not be reached"), "{reason}");
    }

    #[test]
    fn an_allowlisted_action_never_reaches_the_reviewer() {
        // A rule that already decided must not cost a review, or the budget
        // would be spent on the commands that were never in question.
        let host = host_with_reviewer(ReviewOutcome::Caution {
            reason: "should not be consulted".to_owned(),
        });
        let (outcome, reason) = host.decide("read_file", Some("src/main.rs"));
        assert_eq!(outcome, Outcome::Allow, "{reason}");
        assert!(!reason.contains("should not be consulted"), "{reason}");
    }

    #[test]
    fn a_reviewer_is_only_consulted_in_automatic_mode() {
        let mut host = host_with_reviewer(ReviewOutcome::Caution {
            reason: "should not be consulted".to_owned(),
        });
        host.mode = PermissionMode::FullAccess;
        let (outcome, _) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Allow);
    }

    #[test]
    fn an_unresolved_action_is_never_approved_without_a_decision() {
        // Nothing judged it: no rule allowed it and no reviewer saw it. Whether
        // a person is watching is not a decision, so it stays unresolved.
        let mut host = test_host();
        host.mode = PermissionMode::Auto;
        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Ask, "{reason}");
    }

    #[test]
    fn the_status_line_names_the_model_and_the_mode() {
        let host = test_host();
        let line = host.status_line(120);
        assert!(line.contains("test"), "{line}");
        assert!(line.contains("auto"), "{line}");
        assert!(line.contains("ctx"), "{line}");
    }

    #[test]
    fn the_status_line_reports_the_context_already_spent() {
        let host = test_host();
        let before = host.status_line(120);
        assert!(before.contains("ctx 0%"), "{before}");
        host.record_context_size(64_000);
        let after = host.status_line(120);
        assert!(
            !after.contains("ctx 0%"),
            "usage was not reflected: {after}"
        );
    }

    #[test]
    fn wrapping_incrementally_matches_wrapping_the_whole_text() {
        // The cache exists to make a delta cheap, not to change what is drawn.
        // A word straddling a delta boundary must still break where wrapping the
        // finished text would break it.
        let text = "the quick brown fox jumps over the lazy dog again and again";
        let mut lazy = LazyRows::default();
        let mut prefix = String::new();
        for word in text.split_inclusive(' ') {
            prefix.push_str(word);
            let incremental = rows_for(&mut lazy, &prefix, 20);
            let whole = rune_term::width::wrap(&prefix, 20);
            assert_eq!(
                incremental, whole,
                "incremental wrapping diverged at {prefix:?}"
            );
        }
    }

    #[test]
    fn wrapping_across_several_lines_keeps_every_line() {
        let mut lazy = LazyRows::default();
        let mut prefix = String::new();
        for part in ["first line\n", "second line\n", "third line"] {
            prefix.push_str(part);
            let incremental = rows_for(&mut lazy, &prefix, 20);
            assert_eq!(incremental, rune_term::width::wrap(&prefix, 20));
        }
        let rows = rows_for(&mut lazy, &prefix, 20);
        assert_eq!(rows.len(), 3, "{rows:?}");
    }

    #[test]
    fn clearing_a_lane_forgets_its_rows() {
        // A cleared lane must not redraw the response that was dropped.
        let mut lazy = LazyRows::default();
        let rows = rows_for(&mut lazy, "an old answer", 20);
        assert!(!rows.is_empty());
        let rows = rows_for(&mut lazy, "", 20);
        assert!(rows.is_empty(), "stale rows survived: {rows:?}");
    }

    #[test]
    fn context_usage_is_the_size_of_the_conversation_not_a_running_sum() {
        // Every turn resends the whole conversation, so an input count already
        // contains the earlier turns. Summing them counts the same history once
        // per turn, which is what made a session appear to fill its window
        // several times over.
        let host = test_host();
        host.record_context_size(8_000);
        host.record_context_size(8_400);
        host.record_context_size(8_900);
        assert_eq!(
            host.context_used.load(std::sync::atomic::Ordering::Relaxed),
            8_900,
            "the readings were summed rather than taken as a high-water mark"
        );
    }

    #[test]
    fn a_shrinking_reading_never_lowers_the_reported_size() {
        // A turn that reports fewer input tokens than the last is not the
        // conversation getting smaller; taking the smaller value would hide the
        // history that is still being sent.
        let host = test_host();
        host.record_context_size(9_000);
        host.record_context_size(7_000);
        assert_eq!(
            host.context_used.load(std::sync::atomic::Ordering::Relaxed),
            9_000
        );
    }

    #[test]
    fn a_declared_model_window_overrides_the_compiled_default() {
        // A model that accepts a million tokens must not be budgeted against a
        // hundred and twenty-eight thousand, which reports a nearly empty
        // window as a fifth full.
        let settings = Settings {
            context_window: Some(1_000_000),
            ..Settings::default()
        };
        assert_eq!(context_limit(&settings, &BudgetSet::new()), 1_000_000);

        // Without a declaration the compiled default stands, because guessing
        // high would let a conversation grow past what the model accepts.
        let undeclared = Settings::default();
        assert_eq!(
            context_limit(&undeclared, &BudgetSet::new()),
            rune_net::catalog::DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn a_colorless_terminal_cannot_be_forced_into_color() {
        // Capability beats preference: the setting expresses a wish, the
        // terminal states what it accepts, and the terminal wins.
        let config = colorless_config();
        let theme = resolve_theme_for(&config, true);
        assert_eq!(theme.base(), Theme::no_color().base());

        // The same configuration with a terminal that accepts color does take
        // the configured theme, which is what shows the capability is the input
        // rather than the configuration.
        let with_color = resolve_theme_for(&config, false);
        assert_ne!(with_color.base(), Theme::no_color().base());
    }

    #[test]
    fn the_truecolor_check_reads_the_advertised_variables() {
        // The helper consults the environment, so the assertion is on its shape
        // rather than on a value a test would have to mutate globally.
        let supported = truecolor_supported();
        assert_eq!(supported, supported);
    }

    #[test]
    fn the_shell_reports_an_unknown_command_without_leaving() {
        let mut output = Vec::new();
        let action = handle_command(
            "nope",
            "",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::Continue);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("unknown command"), "{text}");
        assert!(text.contains("/help"), "{text}");
    }

    #[test]
    fn naming_a_model_inline_asks_for_a_switch() {
        let mut output = Vec::new();
        let action = handle_command(
            "model",
            "vendor/name",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::SetModel("vendor/name".to_owned()));
    }

    #[test]
    fn the_plural_models_spelling_means_the_same_thing() {
        // Reaching for the plural is what most people do first, and refusing it
        // would teach nothing.
        let mut output = Vec::new();
        let action = handle_command(
            "models",
            "",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::PickModel);

        let mut output = Vec::new();
        let action = handle_command(
            "models",
            "beta-1",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::SetModel("beta-1".to_owned()));
    }

    #[test]
    fn a_bare_model_command_asks_for_the_picker() {
        // The whole point of the command without an argument is the list, so a
        // bare invocation must not be read as switching to the empty model.
        let mut output = Vec::new();
        let action = handle_command(
            "model",
            "   ",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::PickModel);
    }

    #[test]
    fn switching_the_model_changes_what_the_session_sends() {
        let host = test_host();
        assert_eq!(host.model(), "test");
        host.set_model("other/model", None);
        assert_eq!(host.model(), "other/model");
    }

    #[test]
    fn a_reported_window_is_adopted_when_none_was_configured() {
        // Without this the session budgets against the compiled default, which
        // understates a large model so badly that it looks nearly full before
        // anything has been said.
        let host = test_host();
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            rune_net::catalog::DEFAULT_CONTEXT_WINDOW
        );
        host.adopt_context_window(Some(1_000_000), false);
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            1_000_000
        );
    }

    #[test]
    fn a_configured_window_is_not_overridden_by_the_endpoint() {
        // A user who declared a window is describing the model they selected,
        // and an endpoint may advertise a figure that counts only part of the
        // conversation, so their choice wins.
        let host = test_host();
        let before = host
            .context_limit
            .load(std::sync::atomic::Ordering::Relaxed);
        host.adopt_context_window(Some(9_999_999), true);
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            before
        );
    }

    #[test]
    fn an_absent_or_zero_reported_window_leaves_the_limit_alone() {
        let host = test_host();
        let before = host
            .context_limit
            .load(std::sync::atomic::Ordering::Relaxed);
        host.adopt_context_window(None, false);
        host.adopt_context_window(Some(0), false);
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            before
        );
    }

    #[test]
    fn switching_the_model_moves_the_context_window_only_when_one_is_known() {
        // A window carried over from a larger model understates how full a
        // smaller one is, but inventing one is worse, so an unknown window is
        // left as it stands.
        let host = test_host();
        let before = host
            .context_limit
            .load(std::sync::atomic::Ordering::Relaxed);
        host.set_model("small", None);
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            before
        );
        host.set_model("large", Some(1_000_000));
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            1_000_000
        );
        // A zero means nothing is known, not a window of nothing.
        host.set_model("zero", Some(0));
        assert_eq!(
            host.context_limit
                .load(std::sync::atomic::Ordering::Relaxed),
            1_000_000
        );
    }

    #[test]
    fn the_status_command_names_the_model_in_effect() {
        let host = test_host();
        host.set_model("picked/model", None);
        let info = host.info("chat_completions", "https://example.invalid/v1");
        let rendered = render_status(&info);
        assert!(rendered.contains("picked/model"), "{rendered}");
        assert!(rendered.contains("chat_completions"), "{rendered}");
        assert!(rendered.contains("sessiontest1"), "{rendered}");
    }

    #[test]
    fn a_status_line_never_prints_the_credential() {
        // The endpoint carries the credential beside its address, so a status
        // row built from the wrong field would leak the key to the screen.
        let host = test_host();
        let info = host.info("chat_completions", "https://example.invalid/v1");
        let rendered = render_status(&info);
        assert!(!rendered.contains("sk-"), "{rendered}");
    }

    #[test]
    fn the_status_command_names_an_unknown_window_rather_than_a_share_of_it() {
        let mut info = test_info();
        info.context_limit = 0;
        info.context_used = 500;
        let rendered = render_status(&info);
        assert!(rendered.contains("window unknown"), "{rendered}");
        assert!(!rendered.contains('%'), "{rendered}");
    }

    #[test]
    fn the_cost_command_counts_only_what_was_reported() {
        // An absent count must not be added as zero, or a provider that reports
        // nothing looks like a provider that charged nothing.
        let mut totals = Totals::default();
        assert_eq!(
            render_cost(&test_info_with(totals.clone())),
            "no requests have been made in this session yet"
        );
        totals.record(&rune_net::stream::Usage {
            input_tokens: Some(1200),
            output_tokens: Some(450),
            ..rune_net::stream::Usage::default()
        });
        totals.record(&rune_net::stream::Usage {
            input_tokens: None,
            output_tokens: Some(50),
            ..rune_net::stream::Usage::default()
        });
        let rendered = render_cost(&test_info_with(totals));
        assert!(rendered.contains("Requests: 2"), "{rendered}");
        assert!(rendered.contains("1.2k in"), "{rendered}");
        assert!(rendered.contains("500 out"), "{rendered}");
    }

    #[test]
    fn usage_from_a_turn_is_added_to_the_session_total() {
        let host = test_host();
        host.record_usage(&rune_net::stream::Usage {
            input_tokens: Some(10),
            output_tokens: Some(4),
            ..rune_net::stream::Usage::default()
        });
        host.record_usage(&rune_net::stream::Usage {
            input_tokens: Some(5),
            output_tokens: Some(1),
            cache_read_tokens: Some(3),
            ..rune_net::stream::Usage::default()
        });
        let info = host.info("p", "e");
        assert_eq!(info.totals.requests, 2);
        assert_eq!(info.totals.input_tokens, 15);
        assert_eq!(info.totals.output_tokens, 5);
        assert_eq!(info.totals.cache_read_tokens, 3);
    }

    #[test]
    fn the_settings_command_reports_what_is_in_force() {
        let rendered = render_settings(&test_info());
        assert!(rendered.contains("permissions    auto"), "{rendered}");
        assert!(rendered.contains("context window 128.0k"), "{rendered}");
    }

    #[test]
    fn the_undo_command_is_recognized_rather_than_unknown() {
        let mut output = Vec::new();
        let action = handle_command(
            "undo",
            "",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::Undo);
    }

    #[test]
    fn a_bare_rename_asks_for_a_title() {
        // A rename with no title would set the session title to nothing, which
        // is not something the user can undo from the interface.
        let mut output = Vec::new();
        let action = handle_command(
            "rename",
            "  ",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::Continue);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("/rename <title>"), "{text}");
    }

    #[test]
    fn renaming_a_session_asks_for_a_new_title() {
        let mut output = Vec::new();
        let action = handle_command(
            "rename",
            "fix the parser",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::Rename("fix the parser".to_owned()));
    }

    #[test]
    fn only_the_two_file_rewriting_tools_are_treated_as_mutations() {
        // A shell command changes something, but what it changed cannot be read
        // off its arguments, so offering an undo for it would be a lie.
        let write = serde_json::json!({ "path": "a.txt", "content": "x" });
        assert_eq!(
            mutating_path("write_file", &write).map(Utf8Path::to_owned),
            Some(Utf8PathBuf::from("a.txt"))
        );
        assert_eq!(
            mutating_path("edit_file", &write).map(Utf8Path::to_owned),
            Some(Utf8PathBuf::from("a.txt"))
        );
        assert_eq!(mutating_path("shell", &write), None);
        assert_eq!(mutating_path("read_file", &write), None);
        // A call missing its path is not a mutation this can track.
        assert_eq!(mutating_path("write_file", &serde_json::json!({})), None);
    }

    #[test]
    fn undoing_a_creation_removes_the_file() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let host = test_host();
        let created = root.join("made.txt");

        // Captured before the file exists, which is what the tool boundary does
        // on the call that creates it.
        host.remember(&created);
        std::fs::write(&created, "created by the session").expect("write");
        host.undo().expect("undo");
        assert!(!created.exists(), "the created file was left behind");
    }

    #[test]
    fn undoing_a_change_restores_the_original_bytes() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let host = test_host();
        let changed = root.join("kept.txt");
        std::fs::write(&changed, "original\n").expect("write");

        host.remember(&changed);
        std::fs::write(&changed, "replaced\n").expect("write");
        host.undo().expect("undo");
        assert_eq!(
            std::fs::read_to_string(&changed).expect("read"),
            "original\n"
        );
    }

    #[test]
    fn the_first_state_of_a_file_is_what_undo_returns_to() {
        // A chain of edits must come back to where the session found the file,
        // not to the middle of the chain.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let host = test_host();
        let path = root.join("chain.txt");
        std::fs::write(&path, "one\n").expect("write");

        host.remember(&path);
        std::fs::write(&path, "two\n").expect("write");
        host.remember(&path);
        std::fs::write(&path, "three\n").expect("write");
        host.undo().expect("undo");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "one\n");
    }

    #[test]
    fn undoing_twice_reports_nothing_left_rather_than_doing_it_again() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let host = test_host();
        let path = root.join("once.txt");
        assert!(!host.can_undo());
        std::fs::write(&path, "body\n").expect("write");
        host.remember(&path);
        assert!(host.can_undo());
        host.undo().expect("undo");
        assert!(!host.can_undo());
    }

    #[test]
    fn compacting_a_short_conversation_says_so_rather_than_failing() {
        let host = test_host();
        let mut history = History::new();
        history.push_user("hello");
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let mut sink = LockedSink {
            stream: Arc::new(Mutex::new(SharedBuffer(Arc::clone(&captured)))),
        };
        compact_history(&host, &mut history, &mut sink).expect("compacted");
        let text = String::from_utf8(captured.lock().expect("lock").clone()).expect("utf8");
        assert!(text.contains("nothing to compact"), "{text}");
        assert_eq!(history.len(), 1, "the conversation was changed anyway");
    }

    /// A sink that appends to a buffer the test can read back.
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_last_reply_is_read_from_the_conversation_not_the_screen() {
        let mut history = History::new();
        history.push_user("do a thing");
        assert_eq!(last_reply(&history), None, "a user turn is not a reply");
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: "done".to_owned(),
        }]);
        assert_eq!(last_reply(&history).as_deref(), Some("done"));
    }

    #[test]
    fn the_last_reply_is_the_most_recent_one() {
        let mut history = History::new();
        for text in ["first", "second"] {
            history.push_assistant(vec![rune_net::message::ContentPart::Text {
                text: text.to_owned(),
            }]);
        }
        assert_eq!(last_reply(&history).as_deref(), Some("second"));
    }

    #[test]
    fn a_reply_holds_no_tool_call_arguments() {
        // A tool call carries JSON the user never wrote, so copying it would
        // paste an argument list into whatever they are working on.
        let mut history = History::new();
        history.push_assistant(vec![
            rune_net::message::ContentPart::Text {
                text: "looking".to_owned(),
            },
            rune_net::message::ContentPart::ToolCall {
                id: rune_core::id::ToolCallId::new("c1").expect("id"),
                name: "shell".to_owned(),
                arguments: "{\"command\":\"rm -rf /\"}".to_owned(),
            },
        ]);
        let reply = last_reply(&history).expect("a reply");
        assert_eq!(reply, "looking");
        assert!(!reply.contains("rm -rf"), "{reply}");
    }

    #[test]
    fn base64_matches_the_standard_encoding() {
        // The clipboard is decoded by the terminal, so the alphabet and padding
        // have to be exactly the standard ones.
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), expected, "{input:?}");
        }
    }

    #[test]
    fn base64_encodes_multibyte_text_by_its_bytes() {
        // The text is UTF-8 on the wire, so an emoji is four bytes and must not
        // be encoded as one character.
        assert_eq!(base64_encode("é".as_bytes()), "w6k=");
    }

    #[test]
    fn the_copy_and_new_commands_are_recognized() {
        for (name, expected) in [("copy", Handled::Copy), ("new", Handled::NewSession)] {
            let mut output = Vec::new();
            let action = handle_command(
                name,
                "",
                &empty_commands(),
                None,
                Utf8Path::new("/w"),
                &test_info(),
                &mut output,
            )
            .expect("handled");
            assert_eq!(action, expected, "/{name}");
        }
    }

    #[test]
    fn starting_a_new_session_repoints_the_status_line() {
        let host = test_host();
        assert_eq!(host.session_id_name(), "sessiontest1");
        host.set_session_id("another0session");
        assert_eq!(host.session_id_name(), "another0session");
        let info = host.info("p", "e");
        assert_eq!(info.session_id, "another0session");
    }

    #[test]
    fn starting_a_new_session_forgets_the_old_context_reading() {
        let host = test_host();
        host.record_context_size(4_000);
        let info = host.info("p", "e");
        assert_eq!(info.context_used, 4_000);
        host.forget_context();
        let info = host.info("p", "e");
        assert_eq!(info.context_used, 0);
    }

    #[test]
    fn the_dropdown_offers_a_command_being_typed() {
        let theme = Theme::no_color();
        let rows = completion_rows("/mod", 0, &theme, false);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].starts_with("> /model"), "{rows:?}");
        assert!(rows[1].starts_with("  /models"), "{rows:?}");
        // Each row carries what the command does, which is what makes the list
        // usable without trying every name.
        assert!(rows[0].contains("choose a model"), "{rows:?}");
        assert!(rows[1].contains("same as"), "{rows:?}");
    }

    #[test]
    fn a_bare_slash_offers_every_command() {
        let theme = Theme::no_color();
        let rows = completion_rows("/", 0, &theme, false);
        assert_eq!(rows.len(), rune_term::commands::BUILTINS.len(), "{rows:?}");
    }

    #[test]
    fn the_dropdown_is_absent_unless_a_command_is_being_typed() {
        let theme = Theme::no_color();
        for line in ["", "hello", "/help me", "/model x", "a/b", "/zzz"] {
            assert!(
                completion_rows(line, 0, &theme, false).is_empty(),
                "{line:?} offered rows"
            );
        }
        // A whole command name is still offered, so the row does not vanish as
        // the last letter is typed.
        assert!(!completion_rows("/help", 0, &theme, false).is_empty());
    }

    #[test]
    fn the_highlighted_row_is_the_one_the_arrows_moved_to() {
        let theme = Theme::no_color();
        let first = completion_rows("/mod", 0, &theme, false);
        assert!(first[0].starts_with('>'), "{first:?}");
        let second = completion_rows("/mod", 1, &theme, false);
        assert!(second[1].starts_with('>'), "{second:?}");
        assert!(second[0].starts_with("  "), "{second:?}");
    }

    #[test]
    fn the_rows_line_up_their_descriptions() {
        let theme = Theme::no_color();
        let rows = completion_rows("/mod", 0, &theme, false);
        let summaries = rune_term::commands::matching("mod");
        let columns: Vec<usize> = rows
            .iter()
            .zip(summaries.iter())
            .map(|(row, entry)| {
                row.find(entry.summary)
                    .unwrap_or_else(|| panic!("no summary in {row:?}"))
            })
            .collect();
        assert_eq!(columns.len(), rows.len(), "{rows:?}");
        for column in &columns {
            assert_eq!(*column, columns[0], "the rows are ragged: {rows:?}");
        }
    }

    #[test]
    fn accepting_the_highlighted_row_names_it() {
        // Tab completes to the highlighted command, and Enter takes it too when
        // the name is not yet whole, so a half-typed command is never run.
        let theme = Theme::no_color();
        let rows = completion_rows("/mod", 0, &theme, false);
        let chosen = open_completion("/mod", &rows, 0).expect("a completion");
        assert_eq!(chosen.name, "model");
        let rows = completion_rows("/mod", 1, &theme, false);
        let chosen = open_completion("/mod", &rows, 1).expect("a completion");
        assert_eq!(chosen.name, "models");
        // With no dropdown there is nothing to accept.
        assert!(open_completion("hello", &[], 0).is_none());
    }

    #[test]
    fn a_colorless_theme_emits_no_escapes_in_the_dropdown() {
        let theme = Theme::no_color();
        let plain = completion_rows("/mod", 0, &theme, false);
        assert!(!plain.is_empty(), "nothing matched");
        for row in plain {
            assert!(!row.contains('\u{1b}'), "{row:?}");
        }
        // A colored theme does style them.
        let styled_rows = completion_rows("/mod", 0, &Theme::fx_dark(), true);
        assert!(
            styled_rows.iter().any(|r| r.contains('\u{1b}')),
            "{styled_rows:?}"
        );
    }

    #[test]
    fn the_help_text_names_the_commands_that_exist() {
        // A command that is handled but unlisted is one nobody finds.
        let mut output = Vec::new();
        handle_command(
            "help",
            "",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        let text = String::from_utf8_lossy(&output);
        for command in [
            "/model", "/models", "/status", "/cost", "/compact", "/undo", "/copy", "/new",
            "/rename", "/tree",
        ] {
            assert!(text.contains(command), "{command} is missing from: {text}");
        }
    }

    #[test]
    fn the_quit_command_leaves_the_shell() {
        let mut output = Vec::new();
        assert_eq!(
            handle_command(
                "quit",
                "",
                &empty_commands(),
                None,
                Utf8Path::new("/w"),
                &test_info(),
                &mut output
            )
            .expect("handled"),
            Handled::Exit
        );
        assert_eq!(
            handle_command(
                "exit",
                "",
                &empty_commands(),
                None,
                Utf8Path::new("/w"),
                &test_info(),
                &mut output
            )
            .expect("handled"),
            Handled::Exit
        );
    }

    #[test]
    fn a_user_command_expands_for_review_instead_of_running() {
        let mut discovery = rune_context::commands::Discovery::default();
        discovery.commands.push(rune_context::commands::Command {
            name: "fix".to_owned(),
            description: "Fix it".to_owned(),
            argument_hint: None,
            body: "Fix $1 now".to_owned(),
            origin: rune_context::commands::Origin::Project,
            source: None,
        });

        let mut output = Vec::new();
        let handled = handle_command(
            "fix",
            "parser",
            &discovery,
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(handled, Handled::Expand("Fix parser now".to_owned()));
    }

    #[test]
    fn a_command_missing_its_argument_reports_instead_of_expanding() {
        let mut discovery = rune_context::commands::Discovery::default();
        discovery.commands.push(rune_context::commands::Command {
            name: "fix".to_owned(),
            description: "Fix it".to_owned(),
            argument_hint: None,
            body: "Fix $1 now".to_owned(),
            origin: rune_context::commands::Origin::Project,
            source: None,
        });

        let mut output = Vec::new();
        let handled = handle_command(
            "fix",
            "",
            &discovery,
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        assert_eq!(handled, Handled::Continue);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("$1"), "{text}");
    }

    #[test]
    fn help_reports_a_command_file_that_was_skipped() {
        let mut discovery = rune_context::commands::Discovery::default();
        discovery.warnings.push(rune_context::commands::Warning {
            path: Utf8PathBuf::from("/p/.rune/commands/help.md"),
            reason: "`/help` is a built-in command, so this file was skipped".to_owned(),
        });

        let mut output = Vec::new();
        handle_command(
            "help",
            "",
            &discovery,
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("skipped"), "{text}");
        assert!(text.contains("built-in"), "{text}");
    }

    #[test]
    fn help_lists_the_commands() {
        let mut output = Vec::new();
        handle_command(
            "help",
            "",
            &empty_commands(),
            None,
            Utf8Path::new("/w"),
            &test_info(),
            &mut output,
        )
        .expect("handled");
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("/help"));
        assert!(text.contains("/quit"));
    }

    #[test]
    fn a_session_driven_by_a_script_runs_every_line() {
        let mut source = ScriptedSource::new(["first", "second", "/quit", "never"]);
        let mut seen = 0_usize;
        {
            let mut shell = Shell::new(&mut source);
            let reason = shell
                .run(|input| {
                    seen = seen.saturating_add(1);
                    match input {
                        Input::Command { name, .. } if name == "quit" => Ok(Action::Exit),
                        _ => Ok(Action::Continue),
                    }
                })
                .expect("ran");
            assert_eq!(reason, ExitReason::Requested);
        }
        // The line after the exit was never read.
        assert_eq!(seen, 3);
    }

    #[test]
    fn a_reported_turn_yields_its_text_as_lines() {
        let host = test_host();
        let outcome = turn::TurnOutcome {
            stop_reason: StopReason::Completed,
            text: "the answer".to_owned(),
            reasoning: String::new(),
            usage: rune_net::stream::Usage::default(),
            steps: 1,
            calls: Vec::new(),
        };
        let lines = report_turn(&outcome, &host).expect("reported");
        assert!(
            lines.iter().any(|line| line.contains("the answer")),
            "{lines:?}"
        );
    }

    #[test]
    fn reporting_a_step_limit_names_it() {
        let host = test_host();
        let outcome = turn::TurnOutcome {
            stop_reason: StopReason::StepLimit,
            text: String::new(),
            reasoning: String::new(),
            usage: rune_net::stream::Usage::default(),
            steps: 40,
            calls: Vec::new(),
        };
        let lines = report_turn(&outcome, &host).expect("reported");
        assert!(
            lines.iter().any(|line| line.contains("step limit")),
            "{lines:?}"
        );
    }

    #[test]
    fn reporting_a_denied_call_names_the_tool() {
        let host = test_host();
        host.emit(Event::ToolDenied {
            call: turn::PreparedCall {
                id: "c1".to_owned(),
                name: "shell".to_owned(),
                arguments: "{}".to_owned(),
            },
            reason: "denied by a rule".to_owned(),
        });
        let outcome = turn::TurnOutcome {
            stop_reason: StopReason::Completed,
            text: String::new(),
            reasoning: String::new(),
            usage: rune_net::stream::Usage::default(),
            steps: 1,
            calls: Vec::new(),
        };
        let lines = report_turn(&outcome, &host).expect("reported");
        let text = lines.join("\n");
        assert!(text.contains("refused shell"), "{text}");
    }

    #[test]
    fn preparing_a_session_without_a_provider_fails_before_the_terminal() {
        let settings = Settings::default();
        let paths = Paths::resolve(Some("/tmp"), None, None, None, Some("/tmp/s"));
        let outcome = prepare(&settings, &paths, Utf8Path::new("/tmp"), None);
        let Err(err) = outcome else {
            panic!("a session started with no provider configured");
        };
        assert_eq!(err.code(), ErrorCode::AuthenticationRequired);
        assert!(
            err.hint().is_some(),
            "the failure does not say how to connect a provider"
        );
    }

    #[test]
    fn a_session_runs_with_the_rules_that_ship_with_it() {
        // A session built with an empty rule set resolves every action to the
        // mode default, so in automatic mode nothing is ever allowed and each
        // call comes back as an uncollected approval. The rules a session runs
        // with must be the ones a fresh install describes.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = Paths::resolve(Some(root.as_str()), None, None, None, None);
        paths.ensure_roots().expect("roots");

        let provider = rune_core::config::parse_provider("chat_completions");
        crate::provider_setup::connect(&paths, provider.as_str(), "sk-test").expect("credential");

        let settings = Settings {
            provider,
            base_url: Some("https://example.invalid/v1".to_owned()),
            model: "test-model".to_owned(),
            ..Settings::default()
        };

        let config =
            prepare(&settings, &paths, Utf8Path::new("/tmp"), None).expect("a session prepares");

        assert!(
            !config.rules.rules().is_empty(),
            "the session runs without any rules, so every action falls to the mode default"
        );
        assert_eq!(
            config
                .rules
                .evaluate("glob_files", "*.md", Outcome::Ask)
                .outcome,
            Outcome::Allow,
            "a read tool would ask for approval it never gets"
        );
        // The web tools are on unless the run says otherwise, so a fresh session
        // can look something up rather than reporting that it was refused.
        assert_eq!(
            config
                .rules
                .evaluate("web_fetch", "https://example.com", Outcome::Deny)
                .outcome,
            Outcome::Allow,
            "the web tools are refused on a default configuration"
        );
        // And turning them off still refuses, so the setting is a real switch.
        let off = Settings {
            web_tools: false,
            ..settings.clone()
        };
        let off_config =
            prepare(&off, &paths, Utf8Path::new("/tmp"), None).expect("a session prepares");
        assert_eq!(
            off_config
                .rules
                .evaluate("web_fetch", "https://example.com", Outcome::Allow)
                .outcome,
            Outcome::Deny,
            "turning the web tools off did not refuse them"
        );
    }

    /// Builds an empty command set.
    fn empty_commands() -> rune_context::commands::Discovery {
        rune_context::commands::Discovery::default()
    }

    /// Builds a session config whose terminal accepts no color.
    fn colorless_config() -> SessionConfig {
        SessionConfig {
            settings: Settings::default(),
            paths: Paths::resolve(Some("/tmp"), None, None, None, Some("/tmp/s")),
            resume: None,
            workspace: Utf8Path::new("/tmp").to_owned(),
            endpoint: Endpoint::new("https://example.invalid", "k"),
            dialect: Box::new(rune_net::chat_completions::ChatCompletions),
            registry: Registry::new(),
            rules: RuleSet::new(),
        }
    }

    /// Builds a host with no network, for rendering tests.
    fn test_info() -> SessionInfo<'static> {
        test_info_with(Totals::default())
    }

    /// Builds the same report with a chosen spend, for the cost assertions.
    fn test_info_with(totals: Totals) -> SessionInfo<'static> {
        SessionInfo {
            model: "test".to_owned(),
            provider: "chat_completions",
            endpoint: "https://example.invalid/v1",
            mode: PermissionMode::Auto,
            effort: Effort::Auto,
            session_id: "sessiontest1".to_owned(),
            workspace: "/w",
            context_used: 0,
            context_limit: rune_net::catalog::DEFAULT_CONTEXT_WINDOW,
            totals,
        }
    }

    fn test_host() -> SessionHost {
        let mut registry = Registry::new();
        registry
            .insert(Box::new(rune_tools::ReadFile::new()))
            .expect("registered");
        SessionHost {
            endpoint: Endpoint::new("https://example.invalid", "k"),
            dialect: Box::new(rune_net::chat_completions::ChatCompletions),
            model: Mutex::new("test".to_owned()),
            instructions: String::new(),
            tools: Vec::new(),
            rules: crate::permissions::validated(&Settings::default()).expect("rules"),
            mode: PermissionMode::Auto,
            effort: Effort::Auto,
            fast_mode: false,
            limits: BudgetSet::new(),
            context: ExecutionContext::new(Utf8PathBuf::from("/tmp")),
            registry,
            cancellation: Cancellation::new(),
            steering: SteeringQueue::new(4),
            events: Arc::new(Mutex::new(Vec::new())),
            context_used: std::sync::atomic::AtomicU64::new(0),
            context_limit: std::sync::atomic::AtomicU64::new(
                rune_net::catalog::DEFAULT_CONTEXT_WINDOW,
            ),
            theme: Theme::no_color(),
            session_id: Mutex::new("sessiontest1".to_owned()),
            workspace: "/tmp".to_owned(),
            truecolor: false,
            width: std::sync::atomic::AtomicU16::new(80),
            height: std::sync::atomic::AtomicU16::new(24),
            inline: Mutex::new(rune_term::inline::Inline::new(80)),
            provider_order: Vec::new(),
            provider_strict: false,
            streaming: Arc::new(Mutex::new(StreamingText::default())),
            live_out: Arc::new(Mutex::new(None)),
            reasoning: Mutex::new("\u{1b}[2m".to_owned()),
            reviewer: None,
            totals: Mutex::new(Totals::default()),
            undo: Mutex::new(BTreeMap::new()),
            review_session: Arc::new(Mutex::new(ReviewSession::new(&BudgetSet::new()))),
        }
    }
}
