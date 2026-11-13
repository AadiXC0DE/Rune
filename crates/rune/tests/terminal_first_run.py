"""Start banner tips and /init through the real binary in a PTY."""

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
scenario = sys.argv[2]
assert scenario in {"tips", "init"}, scenario


def chunk(delta, finish=None):
    return ("data: " + json.dumps({"choices": [{
        "index": 0, "delta": delta, "finish_reason": finish,
    }]}) + "\n\n").encode()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "first-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            prompts.append(request["messages"][-1]["content"])
            body = chunk({"content": "INIT-ANSWERED"}) + chunk({}, "stop") + b"data: [DONE]\n\n"
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


with tempfile.TemporaryDirectory(prefix="rune-first-run-") as directory:
    root = pathlib.Path(directory)
    workspace = root / "work"
    workspace.mkdir()
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
        "RUNE_MODEL": "first-fixture",
        "RUNE_API_KEY_ENV": "RUNE_FIRST_TEST_KEY",
        "RUNE_FIRST_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })

    def start():
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        try:
            child = subprocess.Popen(
                [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
                cwd=workspace, env=environment, preexec_fn=controlling_terminal,
            )
        finally:
            os.close(slave)
        return child, master, bytearray()

    def wait_for(child, master, transcript, text, start=0):
        deadline = time.monotonic() + 15
        while text.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
        assert not errors, errors

    def quit(child, master, transcript):
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0
        os.close(master)

    if scenario == "tips":
        tips = []
        for _ in range(2):
            child, master, transcript = start()
            try:
                wait_for(child, master, transcript, "/ commands")
                assert b"rune " in transcript and b" auto " in transcript, transcript
                text = transcript.decode(errors="replace")
                line = next(row for row in text.replace("\r", "\n").split("\n") if "tip: " in row)
                tips.append(line[line.index("tip: "):])
                quit(child, master, transcript)
            finally:
                if child.poll() is None:
                    child.kill()
                    drain_wait(child, master, 10, bytearray())
        assert tips[0] != tips[1], tips
    else:
        child, master, transcript = start()
        try:
            wait_for(child, master, transcript, "/ commands")
            begin = len(transcript)
            os.write(master, b"/init\r")
            wait_for(child, master, transcript, "INIT-ANSWERED", begin)
            assert prompts, prompts
            assert prompts[-1].startswith(
                "Write an AGENTS.md file at the repository root for coding agents working here."
            ), prompts[-1]
            # The echo is the command, not the whole prompt.
            assert b"\xe2\x96\x8c /init" in transcript[begin:], transcript[begin:]
            quit(child, master, transcript)
        finally:
            if child.poll() is None:
                child.kill()
                drain_wait(child, master, 10, bytearray())
    server.shutdown()
    server.server_close()
    server_thread.join(timeout=10)
