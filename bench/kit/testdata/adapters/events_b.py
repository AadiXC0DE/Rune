"""Test adapter: records when it starts and ends, to the file named by KIT_EVENTS."""

import os
import sys

NAME = "events_b"
BIN = sys.executable
VERSION_CMD = ["--version"]
HELP_FLAGS = []
VERIFIED = True

CODE = """
import sys, time
def mark(kind):
    with open(sys.argv[1], "a") as f:
        f.write("%s %s %s %.4f\\n" % (sys.argv[2], sys.argv[3], kind, time.time()))
mark("start")
time.sleep(1.0)
mark("end")
"""


def prepare(ctx):
    argv = [sys.executable, "-c", CODE, os.environ["KIT_EVENTS"], NAME, ctx["task_dir"].name]
    return {"argv": argv, "env": {}, "files": {}}
