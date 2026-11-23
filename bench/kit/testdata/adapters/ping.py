"""Test adapter: makes one streaming chat request to the proxy."""

import sys

NAME = "ping"
BIN = sys.executable
VERSION_CMD = ["--version"]
HELP_FLAGS = []
VERIFIED = True

CODE = """
import json, sys, urllib.request
body = json.dumps({"model": sys.argv[2], "stream": True,
                   "stream_options": {"include_usage": True},
                   "messages": [{"role": "user", "content": "hi"}]}).encode()
req = urllib.request.Request(sys.argv[1] + "/v1/chat/completions", data=body,
                             headers={"content-type": "application/json"})
print(len(urllib.request.urlopen(req, timeout=20).read()))
"""


def prepare(ctx):
    argv = [sys.executable, "-c", CODE, ctx["proxy_url"], ctx["model"]]
    return {"argv": argv, "env": {}, "files": {}}
