"""Self-describing subprocess deadlines, shared by the fixtures that spawn rune.

Each fixture `.rs` wrapper prepends this file to the fixture it runs, so every
`subprocess.run` in those fixtures reports what happened at its deadline:

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

import subprocess as _subprocess
import sys as _sys
import time as _time

_run = _subprocess.run


def _tail(data, limit=4000):
    """The trailing bytes of a child's output, decoded for a readable log."""
    if not data:
        return ""
    if isinstance(data, bytes):
        data = data.decode("utf-8", "replace")
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
