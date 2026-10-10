"""R-034: record resize events and bytes from the actual Rune PTY.

Only Python's standard library is needed. Rust replays these events into a grid
and compares the result with the committed fixtures.
"""

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


prompts = []
errors = []


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [
            {"id": f"replay-model-{index:02}"} for index in range(1, 16)
        ]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            prompts.append(request["messages"][-1]["content"])
            choices = [
                {"index": 0, "delta": {"content": "W" * 300 + "END-LONG-WORD"},
                 "finish_reason": None},
                {"index": 0, "delta": {}, "finish_reason": "stop"},
            ]
            body = ("".join(
                "data: " + json.dumps({"choices": [choice]}) + "\n\n"
                for choice in choices
            ) + "data: [DONE]\n\n").encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except Exception as error:
            errors.append(repr(error))
            self.send_error(500)


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


scenario = sys.argv[2]
assert scenario in {"resize", "narrow", "menu-resize"}, scenario
with tempfile.TemporaryDirectory(prefix="rune-r034-") as directory:
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
        "RUNE_MODEL": "replay-model-01" if scenario == "menu-resize" else "replay-fixture",
        "RUNE_API_KEY_ENV": "RUNE_REPLAY_TEST_KEY",
        "RUNE_REPLAY_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    cols, rows = {"resize": (32, 24), "narrow": (12, 24),
                  "menu-resize": (80, 24)}[scenario]
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
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
                f"timed out waiting for {text!r}:\n"
                + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
        assert not errors, errors

    def capture(stage, keys=b"", expected="\x1b[?25h", size=None):
        global offset, cols, rows
        start = len(transcript)
        if size is not None:
            # Drain the old size before recording the ioctl boundary. The
            # terminal driver delivers SIGWINCH to Rune's foreground group.
            assert not select.select([master], [], [], 0)[0]
            cols, rows = size
            fcntl.ioctl(master, termios.TIOCSWINSZ,
                        struct.pack("HHHH", rows, cols, 0, 0))
        if keys:
            os.write(master, keys)
        wait_for(expected, start)
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.15)[0]:
            assert time.monotonic() < deadline, "terminal never finished drawing"
            transcript.extend(os.read(master, 65536))
        captures.append({"stage": stage, "cols": cols, "rows": rows,
                         "bytes": list(transcript[offset:])})
        offset = len(transcript)

    try:
        # At 12 columns the hint is deliberately clipped to this prefix.
        wait_for("ctrl-c cance")
        if scenario == "resize":
            draft = "0123456789" * 5 + "VISIBLE-END"
            capture("narrow-draft", draft.encode(), "VISIBLE-END")
            # No key is sent on either resize. A stale draft must fail here.
            capture("grown-draft", expected=draft, size=(80, 24))
            capture("shrunk-draft", expected="VISIBLE-END", size=(20, 24))
            capture("grown-again", expected=draft, size=(80, 24))
            os.write(master, b"\x03")
            wait_for("\x1b[?25h", offset)
        elif scenario == "menu-resize":
            capture("model-open", b"/model\r", "type to narrow")
            capture("model-shrunk", expected="type to narrow", size=(32, 8))
            capture("model-down", b"\x1b[B", "> replay-model-02")
            capture("model-closed", b"\x1b")
            capture("closed-grown", size=(80, 24))
            assert not prompts, prompts
        else:
            # Wait for the committed ending, rather than an intermediate frame.
            capture("finished-answer", b"long-word\r", "END-LONG-W\r\nORD\r\n")
            assert prompts == ["long-word"], prompts
        os.write(master, b"/quit\r")
        assert child.wait(timeout=10) == 0
        assert not errors, errors
        print(json.dumps(captures))
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
