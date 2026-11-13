"""Pick a file from the @ menu and send the prompt that names it."""

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


with tempfile.TemporaryDirectory(prefix="rune-mention-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n")
    config_file.chmod(0o600)
    workspace = root / "workspace"
    (workspace / "src").mkdir(parents=True)
    (workspace / "docs").mkdir()
    (workspace / "src" / "main.rs").write_text("fn main() {}\n")
    (workspace / "src" / "lib.rs").write_text("pub fn lib() {}\n")
    (workspace / "docs" / "guide.md").write_text("# Guide\n")
    (workspace / ".gitignore").write_text("secret.txt\n")
    (workspace / "secret.txt").write_text("hidden\n")
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
        "RUNE_API_KEY_ENV": "RUNE_MENTION_TEST_KEY",
        "RUNE_MENTION_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    try:
        child = subprocess.Popen(
            [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
            cwd=workspace, env=environment, preexec_fn=controlling_terminal,
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
        mark = len(transcript)
        os.write(master, b"look at @mai")
        wait_for("> src/main.rs", mark)
        mark = len(transcript)
        os.write(master, b"\t")
        wait_for("look at @src/main.rs ", mark)
        mark = len(transcript)
        os.write(master, b"\r")
        wait_for("Read it.", mark)
        mark = transcript.find(b"Read it.", mark)
        wait_for("/ commands", mark)
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0
        assert b"secret.txt" not in transcript, transcript.decode(errors="replace")
    finally:
        if child.poll() is None:
            child.kill()
            # A session blocked writing to the terminal ends only when it is
            # drained while it exits.
            drain_wait(child, master, 10, bytearray())
        child.wait(timeout=10)
        os.close(master)
