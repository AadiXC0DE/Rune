"""Sutra, balanced quality: one non-interactive run with the DeepSeek provider env.

Command line from the Sutra report, section 2.4, which marks it as unverified by its authors.
VERIFIED flips to True only after a clean real run.
"""

import os
import shutil

NAME = "sutra"
BIN = "sutra"
VERSION_CMD = ["--version"]
HELP_CMD = ["--help"]
HELP_FLAGS = ["-p", "--profile", "--quality", "--sandbox"]
DIALECTS = ["chat"]
VERIFIED = False
QUALITY = "balanced"


def find_bin():
    found = shutil.which("sutra")
    if found:
        return [found], ""
    repo = os.environ.get("BENCH_SUTRA_REPO")
    if repo:
        return (
            ["node", repo + "/node_modules/tsx/dist/cli.mjs", repo + "/src/index.ts"],
            "from source via tsx",
        )
    return None


def prepare(ctx):
    cmd = list(ctx.get("cmd") or [ctx["bin"]])
    return {
        "argv": cmd + [
            "-p", "--profile", "flow", "--quality", QUALITY, "--sandbox", "off", ctx["prompt"],
        ],
        "env": {
            "SUTRA_HOME": str(ctx["home"]) + "/.sutra",
            "DEEPSEEK_API_KEY": ctx["key"],
            "DEEPSEEK_BASE_URL": ctx["proxy_url"],
            "DEEPSEEK_MODEL": ctx["model"],
            "SUTRA_MODEL": ctx["model"],
        },
        "files": {},
    }
