//! The interactive session.
//!
//! Connects the terminal shell loop to the agent. One turn runs at a time; a
//! line typed while a turn is running is queued rather than refused, which is
//! what makes the shell usable while the model is working.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, Write as _};
use std::sync::{Arc, Mutex, mpsc};

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
use rune_tools::ask_user::{Answer, Answerer, Question, Unavailable};
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
    /// Question bridge, enabled only while terminal input is being polled.
    questions: Arc<TerminalQuestions>,
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
    answer_rows: transcript::AssistantRows,
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

/// One call waiting for the input thread's answer. No answer creates a grant.
struct ApprovalRequest {
    tool: String,
    target: String,
    reason: String,
    answer: mpsc::SyncSender<Outcome>,
}

/// A tool worker hands questions to the thread that owns terminal input.
#[derive(Debug)]
struct QuestionRequest {
    questions: Vec<Question>,
    answer: mpsc::SyncSender<Result<Answer>>,
}

#[derive(Debug, Default)]
struct TerminalQuestions {
    requests: Mutex<Option<mpsc::Sender<QuestionRequest>>>,
    cancellation: Cancellation,
}

impl Answerer for TerminalQuestions {
    fn ask(&self, questions: &[Question], context: &ExecutionContext) -> Result<Answer> {
        let Some(requests) = self.requests.lock().ok().and_then(|slot| slot.clone()) else {
            return Unavailable.ask(questions, context);
        };
        let (answer, response) = mpsc::sync_channel(1);
        if requests
            .send(QuestionRequest {
                questions: questions.to_vec(),
                answer,
            })
            .is_err()
        {
            return Unavailable.ask(questions, context);
        }
        loop {
            self.cancellation.check()?;
            context.check_cancelled()?;
            match response.recv_timeout(rune_term::shell::POLL_INTERVAL) {
                Ok(answer) => {
                    self.cancellation.check()?;
                    context.check_cancelled()?;
                    return answer;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Unavailable.ask(questions, context);
                }
            }
        }
    }
}

/// Host state for a turn.
struct SessionHost {
    recorder: Option<Arc<Mutex<Recorder>>>,
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
    /// Largest observed conversation size, initially estimated on resume.
    context_used: std::sync::atomic::AtomicU64,
    /// Present while the meter is seeded from saved history.
    context_source: Mutex<Option<&'static str>>,
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
    /// Held across a whole frame, paint and write together.
    ///
    /// Two threads draw once a turn runs on its own thread: the turn draws the
    /// text arriving, and the loop draws the line being typed. Without one lock
    /// covering both steps, one frame's bytes can land inside another's and the
    /// screen shows a mix of two.
    frame: Mutex<()>,
    /// Complete recorded conversation, retained even when model context compacts.
    transcript: Mutex<Vec<Entry>>,
    /// History turns already captured, adjusted whenever compaction renumbers it.
    transcript_history_len: std::sync::atomic::AtomicUsize,
    /// Prevents streaming frames from painting over the transcript or editor.
    transcript_open: std::sync::atomic::AtomicBool,
    /// The line being typed while a turn runs, with the caret's display column.
    ///
    /// Held on the host because the streaming thread redraws the whole frame
    /// for every token, and a frame drawn without this would put an empty
    /// prompt over the correction the user is halfway through typing. The
    /// column is the reader's rather than one worked out from the text, because
    /// the caret may be mid-line and a wide character takes two columns.
    typed: Mutex<(String, usize)>,
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
    /// Attached only while a terminal input loop can collect an answer.
    approval_requests: Mutex<Option<mpsc::Sender<ApprovalRequest>>>,
    questions: Arc<TerminalQuestions>,
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

    fn prepare_request(
        &self,
        history: &mut History,
        plan: &mut rune_net::provider::RequestPlan,
        client: &dyn rune_net::fetch::Fetch,
    ) -> Result<()> {
        use rune_agent::compaction::{self, Trigger};
        let estimate = request_estimate(self, plan)?;
        match compaction::trigger(&estimate, &self.limits) {
            Trigger::NotNeeded | Trigger::Approaching => return Ok(()),
            Trigger::Impossible => return Err(compaction::cannot_fit_error()),
            Trigger::Required => {}
        }
        let Some(compaction_plan) = compaction::plan(history, &self.limits) else {
            // A short conversation can cross the trigger while still fitting.
            return Ok(());
        };
        let removed = summarize_history(self, history, &compaction_plan, client, &|| {
            self.cancellation.is_cancelled()
        })?;
        plan.messages = history.to_messages();
        self.emit(Event::ContextCompacted {
            removed_turns: removed,
            remaining_turns: history.len(),
        });
        self.forget_context();
        if request_estimate(self, plan)?.input_tokens > estimate.capacity {
            return Err(compaction::cannot_fit_error());
        }
        Ok(())
    }

    fn emit(&self, event: Event) {
        // Text is accumulated as it arrives and drawn straight away, which is
        // what makes an answer appear while it is being written rather than
        // after the whole response has been received.
        //
        // Each delta is sanitized on its own before it is kept. Model output can
        // carry sequences a terminal acts on, such as a clipboard write or a
        // screen clear, and the rows drawn from this text reach the terminal
        // as they are. A sequence split across two deltas loses its introducer
        // in the first, so what arrives in the second is plain text.
        match &event {
            Event::TextDelta { delta } => {
                let delta = transcript::sanitize(delta);
                if !self.journal_text(Some(&delta)) {
                    return;
                }
                if let Ok(mut streaming) = self.streaming.lock() {
                    streaming.answer.push_str(&delta);
                }
                self.draw_stream();
            }
            Event::ReasoningDelta { delta } => {
                if let Ok(mut streaming) = self.streaming.lock() {
                    streaming.reasoning.push_str(&transcript::sanitize(delta));
                }
                self.draw_stream();
            }
            Event::StepRestarted { .. } => {
                if !self.journal_text(None) {
                    return;
                }
                self.clear_streaming();
            }
            Event::ContextCompacted {
                removed_turns,
                remaining_turns,
            } => self.draw_notice(&format!(
                "compacted {removed_turns} earlier turn(s); {remaining_turns} turn(s) remain"
            )),
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

    fn execute_with_context(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        if name == "web_fetch" {
            return self.registry.call(name, arguments, context);
        }
        self.execute(name, arguments)
    }

    fn decide_private_network(&self, target: Option<&str>) -> (Outcome, String) {
        // Only a rule explicitly naming this authority at a user-controlled
        // layer can grant it. Broad web grants and project rules cannot.
        let mut rules = RuleSet::new();
        for rule in self.rules.rules() {
            if rule.tool == "web_fetch_private"
                && (rule.outcome != Outcome::Allow
                    || rule.layer >= rune_policy::decision::Layer::User)
            {
                rules.push(rule.clone());
            }
        }
        let target = target.unwrap_or("web_fetch_private");
        let decision = rules.evaluate("web_fetch_private", target, Outcome::Ask);
        let reason = format!(
            "private-network access for this fetch and its redirects; {}",
            decision.explain()
        );
        if decision.outcome != Outcome::Ask {
            return (decision.outcome, reason);
        }
        self.request_approval("web_fetch_private", target, reason)
    }

    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        let (outcome, reason) = turn::decide_call_in_workspace(
            &self.rules,
            self.mode,
            name,
            target,
            self.context.workspace(),
        );
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

        self.request_approval(name, target.unwrap_or(name), reason)
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
    /// Waits for a terminal answer while allowing cancellation to wake us.
    fn request_approval(&self, tool: &str, target: &str, reason: String) -> (Outcome, String) {
        let Some(requests) = self
            .approval_requests
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
        else {
            return (Outcome::Ask, reason);
        };
        let (answer, response) = mpsc::sync_channel(1);
        let request = ApprovalRequest {
            tool: tool.to_owned(),
            target: target.to_owned(),
            reason: reason.clone(),
            answer,
        };
        if requests.send(request).is_err() {
            return (
                Outcome::Deny,
                format!("{reason}; approval input is unavailable"),
            );
        }
        loop {
            if self.cancellation.is_cancelled() {
                return (Outcome::Deny, format!("{reason}; approval was cancelled"));
            }
            match response.recv_timeout(rune_term::shell::POLL_INTERVAL) {
                Ok(Outcome::Allow) if !self.cancellation.is_cancelled() => {
                    return (Outcome::Allow, format!("{reason}; approved for this call"));
                }
                Ok(_) => return (Outcome::Deny, format!("{reason}; approval was denied")),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return (
                        Outcome::Deny,
                        format!("{reason}; approval input was closed"),
                    );
                }
            }
        }
    }

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
            context_source: *self
                .context_source
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
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
        if used == 0 {
            return;
        }
        let mut source = self
            .context_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if source.take().is_some() {
            // The first live reading supersedes the resume estimate, even when
            // the saved per-turn usage overestimated a multi-request turn.
            self.context_used
                .store(used, std::sync::atomic::Ordering::Relaxed);
        } else {
            self.context_used
                .fetch_max(used, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Captures new turns before compaction can remove them from model context.
    fn capture_history(&self, history: &History) {
        let start = self
            .transcript_history_len
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(history.len());
        self.transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(history_entries(&history.turns()[start..]));
        self.transcript_history_len
            .store(history.len(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Forgets the context reading, for a conversation that has been replaced.
    fn forget_context(&self) {
        *self
            .context_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
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
            context_source: *self
                .context_source
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
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
        arriving: &[String],
        caret: (u16, u16),
    ) -> Result<Vec<u8>> {
        self.paint_with_menu(settled, activity, prompt, arriving, &[], caret)
    }

    /// Draws the live region with a list opened under the input.
    ///
    /// Separate from [`SessionHost::paint`] so the many callers that draw no
    /// menu do not have to pass an empty slice for one.
    fn paint_with_menu(
        &self,
        settled: &[String],
        activity: Option<&str>,
        prompt: &[String],
        arriving: &[String],
        menu: &[String],
        caret: (u16, u16),
    ) -> Result<Vec<u8>> {
        self.refresh_size();
        let footer_rows = self.status_rows();
        let mut inline = self
            .inline
            .lock()
            .map_err(|_| RuneError::new(ErrorCode::Internal, "the renderer lock was poisoned"))?;
        Ok(inline.frame(&rune_term::inline::Frame {
            settled,
            arriving,
            activity,
            footer: &footer_rows,
            prompt,
            menu,
            caret,
        }))
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

    /// Rows left below the one-line prompt, including menu headings and hints.
    fn menu_room(&self) -> usize {
        self.refresh_size();
        usize::from(self.height.load(std::sync::atomic::Ordering::Relaxed))
            .saturating_sub(1)
            .saturating_sub(self.status_rows().len())
            .saturating_sub(1)
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

    /// Returns the line being typed and the caret's column within it.
    fn typed_line(&self) -> (String, usize) {
        self.typed
            .lock()
            .map_or_else(|_| (String::new(), 0), |typed| typed.clone())
    }

    /// Draws one frame: the arriving text, a notice, and the line being typed.
    ///
    /// Every writer funnels through here so the line being typed is recorded
    /// once, in one place. Two threads draw during a turn, and a frame drawn
    /// without the current line would put an empty prompt over the correction
    /// the user is halfway through typing.
    ///
    /// `notice` is drawn as the activity line, above the input.
    fn draw_frame(&self, notice: Option<&str>, line: &str, column: usize) {
        if let Ok(mut typed) = self.typed.lock() {
            line.clone_into(&mut typed.0);
            typed.1 = column;
        }
        let rows = self.streaming_rows();
        // One lock across painting and writing, so a frame from one thread
        // cannot land inside a frame from the other.
        let Ok(_frame) = self.frame.lock() else {
            return;
        };
        if self
            .transcript_open
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        // Even a frame with nothing in it is painted, because the one before it
        // may have shown a line or a notice that has to be taken off the screen.
        // A frame that changes nothing costs only a caret move.
        let marker = rune_term::shell::prompt();
        let width = usize::from(self.width());
        let (prompt_rows, caret) = transcript::render_draft_at(marker, line, column, width);
        let Ok(painted) = self.paint(&[], notice, &prompt_rows, &rows, caret) else {
            return;
        };
        self.show(&painted);
    }

    /// Draws the arriving text with the line being typed.
    fn draw_stream_with(&self, line: &str, column: usize) {
        self.draw_frame(None, line, column);
    }

    /// Draws the arriving text with the line the user has typed so far.
    fn draw_stream(&self) {
        let (line, column) = self.typed_line();
        self.draw_frame(None, &line, column);
    }

    /// Draws one notice above the input, keeping the line being typed.
    fn draw_notice(&self, notice: &str) {
        let (line, column) = self.typed_line();
        self.draw_frame(Some(notice), &line, column);
    }

    /// Draws one notice, with the line being typed given explicitly.
    fn draw_notice_with(&self, notice: &str, line: &str, column: usize) {
        self.draw_frame(Some(notice), line, column);
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
            rows.extend(answer_rows.rows(answer, width.saturating_sub(2).max(1)));
        }

        // Every row of the answer is handed over. The renderer owns the region
        // height, because only it knows how tall the region it drew was and
        // therefore which rows it can paint over. Trimming here as well would
        // mean two components each holding a different idea of the limit, and
        // the rows dropped here could not be erased by the frame that follows.
        rows
    }

    /// Saves a delta or retry reset before changing the visible answer.
    fn journal_text(&self, delta: Option<&str>) -> bool {
        let Some(recorder) = &self.recorder else {
            return true;
        };
        let mut recorder = recorder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = match delta {
            Some(text) => recorder.assistant_delta(text),
            None => recorder.reset_assistant(),
        };
        if let Err(error) = result {
            recorder.journal_failed(error);
            self.cancellation.cancel();
            return false;
        }
        true
    }

    /// Clears the streamed text, which a finished step has taken over.
    fn clear_streaming(&self) {
        if let Ok(mut streaming) = self.streaming.lock() {
            streaming.answer.clear();
            streaming.reasoning.clear();
            streaming.answer_rows = transcript::AssistantRows::default();
            streaming.reasoning_rows = LazyRows::default();
        }
    }

    /// Returns the sanitized answer currently visible in the live transcript.
    fn partial_answer(&self) -> String {
        self.streaming
            .lock()
            .map(|streaming| streaming.answer.clone())
            .unwrap_or_default()
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
    let mut limits = config.settings.limits.clone();
    // Only a window someone declared sizes the catalog. The compiled default
    // is a guess, and deriving from it would shrink the catalog for every model
    // that states no window rather than keeping the documented fallback.
    adopt_skill_catalog_budget(&mut limits, config.settings.context_window);
    let prompt = build_prompt(&config.workspace, &config.paths.config_root, &limits);

    // A resumed session continues its stored conversation; a new one starts
    // empty and writes a fresh log.
    let (recorder, mut history) = if let Some(id) = &config.resume {
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

    let (initial_context, context_source) = recorder.resumed_context();
    let transcript = if let Some(id) = &config.resume {
        resumed_entries(&history, &session_log::inspect(&config.paths, id)?)
    } else {
        Vec::new()
    };
    let recorder = Arc::new(Mutex::new(recorder));
    let host = SessionHost {
        recorder: Some(Arc::clone(&recorder)),
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
            .with_offline(config.settings.offline)
            .with_allow_unsandboxed(config.settings.allow_unsandboxed),
        registry: config.registry,
        cancellation: config.questions.cancellation.clone(),
        steering: SteeringQueue::from_limits(&limits),
        events: Arc::new(Mutex::new(Vec::new())),
        context_used: std::sync::atomic::AtomicU64::new(initial_context),
        context_source: Mutex::new(context_source),
        context_limit: std::sync::atomic::AtomicU64::new(context_limit(&config.settings, &limits)),
        theme,
        session_id: Mutex::new(
            recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .id()
                .to_string(),
        ),
        workspace: config.workspace.to_string(),
        truecolor: truecolor_supported(),
        // Seeded from the terminal and refreshed on every frame, so a window
        // resized while the session runs is picked up.
        width: std::sync::atomic::AtomicU16::new(terminal_width()),
        height: std::sync::atomic::AtomicU16::new(terminal_height()),
        inline: Mutex::new(rune_term::inline::Inline::new(terminal_width())),
        frame: Mutex::new(()),
        transcript: Mutex::new(transcript),
        transcript_history_len: std::sync::atomic::AtomicUsize::new(history.len()),
        transcript_open: std::sync::atomic::AtomicBool::new(false),
        typed: Mutex::new((String::new(), 0)),
        provider_order: config.settings.provider_order.clone(),
        provider_strict: config.settings.provider_strict,
        streaming: Arc::new(Mutex::new(StreamingText::default())),
        live_out: Arc::new(Mutex::new(None)),
        reasoning: Mutex::new(reasoning_escape),
        reviewer: crate::auto_review::build(&config.settings, &config.paths)?,
        totals: Mutex::new(Totals::default()),
        undo: Mutex::new(BTreeMap::new()),
        review_session: Arc::new(Mutex::new(ReviewSession::new(&limits))),
        approval_requests: Mutex::new(None),
        questions: config.questions,
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

    // Enable raw input before announcing the session. Otherwise input sent in
    // response to the banner can have its Enter translated by canonical mode
    // before the key reader starts, leaving a complete prompt unsubmitted.
    let mut reader = rune_term::input::KeyReader::new();
    let keyed = reader.is_active();

    // The session identifier is announced up front so a resumed-or-new session
    // can be named later without consulting the listing. It goes through the
    // renderer like everything else, so the rows it occupies are known to the
    // component that later redraws over them.
    {
        let banner = format!(
            "session {}",
            recorder
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .id()
        );
        let mut opening = vec![banner];
        // Replay only for an interactive resume. These settled rows enter the
        // terminal's scrollback once, before the composer accepts any input.
        if keyed && config.resume.is_some() {
            let entries = host
                .transcript
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            opening.extend(render_entries(&entries, &host));
        }
        let (prompt_row, caret) = host.idle_prompt();
        let painted = host.paint(&opening, None, &prompt_row, &[], caret)?;
        if let Ok(mut sink) = out.lock() {
            sink.write_all(&painted)?;
            sink.flush()?;
        }
    }

    // A resumed session keeps the title it was given.
    let mut is_first_prompt = config.resume.is_none()
        && recorder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .title_is_unset();

    // The Escape gesture spans presses, so it outlives a single read.
    let mut cancellation_gesture = rune_term::shell::EscapeGesture::default();

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
    let mut completion_context = ExecutionContext::new(config.workspace.clone());
    for root in &settings.additional_directories {
        completion_context = completion_context.with_root(root.clone());
    }
    let completion_limits = rune_tools::workspace::FileLimits::from_budget(&limits);

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
                            history_file: &mut Option<crate::prompt_history::History>,
                            reader: &mut rune_term::input::KeyReader,
                            gesture: &mut rune_term::shell::EscapeGesture|
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
                        *recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh;
                        history = History::new();
                        host.transcript_history_len
                            .store(0, std::sync::atomic::Ordering::Relaxed);
                        host.transcript
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clear();
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
                        // The input loop and stream observer share one recorder;
                        // its lock serializes every write to the session log.
                        recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .set_title(&session_log::derive_title(&title))?;
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
                    recorder
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .set_title(&session_log::derive_title(&text))?;
                    is_first_prompt = false;
                }
                // Recorded before the turn runs, so a prompt that is interrupted
                // is still recallable.
                if let Some(history) = history_file.as_mut() {
                    let entry = crate::prompt_history::Entry::new(text.clone()).located(
                        &config.workspace,
                        recorder
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .id()
                            .as_str(),
                    );
                    let _ = history.record(entry);
                }
                recorder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .user_message(&text)?;
                history.push_user(text.clone());
                host.transcript
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(Entry::user(text));
                // The turn runs on its own thread so this one keeps reading
                // keys. A turn that ran inline made the keyboard dead for as
                // long as the model took, which is the difference between
                // correcting a long turn and waiting it out.
                host.transcript_history_len
                    .store(history.len(), std::sync::atomic::Ordering::Relaxed);
                recorder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .begin_turn()?;
                let steered = run_turn_steerable(&mut history, &host, reader, gesture);
                recorder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .check_journal()?;
                // A correction the turn took in is part of the conversation the
                // model saw, so a resumed session must see it too.
                for correction in &steered.applied {
                    recorder
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .user_message(correction)?;
                }
                let outcome = match steered.outcome {
                    Ok(outcome) => outcome,
                    // A cancelled or failed turn ends that exchange, not the
                    // session. Cancelling is something the keyboard offers while
                    // a turn runs, so ending the session for it would make the
                    // gesture indistinguishable from quitting. A pipe keeps the
                    // failure, because a script reads it from the exit status.
                    Err(err) => {
                        let partial = host.partial_answer();
                        // Save before clearing the live answer or reporting
                        // the boundary, just as for a completed exchange.
                        if err.code() == ErrorCode::Cancelled {
                            recorder
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .cancelled_turn(&partial)?;
                        } else {
                            recorder
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .failed_turn(&partial, &err)?;
                        }
                        let history_start = host
                            .transcript_history_len
                            .load(std::sync::atomic::Ordering::Relaxed)
                            .min(history.len());
                        retain_partial_answer(&mut history, history_start, &partial);
                        host.capture_history(&history);
                        {
                            let mut transcript = host
                                .transcript
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if let Some(diagnostic) = &steered.diagnostic {
                                transcript.push(Entry::notice(diagnostic));
                            }
                            transcript.push(Entry::notice(err.message()));
                        }
                        if !reader.is_active() {
                            return Err(err);
                        }
                        let lines = report_failed_turn(&err, &host, steered.diagnostic.as_deref());
                        host.clear_events();
                        host.clear_streaming();
                        close_turn(&host, sink, reader, &lines, None, &steered.unsent)?;
                        return Ok(Step::Continue);
                    }
                };
                // The turn is recorded before it is reported, so a session that
                // dies while rendering still has its exchange on disk.
                recorder
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .turn(&outcome)?;
                host.capture_history(&history);
                // The model is read back rather than captured at start, so a
                // turn that ran after `/model` is billed to the model that ran.
                record_usage(&config.paths, &host.model(), &outcome);
                host.record_usage(&outcome.usage);
                host.record_context_size(
                    outcome
                        .last_request
                        .input_tokens
                        .unwrap_or(0)
                        .saturating_add(outcome.last_request.output_tokens.unwrap_or(0)),
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
                close_turn(
                    &host,
                    sink,
                    reader,
                    &lines,
                    activity.as_deref(),
                    &steered.unsent,
                )?;
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
        while let Some(input) = await_submission(
            &mut reader,
            &host,
            &out,
            &recall,
            &completion_context,
            &completion_limits,
        )? {
            let mut sink = LockedSink {
                stream: Arc::clone(&out),
            };
            match handle_input(
                input,
                &mut sink,
                &mut history_file,
                &mut reader,
                &mut cancellation_gesture,
            )? {
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
            match handle_input(input, &mut sink, &mut history_file, &mut reader, &mut cancellation_gesture)? {
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

/// Runs one turn while keeping the keyboard live.
///
/// The turn runs on its own thread because a turn that ran here would block
/// every keystroke until the model finished. What the user types meanwhile is
/// submitted to the steering queue, which the turn drains at the boundaries it
/// already computes; Escape and Control-C reach the cancellation flag the turn
/// already checks.
///
/// Returns the turn's outcome. Cancellation is reported by the turn itself
/// rather than by abandoning it, so the conversation keeps whatever the turn
/// had already recorded.
fn run_turn_steerable(
    history: &mut History,
    host: &SessionHost,
    reader: &mut rune_term::input::KeyReader,
    gesture: &mut rune_term::shell::EscapeGesture,
) -> SteeredTurn {
    if !reader.is_active() {
        // Without a terminal there are no keys to read, so the turn runs here
        // and the steering path is simply unused.
        return SteeredTurn {
            outcome: turn::run_turn(history, host),
            diagnostic: None,
            applied: Vec::new(),
            unsent: Vec::new(),
        };
    }

    // Cleared before the turn rather than after, so a cancel that arrived as
    // the previous turn ended cannot stop this one before it starts.
    host.cancellation.reset();
    // The host's copy of the typed line is what every streamed frame draws, so
    // it starts from the reader's line rather than whatever the last turn left.
    if let Ok(mut typed) = host.typed.lock() {
        *typed = (reader.line().to_owned(), reader.column());
    }
    let mut submitted: Vec<String> = Vec::new();

    let (requests, pending) = mpsc::channel();
    if let Ok(mut slot) = host.approval_requests.lock() {
        *slot = Some(requests);
    }
    let (questions, pending_questions) = mpsc::channel();
    if let Ok(mut slot) = host.questions.requests.lock() {
        *slot = Some(questions);
    }

    let (result, diagnostic) = run_on_worker(
        history,
        |taken| turn::run_turn(taken, host),
        || {
            if let Ok(request) = pending.try_recv() {
                gesture.disarm();
                let answer = collect_approval(&request, host, reader);
                let _ = request.answer.send(answer);
                host.draw_stream_with(reader.line(), reader.column());
                return;
            }
            if let Ok(request) = pending_questions.try_recv() {
                gesture.disarm();
                let answer = collect_questions(&request.questions, host, reader);
                let _ = request.answer.send(answer);
                host.draw_stream_with(reader.line(), reader.column());
                return;
            }
            let Some(key) = reader.poll_key(rune_term::shell::POLL_INTERVAL) else {
                return;
            };
            match key {
                KeyAction::ExternalEditor => {
                    gesture.disarm();
                    edit_draft(reader, host);
                    host.draw_stream_with(reader.line(), reader.column());
                }
                KeyAction::Transcript => {
                    gesture.disarm();
                    let _ = view_transcript(reader, host);
                    host.draw_stream_with(reader.line(), reader.column());
                }
                // Enter submits what has been typed as steering rather than as
                // a new turn, so a correction reaches the running turn instead
                // of queueing behind it.
                KeyAction::Submit => {
                    let text = reader.line().trim().to_owned();
                    reader.clear();
                    gesture.disarm();
                    if text.is_empty() {
                        host.draw_stream_with("", 0);
                        return;
                    }
                    let notice = match host.steering.submit(text.clone()) {
                        Ok(()) => {
                            submitted.push(text);
                            String::from("steering queued for the next boundary")
                        }
                        Err(err) => err.message().to_owned(),
                    };
                    // Drawn with the emptied line, so the submitted text does
                    // not stay on screen as though it were still being typed.
                    host.draw_notice_with(&notice, "", 0);
                }
                // Escape arms on the first press and cancels on the second.
                // The first press also clears a partly typed correction, which
                // is what the key does everywhere else.
                KeyAction::Escape => {
                    if gesture.record() {
                        host.cancellation.cancel();
                        host.draw_notice("cancelling the turn");
                    } else if reader.line().is_empty() {
                        host.draw_notice("press Escape again to cancel");
                    } else {
                        reader.clear();
                        host.draw_stream_with("", 0);
                    }
                }
                // Control-C clears a typed correction first, then cancels. The
                // session is left from the prompt the cancelled turn returns
                // to, so a press here never abandons a turn still writing.
                KeyAction::Cancel => {
                    gesture.disarm();
                    if reader.line().is_empty() {
                        host.cancellation.cancel();
                        host.draw_notice("cancelling the turn");
                    } else {
                        reader.clear();
                        host.draw_stream_with("", 0);
                    }
                }
                // Anything else is an edit, so a half-finished cancel gesture
                // is abandoned rather than left armed, and the line is redrawn
                // with the text that has been typed so far.
                _ => {
                    gesture.disarm();
                    // The reader's line is passed rather than read back from
                    // the host, so the keystroke that just landed is drawn and
                    // not the state before it.
                    host.draw_stream_with(reader.line(), reader.column());
                }
            }
        },
    );
    if let Ok(mut slot) = host.approval_requests.lock() {
        *slot = None;
    }
    if let Ok(mut slot) = host.questions.requests.lock() {
        *slot = None;
    }
    // Anything still queued was typed after the turn's last boundary, so the
    // turn never saw it. The queue is drained in one piece, which makes what is
    // left the newest submissions and everything before them what was applied.
    let unsent: Vec<String> = host
        .steering
        .drain(rune_agent::Boundary::Finalizing)
        .into_iter()
        .map(|message| message.text)
        .collect();
    let applied_count = submitted.len().saturating_sub(unsent.len());
    submitted.truncate(applied_count);
    SteeredTurn {
        outcome: result,
        diagnostic,
        applied: submitted,
        unsent,
    }
}

/// Shows the complete scope in scrollback, then waits on a two-choice picker.
/// Deny is selected initially. Escape, Control-C, and Control-D cancel the turn.
fn collect_approval(
    request: &ApprovalRequest,
    host: &SessionHost,
    reader: &mut rune_term::input::KeyReader,
) -> Outcome {
    let mut picker = rune_term::picker::Picker::new(
        "permission required",
        vec![String::from("Run once"), String::from("Deny")],
        2,
    );
    picker.to(1);
    host.refresh_size();
    let lines = approval_lines(request, usize::from(host.width()));
    if draw_choice(host, &picker, &lines).is_err() {
        return Outcome::Deny;
    }
    loop {
        if host.cancellation.is_cancelled() {
            return Outcome::Deny;
        }
        if let Some(key) = reader.poll_choice(rune_term::shell::POLL_INTERVAL)
            && let Some(answer) = approval_key(key, &mut picker, &host.cancellation)
        {
            return answer;
        }
        // Also refreshes dimensions when no input arrives.
        if draw_choice(host, &picker, &[]).is_err() {
            return Outcome::Deny;
        }
    }
}

/// Collects every answer without editing or submitting the current draft.
fn collect_questions(
    questions: &[Question],
    host: &SessionHost,
    reader: &mut rune_term::input::KeyReader,
) -> Result<Answer> {
    let mut answers = Vec::with_capacity(questions.len());
    for (index, question) in questions.iter().enumerate() {
        host.cancellation.check()?;
        let mut picker = rune_term::picker::Picker::new(
            format!(
                "answer required ({}/{})",
                index.saturating_add(1),
                questions.len()
            ),
            question
                .options
                .iter()
                .map(|option| transcript::sanitize(&option.label))
                .collect(),
            question.options.len(),
        );
        host.refresh_size();
        let lines = question_lines(question, usize::from(host.width()));
        draw_choice(host, &picker, &lines)?;
        loop {
            host.cancellation.check()?;
            if let Some(key) = reader.poll_choice(rune_term::shell::POLL_INTERVAL) {
                let chosen = question_key(key, &mut picker, &host.cancellation);
                host.cancellation.check()?;
                if let Some(chosen) = chosen {
                    answers.push(chosen);
                    break;
                }
            }
            draw_choice(host, &picker, &[])?;
        }
    }
    Ok(Answer::Chosen(answers))
}

/// Keeps the full question and option descriptions available in scrollback.
fn question_lines(question: &Question, width: usize) -> Vec<String> {
    std::iter::once(question.text.clone())
        .chain(question.options.iter().map(|option| {
            option.description.as_ref().map_or_else(
                || option.label.clone(),
                |description| format!("{}: {description}", option.label),
            )
        }))
        .flat_map(|line| rune_term::width::wrap(&transcript::sanitize(&line), width.max(1)))
        .collect()
}

fn question_key(
    key: KeyAction,
    picker: &mut rune_term::picker::Picker,
    cancellation: &Cancellation,
) -> Option<usize> {
    match key {
        KeyAction::Submit => picker.selected().map(|_| picker.cursor()),
        KeyAction::Up => {
            picker.up();
            None
        }
        KeyAction::Down => {
            picker.down();
            None
        }
        KeyAction::Escape | KeyAction::Cancel | KeyAction::Interrupt => {
            cancellation.cancel();
            None
        }
        KeyAction::Complete
        | KeyAction::Ignored
        | KeyAction::Transcript
        | KeyAction::ExternalEditor => None,
    }
}

/// Quoting makes control characters visible without changing the approved scope.
fn approval_lines(request: &ApprovalRequest, width: usize) -> Vec<String> {
    [
        format!("Permission request for {:?}", request.tool),
        format!("Reason: {:?}", request.reason),
        format!("Scope: {:?}", request.target),
    ]
    .into_iter()
    .flat_map(|line| rune_term::width::wrap(&line, width.max(1)))
    .collect()
}

fn approval_key(
    key: KeyAction,
    picker: &mut rune_term::picker::Picker,
    cancellation: &Cancellation,
) -> Option<Outcome> {
    match key {
        KeyAction::Submit => Some(if picker.selected() == Some("Run once") {
            Outcome::Allow
        } else {
            Outcome::Deny
        }),
        KeyAction::Up => {
            picker.up();
            None
        }
        KeyAction::Down => {
            picker.down();
            None
        }
        KeyAction::Escape | KeyAction::Cancel | KeyAction::Interrupt => {
            cancellation.cancel();
            Some(Outcome::Deny)
        }
        KeyAction::Complete
        | KeyAction::Ignored
        | KeyAction::Transcript
        | KeyAction::ExternalEditor => None,
    }
}

/// Uses the session renderer and reports if the choice cannot be displayed.
fn draw_choice(
    host: &SessionHost,
    picker: &rune_term::picker::Picker,
    settled: &[String],
) -> Result<()> {
    let _frame = host
        .frame
        .lock()
        .map_err(|_| RuneError::new(ErrorCode::Internal, "the frame lock was poisoned"))?;
    let out = host
        .live_out
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .ok_or_else(|| RuneError::new(ErrorCode::InputRequired, "choice output is unavailable"))?;
    let hint = vec![String::from(
        "Up/Down choose, Enter confirm, Esc/Ctrl-C cancel",
    )];
    let menu = picker.rows(&host.theme, host.truecolor);
    let painted = host.paint_with_menu(settled, Some(picker.title()), &hint, &[], &menu, (0, 0))?;
    let mut sink = LockedSink { stream: out };
    sink.write_all(&painted)?;
    sink.flush()?;
    Ok(())
}

/// Runs a turn on its own thread, calling `between` until it finishes.
///
/// The conversation is moved in and handed back: a turn pushes what it learned
/// into the history, and the next turn must see it. Moving it is what lets the
/// worker own it outright rather than sharing it behind a lock the loop would
/// then have to hold.
///
/// A worker that panics hands back the conversation as it stood before the
/// turn. The panic ends that exchange; losing every earlier one with it would
/// turn one defect into the loss of the session. What the worker held is not
/// used, because a turn stopped partway can hold a tool call with no result,
/// and every later request would then be refused.
/// The panic is an internal failure, with its diagnostic returned separately
/// for the closing frame.
fn run_on_worker<T, B>(
    history: &mut History,
    turn: T,
    mut between: B,
) -> (Result<turn::TurnOutcome>, Option<String>)
where
    T: FnOnce(&mut History) -> Result<turn::TurnOutcome> + Send,
    B: FnMut(),
{
    let before = history.clone();
    let mut taken = std::mem::take(history);
    let (returned, result, diagnostic) = std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            rune_term::input::catch_worker_panic(|| {
                let result = turn(&mut taken);
                (taken, result)
            })
        });
        while !worker.is_finished() {
            between();
        }
        match worker.join().and_then(std::convert::identity) {
            Ok((returned, result)) => (returned, result, None),
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("non-text panic payload");
                (
                    before,
                    Err(RuneError::new(
                        ErrorCode::Internal,
                        "internal worker failure",
                    )),
                    Some(format!("worker panicked: {message}")),
                )
            }
        }
    });
    *history = returned;
    (result, diagnostic)
}

/// What a steerable turn produced beside its outcome.
struct SteeredTurn {
    /// The turn's own result.
    outcome: Result<turn::TurnOutcome>,
    /// A caught worker panic, settled once through the renderer.
    diagnostic: Option<String>,
    /// Corrections the turn took in, in the order they were typed.
    applied: Vec<String>,
    /// Corrections typed after the turn's last boundary, which it never saw.
    unsent: Vec<String>,
}

/// Draws the frame that closes a turn: its settled lines, then the input row.
///
/// The input row shows whatever the reader holds, because a user who typed
/// during the turn is still typing. A correction the turn ended before taking
/// is put back on the line rather than dropped, where one more Enter sends it.
fn close_turn(
    host: &SessionHost,
    sink: &mut LockedSink,
    reader: &mut rune_term::input::KeyReader,
    lines: &[String],
    activity: Option<&str>,
    unsent: &[String],
) -> Result<()> {
    let mut notice = activity.map(str::to_owned);
    if !unsent.is_empty() {
        let mut restored = unsent.join(" ");
        if !reader.line().trim().is_empty() {
            restored.push(' ');
            restored.push_str(reader.line());
        }
        reader.replace(&restored);
        notice = Some(String::from(
            "the turn ended before this was sent; press Enter to send it",
        ));
    }
    let marker = rune_term::shell::prompt();
    let (prompt_rows, caret) = transcript::render_draft_at(
        marker,
        reader.line(),
        reader.column(),
        usize::from(host.width()),
    );
    if let Ok(mut typed) = host.typed.lock() {
        *typed = (reader.line().to_owned(), reader.column());
    }
    let painted = host.paint(lines, notice.as_deref(), &prompt_rows, &[], caret)?;
    if !painted.is_empty() {
        sink.write_all(&painted)?;
        sink.flush()?;
    }
    Ok(())
}

/// Restores the main terminal even when rendering fails or unwinds.
struct TranscriptScreen<'a>(&'a SessionHost);

/// Uses the same terminal ownership as the transcript to keep worker output
/// away from the editor and preserve the main screen underneath it.
fn edit_draft(reader: &mut rune_term::input::KeyReader, host: &SessionHost) {
    let screen = {
        let _frame = host
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        host.transcript_open
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let screen = TranscriptScreen(host);
        host.show(b"\x1b[?1049h\x1b[0m\x1b[2J\x1b[H\x1b[?25h");
        screen
    };
    let result = reader.edit_external();
    if let Ok(mut typed) = host.typed.lock() {
        *typed = (reader.line().to_owned(), reader.column());
    }
    drop(screen);
    if let Err(error) = result {
        let _frame = host
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (prompt, caret) = transcript::render_draft_at(
            rune_term::shell::prompt(),
            reader.line(),
            reader.column(),
            usize::from(host.width()),
        );
        let message = format!("external editor failed: {error}");
        if let Ok(painted) =
            host.paint(&[transcript::sanitize(&message)], None, &prompt, &[], caret)
        {
            host.show(&painted);
        }
    }
}

impl Drop for TranscriptScreen<'_> {
    fn drop(&mut self) {
        let _frame = self
            .0
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.0.show(b"\x1b[0m\x1b[?1049l\x1b[?25h");
        self.0
            .transcript_open
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Reads a stable snapshot on the alternate screen. The reader's composer is
/// never replaced, so its caret, recall state and kill buffer survive intact.
fn view_transcript(reader: &rune_term::input::KeyReader, host: &SessionHost) -> Result<()> {
    use rune_term::input::TranscriptAction;

    let mut entries = host
        .transcript
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    entries.extend(event_entries(host));
    let partial = host.partial_answer();
    if !partial.is_empty() {
        entries.push(Entry::assistant(partial));
    }
    host.refresh_size();
    let mut size = (
        host.width(),
        host.height.load(std::sync::atomic::Ordering::Relaxed),
    );
    let mut view = rune_term::screen::Transcript::new(full_transcript_rows(&entries, size.0));
    let mut surface = rune_term::frame::FrameSurface::new(size.0, size.1)?;
    let _screen = {
        let _frame = host
            .frame
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        host.transcript_open
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let screen = TranscriptScreen(host);
        host.show(b"\x1b[?1049h\x1b[0m\x1b[2J\x1b[H\x1b[?25l");
        screen
    };

    let mut redraw = true;
    loop {
        host.refresh_size();
        let next_size = (
            host.width(),
            host.height.load(std::sync::atomic::Ordering::Relaxed),
        );
        if next_size != size {
            let top = view.top();
            size = next_size;
            view = rune_term::screen::Transcript::new(full_transcript_rows(&entries, size.0));
            view.jump(top, usize::from(size.1.saturating_sub(1)));
            // Resizing can leave stale cells anywhere on the alternate screen.
            host.show(b"\x1b[2J");
            redraw = true;
        }
        let height = usize::from(size.1.saturating_sub(1));
        if redraw {
            let mut lines = view.visible(height).to_vec();
            lines.resize(height, String::new());
            let footer = vec![String::from(
                "Transcript | Up/Down PgUp/PgDn Home/End | Esc/Ctrl-O close",
            )];
            let target = rune_term::frame::compose(
                &rune_term::frame::Regions::new(&lines, &footer),
                size.0,
                size.1,
            )?;
            let painted = surface.commit(&target)?;
            host.show(&painted.bytes);
        }
        let action = reader.poll_transcript(std::time::Duration::from_millis(50));
        redraw = !matches!(action, Some(TranscriptAction::Ignored) | None);
        match action {
            Some(TranscriptAction::Close) => break,
            Some(TranscriptAction::Up) => view.scroll_up(1),
            Some(TranscriptAction::Down) => view.scroll_down(1, height),
            Some(TranscriptAction::PageUp) => view.page_up(height),
            Some(TranscriptAction::PageDown) => view.page_down(height),
            Some(TranscriptAction::Home) => view.jump(0, height),
            Some(TranscriptAction::End) => view.jump(usize::MAX, height),
            Some(TranscriptAction::Ignored) | None => {}
        }
    }
    Ok(())
}

/// Adds process interruption boundaries to the saved transcript without putting
/// those display notices into the conversation sent to the provider.
fn resumed_entries(history: &History, state: &rune_session::store::SessionState) -> Vec<Entry> {
    use rune_session::event::SessionEvent;
    use rune_term::transcript::Speaker;

    let mut boundaries = std::collections::BTreeSet::new();
    let mut messages = 0_usize;
    for frame in rune_session::replay::replay_events(&state.events) {
        match &frame.event {
            SessionEvent::UserMessage { .. } => messages = messages.saturating_add(1),
            SessionEvent::AssistantMessage { text, .. } if !text.is_empty() => {
                messages = messages.saturating_add(1);
            }
            SessionEvent::TurnInterrupted { .. } => {
                boundaries.insert(messages);
            }
            _ => {}
        }
    }
    messages = 0;
    let mut entries = Vec::new();
    for entry in history_entries(history.turns()) {
        if matches!(entry.speaker, Speaker::User | Speaker::Assistant) {
            messages = messages.saturating_add(1);
        }
        entries.push(entry);
        if boundaries.remove(&messages) {
            entries.push(Entry::notice("interrupted"));
        }
    }
    entries
}

/// Projects recorded messages in order, keeping tool arguments and result bodies.
fn history_entries(turns: &[rune_agent::history::Turn]) -> Vec<Entry> {
    use rune_net::message::{ContentPart, Role};
    turns
        .iter()
        .flat_map(|turn| {
            turn.parts.iter().map(|part| match part {
                ContentPart::Text { text } => match turn.role {
                    Role::User => Entry::user(text.clone()),
                    Role::Assistant => Entry::assistant(text.clone()),
                    Role::Tool => Entry::tool(text.clone()),
                    Role::System => Entry::notice(text.clone()),
                },
                ContentPart::Reasoning { text } => Entry::reasoning(text.clone()),
                ContentPart::ToolCall {
                    name, arguments, ..
                } => Entry::tool(format!("{name} {arguments}")),
                ContentPart::ToolResult {
                    name,
                    content,
                    is_error,
                    ..
                } => Entry::tool(format!(
                    "{name}{}:\n{content}",
                    if *is_error { " (error)" } else { "" }
                )),
                ContentPart::Image { image } => {
                    Entry::notice(format!("image {} ({})", image.id, image.media_type))
                }
            })
        })
        .collect()
}

/// Expands every recorded tool line using the same wrapping as the inline view.
fn full_transcript_rows(entries: &[Entry], cols: u16) -> Vec<String> {
    transcript::render(
        entries,
        Display {
            width: usize::from(cols),
            tool_lines: usize::MAX,
            ..Display::default()
        },
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

/// Waits for a line to be submitted, drawing the prompt as it is typed.
///
/// Returns the submitted input, or `None` when the user asked to leave. The
/// cursor is placed at the editor's caret within the visible draft.
///
/// `recall` supplies earlier prompts for the up and down arrows. Passing an
/// empty slice leaves those keys doing nothing, which is what a session with no
/// recorded history wants.
fn await_submission(
    reader: &mut rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
    recall: &[String],
    context: &ExecutionContext,
    limits: &rune_tools::workspace::FileLimits,
) -> Result<Option<Input>> {
    let marker = rune_term::shell::prompt();
    // Which completion row is highlighted. While the dropdown is open the
    // arrows move it rather than walking the prompt history, because the list is
    // what the user is looking at.
    let mut selected = 0_usize;
    let mut paths: Option<crate::path_completion::Paths> = None;

    loop {
        let rows = paths.as_ref().map_or_else(
            || {
                completion_rows(
                    reader.line(),
                    selected,
                    host.theme(),
                    host.truecolor(),
                    host.menu_room(),
                )
            },
            |paths| {
                path_completion_rows(
                    paths,
                    selected,
                    host.theme(),
                    host.truecolor(),
                    host.menu_room(),
                )
            },
        );
        draw_prompt(reader, host, out, &rows, marker)?;

        let key = reader.read_key();
        if key == KeyAction::Transcript {
            view_transcript(reader, host)?;
            continue;
        }
        if key == KeyAction::ExternalEditor {
            edit_draft(reader, host);
            selected = 0;
            paths = None;
            continue;
        }
        if path_key(key, reader, &mut paths, &mut selected, context, limits) {
            continue;
        }
        match idle_key(key, reader, &mut selected, &rows, recall) {
            Idle::Stay => {}
            Idle::Leave => return Ok(None),
            Idle::Submit(input) => return Ok(Some(input)),
        }
    }
}

/// A filesystem menu is opened only by Tab and discarded when the draft changes.
fn path_key(
    key: KeyAction,
    reader: &mut rune_term::input::KeyReader,
    paths: &mut Option<crate::path_completion::Paths>,
    selected: &mut usize,
    context: &ExecutionContext,
    limits: &rune_tools::workspace::FileLimits,
) -> bool {
    if let Some(open) = paths {
        match key {
            KeyAction::Submit | KeyAction::Complete => {
                if let Some(chosen) = open.matches.get(*selected) {
                    reader.complete_path(open.range.clone(), chosen);
                }
                *paths = None;
                *selected = 0;
                return true;
            }
            KeyAction::Up | KeyAction::Down => {
                *selected = if key == KeyAction::Up {
                    selected.saturating_sub(1)
                } else {
                    selected
                        .saturating_add(1)
                        .min(open.matches.len().saturating_sub(1))
                };
                return true;
            }
            KeyAction::Escape => {
                *paths = None;
                *selected = 0;
                return true;
            }
            _ => *paths = None,
        }
    }
    if key != KeyAction::Complete
        || completion_matches(reader.line()) > 0
        || matches!(Input::parse(reader.line()), Input::Command { .. })
    {
        return false;
    }
    let choices = crate::path_completion::Paths::collect(
        reader.line(),
        reader.cursor_byte(),
        context,
        limits,
    );
    *selected = 0;
    if choices.matches.len() == 1 {
        if let Some(chosen) = choices.matches.first() {
            reader.complete_path(choices.range, chosen);
        }
    } else if !choices.matches.is_empty() {
        *paths = Some(choices);
    }
    true
}

fn path_completion_rows(
    paths: &crate::path_completion::Paths,
    selected: usize,
    theme: &Theme,
    truecolor: bool,
    room: usize,
) -> Vec<String> {
    let count = paths.matches.len();
    let window = completion_window(count, selected, menu_window(count, COMPLETION_WINDOW, room));
    let accent = theme.sgr(Slot::Accent, truecolor);
    let dim = theme.sgr(Slot::Dim, truecolor);
    let mut rows: Vec<_> = window
        .clone()
        .filter_map(|index| {
            paths.matches.get(index).map(|path| {
                if index == selected {
                    styled(&accent, &format!("> {path}"))
                } else {
                    styled(&dim, &format!("  {path}"))
                }
            })
        })
        .collect();
    if count > window.len() && rows.len() < room {
        rows.push(styled(
            &dim,
            &format!(
                "  {}-{} of {count}",
                window.start.saturating_add(1),
                window.end
            ),
        ));
    }
    rows
}

/// What a key pressed at the idle prompt leads to.
enum Idle {
    /// Keep reading keys.
    Stay,
    /// Leave the session.
    Leave,
    /// Hand this input to the session.
    Submit(Input),
}

/// Applies one key read at the idle prompt.
///
/// `selected` is the highlighted completion row, and `rows` are the rows drawn
/// for the completions, empty when none are open.
fn idle_key(
    key: KeyAction,
    reader: &mut rune_term::input::KeyReader,
    selected: &mut usize,
    rows: &[String],
    recall: &[String],
) -> Idle {
    match key {
        KeyAction::Submit => {
            // Enter takes the highlighted completion when the list is open
            // and the command is not yet complete, so a half-typed name is
            // never run. Once the name is complete, Enter runs it.
            if let Some(chosen) = open_completion(reader.line(), rows, *selected)
                && chosen.name != reader.line().trim_start_matches('/')
            {
                reader.replace(&format!("/{}", chosen.name));
                *selected = 0;
                return Idle::Stay;
            }
            let text = reader.line().trim().to_owned();
            reader.clear();
            if text.is_empty() {
                return Idle::Stay;
            }
            Idle::Submit(Input::parse(&text))
        }
        // An empty line is the only thing there is to leave behind, so
        // interrupting it means leaving the session.
        KeyAction::Interrupt => Idle::Leave,
        // Escape only clears the line. While a turn runs, a first Escape
        // asks for a second to cancel, and a turn that ends between the two
        // hands that second press to this prompt, where leaving would end the
        // session the user only meant to stop a turn in.
        KeyAction::Escape => {
            reader.clear();
            *selected = 0;
            Idle::Stay
        }
        KeyAction::Cancel => {
            // Control-C on a partly typed line abandons the line rather
            // than the session, matching what the key does elsewhere.
            if reader.line().is_empty() {
                return Idle::Leave;
            }
            reader.clear();
            *selected = 0;
            Idle::Stay
        }
        response @ (KeyAction::Up | KeyAction::Down) => {
            // The dropdown owns the arrows while it is open. The bound is
            // the number of matches, not the number of rows drawn: the rows
            // include a position line, so clamping on them would strand
            // every command past the first window.
            let matches = completion_matches(reader.line());
            if matches > 0 {
                *selected = if response == KeyAction::Up {
                    selected.saturating_sub(1)
                } else {
                    selected.saturating_add(1).min(matches.saturating_sub(1))
                };
            } else if response == KeyAction::Up {
                reader.recall_previous(recall);
            } else {
                reader.recall_next(recall);
            }
            Idle::Stay
        }
        // Tab completes the highlighted command, which is what every other
        // shell does and what a reader reaches for first.
        KeyAction::Complete => {
            if let Some(chosen) = open_completion(reader.line(), rows, *selected) {
                reader.replace(&format!("/{}", chosen.name));
                *selected = 0;
            }
            Idle::Stay
        }
        KeyAction::Transcript | KeyAction::ExternalEditor => Idle::Stay,
        KeyAction::Ignored => {
            // Typing narrows the list, so the highlight returns to the top
            // rather than pointing at a row that may no longer exist.
            *selected = 0;
            Idle::Stay
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
/// The visible draft scrolls with the caret so the terminal's cursor is where
/// the next character will go.
fn draw_prompt(
    reader: &rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
    menu: &[String],
    marker: &str,
) -> Result<()> {
    let mut sink = out
        .lock()
        .map_err(|_| RuneError::new(ErrorCode::Internal, "the output lock was poisoned"))?;
    let (rows, caret) = transcript::render_draft_at(
        marker,
        reader.line(),
        reader.column(),
        usize::from(host.width()),
    );
    let painted = host.paint_with_menu(&[], None, &rows, &[], menu, caret)?;
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
        let below = picker_menu(&mut picker, &host.theme, host.truecolor, host.menu_room());
        draw_prompt(reader, host, out, &below, marker)?;

        match reader.read_key() {
            // Tab and Enter both accept: the picker is the only thing on
            // screen, so there is no typed argument for Enter to mean.
            KeyAction::Submit | KeyAction::Complete => {
                break picker.selected().map(str::to_owned);
            }
            KeyAction::Interrupt | KeyAction::Cancel | KeyAction::Escape => break None,
            // Both branches fall through to the redraw at the top of the loop.
            response @ (KeyAction::Up | KeyAction::Down) => {
                if response == KeyAction::Up {
                    picker.up();
                } else {
                    picker.down();
                }
            }
            KeyAction::Transcript => {
                view_transcript(reader, host)?;
            }
            KeyAction::ExternalEditor => {}
            KeyAction::Ignored => {
                picker.set_query(reader.line());
            }
        }
    };

    reader.replace(&draft);
    Ok(chosen)
}

/// Fits the model menu to the remaining rows before rendering its choices.
fn picker_menu(
    picker: &mut rune_term::picker::Picker,
    theme: &Theme,
    truecolor: bool,
    max_rows: usize,
) -> Vec<String> {
    if max_rows == 0 {
        return Vec::new();
    }
    // Keep a choice visible even when the title and key hint cannot fit.
    let decorated = max_rows >= 4;
    let room = max_rows.saturating_sub(if decorated { 2 } else { 0 });
    picker.set_window(menu_window(
        picker.matches().len(),
        rune_term::picker::DEFAULT_WINDOW,
        room,
    ));
    let mut rows = picker.rows(theme, truecolor);
    rows.truncate(room);
    if decorated {
        rows.insert(0, picker.title().to_owned());
        rows.push(rune_term::picker::Picker::hint().to_owned());
    }
    rows
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

    match summarize_history(host, history, &plan, &rune_net::transport::agent(), &|| {
        false
    }) {
        Ok(removed) => report(
            host,
            sink,
            &format!(
                "compacted {removed} earlier turn(s); {} turn(s) remain",
                history.len()
            ),
        ),
        Err(err) => report(host, sink, err.message()),
    }
}

/// Estimates the actual dialect body, including instructions and tool schemas.
fn request_estimate(
    host: &SessionHost,
    plan: &rune_net::provider::RequestPlan,
) -> Result<rune_agent::tokens::Estimate> {
    let body = host.dialect.build_request(plan)?;
    let bytes = serde_json::to_vec(&body)
        .map_err(|err| RuneError::new(ErrorCode::Internal, err.to_string()))?
        .len() as u64;
    // Include dialect defaults such as Anthropic's max_tokens reserve.
    let output = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
        .iter()
        .find_map(|key| body.get(key).and_then(serde_json::Value::as_u64))
        .unwrap_or(0);
    let capacity = rune_agent::tokens::usable_input_tokens(
        host.context_limit
            .load(std::sync::atomic::Ordering::Relaxed),
        output,
    );
    Ok(rune_agent::tokens::Estimate::new(
        bytes,
        rune_agent::tokens::estimate_tokens(bytes),
        capacity,
    ))
}

/// Shares the summarizer between manual and automatic compaction.
/// History changes only after a usable summary has arrived.
fn summarize_history(
    host: &SessionHost,
    history: &mut History,
    plan: &rune_agent::compaction::Plan,
    client: &dyn rune_net::fetch::Fetch,
    cancelled: &dyn Fn() -> bool,
) -> Result<usize> {
    let request = rune_agent::compaction::render_summary_request(history, plan);
    let mut request_plan = rune_net::provider::RequestPlan::new(host.model());
    rune_agent::compaction::SUMMARY_INSTRUCTIONS.clone_into(&mut request_plan.instructions);
    request_plan.messages = rune_net::transport::one_shot_messages(&request);
    let outcome = rune_net::transport::stream_completion(
        client,
        &host.endpoint,
        host.dialect.as_ref(),
        &request_plan,
        rune_net::transport::RequestTimeouts {
            head: compaction_timeout(host),
            ..rune_net::transport::RequestTimeouts::from_limits(&host.limits)
        },
        cancelled,
    )
    .map_err(|err| {
        RuneError::new(
            err.kind().code(),
            format!("compaction failed, nothing was changed: {err}"),
        )
    })?;
    let summary = outcome.text();
    rune_agent::compaction::validate_summary(&summary).map_err(|err| {
        RuneError::new(
            err.code(),
            format!("compaction produced nothing usable: {err}"),
        )
    })?;
    host.capture_history(history);
    let removed = rune_agent::compaction::apply(
        history,
        plan,
        rune_agent::compaction::wrap_summary(&summary),
    );
    host.transcript_history_len
        .store(history.len(), std::sync::atomic::Ordering::Relaxed);
    host.record_usage(&outcome.usage);
    Ok(removed)
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

/// How many completions are shown at once.
///
/// A short window keeps the list from swallowing the screen: a bare slash
/// matches every command, and drawing all of them would push the transcript
/// away for a list a reader only ever takes the top few rows of.
pub const COMPLETION_WINDOW: usize = 6;

/// Returns the range of matches on screen for a selection.
///
/// The window scrolls only once the selection reaches its edge and then follows
/// the selection, so growing the selection moves the highlight down a fixed
/// screen until the window has to move, rather than the list jumping on every
/// key.
#[must_use]
pub fn completion_window(count: usize, selected: usize, window: usize) -> std::ops::Range<usize> {
    if window == 0 {
        return 0..0;
    }
    if count <= window {
        return 0..count;
    }
    // The last screenful is the floor, so the list can scroll to its end.
    let last_start = count.saturating_sub(window);
    let selected = selected.min(count.saturating_sub(1));
    // Start far enough back that the selection sits on the last row of the
    // window once it has moved past the first screenful.
    let start = selected
        .saturating_add(1)
        .saturating_sub(window)
        .min(last_start);
    start..start.saturating_add(window)
}

/// Leaves a row for the position indicator whenever a long list has room.
fn menu_window(count: usize, maximum: usize, room: usize) -> usize {
    let window = maximum.min(room);
    let position_row = usize::from(count > window && room > 1);
    window.min(room.saturating_sub(position_row))
}

/// Returns the rows showing what can be typed next.
///
/// Drawn while a slash command is being typed, from the same table the help
/// text and the dispatcher use, so a command that is offered is one that works.
/// The row under the cursor is marked and coloured; every row carries the line
/// that describes the command, which is what makes the list usable without
/// trying each name.
///
/// Only a window of the matches is drawn, and a row saying how far through the
/// list the window is sits under it, so a reader can tell a short list from the
/// top of a long one.
///
/// Returns an empty list when the line is not a command being typed, so a
/// caller can pass every keystroke without checking first.
pub fn completion_rows(
    line: &str,
    selected: usize,
    theme: &Theme,
    truecolor: bool,
    max_rows: usize,
) -> Vec<String> {
    if max_rows == 0 {
        return Vec::new();
    }
    let Some(word) = slash_word(line) else {
        return Vec::new();
    };
    let matches = rune_term::commands::matching(word);
    if matches.is_empty() {
        return Vec::new();
    }

    let accent = theme.sgr(Slot::Accent, truecolor);
    let dim = theme.sgr(Slot::Dim, truecolor);
    // Wide enough for every match rather than only the visible ones, so the
    // descriptions do not shift sideways as the window scrolls.
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

    let window = completion_window(
        matches.len(),
        selected,
        menu_window(matches.len(), COMPLETION_WINDOW, max_rows),
    );
    let mut rows: Vec<String> = Vec::with_capacity(window.len().saturating_add(1));
    for index in window.clone() {
        let Some(entry) = matches.get(index) else {
            continue;
        };
        let left = if entry.arguments.is_empty() {
            format!("/{}", entry.name)
        } else {
            format!("/{} {}", entry.name, entry.arguments)
        };
        let body = format!("{left:<width$}  {}", entry.summary);
        if index == selected {
            rows.push(styled(&accent, &format!("> {body}")));
        } else {
            rows.push(styled(&dim, &format!("  {body}")));
        }
    }

    // How far through the list this window is, so a reader can tell a list that
    // ends here from one that continues. Shown only when there is more to see.
    if matches.len() > window.len() && rows.len() < max_rows {
        let first = window.start.saturating_add(1);
        let last = window.end;
        rows.push(styled(
            &dim,
            &format!("  {first}-{last} of {}", matches.len()),
        ));
    }
    rows
}

/// Returns how many completions the line offers.
///
/// Counted from the table rather than from the rendered rows, because the rows
/// carry a position line that is not a command.
#[must_use]
fn completion_matches(line: &str) -> usize {
    slash_word(line).map_or(0, |word| rune_term::commands::matching(word).len())
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
            let mut lines = vec![format!("could not list models: {}", err.message())];
            if let Some(hint) = err.hint() {
                lines.push(format!("hint: {hint}"));
            }
            flush_lines(host, &mut sink, &lines)?;
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
    let saved = crate::provider_setup::save_selection(paths, &selection);
    let mut sink = LockedSink {
        stream: Arc::clone(out),
    };
    report_model_choice(host, &mut sink, &chosen, saved.err().as_ref())
}

/// Reports a model choice through the renderer.
///
/// The picker runs with the terminal in raw mode, where a line written straight
/// to the stream lands at the caret inside the live region: the next frame
/// paints over it, and the status block it pushed down is left behind as a
/// second copy.
fn report_model_choice(
    host: &SessionHost,
    sink: &mut LockedSink,
    chosen: &str,
    unsaved: Option<&RuneError>,
) -> Result<()> {
    let line = match unsaved {
        None => format!("model set to {chosen}"),
        Some(err) => format!(
            "model set to {chosen} for this session; it could not be saved: {}",
            err.message()
        ),
    };
    flush_lines(host, sink, &[line])
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
    if let Some(source) = info.context_source {
        let _ = writeln!(out, "context source {source} (estimate)");
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
/// Sizes the skill catalog budget from the model's context window.
///
/// A fixed budget is wrong at both ends: too small for a model with a large
/// window, wasting budget it has, and too large for a small one, crowding out
/// the conversation. A caller that named its own value keeps it, because a
/// limit someone set deliberately is not this program's to overwrite.
fn adopt_skill_catalog_budget(limits: &mut BudgetSet, context_window: Option<u64>) {
    use rune_core::budget::LimitName;
    use rune_core::config::Layer;

    if limits.source(LimitName::SkillCatalogBytes).is_some() {
        return;
    }
    let derived = rune_core::budget::skill_catalog_bytes(context_window);
    // A failure here would mean the derived value is outside the limit's own
    // range, which the derivation already clamps for, so the compiled default
    // stands rather than the session refusing to start. The value is reported
    // as a default, because nobody set it: naming the user file would send a
    // reader to look for a line that is not there.
    let _ = limits.set(
        LimitName::SkillCatalogBytes,
        rune_core::budget::Budget::Bounded(derived),
        Layer::Default,
    );
}

/// Returns the context window a session budgets against.
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
    /// Source of the initial resume estimate, absent for live readings.
    context_source: Option<&'static str>,
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
    let mut entries = event_entries(host);

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

    Ok(render_entries(&entries, host))
}

/// Renders a turn that ended in an error rather than an outcome.
///
/// What streamed before the failure is kept, because the reader watched it
/// arrive and a transcript that dropped it would disagree with the screen.
/// Interrupted answers are also saved and retained in the conversation.
fn report_failed_turn(
    err: &RuneError,
    host: &SessionHost,
    diagnostic: Option<&str>,
) -> Vec<String> {
    let mut entries = event_entries(host);
    let partial = host.partial_answer();
    if !partial.trim().is_empty() {
        entries.push(Entry::assistant(partial));
    }
    if let Some(diagnostic) = diagnostic {
        entries.push(Entry::notice(diagnostic));
    }
    if err.code() == ErrorCode::Cancelled {
        entries.push(Entry::notice("cancelled"));
    } else {
        entries.push(Entry::notice(format!("the turn failed: {}", err.message())));
        if let Some(hint) = err.hint() {
            entries.push(Entry::notice(hint.to_owned()));
        }
    }
    render_entries(&entries, host)
}

/// Keeps only the answer bytes the turn has not already added to history.
///
/// An interruption can follow completed model steps or a tool call, whose
/// assistant text is already in history. The live answer may include that
/// prefix, so appending it all would repeat those steps on the next request.
fn retain_partial_answer(history: &mut History, start: usize, partial: &str) {
    let recorded: String = history.turns()[start..]
        .iter()
        .filter(|turn| turn.role == rune_net::message::Role::Assistant)
        .map(|turn| transcript::sanitize(&turn.text()))
        .collect();
    let unrecorded = partial.strip_prefix(&recorded).unwrap_or(partial);
    if !unrecorded.trim().is_empty() {
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: unrecorded.to_owned(),
        }]);
    }
}

/// Returns the transcript entries for the events the running turn reported.
fn event_entries(host: &SessionHost) -> Vec<Entry> {
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
    entries
}

/// Renders transcript entries into terminal lines.
fn render_entries(entries: &[Entry], host: &SessionHost) -> Vec<String> {
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
    let rendered = transcript::render_lanes(entries, display, &lanes);
    rendered.lines().map(str::to_owned).collect()
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
    let base_url = crate::provider_setup::require_base_url(settings)?;
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

    let questions = Arc::new(TerminalQuestions::default());
    let mut registry = inventory::builtin_with_answerer(
        &rune_tools::workspace::FileLimits::from_budget(&settings.limits),
        &settings.limits,
        &paths.managed_skills_dir(),
        crate::web_client::backends(settings),
        questions.clone(),
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
            settings.offline,
        ),
        dialect,
        registry,
        // The rules that ship with the harness, so a fresh install has a usable
        // starting point: reads and in-workspace edits proceed, outbound traffic
        // is refused, and an unknown command resolves to the mode's default
        // rather than to nothing.
        rules: crate::permissions::validated(settings)?,
        questions,
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
    fn a_journal_write_failure_cancels_before_showing_unsaved_text() {
        let root = tempfile::tempdir().expect("temp");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let paths = Paths {
            config_root: base.join("config"),
            state_root: base.join("state"),
            data_root: base.join("data"),
        };
        let id = SessionId::generate();
        let mut recorder = Recorder::create(&paths, &id).expect("created");
        recorder.user_message("slow").expect("prompt");
        recorder.begin_turn().expect("started");
        let metadata = paths.session_dir(&id).join("session.json");
        std::fs::remove_file(&metadata).expect("removed metadata");
        std::fs::create_dir(&metadata).expect("prevent metadata writes");
        let recorder = Arc::new(Mutex::new(recorder));
        let mut host = test_host();
        host.recorder = Some(Arc::clone(&recorder));
        host.emit(Event::TextDelta {
            delta: "unsaved".to_owned(),
        });
        assert!(host.cancellation.is_cancelled());
        assert!(host.partial_answer().is_empty());
        let log = paths.session_dir(&id).join("events.jsonl");
        let bytes = std::fs::read(&log).expect("log");
        host.emit(Event::TextDelta {
            delta: "more unsaved".to_owned(),
        });
        assert!(host.partial_answer().is_empty());
        assert_eq!(std::fs::read(log).expect("log"), bytes);
        assert!(recorder.lock().expect("lock").check_journal().is_err());
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
    fn transcript_ownership_suppresses_streaming_frames_and_restores_after_drop() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        *host.live_out.lock().expect("lock") =
            Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes)))));
        host.transcript_open
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let screen = TranscriptScreen(&host);
        host.emit(Event::TextDelta {
            delta: String::from("LIVE-ANSWER"),
        });
        assert!(
            bytes.lock().expect("lock").is_empty(),
            "stream painted over the viewer"
        );
        assert_eq!(host.partial_answer(), "LIVE-ANSWER");
        drop(screen);
        assert!(
            !host
                .transcript_open
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        assert!(String::from_utf8_lossy(&bytes.lock().expect("lock")).contains("\x1b[?1049l"));
        bytes.lock().expect("lock").clear();
        host.draw_stream_with("saved draft", 3);
        let written = String::from_utf8_lossy(&bytes.lock().expect("lock")).into_owned();
        assert!(written.contains("LIVE-ANSWER"), "{written:?}");
        assert!(written.contains("saved draft"), "{written:?}");
    }

    #[test]
    fn full_transcript_keeps_recorded_calls_replies_and_every_tool_row() {
        use rune_agent::history::Turn;
        use rune_core::id::ToolCallId;
        use rune_net::message::{ContentPart, Role};

        let mut content = String::new();
        for row in 1..=40 {
            writeln!(content, "TOOL-{row:02}").expect("tool row");
        }
        let turns = vec![
            Turn::user(1, "inspect"),
            Turn::assistant(
                2,
                vec![ContentPart::ToolCall {
                    id: ToolCallId::new("read").expect("tool id"),
                    name: String::from("read_file"),
                    arguments: String::from("{\"path\":\"fixture\"}"),
                }],
            ),
            Turn::new(
                3,
                Role::Tool,
                vec![ContentPart::ToolResult {
                    id: ToolCallId::new("read").expect("tool id"),
                    name: String::from("read_file"),
                    content,
                    is_error: false,
                }],
            ),
            Turn::assistant(
                4,
                vec![ContentPart::Text {
                    text: String::from("REPLY-END\x1b[2J"),
                }],
            ),
        ];
        let entries = history_entries(&turns);
        let rows = full_transcript_rows(&entries, 80);
        let rendered = rows.join("\n");
        assert!(rendered.starts_with("> inspect"));
        assert!(rendered.contains("read_file {\"path\":\"fixture\"}"));
        for row in 1..=40 {
            assert!(
                rendered.contains(&format!("TOOL-{row:02}")),
                "missing row {row}"
            );
        }
        assert!(rendered.ends_with("REPLY-END"));
        assert!(!rendered.contains("more line(s)"));
        assert!(!rendered.contains("\x1b[2J"));
        let mut view = rune_term::screen::Transcript::new(rows);
        assert!(view.visible(8).iter().any(|row| row.contains("inspect")));
        view.page_down(8);
        assert!(view.visible(8).iter().any(|row| row.contains("TOOL-10")));
        view.jump(usize::MAX, 8);
        assert!(view.visible(8).iter().any(|row| row.contains("REPLY-END")));
    }

    /// Returns the one-based column of the last caret move in a frame.
    fn last_caret_column(bytes: &[u8]) -> Option<usize> {
        let text = String::from_utf8_lossy(bytes);
        let end = text.rfind('G')?;
        let start = text.get(..end)?.rfind("\u{1b}[")?.saturating_add(2);
        text.get(start..end)?.parse().ok()
    }

    #[test]
    fn a_long_draft_stays_visible_as_a_turn_streams_and_closes() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let out: LiveSink = Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes))));
        let host = test_host();
        *host.live_out.lock().expect("lock") = Some(Arc::clone(&out));
        let mut reader = rune_term::input::KeyReader::new();
        reader.replace(&("a".repeat(160) + "TAIL-END"));
        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        let check = |grid: &mut rune_term::Grid| {
            let drawn = std::mem::take(&mut *bytes.lock().expect("lock"));
            assert!(last_caret_column(&drawn).expect("caret") <= 80);
            grid.feed(&drawn).expect("feed");
            assert!(grid.text().contains("TAIL-END"), "{}", grid.text());
            assert_eq!(grid.cursor().col, 79);
        };
        host.draw_stream_with(reader.line(), reader.column());
        check(&mut grid);
        host.emit(Event::TextDelta {
            delta: "answer".to_owned(),
        });
        check(&mut grid);
        host.draw_notice("still working");
        check(&mut grid);
        let mut sink = LockedSink { stream: out };
        close_turn(
            &host,
            &mut sink,
            &mut reader,
            &["answer".to_owned()],
            None,
            &[],
        )
        .expect("closed");
        check(&mut grid);
        assert_eq!(reader.line(), "a".repeat(160) + "TAIL-END");
    }

    #[test]
    fn multiline_caret_survives_streaming_notices_and_turn_close() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let out: LiveSink = Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes))));
        let host = test_host();
        *host.live_out.lock().expect("lock") = Some(Arc::clone(&out));
        let mut reader = rune_term::input::KeyReader::new();
        reader.paste("first 界\nsecond e\u{301}\nthird 👩‍💻");
        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        let check = |grid: &mut rune_term::Grid, edited_row: u16| {
            let drawn = std::mem::take(&mut *bytes.lock().expect("lock"));
            grid.feed(&drawn).expect("feed");
            let caret = grid.cursor();
            let first = caret.row.saturating_sub(edited_row);
            assert_eq!(grid.row_text(first), "> first 界");
            assert_eq!(grid.row_text(first + 1), "  second e\u{301}");
            assert_eq!(grid.row_text(first + 2), "  third 👩‍💻");
            assert_eq!(caret.col, 10);
        };
        let column =
            rune_term::width::str_width(&rune_term::editor::displayed("first 界\nsecond e\u{301}"));
        host.draw_stream_with(reader.line(), column);
        check(&mut grid, 1);
        host.emit(Event::TextDelta {
            delta: "answer".to_owned(),
        });
        check(&mut grid, 1);
        host.draw_notice("still working");
        check(&mut grid, 1);
        let mut sink = LockedSink { stream: out };
        close_turn(
            &host,
            &mut sink,
            &mut reader,
            &["answer".to_owned()],
            None,
            &[],
        )
        .expect("closed");
        check(&mut grid, 2);
        draw_prompt(&reader, &host, &sink.stream, &[], "> ").expect("idle prompt");
        check(&mut grid, 2);
        reader.clear();
        draw_prompt(&reader, &host, &sink.stream, &[], "> ").expect("cleared");
        grid.feed(&std::mem::take(&mut *bytes.lock().expect("lock")))
            .expect("feed");
        assert!(!grid.text().contains("second"), "{}", grid.text());
        assert!(!grid.text().contains("third"), "{}", grid.text());
    }

    #[test]
    fn a_delta_keeps_the_caret_where_the_reader_left_it() {
        // The caret is placed at the reader's display column. The character
        // count of the line is wrong for a caret moved back into the line or
        // after a wide character, and the caret would jump on every token.
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        if let Ok(mut slot) = host.live_out.lock() {
            *slot = Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&sink)))));
        }
        let marker = rune_term::width::str_width(rune_term::shell::prompt());

        for (line, column) in [("abcd", 1), ("書書", 4)] {
            host.draw_stream_with(line, column);
            sink.lock().expect("lock").clear();
            host.emit(Event::TextDelta {
                delta: "token ".to_owned(),
            });
            let drawn = sink.lock().expect("lock").clone();
            assert_eq!(
                last_caret_column(&drawn),
                Some(marker + column + 1),
                "the caret moved for {line:?}: {:?}",
                String::from_utf8_lossy(&drawn)
            );
        }
    }

    #[test]
    fn clearing_the_line_before_anything_streams_takes_it_off_the_screen() {
        // With nothing streamed yet and no notice, the frame for an emptied
        // line still has to be painted, or the cleared text stays on screen.
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        if let Ok(mut slot) = host.live_out.lock() {
            *slot = Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&sink)))));
        }
        host.draw_stream_with("half a correction", 17);
        host.draw_stream_with("", 0);

        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        grid.feed(&sink.lock().expect("lock")).expect("feed");
        let screen = grid.text();
        assert!(
            !screen.contains("half a correction"),
            "the cleared line is still on screen:\n{screen}"
        );
    }

    #[test]
    fn a_restarted_step_drops_the_text_of_the_failed_attempt() {
        // The retry streams the answer again from the start, so keeping the
        // failed attempt's text would show the answer's opening twice.
        let host = test_host();
        host.emit(Event::TextDelta {
            delta: "partial".to_owned(),
        });
        host.emit(Event::StepRestarted { step: 1 });
        host.emit(Event::TextDelta {
            delta: "complete".to_owned(),
        });
        let answer = host.streaming.lock().expect("lock").answer.clone();
        assert_eq!(answer, "complete");
    }

    #[test]
    fn a_streamed_delta_cannot_drive_the_terminal() {
        // Streamed rows reach the terminal as they are, so a clipboard write, a
        // screen clear, or a switch to the alternate screen in model output
        // would act on the reader's terminal mid-answer. Both lanes are checked,
        // including a sequence whose introducer arrives in an earlier delta.
        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let host = test_host();
        if let Ok(mut slot) = host.live_out.lock() {
            *slot = Some(Arc::new(Mutex::new(SharedSink(Arc::clone(&sink)))));
        }

        let deltas = [
            "before \u{1b}]52;c;SGVsbG8=\u{7} after",
            "wiped\u{1b}[2J",
            "\u{1b}[?1049h alternate",
            "split \u{1b}",
            "]52;c;c3BsaXQ=\u{7} tail",
            "\u{9d}52;c;YzE=\u{9c} c1",
        ];
        for delta in deltas {
            host.emit(Event::ReasoningDelta {
                delta: delta.to_owned(),
            });
            host.emit(Event::TextDelta {
                delta: delta.to_owned(),
            });
        }

        let written = String::from_utf8_lossy(&sink.lock().expect("lock")).into_owned();
        for forbidden in ["\u{1b}]", "\u{1b}[2J", "\u{1b}[?1049", "\u{9d}", "\u{9c}"] {
            assert!(
                !written.contains(forbidden),
                "{forbidden:?} reached the terminal: {written:?}"
            );
        }
        let streaming = host.streaming.lock().expect("lock");
        for lane in [&streaming.answer, &streaming.reasoning] {
            assert!(!lane.contains('\u{1b}'), "an escape was kept: {lane:?}");
            assert!(
                lane.contains("after"),
                "the text around it was lost: {lane:?}"
            );
            assert!(
                lane.contains("tail"),
                "the text around it was lost: {lane:?}"
            );
        }
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
    fn a_model_choice_is_reported_above_the_input_and_stays_there() {
        // The picker runs in raw mode, where a line written straight to the
        // stream lands at the caret inside the live region. The next frame
        // paints over it and leaves a second status block behind.
        let bytes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let out: LiveSink = Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes))));
        let host = test_host();
        let (prompt_row, caret) = host.idle_prompt();
        let draw = |host: &SessionHost| {
            let painted = host
                .paint(&[], None, &prompt_row, &[], caret)
                .expect("painted");
            out.lock()
                .expect("lock")
                .write_all(&painted)
                .expect("wrote");
        };

        draw(&host);
        let mut sink = LockedSink {
            stream: Arc::clone(&out),
        };
        report_model_choice(&host, &mut sink, "next-model", None).expect("reported");
        // The prompt the session draws next, as the loop does after the picker,
        // then the frame that commits the next prompt the user sends.
        draw(&host);
        let echo = transcript::render_prompt(rune_term::shell::prompt(), "hi", 80);
        let sent = host
            .paint(std::slice::from_ref(&echo), None, &prompt_row, &[], caret)
            .expect("painted");
        out.lock().expect("lock").write_all(&sent).expect("wrote");

        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        grid.feed(&bytes.lock().expect("lock")).expect("feed");
        let screen = grid.text();
        assert_eq!(
            screen.matches("model set to next-model").count(),
            1,
            "the report was not kept on screen:\n{screen}"
        );
        for row in ["ctrl-c cancel", "| auto |"] {
            assert_eq!(
                screen.matches(row).count(),
                1,
                "a second status block was left behind:\n{screen}"
            );
        }
        let report = screen.find("model set to").expect("the report");
        let sent = screen.find("> hi").expect("the sent prompt");
        assert!(
            report < sent,
            "the report is not above the prompt:\n{screen}"
        );
    }

    #[test]
    fn escape_at_the_prompt_clears_the_line_and_never_leaves() {
        // While a turn runs, Escape asks for a second press to cancel. A turn
        // that ends between the two presses hands the second to this prompt,
        // where it must not end the session.
        let mut reader = rune_term::input::KeyReader::new();
        let mut selected = 3;
        reader.replace("a draft");
        let step = idle_key(KeyAction::Escape, &mut reader, &mut selected, &[], &[]);
        assert!(matches!(step, Idle::Stay));
        assert_eq!(reader.line(), "");
        assert_eq!(selected, 0);

        let step = idle_key(KeyAction::Escape, &mut reader, &mut selected, &[], &[]);
        assert!(
            matches!(step, Idle::Stay),
            "escape on an empty line ended the session"
        );

        // Control-C and Control-D on an empty line are still the way out.
        let step = idle_key(KeyAction::Cancel, &mut reader, &mut selected, &[], &[]);
        assert!(matches!(step, Idle::Leave));
        let step = idle_key(KeyAction::Interrupt, &mut reader, &mut selected, &[], &[]);
        assert!(matches!(step, Idle::Leave));
    }

    #[test]
    fn caught_worker_panic_renders_one_diagnostic_without_duplicate_status_rows() {
        // A subprocess makes writes from the default panic hook observable,
        // rather than letting the test harness capture and hide the defect.
        if std::env::var_os("RUNE_R059_PANIC_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "session::tests::caught_worker_panic_renders_one_diagnostic_without_duplicate_status_rows",
                    "--nocapture",
                ])
                .env("RUNE_R059_PANIC_CHILD", "1")
                .output()
                .expect("panic fixture subprocess");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{stdout}\n{stderr}");
            assert!(stderr.is_empty(), "panic bypassed the renderer: {stderr}");
            assert!(!stdout.contains("panicked at"), "{stdout}");
            assert!(
                !stdout.contains('\u{1b}'),
                "terminal writes bypassed the sink: {stdout}"
            );
            let capture = stdout
                .lines()
                .find_map(|line| line.strip_prefix("R059_CAPTURE:"))
                .expect("renderer capture");
            let bytes: Vec<u8> = serde_json::from_str(capture).expect("frame bytes");
            let mut grid = rune_term::Grid::new(80, 24).expect("grid");
            grid.feed(&bytes).expect("replay");
            let screen = grid.text();
            assert_eq!(
                screen.matches("worker panicked: R059_FIXTURE").count(),
                1,
                "{screen}"
            );
            assert_eq!(screen.matches("ctrl-c cancel").count(), 1, "{screen}");
            assert_eq!(screen.matches("test | ").count(), 1, "{screen}");
            assert_eq!(screen.matches("draft after panic").count(), 1, "{screen}");
            assert!(screen.contains("partial before panic"), "{screen}");
            assert!(screen.contains("next turn answer"), "{screen}");
            return;
        }

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let out: LiveSink = Arc::new(Mutex::new(SharedSink(Arc::clone(&bytes))));
        let host = test_host();
        *host.live_out.lock().expect("output") = Some(Arc::clone(&out));
        let mut reader = rune_term::input::KeyReader::new();
        reader.replace("draft after panic");
        host.draw_stream_with(reader.line(), reader.column());
        let mut history = History::new();
        history.push_user("panic fixture");
        let (result, diagnostic) = run_on_worker(
            &mut history,
            |_| {
                host.emit(Event::TextDelta {
                    delta: "partial before panic".to_owned(),
                });
                // Bounded, debug-only fixture also checks terminal sanitization.
                std::panic::panic_any(String::from("R059_FIXTURE\u{1b}[2J"));
            },
            || std::thread::sleep(std::time::Duration::from_millis(1)),
        );
        let lines = report_failed_turn(
            &result.expect_err("caught panic"),
            &host,
            diagnostic.as_deref(),
        );
        host.clear_events();
        host.clear_streaming();
        let mut sink = LockedSink { stream: out };
        close_turn(&host, &mut sink, &mut reader, &lines, None, &[]).expect("closed");
        // A subsequent worker and frame must still be usable, and redraws must
        // not settle the diagnostic a second time.
        let (_, diagnostic) = run_on_worker(
            &mut history,
            |_| Err(rune_agent::steering::cancelled_error()),
            || {},
        );
        assert!(diagnostic.is_none());
        host.draw_stream_with(reader.line(), reader.column());
        close_turn(
            &host,
            &mut sink,
            &mut reader,
            &["next turn answer".to_owned()],
            None,
            &[],
        )
        .expect("next turn closed");
        println!(
            "R059_CAPTURE:{}",
            serde_json::to_string(&*bytes.lock().expect("bytes")).expect("json")
        );
    }

    #[test]
    fn a_caught_worker_panic_reports_an_internal_failure() {
        let host = test_host();
        let mut history = History::new();
        history.push_user("panic fixture");
        let (result, diagnostic) = run_on_worker(
            &mut history,
            |_| {
                host.emit(Event::TextDelta {
                    delta: "partial before panic".to_owned(),
                });
                panic!("R060_FIXTURE");
            },
            || std::thread::sleep(std::time::Duration::from_millis(1)),
        );

        let error = result.expect_err("caught worker panic");
        assert_eq!(error.code(), ErrorCode::Internal);
        let lines = report_failed_turn(&error, &host, diagnostic.as_deref()).join("\n");
        assert!(
            lines.contains("the turn failed: internal worker failure"),
            "{lines}"
        );
        assert!(lines.contains("partial before panic"), "{lines}");
        assert_eq!(
            lines.matches("worker panicked: R060_FIXTURE").count(),
            1,
            "{lines}"
        );
        assert!(!lines.contains("cancelled"), "{lines}");
    }

    #[test]
    fn a_turn_that_panics_keeps_the_conversation_it_started_with() {
        // A panic inside a turn ends that exchange and nothing more: falling
        // back to an empty history would throw away every earlier turn.
        let mut history = History::new();
        history.push_user("first question");
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: "first answer".to_owned(),
        }]);
        history.push_user("second question");

        let (result, diagnostic) = run_on_worker(
            &mut history,
            |taken| {
                // Stopped partway: an assistant turn holding a call with no
                // result, which every later request would be refused for.
                taken.push_assistant(vec![rune_net::message::ContentPart::ToolCall {
                    id: rune_core::id::ToolCallId::new("call-1").expect("id"),
                    name: "read_file".to_owned(),
                    arguments: "{}".to_owned(),
                }]);
                panic!("the turn failed");
            },
            || std::thread::sleep(std::time::Duration::from_millis(1)),
        );

        assert_eq!(
            result.err().map(|err| err.code()),
            Some(ErrorCode::Internal)
        );
        assert_eq!(
            diagnostic.as_deref(),
            Some("worker panicked: the turn failed")
        );
        assert_eq!(history.len(), 3, "the conversation was not kept");
        assert_eq!(history.turns()[0].text(), "first question");
        assert_eq!(history.turns()[2].text(), "second question");
        history
            .validate()
            .expect("the kept conversation can still be sent");
    }

    #[test]
    fn a_finished_turn_hands_back_what_it_added() {
        let mut history = History::new();
        history.push_user("question");
        let (result, diagnostic) = run_on_worker(
            &mut history,
            |taken| {
                taken.push_assistant(vec![rune_net::message::ContentPart::Text {
                    text: "answer".to_owned(),
                }]);
                Err(rune_agent::steering::cancelled_error())
            },
            || {},
        );
        assert_eq!(
            result.expect_err("cancelled turn").code(),
            ErrorCode::Cancelled
        );
        assert!(diagnostic.is_none());
        assert_eq!(history.len(), 2);
        assert_eq!(history.turns()[1].text(), "answer");
    }

    #[test]
    fn a_cancelled_turn_keeps_what_streamed_and_says_it_was_cancelled() {
        // The reader watched the partial answer arrive, so a transcript that
        // dropped it would disagree with the screen they just saw.
        let host = test_host();
        if let Ok(mut streaming) = host.streaming.lock() {
            streaming.answer.push_str("half an answer");
        }
        let lines =
            report_failed_turn(&rune_agent::steering::cancelled_error(), &host, None).join("\n");
        assert!(lines.contains("half an answer"), "{lines}");
        assert!(lines.contains("cancelled"), "{lines}");
        assert!(
            !lines.contains("failed"),
            "a cancel is not a failure: {lines}"
        );
    }

    #[test]
    fn cancelled_answers_keep_only_text_not_already_in_the_conversation() {
        let mut history = History::new();
        history.push_user("an earlier question");
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: "an earlier answer".to_owned(),
        }]);
        history.push_user("slow");
        let start = history.len();
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: "STREAM-01\n".to_owned(),
        }]);
        retain_partial_answer(&mut history, start, "STREAM-01\nSTREAM-02\nSTREAM-03\n");
        assert_eq!(history.turns()[3].text(), "STREAM-01\n");
        assert_eq!(history.turns()[4].text(), "STREAM-02\nSTREAM-03\n");

        // Cancelling after text is in history, or before any arrives, adds none.
        retain_partial_answer(&mut history, start, "STREAM-01\nSTREAM-02\nSTREAM-03\n");
        retain_partial_answer(&mut history, start, "");
        assert_eq!(history.len(), 5);
        history
            .validate()
            .expect("the next request can use the partial answer");
    }

    #[test]
    fn a_failed_turn_names_the_failure_and_what_to_do() {
        let host = test_host();
        let err = RuneError::new(
            ErrorCode::TransportFailure,
            "the endpoint closed the stream",
        )
        .with_hint("check the provider status");
        let lines = report_failed_turn(&err, &host, None).join("\n");
        assert!(lines.contains("the endpoint closed the stream"), "{lines}");
        assert!(lines.contains("check the provider status"), "{lines}");
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
        // The skills installed for this user may follow the override, so the
        // check is on what the prompt opens with rather than on all of it.
        assert!(
            prompt.instructions.starts_with("You are a terse reviewer."),
            "{}",
            prompt.instructions
        );
        let built_in = prompt::SYSTEM_PROMPT.lines().next().unwrap_or_default();
        assert!(
            !prompt.instructions.contains(built_in),
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
    fn private_network_access_requires_a_user_answer_in_every_mode() {
        for mode in [
            PermissionMode::Ask,
            PermissionMode::Auto,
            PermissionMode::FullAccess,
        ] {
            let mut host = host_with_reviewer(ReviewOutcome::Clear {
                reviewed_action: "domain:localhost".to_owned(),
            });
            host.mode = mode;
            host.rules.push(rune_policy::rules::Rule::allow(
                "web_fetch",
                "*",
                rune_policy::decision::Layer::User,
            ));
            host.rules.push(rune_policy::rules::Rule::allow(
                "*",
                "*",
                rune_policy::decision::Layer::User,
            ));
            assert_eq!(
                host.decide("web_fetch", Some("domain:localhost")).0,
                Outcome::Allow
            );
            assert_eq!(
                host.decide_private_network(Some("domain:localhost")).0,
                Outcome::Ask
            );
            let (requests, pending) = mpsc::channel();
            *host.approval_requests.lock().expect("lock") = Some(requests);
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    let first = host.decide_private_network(Some("domain:localhost"));
                    let second = host.decide_private_network(Some("domain:localhost"));
                    (first, second)
                });
                let request = pending
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("separate private-network approval");
                assert_eq!(request.tool, "web_fetch_private");
                assert_eq!(request.target, "domain:localhost");
                assert!(request.reason.contains("private-network access"));
                assert!(request.reason.contains("redirects"));
                request.answer.send(Outcome::Allow).expect("approve once");
                let request = pending
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("approval cannot leak to the next call");
                request.answer.send(Outcome::Deny).expect("deny");
                let (first, second) = worker.join().expect("worker");
                assert_eq!(first.0, Outcome::Allow);
                assert_eq!(second.0, Outcome::Deny);
            });
        }
    }

    #[test]
    fn only_an_explicit_user_private_network_rule_can_grant_access() {
        use rune_policy::decision::Layer;
        use rune_policy::rules::Rule;
        for layer in [
            Layer::Default,
            Layer::Project,
            Layer::User,
            Layer::Session,
            Layer::Grant,
        ] {
            let mut host = test_host();
            host.mode = PermissionMode::FullAccess;
            host.rules
                .push(Rule::allow("web_fetch_private", "domain:localhost", layer));
            let outcome = host.decide_private_network(Some("domain:localhost")).0;
            assert_eq!(
                outcome,
                if layer >= Layer::User {
                    Outcome::Allow
                } else {
                    Outcome::Ask
                }
            );
            assert_eq!(
                host.decide_private_network(Some("domain:other")).0,
                Outcome::Ask
            );
        }
        let mut host = test_host();
        host.mode = PermissionMode::FullAccess;
        host.rules
            .push(Rule::deny("web_fetch_private", "*", Layer::User));
        let (requests, pending) = mpsc::channel();
        *host.approval_requests.lock().expect("lock") = Some(requests);
        assert_eq!(
            host.decide_private_network(Some("domain:localhost")).0,
            Outcome::Deny
        );
        assert!(
            pending.try_recv().is_err(),
            "denial cannot be overridden by a prompt"
        );
    }

    #[test]
    fn a_terminal_answer_resolves_only_the_displayed_call() {
        let mut host = test_host();
        host.mode = PermissionMode::Ask;
        let (requests, pending) = mpsc::channel();
        *host.approval_requests.lock().expect("lock") = Some(requests);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let first = host.decide("shell", Some("printf AUDIT_SHELL_OK"));
                let second = host.decide("shell", Some("printf AUDIT_SHELL_OK"));
                (first, second)
            });
            let first = pending
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("first approval request");
            assert_eq!(first.tool, "shell");
            assert_eq!(first.target, "printf AUDIT_SHELL_OK");
            assert!(
                approval_lines(&first, 80)
                    .join("\n")
                    .contains(&first.target)
            );
            first.answer.send(Outcome::Allow).expect("approve once");
            let second = pending
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("the same command needs another approval");
            second.answer.send(Outcome::Deny).expect("deny");
            let (first, second) = worker.join().expect("worker");
            assert_eq!(first.0, Outcome::Allow, "{}", first.1);
            assert_eq!(second.0, Outcome::Deny, "{}", second.1);
        });
    }

    #[test]
    fn cancellation_wakes_a_worker_waiting_for_approval() {
        let mut host = test_host();
        host.mode = PermissionMode::Ask;
        let (requests, pending) = mpsc::channel();
        *host.approval_requests.lock().expect("lock") = Some(requests);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| host.decide("shell", Some("printf AUDIT_SHELL_OK")));
            let _request = pending
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("approval request");
            host.cancellation.cancel();
            let (answer, reason) = worker.join().expect("worker");
            assert_eq!(answer, Outcome::Deny);
            assert!(reason.contains("cancelled"), "{reason}");
        });
    }

    #[test]
    fn closed_approval_input_never_allows_a_call() {
        let mut host = test_host();
        host.mode = PermissionMode::Ask;
        let (requests, pending) = mpsc::channel();
        *host.approval_requests.lock().expect("lock") = Some(requests);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| host.decide("shell", Some("printf AUDIT_SHELL_OK")));
            let request = pending
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("approval request");
            drop(request);
            let (answer, reason) = worker.join().expect("worker");
            assert_eq!(answer, Outcome::Deny);
            assert!(reason.contains("closed"), "{reason}");
        });
        drop(pending);
        assert_eq!(
            host.decide("shell", Some("printf AUDIT_SHELL_OK")).0,
            Outcome::Deny
        );
    }

    #[test]
    fn terminal_approval_does_not_override_a_rule_or_approve_without_input() {
        let mut host = test_host();
        host.mode = PermissionMode::Ask;
        host.rules.push(rune_policy::rules::Rule::deny(
            "read_file",
            ".env",
            rune_policy::decision::Layer::User,
        ));
        assert_eq!(
            host.decide("shell", Some("printf AUDIT_SHELL_OK")).0,
            Outcome::Ask
        );
        let (requests, pending) = mpsc::channel();
        *host.approval_requests.lock().expect("lock") = Some(requests);
        assert_eq!(
            host.decide("read_file", Some("src/main.rs")).0,
            Outcome::Allow
        );
        assert_eq!(host.decide("read_file", Some(".env")).0, Outcome::Deny);
        assert!(
            pending.try_recv().is_err(),
            "settled rules must never prompt"
        );
    }

    #[test]
    fn approval_scope_cannot_hide_terminal_controls_or_clip_long_commands() {
        let (answer, _response) = mpsc::sync_channel(1);
        let request = ApprovalRequest {
            tool: "shell".to_owned(),
            target: "printf 'a\\b'\n\t\u{1b}[2J\u{202e}TAIL-END".to_owned(),
            reason: "no rule allows it".to_owned(),
            answer,
        };
        let lines = approval_lines(&request, 80).join("\n");
        assert!(
            lines.contains("\\n\\t\\u{1b}[2J\\u{202e}TAIL-END"),
            "{lines}"
        );
        assert!(lines.contains("a\\\\b"), "{lines}");
        assert!(!lines.contains('\u{1b}'));
        let narrow = approval_lines(&request, 12);
        assert!(
            narrow
                .iter()
                .all(|line| rune_term::width::str_width(line) <= 12)
        );
        assert!(narrow.join("").ends_with("TAIL-END\""));
    }

    #[test]
    fn an_approval_prompt_that_cannot_be_displayed_denies() {
        let host = test_host();
        let (answer, _response) = mpsc::sync_channel(1);
        let request = ApprovalRequest {
            tool: "shell".to_owned(),
            target: "printf AUDIT_SHELL_OK".to_owned(),
            reason: "no rule allows it".to_owned(),
            answer,
        };
        let mut reader = rune_term::input::KeyReader::new();
        assert_eq!(
            collect_approval(&request, &host, &mut reader),
            Outcome::Deny
        );
    }

    fn fixture_question() -> Question {
        Question {
            text: "Which choice?".to_owned(),
            options: vec![
                rune_tools::ask_user::Choice {
                    label: "Alpha".to_owned(),
                    description: None,
                },
                rune_tools::ask_user::Choice {
                    label: "Beta".to_owned(),
                    description: Some("The second choice".to_owned()),
                },
            ],
        }
    }

    #[test]
    fn question_input_unavailable_or_closed_never_selects_an_answer() {
        let answerer = TerminalQuestions::default();
        let context = ExecutionContext::new(Utf8PathBuf::from("/tmp"));
        let questions = [fixture_question()];
        assert_eq!(
            answerer
                .ask(&questions, &context)
                .expect_err("unavailable")
                .code(),
            ErrorCode::InputRequired
        );
        let (requests, pending) = mpsc::channel();
        *answerer.requests.lock().expect("lock") = Some(requests);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| answerer.ask(&questions, &context));
            let request = pending
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("question request");
            assert_eq!(request.questions, questions);
            drop(request);
            assert_eq!(
                worker.join().expect("worker").expect_err("closed").code(),
                ErrorCode::InputRequired
            );
        });
        drop(pending);
        assert_eq!(
            answerer
                .ask(&questions, &context)
                .expect_err("disconnected")
                .code(),
            ErrorCode::InputRequired
        );
    }

    #[test]
    fn cancellation_wakes_a_worker_waiting_for_question_input() {
        for cancel_context in [false, true] {
            let answerer = TerminalQuestions::default();
            let context = ExecutionContext::new(Utf8PathBuf::from("/tmp"));
            let questions = [fixture_question()];
            let (requests, pending) = mpsc::channel();
            *answerer.requests.lock().expect("lock") = Some(requests);
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| answerer.ask(&questions, &context));
                let _request = pending
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("question request");
                if cancel_context {
                    context.cancellation().cancel();
                } else {
                    answerer.cancellation.cancel();
                }
                assert_eq!(
                    worker
                        .join()
                        .expect("worker")
                        .expect_err("cancelled")
                        .code(),
                    ErrorCode::Cancelled
                );
            });
        }
    }

    #[test]
    fn question_choices_keep_indices_and_sanitize_displayed_text() {
        let mut question = fixture_question();
        question.text.push_str("\u{1b}[2JTAIL-END");
        question.options[0].label = "Beta".to_owned();
        question.options[1].description = Some("\u{1b}]52;c;clipboard\u{7}details".to_owned());
        let lines = question_lines(&question, 12);
        assert!(!lines.join("").contains('\u{1b}'));
        assert!(lines.join("").contains("TAIL-END"));
        assert!(lines.join("").contains("details"));
        assert!(
            lines
                .iter()
                .all(|line| rune_term::width::str_width(line) <= 12)
        );
        let mut picker = rune_term::picker::Picker::new(
            "question",
            question
                .options
                .iter()
                .map(|option| option.label.clone())
                .collect(),
            2,
        );
        let cancellation = Cancellation::new();
        assert_eq!(
            question_key(KeyAction::Down, &mut picker, &cancellation),
            None
        );
        assert_eq!(
            question_key(KeyAction::Ignored, &mut picker, &cancellation),
            None
        );
        assert_eq!(
            question_key(KeyAction::Submit, &mut picker, &cancellation),
            Some(1)
        );
        assert!(!cancellation.is_cancelled());
    }

    #[test]
    fn a_question_that_cannot_be_displayed_requires_input() {
        let host = test_host();
        let mut reader = rune_term::input::KeyReader::new();
        let error = collect_questions(&[fixture_question()], &host, &mut reader)
            .expect_err("cannot display the question");
        assert_eq!(error.code(), ErrorCode::InputRequired);
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
    fn streamed_fenced_code_matches_the_finished_transcript_across_resize() {
        let host = test_host();
        host.width.store(20, std::sync::atomic::Ordering::Relaxed);
        let mut text = String::new();
        for ch in "```rust\n    call(\"alpha  beta\",  gamma);\n```\nafter".chars() {
            text.push(ch);
            // Accumulate without painting, which refreshes the size from the
            // runner's own terminal rather than this test's chosen width.
            host.streaming.lock().expect("streaming").answer.push(ch);
            assert_eq!(
                host.streaming_rows().join("\n").trim_end(),
                render_entries(&[Entry::assistant(&text)], &host).join("\n"),
                "streamed and finished code disagree at {text:?}"
            );
        }
        assert!(
            host.streaming_rows()
                .iter()
                .any(|row| row.starts_with("    ↪ "))
        );
        for width in [12, 40, 20] {
            host.width
                .store(width, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(
                host.streaming_rows(),
                render_entries(&[Entry::assistant(&text)], &host)
            );
        }
        host.clear_streaming();
        host.streaming
            .lock()
            .expect("streaming")
            .answer
            .push_str("words after the code");
        assert_eq!(
            host.streaming_rows(),
            render_entries(&[Entry::assistant("words after the code")], &host)
        );
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
    fn live_usage_supersedes_the_resume_estimate_and_new_clears_its_source() {
        let host = test_host();
        host.context_used
            .store(1234, std::sync::atomic::Ordering::Relaxed);
        *host.context_source.lock().expect("source") = Some("saved usage");
        assert!(host.status_line(120).contains("ctx ~1.2k (saved usage)"));
        assert!(render_status(&host.info("p", "e")).contains("saved usage (estimate)"));
        host.record_context_size(0);
        assert!(host.status_line(120).contains("saved usage"));
        host.record_context_size(1000);
        let info = host.info("p", "e");
        assert_eq!(info.context_used, 1000);
        assert_eq!(info.context_source, None);
        assert!(!host.status_line(120).contains("saved usage"));
        *host.context_source.lock().expect("source") = Some("history bytes");
        host.forget_context();
        let info = host.info("p", "e");
        assert_eq!(info.context_used, 0);
        assert_eq!(info.context_source, None);
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
    fn the_catalog_budget_follows_the_window_it_is_given() {
        // A fixed budget is wrong at both ends, so the session sizes it from
        // the window the model reports.
        use rune_core::budget::LimitName;

        let mut small = BudgetSet::new();
        adopt_skill_catalog_budget(&mut small, Some(8_000));
        let mut large = BudgetSet::new();
        adopt_skill_catalog_budget(&mut large, Some(2_000_000));

        assert!(
            small.get_bytes(LimitName::SkillCatalogBytes)
                < large.get_bytes(LimitName::SkillCatalogBytes),
            "a larger window did not earn a larger catalog budget"
        );
    }

    #[test]
    fn an_undeclared_window_keeps_the_catalog_fallback() {
        // The compiled window is a guess, so a model that states none keeps the
        // documented fallback rather than a budget derived from the guess.
        use rune_core::budget::LimitName;

        let mut limits = BudgetSet::new();
        adopt_skill_catalog_budget(&mut limits, None);
        assert_eq!(
            limits.get_bytes(LimitName::SkillCatalogBytes),
            rune_core::budget::SKILL_CATALOG_FALLBACK_BYTES
        );
    }

    #[test]
    fn a_named_catalog_budget_survives_the_derivation() {
        // A limit someone set on purpose is not this program's to overwrite.
        use rune_core::budget::{Budget, LimitName};
        use rune_core::config::Layer;

        let mut limits = BudgetSet::new();
        limits
            .set(
                LimitName::SkillCatalogBytes,
                Budget::Bounded(1234),
                Layer::User,
            )
            .expect("set");
        adopt_skill_catalog_budget(&mut limits, Some(2_000_000));

        assert_eq!(limits.get_bytes(LimitName::SkillCatalogBytes), 1234);
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

    struct SummaryFixture {
        reply: String,
        status: u16,
        requests: Mutex<Vec<serde_json::Value>>,
        events: Option<Arc<Mutex<Vec<Event>>>>,
    }

    impl SummaryFixture {
        fn new(reply: &str) -> Self {
            Self {
                reply: reply.to_owned(),
                status: 200,
                requests: Mutex::new(Vec::new()),
                events: None,
            }
        }
    }

    impl rune_net::fetch::Fetch for SummaryFixture {
        fn send(
            &self,
            request: rune_net::fetch::FetchRequest,
        ) -> rune_net::error::NetResult<rune_net::fetch::FetchResponse> {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("request JSON");
            if body.to_string().contains("<context_handoff>") {
                let events = self
                    .events
                    .as_ref()
                    .expect("event observer")
                    .lock()
                    .expect("events");
                assert!(
                    events
                        .iter()
                        .any(|event| matches!(event, Event::ContextCompacted { .. })),
                    "the compaction event must precede the next model request"
                );
            }
            self.requests.lock().expect("requests").push(body);
            let body = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({
                    "choices": [{"delta": {"content": self.reply}, "finish_reason": "stop"}]
                })
            );
            Ok(rune_net::fetch::FetchResponse {
                status: self.status,
                content_type: "text/event-stream".to_owned(),
                retry_after: None,
                location: None,
                body: Box::new(std::io::Cursor::new(body.into_bytes())),
            })
        }
    }

    fn compaction_fixture() -> (SessionHost, History, rune_net::provider::RequestPlan) {
        let mut host = test_host();
        for (name, value) in [
            (rune_core::budget::LimitName::MaxToolResultBytes, 16 * 1024),
            (rune_core::budget::LimitName::CompactionTriggerPercent, 50),
        ] {
            host.limits
                .set(
                    name,
                    rune_core::budget::Budget::Bounded(value),
                    rune_core::config::Layer::User,
                )
                .expect("limit");
        }
        let mut history = History::new();
        for index in 0..4 {
            history.push_user(format!("fixture question {index}"));
            history.push_assistant(vec![rune_net::message::ContentPart::Text {
                text: "earlier answer ".repeat(100),
            }]);
        }
        history.push_user("continue the fixture");
        host.capture_history(&history);
        let mut plan = rune_net::provider::RequestPlan::new(host.model());
        plan.messages = history.to_messages();
        let tokens = request_estimate(&host, &plan)
            .expect("estimate")
            .input_tokens;
        host.context_limit.store(
            tokens.saturating_mul(100).saturating_div(60),
            std::sync::atomic::Ordering::Relaxed,
        );
        (host, history, plan)
    }

    #[test]
    fn crossing_the_configured_trigger_compacts_before_the_next_request() {
        let (host, mut history, mut plan) = compaction_fixture();
        let mut fetch = SummaryFixture::new(
            "The earlier fixture exchanges established the task and its current progress.",
        );
        fetch.events = Some(Arc::clone(&host.events));
        let before = history.clone();
        let cut = rune_agent::compaction::plan(&history, &host.limits)
            .expect("plan")
            .removed_turns;
        let estimate = request_estimate(&host, &plan).expect("estimate");
        assert!(estimate.used_percent() >= 50 && estimate.used_percent() < 80);
        host.prepare_request(&mut history, &mut plan, &fetch)
            .expect("automatic compaction");
        assert_eq!(fetch.requests.lock().expect("requests").len(), 1);
        assert!(
            fetch.requests.lock().expect("requests")[0]
                .to_string()
                .contains("<conversation>")
        );
        assert_eq!(plan.messages, history.to_messages());
        assert!(history.turns()[0].text().contains("<context_handoff>"));
        for (retained, original) in history.turns()[1..].iter().zip(&before.turns()[cut..]) {
            assert_eq!(retained.parts, original.parts);
        }
        assert!(matches!(
            host.events.lock().expect("events").last(),
            Some(Event::ContextCompacted { .. })
        ));
        assert!(
            request_estimate(&host, &plan)
                .expect("estimate")
                .used_percent()
                < 50
        );
        host.capture_history(&history);
        assert_eq!(
            *host.transcript.lock().expect("transcript"),
            history_entries(before.turns()),
            "compaction must preserve the visible transcript without inserting the handoff"
        );
        history.validate().expect("compacted history is valid");
        rune_net::transport::stream_completion(
            &fetch,
            &host.endpoint,
            host.dialect.as_ref(),
            &plan,
            rune_net::transport::RequestTimeouts::from_limits(&host.limits),
            &|| false,
        )
        .expect("next model request");
        assert_eq!(fetch.requests.lock().expect("requests").len(), 2);
    }

    #[test]
    fn below_the_trigger_no_summary_request_is_sent() {
        let (host, mut history, mut plan) = compaction_fixture();
        host.context_limit
            .store(100_000, std::sync::atomic::Ordering::Relaxed);
        let fetch = SummaryFixture::new("unused");
        let before = history.to_messages();
        host.prepare_request(&mut history, &mut plan, &fetch)
            .expect("fits");
        assert!(fetch.requests.lock().expect("requests").is_empty());
        assert_eq!(history.to_messages(), before);
        assert!(host.events.lock().expect("events").is_empty());
    }

    #[test]
    fn failed_automatic_compaction_preserves_history_and_blocks_the_model_request() {
        for (reply, status) in [("", 200), ("too short", 200), ("provider error", 500)] {
            let (host, mut history, mut plan) = compaction_fixture();
            let mut fetch = SummaryFixture::new(reply);
            fetch.status = status;
            let before = history.to_messages();
            assert!(
                host.prepare_request(&mut history, &mut plan, &fetch)
                    .is_err()
            );
            assert_eq!(history.to_messages(), before);
            assert_eq!(plan.messages, before);
            assert!(host.events.lock().expect("events").is_empty());
        }
    }

    #[test]
    fn compaction_estimates_include_instructions_tools_and_dialect_output_reserve() {
        let mut host = test_host();
        host.dialect = Box::new(rune_net::anthropic::Anthropic);
        host.context_limit
            .store(20_000, std::sync::atomic::Ordering::Relaxed);
        let mut plan = rune_net::provider::RequestPlan::new(host.model());
        plan.messages = rune_net::transport::one_shot_messages("hello");
        let small = request_estimate(&host, &plan).expect("estimate");
        plan.instructions = "instructions ".repeat(100);
        plan.tools = inventory::advertisement(&host.registry);
        let full = request_estimate(&host, &plan).expect("estimate");
        assert!(full.input_tokens > small.input_tokens);
        assert_eq!(
            full.capacity,
            20_000 - rune_net::anthropic::DEFAULT_MAX_TOKENS
        );
        assert_eq!(
            full.output_tokens, 0,
            "output reserve is subtracted exactly once"
        );
    }

    #[test]
    fn compaction_during_a_turn_keeps_new_transcript_entries_and_partial_answers() {
        let (host, mut history, mut plan) = compaction_fixture();
        history.push_assistant(vec![rune_net::message::ContentPart::Text {
            text: "completed step".to_owned(),
        }]);
        history.push_user("steered correction");
        plan.messages = history.to_messages();
        let fetch = SummaryFixture::new(
            "The earlier exchanges established the task and its current progress.",
        );
        host.prepare_request(&mut history, &mut plan, &fetch)
            .expect("compacted");
        let start = host
            .transcript_history_len
            .load(std::sync::atomic::Ordering::Relaxed);
        retain_partial_answer(&mut history, start, "partial next step");
        host.capture_history(&history);
        let entries = host.transcript.lock().expect("transcript");
        for text in ["completed step", "steered correction", "partial next step"] {
            assert_eq!(entries.iter().filter(|entry| entry.text == text).count(), 1);
        }
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
        let rows = completion_rows("/mod", 0, &theme, false, usize::MAX);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].starts_with("> /model"), "{rows:?}");
        assert!(rows[1].starts_with("  /models"), "{rows:?}");
        // Each row carries what the command does, which is what makes the list
        // usable without trying every name.
        assert!(rows[0].contains("choose a model"), "{rows:?}");
        assert!(rows[1].contains("same as"), "{rows:?}");
    }

    #[test]
    fn path_menus_scroll_with_the_selection_and_keep_within_available_rows() {
        let paths = crate::path_completion::Paths {
            range: 5..7,
            matches: (0..15)
                .map(|index| format!("'fixture {index:02}.txt'"))
                .collect(),
        };
        for selected in 0..paths.matches.len() {
            for room in 1..=8 {
                let rows = path_completion_rows(&paths, selected, &Theme::no_color(), false, room);
                assert!(rows.len() <= room);
                assert!(
                    rows.contains(&format!("> {}", paths.matches[selected])),
                    "{rows:?}"
                );
                assert!(rows.iter().all(|row| !row.contains('\u{1b}')));
            }
        }
        assert!(path_completion_rows(&paths, 0, &Theme::no_color(), false, 0).is_empty());
    }

    #[test]
    fn tab_completes_one_match_without_opening_a_menu_or_changing_slash_commands() {
        let temp = tempfile::tempdir().expect("workspace");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_owned()).expect("UTF-8");
        std::fs::write(root.join("fixture space.txt"), "fixture").expect("file");
        let context = ExecutionContext::new(root);
        let limits = rune_tools::workspace::FileLimits::default();
        let mut reader = rune_term::input::KeyReader::new();
        let mut paths = None;
        let mut selected = 0;
        reader.replace("read ./fi");
        assert!(path_key(
            KeyAction::Complete,
            &mut reader,
            &mut paths,
            &mut selected,
            &context,
            &limits
        ));
        assert_eq!(reader.line(), "read './fixture space.txt'");
        assert!(paths.is_none());
        for line in ["/mod", "/help fixture", "/zzz"] {
            reader.replace(line);
            assert!(!path_key(
                KeyAction::Complete,
                &mut reader,
                &mut paths,
                &mut selected,
                &context,
                &limits
            ));
            assert_eq!(reader.line(), line);
        }
    }

    #[test]
    fn a_bare_slash_offers_a_window_and_says_how_many_there_are() {
        // Every command matches a bare slash. Drawing all of them would swallow
        // the screen for a list a reader only ever takes the top few rows of, so
        // a window is shown with a row saying how far through it is.
        let theme = Theme::no_color();
        let rows = completion_rows("/", 0, &theme, false, usize::MAX);
        let total = rune_term::commands::BUILTINS.len();
        assert_eq!(rows.len(), COMPLETION_WINDOW.saturating_add(1), "{rows:?}");
        assert!(rows[0].starts_with("> /model"), "{rows:?}");
        assert!(
            rows.last().is_some_and(|r| r.contains("1-6 of")),
            "{rows:?}"
        );
        assert!(
            rows.last().is_some_and(|r| r.contains(&total.to_string())),
            "{rows:?}"
        );
    }

    #[test]
    fn a_list_that_fits_carries_no_position_row() {
        let theme = Theme::no_color();
        let rows = completion_rows("/mod", 0, &theme, false, usize::MAX);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains(" of ")), "{rows:?}");
    }

    #[test]
    fn the_window_follows_the_selection_without_the_input_moving() {
        // The highlight moves down a fixed screen until it reaches the last row,
        // and only then does the window scroll. That is what keeps the list from
        // jumping on every key.
        let count = rune_term::commands::BUILTINS.len();
        // Inside the first screenful the window does not move.
        assert_eq!(
            completion_window(count, 0, COMPLETION_WINDOW),
            0..COMPLETION_WINDOW
        );
        assert_eq!(
            completion_window(count, COMPLETION_WINDOW - 1, COMPLETION_WINDOW),
            0..COMPLETION_WINDOW
        );
        // Past it, the selection stays on the last visible row.
        let scrolled = completion_window(count, COMPLETION_WINDOW, COMPLETION_WINDOW);
        assert_eq!(scrolled, 1..COMPLETION_WINDOW.saturating_add(1));
        // And the end of the list is reachable rather than cut off.
        let last = completion_window(count, count.saturating_sub(1), COMPLETION_WINDOW);
        assert_eq!(last.end, count);
    }

    #[test]
    fn a_window_never_looks_past_the_end_of_a_short_list() {
        assert_eq!(completion_window(2, 0, COMPLETION_WINDOW), 0..2);
        assert_eq!(completion_window(0, 0, COMPLETION_WINDOW), 0..0);
        // A selection past the end is clamped rather than panicking.
        let count = rune_term::commands::BUILTINS.len();
        assert!(completion_window(count, 999, COMPLETION_WINDOW).end <= count);
    }

    #[test]
    fn the_highlight_is_always_inside_the_window() {
        // A highlight scrolled out of view would make the list unusable: the
        // user is moving something they cannot see.
        let theme = Theme::no_color();
        let count = rune_term::commands::BUILTINS.len();
        for selected in 0..count {
            let rows = completion_rows("/", selected, &theme, false, usize::MAX);
            let marked = rows.iter().filter(|r| r.starts_with('>')).count();
            assert_eq!(marked, 1, "selection {selected} marked {marked} rows");
        }
    }

    #[test]
    fn short_completion_menus_keep_every_command_visible_and_selectable() {
        let theme = Theme::no_color();
        let count = rune_term::commands::BUILTINS.len();
        for room in 1..=8 {
            for selected in 0..count {
                let rows = completion_rows("/", selected, &theme, false, room);
                assert!(rows.len() <= room, "{rows:?}");
                let chosen = open_completion("/", &rows, selected).expect("completion");
                assert!(
                    rows.iter()
                        .any(|row| row.starts_with(&format!("> /{}", chosen.name))),
                    "selection {selected} is hidden: {rows:?}"
                );
            }
        }
        assert!(completion_rows("/", 0, &theme, false, 0).is_empty());
    }

    #[test]
    fn model_menus_keep_the_selection_visible_after_resizing_and_narrowing() {
        let theme = Theme::no_color();
        let mut picker = rune_term::picker::Picker::new(
            "models from fixture",
            (1..=15).map(|index| format!("model-{index:02}")).collect(),
            rune_term::picker::DEFAULT_WINDOW,
        );
        picker.to(14);
        for room in [16, 4, 3, 2, 1, 16] {
            let menu = picker_menu(&mut picker, &theme, false, room);
            assert!(menu.len() <= room, "{menu:?}");
            assert_eq!(picker.selected(), Some("model-15"));
            assert!(menu.iter().any(|row| row == "> model-15"), "{menu:?}");
        }
        assert!(picker_menu(&mut picker, &theme, false, 0).is_empty());
        picker.set_query("model-03");
        let menu = picker_menu(&mut picker, &theme, false, 4);
        assert!(menu.iter().any(|row| row == "> model-03"), "{menu:?}");
        assert!(!menu.iter().any(|row| row.contains(" of ")), "{menu:?}");
        picker.set_query("missing");
        let menu = picker_menu(&mut picker, &theme, false, 1);
        assert_eq!(menu.len(), 1);
        assert!(menu[0].contains("no match"), "{menu:?}");
    }

    #[test]
    fn accepting_uses_the_selection_the_window_is_showing() {
        // The position row must not be mistaken for a command.
        let theme = Theme::no_color();
        let rows = completion_rows("/", 0, &theme, false, usize::MAX);
        let chosen = open_completion("/", &rows, 0).expect("a completion");
        assert_eq!(chosen.name, "model");
        // Every command is reachable, including the ones past the first window:
        // clamping on the drawn rows instead of the matches left the tail of the
        // list unselectable.
        let last = rune_term::commands::BUILTINS.len().saturating_sub(1);
        let rows = completion_rows("/", last, &theme, false, usize::MAX);
        assert!(rows.iter().any(|r| r.starts_with('>')), "{rows:?}");
        let chosen = open_completion("/", &rows, last).expect("a completion");
        assert_eq!(chosen.name, "quit");
    }

    #[test]
    fn the_dropdown_is_absent_unless_a_command_is_being_typed() {
        let theme = Theme::no_color();
        for line in ["", "hello", "/help me", "/model x", "a/b", "/zzz"] {
            assert!(
                completion_rows(line, 0, &theme, false, usize::MAX).is_empty(),
                "{line:?} offered rows"
            );
        }
        // A whole command name is still offered, so the row does not vanish as
        // the last letter is typed.
        assert!(!completion_rows("/help", 0, &theme, false, usize::MAX).is_empty());
    }

    #[test]
    fn the_highlighted_row_is_the_one_the_arrows_moved_to() {
        let theme = Theme::no_color();
        let first = completion_rows("/mod", 0, &theme, false, usize::MAX);
        assert!(first[0].starts_with('>'), "{first:?}");
        let second = completion_rows("/mod", 1, &theme, false, usize::MAX);
        assert!(second[1].starts_with('>'), "{second:?}");
        assert!(second[0].starts_with("  "), "{second:?}");
    }

    #[test]
    fn the_rows_line_up_their_descriptions() {
        let theme = Theme::no_color();
        let rows = completion_rows("/mod", 0, &theme, false, usize::MAX);
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
        let rows = completion_rows("/mod", 0, &theme, false, usize::MAX);
        let chosen = open_completion("/mod", &rows, 0).expect("a completion");
        assert_eq!(chosen.name, "model");
        let rows = completion_rows("/mod", 1, &theme, false, usize::MAX);
        let chosen = open_completion("/mod", &rows, 1).expect("a completion");
        assert_eq!(chosen.name, "models");
        // With no dropdown there is nothing to accept.
        assert!(open_completion("hello", &[], 0).is_none());
    }

    #[test]
    fn a_colorless_theme_emits_no_escapes_in_the_dropdown() {
        let theme = Theme::no_color();
        let plain = completion_rows("/mod", 0, &theme, false, usize::MAX);
        assert!(!plain.is_empty(), "nothing matched");
        for row in plain {
            assert!(!row.contains('\u{1b}'), "{row:?}");
        }
        // A colored theme does style them.
        let styled_rows = completion_rows("/mod", 0, &Theme::fx_dark(), true, usize::MAX);
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
            last_request: rune_net::stream::Usage::default(),
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
            last_request: rune_net::stream::Usage::default(),
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
            last_request: rune_net::stream::Usage::default(),
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
    fn an_anthropic_session_presents_its_key_the_way_anthropic_reads_it() {
        // Anthropic refuses a bearer token, so the session has to present its
        // key the way the one-shot runner does.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = Paths::resolve(Some(root.as_str()), None, None, None, None);
        paths.ensure_roots().expect("roots");
        crate::provider_setup::connect(&paths, "anthropic", "sk-ant-test").expect("credential");

        let settings = Settings {
            provider: rune_core::config::Provider::Anthropic,
            base_url: Some("https://example.invalid".to_owned()),
            model: "claude-test".to_owned(),
            ..Settings::default()
        };
        let config =
            prepare(&settings, &paths, Utf8Path::new("/tmp"), None).expect("a session prepares");
        assert_eq!(
            config.endpoint.auth,
            rune_net::transport::AuthStyle::ApiKeyHeader
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
        // A fresh session refuses web calls until the user enables them.
        assert_eq!(
            config
                .rules
                .evaluate("web_fetch", "https://example.com", Outcome::Allow)
                .outcome,
            Outcome::Deny,
            "the web tools were allowed without an explicit opt-in"
        );
        // Explicitly enabling web installs the allow above the built-in denial.
        let on = Settings {
            web_tools: true,
            ..settings.clone()
        };
        let on_config =
            prepare(&on, &paths, Utf8Path::new("/tmp"), None).expect("a session prepares");
        assert_eq!(
            on_config
                .rules
                .evaluate("web_fetch", "https://example.com", Outcome::Deny)
                .outcome,
            Outcome::Allow,
            "explicitly enabling the web tools did not allow them"
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
            questions: Arc::new(TerminalQuestions::default()),
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
            context_source: None,
            context_limit: rune_net::catalog::DEFAULT_CONTEXT_WINDOW,
            totals,
        }
    }

    fn test_host() -> SessionHost {
        let mut registry = Registry::new();
        registry
            .insert(Box::new(rune_tools::ReadFile::new()))
            .expect("registered");
        let questions = Arc::new(TerminalQuestions::default());
        SessionHost {
            recorder: None,
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
            cancellation: questions.cancellation.clone(),
            steering: SteeringQueue::new(4),
            events: Arc::new(Mutex::new(Vec::new())),
            context_used: std::sync::atomic::AtomicU64::new(0),
            context_source: Mutex::new(None),
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
            frame: Mutex::new(()),
            transcript: Mutex::new(Vec::new()),
            transcript_history_len: std::sync::atomic::AtomicUsize::new(0),
            transcript_open: std::sync::atomic::AtomicBool::new(false),
            typed: Mutex::new((String::new(), 0)),
            provider_order: Vec::new(),
            provider_strict: false,
            streaming: Arc::new(Mutex::new(StreamingText::default())),
            live_out: Arc::new(Mutex::new(None)),
            reasoning: Mutex::new("\u{1b}[2m".to_owned()),
            reviewer: None,
            totals: Mutex::new(Totals::default()),
            undo: Mutex::new(BTreeMap::new()),
            review_session: Arc::new(Mutex::new(ReviewSession::new(&BudgetSet::new()))),
            approval_requests: Mutex::new(None),
            questions,
        }
    }
}

#[cfg(test)]
mod send_probe {
    use super::SessionHost;
    #[test]
    fn session_host_can_cross_a_thread() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SessionHost>();
    }
}
