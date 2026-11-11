"""Newline keys and the keyboard protocol through the real binary in a PTY."""

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


QUERY = b"\x1b[c"
KITTY_REPLIES = b"\x1b[?1u\x1b[?62;22c"
PLAIN_REPLIES = b"\x1b[?62;22c"
prompts = []
errors = []


def event(text=None, finish=None):
    return ("data: " + json.dumps({"choices": [{
        "index": 0, "delta": {} if text is None else {"content": text},
        "finish_reason": finish,
    }]}) + "\n\n").encode()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "newline-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            prompts.append(request["messages"][-1]["content"])
            body = event("NEWLINE_RECEIVED") + event(finish="stop") + b"data: [DONE]\n\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            errors.append(repr(error))


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def session(environment, root, replies):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    child = subprocess.Popen(
        [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
        cwd=root, env=environment, preexec_fn=controlling_terminal,
    )
    os.close(slave)
    transcript = bytearray()

    def drain():
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.15)[0]:
            assert time.monotonic() < deadline, "terminal never became idle"
            try:
                data = os.read(master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    return
                raise
            if not data:
                return
            transcript.extend(data)

    def wait_for(text, start=0):
        deadline = time.monotonic() + 10
        while text not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript[-4000:].decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited early"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))

    def keys(data):
        os.write(master, data)
        time.sleep(0.06)
        drain()

    return master, child, transcript, wait_for, drain, keys


with tempfile.TemporaryDirectory(prefix="rune-newline-") as directory:
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
        if not key.startswith("RUNE_") and key not in {
            "TMUX", "STY", "SSH_TTY", "SSH_CONNECTION", "COLORTERM", "TERM_PROGRAM",
        } and key.lower() not in {"http_proxy", "https_proxy", "all_proxy"}
    })
    environment.update({
        "HOME": directory,
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_STATE_HOME": str(root / "xdg-state"),
        "RUNE_HOME": str(root / "state"),
        "RUNE_PROVIDER": "chat_completions",
        "RUNE_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
        "RUNE_MODEL": "newline-fixture",
        "RUNE_API_KEY_ENV": "RUNE_NEWLINE_TEST_KEY",
        "RUNE_NEWLINE_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    scenario = sys.argv[2]
    if scenario == "tmux":
        environment["TMUX"] = "/tmp/x,1,0"
        replies, push, pop = PLAIN_REPLIES, b"\x1b[>4;1m", b"\x1b[>4m"
    elif scenario == "setup":
        environment["TERM_PROGRAM"] = "zed"
        replies, push, pop = PLAIN_REPLIES, None, None
    else:
        replies, push, pop = KITTY_REPLIES, b"\x1b[>1u", b"\x1b[<u"
    master, child, transcript, wait_for, drain, keys = session(environment, root, replies)
    result = {}
    try:
        wait_for(QUERY)
        os.write(master, replies)
        wait_for(b"/ commands")
        drain()
        hint = transcript.index(b"/ commands")
        if push is None:
            for sequence in [b"\x1b[>1u", b"\x1b[>4;", b"\x1b[<u", b"\x1b[>4m"]:
                assert sequence not in transcript, transcript
        else:
            assert transcript.find(push) != -1, transcript
            assert transcript.find(push) < hint, transcript
            assert transcript.count(push) == 1, transcript
            assert pop not in transcript, transcript
        if scenario == "setup":
            keymap = root / ".config" / "zed" / "keymap.json"
            start = len(transcript)
            keys(b"/terminal-setup\r")
            wait_for(b"y yes   n no", start)
            drain()
            assert not keymap.exists()
            keys(b"x")
            assert not keymap.exists()
            keys(b"y")
            wait_for(b"restart the terminal or reload its settings", start)
            drain()
            written = json.loads(keymap.read_text())
            assert written[0]["bindings"]["shift-enter"][1] == "\x1b[13;2u", written
            start = len(transcript)
            keys(b"/terminal-setup\r")
            wait_for(b"already configured in", start)
            drain()
        if scenario == "kitty":
            for data in [b"a", b"\x1b[13;2u", b"b", b"\n", b"c", b"\x1b\r", b"d",
                         b"\\", b"\r", b"e"]:
                keys(data)
            assert prompts == [], prompts
            start = len(transcript)
            keys(b"\r")
            wait_for(b"NEWLINE_RECEIVED", start)
            drain()
            result["prompts"] = list(prompts)
        start = len(transcript)
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 10, transcript) == 0, transcript.decode(errors="replace")
        if pop is not None:
            assert pop in transcript[start:], transcript[start:]
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
