"""Does nothing. Proves a task fails as shipped."""

import sys

NAME = "noop"
BIN = sys.executable
VERSION_CMD = ["--version"]
HELP_FLAGS = []
VERIFIED = True


def prepare(ctx):
    return {"argv": [sys.executable, "-c", "pass"], "env": {}, "files": {}}
