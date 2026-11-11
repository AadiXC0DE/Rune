"""End, pause, resume and crash Rune in a real PTY; the terminal comes back."""

import fcntl
import json
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


RESTORED = [b"\x1b[?2026l", b"\x1b[?2004l", b"\x1b[?7h"]


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def launch(root, environment):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    child = subprocess.Popen(
        [sys.argv[1], "--offline"], stdin=slave, stdout=slave, stderr=slave,
        cwd=root, env=environment, preexec_fn=controlling_terminal,
    )
    os.close(slave)
    return child, master


def reader(master, transcript, child):
    def wait_for(text, start=0, timeout=15):
        deadline = time.monotonic() + timeout
        while text not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            if select.select([master], [], [], 0.02)[0]:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    data = b""
                if not data:
                    assert text in transcript[start:], transcript.decode(errors="replace")
                    return
                transcript.extend(data)

    def until_exit(timeout=15):
        deadline = time.monotonic() + timeout
        while child.poll() is None or select.select([master], [], [], 0.05)[0]:
            assert time.monotonic() < deadline, "Rune did not exit"
            if select.select([master], [], [], 0.05)[0]:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    break
                if not data:
                    break
                transcript.extend(data)
        return child.wait(timeout=10)

    return wait_for, until_exit


def restored(after):
    return all(part in after for part in RESTORED) and after.rstrip(b"\r\n").endswith(b"\x1b[?25h")


with tempfile.TemporaryDirectory(prefix="rune-signals-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n")
    config_file.chmod(0o600)
    environment = child_environment({
        key: value for key, value in os.environ.items()
        if not key.startswith("RUNE_") and key not in {"TMUX", "STY"}
    })
    environment.update({
        "HOME": directory,
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_STATE_HOME": str(root / "xdg-state"),
        "RUNE_HOME": str(root / "state"),
        "RUNE_PROVIDER": "chat_completions",
        "RUNE_BASE_URL": "http://127.0.0.1:9/v1",
        "RUNE_MODEL": "signals-fixture",
        "RUNE_API_KEY_ENV": "RUNE_SIGNALS_TEST_KEY",
        "RUNE_SIGNALS_TEST_KEY": "fixture-key",
        "RUNE_NO_PROBE": "1",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    scenario = sys.argv[2]
    if scenario == "panic":
        environment["RUNE_DEBUG_PANIC_AFTER_FIRST_FRAME"] = "1"
    child, master = launch(root, environment)
    transcript = bytearray()
    wait_for, until_exit = reader(master, transcript, child)
    result = {}
    try:
        if scenario == "panic":
            status = until_exit()
            text = bytes(transcript)
            at = text.find(b"debug panic after the first frame")
            assert at > 0, text.decode(errors="replace")
            assert all(part in text[:at] for part in RESTORED), text.decode(errors="replace")
            result["status"] = status
        else:
            wait_for(b"ctrl-c cance")
            time.sleep(0.2)
            mark = len(transcript)
            if scenario == "suspend":
                os.write(master, b"\x1a")
                wait_for(b"\x1b[?2004l", mark)
                after = bytes(transcript[mark:])
                assert all(part in after for part in RESTORED), after
                mark = len(transcript)
                os.kill(child.pid, signal.SIGCONT)
                wait_for(b"\x1b[?2004h", mark)
                wait_for(b"ctrl-c cance", mark)
                os.write(master, b"\x04")
                result["status"] = until_exit()
            else:
                number = {"term": signal.SIGTERM, "hup": signal.SIGHUP, "int": signal.SIGINT}[scenario]
                os.kill(child.pid, number)
                result["status"] = until_exit()
                after = bytes(transcript[mark:])
                assert restored(after), after
                result["signal"] = int(number)
        print(json.dumps(result))
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
