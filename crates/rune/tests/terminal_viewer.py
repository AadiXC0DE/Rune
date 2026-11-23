"""Drive the transcript viewer through a PTY and a local provider."""

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
stages = [threading.Event() for _ in range(3)]
scenario = sys.argv[2]
assert scenario in {"live", "result"}, scenario
FILE_LINES = 1500


def event(delta, finish=None):
    return ("data: " + json.dumps({"choices": [{
        "index": 0, "delta": delta, "finish_reason": finish,
    }]}) + "\n\n").encode()


def text(content):
    return event({"content": content})


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "viewer-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            last = request["messages"][-1]
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            if scenario == "result":
                if last["role"] == "tool":
                    body = text("READ-DONE") + event({}, "stop")
                else:
                    body = event({"tool_calls": [{
                        "index": 0, "id": "viewer-read", "type": "function",
                        "function": {
                            "name": "shell",
                            "arguments": json.dumps({"action": "run", "command": "cat big.txt"}),
                        },
                    }]}) + event({}, "tool_calls")
                self.wfile.write(body + b"data: [DONE]\n\n")
                self.wfile.flush()
                return
            head = "".join(f"EARLY-{row:02}\n\n" for row in range(30))
            self.wfile.write(text("NEEDLE first\n\n" + head))
            self.wfile.flush()
            assert stages[0].wait(20), "the first stage was never released"
            self.wfile.write(text("LIVE-ONE\n\n"))
            self.wfile.flush()
            assert stages[1].wait(20), "the second stage was never released"
            self.wfile.write(text("".join(f"LATER-{row:02}\n\n" for row in range(12))))
            self.wfile.flush()
            assert stages[2].wait(20), "the last stage was never released"
            self.wfile.write(text("LIVE-END") + event({}, "stop") + b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            errors.append(repr(error))


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="rune-viewer-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    # The command keeps its whole output; the model sees a bounded preview and
    # the rest is stored under a result handle.
    config_file.write_text(
        "auto_upgrade = false\npermission_mode = \"full_access\"\n"
        "[limits]\ncommand_output_bytes = 262144\n"
    )
    config_file.chmod(0o600)
    lines = [f"file line {row:05} " + "f" * 24 for row in range(FILE_LINES - 1)]
    lines.append("LAST-LINE-OF-FILE")
    (root / "big.txt").write_text("\n".join(lines) + "\n")
    assert (root / "big.txt").stat().st_size >= 60 * 1024
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
        "RUNE_MODEL": "viewer-fixture",
        "RUNE_API_KEY_ENV": "RUNE_VIEWER_TEST_KEY",
        "RUNE_VIEWER_TEST_KEY": "fixture-key",
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

    def wait_for(needle, start=0):
        deadline = time.monotonic() + 10
        while needle.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {needle!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            read_some(0.05)
        assert not errors, errors

    def capture(stage):
        settle()
        captures.append((stage, list(transcript)))

    def settle():
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.3)[0]:
            assert time.monotonic() < deadline, "terminal never finished drawing"
            transcript.extend(os.read(master, 65536))

    def send(data):
        start = len(transcript)
        os.write(master, data)
        return start

    try:
        wait_for("/ commands")
        if scenario == "live":
            start = send(b"first\r")
            wait_for("EARLY-29", start)
            start = send(b"keep")
            wait_for("keep", start)
            start = send(b"\x0f")
            wait_for("pager", start)
            capture("opened")
            start = len(transcript)
            stages[0].set()
            wait_for("LIVE-ONE", start)
            capture("followed")
            start = send(b"k")
            read_some(0.3)
            stages[1].set()
            wait_for("below,", start)
            capture("below")
            start = send(b"G")
            wait_for("LATER-11", start)
            capture("bottom")
            start = send(b"/NEEDLE\r")
            wait_for('"NEEDLE"', start)
            capture("searched")
            start = send(b"q")
            wait_for("\x1b[?1049l", start)
            # The draft typed before the viewer opened is still being edited.
            start = send(b"Z")
            wait_for("> keepZ", start)
            start = len(transcript)
            stages[2].set()
            wait_for("LIVE-END", start)
        else:
            start = send(b"read it\r")
            wait_for("READ-DONE", start)
            # The window title says idle once the turn has been recorded.
            wait_for("\u00b7 idle", start)
            start = send(b"\x0f")
            wait_for("pager", start)
            # The first Tab selects the call, the second its result.
            start = send(b"\t")
            wait_for("collapsed", start)
            send(b"\t")
            # Only the changed cells are written, so wait for the frame to settle.
            settle()
            start = send(b"e")
            wait_for("expanded", start)
            start = send(b"G")
            wait_for("LAST-LINE-OF-FILE", start)
            capture("expanded")
            # The middle of the file is only in the stored result.
            start = send(b"/01200\r")
            wait_for('"01200"', start)
            capture("middle")
            start = send(b"q")
            wait_for("\x1b[?1049l", start)
        assert not errors, errors
        print(json.dumps(captures))
    finally:
        for stage in stages:
            stage.set()
        if child.poll() is None:
            child.kill()
        # A closing terminal waits for its output to be read.
        deadline = time.monotonic() + 10
        while child.poll() is None and time.monotonic() < deadline:
            try:
                read_some(0.1)
            except OSError:
                break
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
