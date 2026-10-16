"""R-115: capture estimates, provider usage, omitted usage, and a new session."""

import fcntl
import http.server
import json
import os
import pathlib
import select
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


requests = []


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.respond(b'{"data":[{"id":"provenance-fixture"}]}', "application/json")

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        requests.append(request)
        event = {"choices": [{"index": 0, "delta": {
            "content": "PROVENANCE-REPLY-" + str(len(requests)),
        }, "finish_reason": "stop"}]}
        if len(requests) == 1:
            event["usage"] = {"prompt_tokens": 1234, "completion_tokens": 66}
        body = ("data: " + json.dumps(event) + "\n\ndata: [DONE]\n\n").encode()
        self.respond(body, "text/event-stream")

    def respond(self, body, content_type):
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="rune-r115-") as directory:
    root = pathlib.Path(directory)
    config = root / "config.toml"
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    server.daemon_threads = True
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    config.write_text(
        'auto_upgrade = false\nprovider = "chat_completions"\n'
        f'base_url = "http://127.0.0.1:{server.server_port}/v1"\n'
        'api_key_env = "RUNE_PROVENANCE_TEST_KEY"\n'
        '[models.chat_completions]\nid = "provenance-fixture"\n'
        'context_window = 128000\n'
    )
    environment = fixture_environment()
    environment.update({
        "HOME": directory,
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_STATE_HOME": str(root / "xdg-state"),
        "RUNE_HOME": str(root / "state"),
        "RUNE_CONFIG": str(config),
        "RUNE_PROVENANCE_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    master, slave = open_terminal()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 120, 0, 0))
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

    def capture(stage, expected, keys=b""):
        global offset
        if keys:
            write_terminal(master, keys, transcript)
        deadline = time.monotonic() + 10
        while expected.encode() not in transcript[offset:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {expected!r}:\n"
                + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
        drain_terminal(master, transcript, deadline=deadline)
        captures.append((stage, list(transcript[offset:])))
        offset = len(transcript)

    try:
        capture("initial-footer", "(request bytes)")
        capture("initial-status", "context source request bytes (estimate)", b"/status\r")
        assert not requests, "status must not send a provider request"
        capture("provider-footer", "PROVENANCE-REPLY-1\r\n", b"reported\r")
        capture("provider-status", "context source provider-reported", b"/status\r")
        capture("missing-footer", "PROVENANCE-REPLY-2\r\n", b"missing\r")
        capture("missing-status", "context source request bytes (estimate)", b"/status\r")
        capture("new-footer", "started session", b"/new\r")
        capture("new-status", "context source request bytes (estimate)", b"/status\r")
        assert len(requests) == 2, requests
        write_terminal(master, b"/quit\r", transcript)
        assert wait_for_exit(child, master, transcript, 10) == 0
        print(json.dumps(captures))
    finally:
        if child.poll() is None:
            child.kill()
        wait_for_exit(child, master, transcript, 10)
        close_terminal(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
