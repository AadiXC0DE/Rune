"""Answer, ignore, or skip the startup terminal query in a real PTY."""

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
REPLIES = (b"\x1b[?1u\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?2026;2$y"
           b"\x1bP>|FakeTerm(1.0)\x1b\\\x1b[?62;22c")
ANSWERED = ("terminal       FakeTerm(1.0); background #ffffff (light); synchronized"
            " output yes; kitty keyboard 1; tmux no; ssh no")
SILENT = ("terminal       unknown; background unknown; synchronized output unknown;"
          " kitty keyboard no")
WIDTH = 160


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def reader(fd, transcript, child):
    def wait_for(text, start=0, timeout=10):
        deadline = time.monotonic() + timeout
        while text not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, transcript.decode(errors="replace")
            if select.select([fd], [], [], 0.01)[0]:
                transcript.extend(os.read(fd, 65536))

    def drain():
        deadline = time.monotonic() + 10
        while select.select([fd], [], [], 0.15)[0]:
            assert time.monotonic() < deadline, "terminal never became idle"
            data = os.read(fd, 65536)
            if not data:
                return
            transcript.extend(data)

    return wait_for, drain


def interactive(scenario, root, environment):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, WIDTH, 0, 0))
    child = subprocess.Popen(
        [sys.argv[1], "--offline"], stdin=slave, stdout=slave, stderr=slave,
        cwd=root, env=environment, preexec_fn=controlling_terminal,
    )
    os.close(slave)
    transcript = bytearray()
    wait_for, drain = reader(master, transcript, child)
    result = {}
    try:
        if scenario == "disabled":
            wait_for(b"/ commands")
            drain()
            assert QUERY not in transcript, transcript
        else:
            wait_for(QUERY)
            asked = time.monotonic()
            if scenario == "answered":
                half = len(REPLIES) // 2
                os.write(master, REPLIES[:half])
                time.sleep(0.02)
                os.write(master, REPLIES[half:])
            elif scenario == "typed":
                os.write(master, b"hello")
                time.sleep(0.02)
                os.write(master, REPLIES)
            wait_for(b"/ commands")
            result["wait"] = time.monotonic() - asked
            drain()
            assert transcript.count(QUERY) == 1, transcript
        result["first"] = list(transcript)
        if scenario == "typed":
            # Clear the typed draft so the status command starts on an empty line.
            os.write(master, b"\x15")
        start = len(transcript)
        os.write(master, b"/status\r")
        wait_for(b"context source", start)
        drain()
        text = bytes(transcript[start:]).decode(errors="replace")
        status = next(line for line in text.splitlines() if "terminal  " in line)
        result["status"] = status[status.index("terminal  "):]
        os.write(master, b"\x04")
        assert drain_wait(child, master, 10, transcript) == 0, transcript.decode(errors="replace")
        return result
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)


def quiet(args, ready, leave, root, environment):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, WIDTH, 0, 0))
    child = subprocess.Popen(
        [sys.argv[1], "--offline", *args], stdin=slave, stdout=slave, stderr=slave,
        cwd=root, env=environment, preexec_fn=controlling_terminal,
    )
    os.close(slave)
    transcript = bytearray()
    wait_for, drain = reader(master, transcript, child)
    try:
        wait_for(ready)
        drain()
        assert QUERY not in transcript, transcript
        os.write(master, leave)
        assert drain_wait(child, master, 10, transcript) == 0, transcript.decode(errors="replace")
        return {}
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)


def piped_stdin(root, environment):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, WIDTH, 0, 0))
    child = subprocess.Popen(
        [sys.argv[1], "--offline"], stdin=subprocess.PIPE, stdout=slave, stderr=slave,
        cwd=root, env=environment, preexec_fn=os.setsid,
    )
    os.close(slave)
    transcript = bytearray()
    try:
        child.stdin.write(b"/status\n")
        child.stdin.close()
        assert drain_wait(child, master, 10, transcript) == 0, transcript.decode(errors="replace")
        while select.select([master], [], [], 0.05)[0]:
            try:
                data = os.read(master, 65536)
            except OSError:
                break
            if not data:
                break
            transcript.extend(data)
        assert QUERY not in transcript, transcript
        text = transcript.decode(errors="replace")
        assert SILENT in text, text
        return {"status": SILENT}
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)


def piped_stdout(root, environment):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, WIDTH, 0, 0))
    read_end, write_end = os.pipe()
    child = subprocess.Popen(
        [sys.argv[1], "--offline"], stdin=slave, stdout=write_end, stderr=write_end,
        cwd=root, env=environment, preexec_fn=controlling_terminal,
    )
    os.close(slave)
    os.close(write_end)
    transcript = bytearray()
    wait_for, drain = reader(read_end, transcript, child)
    try:
        wait_for(b"/ commands")
        drain()
        start = len(transcript)
        os.write(master, b"/status\r")
        wait_for(b"context source", start)
        drain()
        assert QUERY not in transcript, transcript
        text = bytes(transcript[start:]).decode(errors="replace")
        assert SILENT in text, text
        os.write(master, b"\x04")
        assert drain_wait(child, read_end, 10, transcript) == 0, transcript.decode(errors="replace")
        return {"status": SILENT}
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(read_end)
        os.close(master)


with tempfile.TemporaryDirectory(prefix="rune-probe-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n")
    config_file.chmod(0o600)
    environment = child_environment({
        key: value for key, value in os.environ.items()
        if not key.startswith("RUNE_") and key not in {
            "TMUX", "STY", "SSH_TTY", "SSH_CONNECTION", "COLORTERM",
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
        "RUNE_MODEL": "probe-fixture",
        "RUNE_API_KEY_ENV": "RUNE_PROBE_TEST_KEY",
        "RUNE_PROBE_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    scenario = sys.argv[2]
    if scenario == "answered":
        result = interactive(scenario, root, environment)
        assert result["status"] == ANSWERED, result["status"]
    elif scenario == "silent":
        result = interactive(scenario, root, environment)
        assert result["status"].startswith(SILENT), result["status"]
        # No reply extends the wait, so the session starts well inside a second.
        assert result["wait"] < 0.9, result["wait"]
    elif scenario == "typed":
        result = interactive(scenario, root, environment)
        assert result["status"] == ANSWERED, result["status"]
    elif scenario == "disabled":
        result = interactive(scenario, root, environment | {"RUNE_NO_PROBE": "1"})
        assert result["status"].startswith(SILENT), result["status"]
    elif scenario == "accessible":
        result = quiet(["--accessible"], b"accessible mode:", b"/quit\n", root, environment)
    elif scenario == "stdin-pipe":
        result = piped_stdin(root, environment)
    elif scenario == "stdout-pipe":
        result = piped_stdout(root, environment)
    else:
        raise AssertionError(scenario)
    print(json.dumps(result))
