//! A supervised child process and the bytes it produced.
//!
//! Where [`crate::command`] runs a command to completion, this module keeps one
//! running and hands back a handle, which is what a long-lived command needs.
//! Both share one process-group and exit-status vocabulary, so a caller never
//! has to learn a second one.

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};

use crate::command::{Exit, own_group};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Program every command is run by.
const SHELL: &str = "/bin/sh";

/// Bytes read from a pipe in one call.
const CHUNK_BYTES: usize = 8 * 1024;

/// Programs that deliver a signal, tried in order.
const KILL_PROGRAMS: [&str; 2] = ["/bin/kill", "kill"];

/// Signal sent first when a session is stopped.
const GRACEFUL_SIGNAL: &str = "TERM";

/// Signal sent when a graceful stop leaves something running.
const FORCE_SIGNAL: &str = "KILL";

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

/// The retained state of one captured stream.
#[derive(Debug, Default)]
struct Captured {
    /// Retained bytes, never more than the cap.
    bytes: Vec<u8>,
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
        // The head of the stream is kept rather than the tail: the first bytes
        // usually say what the command is and what it complained about.
        state.bytes.extend_from_slice(&chunk[..room]);
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

/// A child process in its own group, with the bytes it produced.
#[derive(Debug)]
pub struct Process {
    inner: Arc<Inner>,
}

/// State shared with the threads that watch one child.
#[derive(Debug)]
struct Inner {
    /// The child, until the waiter claims it.
    child: Mutex<Option<Child>>,
    /// Standard input, until the pipe closes.
    stdin: Mutex<Option<ChildStdin>>,
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
}

impl Process {
    /// Starts `command` under a shell in a new process group.
    ///
    /// The command string is passed as one argument, so what a caller reviewed
    /// is exactly what the shell parses. At most `capture_bytes` from each
    /// stream are retained.
    pub fn start(command: &str, cwd: Option<&Path>, capture_bytes: usize) -> io::Result<Self> {
        let mut spec = Command::new(SHELL);
        spec.arg("-c")
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            spec.current_dir(cwd);
        }
        own_group(&mut spec);
        let mut child = spec.spawn()?;
        let group = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();
        let inner = Arc::new(Inner {
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(stdin),
            exit: Mutex::new(None),
            pumps: Mutex::new(Vec::new()),
            stdout: Buffer::new(capture_bytes),
            stderr: Buffer::new(capture_bytes),
            group,
        });

        let started = spawn_waiter(Arc::clone(&inner))
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

    /// Writes bytes to the child's standard input.
    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let mut slot = lock(&self.inner.stdin);
        let Some(stdin) = slot.as_mut() else {
            return Err(io::Error::other("the process is not taking input"));
        };
        stdin.write_all(bytes)?;
        stdin.flush()
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

    /// Sends the forceful signal, falling back to the direct child when no
    /// signal program is available.
    fn end_group(&self) {
        if !self.signal(FORCE_SIGNAL) {
            self.kill_leader();
        }
    }

    /// Sends a signal to the process group, returning whether it was delivered.
    fn signal(&self, name: &str) -> bool {
        signal_group(self.inner.group, name)
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
        signal_group(self.group, FORCE_SIGNAL);
        if let Some(mut child) = lock(&self.child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Delivers no signal, on a platform with no process groups to signal.
///
/// Callers fall back to the direct child, which is the only process the
/// standard library can end without a signal.
#[cfg(not(unix))]
fn signal_group(_group: u32, _name: &str) -> bool {
    false
}

impl Drop for Process {
    fn drop(&mut self) {
        // A dropped handle has nobody left to read a graceful exit, and a
        // leaked handle would leave a command running forever, so the group is
        // ended outright rather than asked politely.
        if self.is_running() {
            self.terminate(true);
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
                    Ok(0) | Err(_) => break,
                    Ok(read) => buffer.append(&chunk[..read]),
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
    fn exit_of(process: &Process) -> Exit {
        assert!(
            wait_until(|| process.exit().is_some(), Duration::from_secs(10)),
            "the process did not end"
        );
        process.exit().expect("an exit status")
    }

    /// Returns the id the shell printed for the child it forked.
    fn forked_child(process: &Process) -> Option<String> {
        text(process.stdout())
            .lines()
            .next()
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()))
    }

    #[test]
    fn both_streams_are_captured_and_the_status_is_reported() {
        let process =
            Process::start("echo out; echo err 1>&2; exit 3", None, CAPTURE_BYTES).expect("start");
        let exit = exit_of(&process);
        assert_eq!(exit, Exit::Code(3));
        assert!(text(process.stdout()).contains("out"));
        assert!(text(process.stderr()).contains("err"));
    }

    #[test]
    fn a_successful_command_reports_success() {
        let process = Process::start("exit 0", None, CAPTURE_BYTES).expect("start");
        assert!(exit_of(&process).is_success());
    }

    #[test]
    fn a_process_takes_input_from_its_standard_input() {
        let process =
            Process::start("read line; echo got $line", None, CAPTURE_BYTES).expect("start");
        process.write(b"hello\n").expect("write");
        assert_eq!(exit_of(&process), Exit::Code(0));
        assert!(text(process.stdout()).contains("got hello"));
    }

    #[test]
    fn input_to_an_exited_process_is_refused() {
        let process = Process::start("exit 0", None, CAPTURE_BYTES).expect("start");
        let _ = exit_of(&process);
        assert!(process.write(b"hello\n").is_err());
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
    fn terminating_a_session_ends_a_forked_child() {
        let process =
            Process::start("sleep 30 & echo $!; wait", None, CAPTURE_BYTES).expect("start");
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

    #[test]
    fn a_group_that_ignores_the_graceful_signal_is_killed() {
        // The marker is what says the trap is installed: signalling before the
        // shell reaches it would end the group on the first signal instead.
        let process = Process::start(
            "trap '' TERM; echo ready; while true; do sleep 0.1; done",
            None,
            CAPTURE_BYTES,
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
        let process = Process::start("exit 0", None, CAPTURE_BYTES).expect("start");
        let _ = exit_of(&process);
        assert!(process.terminate(false));
    }

    #[test]
    fn dropping_a_process_ends_its_group() {
        let child;
        let leader;
        {
            let process =
                Process::start("sleep 30 & echo $!; wait", None, CAPTURE_BYTES).expect("start");
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
