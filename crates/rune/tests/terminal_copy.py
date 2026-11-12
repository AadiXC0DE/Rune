"""/copy, /export and /diff through the real binary in a PTY."""

import base64
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
scenario = sys.argv[2]
assert scenario in {"copy", "tmux", "export", "diff"}, scenario
REPLY = "Two blocks:\n```sh\necho first\n```\nand\n```python\nprint('second')\n```\n"


def chunk(delta, finish=None):
    return ("data: " + json.dumps({"choices": [{
        "index": 0, "delta": delta, "finish_reason": finish,
    }]}) + "\n\n").encode()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "copy-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            last = request["messages"][-1]
            if scenario == "diff" and last["role"] == "user":
                body = chunk({"tool_calls": [{
                    "index": 0, "id": "edit-1", "type": "function",
                    "function": {"name": "edit_file", "arguments": json.dumps({
                        "path": "app.txt", "old_string": "beta", "new_string": "BETA",
                    })},
                }]}) + chunk({}, "tool_calls")
            elif scenario == "diff":
                body = chunk({"content": "EDITED"}) + chunk({}, "stop")
            else:
                body = chunk({"content": REPLY}) + chunk({}, "stop")
            body += b"data: [DONE]\n\n"
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


with tempfile.TemporaryDirectory(prefix="rune-copy-") as directory:
    root = pathlib.Path(directory)
    workspace = root / "work"
    workspace.mkdir()
    (workspace / "app.txt").write_text("alpha\nbeta\ngamma\n")
    empty = root / "empty-path"
    empty.mkdir()
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
        if not key.startswith("RUNE_") and key not in {"TMUX", "STY", "SSH_TTY", "SSH_CONNECTION"}
        and key.lower() not in {"http_proxy", "https_proxy", "all_proxy"}
    })
    environment.update({
        "HOME": directory,
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_STATE_HOME": str(root / "xdg-state"),
        "RUNE_HOME": str(root / "state"),
        "RUNE_PROVIDER": "chat_completions",
        "RUNE_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
        "RUNE_MODEL": "copy-fixture",
        "RUNE_API_KEY_ENV": "RUNE_COPY_TEST_KEY",
        "RUNE_COPY_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
        # No clipboard tool can be found, and the session counts as remote.
        "PATH": str(empty),
        "SSH_TTY": "/dev/pts/9",
    })
    if scenario == "tmux":
        environment["TMUX"] = "/tmp/tmux-1/default,1,0"
    if scenario == "diff":
        environment["RUNE_PERMISSION_MODE"] = "full-access"
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    try:
        child = subprocess.Popen(
            [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
            cwd=workspace, env=environment, preexec_fn=controlling_terminal,
        )
    finally:
        os.close(slave)
    transcript = bytearray()

    def wait_for(text, start=0):
        needle = text if isinstance(text, bytes) else text.encode()
        deadline = time.monotonic() + 15
        while needle not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
        assert not errors, errors

    try:
        wait_for("/ commands")
        start = len(transcript)
        os.write(master, b"answer\r")
        wait_for("EDITED" if scenario == "diff" else "second", start)
        wait_for("/ commands", start)
        time.sleep(0.3)
        start = len(transcript)
        if scenario in {"copy", "tmux"}:
            os.write(master, b"/copy code 2\r")
            wait_for("via OSC 52", start)
            encoded = base64.b64encode(b"print('second')")
            assert b"\x1b]52;c;" + encoded + b"\x1b\\" in transcript[start:], transcript[start:]
            if scenario == "tmux":
                assert b"\x1bPtmux;\x1b\x1b]52;c;" + encoded in transcript[start:], transcript[start:]
            else:
                assert b"\x1bPtmux;" not in transcript[start:]
        elif scenario == "export":
            os.write(master, b"/export notes.md\r")
            wait_for("wrote ", start)
            exported = workspace / "notes.md"
            assert exported.exists()
            assert exported.read_text().startswith("# "), exported.read_text()
            start = len(transcript)
            os.write(master, b"/export notes.md\r")
            wait_for("exists; name another file", start)
        else:
            os.write(master, b"/diff\r")
            wait_for("\x1b[?1049h", start)
            wait_for("b/app.txt", start)
            os.write(master, b"q")
            wait_for("\x1b[?1049l", start)
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0
        assert not errors, errors
    finally:
        if child.poll() is None:
            child.kill()
            drain_wait(child, master, 10, bytearray())
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
