"""opencode: `run` with a custom OpenAI-compatible provider supplied through the config env.

Command line from the opencode report, section l. VERIFIED flips to True only after a clean
real run.
"""

import json

NAME = "opencode"
BIN = "opencode"
VERSION_CMD = ["--version"]
HELP_CMD = ["run", "--help"]
HELP_FLAGS = ["--model", "--format", "--auto"]
DIALECTS = ["chat"]
VERIFIED = False


def prepare(ctx):
    config = {
        "$schema": "https://opencode.ai/config.json",
        "provider": {"bench": {
            "npm": "@ai-sdk/openai-compatible",
            "name": "Bench",
            "options": {"baseURL": ctx["proxy_url"] + "/v1", "apiKey": "{env:BENCH_KEY}"},
            "models": {ctx["model"]: {"name": ctx["model"]}},
        }},
        "permission": {"*": "allow"},
    }
    return {
        "argv": [
            ctx["bin"], "run", "--model", "bench/" + ctx["model"], "--format", "json", "--auto",
            ctx["prompt"],
        ],
        "env": {
            "OPENCODE_CONFIG_CONTENT": json.dumps(config),
            "OPENCODE_DISABLE_AUTOUPDATE": "1",
            "BENCH_KEY": ctx["key"],
        },
        "files": {},
    }
