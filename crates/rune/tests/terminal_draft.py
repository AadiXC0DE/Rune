"""Edit drafts, insert newlines, and undo and redo edits in the real process in a PTY."""

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
editor_open = threading.Event()
stream_sent = threading.Event()
editor_done = threading.Event()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "draft-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            prompts.append(request["messages"][-1]["content"])
            if editor_mode == "editor-steering" and len(prompts) == 1:
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()

                def delta(text, finish=None):
                    choice = {"index": 0, "delta": {"content": text}, "finish_reason": finish}
                    self.wfile.write(("data: " + json.dumps({"choices": [choice]}) + "\n\n").encode())
                    self.wfile.flush()

                delta("EDITOR_TURN_STARTED")
                assert editor_open.wait(10), "editor did not open"
                delta("EDITOR_STREAM_HIDDEN")
                stream_sent.set()
                assert editor_done.wait(10), "editor did not return"
                delta("DRAFT_RECEIVED")
                delta("", "stop")
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                return
            body = "".join(
                "data: " + json.dumps({"choices": [choice]}) + "\n\n"
                for choice in [
                    {"index": 0, "delta": {"content": "DRAFT_RECEIVED"},
                     "finish_reason": None},
                    {"index": 0, "delta": {}, "finish_reason": "stop"},
                ]
            ) + "data: [DONE]\n\n"
            body = body.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except Exception as error:
            errors.append(repr(error))
            self.send_error(500)


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="rune-r004-") as directory:
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
    editor_mode = sys.argv[2] if sys.argv[2:] and sys.argv[2].startswith("editor") else None
    if editor_mode:
        stub = root / "stub editor.py"
        stub.write_text('''import pathlib, stat, sys, termios, time
mode, path = sys.argv[1], pathlib.Path(sys.argv[2])
flags = termios.tcgetattr(0)[3]
assert flags & termios.ICANON and flags & termios.ECHO, flags
assert stat.S_IMODE(path.stat().st_mode) == 0o600
assert stat.S_IMODE(path.parent.stat().st_mode) == 0o700
assert path.read_text() == "draft 界"
pathlib.Path("editor-path").write_text(str(path))
if mode == "editor-steering":
    deadline = time.monotonic() + 10
    while not pathlib.Path("editor-exit").exists():
        assert time.monotonic() < deadline, "editor was not released"
        time.sleep(0.01)
if mode == "editor-invalid":
    path.write_bytes(b"\\xff")
elif mode != "editor-unchanged":
    path.write_text("edited é\\nsecond 界")
sys.exit(9 if mode == "editor-failed" else 0)
''')
        command = f"{shlex.quote(sys.executable)} {shlex.quote(str(stub))} {editor_mode}"
        environment["VISUAL"] = command
        # VISUAL must take precedence over EDITOR. Empty VISUAL falls back.
        environment["EDITOR"] = "exit 99"
        if editor_mode == "editor-fallback":
            environment["VISUAL"] = ""
            environment["EDITOR"] = command
        if editor_mode == "editor-missing":
            environment["VISUAL"] = "rune-editor-that-does-not-exist"
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
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
            readable, _, _ = select.select([master], [], [], 0.05)
            if readable:
                transcript.extend(os.read(master, 65536))
        assert not errors, errors

    def capture(stage, keys, expected):
        start = len(transcript)
        os.write(master, keys)
        if editor_mode == "editor-steering" and stage == "returned":
            deadline = time.monotonic() + 10
            while not (root / "editor-path").exists():
                assert time.monotonic() < deadline, "editor did not start"
                time.sleep(0.01)
            editor_open.set()
            assert stream_sent.wait(10), "provider did not stream while editing"
            while select.select([master], [], [], 0.2)[0]:
                assert time.monotonic() < deadline, "terminal never became idle"
                transcript.extend(os.read(master, 65536))
            assert b"EDITOR_STREAM_HIDDEN" not in transcript[start:], transcript[start:]
            (root / "editor-exit").touch()
        wait_for(expected, start)
        # Drain complete frames before handing the byte stream to Rune's grid.
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.1)[0]:
            assert time.monotonic() < deadline, "terminal never finished drawing"
            transcript.extend(os.read(master, 65536))
        captures.append((stage, list(transcript)))

    try:
        wait_for("ctrl-c cancel")
        if editor_mode:
            if editor_mode == "editor-steering":
                os.write(master, b"start\r")
                wait_for("EDITOR_TURN_STARTED")
            capture("original", "draft 界".encode() + b"\x1b[D", "draft 界")
            changed = editor_mode in {"editor", "editor-fallback", "editor-steering"}
            capture("returned", b"\x07", "second 界" if changed else "\x1b[?1049l")
            flags = termios.tcgetattr(master)[3]
            assert not flags & termios.ICANON and not flags & termios.ECHO, flags
            restored = bytes(transcript)
            assert b"\x1b[?2004l" in restored
            assert restored.rfind(b"\x1b[?2004h") > restored.rfind(b"\x1b[?2004l")
            if editor_mode != "editor-missing":
                scratch = pathlib.Path((root / "editor-path").read_text())
                assert not scratch.exists() and not scratch.parent.exists(), scratch
            if editor_mode in {"editor-failed", "editor-invalid", "editor-missing"}:
                assert b"external editor failed:" in restored, restored
            assert prompts == (["start"] if editor_mode == "editor-steering" else []), prompts
            if editor_mode == "editor-steering":
                editor_done.set()
                wait_for("DRAFT_RECEIVED\r\n")
            if changed:
                capture("undone", b"\x1f", "draft 界")
                capture("redone", b"\x1br", "second 界")
                capture("typed", b"!", "second 界!")
                submitted = "edited é\nsecond 界!"
            else:
                capture("typed", b"!", "draft !界")
                submitted = "draft !界"
        elif sys.argv[2:] == ["newline"]:
            capture("first-line", "first 界 line".encode(), "first 界 line")
            # Alt-Enter is ESC followed by CR on a legacy terminal. It must
            # insert one LF without sending an early provider request.
            capture("newline", b"\x1b\r", "\x1b[3G")
            assert not prompts, prompts
            capture("second-line", "second é line".encode(), "second é line")
            assert not prompts, prompts
            submitted = "first 界 line\nsecond é line"
        elif sys.argv[2:] == ["multiline"]:
            draft = "first 界\nsecond e\u0301\nthird 👩‍💻"
            capture("pasted", b"\x1b[200~" + draft.encode() + b"\x1b[201~", "third 👩‍💻")
            assert not prompts, prompts
            capture("third-edited", b"Z", "third 👩‍💻Z")
            capture("second-edited", b"\x1b[D" * 9 + b"X", "second e\u0301X")
            capture("first-edited", b"\x1b[D" * 10 + b"Y", "first 界Y")
            capture("end-again", b"\x05", "\x1b[12G")
            submitted = "first 界Y\nsecond e\u0301X\nthird 👩‍💻Z"
        elif sys.argv[2:] == ["redo"]:
            capture("original", "界ab".encode() + b"\x1b[D", "界ab")
            capture(
                "inserted", b"\x1b[200~" + "e\u0301".encode() + b"\x1b[201~",
                "界ae\u0301b",
            )
            capture("undone", b"\x1f", "界ab")
            capture("moved", b"\x01", "\x1b[3G")
            capture("redone", b"\x1br", "界ae\u0301b")
            submitted = "界ae\u0301b"
        elif sys.argv[2:] == ["undo"]:
            capture("original", "界ab".encode() + b"\x1b[D", "界ab")
            capture(
                "inserted", b"\x1b[200~" + "e\u0301".encode() + b"\x1b[201~",
                "界ae\u0301b",
            )
            capture("deleted", b"\x7f", "界ab")
            # Navigation must not change the caret restored by undo.
            capture("moved", b"\x01", "\x1b[3G")
            capture("undo-delete", b"\x1f", "界ae\u0301b")
            capture("undo-insert", b"\x1f", "界ab")
            submitted = "界ab"
        else:
            draft = "a" * 160 + "TAIL-END"
            capture("end", draft.encode(), "TAIL-END")
            capture("left", b"\x1b[D", "TAIL-END")
            capture("edited", b"\x1b[3~Z", "TAIL-ENZ")
            capture("home", b"\x01", "> " + "a" * 78)
            capture("end-again", b"\x05", "TAIL-ENZ")
            submitted = "a" * 160 + "TAIL-ENZ"
        os.write(master, b"\r")
        # Wait for close_turn's committed line, not its earlier streamed row,
        # so /quit is handled as a command rather than steering the old turn.
        wait_for("DRAFT_RECEIVED\r\n", len(transcript) if editor_mode == "editor-steering" else 0)
        assert prompts == (["start", submitted] if editor_mode == "editor-steering" else [submitted]), prompts
        os.write(master, b"/quit\r")
        assert child.wait(timeout=10) == 0
        assert not errors, errors
        print(json.dumps(captures))
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
