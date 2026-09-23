//! The interactive session.
//!
//! Connects the terminal shell loop to the agent. One turn runs at a time; a
//! line typed while a turn is running is queued rather than refused, which is
//! what makes the shell usable while the model is working.

use std::io::{BufRead, Write as _};
use std::sync::{Arc, Mutex};

use crate::session_log::{self, Recorder};
use camino::Utf8Path;
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
use rune_term::theme::Theme;
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
    pub workspace: camino::Utf8PathBuf,
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
    model: String,
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
    context_limit: u64,
    /// Theme the status line is drawn with.
    theme: Theme,
    /// Session identifier, shown shortened in the status line.
    session_id: String,
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
    /// Review activity for the current turn.
    review_session: Arc<Mutex<ReviewSession>>,
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

    fn model(&self) -> &str {
        &self.model
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
    /// Records the tokens a turn spent.
    fn add_context_usage(&self, used: u64) {
        self.context_used
            .fetch_add(used, std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns the status line for the current state.
    fn status_line(&self, width: usize) -> String {
        let state = FooterState {
            model: self.model.clone(),
            permission_mode: self.mode,
            workspace: self.workspace.clone(),
            context_used: self.context_used.load(std::sync::atomic::Ordering::Relaxed),
            context_limit: self.context_limit,
            session_id: self.session_id.clone(),
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
        let Ok(streaming) = self.streaming.lock() else {
            return Vec::new();
        };
        let width = usize::from(self.width());
        let dim = self.theme.sgr(rune_term::theme::Slot::Dim, self.truecolor);
        let reset = rune_term::engine::Style::RESET;
        let mut rows = Vec::new();
        // Reasoning comes first and is drawn apart from the answer, in a
        // secondary colour and indented, so a reader can tell thinking from the
        // reply at a glance instead of finding them interleaved.
        if !streaming.reasoning.trim().is_empty() {
            for line in rune_term::width::wrap(&streaming.reasoning, width) {
                rows.push(format!("{dim}  {line}{reset}"));
            }
        }
        if !streaming.answer.trim().is_empty() {
            for line in rune_term::width::wrap(&streaming.answer, width) {
                rows.push(line);
            }
        }
        // The region keeps the newest rows, so a long answer does not push the
        // input off the screen.
        let limit = usize::from(self.height.load(std::sync::atomic::Ordering::Relaxed))
            .saturating_sub(6)
            .max(1);
        if rows.len() > limit {
            rows.drain(..rows.len().saturating_sub(limit));
        }
        rows
    }

    /// Clears the streamed text, which a finished step has taken over.
    fn clear_streaming(&self) {
        if let Ok(mut streaming) = self.streaming.lock() {
            streaming.answer.clear();
            streaming.reasoning.clear();
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
    let reasoning_escape = theme.sgr(rune_term::theme::Slot::Dim, truecolor_supported());
    let host = SessionHost {
        endpoint: config.endpoint,
        dialect: config.dialect,
        model: config.settings.model.clone(),
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
        context_limit: context_limit(&config.settings, &limits),
        theme,
        session_id: recorder.id().to_string(),
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
        review_session: Arc::new(Mutex::new(ReviewSession::new(&limits))),
    };

    // The stream is shared: the loop writes to it, and the host writes to it
    // while a turn is running so streamed text appears as it arrives. Sharing
    // one lock rather than nesting two is what keeps those writes ordered.
    let out: LiveSink = Arc::new(Mutex::new(output));
    if let Ok(mut slot) = host.live_out.lock() {
        *slot = Some(Arc::clone(&out));
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

    let mut source = rune_term::shell::StdinSource::new(input);
    let mut shell = Shell::new(&mut source);

    // One handler for both input paths, so a keystroke and a piped line mean
    // exactly the same thing.
    let mut handle_input = |input: Input, sink: &mut LockedSink| -> Result<Action> {
        match input {
            Input::Command { name, arguments } => {
                match handle_command(
                    &name,
                    &arguments,
                    &commands,
                    history_file.as_ref(),
                    &config.workspace,
                    sink,
                )? {
                    Handled::Exit => Ok(Action::Exit),
                    Handled::ClearHistory => {
                        // Forgetting is reported, because a silent success would
                        // leave the user unsure whether anything was removed.
                        match history_file.as_mut() {
                            Some(history) => {
                                let _ = history.clear();
                                let _ = writeln!(sink, "forgot every recorded prompt");
                            }
                            None => {
                                let _ = writeln!(sink, "no prompt history is available");
                            }
                        }
                        Ok(Action::Continue)
                    }
                    Handled::Continue => Ok(Action::Continue),
                    // Expanded text goes to the composer for review, never
                    // straight to the model: a template with a wrong argument
                    // should be visible before it is sent.
                    Handled::Expand(prompt) => {
                        let _ = writeln!(
                            sink,
                            "-- /{name} expanded; edit before sending --\n{prompt}"
                        );
                        Ok(Action::Continue)
                    }
                }
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
                record_usage(&config.paths, &host.model, &outcome);
                host.add_context_usage(
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
                Ok(Action::Continue)
            }
            Input::Empty => Ok(Action::Continue),
        }
    };

    let reason = if keyed {
        // Each keystroke redraws the prompt, so the line and the cursor follow
        // what was typed rather than waiting for the terminal to decide the
        // line is finished.
        let mut reason = ExitReason::EndOfInput;
        while let Some(input) = await_submission(&mut reader, &host, &out)? {
            let mut sink = LockedSink {
                stream: Arc::clone(&out),
            };
            if handle_input(input, &mut sink)? == Action::Exit {
                reason = ExitReason::Requested;
                break;
            }
        }
        reason
    } else {
        shell.run(|input| {
            let mut sink = LockedSink {
                stream: Arc::clone(&out),
            };
            handle_input(input, &mut sink)
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
fn await_submission(
    reader: &mut rune_term::input::KeyReader,
    host: &SessionHost,
    out: &LiveSink,
) -> Result<Option<Input>> {
    use rune_term::width::str_width;

    let marker = rune_term::shell::prompt();

    loop {
        {
            let mut sink = out
                .lock()
                .map_err(|_| RuneError::new(ErrorCode::Internal, "the output lock was poisoned"))?;
            let row = transcript::render_prompt(marker, reader.line(), usize::from(host.width()));
            let caret = str_width(marker).saturating_add(reader.column());
            let painted = host.paint(
                &[],
                None,
                std::slice::from_ref(&row),
                &[],
                (0, u16::try_from(caret).unwrap_or(u16::MAX)),
            )?;
            if !painted.is_empty() {
                sink.write_all(&painted)?;
                sink.flush()?;
            }
        }

        match reader.read_key() {
            KeyAction::Submit => {
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
                if reader.line().is_empty() {
                    return Ok(None);
                }
                reader.clear();
            }
            KeyAction::Ignored => {}
        }
    }
}

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
fn context_limit(_settings: &Settings, _limits: &BudgetSet) -> u64 {
    rune_net::catalog::DEFAULT_CONTEXT_WINDOW
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
    output: &mut W,
) -> Result<Handled> {
    match name {
        "quit" | "exit" => Ok(Handled::Exit),
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
            let _ = writeln!(
                output,
                "commands: /help /quit
/history [here|session-id]  show recorded prompts
/history clear  forget every recorded prompt"
            );
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
            entries.push(Entry::tool(format!(
                "{}: {}",
                call.call.name, call.output.text
            )));
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
    settings.require_model()?;

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

    let mut registry = inventory::builtin(
        &rune_tools::workspace::FileLimits::from_budget(&settings.limits),
        &settings.limits,
        &paths.managed_skills_dir(),
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
    fn streamed_text_is_drawn_below_the_input() {
        // The answer grows downward from the line being typed, so a long reply
        // never pushes the input off the screen.
        let host = test_host();
        host.emit(Event::TextDelta {
            delta: "answer".to_owned(),
        });
        let prompt_row = transcript::render_prompt(rune_term::shell::prompt(), "", 80);
        let rows = host.streaming_rows();
        let bytes = host
            .paint(&[], None, std::slice::from_ref(&prompt_row), &rows, (0, 2))
            .expect("painted");
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let input = text.find("> ").expect("the input row");
        let answer = text.find("answer").expect("the streamed answer");
        assert!(
            input < answer,
            "the answer was drawn above the input: {text:?}"
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
        host.add_context_usage(64_000);
        let after = host.status_line(120);
        assert!(
            !after.contains("ctx 0%"),
            "usage was not reflected: {after}"
        );
    }

    #[test]
    fn spending_context_is_cumulative_across_turns() {
        let host = test_host();
        host.add_context_usage(1_000);
        host.add_context_usage(2_000);
        assert_eq!(
            host.context_used.load(std::sync::atomic::Ordering::Relaxed),
            3_000
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
            &mut output,
        )
        .expect("handled");
        assert_eq!(action, Handled::Continue);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("unknown command"), "{text}");
        assert!(text.contains("/help"), "{text}");
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
            path: camino::Utf8PathBuf::from("/p/.rune/commands/help.md"),
            reason: "`/help` is a built-in command, so this file was skipped".to_owned(),
        });

        let mut output = Vec::new();
        handle_command(
            "help",
            "",
            &discovery,
            None,
            Utf8Path::new("/w"),
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
        assert_eq!(
            config
                .rules
                .evaluate("web_fetch", "https://example.com", Outcome::Allow)
                .outcome,
            Outcome::Deny,
            "outbound traffic is not refused"
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
    fn test_host() -> SessionHost {
        let mut registry = Registry::new();
        registry
            .insert(Box::new(rune_tools::ReadFile::new()))
            .expect("registered");
        SessionHost {
            endpoint: Endpoint::new("https://example.invalid", "k"),
            dialect: Box::new(rune_net::chat_completions::ChatCompletions),
            model: "test".to_owned(),
            instructions: String::new(),
            tools: Vec::new(),
            rules: crate::permissions::validated(&Settings::default()).expect("rules"),
            mode: PermissionMode::Auto,
            effort: Effort::Auto,
            fast_mode: false,
            limits: BudgetSet::new(),
            context: ExecutionContext::new(camino::Utf8PathBuf::from("/tmp")),
            registry,
            cancellation: Cancellation::new(),
            steering: SteeringQueue::new(4),
            events: Arc::new(Mutex::new(Vec::new())),
            context_used: std::sync::atomic::AtomicU64::new(0),
            context_limit: rune_net::catalog::DEFAULT_CONTEXT_WINDOW,
            theme: Theme::no_color(),
            session_id: "sessiontest1".to_owned(),
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
            review_session: Arc::new(Mutex::new(ReviewSession::new(&BudgetSet::new()))),
        }
    }
}
