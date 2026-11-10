"""Claude Code: print mode against the metering proxy's Anthropic route.

Command line from the pi-codex-mini-crush report, sections A13, B12 and C6, and the Claude Code
install notes. VERIFIED flips to True only after a clean real run.
"""

NAME = "claude-code"
BIN = "claude"
VERSION_CMD = ["--version"]
HELP_CMD = ["--help"]
HELP_FLAGS = [
    "--print", "--dangerously-skip-permissions", "--output-format",
    "--no-session-persistence", "--strict-mcp-config",
]
DIALECTS = ["anthropic"]
VERIFIED = False


def prepare(ctx):
    m = ctx["model"]
    return {
        "argv": [
            ctx["bin"], "--print", "--dangerously-skip-permissions", "--output-format", "json",
            "--model", m, "--no-session-persistence", "--strict-mcp-config", ctx["prompt"],
        ],
        "env": {
            "CLAUDE_CONFIG_DIR": str(ctx["home"]) + "/.claude",
            "ANTHROPIC_BASE_URL": ctx["proxy_url"] + "/anthropic",
            "ANTHROPIC_AUTH_TOKEN": ctx["key"],
            "ANTHROPIC_DEFAULT_OPUS_MODEL": m,
            "ANTHROPIC_DEFAULT_SONNET_MODEL": m,
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": m,
            "CLAUDE_CODE_SUBAGENT_MODEL": m,
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "DISABLE_AUTOUPDATER": "1",
        },
        "files": {},
    }
