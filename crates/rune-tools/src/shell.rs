//! The `shell` tool: commands run under a shell and kept as sessions.
//!
//! A session belongs to the tool instance rather than to the call that started
//! it, so a command that outlives its call can be observed and stopped later.
//! Every session runs in its own process group, which is what lets a stop end a
//! whole tree rather than the one process Rune spawned.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use camino::Utf8Path;
use rune_core::LimitName;
use rune_core::budget::BudgetSet;
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput};
use crate::workspace::{FileLimits, bool_arg, resolve, string_arg, truncate_to_bytes, usize_arg};
use rune_exec::command::Exit;
use rune_exec::session::{POLL_INTERVAL, Process};

/// Yield window a `run` uses when none is requested.
pub const DEFAULT_RUN_YIELD_MS: u64 = 30_000;

/// Yield window an `interact` uses when none is requested.
pub const DEFAULT_INTERACT_MS: u64 = 5_000;

/// Largest yield window a caller may request.
///
/// Bounded so one call cannot hold a turn open indefinitely; a command that
/// outlives its window is returned as a session instead.
pub const MAX_YIELD_MS: u64 = 600_000;

/// Time spent collecting the last bytes a command writes as it exits.
const DRAIN_WINDOW: Duration = Duration::from_millis(250);

/// Runs commands and holds the ones that outlive their call.
#[derive(Debug)]
pub struct Shell {
    limits: FileLimits,
    max_sessions: usize,
    state: Mutex<State>,
}

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
    started: u64,
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
    /// A command whose process has ended holds nothing worth reading, so it
    /// stops occupying the bound as soon as another command starts.
    fn reserve(shell: &'a Shell) -> Result<(Self, String)> {
        let mut state = lock(&shell.state);
        state.sessions.retain(|_, held| held.process.is_running());
        let live = state.sessions.len().saturating_add(state.starting);
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
        state.started = state.started.saturating_add(1);
        let id = session_id(state.started);
        Ok((Self { shell, held: true }, id))
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
    command: String,
    process: Process,
    seen: Mutex<(usize, usize)>,
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
        }
    }

    /// Returns the number of sessions currently held.
    #[must_use]
    pub fn live_sessions(&self) -> usize {
        lock(&self.state).sessions.len()
    }

    /// Starts a command and returns either its finished output or a session.
    fn run(&self, arguments: &serde_json::Value, context: &ExecutionContext) -> Result<ToolOutput> {
        let command = string_arg(arguments, "command")?
            .ok_or_else(|| RuneError::missing_field("command"))?
            .to_owned();
        if command.trim().is_empty() {
            return Err(RuneError::invalid_field("command", "the command is empty"));
        }
        let window = yield_window(arguments, DEFAULT_RUN_YIELD_MS)?;
        let cwd = match string_arg(arguments, "cwd")? {
            Some(raw) => Some(resolve(context, raw)?.path),
            None => None,
        };
        let cap = self.limits.output_cap(context);
        // The slot is reserved before the command is spawned, so two starts
        // racing for the last one cannot both win it, and a start that fails or
        // returns a finished command gives the slot back.
        let (mut slot, id) = Slot::reserve(self)?;

        // The start happens outside the map lock, because a command that runs
        // for its whole yield window would otherwise hold every other call up.
        let process = start(&command, cwd.as_deref(), cap, context)?;
        if let Some(exit) = wait_for_exit(&process, window, context)? {
            process.drain_output(DRAIN_WINDOW);
            let text = observe(&process, &command, &exit.describe(), &mut (0, 0), cap);
            return Ok(finish(text, exit));
        }

        let state = format!("session {id} running, {}", ended_by(process.id()));
        let mut seen = (0, 0);
        let text = observe(&process, &command, &state, &mut seen, cap);
        let session = Arc::new(Session {
            command,
            seen: Mutex::new(seen),
            process,
        });
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
        if let Some(chars) = chars {
            session.process.write(chars.as_bytes()).map_err(|err| {
                RuneError::new(
                    ErrorCode::InvalidState,
                    format!("session `{id}` did not accept input: {err}"),
                )
            })?;
        }

        if session.process.exit().is_none() {
            wait_for_progress(&session.process, window, baseline, context)?;
        }
        let ended = session.process.exit();
        if ended.is_some() {
            session.process.drain_output(DRAIN_WINDOW);
        }
        let state = match ended {
            Some(exit) => format!("session {id} ended: {}", exit.describe()),
            None => format!("session {id} running, {}", ended_by(session.process.id())),
        };
        let mut seen = lock(&session.seen);
        let text = observe(&session.process, &session.command, &state, &mut seen, cap);
        if let Some(exit) = ended {
            // A session that has ended is observed once more and then dropped,
            // so a later call cannot report output that was already returned.
            let _ = lock(&self.state).sessions.remove(&id);
            return Ok(finish(text, exit));
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
            Some(exit) => format!("session {id} ended: {}", exit.describe()),
            None => format!("session {id} survived the grace period"),
        };
        let mut seen = lock(&session.seen);
        let text = observe(&session.process, &session.command, &state, &mut seen, cap);
        // A stop that ended the group succeeded even though the command was
        // killed: the signal is how it was asked to stop, not a failure of the
        // stop itself, and the status is stated in the result.
        if ended {
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

impl Tool for Shell {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Run a command under a shell. `run` starts it and returns either the finished output and \
         its exit status, or a session for a command that is still working. `interact` then reads \
         what a session produced since the last read, and sends input when chars is given. `stop` \
         ends a session and everything it started."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["run", "interact", "stop"],
                    "description": "Start a command, read from a session, or end one.",
                },
                "command": {
                    "type": "string",
                    "description": "Command line for the shell, passed through unmodified. Required by `run`.",
                },
                "cwd": {
                    "type": "string",
                    "description": "Directory the command starts in. Defaults to the workspace root.",
                },
                "session_id": {
                    "type": "string",
                    "description": "Session returned by `run`. Required by `interact` and `stop`.",
                },
                "chars": {
                    "type": "string",
                    "description": "Exact input written to the command, newline included when you write one.",
                },
                "yield_time_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_YIELD_MS,
                    "description": "Milliseconds to wait before returning. Zero returns at once.",
                },
                "force": {
                    "type": "boolean",
                    "description": "Skip the graceful stop and kill the group at once.",
                },
            },
            "required": ["action"],
            "additionalProperties": false,
        })
    }

    fn activity(&self) -> Activity {
        Activity::Execute
    }

    fn permission_target(&self, arguments: &serde_json::Value) -> Option<String> {
        arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        let action = Action::parse(
            string_arg(arguments, "action")?.ok_or_else(|| RuneError::missing_field("action"))?,
        )?;
        match action {
            Action::Run => self.run(arguments, context),
            Action::Interact => self.interact(arguments, context),
            Action::Stop => self.stop(arguments, context),
        }
    }
}

/// Mints the id for one session.
///
/// The process id is part of the name, so a transcript from one run cannot be
/// mistaken for a session another run holds.
fn session_id(started: u64) -> String {
    format!("shell-{}-{started}", std::process::id())
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
) -> Result<Process> {
    let workspace = cwd.unwrap_or(&context.workspace);
    // The shell tool exists to run shell commands, so its input always goes to a
    // shell: routing a bare word to direct argv would break builtins such as
    // `exit`.
    let prepared = rune_exec::command::prepare_shell(command, workspace, None, BTreeMap::new())?;
    let policy = rune_exec::SandboxPolicy::new(
        context.workspace.clone(),
        context.additional_roots.clone(),
        // The tool layer is not a policy decider: whether a command may reach
        // the network is settled before it runs, so the sandbox mirrors the
        // context rather than choosing.
        context.external_access,
    );
    // Reaching outside the workspace and skipping the sandbox are different
    // questions, so the second is read from its own field rather than borrowed
    // from the first.
    let wrapped = rune_exec::detect().wrap(&prepared, &policy, context.allow_unsandboxed)?;

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
    Process::start_argv(&wrapped.argv, Some(start_in.as_std_path()), cap).map_err(|err| {
        let hint = match cwd {
            Some(cwd) => format!("the command starts in `{cwd}`"),
            None => String::from("the command starts in the workspace root"),
        };
        RuneError::from(err).with_hint(hint)
    })
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
fn observe(
    process: &Process,
    command: &str,
    state: &str,
    seen: &mut (usize, usize),
    cap: usize,
) -> String {
    let stdout = process.stdout().since(seen.0);
    let stderr = process.stderr().since(seen.1);
    let mut body = String::from_utf8_lossy(&stdout.bytes).into_owned();
    body.push_str(&String::from_utf8_lossy(&stderr.bytes));
    seen.0 = stdout.next;
    seen.1 = stderr.next;

    // A stream whose capture stopped at the cap says so, because the bytes the
    // command printed and the bytes retained are then different numbers, and
    // only the pair makes the shortfall visible. The comparison is the retained
    // head against total production; what a caller has not yet read says
    // nothing about the capture.
    let clipped = [
        ("standard output", process.stdout().totals()),
        ("standard error", process.stderr().totals()),
    ];
    let mut note = String::new();
    for (name, (retained, total)) in clipped {
        if total > retained as u64 {
            if !note.is_empty() {
                note.push(' ');
            }
            let _ = write!(
                note,
                "[{name} capture stopped after {retained} bytes, the stream produced {total} bytes]"
            );
        }
    }
    render(command, body, &note, state, cap)
}

/// Renders the command, the bytes it produced, and the state it ended in.
///
/// The bytes are cut to the cap, and the cut is stated with the number of bytes
/// retained, because a result that is merely short looks like a command that
/// printed nothing.
fn render(command: &str, mut body: String, note: &str, state: &str, cap: usize) -> String {
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
        let mut kept = room;
        // The marker states how many bytes were kept, so its own length depends
        // on the answer. Two passes settle it; the loop is a bound, not a guess.
        for _ in 0..8 {
            let settled =
                truncate_to_bytes(&body, room.saturating_sub(marker(kept, total, cap).len())).len();
            if settled == kept {
                break;
            }
            kept = settled;
        }
        out.push_str(truncate_to_bytes(&body, kept));
        if room > kept {
            out.push_str(&marker(kept, total, cap));
        }
    }
    out.push_str(&note);
    out.push_str(&state);
    truncate_to_bytes(&out, cap).to_owned()
}

/// Bytes of output below which a supplementary note gives up its room.
const MIN_NOTE_BODY_BYTES: usize = 256;

/// Returns the marker that states how much of an output survived the cut.
fn marker(kept: usize, total: usize, cap: usize) -> String {
    format!("\n[output truncated: {kept} of {total} bytes retained, the cap is {cap} bytes]")
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
fn finish(text: String, exit: Exit) -> ToolOutput {
    if exit.is_success() {
        ToolOutput::success(text)
    } else {
        ToolOutput::failure(text)
    }
}

/// Resolves the yield window from the arguments.
fn yield_window(arguments: &serde_json::Value, default: u64) -> Result<Duration> {
    let Some(raw) = usize_arg(arguments, "yield_time_ms")? else {
        return Ok(Duration::from_millis(default));
    };
    let bounded = u64::try_from(raw).unwrap_or(u64::MAX).min(MAX_YIELD_MS);
    Ok(Duration::from_millis(bounded))
}

/// Waits for a process to end, or for the window to elapse.
fn wait_for_exit(
    process: &Process,
    window: Duration,
    context: &ExecutionContext,
) -> Result<Option<Exit>> {
    let deadline = Instant::now().checked_add(window);
    loop {
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
    loop {
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
    /// with what they assert. The tests that do assert the sandbox build their
    /// own context.
    fn workspace() -> (tempfile::TempDir, ExecutionContext) {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("a UTF-8 path");
        (
            dir,
            ExecutionContext::new(path).with_allow_unsandboxed(true),
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
    fn running(text: &str) -> (String, String) {
        let line = text
            .lines()
            .find(|line| line.contains(" running, "))
            .unwrap_or_else(|| panic!("no running session in {text}"));
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

    /// Returns true when `observed` contains `expected` as a path.
    ///
    /// A path is compared after both sides are put in the same spelling. One
    /// platform resolves a path to a form carrying a prefix that the shell it
    /// runs does not print, so a comparison of the raw strings fails on the
    /// directory the test itself created.
    fn shows_path(observed: &str, expected: &Utf8Path) -> bool {
        let rendered = expected.as_str();
        let stripped = rendered.strip_prefix(r"\\?\").unwrap_or(rendered);
        observed.contains(stripped) || observed.contains(rendered)
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
            // The loop is a builtin that ends on its own, so the wait after it
            // is reached once the lines are out.
            format!("{} & {}", numbered_lines(2000), long_sleep())
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

        // The count in the marker is the number of bytes actually kept.
        let marker = observed.find("[output truncated").expect("a marker");
        let reported: usize = observed[marker..]
            .split_once(": ")
            .and_then(|(_, rest)| rest.split_once(" of "))
            .and_then(|(kept, _)| kept.parse().ok())
            .expect("a retained count");
        let head = observed.find('\n').expect("the command line") + 1;
        assert_eq!(reported, marker - head - 1);
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
        assert!(started.contains("capture stopped after"), "{started}");
        assert!(started.contains("standard output"), "{started}");
        let (id, _group) = running(&started);
        let _ = stop(&tool, &context, &id);
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

    #[test]
    fn a_command_cannot_write_outside_the_workspace() {
        // The workspace is the temporary directory, so a write elsewhere is
        // outside it. Without the sandbox in the command path this succeeds,
        // which is what makes the assertion meaningful rather than decorative.
        let Some((_dir, context)) = sandboxed_workspace() else {
            return;
        };
        let tool = shell(4, 64 * 1024);
        let outside = std::env::temp_dir().join("rune-sandbox-escape-probe.txt");
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
    fn a_session_that_ended_is_reported_once_and_then_forgotten() {
        let tool = Shell::default();
        let (_dir, context) = workspace();
        let started = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "run",
                "command": echo_input(),
                "yield_time_ms": 50,
            }),
        );
        let (id, _group) = running(&started);
        let ended = text(
            &tool,
            &context,
            &serde_json::json!({
                "action": "interact",
                "session_id": id,
                "chars": "bye\n",
                "yield_time_ms": 10_000,
            }),
        );
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
