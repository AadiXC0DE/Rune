"""Attach images by paste and by /image, through the real binary in a PTY."""

import fcntl
import http.server
import json
import os
import pathlib
import pty
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


requests = []
errors = []
scenario = sys.argv[3]
assert scenario in {"paste", "refuse"}, scenario


def chunk(delta, finish=None):
    return ("data: " + json.dumps({"choices": [{
        "index": 0, "delta": delta, "finish_reason": finish,
    }]}) + "\n\n").encode()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "image-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request)
            body = chunk({"content": "SAW-IT"}) + chunk({}, "stop") + b"data: [DONE]\n\n"
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


with tempfile.TemporaryDirectory(prefix="rune-image-") as directory:
    root = pathlib.Path(directory)
    workspace = root / "work"
    workspace.mkdir()
    image = workspace / "attachment.png"
    shutil.copy(sys.argv[2], image)
    (workspace / "notes.txt").write_text("not an image")
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
        "RUNE_MODEL": "image-fixture",
        "RUNE_API_KEY_ENV": "RUNE_IMAGE_TEST_KEY",
        "RUNE_IMAGE_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
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
        deadline = time.monotonic() + 15
        while text.encode() not in transcript[start:]:
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
        if scenario == "paste":
            os.write(master, b"\x1b[200~" + str(image).encode() + b"\x1b[201~")
            wait_for("[image #1: attachment.png", start)
            os.write(master, b" what is this")
            wait_for("what is this", start)
            time.sleep(0.2)
            os.write(master, b"\r")
            wait_for("SAW-IT", start)
            content = requests[-1]["messages"][-1]["content"]
            assert isinstance(content, list), content
            urls = [part["image_url"]["url"] for part in content if part.get("type") == "image_url"]
            assert urls and urls[0].startswith("data:image/png;base64,"), content
            texts = [part["text"] for part in content if part.get("type") == "text"]
            assert any("[image #1]" in text and "what is this" in text for text in texts), content
        else:
            os.write(master, b"/image notes.txt\r")
            wait_for("notes.txt is not a PNG, JPEG, GIF or WebP image", start)
            assert not requests, requests
        wait_for("/ commands", start)
        time.sleep(0.2)
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0
    finally:
        if child.poll() is None:
            child.kill()
            drain_wait(child, master, 10, bytearray())
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
