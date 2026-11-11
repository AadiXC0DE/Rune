"""Interrupt a turn through the real binary, a PTY and a local provider."""

import fcntl
import http.server
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
import threading
import time


COMMAND = "echo $$ > pid.txt; exec sleep 30"
errors = []


def stream(*choices):
    return ("".join(
        "data: " + json.dumps({"choices": [choice]}) + "\n\n" for choice in choices
    ) + "data: [DONE]\n\n").encode()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "interrupt-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            last = request["messages"][-1]
            if last["role"] == "user" and last["content"] == "command":
                body = stream(
                    {"index": 0, "delta": {"tool_calls": [{
                        "index": 0,
                        "id": "shell-1",
                        "type": "function",
                        "function": {
                            "name": "shell",
                            "arguments": json.dumps({"action": "run", "command": COMMAND}),
                        },
                    }]}, "finish_reason": None},
                    {"index": 0, "delta": {}, "finish_reason": "tool_calls"},
                )
            else:
                body = stream(
                    {"index": 0, "delta": {"content": "INTERRUPT_FIXTURE_DONE"},
                     "finish_reason": None},
                    {"index": 0, "delta": {}, "finish_reason": "stop"},
                )
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            errors.append(repr(error))


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def exists(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


scenario = sys.argv[2]
assert scenario == "command", scenario

with tempfile.TemporaryDirectory(prefix="rune-interrupt-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n")
    config_file.chmod(0o600)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
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
        "RUNE_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
        "RUNE_MODEL": "interrupt-fixture",
        "RUNE_API_KEY_ENV": "RUNE_INTERRUPT_TEST_KEY",
        "RUNE_INTERRUPT_TEST_KEY": "fixture-key",
        "RUNE_PERMISSION_MODE": "full-access",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    try:
        child = subprocess.Popen(
            [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
            cwd=root, env=environment, preexec_fn=controlling_terminal,
        )
    finally:
        os.close(slave)
    transcript = bytearray()
    pid = None

    def read_some(timeout):
        readable, _, _ = select.select([master], [], [], timeout)
        if readable:
            transcript.extend(os.read(master, 65536))

    def wait_for(text, start=0):
        deadline = time.monotonic() + 10
        while text.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            read_some(0.05)
        assert not errors, errors

    try:
        wait_for("ctrl-c cancel")
        start = len(transcript)
        os.write(master, b"command\r")
        pid_file = root / "pid.txt"
        deadline = time.monotonic() + 10
        while not pid_file.exists() or not pid_file.read_text().strip():
            assert time.monotonic() < deadline, (
                "the command never started:\n" + transcript.decode(errors="replace")
            )
            read_some(0.05)
        pid = int(pid_file.read_text().strip())
        assert exists(pid), pid

        os.write(master, b"\x03")
        deadline = time.monotonic() + 1
        while exists(pid):
            assert time.monotonic() < deadline, (
                f"the command {pid} still runs 1 s after the interrupt:\n"
                + transcript.decode(errors="replace")
            )
            read_some(0.01)
        wait_for("[cancelled]", start)

        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 10, transcript) == 0
    finally:
        if pid is not None and exists(pid):
            os.kill(pid, signal.SIGKILL)
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
