"""R-008: choose Beta in a real terminal and inspect the next provider request."""

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


results = []
prompts = []
errors = []
draft_request = threading.Event()
release_draft = threading.Event()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "question-fixture"}]}).encode()
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
                results.append(last)
                delta = {"content": f"QUESTION_REPLY_{len(results)}\n{last['content']}"}
                finish = "stop"
            else:
                text = last["content"]
                prompts.append(text)
                if text.startswith("question"):
                    assert any(
                        tool["function"]["name"] == "ask_user_question"
                        for tool in request["tools"]
                    ), "question tool was not advertised"
                    if text == "question-draft":
                        draft_request.set()
                        assert release_draft.wait(10), "draft was never released"
                    questions = [{
                        "question": "Which choice should Rune use?",
                        "options": [
                            {"label": "Alpha", "description": "Use the first choice"},
                            {"label": "Beta", "description": "Use the second choice"},
                        ],
                    }]
                    if text == "question-multiple":
                        questions = [
                            {
                                "question": f"Which choice for question {index}?",
                                "options": [{"label": f"Option {choice}"}
                                            for choice in range(1, 7)],
                            }
                            for index in range(1, 5)
                        ]
                    delta = {"tool_calls": [{
                        "index": 0,
                        "id": f"question-{len(prompts)}",
                        "type": "function",
                        "function": {
                            "name": "ask_user_question",
                            "arguments": json.dumps({"questions": questions}),
                        },
                    }]}
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


with tempfile.TemporaryDirectory(prefix="rune-r008-") as directory:
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
        "RUNE_MODEL": "question-fixture",
        "RUNE_API_KEY_ENV": "RUNE_QUESTION_TEST_KEY",
        "RUNE_QUESTION_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    child = subprocess.Popen(
        [sys.argv[1], "--permission-mode", "full-access"],
        stdin=slave, stdout=slave, stderr=slave, cwd=root, env=environment,
        preexec_fn=controlling_terminal,
    )
    os.close(slave)
    transcript = bytearray()

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

    def question(prompt="question"):
        start = len(transcript)
        send(prompt.encode() + b"\r")
        wait_for("answer required (1/1)", start)
        wait_for("Which choice should Rune use?", start)
        wait_for("Use the second choice", start)
        wait_for("> Alpha", start)
        return start

    try:
        wait_for("ctrl-c cancel")
        start = question()
        assert not results, "question answered before confirmation"
        send(b"\x1b[B\r")
        wait_for("QUESTION_REPLY_1", start)
        assert results[0]["tool_call_id"] == "question-1", results
        assert results[0]["content"] == "Which choice should Rune use?\nAnswer: Beta", results

        # An explicit Enter may confirm the initially highlighted choice.
        start = question()
        send(b"\r")
        wait_for("QUESTION_REPLY_2", start)
        assert results[1]["content"].endswith("Answer: Alpha"), results

        # Every supported question and option is reachable in one tool call.
        start = len(transcript)
        send(b"question-multiple\r")
        for index in range(1, 5):
            wait_for(f"answer required ({index}/4)", start)
            send(b"\x1b[B" * 5 + b"\r")
        wait_for("QUESTION_REPLY_3", start)
        assert results[2]["content"] == "\n".join(
            f"Which choice for question {index}?\nAnswer: Option 6"
            for index in range(1, 5)
        ), results

        # Cancel keys stop the waiting turn and never fabricate an answer.
        for cancel in [b"\x1b", b"\x03", b"\x04"]:
            start = question()
            send(cancel)
            wait_for("cancelled", start)
            start = len(transcript)
            send(b"after-cancel\r")
            wait_for("SESSION_STILL_USABLE", start)
            assert len(results) == 3, "cancelled question sent an answer"

        # Choice keys and pasted text preserve the correction and its caret.
        start = len(transcript)
        send(b"question-draft\r")
        assert draft_request.wait(10), "provider did not receive the draft turn"
        send(b"correction\x1b[D\x1b[D\x1b[D")
        wait_for("correction", start)
        release_draft.set()
        wait_for("answer required (1/1)", start)
        send(b"\x1b[200~SHOULD_NOT_STEER\x1b[201~\x15\x1b[B\r")
        wait_for("QUESTION_REPLY_4", start)
        assert results[3]["content"].endswith("Answer: Beta"), results
        start = len(transcript)
        send(b"X\r")
        wait_for("SESSION_STILL_USABLE", start)
        assert prompts[-1] == "correctXion", prompts
        assert all("SHOULD_NOT_STEER" not in prompt for prompt in prompts), prompts

        send(b"/quit\r")
        assert child.wait(timeout=10) == 0

        # Machine callers still fail promptly instead of collecting input.
        piped = subprocess.run(
            [sys.argv[1], "--permission-mode", "full-access"],
            input=b"question\n/quit\n", stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, cwd=root, env=environment, timeout=10,
        )
        assert piped.returncode == 0, piped.stderr
        assert len(results) == 5, results
        assert "this run cannot collect one" in results[-1]["content"], results
        assert "Answer:" not in results[-1]["content"], results
        assert not errors, errors
        print("Beta reached the next provider request; explicit Alpha, four questions "
              "with six options, three cancel keys, draft/caret preservation, "
              "and piped refusal passed")
    finally:
        release_draft.set()
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
