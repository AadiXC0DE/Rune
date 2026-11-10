"""fx: one `ask` in full-access mode against a custom provider pointing at the metering proxy.

Command line from the fx report, section "Exact command lines". VERIFIED flips to True only
after a clean real run.
"""

import json

NAME = "fx"
BIN = "fx"
VERSION_CMD = ["--version"]
HELP_CMD = ["ask", "--help"]
HELP_FLAGS = ["--json", "--full-access", "--no-save", "--provider", "--model"]
DIALECTS = ["chat"]
VERIFIED = False


def prepare(ctx):
    settings = {
        "provider": "bench",
        "providers": {"bench": {
            "protocol": "openai-chat-completions",
            "base_url": ctx["proxy_url"] + "/v1",
            "auth": {"type": "bearer", "env": "BENCH_KEY"},
            "tool_choice_mode": "omit",
            "model_metadata": {ctx["model"]: {
                "context_window": 128000, "max_output_tokens": 8192,
                "supports_tool_use": True, "supports_vision": False,
            }},
        }},
    }
    return {
        "argv": [
            ctx["bin"], "ask", "--json", "--full-access", "--no-save", "--provider", "bench",
            "--model", ctx["model"], "--", ctx["prompt"],
        ],
        "env": {"BENCH_KEY": ctx["key"], "FX_AUTO_UPGRADE": "0"},
        "files": {".fx/settings.json": json.dumps(settings, indent=2) + "\n"},
    }
