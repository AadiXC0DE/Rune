"""The ? key overlay and /help in the viewer, through the real binary in a PTY."""

import fcntl
import os
import pathlib
import pty
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="rune-help-overlay-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n")
    config_file.chmod(0o600)
    environment = child_environment({
        key: value for key, value in os.environ.items()
        if not key.startswith("RUNE_") and key.lower() not in {
            "http_proxy", "https_proxy", "all_proxy",
        }
    })
    environment.update({
        "HOME": directory,
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_STATE_HOME": str(root / "xdg-state"),
        "RUNE_HOME": str(root / "state"),
        "RUNE_PROVIDER": "chat_completions",
        "RUNE_BASE_URL": "http://127.0.0.1:9/v1",
        "RUNE_MODEL": "overlay-fixture",
        "RUNE_API_KEY_ENV": "RUNE_OVERLAY_TEST_KEY",
        "RUNE_OVERLAY_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    try:
        child = subprocess.Popen(
            [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
            cwd=root, env=environment, preexec_fn=controlling_terminal,
        )
    finally:
        os.close(slave)
    transcript = bytearray()

    def wait_for(text, start=0):
        deadline = time.monotonic() + 15
        while text.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))

    def settle():
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.2)[0]:
            assert time.monotonic() < deadline, "terminal never finished drawing"
            transcript.extend(os.read(master, 65536))

    try:
        wait_for("/ commands")
        start = len(transcript)
        os.write(master, b"?")
        wait_for("editing", start)
        wait_for("during a turn", start)
        settle()
        # Any key closes the overlay and is not typed.
        start = len(transcript)
        os.write(master, b"x")
        wait_for("/ commands", start)
        settle()
        assert b"> x" not in transcript[start:], transcript[start:].decode(errors="replace")
        # The overlay is gone and the draft is still empty.
        start = len(transcript)
        os.write(master, b"/help\r")
        wait_for("\x1b[?1049h", start)
        wait_for("/model", start)
        start = len(transcript)
        os.write(master, b"q")
        wait_for("\x1b[?1049l", start)
        wait_for("/ commands", start)
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0
    finally:
        if child.poll() is None:
            child.kill()
            drain_wait(child, master, 10, bytearray())
        child.wait(timeout=10)
        os.close(master)
