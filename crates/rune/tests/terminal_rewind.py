"""Double Escape rewinds to before an earlier message; editing it resends."""

import fcntl
import http.server
import json
import os
import pathlib
import pty
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


requests = []
errors = []
scenario = sys.argv[2]
assert scenario in {"edit", "files"}, scenario


def chunk(delta, finish=None):
    return ("data: " + json.dumps({"choices": [{
        "index": 0, "delta": delta, "finish_reason": finish,
    }]}) + "\n\n").encode()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "rewind-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request)
            users = [message for message in request["messages"] if message["role"] == "user"]
            last = request["messages"][-1]
            if last["role"] == "user" and last.get("content") == "make x":
                call = {"index": 0, "id": "call_make", "type": "function", "function": {
                    "name": "shell",
                    "arguments": json.dumps({"action": "run", "command": "echo x > x.txt"}),
                }}
                body = (chunk({"tool_calls": [call]}) + chunk({}, "tool_calls")
                        + b"data: [DONE]\n\n")
            elif last["role"] == "tool":
                body = chunk({"content": "Made."}) + chunk({}, "stop") + b"data: [DONE]\n\n"
            else:
                body = (chunk({"content": f"A{len(users)}"}) + chunk({}, "stop")
                        + b"data: [DONE]\n\n")
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


def conversation(request):
    """The user and assistant texts of a request, in order."""
    return [
        (message["role"], message.get("content"))
        for message in request["messages"]
        if message["role"] in {"user", "assistant"}
    ]


with tempfile.TemporaryDirectory(prefix="rune-rewind-") as directory:
    root = pathlib.Path(directory)
    workspace = root / "work"
    workspace.mkdir()
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    if scenario == "files":
        git_environment = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null")
        subprocess.run(["git", "init", "-q"], cwd=workspace, env=git_environment, check=True)
        (workspace / "a.txt").write_text("a\n")
        config_file.write_text(
            "auto_upgrade = false\npermission_mode = \"full_access\"\nallow_unsandboxed = true\n"
        )
    else:
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
        "RUNE_MODEL": "rewind-fixture",
        "RUNE_API_KEY_ENV": "RUNE_REWIND_TEST_KEY",
        "RUNE_REWIND_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    try:
        child = subprocess.Popen(
            [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
            cwd=workspace, env=environment, preexec_fn=controlling_terminal,
        )
    finally:
        os.close(slave)
    transcript = bytearray()

    def wait_for(text, start=0, timeout=15):
        deadline = time.monotonic() + timeout
        while text.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
        assert not errors, errors

    def settle():
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            if not select.select([master], [], [], 0.2)[0]:
                return
            transcript.extend(os.read(master, 65536))

    def send(text, reply):
        begin = len(transcript)
        count = len(requests)
        os.write(master, text.encode() + b"\r")
        wait_for(reply, begin)
        deadline = time.monotonic() + 10
        while len(requests) == count:
            assert time.monotonic() < deadline, "no request"
            time.sleep(0.02)
        settle()

    def rewind(begin):
        os.write(master, b"\x1b")
        time.sleep(0.06)
        os.write(master, b"\x1b")
        wait_for("rewind to before which message?", begin)
        settle()

    def files_scenario(original):
        made = workspace / "x.txt"
        send("one", "A1")
        send("make x", "Made.")
        assert made.read_text() == "x\n"
        begin = len(transcript)
        rewind(begin)
        begin = len(transcript)
        os.write(master, b"\r")
        wait_for("fork here (conversation and files)", begin)
        settle()
        for _ in range(3):
            os.write(master, b"\x1b[B")
            time.sleep(0.05)
        settle()
        begin = len(transcript)
        os.write(master, b"\r")
        wait_for("restore these files to before that message?", begin)
        settle()
        preview = transcript[begin:].decode(errors="replace")
        assert "restoring to before turn 2 would change" in preview, preview
        assert "A x.txt" in preview, preview
        assert made.exists(), "a preview changes nothing"
        begin = len(transcript)
        os.write(master, b"y")
        wait_for("forked into session", begin)
        wait_for("restored the workspace to before turn 2", 0)
        settle()
        assert not made.exists(), "the file went back"
        assert (workspace / "a.txt").read_text() == "a\n"
        send("next", "A2")
        sent = conversation(requests[-1])
        # The model hears about the restore with the next message.
        assert sent == [
            ("user", "one"), ("assistant", "A1"),
            ("user", "next\n\n<context-update>\n"
                     "Files were restored to their state before turn 2.\n</context-update>"),
        ], sent
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0

    try:
        wait_for("/ commands")
        found = re.search(rb"session ([A-Za-z0-9_-]{8,})", bytes(transcript))
        assert found, transcript.decode(errors="replace")
        original = found.group(1).decode()
        settle()
        if scenario == "files":
            files_scenario(original)
            sys.exit(0)
        send("one", "A1")
        send("two", "A2")
        send("three", "A3")
        assert conversation(requests[-1]) == [
            ("user", "one"), ("assistant", "A1"),
            ("user", "two"), ("assistant", "A2"),
            ("user", "three"),
        ], requests[-1]

        begin = len(transcript)
        os.write(master, b"\x1b")
        time.sleep(0.06)
        os.write(master, b"\x1b")
        wait_for("rewind to before which message?", begin)
        settle()
        listed = transcript[begin:].decode(errors="replace")
        assert listed.find("three") < listed.find("two") < listed.find("one"), listed
        os.write(master, b"\x1b[B")
        settle()
        begin = len(transcript)
        os.write(master, b"\r")
        wait_for("edit and resend", begin)
        settle()
        begin = len(transcript)
        os.write(master, b"\r")
        wait_for("forked into session", begin)
        wait_for(f"/resume {original} returns", begin)
        wait_for("> two", begin)
        settle()
        count = len(requests)
        send(" fixed", "A2")
        assert len(requests) == count + 1
        assert conversation(requests[-1]) == [
            ("user", "one"), ("assistant", "A1"),
            ("user", "two fixed"),
        ], requests[-1]

        begin = len(transcript)
        os.write(master, f"/resume {original}\r".encode())
        wait_for(f"resumed session {original}", begin)
        settle()
        send("four", "A4")
        assert conversation(requests[-1]) == [
            ("user", "one"), ("assistant", "A1"),
            ("user", "two"), ("assistant", "A2"),
            ("user", "three"), ("assistant", "A3"),
            ("user", "four"),
        ], requests[-1]

        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 15, transcript) == 0
        os.close(master)
    finally:
        if child.poll() is None:
            child.kill()
            drain_wait(child, master, 10, bytearray())
