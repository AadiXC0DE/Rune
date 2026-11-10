"""Codex: `exec` with a custom Responses API provider passed as one config override.

Command line from the pi-codex-mini-crush report, sections A13, B12 and C6. VERIFIED flips to
True only after a clean real run.
"""

NAME = "codex"
BIN = "codex"
VERSION_CMD = ["--version"]
HELP_CMD = ["exec", "--help"]
HELP_FLAGS = [
    "--json", "--ephemeral", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox",
]
DIALECTS = ["responses"]
VERIFIED = False


def prepare(ctx):
    provider = (
        'model_providers.bench={name="bench", base_url="%s/v1", env_key="BENCH_KEY", '
        'wire_api="responses"}' % ctx["proxy_url"]
    )
    return {
        "argv": [
            ctx["bin"], "exec", "--json", "--ephemeral", "--skip-git-repo-check",
            "--dangerously-bypass-approvals-and-sandbox", "-c", "model_provider=bench",
            "-c", provider, "-m", ctx["model"], "--", ctx["prompt"],
        ],
        "env": {"CODEX_HOME": str(ctx["home"]) + "/.codex", "BENCH_KEY": ctx["key"]},
        "files": {},
    }
