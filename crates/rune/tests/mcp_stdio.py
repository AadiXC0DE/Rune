"""R-163: schema discovery, model dispatch, trust, deadlines and child cleanup."""

import fcntl
import http.server
import json
import os
import pathlib
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


SCHEMA = {
    "type": "object", "properties": {"text": {"type": "string"}},
    "required": ["text"], "additionalProperties": False,
}
FIXTURE = r'''
import json, os, pathlib, signal, sys, time
root = pathlib.Path(os.environ["MCP_WORKSPACE"])
mode, label = sys.argv[1:]
# Require Rune to use its bounded SIGKILL fallback during every shutdown.
signal.signal(signal.SIGTERM, signal.SIG_IGN)
(root / (label + ".pid")).write_text(str(os.getpid()))
with (root / (label + ".jsonl")).open("a", buffering=1) as log:
    for line in sys.stdin:
        request = json.loads(line)
        log.write(json.dumps(request) + "\n")
        method = request["method"]
        if method == "notifications/initialized":
            continue
        if (mode == "startup-timeout" and method == "initialize"
                or mode == "list-timeout" and method == "tools/list"
                or mode in ("call-timeout", "cancel") and method == "tools/call"):
            while True:
                time.sleep(1)
        if mode == "startup-error":
            response = {"error": {"code": -32603, "message": os.environ["MCP_SECRET"]}}
        elif method == "initialize":
            assert os.environ["MCP_SECRET"] == "server-only-secret"
            assert "RUNE_MCP_TEST_KEY" not in os.environ
            assert pathlib.Path.cwd() == root
            response = {"result": {"protocolVersion": request["params"]["protocolVersion"],
                                   "capabilities": {"tools": {}}}}
        elif method == "tools/list":
            response = {"result": {"tools": [{"name": "echo", "description": "Fixture echo",
                "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}},
                                "required": ["text"], "additionalProperties": False}}]}}
        else:
            assert method == "tools/call", request
            assert request["params"] == {"name": "echo", "arguments": {"text": "hello MCP"}}
            response = {"result": {"content": [{"type": "text", "text": "ECHO: hello MCP"}],
                                   "isError": mode == "call-error"}}
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], **response}), flush=True)
'''


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def run_fixture(scenario, terminal=False):
    requests, errors = [], []
    unavailable = scenario in {"pending", "rejected", "disabled", "optional-timeout", "startup-error"}
    fatal = scenario == "required-timeout"

    class Provider(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def reply(self, body, content_type):
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.reply(b'{"data": [{"id": "mcp-fixture"}]}', "application/json")

        def do_POST(self):
            try:
                assert self.path == "/v1/chat/completions", self.path
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                requests.append(request)
                tools = {tool["function"]["name"]: tool["function"] for tool in request["tools"]}
                assert all(not name.startswith("mcp_remote_") for name in tools), tools
                step = len(requests)
                if unavailable:
                    assert "mcp_local_echo" not in tools, tools
                    assert step == 1, requests
                else:
                    tool = tools["mcp_local_echo"]
                    assert tool["parameters"] == SCHEMA, tool
                    assert tool["description"] == "Fixture echo", tool
                    if step == 2:
                        result = request["messages"][-1]
                        assert result["role"] == "tool", result
                        if scenario == "call-timeout":
                            assert "timeout" in result["content"], result
                            assert "tools/call" in result["content"], result
                        elif scenario == "policy":
                            assert "denied" in result["content"].lower(), result
                        else:
                            assert result["content"] == "ECHO: hello MCP", result
                    else:
                        assert step == 1, requests
                call = not unavailable and step == 1
                delta = {"tool_calls": [{
                    "index": 0, "id": "mcp-call", "type": "function",
                    "function": {"name": "mcp_local_echo", "arguments": '{"text":"hello MCP"}'},
                }]} if call else {"content": "MCP_FIXTURE_PASSED"}
                body = "".join("data: " + json.dumps({"choices": [choice]}) + "\n\n"
                               for choice in [
                                   {"index": 0, "delta": delta, "finish_reason": None},
                                   {"index": 0, "delta": {}, "finish_reason": "tool_calls" if call else "stop"},
                               ]) + "data: [DONE]\n\n"
                self.reply(body.encode(), "text/event-stream")
            except Exception as error:
                errors.append(repr(error))
                self.send_error(500)

    with tempfile.TemporaryDirectory(prefix="rune-r163-") as directory:
        base = pathlib.Path(directory).resolve()
        root = base / "workspace"
        root.mkdir()
        (base / "state").mkdir(mode=0o700)
        (root / "mcp_fixture.py").write_text(FIXTURE)
        config = base / "config" / "rune"
        config.mkdir(parents=True, mode=0o700)
        (config / "config.toml").write_text(
            "auto_upgrade = false\n[limits]\nmax_agent_steps = 4\nprovider_max_attempts = 1\n"
            "mcp_startup_timeout_ms = 300\n"
            f"mcp_operation_timeout_ms = {10000 if scenario == 'cancel' else 300}\n"
        )
        (config / "config.toml").chmod(0o600)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        environment = child_environment({
            "HOME": directory, "PATH": "/usr/bin:/bin", "NO_COLOR": "1", "TERM": "xterm-256color",
            "XDG_CONFIG_HOME": str(base / "config"), "XDG_DATA_HOME": str(base / "data"),
            "XDG_STATE_HOME": str(base / "xdg-state"), "RUNE_HOME": str(base / "state"),
            "RUNE_PROVIDER": "chat_completions", "RUNE_MODEL": "mcp-fixture",
            "RUNE_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
            "RUNE_API_KEY_ENV": "RUNE_MCP_TEST_KEY", "RUNE_MCP_TEST_KEY": "provider-only-secret",
            "RUNE_TEST_SERVER_SECRET": "server-only-secret",
        })
        mode = {
            "optional-timeout": "startup-timeout", "required-timeout": "list-timeout",
        }.get(scenario, scenario)

        def declaration(label, mode):
            return {
                "command": sys.executable,
                "args": ["${WORKSPACE}/mcp_fixture.py", mode, label],
                "env": {"MCP_WORKSPACE": "${WORKSPACE}", "MCP_SECRET": "${RUNE_TEST_SERVER_SECRET}"},
            }

        entries = {"local": declaration("local", mode)}
        # HTTP is now supported online; this fixture exercises stdio and keeps
        # its remote declaration disabled. R-164 covers online HTTP dispatch.
        entries["remote"] = {"type": "http", "url": f"http://127.0.0.1:{server.server_port}/mcp",
                             "enabled": False}
        if scenario == "disabled":
            entries["local"]["enabled"] = False
            entries["local"]["args"] = ["${UNSET_DISABLED_VARIABLE}"]
        if fatal:
            entries["local"]["required"] = True
        if fatal or scenario == "optional-timeout":
            entries["a_healthy"] = declaration("healthy", "happy")
        if scenario == "partial":
            entries["pending"] = declaration("pending", "happy")
            entries["pending"]["args"] = ["${UNSET_PENDING_VARIABLE}"]
        (root / ".mcp.json").write_text(json.dumps({"mcpServers": entries}))

        def command(*args, **options):
            return subprocess.run([sys.argv[1], *args], cwd=root, env=environment,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15, **options)

        child, master = None, None
        try:
            if scenario != "pending":
                action = "reject" if scenario == "rejected" else "approve"
                approval = command("projects", action)
                assert approval.returncode == 0, approval
            if scenario == "partial":
                # Preserve the CLI-created private record, narrowing to one server.
                trust_file = next((base / "state").glob("*trust*"))
                trust = json.loads(trust_file.read_text())
                trust["entries"][0]["entries"] = ["server:local"]
                trust_file.write_text(json.dumps(trust))
            args = ["--permission-mode", "ask" if scenario == "policy" else "full-access"]
            if scenario == "offline":
                args.append("--offline")
            started = time.monotonic()
            if terminal:
                master, slave = pty.openpty()
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
                child = subprocess.Popen([sys.argv[1], *args], stdin=slave, stdout=slave, stderr=slave,
                                         cwd=root, env=environment, preexec_fn=controlling_terminal)
                os.close(slave)
                transcript = bytearray()

                def wait_for(marker):
                    deadline = time.monotonic() + 10
                    while marker not in transcript:
                        assert child.poll() is None, (child.returncode, transcript)
                        assert time.monotonic() < deadline, transcript
                        if select.select([master], [], [], 0.05)[0]:
                            transcript.extend(os.read(master, 65536))

                wait_for(b"ctrl-c cancel")
                os.write(master, b"echo through MCP\r")
                if scenario == "cancel":
                    deadline = time.monotonic() + 10
                    while "tools/call" not in (root / "local.jsonl").read_text():
                        assert time.monotonic() < deadline, transcript
                        time.sleep(0.02)
                    cancelled = time.monotonic()
                    os.write(master, b"\x03")
                    wait_for(b"[cancelled]")
                    assert time.monotonic() - cancelled < 2, transcript
                else:
                    wait_for(b"MCP_FIXTURE_PASSED")
                os.write(master, b"/quit\r")
                assert child.wait(timeout=5) == 0, transcript
                stdout, stderr = bytes(transcript), b""
            else:
                prompt = b"echo through MCP\n" + (b"no\n" if scenario == "policy" else b"")
                output = command(*args, "--accessible",
                                 input=(b"" if scenario == "offline" else prompt) + b"/quit\n")
                stdout, stderr = output.stdout, output.stderr
                assert (output.returncode != 0) == fatal, (stdout, stderr, errors)
            assert time.monotonic() - started < 8, (scenario, stdout, stderr)
            assert not errors, errors
            if fatal:
                assert not requests, requests
                assert b"timeout" in stderr and b"local" in stderr, stderr
            elif scenario == "offline":
                assert not requests, requests
                assert b"session " in stdout, stdout
            elif scenario == "cancel":
                assert len(requests) == 1, requests
            else:
                assert b"MCP_FIXTURE_PASSED" in stdout, (stdout, stderr)
                assert len(requests) == (1 if unavailable else 2), requests
            assert b"server-only-secret" not in stdout + stderr, (stdout, stderr)
            if scenario in {"optional-timeout", "startup-error"}:
                assert b"MCP server `local`" in stdout, stdout
                if scenario == "startup-error":
                    assert b"<redacted>" in stdout, stdout
            methods = []
            log = root / "local.jsonl"
            if log.exists():
                methods = [json.loads(line)["method"] for line in log.read_text().splitlines()]
            if scenario in {"pending", "rejected", "disabled"}:
                assert not methods and not (root / "local.pid").exists(), methods
            elif scenario in {"policy", "offline"}:
                assert methods == ["initialize", "notifications/initialized", "tools/list"], methods
            elif not unavailable and not fatal:
                assert methods == ["initialize", "notifications/initialized", "tools/list", "tools/call"], methods
            assert not (root / "pending.pid").exists()
            # Successful CLI exit must include cleanup, even for zero-tool or
            # required-failure paths and a child that refuses SIGTERM.
            for pid_file in root.glob("*.pid"):
                pid = int(pid_file.read_text())
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    pass
                else:
                    raise AssertionError(f"MCP child {pid} survived CLI exit ({scenario})")
        finally:
            if child is not None and child.poll() is None:
                child.kill()
                child.wait(timeout=5)
            if master is not None:
                os.close(master)
            for pid_file in root.glob("*.pid"):
                try:
                    os.kill(int(pid_file.read_text()), signal.SIGKILL)
                except ProcessLookupError:
                    pass
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


for scenario in ["happy", "partial", "pending", "rejected", "disabled", "policy",
                 "optional-timeout", "required-timeout", "call-timeout", "call-error", "startup-error", "offline"]:
    run_fixture(scenario)
    print(f"{scenario}: passed", flush=True)
run_fixture("happy", terminal=True)
print("Raw terminal discovery, dispatch and bounded shutdown: passed")
run_fixture("cancel", terminal=True)
print("Raw terminal cancellation stops a call before its operation deadline: passed")
