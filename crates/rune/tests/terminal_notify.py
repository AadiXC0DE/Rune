"""Notifications follow the terminal's focus reports and the notify setting."""

import fcntl
import os
import pathlib
import pty
import re
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


TERMINAL_KEYS = {
    "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "TERM_FEATURES", "LC_TERMINAL",
    "LC_TERMINAL_VERSION", "KITTY_WINDOW_ID", "WEZTERM_EXECUTABLE", "WT_SESSION",
    "TMUX", "STY", "SSH_TTY", "SSH_CONNECTION", "VTE_VERSION", "ALACRITTY_WINDOW_ID",
}

with tempfile.TemporaryDirectory(prefix="rune-notify-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    off = sys.argv[3] == "off"
    config_file.write_text("auto_upgrade = false\n" + ('notify = "off"\n' if off else ""))
    config_file.chmod(0o600)
    environment = child_environment({
        key: value for key, value in os.environ.items()
        if not key.startswith("RUNE_") and key not in TERMINAL_KEYS and key.lower() not in {
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
        "TERM_PROGRAM": "iTerm.app",
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
        needle = text.encode() if isinstance(text, str) else text
        deadline = time.monotonic() + 15
        while needle not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
        return transcript.find(needle, start)

    def turn(text, reply):
        mark = len(transcript)
        os.write(master, text.encode() + b"\r")
        done = wait_for(reply, mark)
        wait_for("/ commands", done)

    try:
        wait_for("/ commands")
        first = bytes(transcript)
        focus_on = first.find(b"\x1b[?1004h")
        frame = first.find(b"\x1b[?2026h")
        assert focus_on >= 0, first
        assert frame < 0 or focus_on < frame, first

        os.write(master, b"\x1b[O")
        time.sleep(0.1)
        turn("hello", "First.")
        if off:
            sent = bytes(transcript)
            assert b"\x1b]9;" not in sent, sent
            outside = re.sub(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", b"", sent)
            assert b"\x07" not in outside, sent
        else:
            wait_for("\x1b]9;rune: finished in rune-notify-")
            os.write(master, b"\x1b[I")
            time.sleep(0.1)
            turn("again", "Second.")
            sent = bytes(transcript)
            assert sent.count(b"\x1b]9;") == 1, sent

        mark = len(transcript)
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0
        tail = bytes(transcript[mark:])
        assert b"\x1b[?1004l" in tail, tail
        if not off:
            # The body names the workspace directory and nothing the model wrote.
            body = f"\x1b]9;rune: finished in {root.name}\x1b\\".encode()
            assert body in bytes(transcript), bytes(transcript)
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
