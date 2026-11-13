"""Test adapter: lists the workspace into stdout."""

import sys

NAME = "lister"
BIN = sys.executable
VERSION_CMD = ["--version"]
HELP_FLAGS = []
VERIFIED = True


def prepare(ctx):
    code = "import os; print(sorted(os.listdir('.')))"
    return {"argv": [sys.executable, "-c", code], "env": {}, "files": {}}
