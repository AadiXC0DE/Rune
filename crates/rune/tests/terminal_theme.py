"""A light terminal background picks the light theme; /theme previews live."""

import fcntl
import json
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


QUERY = b"\x1b[c"
REPLIES = (b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?62;22c")
WIDTH = 160


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="rune-theme-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n")
    config_file.chmod(0o600)
    environment = child_environment({
        key: value for key, value in os.environ.items()
        if not key.startswith("RUNE_") and key not in {
            "TMUX", "STY", "SSH_TTY", "SSH_CONNECTION", "NO_COLOR", "COLORFGBG",
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
        "RUNE_MODEL": "theme-fixture",
        "RUNE_API_KEY_ENV": "RUNE_THEME_TEST_KEY",
        "RUNE_THEME_TEST_KEY": "fixture-key",
        "COLORTERM": "truecolor",
        "TERM": "xterm-256color",
    })
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, WIDTH, 0, 0))
    child = subprocess.Popen(
        [sys.argv[1], "--offline"], stdin=slave, stdout=slave, stderr=slave,
        cwd=root, env=environment, preexec_fn=controlling_terminal,
    )
    os.close(slave)
    transcript = bytearray()

    def wait_for(text, start=0, timeout=10):
        deadline = time.monotonic() + timeout
        while text not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, transcript.decode(errors="replace")
            if select.select([master], [], [], 0.01)[0]:
                transcript.extend(os.read(master, 65536))

    def drain():
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.2)[0]:
            assert time.monotonic() < deadline, "terminal never became idle"
            data = os.read(master, 65536)
            if not data:
                return
            transcript.extend(data)

    try:
        wait_for(QUERY)
        os.write(master, REPLIES)
        wait_for(b"/ commands")
        drain()
        marks = {"first": len(transcript)}
        os.write(master, b"/theme\r")
        wait_for(b"themes")
        drain()
        os.write(master, b"\x1b[B")
        drain()
        marks["down"] = len(transcript)
        os.write(master, b"\x1b")
        drain()
        marks["escape"] = len(transcript)
        os.write(master, b"\x04")
        assert drain_wait(child, master, 10, transcript) == 0, transcript.decode(errors="replace")
        print(json.dumps({"bytes": list(transcript), "marks": marks}))
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
