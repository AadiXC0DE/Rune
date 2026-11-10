import contextlib
import io
import json
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import run  # noqa: E402

ADAPTERS = HERE / "adapters"
PROXY = "http://127.0.0.1:4242"
MODEL = "deepseek/deepseek-flash"
KEY = "key-123"
RIVALS = [
    "fx", "sutra", "sutra-fast", "opencode", "pi", "claude-code", "codex", "mini-swe-agent", "goose",
]


def ctx(**extra):
    base = {
        "bin": "/usr/bin/tool", "cmd": ["/usr/bin/tool"], "home": "/tmp/home",
        "workspace": "/tmp/ws", "task_dir": "/tmp/task", "proxy_url": PROXY, "model": MODEL,
        "key": KEY, "prompt": "fix the bug", "timeout_s": 5,
    }
    base.update(extra)
    return base


def load(name):
    return run.Adapters([]).load(name)


class AdapterCase(unittest.TestCase):
    def test_every_adapter_module_exposes_the_interface(self):
        names = sorted(p.stem for p in ADAPTERS.glob("*.py") if p.stem != "__init__")
        self.assertGreaterEqual(len(names), 12)
        for stem in names:
            module = load(stem)
            for attr in ("NAME", "BIN", "VERSION_CMD", "HELP_FLAGS", "VERIFIED", "prepare"):
                self.assertTrue(hasattr(module, attr), "%s lacks %s" % (stem, attr))
            if module.NAME in RIVALS:
                self.assertIs(module.VERIFIED, False, module.NAME)
        self.assertEqual(sorted(load(n).NAME for n in RIVALS), sorted(RIVALS))

    def prepared(self, name):
        module = load(name)
        launch = module.prepare(ctx())
        text = json.dumps(launch)
        self.assertIn(PROXY, text if name != "codex" else json.dumps(launch["argv"]))
        return launch

    def test_prepare_puts_the_proxy_model_and_key_where_each_harness_reads_them(self):
        fx = self.prepared("fx")
        self.assertEqual(fx["argv"], [
            "/usr/bin/tool", "ask", "--json", "--full-access", "--no-save", "--provider", "bench",
            "--model", MODEL, "--", "fix the bug",
        ])
        self.assertEqual(fx["env"], {"BENCH_KEY": KEY, "FX_AUTO_UPGRADE": "0"})
        settings = json.loads(fx["files"][".fx/settings.json"])
        self.assertEqual(settings["providers"]["bench"]["base_url"], PROXY + "/v1")
        self.assertIn(MODEL, settings["providers"]["bench"]["model_metadata"])

        for name, quality in (("sutra", "balanced"), ("sutra-fast", "fast")):
            launch = self.prepared(name)
            self.assertEqual(launch["argv"], [
                "/usr/bin/tool", "-p", "--profile", "flow", "--quality", quality,
                "--sandbox", "off", "fix the bug",
            ])
            self.assertEqual(launch["env"], {
                "SUTRA_HOME": "/tmp/home/.sutra", "DEEPSEEK_API_KEY": KEY,
                "DEEPSEEK_BASE_URL": PROXY, "DEEPSEEK_MODEL": MODEL, "SUTRA_MODEL": MODEL,
            })
            self.assertEqual(launch["files"], {})
        self.assertEqual(load("sutra-fast").NAME, "sutra-fast")

        oc = self.prepared("opencode")
        self.assertEqual(oc["argv"], [
            "/usr/bin/tool", "run", "--model", "bench/" + MODEL, "--format", "json", "--auto",
            "fix the bug",
        ])
        config = json.loads(oc["env"]["OPENCODE_CONFIG_CONTENT"])
        bench = config["provider"]["bench"]
        self.assertEqual(bench["options"], {"baseURL": PROXY + "/v1", "apiKey": "{env:BENCH_KEY}"})
        self.assertEqual(bench["models"], {MODEL: {"name": MODEL}})
        self.assertEqual(config["permission"], {"*": "allow"})
        self.assertEqual(oc["env"]["BENCH_KEY"], KEY)
        self.assertEqual(oc["env"]["OPENCODE_DISABLE_AUTOUPDATE"], "1")

        pi = self.prepared("pi")
        self.assertEqual(pi["argv"], [
            "/usr/bin/tool", "--print", "--mode", "json", "--no-session", "--provider", "bench",
            "--model", MODEL, "fix the bug",
        ])
        models = json.loads(pi["files"][".pi/agent/models.json"])
        self.assertEqual(models["providers"]["bench"], {
            "baseUrl": PROXY + "/v1", "api": "openai-completions", "apiKey": "$BENCH_KEY",
            "models": [{"id": MODEL, "reasoning": True}],
        })
        self.assertEqual(pi["env"], {"BENCH_KEY": KEY})

        cc = self.prepared("claude-code")
        self.assertEqual(cc["argv"], [
            "/usr/bin/tool", "--print", "--dangerously-skip-permissions", "--output-format",
            "json", "--model", MODEL, "--no-session-persistence", "--strict-mcp-config",
            "fix the bug",
        ])
        self.assertEqual(cc["env"]["ANTHROPIC_BASE_URL"], PROXY + "/anthropic")
        self.assertEqual(cc["env"]["ANTHROPIC_AUTH_TOKEN"], KEY)
        self.assertEqual(cc["env"]["CLAUDE_CONFIG_DIR"], "/tmp/home/.claude")
        for var in ("ANTHROPIC_DEFAULT_OPUS_MODEL", "ANTHROPIC_DEFAULT_SONNET_MODEL",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL", "CLAUDE_CODE_SUBAGENT_MODEL"):
            self.assertEqual(cc["env"][var], MODEL)
        self.assertEqual(cc["env"]["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"], "1")
        self.assertEqual(cc["env"]["DISABLE_AUTOUPDATER"], "1")

        cx = self.prepared("codex")
        override = (
            'model_providers.bench={name="bench", base_url="%s/v1", env_key="BENCH_KEY", '
            'wire_api="responses"}' % PROXY
        )
        self.assertEqual(cx["argv"], [
            "/usr/bin/tool", "exec", "--json", "--ephemeral", "--skip-git-repo-check",
            "--dangerously-bypass-approvals-and-sandbox", "-c", "model_provider=bench",
            "-c", override, "-m", MODEL, "--", "fix the bug",
        ])
        self.assertEqual(cx["env"], {"CODEX_HOME": "/tmp/home/.codex", "BENCH_KEY": KEY})

        ms = self.prepared("mini-swe-agent")
        self.assertEqual(ms["argv"], [
            "/usr/bin/tool", "--yolo", "--exit-immediately", "-m", "openai/" + MODEL,
            "-t", "fix the bug", "-o", "/tmp/home/trajectory.json", "-c", "mini",
            "-c", "model.model_kwargs.api_base=" + PROXY + "/v1",
            "-c", "model.model_kwargs.timeout=600",
        ])
        self.assertEqual(ms["env"], {"MSWEA_API_KEY": KEY, "OPENAI_API_KEY": KEY})

        gs = self.prepared("goose")
        self.assertEqual(gs["argv"], ["/usr/bin/tool", "run", "--no-session", "-t", "fix the bug"])
        self.assertEqual(gs["env"], {
            "GOOSE_PROVIDER": "openai", "GOOSE_MODEL": MODEL, "OPENAI_HOST": PROXY,
            "OPENAI_BASE_PATH": "v1/chat/completions", "OPENAI_API_KEY": KEY,
            "GOOSE_MODE": "auto", "GOOSE_DISABLE_KEYRING": "1",
        })

    def test_prepare_writes_nothing_to_disk_itself(self):
        import tempfile
        with tempfile.TemporaryDirectory() as home:
            for name in RIVALS:
                load(name).prepare(ctx(home=home))
            self.assertEqual(os.listdir(home), [])

    def test_a_model_id_with_a_slash_is_only_prefixed_by_mini_swe_agent(self):
        for name in RIVALS:
            text = json.dumps(load(name).prepare(ctx()))
            self.assertIn(MODEL, text)
        self.assertNotIn("openai/" + MODEL, json.dumps(load("goose").prepare(ctx())))

    def test_codex_is_skipped_without_a_responses_upstream(self):
        modules = [load("rune"), load("codex"), load("claude-code")]
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            chat = run.select_harnesses(modules, {"chat"})
        self.assertEqual([m.NAME for m in chat], ["rune"])
        self.assertEqual(
            out.getvalue().splitlines(),
            ["skip codex: needs the responses API", "skip claude-code: needs the anthropic API"],
        )
        with contextlib.redirect_stdout(io.StringIO()):
            full = run.select_harnesses(modules, {"chat", "responses", "anthropic"})
        self.assertEqual([m.NAME for m in full], ["rune", "codex", "claude-code"])

    def test_sutra_uses_the_repository_when_no_binary_is_installed(self):
        sutra = load("sutra")
        with mock.patch("shutil.which", return_value="/usr/local/bin/sutra"):
            self.assertEqual(sutra.find_bin(), (["/usr/local/bin/sutra"], ""))
        with mock.patch("shutil.which", return_value=None), \
                mock.patch.dict(os.environ, {"BENCH_SUTRA_REPO": "/src/sutra"}):
            self.assertEqual(sutra.find_bin(), ([
                "node", "/src/sutra/node_modules/tsx/dist/cli.mjs", "/src/sutra/src/index.ts",
            ], "from source via tsx"))
            module = load("sutra-fast")
            prefix, _ = module.find_bin()
            argv = module.prepare(ctx(cmd=prefix))["argv"]
            self.assertEqual(argv[:3], prefix)
        env = {k: v for k, v in os.environ.items() if k != "BENCH_SUTRA_REPO"}
        with mock.patch("shutil.which", return_value=None), mock.patch.dict(os.environ, env, clear=True):
            self.assertIsNone(sutra.find_bin())
            self.assertIsNone(run.resolve_command(sutra, ""))


if __name__ == "__main__":
    unittest.main()
