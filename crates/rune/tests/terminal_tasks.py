"""List a running command with `/tasks`, stop it, then pin and hide a plan."""

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


with tempfile.TemporaryDirectory(prefix="rune-tasks-") as directory:
    root = pathlib.Path(directory)
    workspace = root / "work"
    workspace.mkdir()
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\npermission_mode = \"full_access\"\n")
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
        "RUNE_BASE_URL": sys.argv[2],
        "RUNE_MODEL": "scripted",
        "RUNE_API_KEY_ENV": "RUNE_TASKS_TEST_KEY",
        "RUNE_TASKS_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    try:
        child = subprocess.Popen(
            [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
            cwd=workspace, env=environment, preexec_fn=controlling_terminal,
        )
    finally:
        os.close(slave)
    transcript = bytearray()

    def wait_for(text, start=0):
        """Returns the offset just past the first match at or after `start`."""
        deadline = time.monotonic() + 15
        while text.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
        return transcript.index(text.encode(), start) + len(text.encode())

    try:
        wait_for("/ commands")
        os.write(master, b"start it\r")
        seen = wait_for("Started.")
        seen = wait_for("1 running", seen)
        seen = wait_for("/ commands", seen)
        mark = len(transcript)
        os.write(master, b"/tasks\r")
        seen = wait_for("running commands", mark)
        seen = wait_for("sleep 30", seen)
        time.sleep(0.3)
        os.write(master, b"\r")
        seen = wait_for("show output", seen)
        time.sleep(0.3)
        os.write(master, b"\x1b[B")
        time.sleep(0.3)
        os.write(master, b"\r")
        seen = wait_for("/ commands", seen)
        mark = len(transcript)
        os.write(master, b"/tasks\r")
        seen = wait_for("no commands are running", mark)
        wait_for("/ commands", seen)

        mark = len(transcript)
        os.write(master, b"make a plan\r")
        seen = wait_for("Planned.", mark)
        # Rows drawn after the answer belong to the live region, not scrollback.
        seen = wait_for("[~]", seen)
        wait_for("/ commands", seen)
        time.sleep(0.5)
        mark = len(transcript)
        os.write(master, b"\x14")
        time.sleep(1.0)
        while select.select([master], [], [], 0.05)[0]:
            transcript.extend(os.read(master, 65536))
        hidden = transcript[mark:]
        assert hidden, "Ctrl+T drew nothing"
        # A frame drawn before the key arrived may still hold the plan; the
        # newest frame must not.
        newest = hidden.rsplit(b"\x1b[?2026h", 1)[-1]
        assert b"[~]" not in newest, hidden.decode(errors="replace")
        mark = len(transcript)
        os.write(master, b"\x14")
        wait_for("[~]", mark)
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0
    finally:
        if child.poll() is None:
            child.kill()
            # A session blocked writing to the terminal ends only when it is
            # drained while it exits.
            drain_wait(child, master, 10, bytearray())
        child.wait(timeout=10)
        os.close(master)
