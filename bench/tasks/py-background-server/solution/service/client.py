import http.client
import json


class Client:
    """Talks JSON to the echo server over one persistent connection."""

    def __init__(self, host, port, timeout=5):
        self.conn = http.client.HTTPConnection(host, port, timeout=timeout)

    def get(self, path):
        self.conn.request("GET", path)
        return self._read()

    def post(self, path, payload=None):
        body = b"" if payload is None else json.dumps(payload, ensure_ascii=False).encode("utf-8")
        headers = {"Content-Type": "application/json; charset=utf-8"}
        self.conn.request("POST", path, body=body, headers=headers)
        return self._read()

    def _read(self):
        response = self.conn.getresponse()
        data = response.read()
        return response.status, json.loads(data.decode("utf-8"))

    def close(self):
        self.conn.close()
