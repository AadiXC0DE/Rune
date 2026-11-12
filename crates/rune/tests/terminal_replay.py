"""R-034: record resize events and bytes from the actual Rune PTY.

Only Python's standard library is needed. Rust replays these events into a grid
and compares the result with the committed fixtures.
"""

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


prompts = []
errors = []
code_steps = [threading.Event(), threading.Event()]
transcript_live_step = threading.Event()
transcript_live_finished = threading.Event()
markdown_step = threading.Event()
stream_pause = threading.Event()
MARKDOWN_DELTAS = [
    "# Plan\n\n- first item\n- second item\n\n",
    "```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n",
    "| name | size |\n| --- | --- |\n| a | 1 |\n",
    "| bb | 22 |\n\nMARKDOWN-END",
]


def tool_call(index, name, arguments):
    return {"index": index, "id": f"{name}-{index}", "type": "function",
            "function": {"name": name, "arguments": json.dumps(arguments)}}


def tool_step(*calls):
    return [{"index": 0, "delta": {"tool_calls": list(calls)},
             "finish_reason": "tool_calls"}]


def text_step(answer):
    return [{"index": 0, "delta": {"content": answer}, "finish_reason": None},
            {"index": 0, "delta": {}, "finish_reason": "stop"}]


def event(choice):
    return ("data: " + json.dumps({"choices": [choice]}) + "\n\n").encode()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def answer_choices(self, choices):
        body = b"".join(event(choice) for choice in choices) + b"data: [DONE]\n\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        body = json.dumps({"data": [
            {"id": f"replay-model-{index:02}"} for index in range(1, 16)
        ]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            prompts.append(request["messages"][-1]["content"])
            if scenario == "failure":
                body = json.dumps({"error": {
                    "message": "invalid api key", "type": "invalid_request_error",
                }}).encode()
                self.send_response(401)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            if scenario == "tools":
                if len(prompts) == 1:
                    self.answer_choices(tool_step(
                        tool_call(0, "read_file", {"path": "notes.txt"}),
                        tool_call(1, "read_file", {"path": "missing.txt"}),
                    ))
                elif len(prompts) == 2:
                    # Four reads in one step fold into one row.
                    self.answer_choices(tool_step(*(
                        tool_call(index, "read_file", {"path": f"part-{index}.txt"})
                        for index in range(4)
                    )))
                else:
                    self.answer_choices(text_step("TOOLS-DONE"))
                return
            if scenario == "shell-tail":
                self.answer_choices(tool_step(tool_call(0, "shell", {
                    "action": "run",
                    "command": "for i in 1 2 3 4; do echo TAIL-$i; sleep 0.3; done",
                })) if len(prompts) == 1 else text_step("SHELL-DONE"))
                return
            if scenario == "edit":
                self.answer_choices(tool_step(tool_call(0, "edit_file", {
                    "path": "app.txt", "old_string": "beta",
                    "new_string": "BETA\nbeta two",
                })) if len(prompts) == 1 else text_step(
                    "EDIT-DONE" if len(prompts) == 2 else "EDIT-CHECKED"
                ))
                return
            if scenario == "approval":
                self.answer_choices(tool_step(tool_call(0, "shell", {
                    "command": "echo GRID-APPROVAL",
                })))
                return
            if scenario in {"reasoning", "reasoning-full"}:
                self.answer_choices(
                    [{"index": 0, "delta": {"reasoning_content": f"THINK-{n}\n"},
                      "finish_reason": None} for n in range(1, 6)]
                    + text_step("REASONED")
                )
                return
            if scenario == "markdown":
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for index, chunk in enumerate(MARKDOWN_DELTAS):
                    self.wfile.write(event({"index": 0, "delta": {"content": chunk},
                                            "finish_reason": None}))
                    self.wfile.flush()
                    if index == 1:
                        assert markdown_step.wait(10), "markdown frame was not captured"
                self.wfile.write(event({"index": 0, "delta": {}, "finish_reason": "stop"})
                                 + b"data: [DONE]\n\n")
                self.wfile.flush()
                return
            if scenario == "transcript":
                if prompts[-1] == "live":
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    for index, chunk in enumerate(["LIVE-PARTIAL", " LIVE-END"]):
                        choice = {"index": 0, "delta": {"content": chunk},
                                  "finish_reason": None}
                        self.wfile.write(("data: " + json.dumps({"choices": [choice]})
                                          + "\n\n").encode())
                        self.wfile.flush()
                        if index == 0:
                            assert transcript_live_step.wait(10), "live viewer was not captured"
                    choice = {"index": 0, "delta": {}, "finish_reason": "stop"}
                    self.wfile.write(("data: " + json.dumps({"choices": [choice]})
                                      + "\n\ndata: [DONE]\n\n").encode())
                    self.wfile.flush()
                    transcript_live_finished.set()
                    return
                if len(prompts) == 1:
                    choices = [{"index": 0, "delta": {"tool_calls": [{
                        "index": 0, "id": "transcript-read", "type": "function",
                        "function": {"name": "read_file", "arguments": json.dumps({
                            "path": "transcript.txt",
                        })},
                    }]}, "finish_reason": "tool_calls"}]
                else:
                    answer = "\n".join(f"REPLY-{index:02}" for index in range(1, 41))
                    choices = [
                        {"index": 0, "delta": {"content": answer}, "finish_reason": None},
                        {"index": 0, "delta": {}, "finish_reason": "stop"},
                    ]
                body = ("".join(
                    "data: " + json.dumps({"choices": [choice]}) + "\n\n"
                    for choice in choices
                ) + "data: [DONE]\n\n").encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            if scenario in {"progressive", "tall-code", "resize-stream"}:
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                if scenario == "progressive":
                    chunks = [f"PARA-{index:03}\n\n" for index in range(1, 301)]
                    pause = 150
                elif scenario == "resize-stream":
                    chunks = [f"STREAM-{index:02}\n\n" for index in range(1, 31)]
                    pause = 15
                else:
                    chunks = ["```\n"] + [f"CODE-{index:03}\n" for index in range(1, 121)]
                    chunks.append("```\nTALL-END")
                    pause = 81
                for index, chunk in enumerate(chunks, 1):
                    self.wfile.write(event({"index": 0, "delta": {
                        "content": chunk}, "finish_reason": None}))
                    self.wfile.flush()
                    if index == pause:
                        assert stream_pause.wait(10), "the paused frame was not captured"
                self.wfile.write(event({"index": 0, "delta": {}, "finish_reason": "stop"})
                                 + b"data: [DONE]\n\n")
                self.wfile.flush()
                return
            if scenario == "code":
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for index, chunk in enumerate([
                    "```rust\n    abcdefghijklmnopqrstuvwxyz0123456789",
                    "  END-CODE",
                    "\n```\nAFTER-CODE",
                ]):
                    choice = {"index": 0, "delta": {"content": chunk},
                              "finish_reason": None}
                    self.wfile.write(("data: " + json.dumps({"choices": [choice]})
                                      + "\n\n").encode())
                    self.wfile.flush()
                    if index < len(code_steps):
                        assert code_steps[index].wait(10), "code frame was not captured"
                choice = {"index": 0, "delta": {}, "finish_reason": "stop"}
                self.wfile.write(("data: " + json.dumps({"choices": [choice]})
                                  + "\n\ndata: [DONE]\n\n").encode())
                self.wfile.flush()
                return
            answer = "COMPACT-REPLY" if scenario == "compact" else "W" * 300 + "END-LONG-WORD"
            choices = [
                {"index": 0, "delta": {"content": answer},
                 "finish_reason": None},
                {"index": 0, "delta": {}, "finish_reason": "stop"},
            ]
            body = ("".join(
                "data: " + json.dumps({"choices": [choice]}) + "\n\n"
                for choice in choices
            ) + "data: [DONE]\n\n").encode()
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


scenario = sys.argv[2]
assert scenario in {"resize", "narrow", "menu-resize", "completion-resize", "code", "transcript", "compact", "help-40", "help-80", "tools", "shell-tail", "edit", "failure", "approval", "markdown", "reasoning", "reasoning-full", "progressive", "tall-code", "resize-stream"}, scenario
with tempfile.TemporaryDirectory(prefix="rune-r034-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n")
    config_file.chmod(0o600)
    if scenario == "transcript":
        (root / "transcript.txt").write_text(
            "\n".join(f"TOOL-{index:02}" for index in range(1, 31)) + "\n"
        )
    if scenario == "tools":
        (root / "notes.txt").write_text("first note\nsecond note\nthird note\n")
        for index in range(4):
            (root / f"part-{index}.txt").write_text(f"part {index}\n")
    if scenario == "edit":
        (root / "app.txt").write_text("alpha\nbeta\ngamma\n")
    if scenario.startswith("help-"):
        commands = root / ".rune" / "commands"
        commands.mkdir(parents=True)
        (commands / "wrap-fixture.md").write_text(
            "---\ndescription: Recover this full custom description with 界e\u0301 "
            "and its final words from scrollback.\n---\nFixture body.\n"
        )
        (commands / "long-fixture.md").write_text(
            "---\ndescription: " + "W" * 140 + "END-COMMAND\n---\nFixture body.\n"
        )
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
        "RUNE_MODEL": "replay-model-01" if scenario == "menu-resize" else "replay-fixture",
        "RUNE_API_KEY_ENV": "RUNE_REPLAY_TEST_KEY",
        "RUNE_REPLAY_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    if scenario in {"transcript", "tools", "shell-tail", "edit", "markdown"}:
        environment["RUNE_PERMISSION_MODE"] = "full-access"
    if scenario == "approval":
        environment["RUNE_PERMISSION_MODE"] = "ask"
    cols, rows = {"resize": (32, 24), "narrow": (12, 24),
                  "menu-resize": (80, 24), "completion-resize": (80, 24), "code": (32, 24),
                  "transcript": (80, 24), "compact": (80, 4),
                  "help-40": (40, 24), "help-80": (80, 24),
                  "tools": (80, 24), "shell-tail": (80, 24),
                  "edit": (80, 24), "failure": (80, 24),
                  "approval": (80, 24), "markdown": (80, 24),
                  "reasoning": (80, 24), "reasoning-full": (80, 24),
                  "progressive": (80, 24), "tall-code": (80, 24),
                  "resize-stream": (80, 24)}[scenario]
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    try:
        child = subprocess.Popen(
            [sys.argv[1]], stdin=slave, stdout=slave, stderr=slave,
            cwd=root, env=environment, preexec_fn=controlling_terminal,
        )
    finally:
        os.close(slave)
    transcript = bytearray()
    captures = []
    offset = 0

    def wait_for(text, start=0):
        deadline = time.monotonic() + 10
        while text.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n"
                + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))
        assert not errors, errors

    def capture(stage, keys=b"", expected="\x1b[?25h", size=None, repaint=None):
        global offset, cols, rows
        start = len(transcript)
        if size is not None:
            # Drain the old size before recording the ioctl boundary. The
            # terminal driver delivers SIGWINCH to Rune's foreground group.
            assert not select.select([master], [], [], 0)[0]
            cols, rows = size
            fcntl.ioctl(master, termios.TIOCSWINSZ,
                        struct.pack("HHHH", rows, cols, 0, 0))
        if keys:
            os.write(master, keys)
        wait_for(expected, start)
        deadline = time.monotonic() + 10
        changed = time.monotonic()
        while time.monotonic() - changed < 0.15:
            assert time.monotonic() < deadline, "terminal never finished drawing"
            if select.select([master], [], [], 0.05)[0]:
                data = os.read(master, 65536)
                transcript.extend(data)
                # A repaint that draws nothing does not count as drawing.
                if repaint is None or data.replace(repaint, b""):
                    changed = time.monotonic()
        captures.append({"stage": stage, "cols": cols, "rows": rows,
                         "bytes": list(transcript[offset:])})
        offset = len(transcript)

    try:
        if scenario == "compact":
            capture("compact-idle", expected="compact mode (4 rows): prompts work")
        else:
            # At 12 columns the hint is deliberately clipped to this prefix.
            wait_for("/ commands")
        if scenario.startswith("help-"):
            capture("help-settled", b"/help\r")
            assert not prompts, prompts
        elif scenario == "resize":
            draft = "0123456789" * 5 + "VISIBLE-END"
            # The draft wraps: at 32 columns its last row holds only the D, at
            # 20 columns the tail after VISI.
            capture("narrow-draft", draft.encode(), "  D")
            # No key is sent on either resize. A stale draft must fail here.
            capture("grown-draft", expected=draft, size=(80, 24))
            capture("shrunk-draft", expected="BLE-END", size=(20, 24))
            capture("grown-again", expected=draft, size=(80, 24))
            os.write(master, b"\x03")
            wait_for("\x1b[?25h", offset)
        elif scenario == "compact":
            capture("compact-draft", b"narrow height", "> narrow height")
            assert not prompts, prompts
            capture("compact-answer", b"\r", "COMPACT-REPLY\r\n")
            assert prompts == ["narrow height"], prompts
        elif scenario == "menu-resize":
            capture("model-open", b"/model\r", "type to narrow")
            capture("model-shrunk", expected="> replay-model-01 (current)", size=(32, 8))
            capture("model-down", b"\x1b[B", "> replay-model-02")
            capture("model-closed", b"\x1b")
            capture("closed-grown", size=(80, 24))
            assert not prompts, prompts
        elif scenario == "completion-resize":
            # Walk down to /new a step at a time, each step waiting for its
            # frame, because its place in the list moves as commands are added.
            os.write(master, b"/")
            wait_for(" of ", offset)
            for _ in range(64):
                if b"> /new" in transcript[offset:]:
                    break
                mark = len(transcript)
                os.write(master, b"\x1b[B")
                wait_for("\x1b[?25h", mark)
            assert b"> /new" in transcript[offset:], "never reached /new"
            # Already drawn above, so the capture only records it.
            capture("completion-selected", expected="")
            # No key is sent on either resize. Rust checks the selected row.
            capture("completion-shrunk", size=(32, 10))
            capture("completion-grown", size=(80, 24))
            # F2 has no binding and must leave the selection alone.
            capture("completion-ignored", b"\x1bOQ")
            capture("completion-accepted", b"\t")
            # Editing must still reset the selection for the narrowed list.
            capture("completion-edited", b"\x03/" + b"\x1b[B" * 9 + b"m", "> /model")
            capture("completion-cleared", b"\x03")
            assert not prompts, prompts
        elif scenario == "code":
            # The provider pauses until each live frame has been captured,
            # proving the live row is cut before the closing fence or turn completion.
            capture("code-started", b"code\r", "wxy\u2026")
            code_steps[0].set()
            # The live row is already cut to the region, so the second delta
            # draws nothing new; the stage only lets the closing fence follow.
            capture("code-continued", expected="")
            code_steps[1].set()
            capture("code-finished", expected="AFTER-CODE\r\n")
            assert prompts == ["code"], prompts
        elif scenario == "transcript":
            capture("recorded", b"inspect\r", "REPLY-40\r\n")
            assert len(prompts) == 2, prompts
            assert "TOOL-30" in prompts[1], prompts[1]
            capture("draft", "draft 界 tail".encode() + b"\x1b[D" * 6)
            capture("opened", b"\x0f", "Esc/Ctrl-O")
            capture("page-down", b"\x1b[6~", "\x1b[24;1H")
            capture("end", b"\x1b[F", "\x1b[24;1H")
            capture("up", b"\x1b[A", "\x1b[24;1H")
            capture("down", b"\x1b[B", "\x1b[24;1H")
            capture("page-up", b"\x1b[5~", "\x1b[24;1H")
            capture("home", b"\x1b[H", "\x1b[24;1H")
            # Select the result after its call, then exercise disclosure without
            # sending any of these keys to the saved composer draft.
            capture("tool-selected", b"\t\t", "\x1b[24;1H")
            capture("tool-collapsed", b" ", "\x1b[24;1H")
            capture("tool-expanded", b" ", "\x1b[24;1H")
            capture("tool-recollapsed", b" ", "\x1b[24;1H")
            capture("tool-surrounding-start", b"\x1b[H", "\x1b[24;1H")
            capture("tool-surrounding-end", b"\x1b[F", "\x1b[24;1H")
            # Editing keys, Enter and a paste must never reach the composer.
            capture("closed", b"ignored\r\x1b[200~pasted\ntext\x1b[201~\x1b",
                    "\x1b[?1049l")
            capture("edited", b"Z", "draft Z")
            capture("reopened", b"\x0f", "Esc/Ctrl-O")
            capture("resized", expected="Transcript", size=(32, 8))
            capture("closed-resized", b"\x0f", "\x1b[?1049l")
            # Restore size, then send the saved draft and verify its exact bytes.
            capture("restored-size", size=(80, 24))
            capture("submitted", b"\r", "REPLY-40\r\n")
            assert prompts[2] == "draft Z界 tail", prompts
            capture("live-started", b"live\r", "LIVE-PARTIAL")
            capture("live-draft", b"live draft" + b"\x1b[D" * 4)
            capture("live-opened", b"\x0f", "Esc/Ctrl-O")
            capture("live-end", b"\x1b[F", "\x1b[24;1H")
            transcript_live_step.set()
            assert transcript_live_finished.wait(10), "provider did not finish"
            # The provider finished while the snapshot owned the terminal.
            # No streaming frame may overwrite its rows or footer.
            assert not select.select([master], [], [], 0.2)[0], "live output painted over the viewer"
            capture("live-closed", b"\x1b", "LIVE-PARTIAL LIVE-END\r\n")
            capture("live-edited", b"Z", "live dZraft")
            os.write(master, b"\x03")
            wait_for("\x1b[?25h", len(transcript))
            capture("new-session", b"/new\r", "started session")
            capture("empty", b"\x0f", "Esc/Ctrl-O")
            capture("empty-closed", b"\x1b", "\x1b[?1049l")
            assert len(prompts) == 4, prompts
        elif scenario == "tools":
            capture("tools-finished", b"tools\r", "TOOLS-DONE\r\n")
            assert len(prompts) == 3, prompts
        elif scenario == "shell-tail":
            # The command prints a line every 0.3 s. The frame that draws the
            # second one is recorded while the command still runs, without
            # waiting for the spinner to go quiet.
            start = len(transcript)
            os.write(master, b"tail\r")
            wait_for("    TAIL-2", start)
            drawn = transcript.index(b"    TAIL-2", start)
            wait_for("\x1b[?25h", drawn)
            end = transcript.index(b"\x1b[?25h", drawn) + len(b"\x1b[?25h")
            captures.append({"stage": "shell-running", "cols": cols, "rows": rows,
                             "bytes": list(transcript[offset:end])})
            offset = end
            capture("shell-finished", expected="SHELL-DONE\r\n")
            assert len(prompts) == 2, prompts
        elif scenario == "edit":
            # The turn asks for a check once after the edit, so it ends on the
            # reply to that request.
            capture("edit-finished", b"edit\r", "EDIT-CHECKED\r\n")
            assert len(prompts) == 3, prompts
            assert prompts[2].startswith("<harness_check>"), prompts
            assert (root / "app.txt").read_text() == "alpha\nBETA\nbeta two\ngamma\n"
        elif scenario == "failure":
            os.write(master, b"fail\r")
            wait_for("error: the turn failed: ", offset)
            failed = transcript.index(b"error: the turn failed: ", offset)
            # The hint is the indented row under the failure row.
            wait_for("\r\n", failed)
            first_end = transcript.index(b"\r\n", failed) + 2
            wait_for("  ", first_end)
            wait_for("\r\n", first_end)
            capture("failure-reported", expected="")
        elif scenario == "approval":
            # The turn's ticker keeps repainting an empty frame under the prompt.
            capture("approval-open", b"approve\r", "permission required",
                    repaint=b"\x1b[?25l\r\x1b[1G\x1b[?25h")
            time.sleep(0.4)
            capture("approval-cancelled", b"\x03", "cancelled")
            assert prompts == ["approve"], prompts
        elif scenario == "reasoning":
            capture("reasoning-finished", b"think\r", "REASONED\r\n")
            assert prompts == ["think"], prompts
        elif scenario == "reasoning-full":
            # Alt+T twice: collapsed, then tail, then full.
            wait_for("/ commands")
            os.write(master, b"\x1bt")
            wait_for("reasoning display: tail")
            os.write(master, b"\x1bt")
            wait_for("reasoning display: full")
            capture("reasoning-full-finished", b"think\r", "REASONED\r\n")
            assert prompts == ["think"], prompts
        elif scenario == "progressive":
            capture("progressive-paused", b"stream\r", "PARA-150")
            stream_pause.set()
            capture("progressive-finished", expected="PARA-300\r\n")
            assert prompts == ["stream"], prompts
        elif scenario == "tall-code":
            capture("tall-code-paused", b"code\r", "CODE-080")
            stream_pause.set()
            capture("tall-code-finished", expected="TALL-END\r\n")
            assert prompts == ["code"], prompts
        elif scenario == "resize-stream":
            # No key is sent on either resize: the turn redraws once the new
            # size has settled.
            capture("streaming", b"stream\r", "STREAM-15")
            capture("shrunk", size=(40, 24))
            capture("grown", size=(100, 24))
            stream_pause.set()
            # The idle hint is drawn only once the turn has ended.
            capture("finished", expected="/ commands")
            assert prompts == ["stream"], prompts
        elif scenario == "markdown":
            capture("markdown-streaming", b"markdown\r", "fn main")
            markdown_step.set()
            capture("markdown-finished", expected="MARKDOWN-END\r\n")
            assert prompts == ["markdown"], prompts
        else:
            # Wait for the committed ending, rather than an intermediate frame.
            capture("finished-answer", b"long-word\r", "END-LONG-W\r\nORD\r\n")
            assert prompts == ["long-word"], prompts
        os.write(master, b"/quit\r")
        assert drain_wait(child, master, 10, transcript) == 0
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
