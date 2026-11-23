import json
import socket
import unittest

from service.client import Client
from service.server import start


class HiddenTests(unittest.TestCase):
    def setUp(self):
        self.server = start()
        self.port = self.server.server_address[1]
        self.client = Client("127.0.0.1", self.port)

    def tearDown(self):
        self.client.close()
        self.server.shutdown()
        self.server.server_close()

    def test_emoji_body(self):
        payload = {"msg": "ship it \U0001f680\U0001f680", "n": 3}
        status, body = self.client.post("/echo", payload)
        self.assertEqual(status, 200)
        self.assertEqual(body["echo"], payload)
        self.assertEqual(body["bytes"], len(json.dumps(payload, ensure_ascii=False).encode("utf-8")))

    def test_empty_body(self):
        status, body = self.client.post("/echo")
        self.assertEqual(status, 200)
        self.assertEqual(body, {"echo": None, "bytes": 0})

    def test_two_requests_on_one_connection(self):
        first = self.client.post("/echo", {"a": "üñî"})
        second = self.client.post("/echo", {"b": "你好"})
        third = self.client.get("/health")
        self.assertEqual(first[1]["echo"], {"a": "üñî"})
        self.assertEqual(second[1]["echo"], {"b": "你好"})
        self.assertEqual(third, (200, {"status": "ok"}))

    def test_large_mixed_body(self):
        payload = {"text": ("abcé你\U0001f600" * 5000)}
        status, body = self.client.post("/echo", payload)
        self.assertEqual(status, 200)
        self.assertEqual(body["echo"], payload)

    def test_invalid_json_is_a_400(self):
        conn = socket.create_connection(("127.0.0.1", self.port), timeout=5)
        try:
            raw = b"{not json \xc3\xa9"
            conn.sendall(
                b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: %d\r\n\r\n" % len(raw) + raw
            )
            data = b""
            while b"\r\n\r\n" not in data:
                data += conn.recv(4096)
        finally:
            conn.close()
        self.assertTrue(data.startswith(b"HTTP/1.1 400"))

    def test_raw_content_length_matches_bytes(self):
        conn = socket.create_connection(("127.0.0.1", self.port), timeout=5)
        try:
            raw = json.dumps({"s": "\U0001f600"}, ensure_ascii=False).encode("utf-8")
            conn.sendall(
                b"POST /echo HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: %d\r\n\r\n"
                % len(raw)
                + raw
            )
            data = b""
            while True:
                chunk = conn.recv(4096)
                if not chunk:
                    break
                data += chunk
        finally:
            conn.close()
        head, _, body = data.partition(b"\r\n\r\n")
        length = [l for l in head.split(b"\r\n") if l.lower().startswith(b"content-length")][0]
        self.assertEqual(int(length.split(b":")[1]), len(body))


if __name__ == "__main__":
    unittest.main()
