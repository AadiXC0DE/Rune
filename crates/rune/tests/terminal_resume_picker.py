"""Choose a saved session in the resume picker, before and inside a session."""

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


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "picker-fixture"}]}).encode()
        self.respond(body, "application/json")

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        requests.append(request)
        # `ask` appends a note that the run is non-interactive; match the prompt.
        prompt = request["messages"][-1]["content"].split("\n\n<run_context>")[0]
        choices = [
            {"index": 0, "delta": {"content": f"REPLY {prompt}"}, "finish_reason": None},
            {"index": 0, "delta": {}, "finish_reason": "stop"},
        ]
        body = ("".join("data: " + json.dumps({"choices": [choice]}) + "\n\n"
                        for choice in choices) + "data: [DONE]\n\n").encode()
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


with tempfile.TemporaryDirectory(prefix="rune-picker-") as directory:
    root = pathlib.Path(directory)
    elsewhere = root / "elsewhere"
    elsewhere.mkdir()
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
        "RUNE_MODEL": "picker-fixture",
        "RUNE_API_KEY_ENV": "RUNE_PICKER_TEST_KEY",
        "RUNE_PICKER_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    child = None
    master = None
    transcript = bytearray()
    captures = []

    def ask(prompt, cwd):
        result = subprocess.run(
            [sys.argv[1], "ask", "--json", prompt], capture_output=True, text=True,
            cwd=cwd, env=environment, timeout=15,
        )
        assert result.returncode == 0, (result.stdout, result.stderr)

    def launch(*args):
        global child, master, transcript
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        try:
            child = subprocess.Popen(
                [sys.argv[1], *args], stdin=slave, stdout=slave, stderr=slave,
                cwd=root, env=environment, preexec_fn=controlling_terminal,
            )
        finally:
            os.close(slave)
        transcript = bytearray()

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

    def send(data, expected):
        start = len(transcript)
        os.write(master, data)
        wait_for(expected, start)
        drain()

    def capture(stage):
        captures.append({"stage": stage, "bytes": list(transcript)})

    def stop():
        global child, master
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 10, transcript) == 0
        os.close(master)
        master = None
        child = None

    try:
        ask("alpha task", root)
        ask("beta task", root)
        ask("other beta work", elsewhere)
        assert len(requests) == 3

        launch("-r")
        wait_for("sessions in this workspace")
        drain()
        capture("picker-open")
        send(b"bet", "> bet")
        capture("picker-filtered")
        send(b"\t", "sessions in every workspace")
        capture("picker-everywhere")
        send(b"\t", "sessions in this workspace")
        capture("picker-back")
        send(b"\r", "/ commands")
        capture("resumed")
        assert len(requests) == 3, "choosing a session must not send a request"
        stop()

        launch()
        wait_for("/ commands")
        drain()
        send(b"/resume\r", "sessions in this workspace")
        send(b"alpha", "> alpha")
        send(b"\r", "resumed session")
        capture("slash-resumed")
        send(b"continue alpha\r", "REPLY continue alpha")
        sent = [message for message in requests[0]["messages"] if message["role"] == "user"]
        assert sent[0]["content"].startswith("alpha task\n\n<run_context>"), sent
        assert [message for message in requests[-1]["messages"]
                if message["role"] != "system"] == [
            sent[0],
            {"role": "assistant", "content": "REPLY alpha task"},
            {"role": "user", "content": "continue alpha"},
        ], requests[-1]
        stop()
        listed = subprocess.run(
            [sys.argv[1], "sessions", "--json"], capture_output=True, text=True,
            cwd=root, env=environment, timeout=15,
        )
        assert listed.returncode == 0, (listed.stdout, listed.stderr)
        titles = [row["title"] for row in json.loads(listed.stdout)["sessions"]]
        # The prompt sent after switching does not rename the resumed session.
        assert "continue alpha" not in titles, titles
        print(json.dumps(captures))
    finally:
        if child is not None:
            if child.poll() is None:
                child.kill()
                # A session blocked writing to the terminal ends only when it is
                # drained while it exits.
                drain_wait(child, master, 10, bytearray())
            child.wait(timeout=10)
        if master is not None:
            os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
