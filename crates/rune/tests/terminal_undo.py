"""Undo a turn whose command created a file, preview it, then redo it."""

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


with tempfile.TemporaryDirectory(prefix="rune-undo-") as directory:
    root = pathlib.Path(directory)
    workspace = root / "work"
    workspace.mkdir()
    git_environment = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null")
    subprocess.run(["git", "init", "-q"], cwd=workspace, env=git_environment, check=True)
    (workspace / "a.txt").write_text("a\n")
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text(
        "auto_upgrade = false\npermission_mode = \"full_access\"\nallow_unsandboxed = true\n"
    )
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
        "RUNE_API_KEY_ENV": "RUNE_UNDO_TEST_KEY",
        "RUNE_UNDO_TEST_KEY": "fixture-key",
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
    made = workspace / "made-by-command"

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

    def command(line, expected):
        mark = len(transcript)
        os.write(master, line.encode() + b"\r")
        seen = wait_for(expected, mark)
        return wait_for("ctrl-c cancel", seen)

    try:
        wait_for("ctrl-c cancel")
        command("make the file", "Made.")
        assert made.read_text() == "x\n"
        command("/undo preview", "A made-by-command")
        assert made.exists(), "a preview changes nothing"
        command("/undo", "restored the workspace to before turn 1")
        assert not made.exists()
        command("/redo", "restored the workspace to after turn 1")
        assert made.read_text() == "x\n"
        command("/redo", "nothing to redo")
        print("RESTORED", flush=True)
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
