//! A supervised child process and the bytes it produced.
//!
//! Where [`crate::command`] runs a command to completion, this module keeps one
//! running and hands back a handle, which is what a long-lived command needs.
//! Both share one process-group and exit-status vocabulary, so a caller never
//! has to learn a second one.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};

use crate::command::{Exit, apply_limits, own_group, resource_limits};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Program every command is run by.
///
/// The platform's own shell: a command string written for this machine is
/// written for the shell this machine has.
#[cfg(unix)]
const SHELL: &str = "/bin/sh";

/// Program every command is run by.
#[cfg(not(unix))]
const SHELL: &str = "cmd.exe";

/// Flag that hands a shell one string to interpret.
#[cfg(unix)]
const SHELL_FLAG: &str = "-c";

/// Flag that hands a shell one string to interpret.
#[cfg(not(unix))]
const SHELL_FLAG: &str = "/C";

/// Bytes read from a pipe in one call.
const CHUNK_BYTES: usize = 8 * 1024;

/// Programs that deliver a signal, tried in order.
#[cfg(unix)]
const KILL_PROGRAMS: [&str; 2] = ["/bin/kill", "kill"];

/// Signal sent first when a session is stopped.
const GRACEFUL_SIGNAL: &str = "TERM";

/// Signal sent when a graceful stop leaves something running.
const FORCE_SIGNAL: &str = "KILL";

/// Signal that freezes a group before it is killed, on macOS.
const STOP_SIGNAL: &str = "STOP";

/// Time a termination waits, before escalating and again after escalating.
pub const GRACE_PERIOD: Duration = Duration::from_millis(2_000);

/// Time between checks while waiting for a process to end.
pub const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Bytes captured from one stream.
#[derive(Debug)]
pub struct Buffer {
    state: Mutex<Captured>,
    cap: usize,
}

/// Fraction of a stream's cap kept for its newest bytes, as a divisor.
const TAIL_SHARE: usize = 4;

/// The retained state of one captured stream.
#[derive(Debug, Default)]
struct Captured {
    /// Retained bytes, never more than the cap.
    bytes: Vec<u8>,
    /// The newest bytes past the retained head, never more than a quarter of
    /// the cap.
    tail: std::collections::VecDeque<u8>,
    /// Bytes the stream produced in total.
    produced: u64,
}

impl Buffer {
    /// Builds a buffer that retains the first `cap` bytes of a stream.
    fn new(cap: usize) -> Self {
        Self {
            state: Mutex::new(Captured::default()),
            cap,
        }
    }

    /// Appends bytes read from the stream, discarding what does not fit.
    fn append(&self, chunk: &[u8]) {
        let mut state = lock(&self.state);
        state.produced = state
            .produced
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        let room = self.cap.saturating_sub(state.bytes.len()).min(chunk.len());
        // The head of the stream is kept in full, because the first bytes
        // usually say what the command is and what it complained about. Past
        // the head only the newest bytes are kept, because a test run or a
        // build reports its outcome last.
        state.bytes.extend_from_slice(&chunk[..room]);
        let rest = &chunk[room..];
        let tail_cap = self.cap / TAIL_SHARE;
        if rest.len() >= tail_cap {
            state.tail.clear();
            state
                .tail
                .extend(&rest[rest.len().saturating_sub(tail_cap)..]);
        } else {
            state.tail.extend(rest);
            let excess = state.tail.len().saturating_sub(tail_cap);
            state.tail.drain(..excess);
        }
    }

    /// Returns the newest bytes kept past the retained head, and how many bytes
    /// between the head and them were not kept.
    ///
    /// Empty while the stream still fits its head.
    #[must_use]
    pub fn tail(&self) -> (Vec<u8>, u64) {
        let state = lock(&self.state);
        let kept =
            u64::try_from(state.bytes.len().saturating_add(state.tail.len())).unwrap_or(u64::MAX);
        (
            state.tail.iter().copied().collect(),
            state.produced.saturating_sub(kept),
        )
    }

    /// Returns up to `count` of the newest non-empty lines, read from at most
    /// the last `max_bytes` retained bytes, oldest first.
    ///
    /// The unfinished last line is included. A line cut by the byte window is
    /// left out, since its start was not read.
    #[must_use]
    pub fn last_lines(&self, count: usize, max_bytes: usize) -> Vec<String> {
        let state = lock(&self.state);
        let window: Vec<u8> = if state.tail.is_empty() {
            let start = state.bytes.len().saturating_sub(max_bytes);
            state.bytes.get(start..).unwrap_or_default().to_vec()
        } else {
            let skip = state.tail.len().saturating_sub(max_bytes);
            state.tail.iter().skip(skip).copied().collect()
        };
        let whole = if state.tail.is_empty() {
            window.len() == state.bytes.len()
        } else {
            u64::try_from(window.len()).unwrap_or(u64::MAX) == state.produced
        };
        drop(state);
        let text = String::from_utf8_lossy(&window);
        let mut lines: Vec<&str> = text.split('\n').collect();
        if !whole && lines.len() > 1 {
            lines.remove(0);
        }
        let mut newest: Vec<String> = lines
            .iter()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(count)
            .map(|line| (*line).to_owned())
            .collect();
        newest.reverse();
        newest
    }

    /// Returns the retained byte count and the number of bytes produced.
    ///
    /// Read without copying, so a caller polling a running command is cheap.
    #[must_use]
    pub fn totals(&self) -> (usize, u64) {
        let state = lock(&self.state);
        (state.bytes.len(), state.produced)
    }

    /// Returns the retained bytes past `offset`, with the position to continue
    /// from and the number of bytes the stream has produced.
    #[must_use]
    pub fn since(&self, offset: usize) -> Reading {
        let state = lock(&self.state);
        let start = offset.min(state.bytes.len());
        let bytes = if start < state.bytes.len() {
            state.bytes[start..].to_vec()
        } else {
            Vec::new()
        };
        Reading {
            bytes,
            next: state.bytes.len(),
            produced: state.produced,
        }
    }
}

/// What one stream holds beyond a reader's position.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Reading {
    /// Retained bytes the reader has not seen.
    pub bytes: Vec<u8>,
    /// Position to pass to the next read.
    pub next: usize,
    /// Bytes the stream has produced in total.
    pub produced: u64,
}

/// What a process reads from its standard input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Input {
    /// Nothing: a read sees the end of input at once, so a program that falls
    /// back to reading its input, such as `cat` or `rg pattern` given no file,
    /// finishes instead of waiting for bytes nobody will send.
    Closed,
    /// A pipe that [`Process::write`] feeds.
    Piped,
    /// A Linux PTY with a controlling terminal and merged output streams.
    /// Unsupported platforms refuse this mode before spawning a command.
    Pty { rows: u16, cols: u16 },
}

/// How a child's standard output and standard error reach the caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Streams {
    /// Two pipes, read into `stdout()` and `stderr()`.
    Separate,
    /// One pipe for both, read into `stdout()` in the order the child wrote;
    /// `stderr()` stays empty.
    Merged,
}

/// What happens to the rest of a child's group when the child exits.
///
/// On Windows `Keep` behaves like `End`: the tree is ended when the child
/// exits. A background process that leaves the group, for example through
/// `setsid`, is neither detected nor ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Leftovers {
    /// End the group as soon as the child is reaped (the default).
    End,
    /// Leave the group running; the owner ends it with `end_leftovers`.
    Keep,
}

/// Choices made when a child is spawned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpawnOptions {
    pub streams: Streams,
    pub leftovers: Leftovers,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            streams: Streams::Separate,
            leftovers: Leftovers::End,
        }
    }
}

/// A child process in its own group, with the bytes it produced.
#[derive(Debug)]
pub struct Process {
    inner: Arc<Inner>,
}

/// The write side of a child's standard input.
#[derive(Debug)]
enum Stdin {
    /// Open and free for the next write.
    Ready(Writer),
    /// Held by a write the process has not finished reading.
    Writing,
    /// Held by a write, and closed once that write ends.
    CloseAfterWrite,
    /// Never opened, or closed by a failed write.
    Closed,
}

/// A pipe or the write side of a Linux terminal.
#[derive(Debug)]
enum Writer {
    Pipe(ChildStdin),
    #[cfg(target_os = "linux")]
    Terminal(std::fs::File),
}

impl Writer {
    /// Pipe behavior stays blocking; terminal writes poll so shutdown can end
    /// a write even when Linux does not wake it after the slave closes.
    fn write_input(&mut self, bytes: &[u8], inner: &Inner) -> io::Result<()> {
        #[cfg(not(target_os = "linux"))]
        let _ = inner;
        match self {
            Self::Pipe(pipe) => pipe.write_all(bytes).and_then(|()| pipe.flush()),
            #[cfg(target_os = "linux")]
            Self::Terminal(master) => {
                let mut remaining = bytes;
                while !remaining.is_empty() {
                    if lock(&inner.exit).is_some() {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "the terminal process ended",
                        ));
                    }
                    match master.write(remaining) {
                        Ok(0) => {
                            return Err(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "the terminal accepted no input",
                            ));
                        }
                        Ok(written) => remaining = &remaining[written..],
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(POLL_INTERVAL);
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(err) => return Err(err),
                    }
                }
                Ok(())
            }
        }
    }
}

/// State shared with the threads that watch one child.
#[derive(Debug)]
struct Inner {
    /// The child, until the waiter claims it.
    child: Mutex<Option<Child>>,
    /// Standard input, until the pipe closes.
    stdin: Mutex<Stdin>,
    /// Set by the waiter once the child has been reaped.
    exit: Mutex<Option<Exit>>,
    /// Threads reading standard output and standard error.
    pumps: Mutex<Vec<JoinHandle<()>>>,
    /// Standard output captured while the child ran.
    stdout: Buffer,
    /// Standard error captured while the child ran.
    stderr: Buffer,
    /// Group to signal. The child leads its own group, so this is the child id.
    group: u32,
    /// Whether the waiter leaves the group running after the child exits.
    leftovers: Leftovers,
    /// Set when `kill_after` ended the process at its deadline.
    timed_out: AtomicBool,
    #[cfg(target_os = "linux")]
    terminal: Option<std::fs::File>,
    /// The job holding the child's tree, when one could be created and joined.
    #[cfg(windows)]
    job: Option<crate::job::Job>,
}

impl Process {
    /// Starts `command` under a shell in a new process group.
    ///
    /// The command string is passed as one argument, so what a caller reviewed
    /// is exactly what the shell parses. At most `capture_bytes` from each
    /// stream are retained.
    pub fn start(
        command: &str,
        cwd: Option<&Path>,
        capture_bytes: usize,
        input: Input,
    ) -> io::Result<Self> {
        let argv = [
            SHELL.to_owned(),
            String::from(SHELL_FLAG),
            command.to_owned(),
        ];
        Self::start_argv(
            &argv,
            cwd,
            capture_bytes,
            &crate::command::minimal_environment(),
            input,
        )
    }

    /// Starts a command from an explicit argv.
    ///
    /// Exists so a caller can run an argv that a sandbox produced, which is the
    /// only way a restricted command and the command that was reviewed can be
    /// the same thing. The argv is passed as given: nothing here re-parses it.
    ///
    /// Nothing from the parent environment is inherited. A long-lived session
    /// outlives the single command path, so a variable the caller did not pass
    /// would otherwise be visible to every command it runs, including the
    /// provider credentials this process holds.
    pub fn start_argv(
        argv: &[String],
        cwd: Option<&Path>,
        capture_bytes: usize,
        environment: &BTreeMap<String, String>,
        input: Input,
    ) -> io::Result<Self> {
        Self::start_argv_with(
            argv,
            cwd,
            capture_bytes,
            environment,
            input,
            SpawnOptions::default(),
        )
    }

    /// Starts a command from an explicit argv, with the given spawn options.
    ///
    /// A PTY ignores `streams`: a terminal already merges its output.
    pub fn start_argv_with(
        argv: &[String],
        cwd: Option<&Path>,
        capture_bytes: usize,
        environment: &BTreeMap<String, String>,
        input: Input,
        options: SpawnOptions,
    ) -> io::Result<Self> {
        let Some((program, arguments)) = argv.split_first() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an argv needs a program to run",
            ));
        };
        let stdin = match input {
            Input::Closed => Stdio::null(),
            Input::Piped => Stdio::piped(),
            Input::Pty { .. } => {
                #[cfg(not(target_os = "linux"))]
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "PTY shell mode is only supported on Linux",
                ));
                #[cfg(target_os = "linux")]
                Stdio::null()
            }
        };
        let mut spec = Command::new(program);
        add_arguments(&mut spec, program, arguments);
        spec.env_clear().envs(environment).stdin(stdin);
        // Both streams share one pipe, so the reader sees bytes in the order the
        // child wrote them. The write ends move into the command, which is
        // dropped after the spawn, so the reader sees end of file once the
        // child's group closes them.
        let merged = if options.streams == Streams::Merged && !matches!(input, Input::Pty { .. }) {
            let (reader, writer) = io::pipe()?;
            let error_writer = writer.try_clone()?;
            spec.stdout(Stdio::from(writer))
                .stderr(Stdio::from(error_writer));
            Some(reader)
        } else {
            spec.stdout(Stdio::piped()).stderr(Stdio::piped());
            None
        };
        if let Some(cwd) = cwd {
            spec.current_dir(cwd);
        }
        #[cfg(target_os = "linux")]
        let terminal = if let Input::Pty { rows, cols } = input {
            Some(crate::pty::configure(&mut spec, rows, cols)?)
        } else {
            None
        };
        // Long-lived commands use the same resource ceilings as one-shot runs.
        // The PTY path applies them together with controlling-terminal setup.
        if !matches!(input, Input::Pty { .. }) {
            own_group(&mut spec);
            apply_limits(&mut spec, resource_limits());
        }
        // Clone before spawning so a descriptor error cannot leak a child.
        #[cfg(target_os = "linux")]
        let terminal_reader = terminal
            .as_ref()
            .map(std::fs::File::try_clone)
            .transpose()?;
        #[cfg(target_os = "linux")]
        let terminal_writer = terminal
            .as_ref()
            .map(std::fs::File::try_clone)
            .transpose()?;
        let mut child = spec.spawn()?;
        // Assigned straight after the spawn, so what the child starts is in the
        // job too. A process started before this returns can escape it, which
        // the tree kill fallback still reaches.
        #[cfg(windows)]
        let job = crate::job::Job::new()
            .and_then(|job| job.assign(&child).map(|()| job))
            .ok();
        // Command holds the parent's slave descriptors until dropped. Keeping
        // them open would prevent EOF when the child's terminal closes.
        drop(spec);
        let group = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take().map(Writer::Pipe);
        #[cfg(target_os = "linux")]
        let stdin = terminal_writer.map(Writer::Terminal).or(stdin);
        let stdin = stdin.map_or(Stdin::Closed, Stdin::Ready);
        let inner = Arc::new(Inner {
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(stdin),
            exit: Mutex::new(None),
            pumps: Mutex::new(Vec::new()),
            stdout: Buffer::new(capture_bytes),
            stderr: Buffer::new(capture_bytes),
            group,
            leftovers: options.leftovers,
            timed_out: AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            terminal,
            #[cfg(windows)]
            job,
        });

        #[cfg(target_os = "linux")]
        if let Err(err) = attach(&inner, terminal_reader, Pipe::Output) {
            inner.shutdown();
            return Err(err);
        }
        let started = spawn_waiter(Arc::clone(&inner))
            .and_then(|()| attach(&inner, merged, Pipe::Output))
            .and_then(|()| attach(&inner, stdout, Pipe::Output))
            .and_then(|()| attach(&inner, stderr, Pipe::Error));
        if let Err(err) = started {
            // Without the watcher threads the child can be neither reported nor
            // stopped, so it is ended here instead of being left running.
            inner.shutdown();
            return Err(err);
        }
        Ok(Self { inner })
    }

    /// Returns the process id, which is also the id of its group.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.inner.group
    }

    /// Returns the ids of the processes in the command's job, empty when the
    /// command has no job.
    #[cfg(windows)]
    #[must_use]
    pub fn job_process_ids(&self) -> Vec<u32> {
        self.inner
            .job
            .as_ref()
            .and_then(|job| job.process_ids().ok())
            .unwrap_or_default()
    }

    /// Returns true while the process has not been reaped.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.exit().is_none()
    }

    /// Returns how the process ended, once it has been reaped.
    #[must_use]
    pub fn exit(&self) -> Option<Exit> {
        *lock(&self.inner.exit)
    }

    /// Ends the process group once `limit` has passed, unless it has exited.
    /// Runs on a thread of its own, so the limit holds whether or not anyone
    /// is waiting on the process.
    pub fn kill_after(&self, limit: Duration) -> io::Result<()> {
        let inner = Arc::clone(&self.inner);
        let deadline = Instant::now().checked_add(limit);
        std::thread::Builder::new()
            .name(String::from("rune-shell-timeout"))
            .spawn(move || {
                loop {
                    if lock(&inner.exit).is_some() {
                        return;
                    }
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        break;
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                inner.timed_out.store(true, Ordering::SeqCst);
                if !force_group(&inner)
                    && let Some(child) = lock(&inner.child).as_mut()
                {
                    let _ = child.kill();
                }
            })?;
        Ok(())
    }

    /// Returns true when `kill_after` ended the process.
    #[must_use]
    pub fn timed_out(&self) -> bool {
        self.inner.timed_out.load(Ordering::SeqCst)
    }

    /// Returns the captured standard output.
    #[must_use]
    pub fn stdout(&self) -> &Buffer {
        &self.inner.stdout
    }

    /// Returns the captured standard error.
    #[must_use]
    pub fn stderr(&self) -> &Buffer {
        &self.inner.stderr
    }

    /// Resizes a Linux PTY. Pipe sessions and unsupported hosts refuse it.
    pub fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(terminal) = &self.inner.terminal {
            if !self.is_running() {
                return Err(io::Error::other("the terminal process has ended"));
            }
            return crate::pty::resize(terminal, rows, cols);
        }
        let _ = (rows, cols);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "resizing requires a Linux PTY session",
        ))
    }

    /// Writes bytes to the child's standard input, waiting at most `limit`.
    ///
    /// A pipe holds only so much, and a process that is not reading leaves a
    /// larger write blocked for as long as it runs, so the write happens on a
    /// thread of its own and the caller waits a bounded time for it. A write
    /// that outlives the wait carries on as the process reads, and the pipe
    /// refuses more input until it is done. Ending the process ends the write.
    pub fn write(&self, bytes: &[u8], limit: Duration) -> io::Result<()> {
        let mut stdin = {
            let mut slot = lock(&self.inner.stdin);
            match std::mem::replace(&mut *slot, Stdin::Writing) {
                Stdin::Ready(stdin) => stdin,
                Stdin::Writing => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "earlier input is still waiting for the process to read it",
                    ));
                }
                closed @ (Stdin::Closed | Stdin::CloseAfterWrite) => {
                    *slot = closed;
                    return Err(io::Error::other("the process is not taking input"));
                }
            }
        };
        let (sender, receiver) = mpsc::channel();
        let inner = Arc::clone(&self.inner);
        let bytes = bytes.to_vec();
        let spawned = std::thread::Builder::new()
            .name(String::from("rune-shell-stdin"))
            .spawn(move || {
                let written = stdin.write_input(&bytes, &inner);
                let mut slot = lock(&inner.stdin);
                *slot = if written.is_ok() && matches!(*slot, Stdin::Writing) {
                    Stdin::Ready(stdin)
                } else {
                    Stdin::Closed
                };
                drop(slot);
                let _ = sender.send(written);
            });
        if let Err(err) = spawned {
            // The pipe went down with the thread that was to write it.
            *lock(&self.inner.stdin) = Stdin::Closed;
            return Err(err);
        }
        match receiver.recv_timeout(limit) {
            Ok(written) => written,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the process did not read its input within {} ms; the rest is written as \
                     it reads",
                    limit.as_millis()
                ),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(io::Error::other("the input writer stopped"))
            }
        }
    }

    /// Closes the child's standard input, so a reader sees the end of input.
    ///
    /// A write still in progress finishes first, then the pipe closes.
    pub fn close_input(&self) {
        let mut slot = lock(&self.inner.stdin);
        *slot = match std::mem::replace(&mut *slot, Stdin::Closed) {
            Stdin::Writing | Stdin::CloseAfterWrite => Stdin::CloseAfterWrite,
            Stdin::Ready(_) | Stdin::Closed => Stdin::Closed,
        };
    }

    /// Waits a bounded time for the readers to reach end of file.
    ///
    /// The pipes close when the child exits, unless a descendant still holds an
    /// inherited end of one. The wait is bounded so a stray grandchild cannot
    /// hold a result back.
    pub fn drain_output(&self, window: Duration) {
        let deadline = Instant::now().checked_add(window);
        let handles = std::mem::take(&mut *lock(&self.inner.pumps));
        while !handles.iter().all(JoinHandle::is_finished) {
            if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        for handle in handles {
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }

    /// Ends the process group.
    ///
    /// A graceful stop signals the group, waits the grace period, and escalates
    /// only if something survives. Returns true when the child has been reaped.
    pub fn terminate(&self, force: bool) -> bool {
        if self.exit().is_some() {
            return true;
        }
        if force {
            self.end_group();
            return self.wait_for_exit(GRACE_PERIOD);
        }
        self.signal(GRACEFUL_SIGNAL);
        if self.wait_for_exit(GRACE_PERIOD) {
            return true;
        }
        self.end_group();
        self.wait_for_exit(GRACE_PERIOD)
    }

    /// Returns true while any process in the child's group is alive (Unix);
    /// always false elsewhere.
    #[must_use]
    pub fn group_alive(&self) -> bool {
        group_alive(self.inner.group)
    }

    /// Ends what is left of the group, if anything is.
    pub fn end_leftovers(&self) {
        if self.group_alive() {
            force_group(&self.inner);
        }
    }

    /// Sends the forceful stop, falling back to the direct child when no
    /// signal program is available.
    fn end_group(&self) {
        if !force_group(&self.inner) {
            self.kill_leader();
        }
    }

    /// Sends a signal to the process group, returning whether it was delivered.
    fn signal(&self, name: &str) -> bool {
        signal_group(&self.inner, name)
    }

    /// Ends the direct child when the group cannot be signalled.
    ///
    /// Only reached before the waiter takes the child, so the id still names
    /// the process this handle started.
    fn kill_leader(&self) {
        if let Some(child) = lock(&self.inner.child).as_mut() {
            let _ = child.kill();
        }
    }

    /// Waits up to `limit` for the waiter to record an exit.
    fn wait_for_exit(&self, limit: Duration) -> bool {
        let deadline = Instant::now().checked_add(limit);
        loop {
            if self.exit().is_some() {
                return true;
            }
            if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Inner {
    /// Ends the group and reaps the child, best effort.
    fn shutdown(&self) {
        force_group(self);
        if let Some(mut child) = lock(&self.child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Adds the arguments to a command, handing a command string to `cmd.exe` as
/// it was written.
///
/// `cmd.exe` does not read arguments the way the standard library quotes them:
/// a `"` escaped as `\"` reaches the command as a backslash and a quote, so
/// `git commit -m "fix bug"` would arrive broken. The string after `/C` is
/// passed unescaped inside one pair of quotes, and `/S` tells the shell to
/// strip exactly that pair, which leaves the string the caller reviewed.
#[cfg(windows)]
fn add_arguments(spec: &mut Command, program: &str, arguments: &[String]) {
    use std::os::windows::process::CommandExt as _;

    let name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    let is_cmd = name.eq_ignore_ascii_case("cmd.exe") || name.eq_ignore_ascii_case("cmd");
    if is_cmd
        && let Some(flag) = arguments
            .iter()
            .position(|argument| argument.eq_ignore_ascii_case("/C"))
        && flag.saturating_add(2) == arguments.len()
        && let (Some(before), Some(switch), Some(command)) = (
            arguments.get(..flag),
            arguments.get(flag),
            arguments.get(flag.saturating_add(1)),
        )
    {
        spec.args(before);
        spec.raw_arg("/S");
        spec.raw_arg(switch);
        spec.raw_arg(format!("\"{command}\""));
        return;
    }
    spec.args(arguments);
}

/// Adds the arguments to a command.
#[cfg(not(windows))]
fn add_arguments(spec: &mut Command, _program: &str, arguments: &[String]) {
    spec.args(arguments);
}

/// Returns true while any process in the group is alive (Unix); always
/// false elsewhere.
#[must_use]
pub fn group_alive(group: u32) -> bool {
    #[cfg(unix)]
    {
        i32::try_from(group)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .is_some_and(|group| rustix::process::test_kill_process_group(group).is_ok())
    }
    #[cfg(not(unix))]
    {
        let _ = group;
        false
    }
}

/// Ends a process tree, on a platform with no process groups to signal.
///
/// The job the child was put in holds the whole tree, including a process
/// whose parent has exited. Without a job, the platform's own tool walks the
/// tree from the given process. Either way it is forcible, so a caller that
/// asked politely is answered by the tree being gone rather than by a refusal.
///
/// The standard library can only end the direct child, and the process this is
/// called on is usually the shell rather than the work it started, so ending
/// only that would leave the work running under a parent that no longer exists.
#[cfg(not(unix))]
fn signal_group(inner: &Inner, _name: &str) -> bool {
    #[cfg(windows)]
    if inner.job.as_ref().is_some_and(crate::job::Job::terminate) {
        return true;
    }
    Command::new("taskkill")
        .args(["/PID", &inner.group.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

impl Drop for Process {
    fn drop(&mut self) {
        // A dropped handle has nobody left to read a graceful exit, and a
        // leaked handle would leave a command running forever, so the group is
        // ended outright rather than asked politely. A leader that has already
        // exited took its group with it, unless the group was kept, in which
        // case what is left of it ends here.
        if self.is_running() {
            self.terminate(true);
        } else if self.inner.leftovers == Leftovers::Keep {
            self.end_leftovers();
        }
    }
}

/// Which pipe a reader thread drains.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pipe {
    Output,
    Error,
}

/// Starts a thread that copies one pipe into the buffer that belongs to it.
fn start_pump<R: Read + Send + 'static>(
    inner: &Arc<Inner>,
    mut stream: R,
    pipe: Pipe,
) -> io::Result<JoinHandle<()>> {
    let name = match pipe {
        Pipe::Output => "rune-shell-stdout",
        Pipe::Error => "rune-shell-stderr",
    };
    let inner = Arc::clone(inner);
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let buffer = match pipe {
                Pipe::Output => &inner.stdout,
                Pipe::Error => &inner.stderr,
            };
            let mut chunk = [0_u8; CHUNK_BYTES];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => buffer.append(&chunk[..read]),
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        if lock(&inner.exit).is_some() {
                            break;
                        }
                        std::thread::sleep(POLL_INTERVAL);
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        })
}

/// Attaches a reader thread to one pipe, when the pipe was created.
fn attach<R: Read + Send + 'static>(
    inner: &Arc<Inner>,
    stream: Option<R>,
    pipe: Pipe,
) -> io::Result<()> {
    let Some(stream) = stream else {
        return Ok(());
    };
    let handle = start_pump(inner, stream, pipe)?;
    lock(&inner.pumps).push(handle);
    Ok(())
}

/// Starts the thread that reaps the child and records how it ended.
///
/// The thread is detached: its result arrives through the shared exit slot.
fn spawn_waiter(inner: Arc<Inner>) -> io::Result<()> {
    std::thread::Builder::new()
        .name(String::from("rune-shell-wait"))
        .spawn(move || {
            let Some(mut child) = lock(&inner.child).take() else {
                return;
            };
            let exit = child.wait().map_or(Exit::Unknown, classify);
            // What the command left behind, such as `server &`, has nothing
            // left to stop it once the leader is gone, so the group is ended
            // now rather than at some later drop. Now is also when the id is
            // safe to signal: a group id stays taken while any member lives,
            // and a process id stays taken while its handle is open, which is
            // why the child is released only afterwards. An owner that keeps
            // the group ends it later, where the platform has groups to keep.
            if inner.leftovers == Leftovers::End || cfg!(not(unix)) {
                force_group(&inner);
            }
            drop(child);
            *lock(&inner.exit) = Some(exit);
        })?;
    Ok(())
}

/// Delivers a signal to a process group.
///
/// Returns whether a signal program ran. A group that has already ended cannot
/// be signalled, and nothing is left to stop, so the caller treats a failed
/// delivery as complete.
#[cfg(unix)]
fn signal_group(inner: &Inner, name: &str) -> bool {
    let target = format!("-{}", inner.group);
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

/// Ends a process group outright, returning whether the signal was delivered.
///
/// On macOS the group is stopped before it is killed. A kill reaches the
/// members one at a time there, so a shell whose current command dies first
/// could otherwise start the next command of a list before the kill reaches
/// the shell itself.
fn force_group(inner: &Inner) -> bool {
    if cfg!(target_os = "macos") {
        signal_group(inner, STOP_SIGNAL);
    }
    signal_group(inner, FORCE_SIGNAL)
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

/// Locks a mutex, ignoring poisoning.
///
/// A reader thread that panicked leaves a valid buffer behind, and a session
/// must remain stoppable whatever happened to its readers.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes retained by the tests.
    const CAPTURE_BYTES: usize = 64 * 1024;

    /// Time a test gives a process to take its input.
    const WRITE_LIMIT: Duration = Duration::from_secs(10);

    /// Returns a command that writes to both streams and exits with `code`.
    fn echo_both_and_exit(code: i32) -> String {
        if cfg!(windows) {
            format!("echo out & echo err 1>&2 & exit /b {code}")
        } else {
            format!("echo out; echo err 1>&2; exit {code}")
        }
    }

    /// Returns a command that exits with `code`.
    fn exit_with(code: i32) -> String {
        if cfg!(windows) {
            format!("exit /b {code}")
        } else {
            format!("exit {code}")
        }
    }

    /// Returns the argv of a process that reads one line and writes it back.
    ///
    /// The test is about the write reaching the process, so the script is passed
    /// as a single argument and no shell re-parses it. It reads exactly one line,
    /// because the write side stays open for the life of the process: a program
    /// that reads until its input ends would wait there forever.
    ///
    /// The platform whose shell sets a variable from a line needs delayed
    /// expansion to read it back, because the whole line is expanded before it
    /// runs. The argv is passed directly, so no other shell re-parses the
    /// quotes around the builtin's argument.
    fn echo_standard_input() -> Vec<String> {
        let argv: &[&str] = if cfg!(windows) {
            // Delayed expansion is what lets the line be read back: the whole
            // statement is expanded before it runs, so `%line%` would be empty.
            &["cmd", "/V:ON", "/C", "set /p line= & echo !line!"]
        } else {
            &["/bin/sh", "-c", "read line; echo $line"]
        };
        argv.iter().map(|argument| (*argument).to_owned()).collect()
    }

    /// Waits for a condition, polling until the deadline.
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

    /// Returns the text one stream captured.
    fn text(buffer: &Buffer) -> String {
        String::from_utf8_lossy(&buffer.since(0).bytes).into_owned()
    }

    /// Waits for the process to end and returns its exit.
    ///
    /// The readers are drained first, because a reaped child says nothing about
    /// whether its last bytes have been copied out of the pipe yet. A caller
    /// that reads immediately after this sees whatever the reader thread has
    /// managed to move so far, which is a race a loaded machine can win.
    fn exit_of(process: &Process) -> Exit {
        assert!(
            wait_until(|| process.exit().is_some(), Duration::from_secs(10)),
            "the process did not end"
        );
        process.drain_output(Duration::from_secs(5));
        process.exit().expect("an exit status")
    }

    /// Returns the id the shell printed for the child it forked.
    #[cfg(unix)]
    fn forked_child(process: &Process) -> Option<String> {
        text(process.stdout())
            .lines()
            .next()
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()))
    }

    #[test]
    fn both_streams_are_captured_and_the_status_is_reported() {
        let process = Process::start(&echo_both_and_exit(3), None, CAPTURE_BYTES, Input::Closed)
            .expect("start");
        let exit = exit_of(&process);
        assert_eq!(exit, Exit::Code(3));
        assert!(text(process.stdout()).contains("out"));
        assert!(text(process.stderr()).contains("err"));
    }

    /// Returns the argv that runs `command` under the platform shell.
    fn shell_argv(command: &str) -> [String; 3] {
        [
            SHELL.to_owned(),
            String::from(SHELL_FLAG),
            command.to_owned(),
        ]
    }

    #[test]
    fn merged_streams_keep_the_order_the_command_wrote() {
        let command = if cfg!(windows) {
            "echo o1& echo e1 1>&2& echo o2& echo e2 1>&2"
        } else {
            "printf 'o1\\n'; printf 'e1\\n' >&2; printf 'o2\\n'; printf 'e2\\n' >&2"
        };
        let process = Process::start_argv_with(
            &shell_argv(command),
            None,
            CAPTURE_BYTES,
            &crate::command::minimal_environment(),
            Input::Closed,
            SpawnOptions {
                streams: Streams::Merged,
                ..SpawnOptions::default()
            },
        )
        .expect("start");
        assert!(exit_of(&process).is_success());
        let output = text(process.stdout());
        if cfg!(windows) {
            let lines: Vec<&str> = output.lines().map(str::trim).collect();
            assert_eq!(lines, ["o1", "e1", "o2", "e2"], "{output:?}");
        } else {
            assert_eq!(output, "o1\ne1\no2\ne2\n");
        }
        assert_eq!(process.stderr().totals(), (0, 0));
    }

    #[test]
    fn separate_streams_are_unchanged_by_default() {
        let process = Process::start_argv_with(
            &shell_argv(&echo_both_and_exit(0)),
            None,
            CAPTURE_BYTES,
            &crate::command::minimal_environment(),
            Input::Closed,
            SpawnOptions::default(),
        )
        .expect("start");
        assert!(exit_of(&process).is_success());
        assert!(text(process.stdout()).contains("out"));
        assert!(!text(process.stdout()).contains("err"));
        assert!(text(process.stderr()).contains("err"));
    }

    #[test]
    fn a_successful_command_reports_success() {
        let process =
            Process::start(&exit_with(0), None, CAPTURE_BYTES, Input::Closed).expect("start");
        assert!(exit_of(&process).is_success());
    }

    #[test]
    fn a_process_takes_input_from_its_standard_input() {
        let process = Process::start_argv(
            &echo_standard_input(),
            None,
            CAPTURE_BYTES,
            &BTreeMap::new(),
            Input::Piped,
        )
        .expect("start");
        process.write(b"hello\n", WRITE_LIMIT).expect("write");
        assert_eq!(exit_of(&process), Exit::Code(0));
        assert!(
            text(process.stdout()).contains("hello"),
            "the write did not reach the process: {:?}",
            text(process.stdout())
        );
    }

    #[cfg(unix)]
    #[test]
    fn closing_input_ends_a_reader_after_its_last_write() {
        let process = Process::start_argv(
            &["/bin/cat".to_owned()],
            None,
            CAPTURE_BYTES,
            &BTreeMap::new(),
            Input::Piped,
        )
        .expect("start");
        process.write(b"hello\n", WRITE_LIMIT).expect("write");
        process.close_input();
        assert_eq!(exit_of(&process), Exit::Code(0));
        assert_eq!(text(process.stdout()), "hello\n");
        assert!(process.write(b"more", WRITE_LIMIT).is_err());
    }

    #[test]
    fn input_to_an_exited_process_is_refused() {
        let process =
            Process::start(&exit_with(0), None, CAPTURE_BYTES, Input::Piped).expect("start");
        let _ = exit_of(&process);
        assert!(process.write(b"hello\n", WRITE_LIMIT).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn a_quoted_argument_reaches_the_command_as_written() {
        let process =
            Process::start("echo \"fix bug\"", None, CAPTURE_BYTES, Input::Closed).expect("start");
        assert!(exit_of(&process).is_success());
        assert_eq!(text(process.stdout()).trim(), "\"fix bug\"");
    }

    #[cfg(windows)]
    #[test]
    fn a_command_that_begins_with_a_quote_keeps_every_quote() {
        let process = Process::start(
            "\"%SystemRoot%\\System32\\cmd.exe\" /C echo \"a b\"",
            None,
            CAPTURE_BYTES,
            Input::Closed,
        )
        .expect("start");
        assert!(exit_of(&process).is_success());
        assert_eq!(text(process.stdout()).trim(), "\"a b\"");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pty_input_that_is_never_read_has_a_bounded_wait_and_cleans_up() {
        let process = Process::start(
            "stty raw -echo; echo ready; sleep 30",
            None,
            CAPTURE_BYTES,
            Input::Pty { rows: 24, cols: 80 },
        )
        .expect("PTY start");
        assert!(wait_until(
            || text(process.stdout()).contains("ready"),
            Duration::from_secs(10)
        ));
        let err = process
            .write(&vec![b'x'; 1024 * 1024], Duration::from_millis(100))
            .expect_err("terminal input must not block the caller");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            process
                .write(b"more", WRITE_LIMIT)
                .expect_err("busy")
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(process.terminate(true));
        assert!(
            wait_until(
                || matches!(*lock(&process.inner.stdin), Stdin::Closed),
                Duration::from_secs(10)
            ),
            "terminal writer outlived cancellation: {:?}",
            *lock(&process.inner.stdin)
        );
        process.drain_output(Duration::from_secs(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resizing_a_pty_delivers_winch_to_its_foreground_group() {
        let process = Process::start(
            "trap 'echo winch; stty size' WINCH; echo ready; while :; do sleep 1; done",
            None,
            CAPTURE_BYTES,
            Input::Pty { rows: 24, cols: 80 },
        )
        .expect("PTY start");
        assert!(wait_until(
            || text(process.stdout()).contains("ready"),
            Duration::from_secs(10)
        ));
        assert!(process.resize(0, 80).is_err());
        process.resize(41, 109).expect("resize");
        assert!(
            wait_until(
                || text(process.stdout()).contains("winch\r\n41 109"),
                Duration::from_secs(10)
            ),
            "foreground group never received SIGWINCH"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pty_exit_drains_output_and_refuses_invalid_sizes() {
        let process = Process::start(
            "printf final-output; printf error-output >&2; exit 7",
            None,
            CAPTURE_BYTES,
            Input::Pty { rows: 24, cols: 80 },
        )
        .expect("PTY start");
        assert_eq!(exit_of(&process), Exit::Code(7));
        process.drain_output(Duration::from_secs(1));
        assert_eq!(text(process.stdout()), "final-outputerror-output");
        assert_eq!(process.stderr().totals(), (0, 0));
        assert!(process.resize(40, 100).is_err());
        assert!(
            Process::start(
                "sleep 30",
                None,
                CAPTURE_BYTES,
                Input::Pty { rows: 0, cols: 80 }
            )
            .is_err()
        );
        let pipe = Process::start("sleep 30", None, CAPTURE_BYTES, Input::Piped).expect("pipe");
        assert_eq!(
            pipe.resize(40, 100)
                .expect_err("pipe has no terminal")
                .kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn a_write_the_process_never_reads_returns_within_its_limit() {
        // Far more than any pipe holds, to a process that reads nothing.
        let command = if cfg!(windows) {
            "ping -n 60 127.0.0.1 >nul"
        } else {
            "sleep 30"
        };
        let process = Process::start(command, None, CAPTURE_BYTES, Input::Piped).expect("start");
        let bytes = vec![b'x'; 8 * 1024 * 1024];

        let began = Instant::now();
        let err = process
            .write(&bytes, Duration::from_millis(200))
            .expect_err("the write cannot complete");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "the write waited {:?}",
            began.elapsed()
        );

        // The pipe is still held by the first write, so a second one is refused
        // rather than interleaved with it.
        let busy = process
            .write(b"more\n", WRITE_LIMIT)
            .expect_err("the pipe is busy");
        assert_eq!(busy.kind(), io::ErrorKind::WouldBlock);
        assert!(process.terminate(true));
    }

    #[test]
    fn input_to_a_process_started_without_it_is_refused() {
        let process =
            Process::start(&exit_with(0), None, CAPTURE_BYTES, Input::Closed).expect("start");
        assert!(process.write(b"hello\n", WRITE_LIMIT).is_err());
        let _ = exit_of(&process);
    }

    #[test]
    fn a_process_started_without_input_reads_the_end_of_it() {
        // Each program reads its input when given no file, and would wait for
        // the whole bound below if its input were a pipe nobody closes.
        let command = if cfg!(windows) { "findstr x" } else { "cat" };
        let process = Process::start(command, None, CAPTURE_BYTES, Input::Closed).expect("start");
        assert!(
            wait_until(|| process.exit().is_some(), Duration::from_secs(10)),
            "a process with no input waited for some"
        );
    }

    #[test]
    fn a_buffer_keeps_the_first_bytes_and_counts_the_rest() {
        let buffer = Buffer::new(8);
        buffer.append(b"0123456789");
        buffer.append(b"abcdef");
        let reading = buffer.since(0);
        assert_eq!(reading.bytes, b"01234567");
        assert_eq!(reading.next, 8);
        assert_eq!(reading.produced, 16);
        assert_eq!(buffer.since(6).bytes, b"67");
        assert!(buffer.since(8).bytes.is_empty());
    }

    #[test]
    fn a_buffer_keeps_the_newest_bytes_past_its_head() {
        let buffer = Buffer::new(8);
        buffer.append(b"0123456");
        assert_eq!(buffer.tail(), (Vec::new(), 0), "the head still fits");

        // The cap is 8, so the tail holds the newest 2 bytes.
        buffer.append(b"789");
        assert_eq!(buffer.tail(), (b"89".to_vec(), 0));
        buffer.append(b"abcdef");
        let (tail, skipped) = buffer.tail();
        assert_eq!(tail, b"ef");
        assert_eq!(skipped, 16 - 8 - 2, "the gap is what neither part kept");
        assert_eq!(buffer.since(0).bytes, b"01234567", "the head is unchanged");

        // One chunk longer than the tail replaces it outright.
        buffer.append(b"uvwxyz");
        assert_eq!(buffer.tail().0, b"yz");
    }

    #[test]
    fn last_lines_returns_the_newest_complete_and_partial_lines() {
        let buffer = Buffer::new(64);
        assert!(buffer.last_lines(3, 4096).is_empty(), "nothing printed yet");
        buffer.append(b"one\ntwo\n\nthree\nfour\nfi");
        assert_eq!(buffer.last_lines(3, 4096), ["three", "four", "fi"]);
        assert_eq!(buffer.last_lines(1, 4096), ["fi"]);
        // The window starts inside `three`, so that cut line is left out.
        assert_eq!(buffer.last_lines(3, 10), ["four", "fi"]);

        // Past the head the newest bytes come from the tail.
        let full = Buffer::new(40);
        full.append(&[b'h'; 40]);
        full.append(b"x\nlast\nend");
        assert_eq!(full.last_lines(3, 4096), ["last", "end"]);
    }

    #[cfg(unix)]
    #[test]
    fn terminating_a_session_ends_a_forked_child() {
        let process = Process::start(
            "sleep 30 & echo $!; wait",
            None,
            CAPTURE_BYTES,
            Input::Closed,
        )
        .expect("start");
        assert!(
            wait_until(|| forked_child(&process).is_some(), Duration::from_secs(10)),
            "the shell did not report the child it forked"
        );
        let child = forked_child(&process).expect("a forked pid");
        let leader = process.id().to_string();
        assert!(alive(&child), "the forked child was not started");

        assert!(process.terminate(false), "the group did not end");
        assert!(
            wait_until(|| !alive(&child), Duration::from_secs(10)),
            "the forked child outlived its group"
        );
        assert!(
            wait_until(|| !alive(&leader), Duration::from_secs(10)),
            "the leader outlived its group"
        );
        assert_eq!(process.exit(), Some(Exit::Signal(15)));
    }

    #[cfg(unix)]
    #[test]
    fn a_group_that_ignores_the_graceful_signal_is_killed() {
        // The marker is what says the trap is installed: signalling before the
        // shell reaches it would end the group on the first signal instead.
        let process = Process::start(
            "trap '' TERM; echo ready; while true; do sleep 0.1; done",
            None,
            CAPTURE_BYTES,
            Input::Closed,
        )
        .expect("start");
        assert!(
            wait_until(
                || text(process.stdout()).contains("ready"),
                Duration::from_secs(10)
            ),
            "the shell never installed its trap"
        );
        assert!(process.is_running());
        assert!(process.terminate(false), "the group did not end");
        assert_eq!(process.exit(), Some(Exit::Signal(9)));
    }

    #[test]
    fn terminating_a_finished_process_reports_it_as_ended() {
        let process =
            Process::start(&exit_with(0), None, CAPTURE_BYTES, Input::Closed).expect("start");
        let _ = exit_of(&process);
        assert!(process.terminate(false));
    }

    #[cfg(unix)]
    #[test]
    fn a_background_child_ends_when_its_shell_exits() {
        let child;
        {
            let process = Process::start("sleep 30 & echo $!", None, CAPTURE_BYTES, Input::Closed)
                .expect("start");
            assert!(
                wait_until(|| forked_child(&process).is_some(), Duration::from_secs(10)),
                "the shell did not report the child it forked"
            );
            child = forked_child(&process).expect("a forked pid");
            assert_eq!(exit_of(&process), Exit::Code(0));
            assert!(
                wait_until(|| !alive(&child), Duration::from_secs(10)),
                "the background child outlived the shell that started it"
            );
        }
        assert!(!alive(&child));
    }

    /// Starts `command` under the shell with its group kept past its exit.
    #[cfg(unix)]
    fn start_kept(command: &str, cwd: Option<&Path>) -> Process {
        Process::start_argv_with(
            &shell_argv(command),
            cwd,
            CAPTURE_BYTES,
            &crate::command::minimal_environment(),
            Input::Closed,
            SpawnOptions {
                streams: Streams::Merged,
                leftovers: Leftovers::Keep,
            },
        )
        .expect("start")
    }

    #[cfg(unix)]
    #[test]
    fn a_kept_group_outlives_its_leader_until_ended() {
        let process = start_kept("sleep 30 & echo $!", None);
        assert!(
            wait_until(|| forked_child(&process).is_some(), Duration::from_secs(10)),
            "the shell did not report the child it forked"
        );
        let child = forked_child(&process).expect("a forked pid");
        assert_eq!(exit_of(&process), Exit::Code(0));
        std::thread::sleep(Duration::from_millis(200));
        assert!(alive(&child), "a kept group ended with its leader");
        process.end_leftovers();
        assert!(
            wait_until(|| !alive(&child), Duration::from_secs(10)),
            "ending the leftovers left the child running"
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_alive_reports_a_background_child() {
        let quiet = start_kept("exit 0", None);
        let _ = exit_of(&quiet);
        assert!(!quiet.group_alive(), "an empty group was reported alive");

        let process = start_kept("sleep 30 & echo $!", None);
        assert!(wait_until(
            || forked_child(&process).is_some(),
            Duration::from_secs(10)
        ));
        let child = forked_child(&process).expect("a forked pid");
        let _ = exit_of(&process);
        assert!(process.group_alive(), "the background child was not seen");
        drop(process);
        assert!(
            wait_until(|| !alive(&child), Duration::from_secs(10)),
            "a dropped handle left its kept group running"
        );
    }

    #[cfg(unix)]
    #[test]
    fn kill_after_ends_the_group_at_its_deadline() {
        let process = start_kept("sleep 30 & echo $!; wait", None);
        assert!(
            wait_until(|| forked_child(&process).is_some(), Duration::from_secs(10)),
            "the shell did not report the child it forked"
        );
        let child = forked_child(&process).expect("a forked pid");
        let began = Instant::now();
        process
            .kill_after(Duration::from_millis(300))
            .expect("timer");
        assert!(!process.timed_out());
        assert_eq!(exit_of(&process), Exit::Signal(9));
        assert!(
            began.elapsed() >= Duration::from_millis(300),
            "ended before its deadline"
        );
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "ended {:?} after its deadline",
            began.elapsed()
        );
        assert!(process.timed_out());
        assert!(
            wait_until(|| !alive(&child), Duration::from_secs(5)),
            "the group outlived its deadline"
        );

        let quick = start_kept("exit 0", None);
        quick.kill_after(Duration::from_secs(30)).expect("timer");
        assert_eq!(exit_of(&quick), Exit::Code(0));
        assert!(!quick.timed_out());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_stopped_list_does_not_start_its_next_command() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let process = Process::start(
            "echo ready; sleep 30; touch marker",
            Some(dir.path()),
            CAPTURE_BYTES,
            Input::Closed,
        )
        .expect("start");
        assert!(wait_until(
            || text(process.stdout()).contains("ready"),
            Duration::from_secs(10)
        ));
        assert!(process.terminate(true), "the group did not end");
        std::thread::sleep(Duration::from_secs(1));
        assert!(
            !dir.path().join("marker").exists(),
            "the next command of the list ran after the kill"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_process_ends_its_group() {
        let child;
        let leader;
        {
            let process = Process::start(
                "sleep 30 & echo $!; wait",
                None,
                CAPTURE_BYTES,
                Input::Closed,
            )
            .expect("start");
            assert!(wait_until(
                || forked_child(&process).is_some(),
                Duration::from_secs(10)
            ));
            child = forked_child(&process).expect("a forked pid");
            leader = process.id().to_string();
            assert!(alive(&child));
        }
        assert!(
            wait_until(|| !alive(&child), Duration::from_secs(10)),
            "a dropped handle left a process running"
        );
        assert!(
            wait_until(|| !alive(&leader), Duration::from_secs(10)),
            "a dropped handle left its leader running"
        );
    }
}
