"""R-011: reach the last completion and model in a real 32x8 terminal."""

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


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [
            {"id": f"menu-model-{index:02}"} for index in range(1, 16)
        ]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="rune-r011-") as directory:
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
        "RUNE_MODEL": "menu-model-01",
        "RUNE_API_KEY_ENV": "RUNE_MENU_TEST_KEY",
        "RUNE_MENU_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 8, 32, 0, 0))
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
        # Thirteen builtins, including the commands beyond the first window.
        capture("completion-last", b"/" + b"\x1b[B" * 12, "> /quit")
        capture("completion-closed", b"\x1b", "\x1b[?25h")
        capture("model-last", b"/model\r" + b"\x1b[B" * 14, "> menu-model-15")
        capture("model-closed", b"\x1b", "\x1b[?25h")
        os.write(master, b"/quit\r")
        assert child.wait(timeout=10) == 0
        print(json.dumps(captures))
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
