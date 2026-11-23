"""Copies the task's solution over the workspace. Proves a task is solvable."""

import sys

NAME = "oracle"
BIN = sys.executable
VERSION_CMD = ["--version"]
HELP_FLAGS = []
VERIFIED = True


def prepare(ctx):
    copy = "import shutil,sys; shutil.copytree(sys.argv[1], '.', dirs_exist_ok=True)"
    return {
        "argv": [sys.executable, "-c", copy, str(ctx["task_dir"] / "solution")],
        "env": {},
        "files": {},
    }
