"""R-025: cut a provider after three deltas and resume the failed exchange."""

import fcntl
import http.server
import json
import os
import pathlib
import pty
import select
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


requests = []
errors = []
cut_stream = threading.Event()
partial = "STREAM-01\nSTREAM-02\nSTREAM-03\n"


def event(text=None, finish=None):
    return ("data: " + json.dumps({"choices": [{
        "index": 0,
        "delta": {} if text is None else {"content": text},
        "finish_reason": finish,
    }]}) + "\n\n").encode()


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "failure-fixture"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(request)
            prompt = request["messages"][-1]["content"]
            failed = prompt == "die"
            # Persist what Rune displays after stripping provider escape codes.
            prefix = b"".join(event("\x1b[31m" + line + "\x1b[0m\n") for line in partial.splitlines()) if failed else (
                event("AFTER_FAILURE_OK" if prompt == "after-failure" else "AFTER_RESUME_OK")
            )
            tail = event(finish="stop") + b"data: [DONE]\n\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(prefix)
            self.wfile.flush()
            if failed:
                assert cut_stream.wait(15), "the provider was never cut"
                self.close_connection = True
                self.connection.shutdown(socket.SHUT_RDWR)
                return
            self.wfile.write(tail)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            errors.append(repr(error))


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


with tempfile.TemporaryDirectory(prefix="rune-r025-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n[limits]\nprovider_max_attempts = 1\n")
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
        "RUNE_MODEL": "failure-fixture",
        "RUNE_API_KEY_ENV": "RUNE_PROVIDER_FAILURE_TEST_KEY",
        "RUNE_PROVIDER_FAILURE_TEST_KEY": "fixture-key",
        "NO_COLOR": "1",
        "TERM": "xterm-256color",
    })
    child = None
    master = None
    transcript = bytearray()

    def launch(*args):
        global child, master, transcript
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        try:
            child = subprocess.Popen(
                [sys.argv[1], *args], stdin=slave, stdout=slave, stderr=slave,
                cwd=root, env=environment, preexec_fn=controlling_terminal,
            )
        finally:
            os.close(slave)
        transcript = bytearray()

    def wait_for(text, start=0):
        deadline = time.monotonic() + 10
        while text.encode() not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"timed out waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, "Rune exited before the expected frame"
            readable, _, _ = select.select([master], [], [], 0.05)
            if readable:
                transcript.extend(os.read(master, 65536))
        assert not errors, errors

    def stop():
        global child, master
        os.write(master, b"/quit\r")
        assert child.wait(timeout=10) == 0
        os.close(master)
        master = None
        child = None

    def saved_events():
        logs = list((root / "state" / "sessions").glob("*/events.jsonl"))
        assert len(logs) == 1, logs
        return logs[0], [json.loads(line)["event"] for line in logs[0].read_text().splitlines()]

    def check_answer(request):
        answers = [message["content"] for message in request["messages"]
                   if message["role"] == "assistant"]
        assert answers.count(partial) == 1, answers
        for line in partial.splitlines():
            assert sum(answer.count(line) for answer in answers) == 1, answers
        assert all("STREAM-04" not in answer and "failed" not in answer
                   for answer in answers), answers

    try:
        launch()
        wait_for("ctrl-c cancel")
        start = len(transcript)
        os.write(master, b"die\r")
        wait_for("STREAM-03", start)
        cut_stream.set()
        wait_for("[the turn failed: the response stream ended without a completion event]", start)
        log, events = saved_events()
        assert [entry for entry in events if entry["kind"] == "assistant_message"] == [
            {"kind": "assistant_message", "turn": 1, "text": partial},
        ], events
        assert [entry for entry in events if entry["kind"] == "turn_failed"] == [
            {"kind": "turn_failed", "turn": 1, "code": "incomplete_stream",
             "message": "the response stream ended without a completion event"},
        ], events
        assert not any(entry["kind"] == "usage_recorded" for entry in events), events
        assert not any(entry["kind"] == "turn_cancelled" for entry in events), events

        start = len(transcript)
        os.write(master, b"/tree\r")
        wait_for("[failed: incomplete_stream]", start)
        assert transcript[start:].decode(errors="replace").count("[failed: incomplete_stream]") == 1

        start = len(transcript)
        os.write(master, b"after-failure\r")
        wait_for("AFTER_FAILURE_OK", start)
        check_answer(requests[-1])
        session_id = log.parent.name
        stop()

        launch("resume", session_id)
        wait_for("ctrl-c cancel")
        start = len(transcript)
        os.write(master, b"/tree\r")
        wait_for("[failed: incomplete_stream]", start)
        replay = transcript[start:].decode(errors="replace")
        assert replay.count("[failed: incomplete_stream]") == 1, replay
        for line in partial.splitlines():
            assert replay.count(line) == 1, replay
        start = len(transcript)
        os.write(master, b"after-resume\r")
        wait_for("AFTER_RESUME_OK", start)
        check_answer(requests[-1])
        stop()
        _, events = saved_events()
        assert [entry["turn"] for entry in events if entry["kind"] == "turn_started"] == [1, 2, 3], events
        assert sum(entry["kind"] == "turn_failed" for entry in events) == 1, events

        # A pipe still exits with the provider error, after saving the exchange.
        piped_state = root / "piped-state"
        piped_environment = dict(environment, RUNE_HOME=str(piped_state))
        piped = subprocess.run(
            [sys.argv[1]], input=b"die\n", capture_output=True, timeout=10,
            cwd=root, env=piped_environment,
        )
        assert piped.returncode == 1, (piped.returncode, piped.stdout, piped.stderr)
        assert b"the response stream ended without a completion event" in piped.stderr, piped.stderr
        piped_logs = list((piped_state / "sessions").glob("*/events.jsonl"))
        assert len(piped_logs) == 1, piped_logs
        piped_events = [json.loads(line)["event"] for line in piped_logs[0].read_text().splitlines()]
        assert [entry["text"] for entry in piped_events if entry["kind"] == "assistant_message"] == [partial], piped_events
        assert [entry for entry in piped_events if entry["kind"] == "turn_failed"] == [
            {"kind": "turn_failed", "turn": 1, "code": "incomplete_stream",
             "message": "the response stream ended without a completion event"},
        ], piped_events
        assert not errors, errors
        print("STREAM-01 through STREAM-03 and one failure boundary survived resume; "
              "continuation, replay, turn numbering, and piped failure status passed")
    finally:
        cut_stream.set()
        if child is not None:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=10)
        if master is not None:
            os.close(master)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=10)
