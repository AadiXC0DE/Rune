"""R-075: capture two pending retries and the requests that follow them."""

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
release_second = threading.Event()
release_third = threading.Event()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = b'{"data":[{"id":"retry-fixture"}]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            attempt = len(requests)
            if attempt == 2:
                assert release_second.wait(10), "second retry was never observed"
            if attempt == 3:
                assert release_third.wait(10), "third retry was never observed"
            if attempt <= 2:
                body = b'{"error":{"message":"temporary fixture failure"}}'
                self.send_response(429 if attempt == 1 else 503)
                self.send_header("Retry-After", str(attempt))
                self.send_header("Content-Type", "application/json")
            else:
                body = ('data: ' + json.dumps({"choices": [{
                    "index": 0, "delta": {"content": "RETRY-RECOVERED"},
                    "finish_reason": "stop",
                }]}) + '\n\ndata: [DONE]\n\n').encode()
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


with tempfile.TemporaryDirectory(prefix="rune-r075-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n[limits]\nprovider_max_attempts = 3\n")
    config_file.chmod(0o600)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    server.daemon_threads = True
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    environment = {
        key: value for key, value in os.environ.items()
        if not key.startswith("RUNE_") and key.lower() not in {
            "http_proxy", "https_proxy", "all_proxy",
        }
    }
    environment.update({
        "HOME": directory,
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_STATE_HOME": str(root / "xdg-state"),
        "RUNE_HOME": str(root / "state"),
        "RUNE_PROVIDER": "chat_completions",
        "RUNE_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
        "RUNE_MODEL": "retry-fixture",
        "RUNE_API_KEY_ENV": "RUNE_RETRY_TEST_KEY",
        "RUNE_RETRY_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
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
    captures = []
    offset = 0

    def wait_for(text, start=0):
        deadline = time.monotonic() + 10
        while text.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
        assert not errors, errors

    def capture(status, expected=None):
        global offset
        wait_for(expected or status, offset)
        # A complete frame restores the caret after drawing its footer.
        marker = transcript.find((expected or status).encode(), offset)
        wait_for("\x1b[?25h", marker)
        captures.append((status, list(transcript[offset:])))
        offset = len(transcript)

    try:
        wait_for("ctrl-c cancel")
        os.write(master, b"retry please\r")
        capture("provider retry 2/3 in 1000ms")
        assert len(requests) == 1, requests
        capture("provider retry 2/3 |")
        release_second.set()
        capture("provider retry 3/3 in 2000ms")
        assert len(requests) == 2, requests
        capture("provider retry 3/3 |")
        release_third.set()
        wait_for("RETRY-RECOVERED", offset)
        # Drain through the worker poll and completed exchange's final frame.
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.15)[0]:
            assert time.monotonic() < deadline, "the completed renderer did not settle"
            transcript.extend(os.read(master, 65536))
        captures.append(("completed", list(transcript[offset:])))
        assert len(requests) == 3, requests
        assert all(request["messages"][-1]["content"] == "retry please" for request in requests)
        os.write(master, b"/quit\r")
        assert child.wait(timeout=10) == 0
        print(json.dumps(captures))
    finally:
        release_second.set()
        release_third.set()
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
