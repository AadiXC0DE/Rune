"""R-072: ASCII decorations selected by flag, environment and profile."""

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


errors = []
tool_results = []


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "ascii-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if request["messages"][-1]["role"] == "tool":
                tool_results.append(request["messages"][-1]["content"])
                delta = {"content": "```text\n    " + "C" * 180 + "\n```\nASCII_REPLY_END"}
                finish = "stop"
            else:
                delta = {"tool_calls": [{
                    "index": 0, "id": "ascii-read", "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": json.dumps({"path": "fixture.txt"}),
                    },
                }]}
                finish = "tool_calls"
            body = (
                "data: " + json.dumps({"choices": [{
                    "index": 0, "delta": delta, "finish_reason": None,
                }]}) + "\n\n"
                + "data: " + json.dumps({"choices": [{
                    "index": 0, "delta": {}, "finish_reason": finish,
                }]}) + "\n\ndata: [DONE]\n\n"
            ).encode()
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


def capture(binary, root, environment, arguments):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    child = subprocess.Popen(
        [binary, *arguments], stdin=slave, stdout=slave, stderr=slave,
        cwd=root, env=environment, preexec_fn=controlling_terminal,
    )
    os.close(slave)
    transcript = bytearray()

    def wait_for(text, start=0):
        deadline = time.monotonic() + 10
        while text not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, transcript.decode(errors="replace")
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))

    def drain():
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.1)[0]:
            assert time.monotonic() < deadline, "terminal never finished drawing"
            transcript.extend(os.read(master, 65536))

    try:
        wait_for(b"ctrl-c cancel")
        wait_for(b"ascii-fixture")
        start = len(transcript)
        os.write(master, b"/mod")
        wait_for(b"> /model", start)
        drain()
        start = len(transcript)
        os.write(master, b"\x03fixture")
        os.write(master, b"\t")
        wait_for(b"> fixture", start)
        drain()
        # Cancel completion, then submit a multiline prompt for the history menu.
        os.write(master, b"\x1b")
        drain()
        os.write(master, b"\x03\x1b[200~fixture\nsecond\x1b[201~\r")
        wait_for(b"ASCII_REPLY_END")
        drain()
        assert b"read_file" in transcript, "tool summary was not displayed"
        assert b"    > " in transcript, "code continuation was not displayed"
        start = len(transcript)
        os.write(master, b"\x12")
        wait_for(b"> fixture / second", start)
        drain()
        os.write(master, b"\x1b")
        drain()
        start = len(transcript)
        os.write(master, b"\x0f")
        wait_for(b"Transcript", start)
        drain()
        start = len(transcript)
        os.write(master, b"\x1b[F")
        wait_for(b"ASCII_REPLY_END", start)
        drain()
        assert transcript.isascii(), transcript.decode(errors="replace")
        os.write(master, b"\x1b")
        drain()
        os.write(master, b"/quit\r")
        assert child.wait(timeout=10) == 0, transcript.decode(errors="replace")
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
try:
    for setting in ["flag", "environment", "profile"]:
        with tempfile.TemporaryDirectory(prefix="rune-r072-") as directory:
            root = pathlib.Path(directory)
            config = root / "config" / "rune"
            config.mkdir(parents=True, mode=0o700)
            config_file = config / "config.toml"
            config_file.write_text(
                "auto_upgrade = false\n" + ("ascii = true\n" if setting == "profile" else "")
            )
            config_file.chmod(0o600)
            (root / "fixture.txt").write_text("ASCII_TOOL_CONTENT\n" * 20)
            (root / "fixture-other.txt").write_text("another path\n")
            environment = {
                key: value for key, value in os.environ.items()
                if not key.startswith("RUNE_") and key not in {"NO_COLOR", "COLORTERM"}
            }
            environment.update({
                "HOME": directory,
                "XDG_CONFIG_HOME": str(root / "config"),
                "XDG_DATA_HOME": str(root / "data"),
                "XDG_STATE_HOME": str(root / "xdg-state"),
                "RUNE_HOME": str(root / "state"),
                "RUNE_PROVIDER": "chat_completions",
                "RUNE_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
                "RUNE_MODEL": "ascii-fixture",
                "RUNE_API_KEY_ENV": "RUNE_ASCII_TEST_KEY",
                "RUNE_ASCII_TEST_KEY": "fixture-key",
                "TERM": "xterm-256color",
            })
            if setting == "environment":
                environment["RUNE_ASCII"] = "true"
                environment["NO_COLOR"] = "1"
            capture(sys.argv[1], root, environment, ["--ascii"] if setting == "flag" else [])
    assert len(tool_results) == 3, tool_results
    assert all("ASCII_TOOL_CONTENT" in result for result in tool_results), tool_results
    assert not errors, errors
    print("ASCII status, completions, tool summaries, history and code verified in three PTYs")
finally:
    server.shutdown()
    server.server_close()
