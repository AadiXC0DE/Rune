import unittest

from service.client import Client
from service.server import start


class ServiceTests(unittest.TestCase):
    def setUp(self):
        self.server = start()
        self.client = Client("127.0.0.1", self.server.server_address[1])

    def tearDown(self):
        self.client.close()
        self.server.shutdown()
        self.server.server_close()

    def test_health(self):
        self.assertEqual(self.client.get("/health"), (200, {"status": "ok"}))

    def test_echo_ascii(self):
        status, body = self.client.post("/echo", {"msg": "hello"})
        self.assertEqual(status, 200)
        self.assertEqual(body["echo"], {"msg": "hello"})

    def test_echo_accented_text(self):
        status, body = self.client.post("/echo", {"msg": "café crème"})
        self.assertEqual(status, 200)
        self.assertEqual(body["echo"], {"msg": "café crème"})

    def test_unknown_path(self):
        status, _ = self.client.get("/nope")
        self.assertEqual(status, 404)


if __name__ == "__main__":
    unittest.main()
