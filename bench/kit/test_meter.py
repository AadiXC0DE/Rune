import http.client
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import fake_upstream  # noqa: E402
import meter  # noqa: E402

HERE = Path(__file__).resolve().parent
METER = str(HERE / "meter.py")


class MeterCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.log = self.dir / "meter.jsonl"
        self.fake, _ = fake_upstream.serve()
        self.fake_port = self.fake.server_address[1]
        self.proc = None

    def tearDown(self):
        if self.proc is not None:
            self.proc.terminate()
            self.proc.wait(timeout=10)
            self.proc.stdout.close()
        self.fake.shutdown()
        self.fake.server_close()
        self.tmp.cleanup()

    def start(self, *extra, env=None, anthropic=True):
        url = "http://127.0.0.1:%d" % self.fake_port
        cmd = [sys.executable, METER, "--upstream", url, "--log", str(self.log)]
        if anthropic:
            cmd += ["--upstream-anthropic", url]
        cmd += list(extra)
        full_env = dict(os.environ)
        full_env.update(env or {})
        self.proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, text=True, env=full_env)
        self.port = json.loads(self.proc.stdout.readline())["port"]

    def request(self, port, method, path, body=None, headers=None):
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=20)
        data = None if body is None else (body if isinstance(body, bytes) else json.dumps(body).encode())
        hdrs = {"content-type": "application/json"}
        hdrs.update(headers or {})
        conn.request(method, path, body=data, headers=hdrs)
        resp = conn.getresponse()
        payload = resp.read()
        conn.close()
        return resp.status, payload

    def via_meter(self, *args, **kwargs):
        return self.request(self.port, *args, **kwargs)

    def direct(self, *args, **kwargs):
        return self.request(self.fake_port, *args, **kwargs)

    def rows(self):
        text = self.log.read_text(encoding="utf-8")
        return [json.loads(line) for line in text.splitlines()]

    def last(self):
        return json.loads(self.direct("GET", "/_last")[1])


CHAT = {"model": "deepseek-flash", "stream": True, "messages": [{"role": "user", "content": "hi"}]}


class MeterTests(MeterCase):
    def test_streamed_bytes_reach_the_client_unchanged(self):
        self.start()
        body = dict(CHAT, stream_options={"include_usage": True})
        status, through = self.via_meter("POST", "/v1/chat/completions", body)
        _, direct = self.direct("POST", "/v1/chat/completions", body)
        self.assertEqual(status, 200)
        self.assertEqual(through, direct)
        self.assertIn(b"[DONE]", through)

    def test_usage_is_parsed_for_chat_anthropic_and_responses(self):
        self.start()
        self.via_meter("POST", "/v1/chat/completions", CHAT)
        self.via_meter("POST", "/anthropic/v1/messages", {"model": "claude-sonnet-5-5", "stream": True})
        self.via_meter("POST", "/v1/responses", {"model": "gpt-6-luna", "stream": True})
        rows = self.rows()
        self.assertEqual([r["input_tokens"] for r in rows], [100, 95, 100])
        self.assertEqual([r["cached_tokens"] for r in rows], [40, 80, 60])
        self.assertEqual([r["cache_write_tokens"] for r in rows], [0, 5, 0])
        self.assertEqual([r["output_tokens"] for r in rows], [7, 7, 9])
        self.assertEqual([r["reasoning_tokens"] for r in rows], [3, 0, 4])
        self.assertTrue(all(r["usage_found"] for r in rows))
        self.assertTrue(all(r["cost_usd"] > 0 for r in rows))
        self.assertEqual(self.last()["path"], "/v1/responses")

    def test_anthropic_prefix_is_removed_before_forwarding(self):
        self.start()
        self.via_meter("POST", "/anthropic/v1/messages", {"model": "m", "stream": True})
        self.assertEqual(self.last()["path"], "/v1/messages")

    def test_include_usage_is_added_only_when_missing(self):
        self.start()
        self.via_meter("POST", "/v1/chat/completions", CHAT)
        first = json.loads(self.last()["body"])
        self.assertIs(first["stream_options"]["include_usage"], True)
        sent = dict(CHAT, stream_options={"include_usage": True})
        raw = json.dumps(sent).encode()
        self.via_meter("POST", "/v1/chat/completions", raw)
        second = self.last()
        self.assertIs(json.loads(second["body"])["stream_options"]["include_usage"], True)
        self.assertEqual(second["body"].encode(), raw)
        self.assertEqual([r["mutated"] for r in self.rows()], [True, False])

    def test_a_non_json_body_is_forwarded_unchanged(self):
        self.start()
        self.via_meter("POST", "/v1/chat/completions", b"not json")
        self.assertEqual(self.last()["body"], "not json")
        row = self.rows()[0]
        self.assertIsNone(row["model"])
        self.assertFalse(row["mutated"])

    def test_a_spent_cap_returns_429(self):
        self.start("--max-cost", "0.000001")
        first, _ = self.via_meter("POST", "/v1/chat/completions", CHAT)
        second, body = self.via_meter("POST", "/v1/chat/completions", CHAT)
        self.assertEqual(first, 200)
        self.assertEqual(second, 429)
        self.assertIn("spend cap", body.decode())
        rows = self.rows()
        self.assertEqual(rows[1]["status"], 429)
        self.assertEqual(rows[1]["cost_usd"], 0)

    def test_an_unpriced_model_has_no_cost_and_does_not_count_towards_the_cap(self):
        self.start("--max-cost", "0.000001")
        for _ in range(2):
            status, _ = self.via_meter("POST", "/v1/chat/completions", dict(CHAT, model="unknown-model"))
            self.assertEqual(status, 200)
        self.assertEqual([r["cost_usd"] for r in self.rows()], [None, None])

    def test_keys_never_reach_the_log_or_records(self):
        self.start("--key-env", "METER_TEST_KEY", "--record", str(self.dir / "rec"), env={"METER_TEST_KEY": "plainkey123"})
        headers = {"Authorization": "Bearer sk-secretsecret123", "x-fake-status": "500"}
        body = dict(CHAT, note="plainkey123")
        status, _ = self.via_meter("POST", "/v1/chat/completions", body, headers=headers)
        self.assertEqual(status, 500)
        blobs = [self.log.read_text(encoding="utf-8")]
        for path in (self.dir / "rec").iterdir():
            blobs.append(path.read_text(encoding="utf-8"))
        joined = "\n".join(blobs)
        for secret in ("sk-secret", "sk-fakefake", "plainkey123"):
            self.assertNotIn(secret, joined)
        self.assertIn("[redacted]", self.rows()[0]["error"])
        self.assertEqual(sorted(p.name for p in (self.dir / "rec").iterdir()), ["0001.request.json", "0001.response.txt"])

    def test_labels_are_copied_into_every_row(self):
        self.start("--label", "harness=rune", "--label", "trial=2")
        self.via_meter("POST", "/v1/chat/completions", CHAT)
        self.via_meter("GET", "/v1/models")
        for row in self.rows():
            self.assertEqual(row["harness"], "rune")
            self.assertEqual(row["trial"], "2")

    def test_an_unconfigured_anthropic_route_returns_502(self):
        self.start(anthropic=False)
        status, body = self.via_meter("POST", "/anthropic/v1/messages", {"model": "m"})
        self.assertEqual(status, 502)
        self.assertIn("no Anthropic upstream", body.decode())
        self.assertEqual(self.rows()[0]["status"], 502)

    def test_upstream_errors_pass_through_with_the_error_recorded(self):
        self.start()
        status, body = self.via_meter("POST", "/v1/chat/completions", CHAT, headers={"x-fake-status": "503"})
        self.assertEqual(status, 503)
        self.assertIn(b"failure for key", body)
        row = self.rows()[0]
        self.assertEqual(row["status"], 503)
        self.assertFalse(row["usage_found"])
        self.assertIn("failure for key", row["error"])

    def test_an_unreachable_upstream_answers_502(self):
        self.fake.shutdown()
        self.fake.server_close()
        self.start()
        status, body = self.via_meter("GET", "/v1/models")
        self.assertEqual(status, 502)
        self.assertIn("upstream unreachable", body.decode())
        self.fake, _ = fake_upstream.serve()

    def test_row_keys_are_in_the_documented_order(self):
        self.start("--label", "a=b")
        self.via_meter("POST", "/v1/chat/completions", CHAT)
        self.assertEqual(
            list(self.rows()[0]),
            [
                "ts_utc", "method", "path", "model", "status", "latency_ms", "first_byte_ms",
                "request_bytes", "response_bytes", "input_tokens", "cached_tokens",
                "cache_write_tokens", "output_tokens", "reasoning_tokens", "usage_found",
                "cost_usd", "price_window", "mutated", "error", "a",
            ],
        )


class CostTests(unittest.TestCase):
    def test_peak_and_off_peak_prices_follow_the_request_time(self):
        prices = meter.load_prices(HERE / "prices.json")
        counters = {
            "input_tokens": 1000,
            "cached_tokens": 400,
            "cache_write_tokens": 0,
            "output_tokens": 100,
            "reasoning_tokens": 30,
        }
        peak, window = meter.compute_cost(prices, "deepseek-flash", "2026-11-10T02:00:00.000Z", counters)
        self.assertEqual(window, "peak")
        self.assertAlmostEqual(peak, (600 * 0.30 + 400 * 0.006 + 100 * 1.20) / 1e6, places=9)
        off, window = meter.compute_cost(prices, "deepseek-flash", "2026-11-08T02:00:00.000Z", counters)
        self.assertEqual(window, "off_peak")
        self.assertAlmostEqual(off, (600 * 0.15 + 400 * 0.003 + 100 * 0.60) / 1e6, places=9)
        self.assertAlmostEqual(off * 2, peak, places=9)

    def test_models_are_found_by_their_last_path_segment(self):
        prices = meter.load_prices(HERE / "prices.json")
        counters = dict.fromkeys(meter.USAGE_KEYS, 0)
        cost, window = meter.compute_cost(prices, "openrouter/gpt-5-nano", "2026-11-10T12:00:00.000Z", counters)
        self.assertEqual((cost, window), (0.0, None))
        self.assertEqual(meter.compute_cost(prices, "nope", "2026-11-10T12:00:00.000Z", counters), (None, None))

    def test_sse_events_split_across_reads_are_parsed(self):
        event = b'data: {"usage":{"prompt_tokens":5,"completion_tokens":2}}\n\ndata: [DONE]\n\n'
        counters, found = meter.parse_usage("chat", "text/event-stream", event)
        self.assertTrue(found)
        self.assertEqual((counters["input_tokens"], counters["output_tokens"]), (5, 2))


if __name__ == "__main__":
    unittest.main()
