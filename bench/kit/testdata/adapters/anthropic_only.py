"""Test adapter that speaks only the anthropic dialect."""

import sys

NAME = "anthropic_only"
BIN = sys.executable
VERSION_CMD = ["--version"]
HELP_FLAGS = []
DIALECTS = ["anthropic"]
VERIFIED = True


def prepare(ctx):
    return {"argv": [sys.executable, "-c", "pass"], "env": {}, "files": {}}
