"""R-070: canonical input, append-only output, tool input, and saved replay."""

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
requests = []
tool_results = []
first_delta = threading.Event()
release_reply = threading.Event()
LONG_PROMPT = "W" * 300 + "-PROMPT-END"


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "accessible-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            last = request["messages"][-1]
            slow = False
            if last["role"] == "tool":
                tool_results.append(last["content"])
                deltas = [{"content": "TOOL_REPLY"}]
                finish = "stop"
            else:
                prompt = last["content"]
                requests.append(prompt)
                if prompt == "fail":
                    self.send_error(400, "fixture failure")
                    return
                if prompt == "question":
                    name = "ask_user_question"
                    arguments = {"questions": [{
                        "question": "Which option should be used?",
                        "options": [
                            {"label": "Alpha", "description": "first choice"},
                            {"label": "Beta", "description": "second choice"},
                        ],
                    }]}
                elif prompt == "permission":
                    name = "shell"
                    arguments = {
                        "action": "run",
                        "command": "printf ACCESSIBLE_SHELL_OK > approved.txt",
                    }
                else:
                    name = None
                if name:
                    deltas = [{"tool_calls": [{
                        "index": 0,
                        "id": f"call-{len(requests)}",
                        "type": "function",
                        "function": {"name": name, "arguments": json.dumps(arguments)},
                    }]}]
                    finish = "tool_calls"
                elif prompt == LONG_PROMPT:
                    slow = True
                    deltas = [
                        {"reasoning_content": "THINKING"},
                        {"content": "FIRST-"},
                        {"content": "REPLY\n  preserved\n\x1b[2JSAFE-END\r\x08"},
                    ]
                    finish = "stop"
                else:
                    deltas = [{"content": "SESSION_STILL_USABLE"}]
                    finish = "stop"
            chunks = [
                ("data: " + json.dumps({"choices": [{
                    "index": 0, "delta": delta, "finish_reason": None,
                }]}) + "\n\n").encode()
                for delta in deltas
            ]
            chunks.append(("data: " + json.dumps({"choices": [{
                "index": 0, "delta": {}, "finish_reason": finish,
            }]}) + "\n\ndata: [DONE]\n\n").encode())
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(sum(map(len, chunks))))
            self.end_headers()
            for index, chunk in enumerate(chunks):
                self.wfile.write(chunk)
                self.wfile.flush()
                if slow and index == 1:
                    first_delta.set()
                    assert release_reply.wait(10), "stream was not released"
        except Exception as error:
            errors.append(repr(error))
            self.send_error(500)


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


class Terminal:
    def __init__(self, binary, root, environment, extra=()):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 8, 12, 0, 0))
        self.child = subprocess.Popen(
            [binary, "--accessible", "--permission-mode", "ask",
             "--allow-unsandboxed", *extra],
            stdin=slave, stdout=slave, stderr=slave, cwd=root, env=environment,
            preexec_fn=controlling_terminal,
        )
        os.close(slave)
        self.output = bytearray()

    def drain(self, duration=0.15):
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.master], [], [], 0.02)
            if ready:
                try:
                    self.output.extend(os.read(self.master, 65536))
                except OSError:
                    break

    def wait_for(self, text, start=0):
        deadline = time.monotonic() + 12
        while text.encode() not in self.output[start:]:
            assert time.monotonic() < deadline, (
                f"waiting for {text!r}:\n" + self.output.decode(errors="replace")
            )
            assert self.child.poll() is None, self.output.decode(errors="replace")
            self.drain(0.03)
        assert not errors, errors

    def send(self, line):
        os.write(self.master, line.encode() + b"\n")

    def finish(self):
        self.send("/exit")
        assert self.child.wait(timeout=5) == 0
        self.drain()
        # CRLF is terminal newline translation, not an application cursor move.
        output = bytes(self.output).replace(b"\r\n", b"\n")
        assert not any(byte < 32 and byte not in (9, 10) for byte in output), repr(output)
        assert b"ctrl-c cancel" not in output, "interactive footer leaked"
        assert b"ctx " not in output, "context status row leaked"
        return output

    def close(self):
        if self.child.poll() is None:
            self.child.kill()
        self.child.wait(timeout=5)
        os.close(self.master)


with tempfile.TemporaryDirectory(prefix="rune-r070-") as directory:
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
            "http_proxy", "https_proxy", "all_proxy", "no_color",
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
        "RUNE_MODEL": "accessible-fixture",
        "RUNE_API_KEY_ENV": "RUNE_ACCESSIBLE_TEST_KEY",
        "RUNE_ACCESSIBLE_TEST_KEY": "fixture-key",
        "TERM": "xterm-256color",
        "COLORTERM": "truecolor",
    })
    terminal = Terminal(sys.argv[1], root, environment)
    try:
        terminal.wait_for("Input: enter a prompt")
        flags = termios.tcgetattr(terminal.master)[3]
        assert flags & termios.ICANON and flags & termios.ECHO, "raw mode enabled"
        opening = bytes(terminal.output)
        terminal.drain(0.3)
        assert bytes(terminal.output) == opening, "idle status was rewritten"
        terminal.send(LONG_PROMPT)
        assert first_delta.wait(10), "provider did not stream"
        terminal.drain()
        assert b"Assistant: FIRST-" not in terminal.output, "partial reply printed"
        release_reply.set()
        terminal.wait_for("Assistant: SAFE-END")
        assert LONG_PROMPT in requests, "long prompt did not reach provider exactly"
        assert b"User: " + LONG_PROMPT.encode() in terminal.output, "prompt truncated"
        assert terminal.output.count(b"Assistant: FIRST-REPLY") == 1
        assert b"Assistant:   preserved" in terminal.output
        assert b"Reasoning: THINKING" in terminal.output

        terminal.send("question")
        terminal.wait_for('Permission request for "ask_user_question"')
        terminal.send("yes")
        terminal.wait_for("Answer: enter an option number")
        assert not tool_results, "question selected an option automatically"
        terminal.send("0")
        terminal.wait_for("Answer: invalid option number")
        terminal.send("2")
        terminal.wait_for("Assistant: TOOL_REPLY")
        assert "Beta" in tool_results[-1], tool_results

        for answer, allowed in [("", False), ("yes", True)]:
            start = len(terminal.output)
            terminal.send("permission")
            terminal.wait_for("type yes to run once", start)
            assert not (root / "approved.txt").exists(), "shell ran before approval"
            terminal.send(answer)
            terminal.wait_for("Assistant: TOOL_REPLY", start)
            assert (root / "approved.txt").exists() == allowed
        assert (root / "approved.txt").read_text() == "ACCESSIBLE_SHELL_OK"
        assert b"Tool: shell:" in terminal.output

        start = len(terminal.output)
        terminal.send("question")
        terminal.wait_for('Permission request for "ask_user_question"', start)
        terminal.send("yes")
        terminal.wait_for("Answer: enter an option number", start)
        terminal.send("/cancel")
        terminal.wait_for("Notice: cancelled", start)

        start = len(terminal.output)
        terminal.send("fail")
        terminal.wait_for("Notice: the turn failed:", start)
        terminal.send("after-failure")
        terminal.wait_for("Assistant: SESSION_STILL_USABLE", start)
        terminal.send("/status")
        terminal.wait_for("Notice: model", start)
        terminal.send("/copy")
        terminal.wait_for("Notice: accessible mode: copy the reply from terminal scrollback")
        terminal.send("/model")
        terminal.wait_for("Notice: use /model <id>")
        output = terminal.finish()
        assert output.count(b"Assistant: FIRST-REPLY") == 1
        assert output.splitlines().count(opening.splitlines()[0]) == 1
    finally:
        release_reply.set()
        terminal.close()

    terminal = Terminal(sys.argv[1], root, environment, ["resume", "last"])
    try:
        terminal.wait_for("Input: enter a prompt")
        assert terminal.output.count(b"Assistant: FIRST-REPLY") == 1
        assert b"User: " + LONG_PROMPT.encode() in terminal.output
        assert b"Tool: shell" in terminal.output
        terminal.finish()
    finally:
        terminal.close()

    # Piped accessible input uses the same transcript without terminal controls.
    piped = subprocess.run(
        [sys.argv[1], "--accessible", "--permission-mode", "ask"],
        input=b"question\nyes\n2\n/exit\n", stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        cwd=root, env=environment, timeout=15,
    )
    assert piped.returncode == 0, piped.stderr
    assert b"Assistant: TOOL_REPLY" in piped.stdout
    assert b"\x1b" not in piped.stdout + piped.stderr
    assert b"\r" not in piped.stdout
    resumed_alias = subprocess.run(
        [sys.argv[1], "--accessible", "--resume-picker"],
        input=b"", stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        cwd=root, env=environment, timeout=5,
    )
    assert resumed_alias.returncode == 0, resumed_alias.stderr
    assert b"Assistant: TOOL_REPLY" in resumed_alias.stdout
    assert b"\x1b" not in resumed_alias.stdout + resumed_alias.stderr
    server.shutdown()
