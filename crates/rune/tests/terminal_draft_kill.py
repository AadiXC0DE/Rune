"""An unsent draft survives a killed session; a sent one does not come back."""

import fcntl
import os
import pathlib
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="rune-draft-kill-") as directory:
    root = pathlib.Path(directory)
    workspace = root / "work"
    workspace.mkdir()
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
        "RUNE_MODEL": "draft-fixture",
        "RUNE_API_KEY_ENV": "RUNE_DRAFT_TEST_KEY",
        "RUNE_DRAFT_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })

    def start():
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        try:
            child = subprocess.Popen(
                [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
                cwd=workspace, env=environment, preexec_fn=controlling_terminal,
            )
        finally:
            os.close(slave)
        return child, master, bytearray()

    def wait_for(child, master, transcript, text, start=0):
        deadline = time.monotonic() + 15
        while text.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))

    def kill(child, master):
        os.kill(child.pid, signal.SIGKILL)
        drain_wait(child, master, 10, bytearray())
        os.close(master)

    # Typed, left for three seconds, then killed: the draft comes back.
    child, master, transcript = start()
    try:
        wait_for(child, master, transcript, "/ commands")
        os.write(master, b"a")
        time.sleep(0.1)
        os.write(master, b"bc")
        wait_for(child, master, transcript, "> abc")
        time.sleep(3)
        kill(child, master)
    finally:
        if child.poll() is None:
            child.kill()
    child, master, transcript = start()
    try:
        wait_for(child, master, transcript, "restored the unsent draft")
        wait_for(child, master, transcript, "> abc")
        # Sent, then killed: nothing to restore.
        os.write(master, b"\x15/status\r")
        wait_for(child, master, transcript, "model          draft-fixture")
        time.sleep(0.5)
        kill(child, master)
    finally:
        if child.poll() is None:
            child.kill()
    child, master, transcript = start()
    try:
        wait_for(child, master, transcript, "/ commands")
        time.sleep(0.5)
        while select.select([master], [], [], 0.1)[0]:
            transcript.extend(os.read(master, 65536))
        assert b"restored the unsent draft" not in transcript, transcript.decode(errors="replace")
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0
        os.close(master)
    finally:
        if child.poll() is None:
            child.kill()
