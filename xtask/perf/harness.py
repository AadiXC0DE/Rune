"""Shared pieces of the rendering measurements: a fake provider, a session in a
PTY and the resource use of finished children. Each measurement script is
appended to this file and run as one program."""

import fcntl
import http.server
import json
import os
import pathlib
import pty
import resource
import select
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


def chunk(delta, finish=None):
    return ("data: " + json.dumps({"choices": [{
        "index": 0, "delta": delta, "finish_reason": finish,
    }]}) + "\n\n").encode()


DONE = b"data: [DONE]\n\n"


def text_reply(parts):
    """A streamed answer, one delta per part."""
    return b"".join(chunk({"content": part}) for part in parts) + chunk({}, "stop") + DONE


def calls_reply(calls):
    """A streamed answer that asks for these `(id, name, arguments)` calls."""
    deltas = [
        chunk({"tool_calls": [{"index": index, "id": call_id, "type": "function", "function": {
            "name": name, "arguments": json.dumps(arguments),
        }}]})
        for index, (call_id, name, arguments) in enumerate(calls)
    ]
    return b"".join(deltas) + chunk({}, "tool_calls") + DONE


def serve(answer):
    """Starts a provider whose answer to each request is `answer(request)`."""

    class Provider(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            body = json.dumps({"data": [{"id": "perf-fixture"}]}).encode()
            self.respond(body, "application/json")

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            try:
                self.respond(answer(request), "text/event-stream")
            except (BrokenPipeError, ConnectionResetError):
                pass

        def respond(self, body, content_type):
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


class Session:
    """The binary in a PTY, against a fake provider, in a fresh state root."""

    def __init__(self, binary, root, port, workspace, cols=100, rows=30, config=""):
        config_dir = root / "config" / "rune"
        config_dir.mkdir(parents=True, mode=0o700)
        config_file = config_dir / "config.toml"
        config_file.write_text("auto_upgrade = false\n" + config)
        config_file.chmod(0o600)
        environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith("RUNE_") and key.lower() not in {
                "http_proxy", "https_proxy", "all_proxy",
            }
        }
        environment.update({
            "HOME": str(root),
            "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_DATA_HOME": str(root / "data"),
            "XDG_STATE_HOME": str(root / "xdg-state"),
            "RUNE_HOME": str(root / "state"),
            "RUNE_PROVIDER": "chat_completions",
            "RUNE_BASE_URL": f"http://127.0.0.1:{port}/v1",
            "RUNE_MODEL": "perf-fixture",
            "RUNE_API_KEY_ENV": "RUNE_PERF_TEST_KEY",
            "RUNE_PERF_TEST_KEY": "fixture-key",
            "NO_COLOR": "1",
            "TERM": "xterm-256color",
        })
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.child = subprocess.Popen(
            [os.path.abspath(binary)], stdin=slave, stdout=slave, stderr=slave,
            cwd=workspace, env=environment, preexec_fn=controlling_terminal,
        )
        os.close(slave)
        self.seen = bytearray()
        self.answered = False

    def read(self, timeout):
        if select.select([self.master], [], [], timeout)[0]:
            try:
                data = os.read(self.master, 1 << 20)
            except OSError:
                return 0
            self.seen.extend(data)
            # Every terminal answers the primary device attributes query, and
            # the startup probe waits for that answer, so the PTY gives it.
            if not self.answered and b"\x1b[c" in self.seen:
                os.write(self.master, b"\x1b[?62;22c")
                self.answered = True
            return len(data)
        return 0

    def wait_for(self, needle, start=0, timeout=10.0):
        """Returns the offset just past `needle`, failing loudly at the deadline."""
        needle = needle.encode() if isinstance(needle, str) else needle
        deadline = time.monotonic() + timeout
        while True:
            found = self.seen.find(needle, start)
            if found >= 0:
                return found + len(needle)
            if time.monotonic() > deadline or self.child.poll() is not None:
                tail = bytes(self.seen[-3000:]).decode(errors="replace")
                raise SystemExit(f"timed out waiting for {needle!r}; last output:\n{tail}")
            self.read(0.02)

    def write(self, data):
        os.write(self.master, data)

    def settle(self, quiet=0.3, limit=10.0):
        deadline = time.monotonic() + limit
        while time.monotonic() < deadline:
            if not self.read(quiet):
                return

    def quit(self):
        """Leaves the session and waits for the process, draining its output."""
        self.write(b"/quit\r")
        deadline = time.monotonic() + 30
        while self.child.poll() is None:
            if time.monotonic() > deadline:
                self.child.kill()
                raise SystemExit("the session did not exit")
            self.read(0.05)
        self.child.wait()
        os.close(self.master)


def children_usage():
    """CPU seconds and peak resident KiB of the waited children."""
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    # macOS reports bytes, Linux kibibytes.
    rss = usage.ru_maxrss // 1024 if sys.platform == "darwin" else usage.ru_maxrss
    return usage.ru_utime + usage.ru_stime, rss


def fresh_root(prefix):
    directory = tempfile.TemporaryDirectory(prefix=prefix)
    root = pathlib.Path(directory.name)
    workspace = root / "work"
    workspace.mkdir()
    return directory, root, workspace
