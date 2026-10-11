"""R-160: synchronous children, inherited tools, isolated history and bounds."""

import http.server
import json
import pathlib
import subprocess
import sys
import tempfile
import threading


def call(name, arguments, index=0):
    return {
        "index": index, "id": f"call-{name}-{index}", "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def run_fixture(case):
    requests = []
    errors = []

    class Provider(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            self.reply(json.dumps({"data": [{"id": "parent-model"}]}).encode(),
                       "application/json")

        def reply(self, body, content_type):
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            try:
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                requests.append(request)
                last = request["messages"][-1]
                is_child = request["model"] == "child-model"
                if not is_child and last["role"] == "user":
                    assert any(tool["function"]["name"] == "subagent"
                               for tool in request["tools"]), "parent cannot delegate"
                    task = {"action": "run", "task": "Read fixture.txt", "model": "child-model",
                            "instructions": "CHILD_BRIEF_MARKER", "effort": "low"}
                    if case == "named":
                        task["agent"] = "unsupported-role"
                    delta = {"tool_calls": [call("subagent", task)]}
                elif is_child:
                    offered = {tool["function"]["name"] for tool in request["tools"]}
                    assert "read_file" in offered, offered
                    assert not {"write_file", "shell", "subagent", "ask_user_question"} & offered
                    assert "WORKSPACE_INSTRUCTION_MARKER" in json.dumps(request)
                    assert "CHILD_BRIEF_MARKER" in json.dumps(request)
                    assert "PARENT_PRIVATE_HISTORY" not in json.dumps(request), "child inherited parent history"
                    assert request["reasoning_effort"] == "low", request
                    if last["role"] == "user" or case == "cap":
                        calls = [call("read_file", {"path": "fixture.txt"})]
                        if case == "success":
                            calls += [
                                call("write_file", {"path": "forbidden.txt", "content": "BAD"}, 1),
                                call("shell", {"action": "run", "command": "touch shell-ran"}, 2),
                                call("subagent", {"action": "run", "task": "recurse"}, 3),
                                call("read_file", {"path": "../outside.txt"}, 4),
                            ]
                        delta = {"tool_calls": calls}
                    else:
                        results = [msg for msg in request["messages"] if msg["role"] == "tool"]
                        assert len(results) == 5, results
                        assert "ALLOWED_CHILD_READ" in results[0]["content"], results
                        for result in results[1:4]:
                            assert "there is no tool named" in result["content"], result
                        assert "OUTSIDE_SECRET_MARKER" not in results[4]["content"], results[4]
                        assert "outside" in results[4]["content"], results[4]
                        delta = {"content": "CHILD_READ_RESULT: ALLOWED_CHILD_READ"}
                else:
                    assert last["role"] == "tool", last
                    result = last["content"]
                    if case == "success":
                        assert "child child-1: CHILD_READ_RESULT: ALLOWED_CHILD_READ" in result, result
                    elif case == "cap":
                        assert "child stopped: max_model_turns" in result, result
                    else:
                        assert "named agents are unavailable" in result, result
                    delta = {"content": f"PARENT_RECEIVED_{case}"}
                finish = "tool_calls" if "tool_calls" in delta else "stop"
                body = "".join("data: " + json.dumps({"choices": [choice]}) + "\n\n"
                               for choice in [
                                   {"index": 0, "delta": delta, "finish_reason": None},
                                   {"index": 0, "delta": {}, "finish_reason": finish},
                               ]) + "data: [DONE]\n\n"
                self.reply(body.encode(), "text/event-stream")
            except Exception as error:
                errors.append(repr(error))
                self.send_error(500)

    with tempfile.TemporaryDirectory(prefix="rune-r160-") as directory:
        base = pathlib.Path(directory)
        root = base / "workspace"
        root.mkdir()
        (base / "outside.txt").write_text("OUTSIDE_SECRET_MARKER")
        (root / "fixture.txt").write_text("ALLOWED_CHILD_READ")
        (root / "AGENTS.md").write_text("WORKSPACE_INSTRUCTION_MARKER")
        config = base / "config" / "rune"
        config.mkdir(parents=True, mode=0o700)
        (config / "config.toml").write_text(
            "auto_upgrade = false\n[limits]\nmax_agent_steps = 2\nprovider_max_attempts = 1\n"
        )
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        environment = {
            "HOME": directory,
            "PATH": "/usr/bin:/bin",
            "XDG_CONFIG_HOME": str(base / "config"),
            "XDG_DATA_HOME": str(base / "data"),
            "XDG_STATE_HOME": str(base / "state"),
            "RUNE_HOME": str(base / "rune-state"),
            "RUNE_PROVIDER": "chat_completions",
            "RUNE_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
            "RUNE_MODEL": "parent-model",
            "RUNE_API_KEY_ENV": "RUNE_SUBAGENT_TEST_KEY",
            "RUNE_SUBAGENT_TEST_KEY": "fixture-key",
            "NO_COLOR": "1",
        }
        try:
            output = subprocess.run(
                [sys.argv[1], "--permission-mode", "full-access"],
                input=b"PARENT_PRIVATE_HISTORY\n/quit\n", stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, cwd=root, env=environment, timeout=20,
            )
            assert output.returncode == 0, output.stderr
            assert not errors, errors
            assert f"PARENT_RECEIVED_{case}".encode() in output.stdout, output.stdout
            assert len(requests) == (2 if case == "named" else 4), requests
            assert not (root / "forbidden.txt").exists(), "child wrote outside its allowlist"
            assert not (root / "shell-ran").exists(), "child executed shell"
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


for case in ["success", "cap", "named"]:
    run_fixture(case)
print("Child read reached parent; write, shell, nested delegation and outside-root read blocked; step cap and named-role refusal passed")
