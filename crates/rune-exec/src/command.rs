//! Command execution in a supervised process group.
//!
//! A command is prepared before it runs, so the string a caller authorized and
//! the argv that actually runs are two views of one thing and can be compared.
//! The comparison is what closes the gap between review and execution: an alias,
//! a function, or a startup file can only change a command that is routed
//! through a shell, and a command that needs no shell syntax never goes near
//! one.
//!
//! A prepared command runs in a process group of its own, so ending it ends
//! everything it started rather than only the process Rune spawned. Signals are
//! delivered by running `kill` against the negated group id: the standard
//! library exposes no signal API, and this crate keeps unsafe code to the one
//! call that sets a child's resource limits.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, ErrorKind, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::budget::{BudgetSet, LimitName};
use rune_core::error::{ErrorCode, Result, RuneError};

/// Program that parses a command string when shell syntax is present.
///
/// The platform's own shell, because a command written for this machine is
/// written for the shell this machine has. A single hard-coded name would fail
/// everywhere the name does not exist.
#[cfg(unix)]
pub const DEFAULT_SHELL: &str = "/bin/sh";

/// Program that parses a command string when shell syntax is present.
#[cfg(not(unix))]
pub const DEFAULT_SHELL: &str = "cmd.exe";

/// Returns the shell that parses a command string on this host.
///
/// A shell has to be absolute to be verified as the program that was reviewed,
/// and the platform that names its shell keeps it under the system directory,
/// which is not always on the system drive. The conventional location is the
/// fallback for a host that will not say where it is, so the value is always a
/// path rather than a name to look up.
#[must_use]
pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").or_else(|| std::env::var_os("windir"));
        if let Some(root) = root {
            let candidate = Utf8PathBuf::from(root.to_string_lossy().into_owned())
                .join("System32")
                .join("cmd.exe");
            if candidate.exists() {
                return candidate.into_string();
            }
        }
    }
    DEFAULT_SHELL.to_owned()
}

/// Flag that hands a shell one string to interpret.
#[cfg(unix)]
pub const SHELL_COMMAND_FLAG: &str = "-c";

/// Flag that hands a shell one string to interpret.
#[cfg(not(unix))]
pub const SHELL_COMMAND_FLAG: &str = "/C";

/// Tokens that make a command more than a program and its arguments.
///
/// The quoting tokens belong in the list because word splitting here is
/// whitespace splitting: `"a  b"` is two arguments to a shell and three to a
/// split, so a command holding a quote has to be parsed by the shell.
const SHELL_TOKENS: [&str; 14] = [
    "|", "&", ";", ">", "<", "`", "$", "(", ")", "\n", "\"", "'", "\\", "#",
];

/// Tokens whose meaning depends on globbing, which only a shell performs.
const GLOB_TOKENS: [&str; 3] = ["*", "?", "["];

/// Prefix of the note appended to a stream that reached its byte cap.
pub const TRUNCATION_MARKER: &str = "[output truncated:";

/// Bytes read from a pipe in one call.
const CHUNK_BYTES: usize = 8 * 1024;

/// Programs that deliver a signal, tried in order.
#[cfg(unix)]
const KILL_PROGRAMS: [&str; 2] = ["/bin/kill", "kill"];

/// Signal sent to the group first.
const GRACEFUL_SIGNAL: &str = "TERM";

/// Signal sent when the grace period leaves something running.
const FORCE_SIGNAL: &str = "KILL";

/// Time a termination waits after each signal, and the time a reader thread is
/// given to finish once its process has been reaped.
pub const GRACE_PERIOD: Duration = Duration::from_millis(2_000);

/// Time between checks while waiting for a child.
pub const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A command resolved to the argv that runs it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PreparedCommand {
    /// The argv the command runs as, with the program first.
    pub argv: Vec<String>,
    /// Directory the command runs in.
    pub cwd: Utf8PathBuf,
    /// Environment the command runs with, and nothing else.
    pub environment: BTreeMap<String, String>,
    /// Resource ceiling the command runs under.
    pub limits: ResourceLimits,
    /// The exact string that was authorized.
    pub reviewed: String,
}

/// How a command ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Exit {
    /// The command exited with this status code.
    Code(i32),
    /// The command was ended by this signal.
    Signal(i32),
    /// The wait failed, so how it ended is unknown.
    Unknown,
}

impl Exit {
    /// Returns true when the command exited successfully.
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Code(0))
    }

    /// Returns a phrase describing how the command ended.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Code(code) => format!("exited with status {code}"),
            Self::Signal(signal) => match signal_name(signal) {
                Some(name) => format!("killed by signal {signal} ({name})"),
                None => format!("killed by signal {signal}"),
            },
            Self::Unknown => String::from("ended with an unknown status"),
        }
    }
}

/// What a finished command produced.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommandOutcome {
    /// Captured standard output, with the truncation note when it was capped.
    pub stdout: String,
    /// Captured standard error, with the truncation note when it was capped.
    pub stderr: String,
    /// How the command ended.
    pub exit: Exit,
    /// Wall time from starting the command to reaping it.
    pub duration_ms: u64,
    /// Whether standard output reached the byte cap.
    pub stdout_truncated: bool,
    /// Whether standard error reached the byte cap.
    pub stderr_truncated: bool,
    /// Bytes of standard output retained, excluding the truncation note.
    pub stdout_retained: usize,
    /// Bytes of standard error retained, excluding the truncation note.
    pub stderr_retained: usize,
    /// Whether the timeout ended the command rather than the command ending
    /// itself.
    pub timed_out: bool,
}

impl CommandOutcome {
    /// Returns the captured output that a reader would show, stream by stream.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = String::new();
        text.push_str(&self.stdout);
        if !self.stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&self.stderr);
        }
        text
    }
}

/// Resolves a command string to the argv that runs it.
///
/// A command that needs no shell syntax becomes the direct argv, which is the
/// route that cannot be re-interpreted: there is no shell to read an alias or a
/// function. A command that holds shell syntax runs as `shell -c <command>`,
/// with the string passed as one argument, so what the caller reviewed is
/// exactly what the shell parses.
///
/// `shell` must be an absolute path and is only used when a shell is needed.
/// The environment is used as given: nothing from the parent process is
/// inherited, so `PATH` has to be passed for a program to be resolved by name.
pub fn prepare(
    command: &str,
    workspace: &Utf8Path,
    shell: Option<&str>,
    environment: BTreeMap<String, String>,
) -> Result<PreparedCommand> {
    if command.trim().is_empty() {
        return Err(RuneError::invalid_field(
            "command",
            "a command cannot be empty",
        ));
    }
    let resolved = default_shell();
    let shell = shell.unwrap_or(&resolved);
    if shell_reason(command).is_some() && !Utf8Path::new(shell).is_absolute() {
        return Err(RuneError::invalid_field(
            "shell",
            format!("`{shell}` is not an absolute path"),
        )
        .with_hint("a shell resolved through PATH is not the program that was reviewed"));
    }
    Ok(PreparedCommand {
        argv: route(command, shell),
        cwd: workspace.to_owned(),
        environment,
        limits: resource_limits(),
        reviewed: command.to_owned(),
    })
}

/// Prepares a command that must be parsed by a shell.
///
/// The shell tool exists to run shell commands, so its input is always handed to
/// a shell: routing a bare word to direct argv would break builtins such as
/// `exit` and would silently ignore a function or alias the same way the direct
/// route is meant to avoid doing for an authorized command.
pub fn prepare_shell(
    command: &str,
    workspace: &Utf8Path,
    shell: Option<&str>,
    environment: BTreeMap<String, String>,
) -> Result<PreparedCommand> {
    if command.trim().is_empty() {
        return Err(RuneError::invalid_field(
            "command",
            "a command cannot be empty",
        ));
    }
    let resolved = default_shell();
    let shell = shell.unwrap_or(&resolved);
    if !Utf8Path::new(shell).is_absolute() {
        return Err(RuneError::invalid_field(
            "shell",
            format!("`{shell}` is not an absolute path"),
        )
        .with_hint("a shell resolved through PATH is not the program that was reviewed"));
    }
    Ok(PreparedCommand {
        argv: vec![
            shell.to_owned(),
            String::from(SHELL_COMMAND_FLAG),
            command.to_owned(),
        ],
        cwd: workspace.to_owned(),
        environment,
        limits: resource_limits(),
        reviewed: command.to_owned(),
    })
}

/// Environment variables a command may receive.
///
/// An allowlist rather than a denylist, because a credential can sit in a
/// variable of any name: `api_key_env` lets a user keep their key under a name
/// no pattern would recognize. Everything not named here is withheld, including
/// the provider credentials and session state this process holds.
///
/// What is named is what ordinary work breaks without. A program is resolved
/// through `PATH`, most tools find their configuration through `HOME` or the XDG
/// variables, a toolchain installed outside the default location is found
/// through its own home variable, and a machine behind a proxy or a private
/// certificate authority reaches nothing without those settings.
///
/// The names are a constant so the check that no other variable reaches a
/// command cannot drift from the list that decides it.
#[cfg(not(windows))]
pub const ALLOWED_ENVIRONMENT: &[&str] = &[
    // The process itself.
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TZ",
    // The terminal and the locale.
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "__CF_USER_TEXT_ENCODING",
    // Where configuration and caches live.
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    // Toolchains installed outside their default location.
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "GOPATH",
    "GOROOT",
    "GOBIN",
    "JAVA_HOME",
    "NVM_DIR",
    "PNPM_HOME",
    "BUN_INSTALL",
    "DENO_DIR",
    "PYENV_ROOT",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "PKG_CONFIG_PATH",
    "DEVELOPER_DIR",
    "SDKROOT",
    "ANDROID_HOME",
    "ANDROID_SDK_ROOT",
    // The network path a machine is configured to use.
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
];

/// Environment variables a command may receive.
///
/// Windows spells several of these differently and cannot start a system
/// program without `SystemRoot`, which is what a process needs to load the
/// libraries it links against. Omitting it fails in a way that names neither
/// the variable nor the cause.
#[cfg(windows)]
pub const ALLOWED_ENVIRONMENT: &[&str] = &[
    "PATH",
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERNAME",
    "USERDOMAIN",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "CommonProgramFiles",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    "PSModulePath",
    "TERM",
    "LANG",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "GOPATH",
    "GOROOT",
    "JAVA_HOME",
    "NVM_HOME",
    "NVM_SYMLINK",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
];

/// Returns the environment a command is given when the caller names none.
///
/// The variables are resolved once, here, rather than inherited, so a variable
/// added to the environment later in this process cannot leak into a command.
#[must_use]
pub fn minimal_environment() -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    for &name in ALLOWED_ENVIRONMENT {
        // A value this process holds that is not valid UTF-8 is left out
        // rather than lossily converted: a mangled path is worse than none.
        if let Some(value) = std::env::var_os(name)
            && let Ok(value) = value.into_string()
        {
            environment.insert(name.to_owned(), value);
        }
    }
    if !environment.contains_key("PATH") {
        environment.insert("PATH".to_owned(), default_path());
    }
    environment
}

/// Returns a search path for a process that has none of its own.
///
/// A command with no `PATH` cannot resolve a program by name at all, which
/// reads as "the program does not exist" rather than "the environment was
/// empty", so each platform gets a path that could actually work.
fn default_path() -> String {
    #[cfg(windows)]
    {
        String::from(r"C:\Windows\system32;C:\Windows")
    }
    #[cfg(not(windows))]
    {
        String::from("/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
    }
}

/// Returns true when a command has to be parsed by a shell.
#[must_use]
pub fn requires_shell(command: &str) -> bool {
    shell_reason(command).is_some()
}

/// Returns the syntax that forces a shell, or `None` for a plain command.
#[must_use]
pub fn shell_reason(command: &str) -> Option<&'static str> {
    if let Some(token) = SHELL_TOKENS
        .iter()
        .copied()
        .chain(GLOB_TOKENS.iter().copied())
        .find(|token| command.contains(*token))
    {
        return Some(token);
    }
    // A tilde beginning a word is home-directory expansion, which only a shell
    // performs: `echo ~` prints a path where the direct route would print `~`.
    command
        .split_whitespace()
        .any(|word| word.starts_with('~'))
        .then_some("~")
}

/// Returns the argv a command routes to.
fn route(command: &str, shell: &str) -> Vec<String> {
    if requires_shell(command) {
        vec![
            shell.to_owned(),
            String::from(SHELL_COMMAND_FLAG),
            command.to_owned(),
        ]
    } else {
        command.split_whitespace().map(str::to_owned).collect()
    }
}

/// Recomputes the argv from the reviewed string and refuses any drift.
///
/// The argv is rebuilt from the reviewed string with the shell the argv itself
/// names, so a command whose route changed between review and execution is
/// refused rather than run. Everything that changes what runs is refused: a
/// direct command routed through a shell, a shell script whose string is not
/// the reviewed one, an argument added or replaced, and a shell named without
/// an absolute path, since a program resolved through `PATH` is not the program
/// that was reviewed.
///
/// A different absolute shell is accepted, because the argv names the program
/// that runs and that program evaluates the reviewed string either way. What
/// matters is that the string executed is the string reviewed.
///
/// A sandbox wrapper prefixes the argv and leaves the route untouched, so a
/// wrapped command is verified through this function rather than after it.
pub fn verify_unchanged(prepared: &PreparedCommand) -> Result<()> {
    let shell = shell_invocation(prepared);
    if let Some(shell) = shell
        && !Utf8Path::new(shell).is_absolute()
    {
        return Err(route_changed(prepared));
    }
    let expected = prepare(
        &prepared.reviewed,
        &prepared.cwd,
        shell,
        prepared.environment.clone(),
    )?;
    let expected = PreparedCommand {
        limits: prepared.limits,
        ..expected
    };
    if expected.argv == prepared.argv {
        Ok(())
    } else {
        Err(route_changed(prepared))
    }
}

/// Returns the shell an argv invokes the reviewed string with, if it does.
fn shell_invocation(prepared: &PreparedCommand) -> Option<&str> {
    match prepared.argv.as_slice() {
        [shell, flag, command] if flag == SHELL_COMMAND_FLAG && command == &prepared.reviewed => {
            Some(shell)
        }
        _ => None,
    }
}

/// Builds the error for a command that no longer routes to what was reviewed.
fn route_changed(prepared: &PreparedCommand) -> RuneError {
    RuneError::new(
        ErrorCode::InvalidState,
        format!(
            "`{}` is not the command this argv runs; the route changed after it was reviewed",
            prepared.reviewed
        ),
    )
    .with_invariant("prepared_argv")
    .with_observed(prepared.argv.join(" "))
    .with_hint("review the command again before running it")
}

/// Runs a prepared command with the default limits.
///
/// A timeout or a cancellation ends the process group, which is why the outcome
/// still carries whatever the command produced before it was ended.
pub fn run(
    prepared: &PreparedCommand,
    timeout: Duration,
    cancel: &dyn Fn() -> bool,
) -> Result<CommandOutcome> {
    run_with_limits(prepared, timeout, cancel, &BudgetSet::new())
}

/// Runs a prepared command under a specific set of limits.
///
/// Each stream retains at most `command_output_bytes`, counted separately. A
/// zero timeout ends the command as soon as it has started; there is no way to
/// ask for no deadline at all, because a command that never returns is what the
/// deadline exists to stop.
pub fn run_with_limits(
    prepared: &PreparedCommand,
    timeout: Duration,
    cancel: &dyn Fn() -> bool,
    limits: &BudgetSet,
) -> Result<CommandOutcome> {
    let cap = limits.get_usize(LimitName::CommandOutputBytes);
    let Some((program, arguments)) = prepared.argv.split_first() else {
        return Err(RuneError::invalid_field(
            "argv",
            "a prepared command needs a program to run",
        ));
    };
    if program.is_empty() {
        return Err(RuneError::invalid_field(
            "argv",
            "the program of a prepared command cannot be empty",
        ));
    }

    let started = Instant::now();
    let mut child = start(program, arguments, prepared)?;
    let group = child.id();
    let stdout = Arc::new(Stream::new(cap));
    let stderr = Arc::new(Stream::new(cap));
    let stdout_reader = pump("rune-exec-stdout", child.stdout.take(), Arc::clone(&stdout))?;
    let stderr_reader = pump("rune-exec-stderr", child.stderr.take(), Arc::clone(&stderr))?;

    let (exit, timed_out) = supervise(&mut child, group, timeout, cancel)?;

    // The pipes close when the group ends, so the readers finish here. A reader
    // that has not finished is left detached rather than blocking the caller,
    // because its capture is shared and already readable.
    settle(stdout_reader);
    settle(stderr_reader);

    let (stdout, stdout_truncated, stdout_retained) = stdout.render();
    let (stderr, stderr_truncated, stderr_retained) = stderr.render();
    Ok(CommandOutcome {
        stdout,
        stderr,
        exit,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        stdout_truncated,
        stderr_truncated,
        stdout_retained,
        stderr_retained,
        timed_out,
    })
}

/// Starts a prepared command in its own process group.
fn start(program: &str, arguments: &[String], prepared: &PreparedCommand) -> Result<Child> {
    let mut spec = Command::new(program);
    spec.args(arguments)
        // Nothing from the parent environment is inherited, so a variable the
        // caller did not pass cannot reach the command.
        .env_clear()
        .envs(&prepared.environment)
        .current_dir(prepared.cwd.as_std_path())
        // A command runs without a terminal, so reading standard input fails
        // rather than waiting for a prompt nobody can answer.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_group(&mut spec);
    // The ceiling is applied in the child, before it executes.
    apply_limits(&mut spec, prepared.limits);
    spec.spawn().map_err(|err| spawn_error(program, &err))
}

/// Waits for a command, ending its group when the deadline passes or the caller
/// cancels.
fn supervise(
    child: &mut Child,
    group: u32,
    timeout: Duration,
    cancel: &dyn Fn() -> bool,
) -> Result<(Exit, bool)> {
    let deadline = Instant::now().checked_add(timeout);
    loop {
        if let Some(status) = try_wait(child)? {
            return Ok((classify(status), false));
        }
        if cancel() {
            end_group(child, group);
            return Ok((classify(wait(child)?), false));
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            end_group(child, group);
            return Ok((classify(wait(child)?), true));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Ends a process group: one graceful signal, a grace period, then force.
///
/// The forceful signal is sent whether or not the leader has been reaped. The
/// leader can exit on the graceful signal while a member of its group stays
/// alive, and a signal that outlives its leader is an orphan.
fn end_group(child: &mut Child, group: u32) {
    signal_group(group, GRACEFUL_SIGNAL);
    let deadline = Instant::now().checked_add(GRACE_PERIOD);
    loop {
        if try_wait(child).is_ok_and(|status| status.is_some()) {
            break;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    if !signal_group(group, FORCE_SIGNAL) {
        let _ = child.kill();
    }
}

/// Waits for a child whose group has been signalled.
fn wait(child: &mut Child) -> Result<ExitStatus> {
    child.wait().map_err(|err| {
        RuneError::new(
            ErrorCode::Internal,
            format!("the child process could not be reaped: {err}"),
        )
    })
}

/// Returns the status of a child that has ended, without blocking.
fn try_wait(child: &mut Child) -> Result<Option<ExitStatus>> {
    child.try_wait().map_err(|err| {
        RuneError::new(
            ErrorCode::Internal,
            format!("the child process could not be checked: {err}"),
        )
    })
}

/// Returns the error for a command that could not be started.
fn spawn_error(program: &str, err: &io::Error) -> RuneError {
    let code = match err.kind() {
        ErrorKind::NotFound => ErrorCode::NotFound,
        ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        _ => ErrorCode::InvalidState,
    };
    let error = RuneError::new(code, format!("`{program}` could not be started: {err}"))
        .with_observed(err.kind().to_string());
    if err.kind() == ErrorKind::NotFound {
        error.with_hint("pass the program as an absolute path, or pass PATH in the environment")
    } else {
        error
    }
}

/// Starts a child in a process group of its own.
pub fn own_group(spec: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        spec.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = spec;
    }
}

/// Applies a hard resource ceiling to a child before it executes.
///
/// The wall-clock deadline in the supervisor stops a command that runs too
/// long, but it cannot stop one that exhausts a resource first: a process that
/// allocates until the host is out of memory, or forks until the process table
/// is full, takes the machine down in the seconds before the deadline is
/// noticed. These limits are the kernel refusing at the moment of the request
/// rather than this program noticing afterwards.
///
/// Every limit is set soft-equal-to-hard, so the child cannot raise its own
/// ceiling back up.
#[cfg(unix)]
#[allow(
    unsafe_code,
    reason = "pre_exec is the only point at which a child's limits can be set, \
              and it is unsafe by its own contract; the closure calls setrlimit \
              and nothing else"
)]
pub(crate) fn apply_limits(spec: &mut Command, limits: ResourceLimits) {
    use std::os::unix::process::CommandExt as _;

    // `pre_exec` runs in the child between fork and exec, which is the only
    // point where a limit can be set on a process that is already its own.
    // Everything here is async-signal-safe: rustix's `setrlimit` makes one
    // syscall and allocates nothing.
    //
    // SAFETY: the closure calls `setrlimit` only. A failure is returned so the
    // child refuses to exec rather than running unlimited, which is the point.
    unsafe {
        spec.pre_exec(move || limits.apply());
    }
}

#[cfg(not(unix))]
pub(crate) fn apply_limits(spec: &mut Command, limits: ResourceLimits) {
    let _ = (spec, limits);
}

/// A resource ceiling for one command.
///
/// A value of `None` leaves that limit alone, so a platform or a caller that
/// cannot express one is not made to invent a number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ResourceLimits {
    /// Longest a command may run, in seconds of CPU time.
    pub cpu_seconds: Option<u64>,
    /// Largest address space the command may claim, in bytes.
    pub address_space_bytes: Option<u64>,
    /// Most processes and threads the command and its children may create.
    ///
    /// This is what stops a fork bomb: past the ceiling, `fork` fails and the
    /// command cannot clone itself further.
    pub processes: Option<u64>,
    /// Largest file the command may create, in bytes.
    pub file_bytes: Option<u64>,
}

impl Default for ResourceLimits {
    /// The ceiling a command runs under unless a caller says otherwise.
    ///
    /// Chosen to be generous for real work and still bounded: four hours of CPU
    /// or an eight gigabyte file is not something this program should be doing
    /// unattended.
    ///
    /// Address space is left unbounded. It is reserved, not used: a JavaScript
    /// engine, a WebAssembly runtime, a JVM, and an address sanitizer each
    /// reserve many times the memory they touch, so any ceiling low enough to
    /// matter refuses ordinary test suites while bounding nothing real. A caller
    /// that wants one can still name it.
    fn default() -> Self {
        Self {
            cpu_seconds: Some(DEFAULT_CPU_SECONDS),
            address_space_bytes: None,
            processes: Some(DEFAULT_PROCESSES),
            file_bytes: Some(DEFAULT_FILE_BYTES),
        }
    }
}

/// Longest a command may run, in seconds of CPU time.
pub const DEFAULT_CPU_SECONDS: u64 = 4 * 60 * 60;
/// Largest address space a command may claim, for a caller that asks for one.
pub const DEFAULT_ADDRESS_SPACE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Most processes and threads a command may add to what is already running.
pub const DEFAULT_PROCESSES: u64 = 4096;
/// Largest file a command may create.
pub const DEFAULT_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[cfg(unix)]
impl ResourceLimits {
    /// Sets every configured limit on the calling process.
    ///
    /// Called in the child after `fork` and before `exec`, which is why it
    /// returns the platform error rather than a formatted one: the only thing
    /// that can be done with a failure here is to refuse to run.
    ///
    /// A ceiling is lowered to what the platform will accept rather than
    /// refused. macOS caps the process count below any useful value and its
    /// hard maximum is the system's, not this program's, so treating "the
    /// system already forbids more than we asked" as a failure would refuse to
    /// run every command on that platform. A ceiling that cannot be expressed
    /// at all is skipped, and named in the module documentation, rather than
    /// reported as applied.
    fn apply(&self) -> io::Result<()> {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

        #[cfg(target_os = "linux")]
        let wanted = [
            (Resource::Cpu, self.cpu_seconds),
            (Resource::Nproc, self.processes),
            (Resource::Fsize, self.file_bytes),
            (Resource::As, self.address_space_bytes),
        ];
        // macOS has no address space limit worth setting: it rejects a value
        // below its own reservation, which is far larger than any ceiling a
        // command should have, so asking would fail the spawn rather than bound
        // the command.
        #[cfg(not(target_os = "linux"))]
        let wanted = [
            (Resource::Cpu, self.cpu_seconds),
            (Resource::Nproc, self.processes),
            (Resource::Fsize, self.file_bytes),
        ];

        for (resource, value) in wanted {
            let Some(value) = value.filter(|value| *value > 0) else {
                continue;
            };
            // The system's hard maximum is a ceiling this program cannot raise,
            // and does not need to: a command already cannot exceed it.
            let hard = getrlimit(resource).maximum;
            let value = match hard {
                Some(hard) if hard < value => hard,
                _ => value,
            };
            if value == 0 {
                continue;
            }
            // Soft equals hard: the child cannot raise its own ceiling.
            setrlimit(
                resource,
                Rlimit {
                    current: Some(value),
                    maximum: Some(value),
                },
            )
            .map_err(io::Error::from)?;
        }
        Ok(())
    }
}

/// Returns the limits a command runs under.
///
/// A caller may name stricter ones; there is no way to name looser ones, because
/// a limit that can be raised by the thing it constrains is not a limit.
///
/// The process ceiling is headroom over what is already running. Linux counts
/// it against every task the user owns, threads included, so a fixed number
/// would refuse the first `fork` of a command started on a desktop that already
/// runs a browser and an editor. Adding the tasks on the host keeps a fork bomb
/// bounded while leaving ordinary work its full allowance.
#[must_use]
pub fn resource_limits() -> ResourceLimits {
    let mut limits = ResourceLimits::default();
    limits.processes = limits
        .processes
        .map(|headroom| headroom.saturating_add(tasks_on_host()));
    limits
}

/// Returns how many tasks the host is running, or zero where that is not known.
///
/// On Linux the fourth field of `/proc/loadavg` is `running/total`, where the
/// total counts every thread on the system. That is an upper bound on what the
/// user owns, which is the count the process ceiling is measured against.
fn tasks_on_host() -> u64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|text| {
                let field = text.split_whitespace().nth(3)?;
                field.split('/').nth(1)?.parse::<u64>().ok()
            })
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Delivers no signal, on a platform with no process group to signal.
///
/// Callers fall back to ending the direct child, which is the only process the
/// standard library can stop without a signal.
#[cfg(not(unix))]
fn signal_group(_group: u32, _name: &str) -> bool {
    false
}

/// Delivers a signal to a process group.
///
/// Returns whether a signal program ran. A group that has already ended cannot
/// be signalled, and there is nothing left to stop, so the caller treats a
/// failed delivery as complete.
#[cfg(unix)]
fn signal_group(group: u32, name: &str) -> bool {
    let target = format!("-{group}");
    KILL_PROGRAMS.iter().any(|program| {
        Command::new(program)
            .args(["-s", name, "--", target.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    })
}

/// Returns how a child ended.
fn classify(status: ExitStatus) -> Exit {
    #[cfg(unix)]
    if let Some(signal) = terminating_signal(status) {
        return Exit::Signal(signal);
    }
    match status.code() {
        Some(code) => Exit::Code(code),
        None => Exit::Unknown,
    }
}

/// Returns the signal that ended a child, when one did.
#[cfg(unix)]
fn terminating_signal(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

/// Names the signals a command is likely to be ended by.
pub(crate) fn signal_name(signal: i32) -> Option<&'static str> {
    match signal {
        1 => Some("SIGHUP"),
        2 => Some("SIGINT"),
        3 => Some("SIGQUIT"),
        6 => Some("SIGABRT"),
        9 => Some("SIGKILL"),
        13 => Some("SIGPIPE"),
        15 => Some("SIGTERM"),
        _ => None,
    }
}

/// Bytes retained from one stream and how many it produced.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct Capture {
    /// The retained head of the stream.
    bytes: Vec<u8>,
    /// Bytes the stream produced in total.
    produced: u64,
    /// Whether bytes were dropped after the cap.
    truncated: bool,
}

/// One captured stream, shared with the thread that reads it.
#[derive(Debug)]
struct Stream {
    state: Mutex<Capture>,
    cap: usize,
}

impl Stream {
    /// Builds a stream that retains at most `cap` bytes.
    fn new(cap: usize) -> Self {
        Self {
            state: Mutex::new(Capture::default()),
            cap,
        }
    }

    /// Appends bytes read from the stream, dropping what does not fit.
    fn push(&self, chunk: &[u8]) {
        let mut state = lock(&self.state);
        state.produced = state
            .produced
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        let room = self.cap.saturating_sub(state.bytes.len()).min(chunk.len());
        // The head of the stream is kept rather than the tail: the first bytes
        // say what the command is and what it complained about, while the tail
        // of a runaway command repeats itself.
        state.bytes.extend_from_slice(&chunk[..room]);
        state.truncated = state.produced > u64::try_from(self.cap).unwrap_or(u64::MAX);
    }

    /// Returns the text, whether it was truncated, and the retained byte count.
    fn render(&self) -> (String, bool, usize) {
        let state = lock(&self.state);
        let mut text = String::from_utf8_lossy(&state.bytes).into_owned();
        if state.truncated {
            let _ = write!(
                text,
                "\n{TRUNCATION_MARKER} {} of {} bytes retained]",
                state.bytes.len(),
                state.produced
            );
        }
        (text, state.truncated, state.bytes.len())
    }
}

/// Starts the thread that copies one pipe into the stream that belongs to it.
fn pump<R: Read + Send + 'static>(
    name: &str,
    stream: Option<R>,
    capture: Arc<Stream>,
) -> Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let Some(mut stream) = stream else {
                return;
            };
            let mut chunk = [0_u8; CHUNK_BYTES];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => capture.push(&chunk[..read]),
                }
            }
        })
        .map_err(|err| {
            RuneError::new(
                ErrorCode::Internal,
                format!("a reader thread could not be started: {err}"),
            )
        })
}

/// Waits briefly for a reader thread, then detaches it.
fn settle(handle: JoinHandle<()>) {
    let deadline = Instant::now().checked_add(GRACE_PERIOD);
    while !handle.is_finished() {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            // A process that left its group can hold a pipe open for its whole
            // life, and the capture is shared, so the thread is left to finish
            // on its own instead of blocking the caller behind it.
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
    let _ = handle.join();
}

/// Locks a mutex, ignoring poisoning.
///
/// A reader thread that panicked leaves a valid capture behind, and a caller
/// still needs whatever the command produced.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[cfg(unix)]
    use rune_core::budget::Budget;
    #[cfg(unix)]
    use rune_core::config::Layer;
    use tempfile::TempDir;

    /// Bytes retained by the truncation test.
    #[cfg(unix)]
    const TEST_CAP: u64 = 1_000;

    /// Returns a condition that never cancels.
    fn never() -> bool {
        false
    }

    /// Returns a temporary workspace and the guard that owns it.
    fn tempdir() -> (TempDir, Utf8PathBuf) {
        let dir = TempDir::new().expect("temporary directory");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("a UTF-8 path");
        (dir, path)
    }

    /// Waits for a condition, polling until the deadline.
    #[cfg(unix)]
    fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if condition() {
                return true;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return false;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Returns true while a process id exists.
    #[cfg(unix)]
    fn alive(pid: &str) -> bool {
        Command::new("/bin/kill")
            .args(["-0", pid])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Returns the parent `PATH`, which is the only variable a test passes on.
    fn environment() -> BTreeMap<String, String> {
        let mut environment = BTreeMap::new();
        let path = std::env::var("PATH").unwrap_or_else(|_| String::from("/usr/bin:/bin"));
        environment.insert(String::from("PATH"), path);
        environment
    }

    /// Returns a limit set that caps one stream at `bytes`.
    #[cfg(unix)]
    fn limits(bytes: u64) -> BudgetSet {
        let mut limits = BudgetSet::new();
        limits
            .set(
                LimitName::CommandOutputBytes,
                Budget::Bounded(bytes),
                Layer::Default,
            )
            .expect("a valid limit");
        limits
    }

    /// Waits for a shell to record the id of the child it forked.
    #[cfg(unix)]
    fn recorded_pid(path: &Utf8Path) -> String {
        let mut found = String::new();
        assert!(
            wait_until(
                || {
                    if let Ok(text) = std::fs::read_to_string(path) {
                        found = text.trim().to_owned();
                    }
                    !found.is_empty()
                },
                Duration::from_secs(10)
            ),
            "the shell never recorded the child it forked at {path}"
        );
        found
    }

    #[test]
    fn a_plain_command_routes_to_a_direct_argv() {
        let (_dir, dir) = tempdir();
        let prepared =
            prepare("echo hello world", dir.as_path(), None, environment()).expect("prepare");
        assert_eq!(prepared.argv, ["echo", "hello", "world"]);
        assert_eq!(prepared.reviewed, "echo hello world");
        assert!(!requires_shell("echo hello world"));
    }

    #[test]
    fn shell_syntax_routes_through_the_shell_with_the_reviewed_string_verbatim() {
        let (_dir, dir) = tempdir();
        for command in [
            "echo a | cat",
            "echo a && echo b",
            "echo a > out.txt",
            "ls *.rs",
            "echo $HOME",
            "echo \"quoted  spaces\"",
            "cd ~/src",
            "echo a # comment",
        ] {
            let prepared = prepare(command, dir.as_path(), None, environment()).expect("prepare");
            assert_eq!(
                prepared.argv,
                [default_shell().as_str(), SHELL_COMMAND_FLAG, command],
                "`{command}` did not route through the shell"
            );
            assert_eq!(prepared.reviewed, command);
        }
    }

    #[test]
    fn a_caller_supplied_shell_is_used_and_has_to_be_absolute() {
        let (_dir, dir) = tempdir();
        // The shell a caller names is whatever they name, and the rule is about
        // the shape of the path rather than about which shell it is, so the
        // fixture names one this host has.
        let named = default_shell();
        let prepared =
            prepare("echo a | cat", dir.as_path(), Some(&named), environment()).expect("prepare");
        assert_eq!(
            prepared.argv,
            [named.as_str(), SHELL_COMMAND_FLAG, "echo a | cat"]
        );

        let bare = Utf8Path::new(&named)
            .file_name()
            .expect("the shell has a file name");
        let relative = prepare("echo a | cat", dir.as_path(), Some(bare), environment());
        assert_eq!(
            relative.expect_err("a relative shell").code(),
            ErrorCode::InvalidField
        );
    }

    #[test]
    fn an_empty_command_is_refused() {
        let (_dir, dir) = tempdir();
        let error = prepare("   ", dir.as_path(), None, environment()).expect_err("empty");
        assert_eq!(error.code(), ErrorCode::InvalidField);
    }

    #[test]
    #[cfg(unix)]
    fn a_command_runs_in_its_working_directory_and_reports_both_streams() {
        let (_dir, dir) = tempdir();
        let prepared = prepare(
            "pwd; echo out; echo err 1>&2; exit 3",
            dir.as_path(),
            None,
            environment(),
        )
        .expect("prepare");
        let outcome = run(&prepared, Duration::from_secs(10), &never).expect("run");
        assert_eq!(outcome.exit, Exit::Code(3));
        assert!(!outcome.exit.is_success());
        assert!(outcome.stdout.contains("out"), "{}", outcome.stdout);
        assert!(outcome.stderr.contains("err"), "{}", outcome.stderr);
        let combined = outcome.text();
        assert!(combined.contains("out"), "{combined}");
        assert!(combined.contains("err"), "{combined}");
        assert!(
            combined.find("out") < combined.find("err"),
            "the combined text does not lead with standard output: {combined}"
        );
        assert!(!outcome.stdout_truncated);
        assert!(!outcome.timed_out);
        assert!(
            outcome.stdout.contains(
                &std::fs::canonicalize(dir.as_path())
                    .expect("canonical workspace")
                    .to_string_lossy()
                    .into_owned()
            ) || outcome.stdout.contains(dir.as_path().as_str()),
            "the command did not run in the workspace: {}",
            outcome.stdout
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_command_ended_by_a_signal_reports_the_signal() {
        let (_dir, dir) = tempdir();
        let prepared = prepare("kill -9 $$", dir.as_path(), None, environment()).expect("prepare");
        let outcome = run(&prepared, Duration::from_secs(10), &never).expect("run");
        assert_eq!(outcome.exit, Exit::Signal(9));
        assert!(outcome.exit.describe().contains("SIGKILL"));
    }

    #[test]
    #[cfg(unix)]
    fn a_missing_program_is_reported_as_not_found() {
        let (_dir, dir) = tempdir();
        let prepared = prepare(
            "rune-exec-no-such-program",
            dir.as_path(),
            None,
            environment(),
        )
        .expect("prepare");
        let error = run(&prepared, Duration::from_secs(10), &never).expect_err("missing program");
        assert_eq!(error.code(), ErrorCode::NotFound);
    }

    #[test]
    #[cfg(unix)]
    fn output_over_the_cap_is_truncated_with_a_marker_and_the_retained_count() {
        let (_dir, dir) = tempdir();
        let prepared =
            prepare("seq 1 200000", dir.as_path(), None, environment()).expect("prepare");
        assert_eq!(prepared.argv[0], "seq");
        let outcome = run_with_limits(
            &prepared,
            Duration::from_secs(20),
            &never,
            &limits(TEST_CAP),
        )
        .expect("run");
        assert_eq!(outcome.exit, Exit::Code(0));
        assert!(outcome.stdout_truncated);
        assert_eq!(outcome.stdout_retained, 1_000);
        assert!(outcome.stdout.starts_with("1\n"), "{}", outcome.stdout);
        assert!(
            outcome
                .stdout
                .contains(&format!("{TRUNCATION_MARKER} 1000 of ")),
            "{}",
            outcome.stdout
        );
        assert!(!outcome.stderr_truncated);
        assert_eq!(outcome.stderr_retained, 0);
    }

    #[test]
    #[cfg(unix)]
    fn output_within_the_cap_is_not_marked() {
        let (_dir, dir) = tempdir();
        let prepared = prepare("seq 1 10", dir.as_path(), None, environment()).expect("prepare");
        let outcome = run_with_limits(
            &prepared,
            Duration::from_secs(10),
            &never,
            &limits(TEST_CAP),
        )
        .expect("run");
        assert!(!outcome.stdout_truncated);
        assert!(!outcome.stdout.contains(TRUNCATION_MARKER));
        assert_eq!(outcome.stdout_retained, outcome.stdout.len());
    }

    #[test]
    #[cfg(unix)]
    fn a_timeout_ends_the_whole_process_group() {
        let (_dir, dir) = tempdir();
        let pidfile = dir.join("child.pid");
        let mut environment = environment();
        environment.insert(String::from("RUNE_EXEC_PIDFILE"), pidfile.to_string());
        let prepared = prepare(
            "sleep 30 & echo $! > \"$RUNE_EXEC_PIDFILE\"; wait",
            dir.as_path(),
            None,
            environment,
        )
        .expect("prepare");
        assert_ne!(prepared.argv[0], "sleep");
        let started = Instant::now();
        let outcome = run(&prepared, Duration::from_millis(700), &never).expect("run");
        assert!(outcome.timed_out);
        assert!(
            matches!(outcome.exit, Exit::Signal(15 | 9)),
            "{}",
            outcome.exit.describe()
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the timeout did not end the command"
        );
        let child = recorded_pid(&pidfile);
        // An ended process can stay visible as a zombie until whoever
        // inherited it reaps it, and a signal probe succeeds on a zombie.
        assert!(
            wait_until(|| !alive(&child), Duration::from_secs(3)),
            "the forked child outlived the timeout"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_cancelled_run_ends_the_group_promptly() {
        let (_dir, dir) = tempdir();
        let pidfile = dir.join("child.pid");
        let mut environment = environment();
        environment.insert(String::from("RUNE_EXEC_PIDFILE"), pidfile.to_string());
        let prepared = prepare(
            "sleep 30 & echo $! > \"$RUNE_EXEC_PIDFILE\"; wait",
            dir.as_path(),
            None,
            environment,
        )
        .expect("prepare");

        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let watch = pidfile.clone();
        let setter = thread::spawn(move || {
            // Cancelling once the shell has reported its child makes the
            // cancellation, rather than the command ending, the reason the run
            // returns.
            let _ = recorded_pid(&watch);
            flag.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        let outcome = run(&prepared, Duration::from_secs(60), &|| {
            cancelled.load(Ordering::SeqCst)
        })
        .expect("run");
        let elapsed = started.elapsed();
        setter.join().expect("the cancelling thread");

        assert!(
            !outcome.timed_out,
            "the timeout ended the run, not cancellation"
        );
        assert!(
            elapsed < Duration::from_secs(20),
            "a cancelled run took {elapsed:?}"
        );
        assert!(
            matches!(outcome.exit, Exit::Signal(15 | 9)),
            "{}",
            outcome.exit.describe()
        );
        let child = recorded_pid(&pidfile);
        // An ended process can stay visible as a zombie until whoever
        // inherited it reaps it, and a signal probe succeeds on a zombie.
        assert!(
            wait_until(|| !alive(&child), Duration::from_secs(3)),
            "the forked child outlived the cancellation"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_command_cannot_read_a_credential_this_process_holds() {
        // The end-to-end form of the check above: a real command is spawned
        // while this process holds a credential, and asked to print its own
        // environment. Run the suite with the variable set to exercise it;
        // without it the assertion is vacuous but harmless.
        if std::env::var_os("OPENAI_API_KEY").is_none() {
            return;
        }
        let (_dir, dir) = tempdir();
        let prepared =
            prepare("/usr/bin/env", dir.as_path(), None, minimal_environment()).expect("prepare");
        let outcome = run(&prepared, Duration::from_secs(10), &never).expect("run");
        assert!(
            !outcome.stdout.contains("OPENAI_API_KEY"),
            "a command read a credential out of its environment:\n{}",
            outcome.stdout
        );
    }

    #[test]
    fn the_minimal_environment_carries_no_credential() {
        // The helper exists so a command cannot read back a secret this process
        // holds. It is asserted directly, because the leak it prevents is
        // invisible in normal output.
        let environment = minimal_environment();
        for &name in ALLOWED_ENVIRONMENT {
            // A name on the list may only be absent when the parent lacked it.
            assert!(
                environment.contains_key(name) || std::env::var_os(name).is_none(),
                "{name} was dropped when the parent had it"
            );
        }
        for name in environment.keys() {
            assert!(
                ALLOWED_ENVIRONMENT.contains(&name.as_str()),
                "`{name}` reached a command's environment"
            );
            // A key this process holds for a provider must never be among them.
            assert!(
                !name.ends_with("_API_KEY") && !name.starts_with("RUNE_"),
                "`{name}` looks like a secret that leaked"
            );
        }
        // A command with no PATH of its own cannot resolve a program by name,
        // which reads as the program missing rather than the environment being
        // empty, so one is always supplied.
        assert!(environment.contains_key("PATH"));
    }

    #[test]
    #[cfg(windows)]
    fn a_command_on_windows_can_load_its_system_libraries() {
        // A process without SystemRoot cannot load the libraries it is linked
        // against. PowerShell fails with "Loading managed Windows PowerShell
        // failed", which names neither the variable nor the cause.
        assert!(
            minimal_environment().contains_key("SystemRoot"),
            "a command would be unable to start a system program"
        );
    }

    #[test]
    fn the_minimal_environment_is_resolved_not_inherited() {
        // The values are copied when the environment is built, so what a
        // command receives cannot change underneath it. A variable this process
        // does not hold is absent, which is the observable half of that.
        let environment = minimal_environment();
        assert!(
            !environment.contains_key("RUNE_NOT_SET_ANYWHERE_PROBE"),
            "a variable this process does not hold reached a command"
        );
        // Every value is a copy: mutating the map cannot reach the parent.
        let mut copy = environment.clone();
        copy.insert("PATH".to_owned(), String::from("/nonexistent"));
        assert_ne!(
            copy.get("PATH"),
            minimal_environment().get("PATH"),
            "the environment is shared rather than copied"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_command_cannot_create_a_file_past_its_ceiling() {
        // The ceiling is checked by the kernel when the write happens, so this
        // is the observable half of it: a command that ignores every polite
        // stopping signal still cannot exceed its file size.
        let (_dir, dir) = tempdir();
        let prepared = prepare(
            "head -c 4096 /dev/zero > big; echo done; wc -c < big",
            dir.as_path(),
            None,
            environment(),
        )
        .expect("prepare");
        // The command is truncated mid-write, so the shell reports the write
        // failure rather than the size it hoped for.
        let mut prepared = prepared;
        prepared.limits = ResourceLimits {
            file_bytes: Some(1024),
            ..resource_limits()
        };
        let outcome = run(&prepared, Duration::from_secs(10), &never).expect("run");
        // The command is stopped by a signal, so the shell never reports the
        // size; the point is that it did not report 4096.
        assert!(
            !outcome.stdout.contains("4096"),
            "a file larger than the ceiling was created: {}",
            outcome.stdout
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_command_sees_the_ceiling_it_was_given() {
        // The ceiling is only real if the child inherits it. Asserting the
        // numbers on the struct proves nothing about the spawn: a limit that is
        // computed and never applied reads as protection that is not there.
        //
        // The file limit is compared against the default rather than against a
        // block count, because the unit `ulimit -f` reports in is the shell's
        // business and differs between platforms.
        let (_dir, dir) = tempdir();
        let ceiling = 4096;
        let mut prepared =
            prepare("ulimit -f; ulimit -t", dir.as_path(), None, environment()).expect("prepare");
        prepared.limits = ResourceLimits {
            cpu_seconds: Some(1234),
            file_bytes: Some(ceiling),
            ..resource_limits()
        };
        let bounded = run(&prepared, Duration::from_secs(10), &never).expect("run");

        let mut free =
            prepare("ulimit -f; ulimit -t", dir.as_path(), None, environment()).expect("prepare");
        free.limits = ResourceLimits {
            cpu_seconds: None,
            file_bytes: None,
            ..resource_limits()
        };
        let unbounded = run(&free, Duration::from_secs(10), &never).expect("run");

        assert!(
            bounded.stdout.contains("1234"),
            "the cpu ceiling did not reach the command: {}",
            bounded.stdout
        );
        assert_ne!(
            bounded.stdout.lines().next(),
            unbounded.stdout.lines().next(),
            "the file ceiling did not reach the command: {}",
            bounded.stdout
        );
    }

    #[test]
    #[cfg(unix)]
    fn limits_leave_a_command_that_stays_under_them_alone() {
        // The ceiling must not be so eager that ordinary work fails. This is
        // the guard against a limit that is technically applied and practically
        // fatal.
        let (_dir, dir) = tempdir();
        let prepared = prepare("echo hello", dir.as_path(), None, environment()).expect("prepare");
        let outcome = run(&prepared, Duration::from_secs(10), &never).expect("run");
        assert_eq!(outcome.stdout.trim(), "hello");
    }

    #[test]
    fn the_default_ceiling_leaves_address_space_alone() {
        // Runtimes reserve far more address space than they touch, so a
        // ceiling here refuses ordinary test suites and bounds nothing real.
        assert_eq!(resource_limits().address_space_bytes, None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn the_process_ceiling_is_headroom_over_what_is_running() {
        // The kernel counts every task the user owns against this limit, so a
        // fixed number would refuse the first fork on a busy desktop.
        let running = tasks_on_host();
        assert!(running > 0, "the host reported no tasks");
        assert!(
            resource_limits().processes.unwrap_or(0) >= running + DEFAULT_PROCESSES,
            "a command would start with less than its full allowance"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_default_ceiling_admits_work_a_build_needs() {
        // The numbers are only useful if an ordinary command runs under them.
        // A `cargo build` allocates and forks, and must not be refused.
        let limits = resource_limits();
        assert!(limits.cpu_seconds.unwrap_or(0) >= 60 * 60);
        assert!(limits.processes.unwrap_or(0) >= 256);
        assert!(limits.file_bytes.unwrap_or(0) >= 1024 * 1024 * 1024);
    }

    #[test]
    #[cfg(unix)]
    fn the_environment_holds_exactly_what_the_caller_passed() {
        let (_dir, dir) = tempdir();
        let prepared =
            prepare("/usr/bin/env", dir.as_path(), None, environment()).expect("prepare");
        let outcome = run(&prepared, Duration::from_secs(10), &never).expect("run");
        let names: Vec<&str> = outcome
            .stdout
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .collect();
        assert_eq!(names, ["PATH"], "the child saw {} variables", names.len());

        // The same check from inside a shell, where a leftover variable is the
        // difference between an empty line and a value.
        let leaker = prepare(
            "sh -c 'echo $RUNE_TEST_LEAK'",
            dir.as_path(),
            None,
            environment(),
        )
        .expect("prepare");
        let outcome = run(&leaker, Duration::from_secs(10), &never).expect("run");
        assert_eq!(outcome.exit, Exit::Code(0));
        assert_eq!(outcome.stdout.trim(), "");
    }

    #[test]
    #[cfg(unix)]
    fn a_variable_the_caller_passes_reaches_the_command() {
        let (_dir, dir) = tempdir();
        let mut environment = environment();
        environment.insert(String::from("RUNE_EXEC_MARK"), String::from("present"));
        let prepared = prepare(
            "sh -c 'echo $RUNE_EXEC_MARK'",
            dir.as_path(),
            None,
            environment,
        )
        .expect("prepare");
        let outcome = run(&prepared, Duration::from_secs(10), &never).expect("run");
        assert_eq!(outcome.stdout.trim(), "present");
    }

    #[test]
    fn a_route_change_is_refused() {
        let (_dir, dir) = tempdir();
        let prepared = prepare("echo hello", dir.as_path(), None, environment()).expect("prepare");
        let mut forged = prepared.clone();
        forged.argv = vec![
            String::from("/bin/sh"),
            String::from("-c"),
            String::from("echo hello"),
        ];
        let error = verify_unchanged(&forged).expect_err("a shell route for a direct command");
        assert_eq!(error.code(), ErrorCode::InvalidState);
        assert_eq!(error.detail().invariant.as_deref(), Some("prepared_argv"));

        let mut swapped = prepared.clone();
        swapped.argv = vec![String::from("echo"), String::from("goodbye")];
        assert!(
            verify_unchanged(&swapped).is_err(),
            "a changed argument was accepted"
        );

        let mut extra = prepared.clone();
        extra.argv.push(String::from("--now"));
        assert!(
            verify_unchanged(&extra).is_err(),
            "an extra argument was accepted"
        );
    }

    #[test]
    fn a_shell_route_that_evaluates_a_different_string_is_refused() {
        let (_dir, dir) = tempdir();
        let prepared =
            prepare("echo released", dir.as_path(), None, environment()).expect("prepare");

        let mut slipped = prepared.clone();
        slipped.argv = vec![
            String::from("/bin/sh"),
            String::from("-c"),
            String::from("echo compromised"),
        ];
        assert!(
            verify_unchanged(&slipped).is_err(),
            "a shell argv was accepted with a string other than the reviewed one"
        );

        // The reviewed string is rewritten while the argv keeps the original,
        // which is the same drift seen from the other side.
        let mut relabelled = prepared.clone();
        relabelled.reviewed = String::from("echo compromised");
        assert!(
            verify_unchanged(&relabelled).is_err(),
            "a changed reviewed string was accepted"
        );
    }

    #[test]
    fn a_direct_route_that_smuggles_a_shell_is_refused() {
        let (_dir, dir) = tempdir();
        let prepared = prepare("env", dir.as_path(), None, environment()).expect("prepare");
        // The reviewed string needs no shell, so an argv that adds one is a
        // changed route even though the program name is untouched.
        let mut smuggled = prepared.clone();
        smuggled.argv.insert(0, String::from("/bin/sh"));
        smuggled.argv.insert(1, String::from("-c"));
        assert!(verify_unchanged(&smuggled).is_err());
    }

    #[test]
    fn a_sandbox_wrapper_leaves_the_route_verifiable() {
        let (_dir, dir) = tempdir();
        let prepared =
            prepare("echo wrapped", dir.as_path(), None, environment()).expect("prepare");
        // A wrapper prefixes the argv and moves the route to the tail, so the
        // route is verified on the command the caller prepared rather than on
        // the wrapped one.
        let mut wrapped = prepared.clone();
        wrapped
            .argv
            .insert(0, String::from("/usr/bin/sandbox-exec"));
        assert_ne!(wrapped.argv, prepared.argv);
        assert!(verify_unchanged(&prepared).is_ok());
    }

    #[test]
    fn shell_routing_covers_expansion_a_direct_argv_could_not_perform() {
        assert!(requires_shell("echo *.rs"));
        assert!(requires_shell("echo a?c"));
        assert!(requires_shell("ls [ab].rs"));
        assert!(requires_shell("echo ~"));
        assert!(requires_shell("cat < in.txt"));
        assert!(requires_shell("echo `date`"));
        assert!(requires_shell("echo \"two  spaces\""));
        assert!(!requires_shell("echo hello"));
        assert!(!requires_shell("/usr/bin/env"));
        assert!(!requires_shell("git status --short"));
    }

    #[test]
    #[cfg(unix)]
    fn a_quoted_direct_argument_is_kept_verbatim_as_a_shell_script() {
        let (_dir, dir) = tempdir();
        // A quoted argument needs the shell, and the whole string is passed as
        // one argument, so the shell's own parsing is what produced the words.
        let prepared =
            prepare("echo \"a  b\"", dir.as_path(), None, environment()).expect("prepare");
        assert_eq!(
            prepared.argv,
            [default_shell().as_str(), "-c", "echo \"a  b\""]
        );
        let outcome = run(&prepared, Duration::from_secs(10), &never).expect("run");
        assert_eq!(outcome.stdout, "a  b\n");
    }

    #[test]
    #[cfg(unix)]
    fn a_direct_argv_is_not_re_split_by_a_shell() {
        let (_dir, dir) = tempdir();
        let prepared =
            prepare("/bin/echo one two", dir.as_path(), None, environment()).expect("prepare");
        assert_eq!(prepared.argv, ["/bin/echo", "one", "two"]);
        let outcome = run(&prepared, Duration::from_secs(10), &never).expect("run");
        assert_eq!(outcome.stdout, "one two\n");
    }

    #[test]
    fn an_unchanged_command_verifies_on_both_routes() {
        let (_dir, dir) = tempdir();
        for command in ["echo hello", "echo hello | cat"] {
            let prepared = prepare(command, dir.as_path(), None, environment()).expect("prepare");
            assert!(
                verify_unchanged(&prepared).is_ok(),
                "`{command}` was refused"
            );
        }
    }

    #[test]
    fn a_shell_resolved_through_path_is_refused_by_verification() {
        let (_dir, dir) = tempdir();
        let mut prepared =
            prepare("echo hello | cat", dir.as_path(), None, environment()).expect("prepare");
        prepared.argv[0] = String::from("sh");
        let error = verify_unchanged(&prepared).expect_err("a relative shell");
        assert_eq!(error.code(), ErrorCode::InvalidState);
    }

    #[test]
    fn an_argv_without_a_program_is_refused() {
        let (_dir, dir) = tempdir();
        let mut prepared =
            prepare("echo hello", dir.as_path(), None, environment()).expect("prepare");
        prepared.argv.clear();
        let error = run(&prepared, Duration::from_secs(1), &never).expect_err("no program");
        assert_eq!(error.code(), ErrorCode::InvalidField);
    }

    #[test]
    #[cfg(unix)]
    fn a_zero_timeout_ends_the_command_immediately() {
        let (_dir, dir) = tempdir();
        let prepared = prepare("sleep 30", dir.as_path(), None, environment()).expect("prepare");
        let outcome = run(&prepared, Duration::ZERO, &never).expect("run");
        assert!(outcome.timed_out);
        assert!(matches!(outcome.exit, Exit::Signal(15 | 9)));
    }

    #[test]
    #[cfg(unix)]
    fn a_command_that_leaves_a_pipe_open_still_returns() {
        let (_dir, dir) = tempdir();
        let pidfile = dir.join("held.pid");
        let mut environment = environment();
        environment.insert(String::from("RUNE_EXEC_PIDFILE"), pidfile.to_string());
        // The shell exits while its child still holds the write end of the pipe,
        // so waiting for the pipe to close would wait for the child.
        let prepared = prepare(
            "sleep 30 & echo $! > \"$RUNE_EXEC_PIDFILE\"; echo done; exit 4",
            dir.as_path(),
            None,
            environment,
        )
        .expect("prepare");
        let started = Instant::now();
        let outcome = run(&prepared, Duration::from_secs(30), &never).expect("run");
        let elapsed = started.elapsed();
        assert_eq!(outcome.exit, Exit::Code(4));
        assert_eq!(outcome.stdout.trim(), "done");
        assert!(
            elapsed < Duration::from_secs(20),
            "the run waited for a process the command left behind: {elapsed:?}"
        );
        let held = recorded_pid(&pidfile);
        assert!(
            alive(&held),
            "the held process ended, so the pipe was not held open"
        );
        // The command left this process behind, so the test ends it rather than
        // leaving it to outlive the suite.
        let _ = Command::new("/bin/kill")
            .args(["-9", held.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        assert!(wait_until(|| !alive(&held), Duration::from_secs(10)));
    }

    #[test]
    fn a_capture_keeps_the_head_and_counts_the_rest() {
        let stream = Stream::new(8);
        stream.push(b"0123456789");
        stream.push(b"abcdef");
        let (text, truncated, retained) = stream.render();
        assert!(text.starts_with("01234567"), "{text}");
        assert!(text.contains(TRUNCATION_MARKER), "{text}");
        assert!(truncated);
        assert_eq!(retained, 8);
    }

    #[test]
    fn a_capture_within_the_cap_is_whole() {
        let stream = Stream::new(8);
        stream.push(b"01234567");
        let (text, truncated, retained) = stream.render();
        assert_eq!(text, "01234567");
        assert!(!truncated);
        assert_eq!(retained, 8);
    }
}
