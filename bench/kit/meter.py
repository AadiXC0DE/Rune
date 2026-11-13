#!/usr/bin/env python3
"""A local metering proxy for model traffic.

Forwards requests unchanged, streams responses through, and appends one JSON row per request
(tokens, cache use, latency, status, list-price cost) to a log file.
"""

import argparse
import datetime
import http.client
import http.server
import json
import os
import re
import signal
import ssl
import sys
import threading
import time
import urllib.parse
from pathlib import Path

KEEP_LIMIT = 8 * 1024 * 1024
SK_PATTERN = re.compile(r"sk-[A-Za-z0-9_-]{8,}")
SKIP_REQUEST_HEADERS = {
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "upgrade",
    "content-length",
    "accept-encoding",
}
SKIP_RESPONSE_HEADERS = {"connection", "keep-alive", "transfer-encoding", "content-length"}
USAGE_KEYS = (
    "input_tokens",
    "cached_tokens",
    "cache_write_tokens",
    "output_tokens",
    "reasoning_tokens",
)


def dialect_of(path):
    clean = path.split("?", 1)[0]
    if "/anthropic/" in clean or clean.endswith("/messages"):
        return "anthropic"
    if clean.endswith("/responses"):
        return "responses"
    return "chat"


def _num(obj, *keys):
    for key in keys:
        if not isinstance(obj, dict):
            return 0
        obj = obj.get(key)
    return obj if isinstance(obj, (int, float)) and not isinstance(obj, bool) else 0


def extract_usage(dialect, usage):
    """Normalise one usage object to the five counters."""
    if dialect == "anthropic":
        read = _num(usage, "cache_read_input_tokens")
        write = _num(usage, "cache_creation_input_tokens")
        return {
            "input_tokens": _num(usage, "input_tokens") + read + write,
            "cached_tokens": read,
            "cache_write_tokens": write,
            "output_tokens": _num(usage, "output_tokens"),
            "reasoning_tokens": 0,
        }
    if dialect == "responses":
        return {
            "input_tokens": _num(usage, "input_tokens"),
            "cached_tokens": _num(usage, "input_tokens_details", "cached_tokens"),
            "cache_write_tokens": 0,
            "output_tokens": _num(usage, "output_tokens"),
            "reasoning_tokens": _num(usage, "output_tokens_details", "reasoning_tokens"),
        }
    cached = _num(usage, "prompt_cache_hit_tokens") or _num(
        usage, "prompt_tokens_details", "cached_tokens"
    )
    return {
        "input_tokens": _num(usage, "prompt_tokens"),
        "cached_tokens": cached,
        "cache_write_tokens": _num(usage, "prompt_tokens_details", "cache_write_tokens"),
        "output_tokens": _num(usage, "completion_tokens"),
        "reasoning_tokens": _num(usage, "completion_tokens_details", "reasoning_tokens"),
    }


def _find_usage(obj):
    if not isinstance(obj, dict):
        return None
    for holder in (obj, obj.get("message"), obj.get("response")):
        if isinstance(holder, dict) and isinstance(holder.get("usage"), dict):
            return holder["usage"]
    return None


def parse_usage(dialect, content_type, body):
    """Return (counters, usage_found) from a kept response body."""
    totals = dict.fromkeys(USAGE_KEYS, 0)
    found = False
    docs = []
    text = body.decode("utf-8", "replace")
    if "text/event-stream" in content_type:
        for line in text.splitlines():
            if not line.startswith("data:"):
                continue
            payload = line[5:].strip()
            if not payload or payload == "[DONE]":
                continue
            try:
                docs.append(json.loads(payload))
            except ValueError:
                continue
    else:
        try:
            docs.append(json.loads(text))
        except ValueError:
            pass
    for doc in docs:
        usage = _find_usage(doc)
        if usage is None:
            continue
        found = True
        got = extract_usage(dialect, usage)
        for key in USAGE_KEYS:
            totals[key] = max(totals[key], got[key])
    return totals, found


def load_prices(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def _minutes(hhmm):
    hours, minutes = hhmm.split(":")
    return int(hours) * 60 + int(minutes)


def in_peak(entry, when):
    now = when.hour * 60 + when.minute
    for window in entry.get("peak_windows_utc", []):
        if when.weekday() in window["days"] and _minutes(window["start"]) <= now < _minutes(window["end"]):
            return True
    return False


def lookup_model(prices, model):
    models = prices.get("models", {})
    if not isinstance(model, str):
        return None
    if model in models:
        return models[model]
    return models.get(model.rsplit("/", 1)[-1])


def compute_cost(prices, model, ts_utc, counters):
    """Return (cost_usd or None, price_window or None)."""
    entry = lookup_model(prices, model)
    if entry is None:
        return None, None
    table, window = entry, None
    if "off_peak" in entry:
        when = datetime.datetime.strptime(ts_utc[:19], "%Y-%m-%dT%H:%M:%S")
        if in_peak(entry, when):
            window = "peak"
        else:
            table, window = entry["off_peak"], "off_peak"
    uncached = max(
        counters["input_tokens"] - counters["cached_tokens"] - counters["cache_write_tokens"], 0
    )
    cost = (
        uncached * table["input"]
        + counters["cached_tokens"] * table["cached_input"]
        + counters["cache_write_tokens"] * table["cache_write"]
        + counters["output_tokens"] * table["output"]
    ) / 1e6
    return round(cost, 8), window


class Redactor:
    def __init__(self, secret):
        self.secret = secret or ""

    def __call__(self, text):
        text = SK_PATTERN.sub("[redacted]", text)
        if self.secret:
            text = text.replace(self.secret, "[redacted]")
        return text


class State:
    def __init__(self, args, prices):
        self.upstream = urllib.parse.urlsplit(args.upstream)
        self.anthropic = (
            urllib.parse.urlsplit(args.upstream_anthropic) if args.upstream_anthropic else None
        )
        self.prices = prices
        self.max_cost = args.max_cost
        self.labels = args.labels
        self.redact = Redactor(args.key_value)
        self.record_dir = Path(args.record) if args.record else None
        self.lock = threading.Lock()
        self.spent = 0.0
        self.counter = 0
        self.closed = False
        self.log = open(args.log, "a", encoding="utf-8")

    def cap_reached(self):
        with self.lock:
            return self.max_cost is not None and self.spent >= self.max_cost

    def next_index(self):
        with self.lock:
            self.counter += 1
            return self.counter

    def write_row(self, row):
        line = json.dumps(row, separators=(",", ":"), ensure_ascii=False)
        with self.lock:
            if self.closed:
                return
            if isinstance(row.get("cost_usd"), (int, float)):
                self.spent += row["cost_usd"]
            self.log.write(line + "\n")
            self.log.flush()

    def close(self):
        with self.lock:
            self.closed = True
            self.log.close()

    def record(self, index, request_body, response_body):
        if self.record_dir is None:
            return
        self.record_dir.mkdir(parents=True, exist_ok=True)
        stem = self.record_dir / ("%04d" % index)
        req = self.redact(request_body.decode("utf-8", "replace"))
        resp = self.redact(response_body.decode("utf-8", "replace"))
        stem.with_name(stem.name + ".request.json").write_text(req, encoding="utf-8")
        stem.with_name(stem.name + ".response.txt").write_text(resp, encoding="utf-8")


def now_iso():
    now = datetime.datetime.now(datetime.timezone.utc)
    return now.strftime("%Y-%m-%dT%H:%M:%S.") + "%03dZ" % (now.microsecond // 1000)


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    state = None

    def log_message(self, *args):
        pass

    def read_body(self):
        if "chunked" in self.headers.get("transfer-encoding", "").lower():
            data = b""
            while True:
                size_line = self.rfile.readline(65537).split(b";", 1)[0].strip()
                size = int(size_line or b"0", 16)
                if size == 0:
                    while self.rfile.readline(65537).strip():
                        pass
                    return data
                data += self.rfile.read(size)
                self.rfile.readline(3)
        length = int(self.headers.get("content-length") or 0)
        return self.rfile.read(length) if length else b""

    def send_plain(self, status, body):
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def answer(self, row_state, status, obj, finish, cost=None):
        body = json.dumps(obj, separators=(",", ":")).encode()
        row_state.update(status=status, kept=body, response_bytes=len(body), ctype="application/json")
        finish(cost)
        try:
            self.send_plain(status, body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def handle_any(self):
        self.close_connection = True
        st = self.state
        started = time.monotonic()
        ts = now_iso()
        index = st.next_index()
        body = self.read_body()
        path = self.path
        clean_path = path.split("?", 1)[0]

        model = None
        mutated = False
        req_json = None
        try:
            req_json = json.loads(body) if body else None
        except ValueError:
            req_json = None
        if isinstance(req_json, dict):
            if isinstance(req_json.get("model"), str):
                model = req_json["model"]
            opts = req_json.get("stream_options")
            if (
                self.command == "POST"
                and clean_path.endswith("chat/completions")
                and req_json.get("stream") is True
                and not (isinstance(opts, dict) and opts.get("include_usage") is True)
            ):
                merged = dict(opts) if isinstance(opts, dict) else {}
                merged["include_usage"] = True
                req_json["stream_options"] = merged
                body = json.dumps(req_json, separators=(",", ":"), ensure_ascii=False).encode()
                mutated = True

        row_state = {
            "status": 0,
            "first_byte_ms": None,
            "response_bytes": 0,
            "kept": b"",
            "ctype": "",
            "error": None,
        }

        def finish(cost_override=None):
            counters, found = parse_usage(
                dialect_of(path), row_state["ctype"], row_state["kept"]
            )
            if cost_override is None:
                cost, window = compute_cost(st.prices, model, ts, counters)
            else:
                cost, window = cost_override, None
            error = row_state["error"]
            if error is None and row_state["status"] >= 400:
                error = row_state["kept"][:4096].decode("utf-8", "replace")
            if error is not None:
                error = st.redact(error)[:300]
            row = {
                "ts_utc": ts,
                "method": self.command,
                "path": path,
                "model": model,
                "status": row_state["status"],
                "latency_ms": round((time.monotonic() - started) * 1000, 1),
                "first_byte_ms": row_state["first_byte_ms"],
                "request_bytes": len(body),
                "response_bytes": row_state["response_bytes"],
            }
            row.update(counters)
            row["usage_found"] = found
            row["cost_usd"] = cost
            row["price_window"] = window
            row["mutated"] = mutated
            row["error"] = error
            for key, value in st.labels:
                row[key] = value
            st.write_row(row)
            st.record(index, body, row_state["kept"])

        if st.cap_reached():
            msg = {
                "error": {
                    "type": "meter_cost_cap",
                    "message": "the meter's spend cap of $%.2f is reached" % st.max_cost,
                }
            }
            self.answer(row_state, 429, msg, finish, 0.0)
            return

        if clean_path.startswith("/anthropic/"):
            target = st.anthropic
            path_out = path[len("/anthropic"):]
            if target is None:
                msg = {
                    "error": {
                        "type": "meter",
                        "message": "no Anthropic upstream is configured for the meter",
                    }
                }
                self.answer(row_state, 502, msg, finish)
                return
        else:
            target = st.upstream
            path_out = path
        path_out = target.path.rstrip("/") + path_out

        headers = {k: v for k, v in self.headers.items() if k.lower() not in SKIP_REQUEST_HEADERS}
        headers["host"] = target.netloc
        headers["accept-encoding"] = "identity"
        if body or self.command in ("POST", "PUT", "PATCH"):
            headers["content-length"] = str(len(body))

        try:
            if target.scheme == "https":
                conn = http.client.HTTPSConnection(
                    target.hostname,
                    target.port,
                    context=ssl.create_default_context(),
                    timeout=600,
                )
            else:
                conn = http.client.HTTPConnection(target.hostname, target.port, timeout=600)
            conn.request(self.command, path_out, body=body or None, headers=headers)
            resp = conn.getresponse()
        except Exception as exc:  # any connection failure becomes a 502
            msg = {
                "error": {
                    "type": "meter",
                    "message": "upstream unreachable: %s" % exc,
                }
            }
            row_state["error"] = msg["error"]["message"]
            self.answer(row_state, 502, msg, finish)
            return

        row_state["status"] = resp.status
        row_state["ctype"] = resp.getheader("content-type") or ""
        length = resp.getheader("content-length")
        kept = bytearray()
        logged = False
        try:
            self.send_response(resp.status, resp.reason)
            for key, value in resp.getheaders():
                if key.lower() not in SKIP_RESPONSE_HEADERS:
                    self.send_header(key, value)
            if length is not None:
                self.send_header("content-length", length)
            else:
                self.send_header("transfer-encoding", "chunked")
            self.send_header("connection", "close")
            self.end_headers()
            self.wfile.flush()
            total = int(length) if length is not None and length.isdigit() else None
            while True:
                piece = resp.read1(65536)
                if not piece:
                    break
                if row_state["first_byte_ms"] is None:
                    row_state["first_byte_ms"] = round((time.monotonic() - started) * 1000, 1)
                row_state["response_bytes"] += len(piece)
                if len(kept) < KEEP_LIMIT:
                    kept += piece[: KEEP_LIMIT - len(kept)]
                if total is not None:
                    if row_state["response_bytes"] >= total:
                        row_state["kept"] = bytes(kept)
                        finish()
                        logged = True
                    self.wfile.write(piece)
                else:
                    self.wfile.write(b"%x\r\n" % len(piece) + piece + b"\r\n")
                self.wfile.flush()
            if length is None:
                row_state["kept"] = bytes(kept)
                finish()
                logged = True
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            row_state["error"] = "client disconnected"
        except (http.client.HTTPException, OSError) as exc:
            row_state["error"] = "upstream read failed: %s" % exc
        finally:
            conn.close()
        if not logged:
            row_state["kept"] = bytes(kept)
            finish()

    do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = do_HEAD = do_OPTIONS = handle_any


def build_parser():
    parser = argparse.ArgumentParser(description="Metering proxy for model traffic.")
    parser.add_argument("--upstream", required=True)
    parser.add_argument("--upstream-anthropic")
    parser.add_argument("--log", required=True)
    parser.add_argument("--prices", default=str(Path(__file__).resolve().parent / "prices.json"))
    parser.add_argument("--max-cost", type=float)
    parser.add_argument("--record")
    parser.add_argument("--label", action="append", default=[])
    parser.add_argument("--key-env")
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    labels = []
    for item in args.label:
        if "=" not in item:
            parser.error("--label needs KEY=VALUE: %s" % item)
        key, value = item.split("=", 1)
        labels.append((key, value))
    args.labels = labels
    args.key_value = os.environ.get(args.key_env, "") if args.key_env else ""

    state = State(args, load_prices(args.prices))
    Handler.state = state
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True

    def stop(*_):
        threading.Thread(target=server.shutdown).start()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    print(json.dumps({"port": server.server_address[1]}), flush=True)
    try:
        server.serve_forever(poll_interval=0.02)
    finally:
        server.server_close()
        state.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
