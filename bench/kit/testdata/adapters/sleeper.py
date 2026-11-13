"""Test adapter: leaves a background child and then sleeps."""

import sys

NAME = "sleeper"
BIN = sys.executable
VERSION_CMD = ["--version"]
HELP_FLAGS = []
VERIFIED = True


def prepare(ctx):
    return {"argv": ["/bin/sh", "-c", "sleep 30 & sleep 30"], "env": {}, "files": {}}
