"""Stream many small deltas through a PTY and record the frames written."""

import errno
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


scenario = sys.argv[2]
DELTAS = 50 if scenario == "dumb" else 2000
errors = []
stream_started = threading.Event()
started_at = []


def event(text=None, finish=None):
    return ("data: " + json.dumps({"choices": [{
        "index": 0, "delta": {} if text is None else {"content": text},
        "finish_reason": finish,
    }]}) + "\n\n").encode()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            self.rfile.read(int(self.headers["Content-Length"]))
            chunks = [event("x ") for _ in range(DELTAS)]
            chunks.append(event("PACING-END"))
            chunks.append(event(finish="stop") + b"data: [DONE]\n\n")
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(sum(len(chunk) for chunk in chunks)))
            self.end_headers()
            started_at.append(time.monotonic())
            stream_started.set()
            for chunk in chunks:
                self.wfile.write(chunk)
                self.wfile.flush()
                time.sleep(0.0005)
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            errors.append(repr(error))


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="rune-pacing-") as directory:
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
        "RUNE_MODEL": "pacing-fixture",
        "RUNE_API_KEY_ENV": "RUNE_PACING_TEST_KEY",
        "RUNE_PACING_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "dumb" if scenario == "dumb" else "xterm-256color",
    })
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    child = subprocess.Popen(
        [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
        cwd=root, env=environment, preexec_fn=controlling_terminal,
    )
    os.close(slave)
    transcript = bytearray()

    def pump(timeout):
        if select.select([master], [], [], timeout)[0]:
            try:
                data = os.read(master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    return
                raise
            transcript.extend(data)

    def wait_for(text, start=0, timeout=20):
        deadline = time.monotonic() + timeout
        while text not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript[-4000:].decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited early"
            pump(0.005)
        assert not errors, errors
        return transcript.index(text, start)

    try:
        wait_for(b"ctrl-c cance")
        os.write(master, b"pace")
        wait_for(b"> pace")
        start = len(transcript)
        written = time.monotonic()
        os.write(master, b"\r")
        result = {}
        if scenario == "key":
            while not stream_started.is_set():
                assert time.monotonic() < written + 20, "the stream never began"
                pump(0.005)
            typed_at = started_at[0] + 0.3
            while time.monotonic() < typed_at:
                pump(0.002)
            key_start = len(transcript)
            os.write(master, b"Z")
            key_written = time.monotonic()
            wait_for(b"> Z", key_start)
            result["key_ms"] = (time.monotonic() - key_written) * 1000
        # Every delta is journaled before it is drawn, so a slow disk sets the
        # pace of the stream rather than the renderer.
        end = wait_for(b"PACING-END\r\n", start, timeout=150)
        result["elapsed_ms"] = (time.monotonic() - written) * 1000
        if scenario == "dumb":
            wait_for(b"ctrl-c cance", end)
            time.sleep(0.2)
            pump(0.05)
            result["bytes"] = list(transcript)
        else:
            close = wait_for(b"\x1b[?2026l", end)
            result["bytes"] = list(transcript[start:close + len(b"\x1b[?2026l")])
        os.write(master, b"\x15\x04")
        assert drain_wait(child, master, 10, transcript) == 0
        assert not errors, errors
        print(json.dumps(result))
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
