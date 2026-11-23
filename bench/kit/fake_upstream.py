#!/usr/bin/env python3
"""A fake model API for testing the benchmark kit offline.

Serves chat completions, Anthropic messages and Responses streams with fixed usage numbers.
"""

import argparse
import json
import signal
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CHAT_USAGE = {
    "prompt_tokens": 100,
    "completion_tokens": 7,
    "prompt_tokens_details": {"cached_tokens": 40},
    "completion_tokens_details": {"reasoning_tokens": 3},
}
RESPONSES_USAGE = {
    "input_tokens": 100,
    "input_tokens_details": {"cached_tokens": 60},
    "output_tokens": 9,
    "output_tokens_details": {"reasoning_tokens": 4},
}


def _sse(events):
    out = b""
    for name, obj in events:
        data = obj if isinstance(obj, str) else json.dumps(obj, separators=(",", ":"))
        head = "event: %s\n" % name if name else ""
        out += ("%sdata: %s\n\n" % (head, data)).encode()
    return out


def _chat_stream(reply, include_usage):
    base = {"id": "fake-1", "object": "chat.completion.chunk", "model": "fake"}

    def chunk(delta, finish=None):
        c = dict(base)
        c["choices"] = [{"index": 0, "delta": delta, "finish_reason": finish}]
        return (None, c)

    events = [
        chunk({"role": "assistant"}),
        chunk({"content": reply}),
        chunk({}, "stop"),
    ]
    if include_usage:
        u = dict(base)
        u["choices"] = []
        u["usage"] = CHAT_USAGE
        events.append((None, u))
    events.append((None, "[DONE]"))
    return _sse(events)


def _anthropic_stream(reply):
    start_usage = {
        "input_tokens": 10,
        "cache_read_input_tokens": 80,
        "cache_creation_input_tokens": 5,
        "output_tokens": 1,
    }
    return _sse(
        [
            (
                "message_start",
                {
                    "type": "message_start",
                    "message": {"id": "fake-1", "role": "assistant", "content": [], "usage": start_usage},
                },
            ),
            (
                "content_block_start",
                {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
            ),
            (
                "content_block_delta",
                {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": reply},
                },
            ),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            (
                "message_delta",
                {
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 7},
                },
            ),
            ("message_stop", {"type": "message_stop"}),
        ]
    )


def _responses_stream(reply):
    return _sse(
        [
            ("response.created", {"type": "response.created", "response": {"id": "fake-1"}}),
            (
                "response.output_text.delta",
                {"type": "response.output_text.delta", "delta": reply},
            ),
            (
                "response.completed",
                {
                    "type": "response.completed",
                    "response": {"id": "fake-1", "status": "completed", "usage": RESPONSES_USAGE},
                },
            ),
        ]
    )


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _send(self, status, body, ctype):
        self.send_response(status)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(body)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def _json(self, status, obj):
        self._send(status, json.dumps(obj, separators=(",", ":")).encode(), "application/json")

    def _read_body(self):
        n = int(self.headers.get("content-length") or 0)
        return self.rfile.read(n) if n else b""

    def _fail_if_asked(self):
        status = self.headers.get("x-fake-status")
        if status is None:
            return False
        self._json(int(status), {"error": {"message": "failure for key sk-fakefakefake1234"}})
        return True

    def do_GET(self):
        if self.path == "/_last":
            self._json(200, self.server.last or {})
            return
        if self._fail_if_asked():
            return
        self._json(200, {"data": [{"id": "fake"}]})

    def do_POST(self):
        body = self._read_body()
        self.server.last = {
            "path": self.path,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "body": body.decode("utf-8", "replace"),
        }
        if self._fail_if_asked():
            return
        path = self.path.split("?", 1)[0]
        reply = self.server.reply
        try:
            req = json.loads(body)
        except ValueError:
            req = {}
        if not isinstance(req, dict):
            req = {}
        if path.endswith("chat/completions"):
            if req.get("stream") is True:
                opts = req.get("stream_options")
                include = isinstance(opts, dict) and opts.get("include_usage") is True
                self._send(200, _chat_stream(reply, include), "text/event-stream")
            else:
                self._json(
                    200,
                    {
                        "id": "fake-1",
                        "object": "chat.completion",
                        "model": "fake",
                        "choices": [
                            {
                                "index": 0,
                                "message": {"role": "assistant", "content": reply},
                                "finish_reason": "stop",
                            }
                        ],
                        "usage": CHAT_USAGE,
                    },
                )
        elif path.endswith("/messages"):
            self._send(200, _anthropic_stream(reply), "text/event-stream")
        elif path.endswith("/responses"):
            self._send(200, _responses_stream(reply), "text/event-stream")
        else:
            self._json(404, {"error": {"message": "unknown path"}})


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def serve(port=0, reply="Nothing to change."):
    server = Server(("127.0.0.1", port), Handler)
    server.reply = reply
    server.last = None
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
    thread.start()
    return server, thread


def main():
    parser = argparse.ArgumentParser(description="Fake model API for offline kit tests.")
    parser.add_argument("--reply", default="Nothing to change.")
    args = parser.parse_args()
    server, _thread = serve(0, args.reply)
    print(json.dumps({"port": server.server_address[1]}), flush=True)
    done = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: done.set())
    signal.signal(signal.SIGINT, lambda *_: done.set())
    done.wait()
    threading.Thread(target=server.shutdown).start()
    return 0


if __name__ == "__main__":
    sys.exit(main())
