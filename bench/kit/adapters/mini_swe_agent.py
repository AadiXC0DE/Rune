"""mini-swe-agent: yolo mode with the proxy as an OpenAI-compatible api base.

Command line from the pi-codex-mini-crush report, sections A13, B12 and C6. VERIFIED flips to
True only after a clean real run.
"""

NAME = "mini-swe-agent"
BIN = "mini-swe-agent"
VERSION_CMD = ["--version"]
HELP_CMD = ["--help"]
HELP_FLAGS = ["--yolo", "--exit-immediately"]
DIALECTS = ["chat"]
VERIFIED = False


def prepare(ctx):
    return {
        "argv": [
            ctx["bin"], "--yolo", "--exit-immediately", "-m", "openai/" + ctx["model"],
            "-t", ctx["prompt"], "-o", str(ctx["home"]) + "/trajectory.json",
            "-c", "mini", "-c", "model.model_kwargs.api_base=" + ctx["proxy_url"] + "/v1",
            "-c", "model.model_kwargs.timeout=600",
        ],
        "env": {"MSWEA_API_KEY": ctx["key"], "OPENAI_API_KEY": ctx["key"]},
        "files": {},
    }
