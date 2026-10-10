"""R-073: discover bindings through /help, then exercise them in a real PTY."""

import errno
import fcntl
import http.server
import json
import os
import pathlib
import pty
import select
import shlex
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


prompts = []
errors = []
releases = {name: threading.Event() for name in ["slow-c", "slow-esc"]}


def event(text=None, finish=None):
    return ("data: " + json.dumps({"choices": [{
        "index": 0, "delta": {} if text is None else {"content": text},
        "finish_reason": finish,
    }]}) + "\n\n").encode()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "draft-fixture"}, {"id": "second-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            prompt = request["messages"][-1]["content"]
            prompts.append(prompt)
            slow = prompt.startswith("slow")
            reply = "HELP_TURN_RUNNING" if slow else "HELP_RECEIVED"
            if prompt == "edited":
                reply = "".join(f"ROW-{row:02}\n" for row in range(60)) + reply
            body = event(reply)
            tail = event(finish="stop") + b"data: [DONE]\n\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body) + len(tail)))
            self.end_headers()
            self.wfile.write(body)
            self.wfile.flush()
            if slow:
                assert releases[prompt].wait(15), "slow turn was never released"
            self.wfile.write(tail)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            errors.append(repr(error))


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="rune-r073-") as directory:
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
        "RUNE_MODEL": "draft-fixture",
        "RUNE_API_KEY_ENV": "RUNE_DRAFT_TEST_KEY",
        "RUNE_DRAFT_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    stub = root / "editor.py"
    stub.write_text("import pathlib, sys\np = pathlib.Path(sys.argv[1])\n"
                    "assert p.read_text() == 'draft'\np.write_text('edited')\n")
    environment["VISUAL"] = f"{shlex.quote(sys.executable)} {shlex.quote(str(stub))}"
    for name in ["fixture a.txt", "fixture b.txt"]:
        (root / name).write_text("fixture")
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    child = subprocess.Popen(
        [sys.argv[1], *sys.argv[2:]], stdin=slave, stdout=slave, stderr=slave,
        cwd=root, env=environment, preexec_fn=controlling_terminal,
    )
    os.close(slave)
    transcript = bytearray()
    captures = []

    def drain():
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.15)[0]:
            assert time.monotonic() < deadline, "terminal never became idle"
            try:
                transcript.extend(os.read(master, 65536))
            except OSError as error:
                if error.errno == errno.EIO:
                    return  # The child closed its PTY on a requested exit.
                raise

    def wait_for(text, start=0):
        deadline = time.monotonic() + 10
        while text.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript[-6000:].decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited early"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
        drain()
        assert not errors, errors

    def keys(data):
        os.write(master, data)
        drain()

    def draft(label, data, text, column):
        keys(data)
        captures.append([label, list(transcript), "> " + text, column])

    def submit(text):
        start = len(transcript)
        keys(b"\r")
        wait_for("HELP_RECEIVED\r\n", start)
        assert prompts[-1] == text, prompts

    def viewer(label, data):
        keys(data)
        captures.append(["viewer-" + label, list(transcript), "", 0])

    try:
        if sys.argv[2:] == ["--accessible"]:
            wait_for("accessible mode:")
            keys(b"/help\n")
            wait_for("Input: terminal line mode")
            assert b"Keyboard bindings (interactive key input)" not in transcript
            assert b"Enter submits one line; end of input leaves the session." in transcript
            keys(b"/quit\n")
            assert child.wait(timeout=10) == 0
            print(json.dumps(captures))
            sys.exit(0)

        wait_for("ctrl-c cancel")
        keys(b"/help\r")
        wait_for("Terminals without bracketed paste send ordinary keys.")
        # Verify the complete help through the output emitted by /help itself.
        help_output = transcript.decode(errors="replace")
        for binding in ["Enter", "Alt-Enter", "Left / Right", "Home / Ctrl-A",
                        "End / Ctrl-E", "Ctrl-B / Alt-Left", "Ctrl-F / Alt-Right",
                        "Backspace", "Delete", "Ctrl-D", "Ctrl-W", "Ctrl-U",
                        "Ctrl-K", "Ctrl-Y", "Ctrl-_", "Ctrl-7", "Alt-R", "Ctrl-G",
                        "Up / Down", "Tab", "Ctrl-R", "Esc / Ctrl-C", "Ctrl-O",
                        "Permission and question menus:",
                        "PageUp / PageDown", "Home / End", "Bracketed paste"]:
            assert binding in help_output, binding
        assert "within 1000 ms" in help_output
        assert "clear a nonempty draft first" in help_output

        # Each alternative spelling gets its own caret assertion.
        draft("typed", "one 界e\u0301 two".encode(), "one 界e\u0301 two", 13)
        draft("left", b"\x1b[D", "one 界e\u0301 two", 12)
        draft("right", b"\x1b[C", "one 界e\u0301 two", 13)
        draft("home", b"\x1b[H", "one 界e\u0301 two", 2)
        draft("end", b"\x1b[F", "one 界e\u0301 two", 13)
        draft("ctrl-a", b"\x01", "one 界e\u0301 two", 2)
        draft("ctrl-e", b"\x05", "one 界e\u0301 two", 13)
        draft("ctrl-b", b"\x02", "one 界e\u0301 two", 10)
        draft("ctrl-f", b"\x06", "one 界e\u0301 two", 13)
        draft("alt-left", b"\x1b[1;3D", "one 界e\u0301 two", 10)
        draft("alt-right", b"\x1b[1;3C", "one 界e\u0301 two", 13)
        draft("ctrl-w", b"\x17", "one 界e\u0301 ", 10)
        draft("undo-word", b"\x1f", "one 界e\u0301 two", 13)
        draft("ctrl-u", b"\x15", "", 2)
        draft("ctrl-y-start", b"\x19", "one 界e\u0301 two", 13)
        draft("ctrl-k", b"\x02\x0b", "one 界e\u0301 ", 10)
        draft("ctrl-y-end", b"\x19", "one 界e\u0301 two", 13)
        draft("backspace-grapheme", b"\x02\x7f\x7f", "one 界two", 8)
        draft("delete", b"\x1b[3~", "one 界wo", 8)
        draft("ctrl-d-delete", b"\x04", "one 界o", 8)
        draft("undo", b"\x1f", "one 界wo", 8)
        draft("redo", b"\x1br", "one 界o", 8)
        draft("escape-idle", b"\x1b", "", 2)
        draft("ctrl-c-draft", b"discard\x03", "", 2)
        draft("external-editor", b"draft\x07", "edited", 8)
        submit("edited")

        draft("history-up", b"unfinished\x1b[A", "edited", 8)
        draft("history-down", b"\x1b[B", "unfinished", 12)
        draft("history-cancel-esc", b"\x12edited\x1b", "unfinished", 12)
        draft("history-cancel-ctrl-c", b"\x12edited\x03", "unfinished", 12)
        draft("history-cancel-ctrl-d", b"\x12\x04", "unfinished", 12)
        draft("history-tab", b"\x12edited\t", "edited", 8)
        draft("history-enter", b"\x12edited\r", "edited", 8)
        assert prompts == ["edited"], prompts
        draft("clear", b"\x1b", "", 2)
        draft("command-tab", b"/mo\x1b[B\x1b[A\t", "/model", 8)
        keys(b"\x1b")
        draft("command-enter", b"/mod\r", "/model", 8)
        keys(b"\r")
        wait_for("second-fixture")
        draft("model-cancel-esc", b"\x1b", "", 2)
        keys(b"/model\r")
        wait_for("second-fixture", len(transcript) - 3000)
        draft("model-cancel-ctrl-c", b"\x03", "", 2)
        keys(b"/model\r")
        draft("model-cancel-ctrl-d", b"\x04", "", 2)
        keys(b"/model\r")
        keys(b"\x1b[B\x1b[A\t")
        wait_for("model set to draft-fixture")
        keys(b"/model\r")
        keys(b"\x1b[B\x1b[A\r")
        wait_for("model set to draft-fixture")

        draft("path-open", b"read ./fixture\t", "read ./fixture", 16)
        draft("path-escape", b"\x1b", "read ./fixture", 16)
        draft("path-tab", b"\t\x1b[B\x1b[A\t", "read './fixture a.txt'", 24)
        keys(b"\x1b")
        draft("path-enter", b"read ./fixture\t\x1b[B\r", "read './fixture b.txt'", 24)
        assert prompts == ["edited"], prompts
        keys(b"\x1b")
        draft("before-viewer", b"draft\x1b[D", "draft", 6)
        viewer("open", b"\x0f")
        viewer("down", b"\x1b[B")
        viewer("up", b"\x1b[A")
        viewer("page-down", b"\x1b[6~")
        viewer("page-up", b"\x1b[5~")
        viewer("end", b"\x1b[F")
        viewer("home", b"\x1b[H")
        for label, close in [("escape", b"\x1b"), ("ctrl-o", b"\x0f"),
                             ("ctrl-c", b"\x03"), ("ctrl-d", b"\x04")]:
            if label != "escape":
                keys(b"\x0f")
            draft("viewer-close-" + label, close, "draft", 6)

        # Bracketed paste inserts at the caret, preserves tabs/newlines, strips
        # control bytes, and must not submit or trigger completion.
        paste = b"\x1b[200~A\r\nB\tC\rD\x03\x07\x1b[201~"
        keys(b"\x1b")
        keys(b"xy\x1b[D" + paste)
        captures.append(["paste", list(transcript), "  Dy", 3])
        assert prompts == ["edited"], prompts
        submit("xA\nB\tC\nDy")
        keys(b"first\x1b\rsecond")
        captures.append(["newline", list(transcript), "  second", 8])
        assert len(prompts) == 2, prompts
        submit("first\nsecond")

        # A held SSE turn makes cancellation ordering observable.
        keys(b"slow-c\r")
        wait_for("HELP_TURN_RUNNING")
        draft("running-ctrl-c-clear", b"correction\x03", "", 2)
        keys(b"\x04")  # Empty Ctrl-D must not stop or leave a running turn.
        assert b"[cancelled]" not in transcript and child.poll() is None
        keys(b"\x0f")  # Transcript also opens while the turn runs.
        wait_for("Transcript", len(transcript) - 1000)
        keys(b"\x03")  # Close takes precedence over cancellation.
        assert b"[cancelled]" not in transcript
        start = len(transcript)
        keys(b"steer\r")
        wait_for("steering queued for the next boundary", start)
        keys(b"\x03")
        wait_for("[cancelled]", start)
        releases["slow-c"].set()
        keys(b"\x15")  # Cancellation restores unsent steering as a draft.
        start = len(transcript)
        keys(b"slow-esc\r")
        wait_for("HELP_TURN_RUNNING", start)
        draft("running-esc-clear", b"correction\x1b", "", 2)
        assert b"[cancelled]" not in transcript[start:]
        keys(b"X")  # An intervening edit disarms the first Escape.
        keys(b"\x1b")
        assert b"[cancelled]" not in transcript[start:]
        keys(b"\x1b")
        wait_for("[cancelled]", start)
        releases["slow-esc"].set()

        # Idle Escape stays, idle Ctrl-D leaves. A second process proves Ctrl-C.
        draft("idle-escape-empty", b"\x1b", "", 2)
        os.write(master, b"\x04")
        assert child.wait(timeout=10) == 0
        os.close(master)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        child = subprocess.Popen(
            [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
            cwd=root, env=environment, preexec_fn=controlling_terminal,
        )
        os.close(slave)
        transcript = bytearray()
        wait_for("ctrl-c cancel")
        os.write(master, b"\x03")
        assert child.wait(timeout=10) == 0
        assert not errors, errors
        print(json.dumps(captures))
    finally:
        for release in releases.values():
            release.set()
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
