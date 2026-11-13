"""Run a session that compacts, optionally quitting and resuming before its last prompt."""

import fcntl
import os
import pathlib
import pty
import select
import shutil
import struct
import subprocess
import sys
import termios
import time

binary, base_url, base, mode = sys.argv[1:5]
base = pathlib.Path(base)
if base.exists():
    shutil.rmtree(base)
home = base / "home"
workspace = base / "work"
config = home / "config" / "rune"
config.mkdir(parents=True, mode=0o700)
workspace.mkdir(parents=True)
config_file = config / "config.toml"
config_file.write_text("auto_upgrade = false\npermission_mode = \"ask\"\n")
config_file.chmod(0o600)
for index in range(1, 8):
    line = f"file {index} holds this line of fixture text\n"
    text = (line * (3000 // len(line) + 1))[:2999] + "\n"
    (workspace / f"f{index}.txt").write_text(text)

environment = child_environment({
    key: value for key, value in os.environ.items()
    if not key.startswith("RUNE_") and key.lower() not in {
        "http_proxy", "https_proxy", "all_proxy",
    }
})
environment.update({
    "HOME": str(home),
    "XDG_CONFIG_HOME": str(home / "config"),
    "XDG_DATA_HOME": str(home / "data"),
    "XDG_STATE_HOME": str(home / "xdg-state"),
    "RUNE_HOME": str(home / "state"),
    "RUNE_PROVIDER": "chat_completions",
    "RUNE_BASE_URL": base_url,
    "RUNE_MODEL": "scripted",
    "RUNE_API_KEY_ENV": "RUNE_FIDELITY_TEST_KEY",
    "RUNE_FIDELITY_TEST_KEY": "fixture-key",
    "RUNE_CONTEXT_WINDOW": "64000",
    "RUNE_LIMITS": "compaction_trigger_percent=13,compaction_keep_tokens=1000",
    "NO_COLOR": "1",
    "TERM": "xterm-256color",
})


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


class Session:
    def __init__(self, *args):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        try:
            self.child = subprocess.Popen(
                [binary, *args], stdin=slave, stdout=slave, stderr=slave,
                cwd=workspace, env=environment, preexec_fn=controlling_terminal,
            )
        finally:
            os.close(slave)
        self.transcript = bytearray()
        self.wait_for("/ commands")

    def wait_for(self, text, start=0):
        deadline = time.monotonic() + 30
        while text.encode() not in self.transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + self.transcript.decode(errors="replace")
            )
            assert self.child.poll() is None, (
                "Rune exited before the expected frame:\n"
                + self.transcript.decode(errors="replace")
            )
            if select.select([self.master], [], [], 0.05)[0]:
                self.transcript.extend(os.read(self.master, 65536))

    def send(self, line, expect):
        mark = len(self.transcript)
        os.write(self.master, line.encode() + b"\r")
        self.wait_for(expect, mark)
        # The idle footer is drawn again once the turn is over.
        self.wait_for("/ commands", self.transcript.index(expect.encode(), mark))

    def quit(self):
        os.write(self.master, b"/quit\r")
        try:
            assert drain_wait(self.child, self.master, 30, self.transcript) == 0
        finally:
            if self.child.poll() is None:
                self.child.kill()
            self.child.wait(timeout=10)
            os.close(self.master)


session = Session()
session.send("/plan", "plan mode is on")
session.send("read two files", "Read two.")
session.send("/plan off", "plan mode is off")
session.send("read five files", "Read five.")
if mode == "resume":
    session.quit()
    session = Session("resume")
session.send("last", "Last.")
session.quit()
