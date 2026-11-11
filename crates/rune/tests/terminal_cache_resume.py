"""Quit, edit AGENTS.md, resume: the provider's cached prefix still applies."""

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


with tempfile.TemporaryDirectory(prefix="rune-cache-resume-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\npermission_mode = \"ask\"\n")
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
        "RUNE_API_KEY_ENV": "RUNE_CONTEXT_TEST_KEY",
        "RUNE_CONTEXT_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    (root / "AGENTS.md").write_text("Keep answers short.\n")

    def session(*args):
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        try:
            child = subprocess.Popen(
                [sys.argv[1], *args], stdin=slave, stdout=slave, stderr=slave,
                cwd=root, env=environment, preexec_fn=controlling_terminal,
            )
        finally:
            os.close(slave)
        return child, master

    def drive(child, master, prompt, reply):
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

        try:
            wait_for("ctrl-c cancel")
            os.write(master, prompt.encode() + b"\r")
            wait_for(reply)
            mark = len(transcript)
            wait_for("ctrl-c cancel", mark)
            os.write(master, b"/quit\r")
            assert drain_wait(child, master, 15, transcript) == 0
        finally:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=10)
            os.close(master)

    drive(*session(), "hello", "First.")
    with (root / "AGENTS.md").open("a") as rules:
        rules.write("Name every file you change.\n")
    drive(*session("resume", "last"), "again", "Second.")
