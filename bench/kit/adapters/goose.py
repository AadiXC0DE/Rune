"""Goose: `run` with the OpenAI provider pointed at the metering proxy.

Command line from the pi-codex-mini-crush report, sections A13, B12 and C6. VERIFIED flips to
True only after a clean real run.
"""

NAME = "goose"
BIN = "goose"
VERSION_CMD = ["--version"]
HELP_CMD = ["run", "--help"]
HELP_FLAGS = ["--no-session"]
DIALECTS = ["chat"]
VERIFIED = False


def prepare(ctx):
    return {
        "argv": [ctx["bin"], "run", "--no-session", "-t", ctx["prompt"]],
        "env": {
            "GOOSE_PROVIDER": "openai",
            "GOOSE_MODEL": ctx["model"],
            "OPENAI_HOST": ctx["proxy_url"],
            "OPENAI_BASE_PATH": "v1/chat/completions",
            "OPENAI_API_KEY": ctx["key"],
            "GOOSE_MODE": "auto",
            "GOOSE_DISABLE_KEYRING": "1",
        },
        "files": {},
    }
