"""Test adapter whose help text lacks a required flag and which is unverified."""

import sys

NAME = "noflag"
BIN = sys.executable
VERSION_CMD = ["--version"]
HELP_CMD = ["-c", "print('--present')"]
HELP_FLAGS = ["--present", "--absent", "--also-absent"]
VERIFIED = False


def prepare(ctx):
    return {"argv": [sys.executable, "-c", "pass"], "env": {}, "files": {}}
