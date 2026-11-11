//! The `shell` tool: commands run under a shell and kept as sessions.
//!
//! A session belongs to the tool instance rather than to the call that started
//! it, so a command that outlives its call can be observed and stopped later.
//! Every session runs in its own process group, which is what lets a stop end a
//! whole tree rather than the one process Rune spawned. A command given
//! `timeout_ms` has its group killed at that deadline, whether or not a call is
//! still waiting on it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use camino::Utf8Path;
use rune_core::LimitName;
use rune_core::budget::BudgetSet;
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::contract::{
    Activity, CheckRun, ExecutionContext, Tool, ToolOutput, validate_closed_arguments,
};
use crate::workspace::{
    FileLimits, bool_arg, head_and_tail_split, resolve, string_arg, truncate_to_bytes, usize_arg,
};
use rune_exec::command::Exit;
use rune_exec::session::{Input, Leftovers, POLL_INTERVAL, Process, SpawnOptions, Streams};
use sha2::{Digest as _, Sha256};

/// Yield window a `run` uses when none is requested.
pub const DEFAULT_RUN_YIELD_MS: u64 = 30_000;

/// Yield window an `interact` uses when none is requested.
pub const DEFAULT_INTERACT_MS: u64 = 5_000;

/// Largest yield window a caller may request.
///
/// Bounded so one call cannot hold a turn open indefinitely; a command that
/// outlives its window is returned as a session instead. A command with
/// `timeout_ms` is still killed at its deadline after the call has returned.
pub const MAX_YIELD_MS: u64 = 600_000;

/// Largest `timeout_ms` a caller may give.
const MAX_TIMEOUT_MS: u64 = 3_600_000;

/// Time spent collecting the last bytes a command writes as it exits.
const DRAIN_WINDOW: Duration = Duration::from_millis(200);

/// Finished commands whose background processes are kept; past this, the
/// oldest group is ended.
const MAX_LEFTOVERS: usize = 16;

/// Line added after the state of a finished command whose group still runs.
const BACKGROUND_NOTICE: &str =
    "[processes started by this command are still running in the background]";

/// Bounds on the time an `interact` waits for a session to take its input.
///
/// The wait follows the call's own window, so a caller that asked to return at
/// once still gives a small write the moment it needs, and one that asked to
/// wait long cannot be held past the bound by a command that never reads.
const MIN_INPUT_WINDOW: Duration = Duration::from_millis(500);
const MAX_INPUT_WINDOW: Duration = Duration::from_secs(10);

/// Permission target of a `stop`.
///
/// Ending a session changes nothing the command did not already start, so the
/// target names the action rather than the command, and one rule can answer
/// for every stop.
pub const STOP_TARGET: &str = "stop";

/// The command each live session runs, by session id, across every instance
/// of the tool in this process.
///
/// A permission target is read from a call's arguments by whichever view of the
/// tool the caller holds, which need not be the instance that owns the session.
/// Input sent to a session is judged together with the command it goes to, so
/// that command is kept where every view can read it.
static COMMANDS: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

/// Idle interactions in a row after which each one carries [`IDLE_HINT`].
const IDLE_POLLS_BEFORE_HINT: u32 = 4;
/// Added to an interaction with a session that keeps producing nothing.
const IDLE_HINT: &str =
    "[the process has produced no new output; wait longer with a larger yield_time_ms, or stop it]";

/// Sessions started in this process, counted across every instance so that no
/// two sessions share an id.
static STARTED: AtomicU64 = AtomicU64::new(0);

/// Background work started by any shell call in this process: sessions that
/// outlived their call and groups left running after a command exited.
static BACKGROUND: Mutex<Background> = Mutex::new(Background {
    watched: Vec::new(),
    generation: 0,
});

/// What [`BACKGROUND`] watches, and a number that changes with it.
struct Background {
    watched: Vec<Watched>,
    generation: u64,
}

/// One piece of background work.
enum Watched {
    Session(std::sync::Weak<Session>),
    Group(u32),
}

impl Watched {
    fn alive(&self) -> bool {
        match self {
            Self::Session(session) => session
                .upgrade()
                .is_some_and(|session| session.process.is_running()),
            Self::Group(group) => rune_exec::session::group_alive(*group),
        }
    }
}

/// Adds background work to the watch list.
fn watch(watched: Watched) {
    let mut background = lock(&BACKGROUND);
    background.watched.push(watched);
    background.generation = background.generation.saturating_add(1);
}

/// How much background work is alive, and a number that changes whenever some
/// starts or ends. Ending is noticed when this is called.
///
/// The list is shared by every conversation in the process, so background work
/// in one also counts for the others.
pub fn background() -> (usize, u64) {
    let mut background = lock(&BACKGROUND);
    let before = background.watched.len();
    background.watched.retain(Watched::alive);
    let ended = before.saturating_sub(background.watched.len());
    background.generation = background
        .generation
        .saturating_add(u64::try_from(ended).unwrap_or(u64::MAX));
    (background.watched.len(), background.generation)
}

/// Runs commands and holds the ones that outlive their call.
///
/// Background processes a finished command started, such as `server &`, keep
/// running until the tool is dropped or the run ends.
#[derive(Debug)]
pub struct Shell {
    limits: FileLimits,
    max_sessions: usize,
    state: Mutex<State>,
    /// The last log of each test command that finished in its call, so a
    /// repeat can be shown as what changed.
    runs: Mutex<BTreeMap<String, String>>,
}

/// Test commands whose last log is kept for a delta.
const MAX_REMEMBERED_RUNS: usize = 8;

/// Returns what a stop on this session will end.
///
/// A group is what a session is started in where the platform has them; where it
/// does not, the session's own process is what a stop reaches, and its tree is
/// ended from there.
fn ended_by(process: u32) -> String {
    if cfg!(unix) {
        format!("process group {process}")
    } else {
        format!("process tree {process}")
    }
}

/// The sessions one tool instance owns.
#[derive(Debug, Default)]
struct State {
    sessions: BTreeMap<String, Arc<Session>>,
    /// Commands that are starting but do not yet have a session entry.
    starting: usize,
    /// Finished commands whose groups still have processes running, oldest
    /// first.
    leftovers: Vec<Leftover>,
}

/// A finished command whose group still has processes running.
#[derive(Debug)]
enum Leftover {
    /// A command that finished within its `run`.
    Run(Process),
    /// A session that ended.
    Session(Arc<Session>),
}

impl Leftover {
    /// Returns the process whose group is kept.
    fn process(&self) -> &Process {
        match self {
            Self::Run(process) => process,
            Self::Session(session) => &session.process,
        }
    }
}

/// A slot in the session bound, held while a command starts.
///
/// The slot is reserved before the command is spawned and handed to the session
/// it becomes, so two calls racing for the last slot cannot both win it and a
/// start that fails or is cancelled gives its slot back.
#[derive(Debug)]
struct Slot<'a> {
    shell: &'a Shell,
    held: bool,
}

impl<'a> Slot<'a> {
    /// Reserves a slot and mints the session id, or reports the bound is full.
    ///
    /// Completed sessions keep their unread output until observed, but only
    /// running processes and reserved starts occupy the bound.
    fn reserve(shell: &'a Shell) -> Result<(Self, String)> {
        let mut state = lock(&shell.state);
        // Bound retained output as well as running processes. Never evict unread output.
        if state.sessions.len().saturating_add(state.starting)
            >= shell.max_sessions.saturating_mul(2)
        {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                "shell session retention limit reached",
            )
            .with_hint("poll or stop a retained session before starting another"));
        }
        let live = state
            .sessions
            .values()
            .filter(|held| held.process.is_running())
            .count()
            .saturating_add(state.starting);
        if live >= shell.max_sessions {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                format!(
                    "{live} sessions are already running, the limit is {}",
                    shell.max_sessions
                ),
            )
            .with_hint("stop a session before starting another"));
        }
        state.starting = state.starting.saturating_add(1);
        let started = STARTED.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        Ok((Self { shell, held: true }, session_id(started)))
    }

    /// Hands the slot to the session that now occupies it.
    fn hand_over(&mut self, id: String, session: Arc<Session>) {
        let mut state = lock(&self.shell.state);
        if self.held {
            state.starting = state.starting.saturating_sub(1);
            self.held = false;
        }
        state.sessions.insert(id, session);
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        if self.held {
            let mut state = lock(&self.shell.state);
            state.starting = state.starting.saturating_sub(1);
        }
    }
}

/// One running command.
///
/// Only the reader positions need guarding, and they are held for a moment at a
/// time. Whether the command is still running is answered by the process itself,
/// so a call that waits out its window never blocks another call's work.
#[derive(Debug)]
struct Session {
    id: String,
    command: String,
    process: Process,
    seen: Mutex<(usize, usize)>,
    input: Input,
    /// Whether the sandbox kept this command off the network.
    network_denied: bool,
    /// The deadline the command was given, in milliseconds.
    timeout_ms: Option<u64>,
    /// Interactions in a row that sent nothing and saw no new output.
    idle_polls: AtomicU32,
}

impl Session {
    /// Builds a session and records the command it runs under its id.
    fn new(
        id: String,
        command: String,
        process: Process,
        seen: (usize, usize),
        input: Input,
        network_denied: bool,
    ) -> Self {
        let _ = lock(&COMMANDS).insert(id.clone(), command.clone());
        Self {
            id,
            command,
            process,
            seen: Mutex::new(seen),
            input,
            network_denied,
            timeout_ms: None,
            idle_polls: AtomicU32::new(0),
        }
    }

    /// Returns the state line of a session that has ended this way, or its
    /// timeout line when its deadline killed it.
    fn ended_state(&self, exit: Exit) -> String {
        match self.timeout_ms {
            Some(timeout) if self.process.timed_out() => timeout_state(timeout),
            _ => format!("session {} ended: {}", self.id, exit.describe()),
        }
    }

    /// Returns the exit a result is judged by: the timeout status when the
    /// deadline killed the command.
    fn judged_exit(&self, exit: Exit) -> Exit {
        if self.process.timed_out() {
            Exit::Code(TIMEOUT_STATUS)
        } else {
            exit
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = lock(&COMMANDS).remove(&self.id);
    }
}

/// Returns the command a live session runs.
fn session_command(id: &str) -> Option<String> {
    lock(&COMMANDS).get(id).cloned()
}

/// The action one call performs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Run,
    Interact,
    Stop,
}

impl Action {
    /// Parses an action name.
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "run" => Ok(Self::Run),
            "interact" => Ok(Self::Interact),
            "stop" => Ok(Self::Stop),
            other => Err(
                RuneError::invalid_field("action", format!("`{other}` is not an action"))
                    .with_hint("use `run`, `interact`, or `stop`"),
            ),
        }
    }
}

impl Shell {
    /// Builds the tool with the caps and the session bound from a limit set.
    #[must_use]
    pub fn new(budget: &BudgetSet) -> Self {
        Self {
            limits: FileLimits::from_budget(budget),
            max_sessions: budget.get_usize(LimitName::ParallelToolCalls).max(1),
            state: Mutex::new(State::default()),
            runs: Mutex::new(BTreeMap::new()),
        }
    }

    /// Returns the number of sessions currently held.
    #[must_use]
    pub fn live_sessions(&self) -> usize {
        lock(&self.state).sessions.len()
    }

    /// Keeps a finished command whose group still has processes running, and
    /// says so after the result's state line. Past the bound, the oldest kept
    /// group is ended.
    fn keep_leftovers(&self, leftover: Leftover, text: &mut String) {
        if !leftover.process().group_alive() {
            return;
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(BACKGROUND_NOTICE);
        text.push('\n');
        watch(Watched::Group(leftover.process().id()));
        let evicted = {
            let mut state = lock(&self.state);
            let evicted = if state.leftovers.len() >= MAX_LEFTOVERS {
                Some(state.leftovers.remove(0))
            } else {
                None
            };
            state.leftovers.push(leftover);
            evicted
        };
        if let Some(evicted) = evicted {
            evicted.process().end_leftovers();
        }
    }

    /// Starts a command and returns either its finished output or a session.
    fn run(&self, arguments: &serde_json::Value, context: &ExecutionContext) -> Result<ToolOutput> {
        let command = string_arg(arguments, "command")?
            .ok_or_else(|| RuneError::missing_field("command"))?
            .to_owned();
        if command.trim().is_empty() {
            return Err(RuneError::invalid_field("command", "the command is empty"));
        }
        let timeout = timeout_ms(arguments)?;
        // A command with a deadline is waited on until just past it, so its
        // timeout is usually reported by the call that started it.
        let window = match timeout {
            Some(timeout) if usize_arg(arguments, "yield_time_ms")?.is_none() => {
                Duration::from_millis(timeout.saturating_add(1_000).min(MAX_YIELD_MS))
            }
            _ => yield_window(arguments, DEFAULT_RUN_YIELD_MS)?,
        };
        let cwd = match string_arg(arguments, "cwd")? {
            Some(raw) => Some(resolve(context, raw)?.path),
            None => None,
        };
        let dimensions = terminal_dimensions(arguments)?;
        let input = if bool_arg(arguments, "pty")?.unwrap_or(false) {
            if !cfg!(target_os = "linux") {
                return Err(RuneError::new(
                    ErrorCode::Unsupported,
                    "PTY shell mode is only supported on Linux",
                )
                .with_hint("use `interactive` for piped input without a terminal"));
            }
            let (rows, cols) = dimensions.unwrap_or((24, 80));
            Input::Pty { rows, cols }
        } else if dimensions.is_some() {
            return Err(RuneError::invalid_field(
                "pty",
                "rows and cols require PTY mode",
            ));
        } else if bool_arg(arguments, "interactive")?.unwrap_or(false) {
            Input::Piped
        } else {
            Input::Closed
        };
        let cap = self.limits.output_cap(context);
        // Kept groups whose processes have all ended need no more holding.
        let ended: Vec<Leftover> = {
            let mut state = lock(&self.state);
            let (alive, ended) = std::mem::take(&mut state.leftovers)
                .into_iter()
                .partition(|leftover| leftover.process().group_alive());
            state.leftovers = alive;
            ended
        };
        drop(ended);
        // The slot is reserved before the command is spawned, so two starts
        // racing for the last one cannot both win it, and a start that fails or
        // returns a finished command gives the slot back.
        let (mut slot, id) = Slot::reserve(self)?;

        // The start happens outside the map lock, because a command that runs
        // for its whole yield window would otherwise hold every other call up.
        let (process, network_denied) = start(&command, cwd.as_deref(), cap, context, input)?;
        if let Some(timeout) = timeout {
            process
                .kill_after(Duration::from_millis(timeout))
                .map_err(RuneError::from)?;
        }
        let waited = wait_for_exit(&process, window, context);
        if let Err(err) = &waited
            && err.code() == ErrorCode::Cancelled
        {
            process.terminate(true);
        }
        if let Some(exit) = waited? {
            process.drain_output(DRAIN_WINDOW);
            let (state, exit) = match timeout {
                Some(timeout) if process.timed_out() => {
                    (timeout_state(timeout), Exit::Code(TIMEOUT_STATUS))
                }
                _ => (exit.describe(), exit),
            };
            let (mut text, full_text, check) = observe(
                &process,
                &command,
                &state,
                &mut (0, 0),
                cap,
                Some(&self.runs),
            );
            self.keep_leftovers(Leftover::Run(process), &mut text);
            let mut output = finish(text, exit, network_denied);
            output.full_text = full_text;
            output.check = check.map(|check| with_exit(check, exit));
            return Ok(output);
        }

        // This first result is the one most easily read as a failure, so it
        // says plainly what to do next.
        let state = format!(
            "still running after {} s as session {id} ({}). Do not run the command again. \
             Use action \"interact\" with this session_id to wait for more output, or action \
             \"stop\" to end it.",
            seconds(u64::try_from(window.as_millis()).unwrap_or(u64::MAX)),
            ended_by(process.id())
        );
        let mut seen = (0, 0);
        let (text, _, _) = observe(&process, &command, &state, &mut seen, cap, None);
        let mut session = Session::new(id.clone(), command, process, seen, input, network_denied);
        session.timeout_ms = timeout;
        let session = Arc::new(session);
        watch(Watched::Session(Arc::downgrade(&session)));
        slot.hand_over(id, session);
        Ok(ToolOutput::success(text))
    }

    /// Reads what a session produced since the last observation, sending the
    /// caller's input first when there is any.
    fn interact(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        let id = string_arg(arguments, "session_id")?
            .ok_or_else(|| RuneError::missing_field("session_id"))?
            .to_owned();
        let chars = string_arg(arguments, "chars")?;
        let dimensions = terminal_dimensions(arguments)?;
        let window = yield_window(arguments, DEFAULT_INTERACT_MS)?;
        let cap = self.limits.output_cap(context);
        let held = {
            let state = lock(&self.state);
            state
                .sessions
                .get(&id)
                .map(Arc::clone)
                .ok_or_else(|| unknown_session(&state, &id))?
        };
        let session = held.as_ref();
        // The baseline is taken before the input is written, so a command that
        // answers straight away satisfies the wait rather than being waited on
        // for the whole window.
        let baseline = produced(&session.process);
        let interaction = (|| {
            if let Some((rows, cols)) = dimensions {
                session.process.resize(rows, cols).map_err(|err| {
                    RuneError::new(
                        ErrorCode::InvalidState,
                        format!("session `{id}` could not resize: {err}"),
                    )
                })?;
            }
            if let Some(chars) = chars {
                if session.input == Input::Closed {
                    return Err(RuneError::new(
                        ErrorCode::InvalidState,
                        format!("session `{id}` was started without input"),
                    )
                    .with_hint("start the command with `interactive` set to send it input"));
                }
                let limit = window.clamp(MIN_INPUT_WINDOW, MAX_INPUT_WINDOW);
                session
                    .process
                    .write(chars.as_bytes(), limit)
                    .map_err(|err| {
                        RuneError::new(
                            ErrorCode::InvalidState,
                            format!("session `{id}` did not accept input: {err}"),
                        )
                    })?;
            }

            if session.process.exit().is_none() {
                wait_for_progress(&session.process, window, baseline, context)?;
            }
            if matches!(session.input, Input::Pty { .. }) {
                context.check_cancelled()?;
            }
            Ok(())
        })();
        if let Err(err) = interaction {
            if context.is_cancelled() && matches!(session.input, Input::Pty { .. }) {
                session.process.terminate(true);
                let _ = lock(&self.state).sessions.remove(&id);
            }
            return Err(err);
        }
        let ended = session.process.exit();
        if ended.is_some() {
            session.process.drain_output(DRAIN_WINDOW);
        }
        let idle = chars.is_none() && ended.is_none() && produced(&session.process) == baseline;
        let idle_polls = if idle {
            session
                .idle_polls
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1)
        } else {
            session.idle_polls.store(0, Ordering::Relaxed);
            0
        };
        let state = match ended {
            Some(exit) => session.ended_state(exit),
            None => format!("session {id} running, {}", ended_by(session.process.id())),
        };
        let mut seen = lock(&session.seen);
        let (mut text, _, check) = observe(
            &session.process,
            &session.command,
            &state,
            &mut seen,
            cap,
            None,
        );
        if let Some(exit) = ended {
            // A session that has ended is observed once more and then dropped,
            // so a later call cannot report output that was already returned.
            let _ = lock(&self.state).sessions.remove(&id);
            self.keep_leftovers(Leftover::Session(Arc::clone(&held)), &mut text);
            let exit = session.judged_exit(exit);
            let mut output = finish(text, exit, session.network_denied);
            output.check = check.map(|check| with_exit(check, exit));
            return Ok(output);
        }
        if idle_polls >= IDLE_POLLS_BEFORE_HINT {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(IDLE_HINT);
        }
        Ok(ToolOutput::success(text))
    }

    /// Ends a session and reports how it ended.
    fn stop(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        let id = string_arg(arguments, "session_id")?
            .ok_or_else(|| RuneError::missing_field("session_id"))?
            .to_owned();
        let force = bool_arg(arguments, "force")?.unwrap_or(false);
        let cap = self.limits.output_cap(context);
        let held = {
            let mut state = lock(&self.state);
            state
                .sessions
                .remove(&id)
                .ok_or_else(|| unknown_session(&state, &id))?
        };
        // The session is no longer reachable, so a concurrent call that already
        // took it finishes before this one waits for the group to end.
        let session = held.as_ref();
        let ended = session.process.terminate(force);
        session.process.drain_output(DRAIN_WINDOW);
        let state = match session.process.exit() {
            Some(exit) => session.ended_state(exit),
            None => format!("session {id} survived the grace period"),
        };
        let mut seen = lock(&session.seen);
        let (mut text, _, _) = observe(
            &session.process,
            &session.command,
            &state,
            &mut seen,
            cap,
            None,
        );
        if session.process.exit().is_some() {
            self.keep_leftovers(Leftover::Session(Arc::clone(&held)), &mut text);
        }
        // A stop that ended the group succeeded even though the command was
        // killed: the signal is how it was asked to stop, not a failure of the
        // stop itself, and the status is stated in the result. A command its
        // deadline had already killed is reported as the failure it was.
        if ended && !session.process.timed_out() {
            Ok(ToolOutput::success(text))
        } else {
            Ok(ToolOutput::failure(text))
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new(&BudgetSet::new())
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        // Sessions end their groups when they are dropped; kept groups of
        // finished commands are ended here.
        let leftovers = std::mem::take(&mut lock(&self.state).leftovers);
        for leftover in &leftovers {
            leftover.process().end_leftovers();
        }
    }
}

impl Tool for Shell {
    fn name(&self) -> &'static str {
        "shell"
    }

    /// Stops every session this tool started, each asked to exit and killed
    /// after the grace period. They are stopped together, so many sessions
    /// take one grace period rather than one each.
    fn shutdown(&self) {
        let (sessions, leftovers): (Vec<Arc<Session>>, Vec<Leftover>) = {
            let mut state = lock(&self.state);
            (
                std::mem::take(&mut state.sessions).into_values().collect(),
                std::mem::take(&mut state.leftovers),
            )
        };
        std::thread::scope(|scope| {
            for session in &sessions {
                scope.spawn(|| session.process.terminate(false));
            }
        });
        // A leader that ended leaves its kept group behind, and so does a
        // finished command that started background processes.
        for session in &sessions {
            session.process.end_leftovers();
        }
        for leftover in &leftovers {
            leftover.process().end_leftovers();
        }
    }

    fn description(&self) -> &'static str {
        "Run a shell command. `run` returns the output and exit status, or a session_id if \
         it is still running after yield_time_ms; never rerun a running command. `interact` \
         reads new output and sends chars; `stop` ends it."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["run", "interact", "stop"],
                },
                "command": {
                    "type": "string",
                    "description": "Required by `run`.",
                },
                "cwd": {
                    "type": "string",
                    "description": "Defaults to the workspace root.",
                },
                "interactive": {
                    "type": "boolean",
                    "description": "Keep standard input open.",
                },
                "pty": {
                    "type": "boolean",
                    "description": "Linux only: run under a terminal.",
                },
                "rows": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 65535,
                },
                "cols": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 65535,
                },
                "session_id": {
                    "type": "string",
                    "description": "From `run`.",
                },
                "chars": {
                    "type": "string",
                    "description": "Input to send, newline included.",
                },
                "yield_time_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_YIELD_MS,
                    "description": "Wait before returning.",
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT_MS,
                    "description": "Kill the command after this long.",
                },
                "force": {
                    "type": "boolean",
                    "description": "Kill at once.",
                },
            },
            "required": ["action"],
            "additionalProperties": false,
        })
    }

    fn activity(&self) -> Activity {
        Activity::Execute
    }

    /// Returns what a call is judged by.
    ///
    /// A `run` is judged by its command. Input sent by `interact` is judged
    /// with the command it goes to, one line after it, so a rule that refuses a
    /// command also refuses typing it into a running shell; a `command`
    /// argument passed with it is not what the session runs and is ignored. A
    /// `stop` only ends what was already started, so every stop shares one
    /// target.
    fn permission_target(&self, arguments: &serde_json::Value) -> Option<String> {
        let text = |name: &str| arguments.get(name).and_then(serde_json::Value::as_str);
        match text("action") {
            Some("interact") => {
                let id = text("session_id").unwrap_or_default();
                // A session this process does not hold refuses the call, so its
                // id stands in for the command it would have named.
                let command = session_command(id).unwrap_or_else(|| id.to_owned());
                Some(match text("chars").filter(|chars| !chars.is_empty()) {
                    Some(chars) => format!("{command}\n{chars}"),
                    None => command,
                })
            }
            Some("stop") => Some(STOP_TARGET.to_owned()),
            _ => text("command").map(str::to_owned),
        }
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        validate_closed_arguments(arguments, &self.input_schema())?;
        let action = Action::parse(
            string_arg(arguments, "action")?.ok_or_else(|| RuneError::missing_field("action"))?,
        )?;
        if action != Action::Run && arguments.get("timeout_ms").is_some() {
            return Err(RuneError::invalid_field(
                "timeout_ms",
                "applies only to `run`",
            ));
        }
        match action {
            Action::Run => self.run(arguments, context),
            Action::Interact => self.interact(arguments, context),
            Action::Stop => self.stop(arguments, context),
        }
    }
}

/// Parses a complete terminal size, refusing partial or out-of-range values.
fn terminal_dimensions(arguments: &serde_json::Value) -> Result<Option<(u16, u16)>> {
    let rows = usize_arg(arguments, "rows")?;
    let cols = usize_arg(arguments, "cols")?;
    match (rows, cols) {
        (None, None) => Ok(None),
        (Some(rows), Some(cols)) => {
            let dimension = |value, name| {
                u16::try_from(value)
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or_else(|| {
                        RuneError::invalid_field(
                            name,
                            "terminal dimensions must be between 1 and 65535",
                        )
                    })
            };
            Ok(Some((dimension(rows, "rows")?, dimension(cols, "cols")?)))
        }
        _ => Err(RuneError::invalid_field(
            "rows",
            "rows and cols must be supplied together",
        )),
    }
}

/// Mints the id for one session.
///
/// The process id is part of the name, so a transcript from one run cannot be
/// mistaken for a session another run holds.
fn session_id(started: u64) -> String {
    format!("shell-{}-{started}", std::process::id())
}

/// Carries the context's grants and offline restriction into the sandbox.
pub fn sandbox_policy(context: &ExecutionContext) -> rune_exec::SandboxPolicy {
    rune_exec::SandboxPolicy::new(
        context.workspace.clone(),
        context.additional_roots.clone(),
        (context.external_access || context.network) && !context.offline,
    )
    .with_cache_roots(context.cache_roots.clone())
}

/// Added once to a failed command's output when the sandbox kept it off the
/// network and its output looks like a failed connection.
const NETWORK_NOTICE: &str =
    "[commands in this run have no network access; the user can allow it with --network]";

/// What a failed connection prints, in the tools agents run most.
const NETWORK_FAILURES: &[&str] = &[
    "could not resolve host",
    "temporary failure in name resolution",
    "getaddrinfo",
    "enotfound",
    "eai_again",
    "network is unreachable",
    "failed to connect",
];

/// Returns the command to run, wrapped by `backend` unless the context says no
/// sandbox applies.
///
/// The decision is split out from [`start`] so both the confined and the
/// unconfined path can be exercised with an injected backend, without depending
/// on whether the host has a sandbox helper installed.
fn confined(
    prepared: &rune_exec::PreparedCommand,
    policy: &rune_exec::SandboxPolicy,
    context: &ExecutionContext,
    input: Input,
    backend: &dyn rune_exec::Sandbox,
) -> Result<rune_exec::PreparedCommand> {
    if !context.sandboxed {
        return Ok(prepared.clone());
    }
    if matches!(input, Input::Pty { .. }) {
        backend.wrap_pty(prepared, policy, context.allow_unsandboxed)
    } else {
        backend.wrap(prepared, policy, context.allow_unsandboxed)
    }
}

/// Starts a command in a process group of its own, under the host sandbox.
///
/// The command string is prepared first so the argv that runs is the one that
/// was reviewed, then wrapped by the sandbox. A host with no usable backend
/// refuses rather than running the command unrestricted, because a sandbox that
/// silently does nothing is worse than one that is absent.
fn start(
    command: &str,
    cwd: Option<&Utf8Path>,
    cap: usize,
    context: &ExecutionContext,
    input: Input,
) -> Result<(Process, bool)> {
    let workspace = cwd.unwrap_or(&context.workspace);
    // The shell tool exists to run shell commands, so its input always goes to a
    // shell: routing a bare word to direct argv would break builtins such as
    // `exit`.
    // The environment is built here rather than inherited: this process holds
    // the provider credential, and a command that reaches the network must not
    // be able to read it back out of its own environment. A command without a
    // terminal is told so, and pagers and prompts are off.
    let environment =
        rune_exec::command_environment(&context.environment, matches!(input, Input::Pty { .. }));
    let prepared =
        rune_exec::command::prepare_shell(command, workspace, None, environment.clone())?;
    let policy = sandbox_policy(context);
    // Reaching outside the workspace and skipping the sandbox are different
    // questions, so the second is read from its own field rather than borrowed
    // from the first. A context that is not unconfined still goes through the
    // host backend, which is the path production takes.
    let backend = rune_exec::detect();
    let wrapped = confined(&prepared, &policy, context, input, &*backend)?;
    // Only a command the sandbox actually confines without network can blame
    // a failed connection on it; the unsandboxed fallback had network.
    let network_denied = context.sandboxed && !policy.network && backend.support().is_full();

    // The working directory is the resolved workspace, which is the path the
    // sandbox rule was built from. Starting in the path as written would leave
    // the process in a directory the rule does not name, and on a host where a
    // temporary directory resolves elsewhere the command could not write
    // anything at all.
    //
    // A directory that cannot be resolved is reported as missing here rather
    // than left to the spawn. A spawn failure for a working directory is
    // reported by the platform as a generic failure, which loses the fact that
    // the caller named a directory that is not there.
    let start_in = workspace.canonicalize_utf8().map_err(|err| {
        let code = if err.kind() == std::io::ErrorKind::NotFound {
            ErrorCode::NotFound
        } else {
            ErrorCode::InvalidField
        };
        RuneError::new(code, format!("`{workspace}` cannot be started in: {err}"))
    })?;
    Process::start_argv_with(
        &wrapped.argv,
        Some(start_in.as_std_path()),
        cap,
        &environment,
        input,
        SpawnOptions {
            streams: Streams::Merged,
            leftovers: Leftovers::Keep,
        },
    )
    .map_err(|err| {
        let hint = match cwd {
            Some(cwd) => format!("the command starts in `{cwd}`"),
            None => String::from("the command starts in the workspace root"),
        };
        RuneError::from(err).with_hint(hint)
    })
    .map(|process| (process, network_denied))
}

/// Returns the total bytes both streams have produced.
fn produced(process: &Process) -> u64 {
    process
        .stdout()
        .totals()
        .1
        .saturating_add(process.stderr().totals().1)
}

/// Renders one observation of a session.
///
/// The command is echoed byte for byte, so what the caller reads is the exact
/// string the shell received rather than a rendering of it. Bytes the caller
/// has already seen are not repeated.
///
/// With `runs`, a finished test run that repeats an earlier one is shown as
/// what changed, and the complete rendering is returned beside it. An ended
/// command also yields what it reported, with the exit status the process
/// gave; a caller that judges the exit differently replaces it.
fn observe(
    process: &Process,
    command: &str,
    state: &str,
    seen: &mut (usize, usize),
    cap: usize,
    runs: Option<&Mutex<BTreeMap<String, String>>>,
) -> (String, Option<String>, Option<CheckRun>) {
    // Standard error shares the standard output pipe, so this one stream holds
    // everything in the order the command wrote it.
    let stdout = process.stdout().since(seen.0);
    // An ended command is observed for the last time, so the newest bytes of a
    // stream that outgrew its head follow that head, once.
    let exit = process.exit();
    let mut body = String::from_utf8_lossy(&stdout.bytes).into_owned();
    let mut full = None;
    let mut check = None;
    if let Some(exit) = exit {
        push_tail(&mut body, process.stdout());
        check = Some(with_exit(check_run(&body), exit));
        // A finished test run leads with its verdict, which the cut below
        // always keeps, because the start of the output survives it.
        if let Some(counts) = crate::test_report::parse(&body) {
            let shown = runs.and_then(|runs| {
                let mut runs = lock(runs);
                let shown = runs
                    .get(command)
                    .and_then(|previous| crate::test_report::delta(previous, &body));
                if runs.len() >= MAX_REMEMBERED_RUNS && !runs.contains_key(command) {
                    runs.pop_first();
                }
                runs.insert(command.to_owned(), body.clone());
                shown
            });
            let header = format!("[tests: {counts}]\n");
            if let Some(shown) = shown {
                full = Some(format!("{header}{body}"));
                body = format!("{header}{shown}");
            } else {
                body = format!("{header}{body}");
            }
        }
    }
    seen.0 = stdout.next;

    // A capture that stopped at the cap says so, because the bytes the command
    // printed and the bytes retained are then different numbers, and only the
    // pair makes the shortfall visible. The comparison is the retained head
    // against total production; what a caller has not yet read says nothing
    // about the capture.
    let (retained, total) = process.stdout().totals();
    let note = if total > retained as u64 {
        format!(
            "[output capture stopped after {retained} bytes, the command produced {total} bytes]"
        )
    } else {
        String::new()
    };
    let failed = process.exit().is_some_and(|exit| !exit.is_success()) || process.timed_out();
    let full = full.map(|full| render(command, full, &note, state, usize::MAX, failed));
    (
        render(command, body, &note, state, cap, failed),
        full,
        check,
    )
}

/// Most failing test names a check run keeps.
const MAX_CHECK_FAILING: usize = 10;

/// Lines of output a check run's signature is taken from when no failing
/// test is named.
const SIGNATURE_LINES: usize = 20;

/// Reads what a finished command's output reports, without its exit status.
fn check_run(body: &str) -> CheckRun {
    let failed_tests = crate::test_report::parse(body).map(|counts| counts.failed);
    let mut names = crate::test_report::failures(body);
    let signature = if names.is_empty() {
        let lines: Vec<&str> = body.lines().collect();
        let tail = lines
            .get(lines.len().saturating_sub(SIGNATURE_LINES)..)
            .unwrap_or_default()
            .join("\n");
        let digest = Sha256::digest(tail.as_bytes());
        digest.iter().take(8).fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
    } else {
        let mut sorted = names.clone();
        sorted.sort();
        sorted.join("\n")
    };
    names.truncate(MAX_CHECK_FAILING);
    CheckRun {
        exit_code: None,
        failed_tests,
        failing: names,
        signature,
    }
}

/// Sets a check run's exit status: the code of a command that exited, else none.
fn with_exit(check: CheckRun, exit: Exit) -> CheckRun {
    CheckRun {
        exit_code: match exit {
            Exit::Code(code) => Some(code),
            _ => None,
        },
        ..check
    }
}

/// Appends a stream's newest bytes past its retained head, with the gap.
fn push_tail(body: &mut String, stream: &rune_exec::Buffer) {
    let (tail, skipped) = stream.tail();
    if tail.is_empty() {
        return;
    }
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    let _ = writeln!(body, "[{skipped} bytes not retained]");
    body.push_str(&String::from_utf8_lossy(&tail));
}

/// Renders the command, the bytes it produced, and the state it ended in.
///
/// The bytes are cut to the cap, and the cut is stated with the number of bytes
/// retained, because a result that is merely short looks like a command that
/// printed nothing. A cut keeps the start and the end and drops the middle:
/// a build names its first error at the top, and a test run puts its failures
/// and its summary at the bottom. A command that failed keeps more of its end,
/// where the error usually is.
fn render(
    command: &str,
    mut body: String,
    note: &str,
    state: &str,
    cap: usize,
    failed: bool,
) -> String {
    if cap == 0 {
        return String::new();
    }
    let head = format!("$ {command}\n");
    let state = format!("[{state}]\n");
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }

    // Room is allocated in order of what cannot be recovered later: what was
    // run, where the session stands, then the fact that output was cut. The
    // note about the capture is commentary on the output, so it gives up its
    // room rather than crowding out the cut itself.
    let available = cap.saturating_sub(head.len()).saturating_sub(state.len());
    let worst_marker = marker(0, body.len(), cap).len();
    let note = if note.is_empty()
        || available
            < note
                .len()
                .saturating_add(worst_marker)
                .saturating_add(MIN_NOTE_BODY_BYTES)
    {
        String::new()
    } else {
        format!("{note}\n")
    };

    let mut out = head;
    let room = available.saturating_sub(note.len());
    if body.len() <= room {
        out.push_str(&body);
    } else {
        let total = body.len();
        let share = if failed { 5 } else { 3 };
        let mut kept = room;
        // The marker states how many bytes were kept, so its own length depends
        // on the answer. Two passes settle it; the loop is a bound, not a guess.
        for _ in 0..8 {
            let (start, end) = head_and_tail_split(
                &body,
                room.saturating_sub(marker(kept, total, cap).len()),
                share,
            );
            let settled = start.len().saturating_add(end.len());
            if settled == kept {
                break;
            }
            kept = settled;
        }
        let (start, end) = head_and_tail_split(
            &body,
            room.saturating_sub(marker(kept, total, cap).len()),
            share,
        );
        out.push_str(start);
        if room > kept {
            out.push_str(&marker(kept, total, cap));
        }
        out.push_str(end);
    }
    out.push_str(&note);
    out.push_str(&state);
    truncate_to_bytes(&out, cap).to_owned()
}

/// Bytes of output below which a supplementary note gives up its room.
const MIN_NOTE_BODY_BYTES: usize = 256;

/// Returns the marker that states how much of an output survived the cut.
fn marker(kept: usize, total: usize, cap: usize) -> String {
    format!("\n[output truncated: {kept} of {total} bytes retained, the cap is {cap} bytes]\n")
}

/// Returns the error for a session id the tool does not hold.
fn unknown_session(state: &State, id: &str) -> RuneError {
    let known = state
        .sessions
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let hint = if known.is_empty() {
        String::from("no session is running")
    } else {
        format!("sessions are {known}")
    };
    RuneError::new(ErrorCode::NotFound, format!("no session `{id}`"))
        .with_observed(id)
        .with_hint(hint)
}

/// Marks a finished result as a failure when the command did not succeed.
/// Builds a command's result; a failure that looks like a blocked connection
/// from a command kept off the network says how the user can allow it.
fn finish(mut text: String, exit: Exit, network_denied: bool) -> ToolOutput {
    if exit.is_success() {
        return ToolOutput::success(text);
    }
    let lower = text.to_ascii_lowercase();
    if network_denied
        && NETWORK_FAILURES
            .iter()
            .any(|failure| lower.contains(failure))
        && !text.contains(NETWORK_NOTICE)
    {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(NETWORK_NOTICE);
    }
    ToolOutput::failure(text)
}

/// Resolves the yield window from the arguments.
fn yield_window(arguments: &serde_json::Value, default: u64) -> Result<Duration> {
    let Some(raw) = usize_arg(arguments, "yield_time_ms")? else {
        return Ok(Duration::from_millis(default));
    };
    let bounded = u64::try_from(raw).unwrap_or(u64::MAX).min(MAX_YIELD_MS);
    Ok(Duration::from_millis(bounded))
}

/// Exit status a command killed at its deadline is reported with.
const TIMEOUT_STATUS: i32 = 124;

/// Reads `timeout_ms`, refusing a value outside its bounds.
fn timeout_ms(arguments: &serde_json::Value) -> Result<Option<u64>> {
    let Some(raw) = usize_arg(arguments, "timeout_ms")? else {
        return Ok(None);
    };
    let raw = u64::try_from(raw).unwrap_or(u64::MAX);
    if !(1..=MAX_TIMEOUT_MS).contains(&raw) {
        return Err(RuneError::invalid_field(
            "timeout_ms",
            format!("`timeout_ms` must be between 1 and {MAX_TIMEOUT_MS}"),
        ));
    }
    Ok(Some(raw))
}

/// Returns the state line of a command its deadline killed.
fn timeout_state(timeout: u64) -> String {
    format!(
        "killed after {} s (timeout_ms), exit status {TIMEOUT_STATUS}. The output above is \
         partial. If it needs longer, run it with a larger timeout_ms, or without one and \
         wait with interact.",
        seconds(timeout)
    )
}

/// Writes milliseconds as seconds, with no fractional part when it is whole
/// and the fraction trimmed of trailing zeros otherwise.
fn seconds(ms: u64) -> String {
    let (whole, fraction) = (ms / 1_000, ms % 1_000);
    if fraction == 0 {
        whole.to_string()
    } else {
        let fraction = format!("{fraction:03}");
        format!("{whole}.{}", fraction.trim_end_matches('0'))
    }
}

/// Waits for a process to end, or for the window to elapse.
fn wait_for_exit(
    process: &Process,
    window: Duration,
    context: &ExecutionContext,
) -> Result<Option<Exit>> {
    let deadline = Instant::now().checked_add(window);
    let mut tail = TailWriter::default();
    loop {
        tail.store(process, context);
        if let Some(exit) = process.exit() {
            return Ok(Some(exit));
        }
        context.check_cancelled()?;
        if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
            return Ok(None);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Waits until a process ends or produces more bytes than `baseline`.
///
/// Waiting on output rather than on the clock is what makes an observation
/// cheap: a command that has already answered returns at once, and one that has
/// not returns when its window elapses.
fn wait_for_progress(
    process: &Process,
    window: Duration,
    baseline: u64,
    context: &ExecutionContext,
) -> Result<()> {
    let deadline = Instant::now().checked_add(window);
    let mut tail = TailWriter::default();
    loop {
        tail.store(process, context);
        if process.exit().is_some() || produced(process) > baseline {
            return Ok(());
        }
        context.check_cancelled()?;
        if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
            return Ok(());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Shortest time between two stores of a running command's newest lines.
const TAIL_INTERVAL: Duration = Duration::from_millis(100);

/// Newest output lines kept for a host to show under a running command.
const TAIL_LINES: usize = 3;

/// Bytes read from the end of a stream to find those lines.
const TAIL_BYTES: usize = 4096;

/// Copies a running command's newest output lines into the context's tail.
#[derive(Default)]
struct TailWriter {
    /// When the lines were last stored.
    stored: Option<Instant>,
    /// Bytes each stream had produced at that store.
    seen: (u64, u64),
}

impl TailWriter {
    /// Stores the newest lines of stdout, or of stderr when stdout produced
    /// nothing new since the last store, at most every [`TAIL_INTERVAL`].
    fn store(&mut self, process: &Process, context: &ExecutionContext) {
        let Some(target) = &context.live_tail else {
            return;
        };
        if self
            .stored
            .is_some_and(|stored| stored.elapsed() < TAIL_INTERVAL)
        {
            return;
        }
        let produced = (process.stdout().totals().1, process.stderr().totals().1);
        let lines = if produced.0 > self.seen.0 {
            process.stdout().last_lines(TAIL_LINES, TAIL_BYTES)
        } else if produced.1 > self.seen.1 {
            process.stderr().last_lines(TAIL_LINES, TAIL_BYTES)
        } else {
            return;
        };
        self.stored = Some(Instant::now());
        self.seen = produced;
        *lock(target) = lines;
    }
}

/// Locks the session map, ignoring poisoning.
///
/// A call that panicked left the map consistent enough to keep serving, and a
/// session must stay stoppable whatever happened to another call.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use rune_core::budget::Budget;
    use rune_core::config::Layer;
    use std::process::{Command, Stdio};

    /// Cap the truncation tests use.
    const TEST_CAP: usize = 4096;

    /// Builds a shell tool with a session bound and an output cap.
    fn shell(sessions: u64, output_bytes: usize) -> Shell {
        let mut set = BudgetSet::new();
        set.set(
            LimitName::ParallelToolCalls,
            Budget::Bounded(sessions),
            Layer::User,
        )
        .expect("a bounded session count");
        set.set(
            LimitName::CommandOutputBytes,
            Budget::Bounded(u64::try_from(output_bytes).expect("a bounded cap")),
            Layer::User,
        )
        .expect("a bounded output cap");
        Shell::new(&set)
    }

    /// Returns a workspace for one test.
    /// Returns a workspace for a test about the shell itself.
    ///
    /// The sandbox is left off, because a host that cannot sandbox would
    /// otherwise fail every one of these for a reason that has nothing to do
    /// with what they assert. A host that *can* sandbox would wrap the command
    /// in a PID namespace and a new session, which would change the very
    /// process-group and signal behaviour these tests observe, so the context
    /// is unconfined rather than merely permitted to fall back. The tests that
    /// do assert the sandbox build their own context.
    fn workspace() -> (tempfile::TempDir, ExecutionContext) {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("a UTF-8 path");
        (
            dir,
            ExecutionContext::new(path)
                .with_allow_unsandboxed(true)
                .unconfined(),
        )
    }

    /// Returns a workspace whose commands are subject to the sandbox.
    ///
    /// Returns `None` on a host with no usable backend, where the assertions
    /// would be about the host rather than about the code.
    fn sandboxed_workspace() -> Option<(tempfile::TempDir, ExecutionContext)> {
        let backend = rune_exec::detect();
        if !backend.support().is_full() {
            return None;
        }
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("a UTF-8 path");
        Some((dir, ExecutionContext::new(path)))
    }

    /// Runs one call, expecting it to complete.
    fn call(tool: &Shell, context: &ExecutionContext, arguments: &serde_json::Value) -> ToolOutput {
        tool.call(arguments, context).expect("the call ran")
    }

    /// Runs one call, expecting a successful result, and returns its text.
    fn text(tool: &Shell, context: &ExecutionContext, arguments: &serde_json::Value) -> String {
        let output = call(tool, context, arguments);
        assert!(!output.is_error, "{}", output.text);
        output.text
    }

    /// Polls a condition until it holds or the deadline passes.
    fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if condition() {
                return true;
            }
            if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Sends a signal through `kill`, reporting whether it was accepted.
    #[cfg(unix)]
    fn kill(signal: &str, target: &str) -> bool {
        Command::new("/bin/kill")
            .args(["-s", signal, "--", target])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Returns true while a process id exists.
    #[cfg(unix)]
    fn alive(pid: &str) -> bool {
        kill("0", pid)
    }

    /// Returns true while any process is in a group.
    #[cfg(unix)]
    fn group_alive(group: &str) -> bool {
        kill("0", &format!("-{group}"))
    }

    /// Returns the session id and process group a running result reports.
    ///
    /// Reads both the first result of a `run` that yielded
    /// (`still running after N s as session ID (GROUP). ...`) and the shorter
    /// line of an `interact` (`session ID running, GROUP`).
    fn running(text: &str) -> (String, String) {
        let line = text
            .lines()
            .find(|line| line.contains("as session ") || line.contains(" running, "))
            .unwrap_or_else(|| panic!("no running session in {text}"));
        if let Some((_, rest)) = line.split_once("as session ") {
            let (id, rest) = rest.split_once(" (").expect("a session id and a group");
            let group = rest.split_once(')').expect("the end of the group").0;
            return (id.trim().to_owned(), group.trim().to_owned());
        }
        let rest = line.split_once("session ").expect("a session id").1;
        let (id, group) = rest
            .split_once(" running, ")
            .expect("a session id and a group");
        (
            id.trim().to_owned(),
            group.trim_end_matches(']').trim().to_owned(),
        )
    }

    /// Returns the process id a session reported for the child it forked.
    ///
    /// The pid usually arrives with the start result, because a shell prints it
    /// before it starts waiting. A command slow to print is observed until it
    /// does, which is a poll rather than a sleep so the test stays quick.
    #[cfg(unix)]
    fn forked_pid(tool: &Shell, context: &ExecutionContext, id: &str, started: &str) -> String {
        let mut seen = String::from(started);
        let found = seen.lines().any(is_pid)
            || wait_until(
                || {
                    let observed = text(
                        tool,
                        context,
                        &serde_json::json!({
                            "action": "interact",
                            "session_id": id,
                            "yield_time_ms": 100,
                        }),
                    );
                    seen.push_str(&observed);
                    seen.lines().any(is_pid)
                },
                Duration::from_secs(10),
            );
        assert!(found, "the shell never reported its forked child: {seen}");
        seen.lines()
            .find(|line| is_pid(line))
            .expect("a pid line")
            .trim()
            .to_owned()
    }

    /// Starts a command and waits until it prints `marker`.
    ///
    /// A command that installs a signal trap is only ready to be tested once it
    /// says so; signalling before the trap is in place ends it on the first
    /// signal and hides what the test is about.
    #[cfg(unix)]
    fn start_ready(tool: &Shell, context: &ExecutionContext, command: &str) -> (String, String) {
        let started = text(
            tool,
            context,
            &serde_json::json!({
                "action": "run",
                "command": command,
                "yield_time_ms": 200,
            }),
        );
        let (id, group) = running(&started);
        assert!(started.contains("ready"), "{started}");
        (id, group)
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn a_running_command_stores_its_newest_lines_in_the_live_tail() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let tail = Arc::new(Mutex::new(Vec::new()));
        let context = context.with_live_tail(Arc::clone(&tail));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let watcher = {
            let (tail, seen) = (Arc::clone(&tail), Arc::clone(&seen));
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline {
                    let lines = lock(&tail).clone();
                    if lines.iter().any(|line| line == "four") {
                        *lock(&seen) = lines;
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };
        let output = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": "printf 'one\\ntwo\\nthree\\nfour\\n'; sleep 1",
            }),
        );
        watcher.join().expect("watcher");
        assert!(output.contains("four"), "{output}");
        assert_eq!(*lock(&seen), ["two", "three", "four"]);
    }

    #[test]
    fn shutdown_ends_every_session_gracefully() {
        let tool = shell(4, TEST_CAP);
        let (_dir, context) = workspace();
        // One ignores TERM and must be killed after the grace period; the
        // other ends on TERM. They are stopped together.
        let group = |command: &str| {
            let (_, group) = start_ready(&tool, &context, command);
            group
                .strip_prefix("process group ")
                .expect("group id")
                .to_owned()
        };
        let stubborn = group("trap '' TERM; echo ready; sleep 30");
        let polite = group("echo ready; sleep 30");
        assert!(group_alive(&stubborn) && group_alive(&polite));
        let started = Instant::now();
        tool.shutdown();
        let took = started.elapsed();
        assert!(
            wait_until(
                || !group_alive(&stubborn) && !group_alive(&polite),
                Duration::from_secs(5)
            ),
            "a session survived shutdown"
        );
        assert!(took >= rune_exec::session::GRACE_PERIOD, "{took:?}");
        assert!(
            took < rune_exec::session::GRACE_PERIOD
                .saturating_mul(2)
                .saturating_add(Duration::from_secs(1)),
            "{took:?}"
        );
        assert_eq!(tool.live_sessions(), 0);
    }

    /// Returns true when `observed` contains `expected` as a path.
    ///
    /// Resolve both paths so separators, Windows short names, drive-letter
    /// case and verbatim prefixes do not affect directory identity.
    fn shows_path(observed: &str, expected: &Utf8Path) -> bool {
        let expected = std::fs::canonicalize(expected).expect("expected directory");
        observed
            .lines()
            .any(|line| std::fs::canonicalize(line.trim()).is_ok_and(|path| path == expected))
    }

    /// Returns a command that prints a marker for each line it reads.
    ///
    /// The fixture writes a line and expects to see it back, so the command
    /// copies its input to its output. The platform that expands a variable
    /// before running the line needs a program that reads rather than a
    /// builtin that sets.
    #[cfg(unix)]
    fn echo_input_then_sleep() -> String {
        if cfg!(windows) {
            format!("more & {}", long_sleep())
        } else {
            format!("read line; echo got $line; {}", long_sleep())
        }
    }

    /// Returns a command that reads one line and prints it back with a prefix.
    #[cfg(unix)]
    fn echo_input() -> String {
        if cfg!(windows) {
            // The platform's shell would expand a variable before the line is
            // read, so a program reads the line and prints it instead.
            String::from(
                "powershell -NoProfile -Command \"Write-Output ('got ' + [Console]::In.ReadLine())\"",
            )
        } else {
            String::from("read line; echo got $line")
        }
    }

    /// Returns a command that waits long enough to be stopped by the test.
    fn long_sleep() -> String {
        if cfg!(windows) {
            // The platform's sleep is a console command that fails when its
            // input is redirected, so a ping is used as a delay instead.
            String::from("ping -n 60 127.0.0.1 >nul")
        } else {
            String::from("sleep 30")
        }
    }

    /// Returns a command that prints far more than the cap and then keeps going.
    fn flood_then_wait() -> String {
        if cfg!(windows) {
            // The loop is grouped, because the separator that joins it to the
            // wait binds inside the loop body rather than after the loop, which
            // would run the wait once per line and emit one line.
            format!("({}) & {}", numbered_lines(2000), long_sleep())
        } else {
            format!("{}; {}", numbered_lines(2000), long_sleep())
        }
    }

    /// Returns a command that prints `count` numbered lines.
    fn numbered_lines(count: u32) -> String {
        if cfg!(windows) {
            format!("for /l %i in (1,1,{count}) do @echo line %i padding")
        } else {
            format!("i=0; while [ $i -lt {count} ]; do i=$((i+1)); echo \"line $i padding\"; done")
        }
    }

    /// Returns a command that ends with `code`.
    fn exit_with(code: i32) -> String {
        if cfg!(windows) {
            format!("exit /b {code}")
        } else {
            format!("exit {code}")
        }
    }

    /// Returns a command that prints two words, the first holding two spaces.
    fn echo_two_words() -> String {
        if cfg!(windows) {
            // The platform's shell would collapse the spaces, so the text is
            // printed by a program that takes it as one argument.
            String::from("powershell -NoProfile -Command \"Write-Output 'a  b c'\"")
        } else {
            String::from("printf 'a  b'; echo ' c'")
        }
    }

    /// Returns a command that writes to the error stream and exits with `code`.
    fn echo_to_stderr_and_exit(code: i32) -> String {
        if cfg!(windows) {
            format!("echo oops 1>&2 & exit /b {code}")
        } else {
            format!("echo oops 1>&2; exit {code}")
        }
    }

    /// Returns a command that ends the shell with the signal that kills it.
    #[cfg(unix)]
    fn kill_self() -> String {
        if cfg!(windows) {
            // The platform has no signal to send itself, so the closest
            // equivalent is to end the shell forcefully.
            String::from("taskkill /F /PID %CMDERPID%")
        } else {
            String::from("kill -9 $$")
        }
    }

    /// Returns a command that starts a child, prints its id, and waits.
    #[cfg(unix)]
    fn fork_and_wait() -> String {
        if cfg!(windows) {
            // A child that outlives the parent is what the stop has to reach.
            String::from("start /b ping -n 60 127.0.0.1 >nul & ping -n 60 127.0.0.1 >nul")
        } else {
            String::from("sleep 30 & echo $!; wait")
        }
    }

    /// Returns a command that prints the working directory.
    fn print_cwd() -> String {
        if cfg!(windows) {
            // The platform's shell has a directory builtin, and a program that
            // prints a directory prints it in its own notation.
            String::from("cd")
        } else {
            String::from("pwd")
        }
    }

    /// Returns true for a line holding only digits.
    #[cfg(unix)]
    fn is_pid(line: &str) -> bool {
        let trimmed = line.trim();
        !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit())
    }

    /// Stops a session, expecting the stop to succeed.
    fn stop(tool: &Shell, context: &ExecutionContext, id: &str) -> String {
        text(
            tool,
            context,
            &serde_json::json!({ "action": "stop", "session_id": id }),
        )
    }

    /// Exercises the public tool, not just the terminal syscall wrappers.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_linux_pty_accepts_input_resizes_and_cancels_its_complete_group() {
        let tool = Shell::default();
        let (dir, context) = workspace();
        std::fs::write(
            dir.path().join("pty.sh"),
            include_str!("../tests/fixtures/pty.sh"),
        )
        .expect("fixture");
        let mut started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run", "command": "/bin/sh pty.sh", "pty": true,
                "rows": 27, "cols": 91, "yield_time_ms": 200,
            }),
        );
        let (id, group) = running(&started);
        assert!(
            wait_until(
                || {
                    if started.contains("initial:27 91") {
                        return true;
                    }
                    started.push_str(&text(
                        &tool,
                        &context,
                        &serde_json::json!({
                            "action": "interact", "session_id": id, "yield_time_ms": 100,
                        }),
                    ));
                    false
                },
                Duration::from_secs(10)
            ),
            "{started}"
        );
        let mut observed = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "interact", "session_id": id, "rows": 42, "cols": 110,
                "chars": "hello terminal\n", "yield_time_ms": 200,
            }),
        );
        assert!(
            wait_until(
                || {
                    if dir.path().join("ready").exists() && observed.contains("stderr-marker") {
                        return true;
                    }
                    observed.push_str(&text(
                        &tool,
                        &context,
                        &serde_json::json!({
                            "action": "interact", "session_id": id, "yield_time_ms": 100,
                        }),
                    ));
                    false
                },
                Duration::from_secs(10)
            ),
            "{observed}"
        );
        assert!(observed.contains("input:hello terminal"), "{observed}");
        assert!(observed.contains("resized:42 110"), "{observed}");
        let child = std::fs::read_to_string(dir.path().join("child.pid")).expect("child pid");
        let grandchild =
            std::fs::read_to_string(dir.path().join("grandchild.pid")).expect("grandchild pid");
        assert!(alive(child.trim()) && alive(grandchild.trim()));
        let held = lock(&tool.state)
            .sessions
            .get(&id)
            .map(Arc::clone)
            .expect("session");
        assert_eq!(
            held.process.stderr().totals(),
            (0, 0),
            "PTY streams must be merged"
        );
        let cancelled = context.cancellation();
        let err = std::thread::scope(|scope| {
            let pending = scope.spawn(|| {
                tool.call(
                    &serde_json::json!({
                        "action": "interact", "session_id": id, "yield_time_ms": 10_000,
                    }),
                    &context,
                )
            });
            std::thread::sleep(Duration::from_millis(100));
            cancelled.cancel();
            pending
                .join()
                .expect("interaction thread")
                .expect_err("cancelled")
        });
        assert_eq!(err.code(), ErrorCode::Cancelled);
        assert!(held.process.exit().is_some(), "leader not reaped");
        assert_eq!(tool.live_sessions(), 0);
        let group = group.strip_prefix("process group ").expect("group id");
        assert!(
            wait_until(
                || !alive(child.trim()) && !alive(grandchild.trim()) && !group_alive(group),
                Duration::from_secs(10)
            ),
            "descendants outlived cancellation"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pty_stop_and_drop_end_sessions_and_output_remains_bounded() {
        for force in [false, true] {
            let tool = shell(1, TEST_CAP);
            let (_dir, context) = workspace();
            let started = text(
                &tool,
                &context,
                &serde_json::json!({
                    "action": "run", "command": "stty size; yes x & wait", "pty": true,
                    "yield_time_ms": 100,
                }),
            );
            let (id, group) = running(&started);
            assert!(started.contains("24 80"), "{started}");
            let held = lock(&tool.state)
                .sessions
                .get(&id)
                .map(Arc::clone)
                .expect("session");
            assert!(wait_until(
                || held.process.stdout().totals().1 > TEST_CAP as u64,
                Duration::from_secs(10)
            ));
            assert_eq!(held.process.stdout().totals().0, TEST_CAP);
            assert!(started.len() <= TEST_CAP);
            text(
                &tool,
                &context,
                &serde_json::json!({
                    "action": "stop", "session_id": id, "force": force,
                }),
            );
            let group = group.strip_prefix("process group ").expect("group id");
            assert!(wait_until(|| !group_alive(group), Duration::from_secs(10)));
        }
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run", "command": "sleep 30 & wait", "pty": true, "yield_time_ms": 100,
            }),
        );
        let (_, group) = running(&started);
        drop(tool);
        let group = group.strip_prefix("process group ").expect("group id");
        assert!(wait_until(|| !group_alive(group), Duration::from_secs(10)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandboxed_pty_keeps_its_controlling_terminal_and_workspace_restrictions() {
        let Some((dir, context)) = sandboxed_workspace() else {
            return;
        };
        let tool = Shell::default();
        let forbidden = tempfile::tempdir().expect("outside workspace");
        let target = forbidden.path().join("must-not-exist");
        // Paths generated by tempfile contain no shell quotes.
        let command = format!(
            "test -t 0 && test -t 1 && test -t 2 && stty size </dev/tty; \
             if touch '{}' 2>/dev/null; then exit 99; fi",
            target.display()
        );
        let result = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run", "command": command, "pty": true, "yield_time_ms": 10_000,
            }),
        );
        assert!(result.contains("24 80"), "{result}");
        assert!(result.contains("exited with status 0"), "{result}");
        assert!(!target.exists());
        assert_eq!(tool.live_sessions(), 0);
        assert!(dir.path().exists());
    }

    #[test]
    fn terminal_dimensions_are_validated_before_spawning() {
        let tool = Shell::default();
        let (dir, context) = workspace();
        for arguments in [
            serde_json::json!({"pty": true, "rows": 0, "cols": 80}),
            serde_json::json!({"pty": true, "rows": 24, "cols": 65536}),
            serde_json::json!({"pty": true, "rows": 24}),
            serde_json::json!({"rows": 24, "cols": 80}),
        ] {
            let mut arguments = arguments;
            arguments["action"] = "run".into();
            arguments["command"] = "touch spawned".into();
            assert!(tool.call(&arguments, &context).is_err(), "{arguments}");
        }
        assert!(!dir.path().join("spawned").exists());
        assert_eq!(tool.live_sessions(), 0);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn pty_shell_mode_is_explicitly_unsupported_off_linux() {
        let tool = Shell::default();
        let (dir, mut context) = workspace();
        for sandboxed in [false, true] {
            context.sandboxed = sandboxed;
            let err = tool
                .call(
                    &serde_json::json!({
                        "action": "run", "command": "echo hello > spawned", "pty": true,
                    }),
                    &context,
                )
                .expect_err("Linux only");
            assert_eq!(err.code(), ErrorCode::Unsupported);
            assert_eq!(err.message(), "PTY shell mode is only supported on Linux");
            assert!(
                err.hint()
                    .expect("piped input alternative")
                    .contains("interactive")
            );
            assert!(!dir.path().join("spawned").exists());
            assert_eq!(tool.live_sessions(), 0);
        }
    }

    #[test]
    fn the_schema_names_the_three_actions_and_requires_one() {
        let schema = Shell::default().input_schema();
        let actions = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("an action enum");
        assert_eq!(actions.len(), 3);
        assert_eq!(schema["required"][0], "action");
    }

    #[test]
    fn input_to_a_session_is_judged_with_the_command_it_goes_to() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": long_sleep(),
                "yield_time_ms": 0,
            }),
        );
        let (id, _group) = running(&started);

        // Any view of the tool names the same target, including one that does
        // not hold the session, which is what the loop consults.
        for view in [&tool, &Shell::default()] {
            let sending = view.permission_target(&serde_json::json!({
                "action": "interact",
                "session_id": id,
                "command": "ls",
                "chars": "rm -rf .git\n",
            }));
            assert_eq!(
                sending.as_deref(),
                Some(format!("{}\nrm -rf .git\n", long_sleep()).as_str())
            );
            let reading = view.permission_target(&serde_json::json!({
                "action": "interact",
                "session_id": id,
            }));
            assert_eq!(reading.as_deref(), Some(long_sleep().as_str()));
        }

        let stopping = tool.permission_target(&serde_json::json!({
            "action": "stop",
            "session_id": id,
            "command": "ls",
        }));
        assert_eq!(stopping.as_deref(), Some(STOP_TARGET));
        let _ = stop(&tool, &context, &id);

        // A session that has gone is named by its id, so the input is still
        // judged rather than dropped.
        let gone = tool.permission_target(&serde_json::json!({
            "action": "interact",
            "session_id": id,
            "chars": "rm -rf .git\n",
        }));
        assert_eq!(gone, Some(format!("{id}\nrm -rf .git\n")));
    }

    #[test]
    fn a_command_that_reads_its_input_finishes_when_it_is_not_interactive() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        // Each program reads standard input when it is given no file.
        let command = if cfg!(windows) { "findstr x" } else { "cat" };
        let observed = call(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": command,
                "yield_time_ms": 10_000,
            }),
        );
        assert!(
            observed.text.contains("exited with status"),
            "{}",
            observed.text
        );
        // `findstr` that matches nothing exits with 1, so only `cat` has a
        // status to pin.
        if cfg!(unix) {
            assert!(
                observed.text.contains("exited with status 0"),
                "{}",
                observed.text
            );
        }
        assert_eq!(tool.live_sessions(), 0);
    }

    #[test]
    fn input_to_a_session_started_without_it_is_refused_with_a_remedy() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": long_sleep(),
                "yield_time_ms": 0,
            }),
        );
        let (id, _group) = running(&started);
        let err = tool
            .call(
                &serde_json::json!({ "action": "interact", "session_id": id, "chars": "x\n" }),
                &context,
            )
            .expect_err("the session takes no input");
        assert_eq!(err.code(), ErrorCode::InvalidState);
        assert!(
            err.detail()
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("interactive")),
            "{err}"
        );
        let _ = stop(&tool, &context, &id);
    }

    #[test]
    fn input_a_session_never_reads_does_not_hold_the_call() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": long_sleep(),
                "interactive": true,
                "yield_time_ms": 0,
            }),
        );
        let (id, _group) = running(&started);
        let began = Instant::now();
        let err = tool
            .call(
                &serde_json::json!({
                    "action": "interact",
                    "session_id": id,
                    "chars": "x".repeat(8 * 1024 * 1024),
                    "yield_time_ms": 0,
                }),
                &context,
            )
            .expect_err("the session never read its input");
        assert_eq!(err.code(), ErrorCode::InvalidState);
        assert!(
            began.elapsed() < MAX_INPUT_WINDOW,
            "the call waited {:?}",
            began.elapsed()
        );
        let _ = stop(&tool, &context, &id);
    }

    #[test]
    fn a_run_is_judged_by_its_command() {
        let target = Shell::default().permission_target(&serde_json::json!({
            "action": "run",
            "command": "git status",
        }));
        assert_eq!(target.as_deref(), Some("git status"));
    }

    #[test]
    fn a_command_that_exits_immediately_returns_its_status_without_a_session() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let observed = text(
            &tool,
            &context,
            &serde_json::json!({ "action": "run", "command": "echo hello" }),
        );
        assert!(observed.contains("hello"), "{observed}");
        assert!(observed.contains("exited with status 0"), "{observed}");
        assert!(!observed.contains(" running"), "{observed}");
        assert_eq!(tool.live_sessions(), 0);
    }

    #[test]
    fn the_exact_command_string_is_recorded_in_the_result() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        // Two words, the first holding a run of spaces that must survive both
        // the shell and the result.
        let command = echo_two_words();
        let observed = text(
            &tool,
            &context,
            &serde_json::json!({ "action": "run", "command": command }),
        );
        assert!(
            observed.contains(&format!("$ {command}")),
            "the command was not recorded as written: {observed}"
        );
        assert!(
            observed.contains("a  b c"),
            "the spacing did not survive: {observed}"
        );
    }

    #[test]
    fn a_failing_command_reports_its_status_as_a_failure() {
        let (_dir, context) = workspace();
        let output = call(
            &Shell::default(),
            &context,
            &serde_json::json!({ "action": "run", "command": echo_to_stderr_and_exit(7) }),
        );
        assert!(output.is_error);
        assert!(output.text.contains("oops"), "{}", output.text);
        assert!(
            output.text.contains("exited with status 7"),
            "{}",
            output.text
        );
    }

    #[cfg(unix)]
    #[test]
    fn standard_error_appears_where_it_was_written() {
        let (_dir, context) = workspace();
        let output = call(
            &Shell::default(),
            &context,
            &serde_json::json!({
                "action": "run",
                "command": "printf 'o1\\n'; printf 'e1\\n' >&2; printf 'o2\\n'; printf 'e2\\n' >&2",
            }),
        );
        assert!(!output.is_error, "{}", output.text);
        assert!(
            output.text.contains("\no1\ne1\no2\ne2\n"),
            "{}",
            output.text
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_signal_that_ends_a_command_is_reported_by_name() {
        let (_dir, context) = workspace();
        let output = call(
            &Shell::default(),
            &context,
            &serde_json::json!({ "action": "run", "command": kill_self() }),
        );
        assert!(output.is_error);
        assert!(
            output.text.contains("killed by signal 9 (SIGKILL)"),
            "{}",
            output.text
        );
        assert!(
            output
                .text
                .contains("[killed by signal 9 (SIGKILL), exit status 137]"),
            "{}",
            output.text
        );
    }

    /// Runs `sleep 30 & echo $!` to completion and returns the result and the
    /// pid of the background process.
    #[cfg(unix)]
    fn run_background(tool: &Shell, context: &ExecutionContext) -> (ToolOutput, String) {
        let output = call(
            tool,
            context,
            &serde_json::json!({ "action": "run", "command": "sleep 30 & echo $!" }),
        );
        let pid = output
            .text
            .lines()
            .find(|line| is_pid(line))
            .unwrap_or_else(|| panic!("no pid in {}", output.text))
            .trim()
            .to_owned();
        (output, pid)
    }

    #[cfg(unix)]
    #[test]
    fn a_background_process_is_reported_and_left_running() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let began = Instant::now();
        let (output, pid) = run_background(&tool, &context);
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "the call waited {:?}",
            began.elapsed()
        );
        assert!(!output.is_error, "{}", output.text);
        assert!(
            output.text.contains("[exited with status 0]\n"),
            "{}",
            output.text
        );
        assert!(
            output.text.ends_with(&format!("{BACKGROUND_NOTICE}\n")),
            "{}",
            output.text
        );
        assert!(alive(&pid), "the background process was ended");

        let quiet = text(
            &tool,
            &context,
            &serde_json::json!({ "action": "run", "command": "echo hi" }),
        );
        assert!(!quiet.contains(BACKGROUND_NOTICE), "{quiet}");
        drop(tool);
        assert!(wait_until(|| !alive(&pid), Duration::from_secs(5)));
    }

    #[cfg(unix)]
    #[test]
    fn background_counts_a_live_session_and_notices_its_end() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({ "action": "run", "command": "sleep 30", "yield_time_ms": 100 }),
        );
        let (id, _) = running(&started);
        let (alive, before) = background();
        assert!(alive >= 1, "{alive}");
        let _ = text(
            &tool,
            &context,
            &serde_json::json!({ "action": "stop", "session_id": id }),
        );
        assert!(wait_until(
            || background().1 > before,
            Duration::from_secs(5)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn background_counts_a_leftover_group() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let (_, before) = background();
        let (_, pid) = run_background(&tool, &context);
        assert!(alive(&pid));
        let (alive_now, after) = background();
        assert!(alive_now >= 1, "{alive_now}");
        assert!(after > before);
        drop(tool);
        assert!(wait_until(
            || background().1 > after,
            Duration::from_secs(5)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_tool_ends_leftover_background_processes() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let (_, pid) = run_background(&tool, &context);
        assert!(alive(&pid));
        drop(tool);
        assert!(
            wait_until(|| !alive(&pid), Duration::from_secs(2)),
            "a background process outlived the tool"
        );
    }

    #[cfg(unix)]
    #[test]
    fn leftover_groups_are_bounded() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let pids: Vec<String> = (0..=MAX_LEFTOVERS)
            .map(|_| run_background(&tool, &context).1)
            .collect();
        let (oldest, rest) = pids.split_first().expect("pids");
        assert!(
            wait_until(|| !alive(oldest), Duration::from_secs(5)),
            "the oldest kept group was not ended"
        );
        assert!(rest.iter().all(|pid| alive(pid)));
        assert_eq!(lock(&tool.state).leftovers.len(), MAX_LEFTOVERS);
        drop(tool);
        assert!(wait_until(
            || rest.iter().all(|pid| !alive(pid)),
            Duration::from_secs(5)
        ));
    }

    /// The state line of a command killed at a 0.5 s deadline.
    const HALF_SECOND_TIMEOUT: &str = "[killed after 0.5 s (timeout_ms), exit status 124. The \
        output above is partial. If it needs longer, run it with a larger timeout_ms, or \
        without one and wait with interact.]";

    #[cfg(unix)]
    #[test]
    fn a_timeout_kills_the_group_and_says_the_output_is_partial() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let began = Instant::now();
        let output = call(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": "echo start; echo $$; sleep 30",
                "timeout_ms": 500,
            }),
        );
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "the call waited {:?}",
            began.elapsed()
        );
        assert!(output.is_error, "{}", output.text);
        assert!(output.text.contains("\nstart\n"), "{}", output.text);
        assert_eq!(output.text.lines().last(), Some(HALF_SECOND_TIMEOUT));
        let group = output
            .text
            .lines()
            .find(|line| is_pid(line))
            .expect("the shell's pid")
            .trim()
            .to_owned();
        assert!(
            wait_until(|| !group_alive(&group), Duration::from_secs(5)),
            "the group outlived its timeout"
        );
        assert_eq!(tool.live_sessions(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_still_applies_after_the_call_yields() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": "sleep 30",
                "timeout_ms": 2_000,
                "yield_time_ms": 100,
            }),
        );
        assert!(started.contains("still running after 0.1 s"), "{started}");
        let (id, group) = running(&started);
        let group = group.strip_prefix("process group ").expect("group id");
        std::thread::sleep(Duration::from_millis(2_500));
        assert!(!group_alive(group), "the deadline did not end the group");
        let output = call(
            &tool,
            &context,
            &serde_json::json!({ "action": "interact", "session_id": id }),
        );
        assert!(output.is_error, "{}", output.text);
        assert!(
            output.text.lines().last().is_some_and(
                |line| line.starts_with("[killed after 2 s (timeout_ms), exit status 124.")
            ),
            "{}",
            output.text
        );
        assert_eq!(tool.live_sessions(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_ms_is_refused_outside_run() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": echo_input_then_sleep(),
                "interactive": true,
                "yield_time_ms": 0,
            }),
        );
        let (id, _group) = running(&started);
        for action in ["interact", "stop"] {
            let err = tool
                .call(
                    &serde_json::json!({
                        "action": action,
                        "session_id": id,
                        "chars": "hello\n",
                        "timeout_ms": 1_000,
                    }),
                    &context,
                )
                .expect_err("timeout_ms applies only to run");
            assert_eq!(err.code(), ErrorCode::InvalidField);
            assert_eq!(err.field(), Some("timeout_ms"));
        }
        let observed = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "interact", "session_id": id, "yield_time_ms": 200,
            }),
        );
        assert!(!observed.contains("got hello"), "{observed}");
        assert_eq!(
            tool.live_sessions(),
            1,
            "the refused stop ended the session"
        );
        let _ = stop(&tool, &context, &id);

        for bound in [0, 4_000_000] {
            let err = tool
                .call(
                    &serde_json::json!({
                        "action": "run", "command": "echo hi", "timeout_ms": bound,
                    }),
                    &context,
                )
                .expect_err("out of bounds");
            assert_eq!(err.field(), Some("timeout_ms"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_finishes_first_ignores_its_timeout() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let began = Instant::now();
        let output = call(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run", "command": "echo hi; exit 3", "timeout_ms": 10_000,
            }),
        );
        assert!(began.elapsed() < Duration::from_secs(5));
        assert!(output.is_error, "{}", output.text);
        assert_eq!(output.text.lines().last(), Some("[exited with status 3]"));
        assert!(!output.text.contains("timeout_ms"), "{}", output.text);
    }

    #[cfg(unix)]
    #[test]
    fn a_yielded_run_says_not_to_run_the_command_again() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run", "command": "sleep 30", "yield_time_ms": 100,
            }),
        );
        let (id, group) = running(&started);
        assert_eq!(
            started.lines().last().map(str::to_owned),
            Some(format!(
                "[still running after 0.1 s as session {id} ({group}). Do not run the command \
                 again. Use action \"interact\" with this session_id to wait for more output, \
                 or action \"stop\" to end it.]"
            ))
        );
        let _ = stop(&tool, &context, &id);
    }

    #[test]
    fn the_description_does_not_grow() {
        assert!(Shell::default().description().len() <= 219);
    }

    #[test]
    fn seconds_drop_a_whole_fraction_and_trailing_zeros() {
        assert_eq!(seconds(30_000), "30");
        assert_eq!(seconds(1_500), "1.5");
        assert_eq!(seconds(500), "0.5");
        assert_eq!(seconds(1_250), "1.25");
        assert_eq!(seconds(7), "0.007");
    }

    #[cfg(unix)]
    #[test]
    fn a_server_returns_a_session_and_an_interaction_reads_its_output() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": echo_input_then_sleep(),
                "interactive": true,
                "yield_time_ms": 100,
            }),
        );
        let (id, _group) = running(&started);

        let answered = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "interact",
                "session_id": id,
                "chars": "go\n",
                "yield_time_ms": 10_000,
            }),
        );
        assert!(answered.contains("got go"), "{answered}");
        assert!(answered.contains(" running"), "{answered}");

        let quiet = text(
            &tool,
            &context,
            &serde_json::json!({ "action": "interact", "session_id": id, "yield_time_ms": 0 }),
        );
        assert!(!quiet.contains("got go"), "{quiet}");
        assert!(quiet.contains(" running"), "{quiet}");

        let stopped = stop(&tool, &context, &id);
        assert!(stopped.contains("ended"), "{stopped}");
        assert_eq!(tool.live_sessions(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn idle_interacts_get_a_hint_on_the_fourth_poll() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({ "action": "run", "command": "sleep 30", "yield_time_ms": 0 }),
        );
        let (id, _group) = running(&started);
        for poll in 1..=5 {
            let polled = text(
                &tool,
                &context,
                &serde_json::json!({ "action": "interact", "session_id": id, "yield_time_ms": 10 }),
            );
            assert!(polled.contains(" running"), "{polled}");
            assert_eq!(
                polled.contains(IDLE_HINT),
                poll >= 4,
                "poll {poll}: {polled}"
            );
        }
        let stopped = stop(&tool, &context, &id);
        assert!(stopped.contains("ended"), "{stopped}");
    }

    #[cfg(unix)]
    #[test]
    fn a_stop_ends_the_whole_process_group_including_a_forked_child() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": fork_and_wait(),
                "yield_time_ms": 200,
            }),
        );
        let (id, group) = running(&started);
        let child = forked_pid(&tool, &context, &id, &started);
        assert!(alive(&child), "the forked child was not started");

        let stopped = stop(&tool, &context, &id);
        assert!(stopped.contains("ended"), "{stopped}");
        assert!(
            wait_until(|| !alive(&child), Duration::from_secs(10)),
            "the forked child outlived the stop"
        );
        assert!(
            wait_until(|| !group_alive(&group), Duration::from_secs(10)),
            "the process group outlived the stop"
        );
    }

    #[test]
    fn a_zero_yield_window_returns_the_session_immediately() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = Instant::now();
        let observed = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": format!("echo late & {}", long_sleep()),
                "yield_time_ms": 0,
            }),
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a zero window waited"
        );
        assert!(observed.contains(" running"), "{observed}");
        let (id, _group) = running(&observed);
        let _ = stop(&tool, &context, &id);
    }

    #[test]
    fn output_past_the_cap_is_truncated_with_a_marker_and_the_retained_count() {
        let tool = shell(4, TEST_CAP);
        let (_dir, context) = workspace();
        let observed = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": numbered_lines(2000),
            }),
        );
        assert!(
            observed.len() <= TEST_CAP,
            "the result is {} bytes",
            observed.len()
        );
        assert!(observed.contains("output truncated"), "{observed}");
        assert!(observed.contains("bytes retained"), "{observed}");
        assert!(
            observed.contains(&format!("the cap is {TEST_CAP} bytes")),
            "{observed}"
        );

        // The count in the marker is the number of bytes actually kept, which
        // are the start before the marker and the end after it.
        let marker = observed.find("\n[output truncated").expect("a marker");
        let reported: usize = observed[marker..]
            .split_once(": ")
            .and_then(|(_, rest)| rest.split_once(" of "))
            .and_then(|(kept, _)| kept.parse().ok())
            .expect("a retained count");
        let head = observed.find('\n').expect("the command line") + 1;
        let after = marker + observed[marker..].find("]\n").expect("marker end") + 2;
        // The end runs to the capture note or, without one, the state line.
        let state = ["\n[output capture", "\n[exited"]
            .iter()
            .filter_map(|line| observed[after..].find(line))
            .min()
            .map(|offset| after + offset + 1)
            .expect("the state line");
        assert_eq!(reported, (marker - head) + (state - after));

        // The end of the output survives the cut, which is where a test run
        // reports what failed.
        assert!(observed[after..state].contains("2000"), "{observed}");
        assert!(observed[head..marker].contains('1'), "{observed}");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_command_keeps_more_of_its_tail() {
        let tool = shell(4, TEST_CAP);
        let (_dir, context) = workspace();
        // Returns the bytes kept before and after the cut marker.
        let sides = |code: i32| {
            let output = call(
                &tool,
                &context,
                &serde_json::json!({
                    "action": "run",
                    "command": format!("{}; {}", numbered_lines(2000), exit_with(code)),
                }),
            );
            assert_eq!(output.is_error, code != 0, "{}", output.text);
            let text = output.text;
            assert!(text.contains("line 2000 padding"), "{text}");
            let head = text.find('\n').expect("the command line") + 1;
            let marker = text.find("\n[output truncated").expect("a marker");
            let after = marker + text[marker..].find("]\n").expect("marker end") + 2;
            let state = ["\n[output capture", "\n[exited"]
                .iter()
                .filter_map(|line| text[after..].find(line))
                .min()
                .map(|offset| after + offset + 1)
                .expect("the state line");
            (marker - head, state - after)
        };
        let (head, tail) = sides(1);
        assert!(tail >= head * 3, "head {head}, tail {tail}");
        let (head, tail) = sides(0);
        assert!(tail < head * 3, "head {head}, tail {tail}");
        assert!(tail > head, "head {head}, tail {tail}");
    }

    #[test]
    fn a_stream_whose_capture_stopped_at_the_cap_is_reported() {
        let tool = shell(4, TEST_CAP);
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": flood_then_wait(),
                "yield_time_ms": 2_000,
            }),
        );
        assert!(
            started.contains("output capture stopped after"),
            "{started}"
        );
        let (id, _group) = running(&started);
        let _ = stop(&tool, &context, &id);
    }

    #[cfg(unix)]
    #[test]
    fn a_finished_test_run_leads_with_its_counts() {
        let tool = shell(4, TEST_CAP);
        let (_dir, context) = workspace();
        // A failing run exits nonzero, which the result reports as an error.
        let observed = call(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": "printf 'running 3 tests\\ntest result: FAILED. 2 passed; 1 failed; 0 ignored\\n'; exit 101",
            }),
        )
        .text;
        let second = observed.lines().nth(1).unwrap_or_default();
        assert_eq!(second, "[tests: 2 passed, 1 failed]", "{observed}");
    }

    #[cfg(unix)]
    #[test]
    fn a_finished_run_reports_its_exit_code_and_failing_tests() {
        let tool = shell(4, TEST_CAP);
        let (_dir, context) = workspace();
        let output = call(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": "printf 'test a ... FAILED\\ntest result: FAILED. 0 passed; 1 failed; 0 ignored\\n'; exit 101",
            }),
        );
        let check = output.check.expect("a finished command reports its check");
        assert_eq!(check.failing, vec!["a".to_owned()]);
        assert_eq!(check.exit_code, Some(101));
        assert_eq!(check.failed_tests, Some(1));
        assert_eq!(check.signature, "a");

        let plain = call(
            &tool,
            &context,
            &serde_json::json!({"action": "run", "command": "echo fine"}),
        );
        let check = plain.check.expect("a finished command reports its check");
        assert_eq!(check.exit_code, Some(0));
        assert_eq!(check.failed_tests, None);
        assert_eq!(check.signature.len(), 16);

        // A command still running has reported nothing yet; its end does.
        let started = call(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": "sleep 0.5; exit 3",
                "yield_time_ms": 50,
            }),
        );
        assert_eq!(started.check, None, "{}", started.text);
        let (id, _) = running(&started.text);
        let ended = call(
            &tool,
            &context,
            &serde_json::json!({"action": "interact", "session_id": id, "yield_time_ms": 10_000}),
        );
        let check = ended.check.expect("an ended session reports its check");
        assert_eq!(check.exit_code, Some(3), "{}", ended.text);
    }

    #[cfg(unix)]
    #[test]
    fn a_repeated_test_run_shows_what_changed_and_keeps_the_full_log() {
        let tool = shell(4, TEST_CAP);
        let (_dir, context) = workspace();
        // The first run fails `a`; the second, finding the marker, passes it.
        let arguments = serde_json::json!({
            "action": "run",
            "command": "for n in 1 2 3 4 5 6 7 8 9 10; do echo \"  at frame $n\"; done; \
                if [ -f seen ]; then echo 'test a ... ok'; \
                else touch seen; echo 'test a ... FAILED'; fi; \
                echo 'test b ... FAILED'; \
                echo 'test result: FAILED. 0 passed; 1 failed; 0 ignored'; exit 101",
        });
        let first = call(&tool, &context, &arguments);
        assert!(first.full_text.is_none());
        let second = call(&tool, &context, &arguments);
        assert!(
            second
                .text
                .contains("[since the last run: now passing: a; still failing: b;"),
            "{}",
            second.text
        );
        assert!(!second.text.contains("frame 4"), "{}", second.text);
        assert!(second.full_text.expect("full log").contains("frame 4"));
    }

    #[test]
    fn a_tiny_cap_still_bounds_the_result_and_keeps_the_command() {
        let tool = shell(4, 200);
        let (_dir, context) = workspace();
        let observed = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": numbered_lines(100),
            }),
        );
        assert!(
            observed.len() <= 200,
            "the result is {} bytes",
            observed.len()
        );
        assert!(
            observed.starts_with(&format!("$ {}", numbered_lines(100))),
            "{observed}"
        );
        assert!(observed.contains("output truncated"), "{observed}");
        assert!(observed.contains("exited with status 0"), "{observed}");
    }

    #[test]
    fn concurrent_starts_cannot_both_win_the_last_slot() {
        let tool = Arc::new(shell(1, 64 * 1024));
        let (_dir, context) = workspace();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let outcomes: Vec<bool> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let tool = Arc::clone(&tool);
                    let context = context.clone();
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        tool.call(
                            &serde_json::json!({
                                "action": "run",
                                "command": long_sleep(),
                                "yield_time_ms": 50,
                            }),
                            &context,
                        )
                        .is_ok()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("a start thread"))
                .collect()
        });
        assert_eq!(
            outcomes.iter().filter(|started| **started).count(),
            1,
            "exactly one start should have taken the only slot"
        );
        assert_eq!(tool.live_sessions(), 1);
    }

    #[test]
    fn a_start_that_returns_a_finished_command_gives_its_slot_back() {
        let tool = shell(1, 64 * 1024);
        let (_dir, context) = workspace();
        // Commands that end inside their window must not consume the bound,
        // or a run of quick commands would exhaust it without holding anything.
        for _ in 0..3 {
            let output = call(
                &tool,
                &context,
                &serde_json::json!({ "action": "run", "command": "echo done" }),
            );
            assert!(!output.is_error, "{}", output.text);
        }
        assert_eq!(tool.live_sessions(), 0);
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": long_sleep(),
                "yield_time_ms": 50,
            }),
        );
        let (id, _group) = running(&started);
        let _ = stop(&tool, &context, &id);
    }

    #[test]
    fn a_slow_observation_does_not_hold_up_another_session() {
        let tool = Arc::new(shell(4, 64 * 1024));
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": long_sleep(),
                "yield_time_ms": 50,
            }),
        );
        let (id, _group) = running(&started);

        // This observation spends its whole window waiting on a command that
        // prints nothing, so anything serialized behind it would stall too.
        let watcher = {
            let tool = Arc::clone(&tool);
            let context = context.clone();
            let id = id.clone();
            std::thread::spawn(move || {
                text(
                    &tool,
                    &context,
                    &serde_json::json!({
                        "action": "interact",
                        "session_id": id,
                        "yield_time_ms": 3_000,
                    }),
                )
            })
        };

        let began = Instant::now();
        let quick = text(
            &tool,
            &context,
            &serde_json::json!({ "action": "run", "command": "echo ready" }),
        );
        let elapsed = began.elapsed();
        assert!(quick.contains("ready"), "{quick}");
        assert!(
            elapsed < Duration::from_secs(2),
            "a start waited {elapsed:?} behind another session"
        );
        let _ = watcher.join();
        let _ = stop(&tool, &context, &id);
    }

    #[test]
    fn a_command_runs_in_a_path_holding_a_space_and_a_non_ascii_name() {
        // The workspace path is passed to the sandbox as a profile rule and to
        // the shell as its working directory, so a space in it must not split
        // into two arguments and a non-ASCII name must survive both. Both halves
        // need a sandbox to be in play.
        if !rune_exec::detect().support().is_full() {
            return;
        }
        let tool = shell(4, 64 * 1024);
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let awkward = root.join("a dir with spaces/płik.dir.");
        std::fs::create_dir_all(&awkward).expect("mkdir");
        let context = ExecutionContext::new(awkward.clone());

        let output = call(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "cwd": awkward.as_str(),
                "command": "printf ok > 'a file.txt'",
                "yield_time_ms": 2_000,
            }),
        );
        assert!(
            awkward.join("a file.txt").exists(),
            "the command did not write in the workspace: {}",
            output.text
        );
    }

    #[test]
    fn a_command_without_a_working_directory_writes_in_the_workspace() {
        // The policy grants the resolved workspace while the process starts in
        // the path as written. On a host where those differ, a rule built from
        // one and a working directory set to the other leave the command unable
        // to write anything, which is what this pins. It is a property of the
        // sandbox, so it needs one.
        if !rune_exec::detect().support().is_full() {
            return;
        }
        let tool = shell(4, 64 * 1024);
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let workspace = root.join("plain");
        std::fs::create_dir_all(&workspace).expect("mkdir");
        let context = ExecutionContext::new(workspace.clone());

        let output = call(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": "printf ok > out.txt",
                "yield_time_ms": 2_000,
            }),
        );
        assert!(
            workspace.join("out.txt").exists(),
            "the command could not write in its own workspace: {}",
            output.text
        );
    }

    #[test]
    fn a_command_runs_when_the_override_is_set_and_no_backend_is_available() {
        // A host with no usable backend refuses every command by default. The
        // override is what makes such a host usable at all, so it has to reach
        // the wrapper rather than being read and dropped.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");

        let blocked = ExecutionContext::new(root.to_owned());
        let allowed = ExecutionContext::new(root.to_owned()).with_allow_unsandboxed(true);

        // Both contexts describe the same workspace; only the override differs.
        assert!(!blocked.allow_unsandboxed);
        assert!(allowed.allow_unsandboxed);
    }

    #[test]
    fn the_override_is_separate_from_reaching_outside_the_workspace() {
        // Reaching out and skipping the sandbox are different questions. Tying
        // them together means a host can only run a command that stays inside
        // its workspace by also granting it the whole machine.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");

        let outward = ExecutionContext::new(root.to_owned()).with_external_access(true);
        assert!(outward.external_access);
        assert!(
            !outward.allow_unsandboxed,
            "permitting a path outside the workspace also skipped the sandbox"
        );

        let unsandboxed = ExecutionContext::new(root.to_owned()).with_allow_unsandboxed(true);
        assert!(unsandboxed.allow_unsandboxed);
        assert!(
            !unsandboxed.external_access,
            "skipping the sandbox also permitted paths outside the workspace"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn offline_shell_policy_restricts_network_even_with_external_access() {
        use rune_exec::Sandbox;

        let (_dir, context) = workspace();
        let backend = rune_exec::LinuxSandbox::with_helper(Utf8PathBuf::from("/usr/bin/bwrap"));
        let prepared = rune_exec::command::prepare_shell(
            "printf probe",
            &context.workspace,
            None,
            rune_exec::minimal_environment(),
        )
        .expect("prepare");
        for (external_access, offline, restricted) in [
            (false, false, true),
            (false, true, true),
            (true, false, false),
            (true, true, true),
        ] {
            let context = context
                .fork()
                .with_external_access(external_access)
                .with_offline(offline)
                .fork();
            let wrapped = backend
                .wrap(&prepared, &sandbox_policy(&context), false)
                .expect("wrap");
            assert_eq!(
                wrapped.argv.iter().any(|arg| arg == "--unshare-net"),
                restricted,
                "external_access={external_access}, offline={offline}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_confined_command_goes_through_the_sandbox_backend() {
        let dir = tempfile::tempdir().expect("workspace");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let context = ExecutionContext::new(root.clone());
        assert!(context.sandboxed, "a default context must be confined");
        let prepared = rune_exec::command::prepare_shell(
            "printf probe",
            &root,
            None,
            rune_exec::minimal_environment(),
        )
        .expect("prepare");
        let policy = sandbox_policy(&context);
        // A full backend is injected, so the test does not depend on whether
        // this host has a sandbox helper installed.
        let backend = rune_exec::LinuxSandbox::with_helper(Utf8PathBuf::from("/usr/bin/bwrap"));
        let wrapped =
            confined(&prepared, &policy, &context, Input::Closed, &backend).expect("wrap");
        assert_ne!(
            wrapped.argv, prepared.argv,
            "a confined command was not wrapped"
        );
        assert_eq!(
            wrapped.argv.first().map(String::as_str),
            Some("/usr/bin/bwrap")
        );
    }

    /// A shell test's context is unconfined, so a command runs directly even on
    /// a host that could sandbox it.
    ///
    /// This is the regression the Ubuntu runner exposed: a sandbox wrapper runs
    /// the command in a PID namespace and a new session, so the host PIDs the
    /// process-group tests read are not the running processes, and a signal that
    /// ends the inner command is seen by Rune only as the wrapper's exit code.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_unconfined_command_skips_the_sandbox_even_when_one_is_available() {
        let (_dir, context) = workspace();
        assert!(
            !context.sandboxed,
            "a shell test context must be unconfined"
        );
        let prepared = rune_exec::command::prepare_shell(
            "printf probe",
            &context.workspace,
            None,
            rune_exec::minimal_environment(),
        )
        .expect("prepare");
        let policy = sandbox_policy(&context);
        let backend = rune_exec::LinuxSandbox::with_helper(Utf8PathBuf::from("/usr/bin/bwrap"));
        // PTY and piped/closed input both have to pass through: the PTY case
        // is the one the old helper would have refused with a foreign-backend
        // error.
        for input in [
            Input::Closed,
            Input::Piped,
            Input::Pty { rows: 24, cols: 80 },
        ] {
            let wrapped = confined(&prepared, &policy, &context, input, &backend).expect("wrap");
            assert_eq!(
                wrapped.argv, prepared.argv,
                "an unconfined command was wrapped in the sandbox"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_offline_full_access_shell_cannot_reach_a_local_fixture() {
        use std::io::Read;
        use std::net::TcpListener;

        let backend = rune_exec::detect();
        if !backend.support().is_full() {
            eprintln!("skipping live Linux sandbox test: {:?}", backend.support());
            return;
        }
        let dir = tempfile::tempdir().expect("workspace");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let context = ExecutionContext::new(root.to_owned()).with_external_access(true);
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("address").port();
        let arguments = serde_json::json!({
            "action": "run",
            "command": format!(
                "printf 'SHELL_STARTED\\n'; /bin/bash -c 'printf FIXTURE_DATA > /dev/tcp/127.0.0.1/{port}'"
            ),
            "yield_time_ms": 2_000,
        });
        let tool = shell(4, TEST_CAP);

        // The online control proves the command reaches this fixture.
        text(&tool, &context, &arguments);
        let (mut stream, _) = listener.accept().expect("online connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read deadline");
        let mut received = String::new();
        stream.read_to_string(&mut received).expect("fixture data");
        assert_eq!(received, "FIXTURE_DATA");

        let output = call(&tool, &context.with_offline(true).fork(), &arguments);
        assert!(output.is_error, "{}", output.text);
        assert!(output.text.contains("SHELL_STARTED"), "{}", output.text);
        let error = listener.accept().expect_err("offline opened no connection");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_network_grant_reaches_a_local_fixture() {
        use std::io::Read;
        use std::net::TcpListener;

        let backend = rune_exec::detect();
        if !backend.support().is_full() {
            eprintln!("skipping live Linux sandbox test: {:?}", backend.support());
            return;
        }
        let dir = tempfile::tempdir().expect("workspace");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("address").port();
        let arguments = serde_json::json!({
            "action": "run",
            "command": format!(
                "printf 'SHELL_STARTED\\n'; /bin/bash -c 'printf FIXTURE_DATA > /dev/tcp/127.0.0.1/{port}'"
            ),
            "yield_time_ms": 2_000,
        });
        let tool = shell(4, TEST_CAP);

        // Without the grant the sandbox keeps the command off the network.
        let denied = call(&tool, &ExecutionContext::new(root.to_owned()), &arguments);
        assert!(denied.is_error, "{}", denied.text);
        let error = listener.accept().expect_err("no grant, no connection");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);

        let granted = ExecutionContext::new(root.to_owned()).with_network(true);
        text(&tool, &granted, &arguments);
        listener.set_nonblocking(false).expect("blocking");
        let (mut stream, _) = listener.accept().expect("granted connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read deadline");
        let mut received = String::new();
        stream.read_to_string(&mut received).expect("fixture data");
        assert_eq!(received, "FIXTURE_DATA");
    }

    #[test]
    fn network_context_grants_the_sandbox_network() {
        let context = ExecutionContext::new(Utf8PathBuf::from("/work"));
        assert!(!sandbox_policy(&context).network);
        let granted = context.clone().with_network(true);
        assert!(sandbox_policy(&granted).network);
        assert!(!sandbox_policy(&granted.with_offline(true)).network);
        // The network grant does not reach outside the workspace for files.
        assert!(!context.with_network(true).external_access);
    }

    #[test]
    fn a_name_resolution_failure_names_the_network_flag() {
        let text = "curl: (6) Could not resolve host: example.com".to_owned();
        let denied = finish(text.clone(), Exit::Code(6), true);
        assert!(denied.is_error);
        assert!(denied.text.ends_with(NETWORK_NOTICE), "{}", denied.text);
        assert_eq!(denied.text.matches(NETWORK_NOTICE).count(), 1);
        assert!(
            !finish(text.clone(), Exit::Code(6), false)
                .text
                .contains(NETWORK_NOTICE)
        );
        assert!(
            !finish(text, Exit::Code(0), true)
                .text
                .contains(NETWORK_NOTICE)
        );
        assert!(
            !finish("ordinary failure".to_owned(), Exit::Code(1), true)
                .text
                .contains(NETWORK_NOTICE)
        );
    }

    #[test]
    fn a_command_cannot_write_outside_the_workspace() {
        // The probe is written under the build's output directory, which no
        // sandbox grant covers. The temporary directory would not do: a
        // command may write there, because compilers and test runners need to.
        // Without the sandbox in the command path this succeeds, which is what
        // makes the assertion meaningful rather than decorative.
        let Some((_dir, context)) = sandboxed_workspace() else {
            return;
        };
        let tool = shell(4, 64 * 1024);
        let parent =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/sandbox-tests");
        std::fs::create_dir_all(&parent).expect("probe directory");
        let outside = parent.join("rune-sandbox-escape-probe.txt");
        let _ = std::fs::remove_file(&outside);

        // The command itself fails, so this asserts on the call rather than
        // through the success helper.
        let output = call(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": format!("echo escaped > {}", outside.display()),
                "yield_time_ms": 2_000,
            }),
        );
        assert!(
            !outside.exists(),
            "a sandboxed command wrote outside the workspace: {}",
            output.text
        );
        assert!(
            output.text.contains("not permitted") || output.is_error,
            "the refusal was not reported: {}",
            output.text
        );
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn a_command_can_still_write_inside_the_workspace() {
        // The companion to the test above: a sandbox that denied every write
        // would pass it, so this shows the restriction is scoped.
        let Some((_dir, context)) = sandboxed_workspace() else {
            return;
        };
        let tool = shell(4, 64 * 1024);
        let inside = context.workspace.join("written.txt");

        text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": format!("echo kept > {inside}"),
                "yield_time_ms": 2_000,
            }),
        );
        assert!(
            inside.exists(),
            "a sandboxed command could not write inside the workspace"
        );
    }

    #[test]
    fn starting_beyond_the_session_bound_fails_with_the_limit_code() {
        let tool = shell(1, 64 * 1024);
        let (_dir, context) = workspace();
        let first = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": long_sleep(),
                "yield_time_ms": 50,
            }),
        );
        let (id, _group) = running(&first);
        let err = tool
            .call(
                &serde_json::json!({
                    "action": "run",
                    "command": long_sleep(),
                    "yield_time_ms": 0,
                }),
                &context,
            )
            .expect_err("the second session was refused");
        assert_eq!(err.code(), ErrorCode::LimitExceeded);
        assert_eq!(tool.live_sessions(), 1);

        // A session that has ended stops occupying the bound.
        let _ = stop(&tool, &context, &id);
        let _ = text(
            &tool,
            &context,
            &serde_json::json!({ "action": "run", "command": exit_with(0) }),
        );
        let again = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": long_sleep(),
                "yield_time_ms": 50,
            }),
        );
        let (id, _group) = running(&again);
        let _ = stop(&tool, &context, &id);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_the_tool_ends_every_live_session() {
        let tool = shell(2, 64 * 1024);
        let (_dir, context) = workspace();
        let mut children = Vec::new();
        let mut groups = Vec::new();
        for _ in 0..2 {
            let started = text(
                &tool,
                &context,
                &serde_json::json!({
                    "action": "run",
                    "command": fork_and_wait(),
                    "yield_time_ms": 200,
                }),
            );
            let (id, group) = running(&started);
            children.push(forked_pid(&tool, &context, &id, &started));
            groups.push(group);
        }
        assert_eq!(tool.live_sessions(), 2);
        for child in &children {
            assert!(alive(child), "the forked child was not started");
        }

        drop(tool);
        assert!(
            wait_until(
                || children.iter().all(|child| !alive(child)),
                Duration::from_secs(10)
            ),
            "a dropped tool left a command running"
        );
        assert!(
            wait_until(
                || groups.iter().all(|group| !group_alive(group)),
                Duration::from_secs(10)
            ),
            "a dropped tool left a group running"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_forced_stop_ends_a_command_that_ignores_the_graceful_signal() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let (id, group) = start_ready(
            &tool,
            &context,
            "trap '' TERM; echo ready; while true; do sleep 0.05; done",
        );
        let stopped = text(
            &tool,
            &context,
            &serde_json::json!({ "action": "stop", "session_id": id, "force": true }),
        );
        assert!(stopped.contains("SIGKILL"), "{stopped}");
        assert!(
            wait_until(|| !group_alive(&group), Duration::from_secs(10)),
            "the group survived a forced stop"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_graceful_stop_escalates_when_the_group_ignores_it() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let (id, group) = start_ready(
            &tool,
            &context,
            "trap '' TERM; echo ready; while true; do sleep 0.05; done",
        );
        let stopped = stop(&tool, &context, &id);
        assert!(stopped.contains("SIGKILL"), "{stopped}");
        assert!(
            wait_until(|| !group_alive(&group), Duration::from_secs(10)),
            "the group survived the escalation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn completed_sessions_obey_retention_limit() {
        let tool = shell(1, 1024);
        let (_dir, context) = workspace();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let started = text(
                &tool,
                &context,
                &serde_json::json!({"action":"run", "command":"read line; echo LAST", "interactive":true, "yield_time_ms":0}),
            );
            let (id, _) = running(&started);
            let held = Arc::clone(lock(&tool.state).sessions.get(&id).expect("session"));
            held.process
                .write(b"go\n", Duration::from_secs(2))
                .expect("release");
            assert!(wait_until(
                || !held.process.is_running(),
                Duration::from_secs(10)
            ));
            ids.push(id);
        }
        assert_eq!(
            Slot::reserve(&tool).expect_err("retention full").code(),
            ErrorCode::LimitExceeded
        );
        assert_eq!(tool.live_sessions(), 2);
        let output = text(
            &tool,
            &context,
            &serde_json::json!({"action":"interact", "session_id":ids[0], "yield_time_ms":0}),
        );
        assert!(output.contains("LAST"));
        assert!(Slot::reserve(&tool).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn starting_another_command_keeps_unread_completed_output() {
        for code in [0, 7] {
            let tool = shell(1, 64 * 1024);
            let (_dir, context) = workspace();
            let command =
                format!("read line; echo got \"$line\"; echo error \"$line\" >&2; exit {code}");
            let started = text(
                &tool,
                &context,
                &serde_json::json!({
                    "action": "run",
                    "command": command,
                    "interactive": true,
                    "yield_time_ms": 0,
                }),
            );
            let (id, _group) = running(&started);

            // Let the yielded command finish without consuming its output.
            // Waiting for the recorded exit avoids a timing-dependent sleep.
            {
                let held = Arc::clone(lock(&tool.state).sessions.get(&id).expect("a session"));
                held.process
                    .write(b"LAST\n", Duration::from_secs(2))
                    .expect("release the command");
                assert!(wait_until(
                    || !held.process.is_running(),
                    Duration::from_secs(10)
                ));
            }

            // The completed session must release the only running slot while
            // keeping its output and permission target available.
            let other = text(
                &tool,
                &context,
                &serde_json::json!({ "action": "run", "command": "echo OTHER" }),
            );
            assert!(other.lines().any(|line| line == "OTHER"), "{other}");
            assert_eq!(tool.live_sessions(), 1);
            let arguments = serde_json::json!({
                "action": "interact",
                "session_id": id,
                "yield_time_ms": 0,
            });
            assert_eq!(tool.permission_target(&arguments), Some(command));
            let ended = call(&tool, &context, &arguments);
            assert_eq!(ended.is_error, code != 0, "{}", ended.text);
            assert!(
                ended.text.lines().any(|line| line == "got LAST"),
                "{}",
                ended.text
            );
            assert!(
                ended.text.lines().any(|line| line == "error LAST"),
                "{}",
                ended.text
            );
            assert!(
                ended.text.contains(&format!("exited with status {code}")),
                "{}",
                ended.text
            );
            assert_eq!(tool.live_sessions(), 0);
            let err = tool
                .call(&arguments, &context)
                .expect_err("the final output was already consumed");
            assert_eq!(err.code(), ErrorCode::NotFound);
            assert!(session_command(&id).is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_session_that_ended_is_reported_once_and_then_forgotten() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": echo_input(),
                "interactive": true,
                "yield_time_ms": 50,
            }),
        );
        let (id, _group) = running(&started);
        let mut ended = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "interact",
                "session_id": id,
                "chars": "bye\n",
                "yield_time_ms": 10_000,
            }),
        );
        // A read returns as soon as output arrives, and the exit can follow
        // the output by a moment on a loaded machine, so the next read is the
        // one that reports it.
        if !ended.contains("exited with status") {
            ended.push_str(&text(
                &tool,
                &context,
                &serde_json::json!({
                    "action": "interact",
                    "session_id": id,
                    "yield_time_ms": 10_000,
                }),
            ));
        }
        assert!(ended.contains("got bye"), "{ended}");
        assert!(ended.contains("exited with status 0"), "{ended}");
        assert_eq!(tool.live_sessions(), 0);

        let err = tool
            .call(
                &serde_json::json!({ "action": "interact", "session_id": id }),
                &context,
            )
            .expect_err("the session is gone");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_run_stops_its_process_group() {
        let tool = Shell::default();
        let (dir, context) = workspace();
        let pid_file = dir.path().join("pid");
        let cancellation = context.cancellation();
        let err = std::thread::scope(|scope| {
            let call = scope.spawn(|| {
                tool.call(
                    &serde_json::json!({
                        "action": "run",
                        "command": "echo $$ > pid; exec sleep 30",
                    }),
                    &context,
                )
            });
            assert!(
                wait_until(
                    || std::fs::read_to_string(&pid_file).is_ok_and(|pid| !pid.trim().is_empty()),
                    Duration::from_secs(10),
                ),
                "the command wrote its pid"
            );
            cancellation.cancel();
            call.join()
                .expect("the call thread")
                .expect_err("the call was cancelled")
        });
        assert_eq!(err.code(), ErrorCode::Cancelled);
        let pid = std::fs::read_to_string(&pid_file).expect("pid file");
        let pid = pid.trim();
        assert!(
            wait_until(|| !alive(pid), Duration::from_secs(1)),
            "process {pid} still runs after the cancelled call returned"
        );
    }

    #[test]
    fn a_cancelled_call_is_refused() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        context.cancellation().cancel();
        let err = tool
            .call(
                &serde_json::json!({ "action": "run", "command": "echo hi" }),
                &context,
            )
            .expect_err("the call was cancelled");
        assert_eq!(err.code(), ErrorCode::Cancelled);
    }

    #[test]
    fn an_unknown_action_is_named_in_the_error() {
        let (_dir, context) = workspace();
        let err = Shell::default()
            .call(&serde_json::json!({ "action": "start" }), &context)
            .expect_err("the action is unknown");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("action"));
    }

    #[test]
    fn a_missing_command_is_named_in_the_error() {
        let (_dir, context) = workspace();
        let err = Shell::default()
            .call(&serde_json::json!({ "action": "run" }), &context)
            .expect_err("the command is missing");
        assert_eq!(err.code(), ErrorCode::MissingField);
        assert_eq!(err.field(), Some("command"));
    }

    #[test]
    fn an_empty_command_is_refused() {
        let (_dir, context) = workspace();
        let err = Shell::default()
            .call(
                &serde_json::json!({ "action": "run", "command": "   " }),
                &context,
            )
            .expect_err("the command is empty");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("command"));
    }

    #[test]
    fn a_cwd_outside_every_root_is_refused_until_external_access_is_granted() {
        let tool = Shell::default();
        let (dir, context) = workspace();
        let err = tool
            .call(
                &serde_json::json!({ "action": "run", "command": print_cwd(), "cwd": ".." }),
                &context,
            )
            .expect_err("the directory is outside the workspace");
        assert_eq!(err.code(), ErrorCode::PathOutsideWorkspace);

        let allowed = context.clone().with_external_access(true);
        let observed = text(
            &tool,
            &allowed,
            &serde_json::json!({ "action": "run", "command": print_cwd(), "cwd": ".." }),
        );
        // Resolved, because the shell prints the resolved path.
        let parent = dir
            .path()
            .parent()
            .expect("a parent directory")
            .canonicalize()
            .expect("resolved");
        let parent = Utf8PathBuf::from_path_buf(parent).expect("utf8");
        assert!(shows_path(&observed, &parent), "{observed}");
    }

    #[cfg(unix)]
    #[test]
    fn a_cwd_reached_through_a_link_that_leads_outside_is_refused() {
        let tool = Shell::default();
        let (dir, context) = workspace();
        let elsewhere = tempfile::tempdir().expect("a temporary directory");
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("link")).expect("a link");
        let err = tool
            .call(
                &serde_json::json!({ "action": "run", "command": print_cwd(), "cwd": "link" }),
                &context,
            )
            .expect_err("the directory is outside the workspace");
        assert_eq!(err.code(), ErrorCode::PathOutsideWorkspace);
        assert_eq!(tool.live_sessions(), 0);
    }

    #[test]
    fn a_cwd_inside_the_workspace_is_used() {
        let tool = Shell::default();
        let (dir, context) = workspace();
        std::fs::create_dir(dir.path().join("sub")).expect("a directory");
        let observed = text(
            &tool,
            &context,
            &serde_json::json!({ "action": "run", "command": print_cwd(), "cwd": "sub" }),
        );
        // The shell prints the resolved path, so the expectation is resolved
        // too: on a host where a temporary directory resolves elsewhere, the
        // unresolved spelling would never appear in the output.
        let expected = dir.path().join("sub").canonicalize().expect("resolved");
        let expected = Utf8PathBuf::from_path_buf(expected).expect("utf8");
        assert!(shows_path(&observed, &expected), "{observed}");
    }

    #[test]
    fn a_cwd_that_does_not_exist_is_reported_as_missing() {
        let (_dir, context) = workspace();
        let err = Shell::default()
            .call(
                &serde_json::json!({ "action": "run", "command": print_cwd(), "cwd": "missing" }),
                &context,
            )
            .expect_err("the directory does not exist");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }

    #[test]
    fn an_unknown_session_lists_the_sessions_that_are_running() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": long_sleep(),
                "yield_time_ms": 50,
            }),
        );
        let (id, _group) = running(&started);
        let err = tool
            .call(
                &serde_json::json!({ "action": "interact", "session_id": "shell-nope-999" }),
                &context,
            )
            .expect_err("the session is unknown");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(
            err.detail()
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains(&id)),
            "{err}"
        );
        let _ = stop(&tool, &context, &id);
    }

    #[cfg(unix)]
    #[test]
    fn a_stopped_session_takes_no_more_input() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": echo_input_then_sleep(),
                "interactive": true,
                "yield_time_ms": 50,
            }),
        );
        let (id, _group) = running(&started);
        let _ = stop(&tool, &context, &id);
        let err = tool
            .call(
                &serde_json::json!({ "action": "interact", "session_id": id, "chars": "x\n" }),
                &context,
            )
            .expect_err("the session is gone");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }

    #[test]
    fn a_yield_window_beyond_the_maximum_is_capped() {
        let window = yield_window(
            &serde_json::json!({ "yield_time_ms": MAX_YIELD_MS + 1_000_000 }),
            DEFAULT_RUN_YIELD_MS,
        )
        .expect("a window");
        assert_eq!(window, Duration::from_millis(MAX_YIELD_MS));
        let default = yield_window(&serde_json::json!({}), DEFAULT_INTERACT_MS).expect("a window");
        assert_eq!(default, Duration::from_millis(DEFAULT_INTERACT_MS));
    }
}
