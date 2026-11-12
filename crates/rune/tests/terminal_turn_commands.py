"""Slash commands typed during a turn, argument values and custom expansion, through a PTY."""

import fcntl
import http.server
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
import threading
import time


requests = []
errors = []
release = threading.Event()
scenario = sys.argv[2]
assert scenario in {"turn", "values", "expand"}, scenario


def event(text=None, finish=None):
    return ("data: " + json.dumps({"choices": [{
        "index": 0,
        "delta": {} if text is None else {"content": text},
        "finish_reason": finish,
    }]}) + "\n\n").encode()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "commands-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request["messages"])
            prompt = request["messages"][-1]["content"]
            head = event(f"STARTED-{prompt}\n")
            tail = event(f"ANSWERED-{prompt}") + event(finish="stop") + b"data: [DONE]\n\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(head) + len(tail)))
            self.end_headers()
            self.wfile.write(head)
            self.wfile.flush()
            if prompt == "first":
                assert release.wait(20), "the held turn was never released"
            self.wfile.write(tail)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            errors.append(repr(error))


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="rune-turn-commands-") as directory:
    root = pathlib.Path(directory)
    workspace = root / "work"
    commands = workspace / ".rune" / "commands"
    commands.mkdir(parents=True)
    (commands / "greet.md").write_text("say hello to $1")
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n")
    config_file.chmod(0o600)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    server.daemon_threads = True
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
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
        "RUNE_MODEL": "commands-fixture",
        "RUNE_API_KEY_ENV": "RUNE_COMMANDS_TEST_KEY",
        "RUNE_COMMANDS_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    try:
        child = subprocess.Popen(
            [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
            cwd=workspace, env=environment, preexec_fn=controlling_terminal,
        )
    finally:
        os.close(slave)
    transcript = bytearray()
    captures = []

    def read_some(timeout):
        readable, _, _ = select.select([master], [], [], timeout)
        if readable:
            transcript.extend(os.read(master, 65536))

    def wait_for(text, start=0):
        deadline = time.monotonic() + 15
        while text.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            read_some(0.05)
        assert not errors, errors

    def capture(stage):
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.2)[0]:
            assert time.monotonic() < deadline, "terminal never finished drawing"
            transcript.extend(os.read(master, 65536))
        captures.append((stage, list(transcript)))

    try:
        wait_for("/ commands")
        if scenario == "turn":
            start = len(transcript)
            os.write(master, b"first\r")
            wait_for("STARTED-first", start)
            wait_for("enter steer", start)
            start = len(transcript)
            os.write(master, b"/status\r")
            wait_for("model          commands-fixture", start)
            wait_for("session        ", start)
            start = len(transcript)
            os.write(master, b"/model\r")
            wait_for("/model is available when the turn has finished", start)
            release.set()
            wait_for("ANSWERED-first", start)
            capture("finished")
            assert len(requests) == 1, requests
            assert "/status" not in json.dumps(requests), requests
        elif scenario == "values":
            start = len(transcript)
            os.write(master, b"/permissions ")
            wait_for("auto (current)", start)
            capture("menu")
            start = len(transcript)
            os.write(master, b"\x1b[B")
            time.sleep(0.2)
            os.write(master, b"\x1b[B")
            time.sleep(0.2)
            os.write(master, b"\t")
            wait_for("/permissions full_access", start)
            start = len(transcript)
            os.write(master, b"\r")
            wait_for("permissions set to", start)
            capture("set")
            assert requests == [], requests
        else:
            start = len(transcript)
            os.write(master, b"/greet Ana\r")
            wait_for("/greet expanded into the draft", start)
            capture("expanded")
            assert requests == [], requests
        assert not errors, errors
        print(json.dumps(captures))
    finally:
        release.set()
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
