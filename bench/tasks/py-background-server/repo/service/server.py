"""A small JSON echo server.

Run it by hand with: python3 -m service.server --port 8099
"""
import argparse
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        pass

    def send_json(self, status, payload):
        body = json.dumps(payload, ensure_ascii=False)
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body.encode("utf-8"))

    def do_GET(self):
        if self.path == "/health":
            self.send_json(200, {"status": "ok"})
        else:
            self.send_json(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length)
        if self.path != "/echo":
            self.send_json(404, {"error": "not found"})
            return
        try:
            payload = json.loads(raw.decode("utf-8")) if raw else None
        except ValueError:
            self.send_json(400, {"error": "invalid json"})
            return
        self.send_json(200, {"echo": payload, "bytes": len(raw)})


def start(host="127.0.0.1", port=0):
    """Start the server on a background thread and return it. Port 0 picks a free port."""
    server = ThreadingHTTPServer((host, port), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8099)
    args = parser.parse_args()
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print("listening on %s:%d" % server.server_address, flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
