"""Time from spawn to the first drawn terminal frame of the rune binary."""

import fcntl
import http.server
import json
import os
import pathlib
import pty
import select
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

SAMPLES = 11
MARKER = b"ctrl-c cancel"


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "perf-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def sample(binary, environment, cwd):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    started = time.monotonic()
    child = subprocess.Popen(
        [binary], stdin=slave, stdout=slave, stderr=slave,
        cwd=cwd, env=environment, preexec_fn=controlling_terminal,
    )
    os.close(slave)
    seen = bytearray()
    try:
        deadline = started + 10
        while MARKER not in seen:
            if time.monotonic() > deadline or child.poll() is not None:
                tail = bytes(seen[-2000:]).decode(errors="replace")
                raise SystemExit("no first frame; last output:\n" + tail)
            if select.select([master], [], [], 0.01)[0]:
                try:
                    seen.extend(os.read(master, 65536))
                except OSError:
                    break
        elapsed = (time.monotonic() - started) * 1000
        os.write(master, b"\x04")
        deadline = time.monotonic() + 5
        while child.poll() is None:
            if time.monotonic() > deadline:
                child.kill()
                break
            if select.select([master], [], [], 0.05)[0]:
                try:
                    os.read(master, 65536)
                except OSError:
                    pass
        child.wait()
        return elapsed
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)


def main():
    binary = sys.argv[1]
    with tempfile.TemporaryDirectory(prefix="rune-frame-") as directory:
        root = pathlib.Path(directory)
        config = root / "config" / "rune"
        config.mkdir(parents=True, mode=0o700)
        config_file = config / "config.toml"
        config_file.write_text("auto_upgrade = false\n")
        config_file.chmod(0o600)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
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
            "RUNE_MODEL": "perf-fixture",
            "RUNE_API_KEY_ENV": "RUNE_PERF_TEST_KEY",
            "RUNE_PERF_TEST_KEY": "fixture-key",
            "NO_COLOR": "1",
            "TERM": "xterm-256color",
        })
        samples = [sample(binary, environment, directory) for _ in range(SAMPLES)]
    print(json.dumps({
        "samples_ms": [round(value, 2) for value in samples],
        "median_ms": round(statistics.median(samples), 2),
    }))


main()
