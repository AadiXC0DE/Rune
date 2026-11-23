"""OAuth-protected HTTP MCP fixture. Prints its port, serves until stdin closes.

Mode `oauth` serves resource and authorization server metadata, registration,
an authorization endpoint that redirects at once to the loopback callback, a
token endpoint and an MCP endpoint that accepts only the current token. POST
/expire invalidates every token (a body of `reject` also refuses refreshes);
GET /stats reports registrations and refreshes. Mode `sse` answers like a
server that speaks only the legacy SSE transport.
"""

import http.server
import json
import sys
import threading
import urllib.parse

MODE = sys.argv[1] if len(sys.argv) > 1 else "oauth"
STATE = {"registrations": 0, "refreshes": 0, "reject_refresh": False, "valid": set()}


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def send(self, status, body=b"", content_type="application/json", headers=()):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        for name, value in headers:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    def json(self, value, status=200):
        self.send(status, json.dumps(value).encode())

    def base(self):
        return "http://" + self.headers["Host"]

    def body(self):
        return self.rfile.read(int(self.headers.get("Content-Length", "0")))

    def do_GET(self):
        url = urllib.parse.urlsplit(self.path)
        if MODE == "sse":
            if url.path == "/mcp" and "text/event-stream" in self.headers.get("Accept", ""):
                return self.send(200, b": legacy\n\n", "text/event-stream")
            return self.send(404)
        if url.path == "/.well-known/oauth-protected-resource/mcp":
            return self.json({"resource": self.base() + "/mcp",
                              "authorization_servers": [self.base()]})
        if url.path == "/.well-known/oauth-authorization-server":
            base = self.base()
            return self.json({"issuer": base, "authorization_endpoint": base + "/authorize",
                              "token_endpoint": base + "/token",
                              "registration_endpoint": base + "/register"})
        if url.path == "/authorize":
            query = dict(urllib.parse.parse_qsl(url.query))
            assert query["client_id"] == "registered-client", query
            assert query["code_challenge_method"] == "S256", query
            target = query["redirect_uri"] + "?" + urllib.parse.urlencode(
                {"code": "the-code", "state": query["state"]})
            return self.send(302, headers=[("Location", target)])
        if url.path == "/stats":
            return self.json({"registrations": STATE["registrations"],
                              "refreshes": STATE["refreshes"]})
        return self.send(404)

    def do_POST(self):
        url = urllib.parse.urlsplit(self.path)
        data = self.body()
        if MODE == "sse":
            return self.send(405)
        if url.path == "/register":
            request = json.loads(data)
            assert request["token_endpoint_auth_method"] == "none", request
            assert request["grant_types"] == ["authorization_code", "refresh_token"], request
            STATE["registrations"] += 1
            return self.json({"client_id": "registered-client"}, 201)
        if url.path == "/token":
            form = dict(urllib.parse.parse_qsl(data.decode()))
            if form["grant_type"] == "authorization_code":
                assert form["code"] == "the-code" and form["code_verifier"], form
                STATE["valid"] = {"access-1"}
                return self.json({"access_token": "access-1", "token_type": "Bearer",
                                  "refresh_token": "refresh-1"})
            assert form["grant_type"] == "refresh_token", form
            STATE["refreshes"] += 1
            if STATE["reject_refresh"] or form["refresh_token"] != "refresh-1":
                return self.json({"error": "invalid_grant"}, 400)
            STATE["valid"] = {"access-2"}
            return self.json({"access_token": "access-2", "token_type": "Bearer"})
        if url.path == "/expire":
            STATE["valid"] = set()
            STATE["reject_refresh"] = data == b"reject"
            return self.send(204)
        if url.path == "/mcp":
            authorization = self.headers.get("Authorization", "")
            token = authorization[len("Bearer "):] if authorization.startswith("Bearer ") else None
            if token not in STATE["valid"]:
                metadata = self.base() + "/.well-known/oauth-protected-resource/mcp"
                return self.send(401, headers=[(
                    "WWW-Authenticate", f'Bearer resource_metadata="{metadata}"')])
            request = json.loads(data)
            if "id" not in request:
                return self.send(202)
            method = request["method"]
            if method == "initialize":
                result = {"protocolVersion": request["params"]["protocolVersion"],
                          "capabilities": {"tools": {}}}
            elif method == "tools/list":
                result = {"tools": [{"name": "echo", "description": "Echo",
                                     "inputSchema": {"type": "object", "properties": {}}}]}
            else:
                result = {"content": [{"type": "text", "text": "echo from " + token}]}
            return self.json({"jsonrpc": "2.0", "id": request["id"], "result": result})
        return self.send(404)


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
print(server.server_address[1], flush=True)
sys.stdin.read()
server.shutdown()
