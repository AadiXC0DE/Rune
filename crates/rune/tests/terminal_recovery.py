"""Failed turns offer their recovery: retry, a rate-limit countdown, and /connect."""

import fcntl
import http.server
import json
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
import threading
import time


requests = []
errors = []
scenario = sys.argv[2]
assert scenario in {"retry", "rate", "auth"}, scenario


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
        body = json.dumps({"data": [{"id": "recovery-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def refuse(self, status, extra=()):
        body = json.dumps({"error": {"message": "fixture refusal"}}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for name, value in extra:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request["messages"][-1]["content"])
            if scenario == "retry" and len(requests) == 1:
                return self.refuse(500)
            if scenario == "rate":
                return self.refuse(429, [("retry-after", "3")])
            if scenario == "auth":
                return self.refuse(401)
            body = event("RECOVERED") + event(finish="stop") + b"data: [DONE]\n\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            errors.append(repr(error))


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="rune-recovery-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    attempts = "1" if scenario != "rate" else "5"
    config_file.write_text(f"auto_upgrade = false\n[limits]\nprovider_max_attempts = {attempts}\n")
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
        "RUNE_MODEL": "recovery-fixture",
        "RUNE_API_KEY_ENV": "RUNE_RECOVERY_TEST_KEY",
        "RUNE_RECOVERY_TEST_KEY": "fixture-key",
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

    def read_some(timeout):
        if select.select([master], [], [], timeout)[0]:
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

    def countdown(start):
        """Returns the newest countdown number drawn after `start`."""
        # Only changed cells are redrawn, so the number is read on its own.
        found = re.findall(rb"(\d+)s \(attempt", bytes(transcript[start:]))
        assert found, transcript[start:].decode(errors="replace")
        return int(found[-1])

    try:
        wait_for("/ commands")
        start = len(transcript)
        os.write(master, b"please work\r")
        if scenario == "retry":
            wait_for("error: ", start)
            wait_for("enter retries   esc dismisses", start)
            wait_for("/ commands", transcript.index(b"enter retries", start))
            time.sleep(0.2)
            again = len(transcript)
            os.write(master, b"\r")
            wait_for("RECOVERED", again)
            assert requests == ["please work", "please work"], requests
        elif scenario == "rate":
            wait_for("rate limited, retrying in", start)
            wait_for("(attempt 2 of 5)", start)
            first = countdown(start)
            mark = len(transcript)
            time.sleep(1.1)
            read_some(0.2)
            while select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
            second = countdown(mark)
            assert second < first, (first, second)
            interrupted = len(transcript)
            os.write(master, b"\x1b")
            wait_for("interrupted", interrupted)
        else:
            wait_for("error: ", start)
            wait_for("refused the credential; /connect stores a new key", start)
            wait_for("/ commands", transcript.index(b"/connect stores", start))
            time.sleep(0.2)
            # Enter on an empty draft does not retry a refused credential.
            count = len(requests)
            os.write(master, b"\r")
            time.sleep(0.5)
            assert len(requests) == count, requests
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 10, transcript) == 0
        assert not errors, errors
    finally:
        if child.poll() is None:
            child.kill()
            # A session blocked writing to the terminal ends only when it is
            # drained while it exits.
            drain_wait(child, master, 10, bytearray())
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
