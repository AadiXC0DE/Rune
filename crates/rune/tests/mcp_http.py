"""R-164: real interactive CLI against isolated model and HTTP MCP servers."""

import fcntl
import http.client
import http.server
import json
import os
import pathlib
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


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def run_fixture(scenario, terminal=False):
    model_requests, mcp_requests, http_requests, errors = [], [], [], []
    unavailable = scenario in {"pending", "rejected", "disabled", "startup-error"}

    class Server(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def reply(self, body, content_type="application/json", status=200):
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            http_requests.append(("GET", self.path))
            if self.path == "/v1/models":
                self.reply(b'{"data": [{"id": "mcp-fixture"}]}')
            else:
                errors.append("unexpected GET: " + self.path)
                self.send_error(404)

        def do_POST(self):
            http_requests.append(("POST", self.path))
            try:
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if self.path == "/mcp":
                    mcp_requests.append(request)
                    assert self.headers["x-fixture-secret"] == "http-only-secret", self.headers
                    assert self.headers.get("Authorization") is None, self.headers
                    method = request["method"]
                    if scenario == "startup-error":
                        response = {"error": {"code": -32603, "message": "http-only-secret"}}
                    elif method == "initialize":
                        response = {"result": {"protocolVersion": request["params"]["protocolVersion"],
                                               "capabilities": {"tools": {}}}}
                    elif method == "notifications/initialized":
                        self.reply(b"", status=202)
                        return
                    elif method == "tools/list":
                        response = {"result": {"tools": [{"name": "echo", "description": "HTTP fixture echo",
                                                          "inputSchema": SCHEMA}]}}
                    else:
                        assert method == "tools/call", request
                        assert request["params"] == {"name": "echo", "arguments": {"text": "hello HTTP"}}
                        response = {"result": {"content": [{"type": "text", "text": "HTTP: hello HTTP"}]}}
                    self.reply(json.dumps({"jsonrpc": "2.0", "id": request["id"], **response}).encode())
                    return
                assert self.path == "/v1/chat/completions", self.path
                model_requests.append(request)
                tools = {tool["function"]["name"]: tool["function"] for tool in request["tools"]}
                assert not any(name.startswith("mcp_untrusted_") for name in tools), tools
                step = len(model_requests)
                if unavailable:
                    assert "mcp_remote_echo" not in tools, tools
                    assert step == 1, model_requests
                else:
                    tool = tools["mcp_remote_echo"]
                    assert tool["parameters"] == SCHEMA, tool
                    assert tool["description"] == "HTTP fixture echo", tool
                    if step == 2:
                        result = request["messages"][-1]
                        assert result["role"] == "tool", result
                        if scenario == "policy":
                            assert "denied" in result["content"].lower(), result
                        else:
                            assert result["content"] == "HTTP: hello HTTP", result
                    else:
                        assert step == 1, model_requests
                call = not unavailable and step == 1
                delta = {"tool_calls": [{
                    "index": 0, "id": "http-call", "type": "function",
                    "function": {"name": "mcp_remote_echo", "arguments": '{"text":"hello HTTP"}'},
                }]} if call else {"content": "HTTP_MCP_PASSED"}
                body = "".join("data: " + json.dumps({"choices": [choice]}) + "\n\n"
                               for choice in [
                                   {"index": 0, "delta": delta, "finish_reason": None},
                                   {"index": 0, "delta": {}, "finish_reason": "tool_calls" if call else "stop"},
                               ]) + "data: [DONE]\n\n"
                self.reply(body.encode(), "text/event-stream")
            except Exception as error:
                errors.append(repr(error))
                self.send_error(500)

    with tempfile.TemporaryDirectory(prefix="rune-r164-") as directory:
        base = pathlib.Path(directory).resolve()
        root = base / "workspace"
        root.mkdir()
        (base / "state").mkdir(mode=0o700)
        config = base / "config" / "rune"
        config.mkdir(parents=True, mode=0o700)
        (config / "config.toml").write_text(
            "auto_upgrade = false\n[limits]\nmax_agent_steps = 4\nprovider_max_attempts = 1\n"
            # Generous, because these bound the fixture's own handshakes rather than testing a
            # timeout. At one second the MCP initialize handshake exceeded the deadline whenever
            # the full workspace suite was loading the machine, which dropped the connection and
            # failed the run for reasons that had nothing to do with the branch.
            "mcp_startup_timeout_ms = 30000\nmcp_operation_timeout_ms = 30000\n"
        )
        (config / "config.toml").chmod(0o600)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Server)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        # Read the bound address back from the listening socket and connect to
        # exactly that, so the bind and every client agree on one address on every
        # platform instead of one resolving a name the other never bound.
        host, port = server.server_address[:2]
        # Assert the wire contract that prevents reuse before Python closes the socket.
        probe = http.client.HTTPConnection(host, port, timeout=5)
        probe.request("GET", "/v1/models")
        response = probe.getresponse()
        assert response.getheader("Connection") == "close"
        response.read()
        probe.close()
        http_requests.clear()

        environment = child_environment({
            "HOME": directory, "PATH": "/usr/bin:/bin", "NO_COLOR": "1", "TERM": "xterm-256color",
            "XDG_CONFIG_HOME": str(base / "config"), "XDG_DATA_HOME": str(base / "data"),
            "XDG_STATE_HOME": str(base / "xdg-state"), "RUNE_HOME": str(base / "state"),
            "RUNE_PROVIDER": "chat_completions", "RUNE_MODEL": "mcp-fixture",
            "RUNE_BASE_URL": f"http://{host}:{port}/v1",
            "RUNE_API_KEY_ENV": "RUNE_HTTP_MODEL_KEY", "RUNE_HTTP_MODEL_KEY": "provider-only-secret",
            "RUNE_HTTP_MCP_SECRET": "http-only-secret",
        })
        entries = {"remote": {
            "type": "http", "url": f"http://{host}:{port}/mcp",
            "headers": {"x-fixture-secret": "${RUNE_HTTP_MCP_SECRET}"},
        }}
        if scenario == "disabled":
            entries["remote"]["enabled"] = False
        # Filtering must precede expansion, even for a required declaration.
        if scenario in {"pending", "rejected", "disabled", "offline"}:
            entries["remote"]["required"] = True
            entries["remote"]["headers"]["x-unused"] = "${UNSET_BLOCKED_SECRET}"
        if scenario == "partial":
            entries["untrusted"] = {
                "url": f"http://{host}:{port}/untrusted",
                "headers": {"x-unused": "${UNSET_UNTRUSTED_SECRET}"}, "required": True,
            }
        if scenario == "implicit-http":
            del entries["remote"]["type"]
        (root / ".mcp.json").write_text(json.dumps({"mcpServers": entries}))

        def command(*args, **options):
            return subprocess.run([sys.argv[1], *args], cwd=root, env=environment,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15, **options)

        child, master = None, None
        transcript = bytearray()
        try:
            if scenario != "pending":
                action = "reject" if scenario == "rejected" else "approve"
                approval = command("projects", action)
                assert approval.returncode == 0, approval
            if scenario == "partial":
                trust_file = next((base / "state").glob("*trust*"))
                trust = json.loads(trust_file.read_text())
                trust["entries"][0]["entries"] = ["server:remote"]
                trust_file.write_text(json.dumps(trust))
            args = ["--permission-mode", "ask" if scenario == "policy" else "full-access"]
            if scenario == "offline":
                args.append("--offline")
            if terminal:
                master, slave = open_terminal()
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
                try:
                    child = subprocess.Popen([sys.argv[1], *args], stdin=slave, stdout=slave, stderr=slave,
                                             cwd=root, env=environment, preexec_fn=controlling_terminal)
                finally:
                    os.close(slave)

                def pump(timeout):
                    return _pump(master, transcript, timeout)

                def wait_for(marker, timeout=30):
                    deadline = time.monotonic() + timeout
                    while marker not in transcript:
                        assert child.poll() is None, (
                            f"session exited (status {child.returncode}) before printing {marker!r}:\n"
                            + transcript.decode(errors="replace")
                        )
                        remaining = deadline - time.monotonic()
                        assert remaining > 0, (
                            f"waited {timeout}s for the terminal to print {marker!r}:\n"
                            + transcript.decode(errors="replace")
                        )
                        pump(min(remaining, 0.1))

                wait_for(b"ctrl-c cancel")
                os.write(master, b"echo through HTTP MCP\r")
                # Wait for the committed reply, not the streamed delta. While a
                # turn is running the keyboard feeds the steering queue, so an
                # Enter sent on the first sight of the text is swallowed by the
                # turn and never reaches command handling; the \r\n appears only
                # once report_turn has committed the finished reply.
                wait_for(b"HTTP_MCP_PASSED\r\n")
                os.write(master, b"/quit\r")
                assert wait_for_exit(child, master, transcript, 30) == 0, (
                    f"session exited with status {child.returncode}:\n"
                    + transcript.decode(errors="replace")
                )
                stdout, stderr = bytes(transcript), b""
            else:
                prompt = b"echo through HTTP MCP\n" + (b"no\n" if scenario == "policy" else b"")
                output = command(*args, "--accessible",
                                 input=(b"" if scenario == "offline" else prompt) + b"/quit\n")
                stdout, stderr = output.stdout, output.stderr
                assert output.returncode == 0, (stdout, stderr, errors)
            assert not errors, errors
            assert b"http-only-secret" not in stdout + stderr, (stdout, stderr)
            assert b"MCP server `untrusted`" not in stdout, stdout
            if scenario == "offline":
                assert not http_requests, http_requests
                assert b"session " in stdout, stdout
            else:
                assert b"HTTP_MCP_PASSED" in stdout, (stdout, stderr)
                assert len(model_requests) == (1 if unavailable else 2), model_requests
            methods = [request["method"] for request in mcp_requests]
            if scenario in {"pending", "rejected", "disabled", "offline"}:
                assert not methods, methods
                assert b"MCP server `remote`" not in stdout, stdout
            elif scenario == "startup-error":
                assert methods == ["initialize"], methods
                assert b"MCP server `remote`" in stdout and b"<redacted>" in stdout, stdout
            else:
                expected = ["initialize", "notifications/initialized", "tools/list"]
                if scenario != "policy":
                    expected.append("tools/call")
                assert methods == expected, methods
        finally:
            if child is not None:
                if child.poll() is None:
                    child.kill()
                wait_for_exit(child, master, transcript, 5)
            if master is not None:
                close_terminal(master)
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


for scenario in ["happy", "implicit-http", "partial", "pending", "rejected", "disabled",
                 "offline", "policy", "startup-error"]:
    run_fixture(scenario)
    print(f"{scenario}: passed", flush=True)
run_fixture("happy", terminal=True)
print("Raw terminal HTTP MCP discovery and dispatch: passed")
