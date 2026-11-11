"""Run a `!` command and record a `#` note, then prompt after each."""

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


with tempfile.TemporaryDirectory(prefix="rune-bang-") as directory:
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
        "RUNE_API_KEY_ENV": "RUNE_BANG_TEST_KEY",
        "RUNE_BANG_TEST_KEY": "fixture-key",
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
        wait_for("ctrl-c cancel")
        os.write(master, b"!echo BANG-OUT\r")
        seen = wait_for("shell echo BANG-OUT")
        seen = wait_for("exit 0", seen)
        wait_for("ctrl-c cancel", seen)
        print("ROW", flush=True)
        assert sys.stdin.readline().strip() == "go"
        mark = len(transcript)
        os.write(master, b"what did it print\r")
        seen = wait_for("First.", mark)
        wait_for("ctrl-c cancel", seen)
        mark = len(transcript)
        os.write(master, b"# always use tabs\r")
        wait_for("add to AGENTS.md? y/n", mark)
        os.write(master, b"y")
        seen = wait_for("added to AGENTS.md", mark)
        wait_for("ctrl-c cancel", seen)
        notes = (workspace / "AGENTS.md").read_text()
        assert notes.endswith("- always use tabs\n"), notes
        mark = len(transcript)
        os.write(master, b"again\r")
        seen = wait_for("Second.", mark)
        wait_for("ctrl-c cancel", seen)
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
