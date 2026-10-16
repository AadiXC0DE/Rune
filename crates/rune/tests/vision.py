"""R-162: configured image upload, returned analysis, unavailable and bounded paths."""

import base64
import http.server
import json
import pathlib
import subprocess
import sys
import tempfile
import threading

PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aD1sAAAAASUVORK5CYII=")


def run_fixture(ask, mode):
    requests, images, errors = [], [], []
    expected_error = {
        "absent": "no image analysis is configured",
        "outside": "path_outside_workspace",
        "oversized": "limit is 8388608",
        "attached": "session image IDs are unavailable",
        "incomplete": "did not complete",
    }.get(mode)

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
                assert self.headers["Authorization"] == "Bearer fixture-key"
                if request["model"] == "vision-fixture":
                    assert not request.get("tools"), request
                    user = next(message for message in request["messages"] if message["role"] == "user")
                    parts = [part for part in user["content"] if part["type"] == "image_url"]
                    assert len(parts) == 1, parts
                    url = parts[0]["image_url"]["url"]
                    assert url.startswith("data:image/png;base64,"), url
                    assert base64.b64decode(url.split(",", 1)[1]) == PNG
                    images.append(request)
                    delta = {"content": "VISION_ANALYSIS_RESULT: a white pixel. " + ("observation " * 100 if mode == "bounded" else "")}
                    finish = "length" if mode == "incomplete" else "stop"
                else:
                    requests.append(request)
                    if len(requests) == 1:
                        vision = next(tool["function"] for tool in request["tools"] if tool["function"]["name"] == "vision")
                        assert ("Unavailable:" in vision["description"]) == (mode == "absent"), vision
                        arguments = {"paths": ["pixel.png"]}
                        if mode == "outside":
                            arguments = {"paths": ["../outside.png"]}
                        elif mode == "oversized":
                            arguments = {"paths": ["pixel.png", "oversized.png"]}
                        elif mode == "attached":
                            arguments = {"image_ids": [1]}
                        delta = {"tool_calls": [{
                            "index": 0, "id": "vision-call", "type": "function",
                            "function": {"name": "vision", "arguments": json.dumps(arguments)},
                        }]}
                        finish = "tool_calls"
                    else:
                        assert len(requests) == 2, requests
                        last = request["messages"][-1]
                        assert last["role"] == "tool", last
                        if expected_error:
                            assert expected_error in last["content"], last
                        else:
                            assert "VISION_ANALYSIS_RESULT: a white pixel." in last["content"], last
                            if mode == "bounded":
                                assert "analysis truncated" in last["content"], last
                                assert len(last["content"]) < 512, last
                        delta, finish = {"content": "VISION_FIXTURE_PASSED"}, "stop"
                body = "".join("data: " + json.dumps({"choices": [choice]}) + "\n\n" for choice in [
                    {"index": 0, "delta": delta, "finish_reason": None},
                    {"index": 0, "delta": {}, "finish_reason": finish},
                ]) + "data: [DONE]\n\n"
                self.reply(body.encode(), "text/event-stream")
            except Exception as error:
                errors.append(repr(error))
                self.send_error(500)

    with tempfile.TemporaryDirectory(prefix="rune-r162-") as directory:
        base = pathlib.Path(directory)
        root = base / "workspace"
        root.mkdir()
        (root / "pixel.png").write_bytes(PNG)
        (base / "outside.png").write_bytes(PNG)
        with (root / "oversized.png").open("wb") as image:
            image.write(PNG)
            image.truncate(8 * 1024 * 1024 + 1)
        config = base / "config" / "rune"
        config.mkdir(parents=True, mode=0o700)
        (config / "config.toml").write_text(
            "auto_upgrade = false\n" + ("vision_model = 'vision-fixture'\n" if mode != "absent" else "") +
            "[limits]\nmax_agent_steps = 4\nprovider_max_attempts = 1\nimage_adapter_output_bytes = 256\n"
        )
        (config / "config.toml").chmod(0o600)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        environment = child_environment({
            "HOME": directory, "PATH": "/usr/bin:/bin", "NO_COLOR": "1",
            "XDG_CONFIG_HOME": str(base / "config"), "XDG_DATA_HOME": str(base / "data"),
            "XDG_STATE_HOME": str(base / "state"), "RUNE_HOME": str(base / "rune-state"),
            "RUNE_PROVIDER": "chat_completions", "RUNE_MODEL": "fixture-model",
            "RUNE_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
            "RUNE_API_KEY_ENV": "RUNE_VISION_TEST_KEY", "RUNE_VISION_TEST_KEY": "fixture-key",
        })
        try:
            args = [sys.argv[1], "--permission-mode", "full-access"]
            args += ["ask", "--json", "describe the image"] if ask else ["--accessible"]
            output = subprocess.run(
                args, input=b"describe the image\n/quit\n" if not ask else b"",
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                cwd=root, env=environment, timeout=20,
            )
            assert output.returncode == 0, (mode, output.stdout, output.stderr, errors)
            assert not errors, (mode, errors)
            assert b"VISION_FIXTURE_PASSED" in output.stdout, output.stdout
            assert len(requests) == 2, requests
            assert len(images) == (1 if mode in ["configured", "bounded", "incomplete"] else 0), (mode, images)
            if ask:
                result = json.loads(output.stdout)
                assert result["tool_calls"] == [{"name": "vision", "status": "error" if expected_error else "success"}], result
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


for ask in [False, True]:
    for mode in ["configured", "absent", "outside", "oversized", "attached", "incomplete", "bounded"]:
        run_fixture(ask, mode)
print("Interactive and ask vision upload, results, unavailable descriptions and image bounds passed")
