"""R-038: save fixture replies and capture interactive resume before input."""

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
reply = "SAVED-REPLY\nUnicode 世界 é\nREPLY-END"
long_reply = "\n".join(f"SAVED-ROW-{index:02}" for index in range(1, 41))


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "resume-fixture"}]}).encode()
        self.respond(body, "application/json")

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        requests.append(request)
        prompt = request["messages"][-1]["content"]
        answer = long_reply if prompt == "long saved prompt" else (
            "CONTINUED-REPLY" if prompt == "continue saved prompt" else reply
        )
        choices = [
            {"index": 0, "delta": {"content": answer}, "finish_reason": None},
            {"index": 0, "delta": {}, "finish_reason": "stop"},
        ]
        usage = {"choices": [], "usage": {"prompt_tokens": 1234, "completion_tokens": 0}}
        body = ("".join("data: " + json.dumps({"choices": [choice]}) + "\n\n"
                        for choice in choices) + "data: " + json.dumps(usage) + "\n\n"
                + "data: [DONE]\n\n").encode()
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


with tempfile.TemporaryDirectory(prefix="rune-r038-") as directory:
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
        "RUNE_MODEL": "resume-fixture",
        "RUNE_API_KEY_ENV": "RUNE_RESUME_TEST_KEY",
        "RUNE_RESUME_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    child = None
    master = None
    transcript = bytearray()
    captures = []

    def run_pipe(*args, input=""):
        result = subprocess.run(
            [sys.argv[1], *args], input=input, capture_output=True, text=True,
            cwd=root, env=environment, timeout=15,
        )
        assert result.returncode == 0, (result.stdout, result.stderr)
        return result.stdout

    def launch(cols, *args):
        global child, master, transcript
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, cols, 0, 0))
        try:
            child = subprocess.Popen(
                [sys.argv[1], *args], stdin=slave, stdout=slave, stderr=slave,
                cwd=root, env=environment, preexec_fn=controlling_terminal,
            )
        finally:
            os.close(slave)
        transcript = bytearray()
        wait_for("ctrl-c cancel" if cols == 80 else "resume-fixt")
        drain()

    def wait_for(text, start=0):
        deadline = time.monotonic() + 10
        while text.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            readable, _, _ = select.select([master], [], [], 0.05)
            if readable:
                transcript.extend(os.read(master, 65536))

    def drain():
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            readable, _, _ = select.select([master], [], [], 0.15)
            if not readable:
                return
            transcript.extend(os.read(master, 65536))
        raise AssertionError("terminal never settled")

    def capture(stage, cols):
        captures.append({"stage": stage, "cols": cols, "bytes": list(transcript)})

    def stop():
        global child, master
        os.write(master, b"/quit\r")
        assert child.wait(timeout=10) == 0
        os.close(master)
        master = None
        child = None

    try:
        saved = json.loads(run_pipe("ask", "--json", "saved prompt"))
        session_id = saved["session_id"]
        log = root / "state" / "sessions" / session_id / "events.jsonl"
        original_log = log.read_bytes()
        assert len(requests) == 1

        launch(80, "resume", "last")
        capture("resume-last-before-input", 80)
        assert len(requests) == 1, "replay must not request another reply"
        assert log.read_bytes() == original_log, "replay must not append messages"
        os.write(master, b"draft")
        wait_for("> draft")
        drain()
        capture("resume-last-draft", 80)
        assert len(requests) == 1
        os.write(master, b"\x03")
        drain()
        start = len(transcript)
        os.write(master, b"continue saved prompt\r")
        wait_for("CONTINUED-REPLY", start)
        drain()
        assert [message for message in requests[-1]["messages"]
                if message["role"] != "system"] == [
            {"role": "user", "content": "saved prompt"},
            {"role": "assistant", "content": reply},
            {"role": "user", "content": "continue saved prompt"},
        ], requests[-1]
        stop()

        launch(80, "resume", session_id)
        capture("resume-id-before-input", 80)
        assert len(requests) == 2
        stop()

        piped = run_pipe("resume", session_id, input="/quit\n")
        assert "SAVED-REPLY" not in piped and "saved prompt" not in piped, piped
        assert len(requests) == 2

        json.loads(run_pipe("ask", "--json", "long saved prompt"))
        launch(32, "resume", "last")
        capture("resume-long-before-input", 32)
        assert len(requests) == 3
        stop()

        launch(80)
        capture("new-session-before-input", 80)
        assert len(requests) == 3
        stop()
        print(json.dumps(captures))
    finally:
        if child is not None:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=10)
        if master is not None:
            os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
