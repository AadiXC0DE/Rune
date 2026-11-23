"""Rune: one bounded `ask` turn in full-access mode, pointed at the metering proxy."""

NAME = "rune"
BIN = "rune"
VERSION_CMD = ["--version"]
HELP_CMD = ["--help"]
HELP_EXTRA_CMDS = [["ask", "--help"]]
HELP_FLAGS = ["--permission-mode", "--json", "--no-save"]
DIALECTS = ["chat"]
VERIFIED = True
EXTRA_GLOBAL_FLAGS = []


def prepare(ctx):
    return {
        "argv": [
            ctx["bin"], "--permission-mode", "full-access", *EXTRA_GLOBAL_FLAGS,
            "ask", "--json", "--no-save", "--", ctx["prompt"],
        ],
        "env": {
            "RUNE_HOME": str(ctx["home"]) + "/.rune-state",
            "RUNE_PROVIDER": "chat_completions",
            "RUNE_BASE_URL": ctx["proxy_url"] + "/v1",
            "RUNE_MODEL": ctx["model"],
            "RUNE_API_KEY_ENV": "BENCH_KEY",
            "BENCH_KEY": ctx["key"],
        },
        "files": {},
    }
