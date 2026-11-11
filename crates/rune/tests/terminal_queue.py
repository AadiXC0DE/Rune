"""Queue follow-up messages while a turn runs, through a PTY and a local provider."""

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
assert scenario in {"order", "pull", "interrupt", "rebound"}, scenario


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
        body = json.dumps({"data": [{"id": "queue-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            prompt = request["messages"][-1]["content"]
            requests.append(prompt)
            held = prompt == "first"
            head = event(f"STARTED-{prompt}\n")
            tail = event(f"ANSWERED-{prompt}") + event(finish="stop") + b"data: [DONE]\n\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(head) + len(tail)))
            self.end_headers()
            self.wfile.write(head)
            self.wfile.flush()
            if held:
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


with tempfile.TemporaryDirectory(prefix="rune-queue-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n")
    config_file.chmod(0o600)
    if scenario == "rebound":
        (config_file.parent / "keys.toml").write_text('queue = "ctrl+q"\n')
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
        "RUNE_MODEL": "queue-fixture",
        "RUNE_API_KEY_ENV": "RUNE_QUEUE_TEST_KEY",
        "RUNE_QUEUE_TEST_KEY": "fixture-key",
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

    def capture(stage):
        # Drain complete frames before handing the byte stream to Rune's grid.
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.2)[0]:
            assert time.monotonic() < deadline, "terminal never finished drawing"
            transcript.extend(os.read(master, 65536))
        captures.append((stage, list(transcript)))

    def hold_first():
        wait_for("/ commands")
        start = len(transcript)
        os.write(master, b"first\r")
        wait_for("STARTED-first", start)
        # While the turn runs, the hint row names the keys that act on it.
        wait_for("enter steer   tab queue", start)

    try:
        if scenario == "order":
            hold_first()
            start = len(transcript)
            os.write(master, b"second\t")
            wait_for("queued 1: second", start)
            wait_for("tab queue   alt-up edit queued", start)
            os.write(master, b"third\t")
            wait_for("queued 2: third", start)
            capture("queued")
            release.set()
            wait_for("ANSWERED-third", start)
            capture("finished")
            assert requests == ["first", "second", "third"], requests
        elif scenario == "pull":
            hold_first()
            start = len(transcript)
            os.write(master, b"later\t")
            wait_for("queued 1: later", start)
            os.write(master, b"now\r")
            wait_for("steering: now", start)
            start = len(transcript)
            os.write(master, b"\x1b[1;3A")
            wait_for("> now", start)
            capture("pulled-steering")
            start = len(transcript)
            os.write(master, b"\x1b[1;3A")
            wait_for("later", start)
            capture("pulled-queued")
            release.set()
            wait_for("ANSWERED-first")
            assert requests == ["first"], requests
        elif scenario == "rebound":
            hold_first()
            start = len(transcript)
            os.write(master, b"second\t")
            wait_for("second", start)
            # Tab only completes now, which does nothing while a turn runs.
            read_some(0.5)
            assert b"queued 1:" not in transcript[start:], transcript.decode(errors="replace")
            os.write(master, b"\x11")
            wait_for("queued 1: second", start)
            capture("queued")
            release.set()
            wait_for("ANSWERED-second", start)
            assert requests == ["first", "second"], requests
        else:
            hold_first()
            start = len(transcript)
            os.write(master, b"x\t")
            wait_for("queued 1: x", start)
            # Control-C on an empty draft interrupts the running turn.
            os.write(master, b"\x03")
            wait_for("1 unsent message(s) returned to the draft", start)
            capture("interrupted")
            release.set()
            time.sleep(0.5)
            assert requests == ["first"], requests
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
