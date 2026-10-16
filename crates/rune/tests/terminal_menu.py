"""Exercise completion and model menus through real terminal input."""

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
        body = json.dumps({"data": [] if tagged_model else [
            {"id": f"menu-model-{index:02}"} for index in range(1, 16)
        ]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
        body = ('data: ' + json.dumps({"choices": [{
            "index": 0, "delta": {"content": "TAGGED-MODEL-REPLY"},
            "finish_reason": "stop",
        }]}) + '\n\ndata: [DONE]\n\n').encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


custom_completion = len(sys.argv) > 2 and sys.argv[2] == "custom-completion"
tagged_model = len(sys.argv) > 2 and sys.argv[2] == "tagged-model"

with tempfile.TemporaryDirectory(prefix="rune-terminal-menu-") as directory:
    root = pathlib.Path(directory)
    if custom_completion:
        commands = root / ".rune" / "commands"
        commands.mkdir(parents=True)
        (commands / "audit-command.md").write_text("Describe audit command files.")
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
        "RUNE_MODEL": "menu-model-01",
        "RUNE_API_KEY_ENV": "RUNE_MENU_TEST_KEY",
        "RUNE_MENU_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    master, slave = pty.openpty()
    rows, columns = (24, 80) if custom_completion or tagged_model else (8, 32)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
    child = subprocess.Popen(
        [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
        cwd=root, env=environment, preexec_fn=controlling_terminal,
    )
    os.close(slave)
    transcript = bytearray()
    captures = []

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

    def capture(stage, keys, expected):
        start = len(transcript)
        os.write(master, keys)
        wait_for(expected, start)
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.1)[0]:
            assert time.monotonic() < deadline, "terminal never finished drawing"
            transcript.extend(os.read(master, 65536))
        captures.append((stage, list(transcript)))

    try:
        wait_for("ctrl-c cancel")
        if tagged_model:
            capture("empty-catalog", b"/model\r", "no match for ``")
            # Empty and malformed IDs must leave the picker open. Typing a
            # valid ID afterwards proves Enter did not silently dismiss it.
            capture("invalid-tag", b"\rqwen3:", "no match for `qwen3:`")
            capture("typed-tag", b"\rlatest", "> use qwen3:latest (not listed)")
            capture("chosen-tag", b"\r", "model set to qwen3:latest")
            capture("reply", b"respond using the selected model\r", "TAGGED-MODEL-REPLY")
            assert requests, "no provider request was sent"
            assert all(request["model"] == "qwen3:latest" for request in requests), requests
            assert 'qwen3:latest' in config_file.read_text(), "choice was not saved"
        elif custom_completion:
            capture("custom-menu-tab", b"/audit", "> /audit-command")
            capture("custom-tab", b"\t", "> /audit-command")
            capture("custom-menu-enter", b"\x15/audit", "> /audit-command")
            capture("custom-enter", b"\r", "> /audit-command")
            capture("custom-expanded", b"\r", "/audit-command expanded; edit before sending")
            os.write(master, b"\x15")
        else:
            # Fifteen builtins, including /pin and commands beyond the first window.
            capture("completion-last", b"/" + b"\x1b[B" * 14, "> /quit")
            capture("completion-closed", b"\x1b", "\x1b[?25h")
            capture("model-last", b"/model\r" + b"\x1b[B" * 14, "> menu-model-15")
            capture("model-closed", b"\x1b", "\x1b[?25h")
        os.write(master, b"/quit\r")
        assert wait_for_exit(child, master, transcript, 10) == 0
        print(json.dumps({"captures": captures, "requests": requests} if tagged_model else captures))
    finally:
        if child.poll() is None:
            child.kill()
        wait_for_exit(child, master, transcript, 10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
