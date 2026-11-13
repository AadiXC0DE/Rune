"""pi: print mode with a custom provider declared in the agent models file.

Command line from the pi report, sections A13, B12 and C6 of pi-codex-mini-crush. VERIFIED
flips to True only after a clean real run.
"""

import json

NAME = "pi"
BIN = "pi"
VERSION_CMD = ["--version"]
HELP_CMD = ["--help"]
HELP_FLAGS = ["--print", "--mode", "--no-session", "--provider", "--model"]
DIALECTS = ["chat"]
VERIFIED = False


def prepare(ctx):
    models = {"providers": {"bench": {
        "baseUrl": ctx["proxy_url"] + "/v1",
        "api": "openai-completions",
        "apiKey": "$BENCH_KEY",
        "models": [{"id": ctx["model"], "reasoning": True}],
    }}}
    return {
        "argv": [
            ctx["bin"], "--print", "--mode", "json", "--no-session", "--provider", "bench",
            "--model", ctx["model"], ctx["prompt"],
        ],
        "env": {"BENCH_KEY": ctx["key"]},
        "files": {".pi/agent/models.json": json.dumps(models, indent=2) + "\n"},
    }
