"""R-003: exercise approval input against a local provider and the real PTY."""

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


COMMAND = "printf AUDIT_SHELL_OK; printf x >> shell-ran"
results = []
prompts = []
errors = []
draft_request = threading.Event()
release_draft = threading.Event()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "approval-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            last = request["messages"][-1]
            if last["role"] == "tool":
                results.append(last["content"])
                delta = {
                    "content": f"FIXTURE_REPLY_{len(results)}\nTool reply: {last['content']}",
                }
                finish = "stop"
            else:
                text = last["content"]
                prompts.append(text)
                if text.startswith("permission"):
                    if text == "permission-draft":
                        draft_request.set()
                        assert release_draft.wait(10), "draft was never released"
                    delta = {
                        "tool_calls": [{
                            "index": 0,
                            "id": f"shell-{len(prompts)}",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": json.dumps({
                                    "action": "run",
                                    "command": COMMAND,
                                }),
                            },
                        }],
                    }
                    finish = "tool_calls"
                else:
                    delta = {"content": "SESSION_STILL_USABLE"}
                    finish = "stop"
            body = "".join(
                "data: " + json.dumps({"choices": [choice]}) + "\n\n"
                for choice in [
                    {"index": 0, "delta": delta, "finish_reason": None},
                    {"index": 0, "delta": {}, "finish_reason": finish},
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


with tempfile.TemporaryDirectory(prefix="rune-r003-") as directory:
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
        "RUNE_MODEL": "approval-fixture",
        "RUNE_API_KEY_ENV": "RUNE_APPROVAL_TEST_KEY",
        "RUNE_APPROVAL_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    child = subprocess.Popen(
        [sys.argv[1], "--permission-mode", "ask", "--allow-unsandboxed"],
        stdin=slave, stdout=slave, stderr=slave, cwd=root, env=environment,
        preexec_fn=controlling_terminal,
    )
    os.close(slave)
    transcript = bytearray()
    side_effect = root / "shell-ran"

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

    def send(data):
        os.write(master, data)

    def settled_reply(text, start):
        # Streaming rows end in an erase/style sequence. The plain line ending
        # appears only when close_turn commits the reply after joining the
        # worker. Sending another prompt on streamed text races that join and
        # submits steering to the old turn instead of starting the next one.
        wait_for(text + "\r\n", start)

    def permission(prompt="permission"):
        start = len(transcript)
        send(prompt.encode() + b"\r")
        wait_for("permission required", start)
        wait_for(f'Scope: "{COMMAND}"', start)
        wait_for("  Run once", start)
        wait_for("> Deny", start)
        return start

    try:
        wait_for("session ")
        start = permission()
        assert not side_effect.exists(), "shell ran before approval"
        confirmed_at = len(transcript)
        send(b"\x1b[A\r")
        settled_reply("FIXTURE_REPLY_1", start)
        wait_for("AUDIT_SHELL_OK", confirmed_at)
        assert "AUDIT_SHELL_OK" in results[0], results
        assert side_effect.read_bytes() == b"x"

        # Approving once does not grant the same command on a subsequent turn.
        start = permission()
        send(b"\r")
        settled_reply("FIXTURE_REPLY_2", start)
        assert "refused by policy" in results[1], results
        assert side_effect.read_bytes() == b"x", "denied shell ran"

        # Every cancel key releases the worker and leaves the session usable.
        for cancel in [b"\x1b", b"\x03", b"\x04"]:
            start = permission()
            send(cancel)
            settled_reply("[cancelled]", start)
            assert side_effect.read_bytes() == b"x", "cancelled shell ran"
            start = len(transcript)
            send(b"after-cancel\r")
            settled_reply("SESSION_STILL_USABLE", start)

        # An approval consumes neither a correction draft nor its caret position.
        start = len(transcript)
        send(b"permission-draft\r")
        assert draft_request.wait(10), "provider did not receive the draft turn"
        send(b"correction\x1b[D\x1b[D\x1b[D")
        wait_for("correction", start)
        # Release the provider only after all three caret moves were rendered,
        # otherwise a pending Left key can land in the approval picker.
        draft_end = transcript.index(b"correction", start) + len(b"correction")
        wait_for("\x1b[10G\x1b[?25h", draft_end)
        release_draft.set()
        wait_for("permission required", start)
        wait_for("> Deny", start)
        send(b"\x1b[200~SHOULD_NOT_STEER\x1b[201~\x15\x1b[A\r")
        settled_reply("FIXTURE_REPLY_3", start)
        assert side_effect.read_bytes() == b"xx"
        start = len(transcript)
        send(b"X\r")
        settled_reply("SESSION_STILL_USABLE", start)
        assert prompts[-1] == "correctXion", prompts
        assert all("SHOULD_NOT_STEER" not in prompt for prompt in prompts), prompts

        send(b"/quit\r")
        assert child.wait(timeout=10) == 0

        # Piped sessions have nobody to ask and must keep their refusal behavior.
        piped = subprocess.run(
            [sys.argv[1], "--permission-mode", "ask", "--allow-unsandboxed"],
            input=b"permission\n/quit\n", stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, cwd=root, env=environment, timeout=10,
        )
        assert piped.returncode == 0, piped.stderr
        assert "no way to ask for approval" in results[-1], results
        assert side_effect.read_bytes() == b"xx", "piped shell ran"
        assert not errors, errors
        print("approved shell returned AUDIT_SHELL_OK; repeat denial, three cancel keys, "
              "draft/caret preservation, and piped refusal passed")
    finally:
        release_draft.set()
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
