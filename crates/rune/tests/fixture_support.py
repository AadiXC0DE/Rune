"""Shared plumbing for the fixtures that spawn rune over a PTY.

Each fixture `.rs` wrapper prepends this file to the fixture it runs.

`drain_terminal` settles screen captures and handles both PTY EOF conventions.
It is an output-silence heuristic, not a command-completion signal.

`wait_for_exit` is the one shutdown path for a PTY child: it drains the master
while it waits, so the child can write its last frame and exit instead of
wedging on a full PTY buffer (macOS). The fixtures call it in place of a bare
`child.wait(timeout=...)`, which is what used to hang.

Every `subprocess.run` in those fixtures also reports what happened at its
deadline:

- which command was run (the rune arguments, without the machine-specific path),
- how long the wait actually was,
- on timeout, the child's last stdout and stderr before it was killed,
- on success, the child's exit status and elapsed time.

The wrapper is installed by rebinding `subprocess.run`; the original is kept and
every call keeps its exact semantics. Reports go to stderr, so a fixture whose
own stdout is structured data (for example the resume capture JSON) is not
disturbed. This only observes the fixtures: it changes no wait, no bound, and no
assertion.
"""

import errno as _errno
import os as _os
import select as _select
import subprocess as _subprocess
import sys as _sys
import time as _time

_run = _subprocess.run


def _pump(master, transcript, timeout):
    """Read once: bytes for output, b"" for EOF, None for no ready output.

    EOF remains selectable. macOS can return an empty read where Linux PTYs
    raise EIO; neither means there is another chunk to drain. Other errors
    (including a bad descriptor) must still fail the fixture.
    """
    ready, _, _ = _select.select([master], [], [], timeout)
    if not ready:
        return None
    try:
        chunk = _os.read(master, 65536)
    except OSError as error:
        if error.errno != _errno.EIO:
            raise
        return b""
    if transcript is not None:
        transcript.extend(chunk)
    return chunk


def drain_terminal(master, transcript, quiet=0.15, timeout=10, deadline=None):
    """Collect a frame until output is quiet or the PTY reaches EOF.

    Silence only settles screen captures; it does not prove that a command or
    turn completed. Fixtures must first wait for their expected product output,
    and use wait_for_exit for shutdown. A slave kept open without output is
    quiet, so this never waits for all inherited descriptors to close.
    """
    if deadline is None:
        deadline = _time.monotonic() + timeout
    while True:
        remaining = deadline - _time.monotonic()
        if remaining <= 0:
            raise AssertionError("terminal never became idle:\n" + _tail(transcript))
        if not _pump(master, transcript, min(quiet, remaining)):
            return


def wait_for_exit(child, master, transcript=None, timeout=10):
    """Waits for a PTY child to exit, draining the master while it shuts down.

    A terminal session writes its last frame as it exits. On a platform whose
    PTY buffer is small (macOS) a driver that merely waits lets that buffer
    fill, which wedges the child in a write and leaves it unreaped; a later
    kill can then hang too, because the child never leaves the write. Reading
    the master while waiting is what keeps the child moving -- the same
    remedy the MCP fixtures already use. Bytes read are appended to
    `transcript` when one is supplied, so a fixture keeps every byte it reads
    for its own assertions. Returns the exit status, and raises an
    AssertionError carrying the transcript tail if the child outlives
    `timeout`.
    """
    deadline = _time.monotonic() + timeout
    while child.poll() is None:
        remaining = deadline - _time.monotonic()
        if remaining <= 0:
            raise AssertionError(
                f"session did not exit within {timeout}s of the quit request:\n"
                + _tail(transcript)
            )
        _pump(master, transcript, min(remaining, 0.1))
    # Collect the bytes the child wrote just before it exited.
    _pump(master, transcript, 0)
    return child.returncode


def _tail(data, limit=4000):
    """The trailing bytes of a child's output, decoded for a readable log."""
    if not data:
        return ""
    if isinstance(data, (bytes, bytearray, memoryview)):
        data = bytes(data).decode("utf-8", "replace")
    return data[-limit:]


def _label(argv):
    """The rune arguments, which differ per phase and identify the wait."""
    return " ".join(str(part) for part in argv[1:]) or "<no arguments>"


def _report(message):
    print(message, file=_sys.stderr, flush=True)


def _diagnosed_run(*args, **kwargs):
    timeout = kwargs.get("timeout")
    if timeout is None or not args:
        return _run(*args, **kwargs)
    label = _label(args[0])
    started = _time.monotonic()
    _report(f"[fixture] run: {label}")
    try:
        output = _run(*args, **kwargs)
    except _subprocess.TimeoutExpired as error:
        elapsed = _time.monotonic() - started
        _report(
            f"[fixture] TIMEOUT: {label} did not exit within {timeout}s "
            f"(waited {elapsed:.2f}s); the child was killed"
        )
        _report(f"[fixture]   last stdout: {_tail(error.stdout)!r}")
        _report(f"[fixture]   last stderr: {_tail(error.stderr)!r}")
        raise
    elapsed = _time.monotonic() - started
    _report(
        f"[fixture] exit: {label} status={output.returncode} after {elapsed:.2f}s "
        f"(stdout {len(output.stdout or b'')}B, stderr {len(output.stderr or b'')}B)"
    )
    return output


_subprocess.run = _diagnosed_run
