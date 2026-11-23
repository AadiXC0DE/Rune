"""Test adapter whose executable does not exist."""

NAME = "nobin"
BIN = "rune-kit-no-such-binary"
VERSION_CMD = ["--version"]
HELP_FLAGS = ["--anything"]
VERIFIED = True


def prepare(ctx):
    return {"argv": [ctx["bin"]], "env": {}, "files": {}}
