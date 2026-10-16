"""R-161: discovered names, searchable locations, exact instructions and bounds."""

import html
import http.server
import json
import pathlib
import subprocess
import sys
import tempfile
import threading


def assert_skill_location(location, expected):
    assert location.startswith("skill:"), location
    # Filesystem identity also handles Windows short names and drive-letter case.
    assert pathlib.Path(location.removeprefix("skill:")).samefile(expected), location


def run_fixture(ask):
    requests = []
    errors = []
    instructions = "---\nname: release-guide\ndescription: Ships the service\n---\n  Preserve indentation.\nUse café.\n\n"

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
            self.reply(b'{"data": [{"id": "fixture-model"}]}', "application/json")

        def do_POST(self):
            try:
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                requests.append(request)
                step = len(requests)
                last = request["messages"][-1]
                if step == 1:
                    offered = {tool["function"]["name"] for tool in request["tools"]}
                    assert {"capability_search", "skill"} <= offered, offered
                    assert "release-guide" in request["messages"][0]["content"]
                    assert "Preserve indentation." not in json.dumps(request)
                    name, arguments = "capability_search", {"query": "ships service"}
                elif step == 2:
                    assert last["role"] == "tool", last
                    lines = last["content"].split("\n")
                    assert len(lines) == 3, last
                    assert lines[0] == "release-guide", last
                    assert lines[1].startswith("  skill:"), last
                    assert lines[2] == "  Ships the service", last
                    location = lines[1].removeprefix("  ")
                    assert_skill_location(location, skill_file)
                    name, arguments = "skill", {"location": location}
                elif step == 3:
                    assert last["role"] == "tool", last
                    assert last["content"] == f'<skill_content name="release-guide">\n{instructions}\n</skill_content>\n', last
                    # The tool asks for the advertised location, not a path
                    # reconstructed in the parent process's spelling.
                    catalog = request["messages"][0]["content"].splitlines()
                    entry, = [line for line in catalog if line.startswith("- oversized: ")]
                    location = html.unescape(entry.rsplit(" (location: ", 1)[1].removesuffix(")"))
                    assert_skill_location(location, oversized_file)
                    name, arguments = "skill", {"location": location}
                else:
                    assert step == 4, requests
                    assert last["role"] == "tool", last
                    assert "too_large" in last["content"], last
                    assert "skill_file_bytes" in last["content"], last
                    assert "256" in last["content"], last
                    assert "OVERSIZED_BODY" not in last["content"], last
                if step < 4:
                    delta = {"tool_calls": [{
                        "index": 0, "id": f"skill-call-{step}", "type": "function",
                        "function": {"name": name, "arguments": json.dumps(arguments)},
                    }]}
                else:
                    delta = {"content": "SKILL_FIXTURE_PASSED"}
                finish = "tool_calls" if step < 4 else "stop"
                body = "".join("data: " + json.dumps({"choices": [choice]}) + "\n\n"
                               for choice in [
                                   {"index": 0, "delta": delta, "finish_reason": None},
                                   {"index": 0, "delta": {}, "finish_reason": finish},
                               ]) + "data: [DONE]\n\n"
                self.reply(body.encode(), "text/event-stream")
            except Exception as error:
                errors.append(repr(error))
                self.send_error(500)

    with tempfile.TemporaryDirectory(prefix="rune-r161-") as directory:
        base = pathlib.Path(directory)
        root = base / "workspace"
        skill_file = root / ".agents" / "skills" / "opaque-directory" / "SKILL.md"
        skill_file.parent.mkdir(parents=True)
        # Exact loading must compare against the bytes actually written, without
        # Python's platform newline translation changing the fixture's input.
        skill_file.write_bytes(instructions.encode("utf-8"))
        oversized_file = root / "skills" / "oversized" / "SKILL.md"
        oversized_file.parent.mkdir(parents=True)
        oversized_file.write_text("---\nname: oversized\n---\n" + "OVERSIZED_BODY" * 40)
        config = base / "config" / "rune"
        config.mkdir(parents=True, mode=0o700)
        (config / "config.toml").write_text(
            "auto_upgrade = false\n[limits]\nmax_agent_steps = 8\n"
            "provider_max_attempts = 1\nskill_file_bytes = 256\n"
        )
        (config / "config.toml").chmod(0o600)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        environment = child_environment({
            "HOME": directory, "PATH": "/usr/bin:/bin", "NO_COLOR": "1",
            "XDG_CONFIG_HOME": str(base / "config"),
            "XDG_DATA_HOME": str(base / "data"),
            "XDG_STATE_HOME": str(base / "state"),
            "RUNE_HOME": str(base / "rune-state"),
            "RUNE_PROVIDER": "chat_completions", "RUNE_MODEL": "fixture-model",
            "RUNE_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
            "RUNE_API_KEY_ENV": "RUNE_SKILL_TEST_KEY", "RUNE_SKILL_TEST_KEY": "fixture-key",
        })
        try:
            args = [sys.argv[1], "--permission-mode", "full-access"]
            args += ["ask", "--json", "load the fixture skill"] if ask else ["--accessible"]
            output = subprocess.run(
                args, input=b"load the fixture skill\n/quit\n" if not ask else b"",
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                cwd=root, env=environment, timeout=20,
            )
            assert output.returncode == 0, (output.stdout, output.stderr, errors)
            assert not errors, errors
            assert b"SKILL_FIXTURE_PASSED" in output.stdout, output.stdout
            assert len(requests) == 4, requests
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


for ask in [False, True]:
    run_fixture(ask)
print("Interactive and ask skill search, exact loading and configured size refusal passed")
