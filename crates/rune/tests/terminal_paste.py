"""Paste into the prompt through a PTY and a local provider."""

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
scenario = sys.argv[2]
assert scenario in {"chip", "burst"}, scenario


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
        body = json.dumps({"data": [{"id": "paste-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request["messages"][-1]["content"])
            body = event("ANSWERED") + event(finish="stop") + b"data: [DONE]\n\n"
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


with tempfile.TemporaryDirectory(prefix="rune-paste-") as directory:
    root = pathlib.Path(directory)
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
        "RUNE_MODEL": "paste-fixture",
        "RUNE_API_KEY_ENV": "RUNE_PASTE_TEST_KEY",
        "RUNE_PASTE_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    if scenario == "burst":
        environment["RUNE_PASTE_BURST"] = "1"
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
    captures = []

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

    def wait_until(predicate, what):
        deadline = time.monotonic() + 10
        while not predicate():
            assert time.monotonic() < deadline, f"timed out waiting for {what}"
            read_some(0.05)
        assert not errors, errors

    def write_all(data):
        # The terminal takes input in small pieces; output is drained between
        # them so neither side waits on the other.
        view = memoryview(data)
        while view:
            _, writable, _ = select.select([], [master], [], 0.05)
            if writable:
                view = view[os.write(master, view[:1024]):]
            read_some(0)

    def capture(stage):
        # Drain complete frames before handing the byte stream to Rune's grid.
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.2)[0]:
            assert time.monotonic() < deadline, "terminal never finished drawing"
            transcript.extend(os.read(master, 65536))
        captures.append((stage, list(transcript)))

    try:
        wait_for("/ commands")
        if scenario == "chip":
            start = len(transcript)
            body = "".join(f"line {i}\n" for i in range(5000)).encode()
            write_all(b"\x1b[200~" + body + b"\x1b[201~")
            wait_for("[pasted #1: 5000 lines]", start)
            wait_for("ctrl-o expands the paste", start)
            capture("pasted")
            os.write(master, b"\r")
            wait_until(lambda: requests, "the request")
            sent = requests[-1]
            assert len(sent.split("\n")) == 5000, len(sent.split("\n"))
            assert sent.startswith("line 0\nline 1\n"), sent[:40]
            assert sent.endswith("line 4999"), sent[-40:]
        else:
            start = len(transcript)
            os.write(master, b"ab\rcd")
            wait_for("cd", start)
            deadline = time.monotonic() + 0.3
            while time.monotonic() < deadline:
                read_some(0.05)
            assert requests == [], requests
            capture("typed")
            time.sleep(0.2)
            os.write(master, b"\r")
            wait_until(lambda: requests, "the request")
            assert requests == ["ab\ncd"], requests
        assert not errors, errors
        print(json.dumps(captures))
    finally:
        if child.poll() is None:
            child.kill()
            # The session may be writing the long echo; reading lets the
            # write, and so the kill, finish.
            drain_wait(child, master, 10, transcript)
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
