"""The window title and progress follow the session, and are put back on exit."""

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


with tempfile.TemporaryDirectory(prefix="rune-title-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n" + ("terminal_title = false\n" if sys.argv[3] == "off" else ""))
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
        "WT_SESSION": "1",
        "RUNE_NO_PROBE": "1",
        "TERM": "xterm-256color",
    })
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
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

    try:
        wait_for("/ commands")
        first = bytes(transcript)
        on = sys.argv[3] == "on"
        if on:
            assert b"\x1b[22;2t" in first, first
            assert "\x1b]2;rune \u00b7 ".encode() in first, first
            assert "\u00b7 idle\x1b\\".encode() in first, first
        else:
            assert b"\x1b]2;" not in first and b"\x1b[22;2t" not in first, first
        mark = len(transcript)
        os.write(master, b"hello\r")
        wait_for("First.", mark)
        turn = bytes(transcript[mark:])
        assert b"\x1b]9;4;3;0\x07" in turn, turn
        if on:
            assert "\u00b7 working".encode() in turn, turn
        mark = transcript.find(b"First.", mark)
        wait_for("/ commands", mark)
        wait_for("\x1b]9;4;0;0\x07", 0)
        mark = len(transcript)
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0
        tail = bytes(transcript[mark:])
        if on:
            assert b"\x1b[23;2t" in tail, tail
        else:
            assert b"\x1b]2;" not in bytes(transcript) and b"\x1b[23;2t" not in tail, tail
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
