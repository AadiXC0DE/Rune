import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import ab  # noqa: E402

RUNE = HERE.parent.parent / "target" / "release" / "rune"
TASKS = HERE / "testdata" / "tasks"


class DecideCase(unittest.TestCase):
    def test_decide_adopts_cheaper_and_not_worse(self):
        self.assertEqual(ab.decide(0.0, -2.0, 2.0, -0.08, None), (True, "not worse and cheaper"))

    def test_decide_keeps_when_interval_reaches_minus_three(self):
        self.assertEqual(ab.decide(0.0, -3.0, 3.0, -0.5, -0.5), (False, "pass rate may be lower"))

    def test_decide_adopts_better(self):
        self.assertEqual(ab.decide(4.0, 0.5, 7.0, 0.0, 0.0), (True, "better"))

    def test_decide_keeps_when_nothing_changes(self):
        self.assertEqual(
            ab.decide(0.0, -1.0, 1.0, 0.0, None), (False, "not shown cheaper and not worse")
        )


class VariantCase(unittest.TestCase):
    def test_variant_merge_overrides_env_and_inserts_flags(self):
        prepared = {
            "argv": ["/bin/rune", "--permission-mode", "full-access", "ask", "--", "p"],
            "env": {"RUNE_MODEL": "m", "KEEP": "1"},
            "files": {},
        }
        variant = {
            "env": {"RUNE_MODEL": "other", "RUNE_TOOL_TEXT": "{home}/text.json"},
            "flags": ["--effort", "low"],
            "files": {"text.json": "{}"},
        }
        merged = ab.merge(prepared, variant, "/tmp/home")
        self.assertEqual(merged["argv"][:3], ["/bin/rune", "--effort", "low"])
        self.assertEqual(merged["argv"][3:], prepared["argv"][1:])
        self.assertEqual(merged["env"]["RUNE_MODEL"], "other")
        self.assertEqual(merged["env"]["KEEP"], "1")
        self.assertEqual(merged["env"]["RUNE_TOOL_TEXT"], "/tmp/home/text.json")
        self.assertEqual(merged["files"], {"text.json": "{}"})
        self.assertEqual(prepared["env"]["RUNE_MODEL"], "m", "the input is not changed")

    def test_unknown_variant_key_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "odd.json").write_text(json.dumps({"envs": {}}), encoding="utf-8")
            with self.assertRaises(ab.VariantError) as caught:
                ab.load_variant("odd", tmp)
            self.assertIn("unknown key envs", str(caught.exception))
            with self.assertRaises(ab.VariantError) as caught:
                ab.load_variant("absent", tmp)
            self.assertIn("no variant file bench/kit/variants/absent.json", str(caught.exception))


@unittest.skipUnless(RUNE.is_file(), "needs a release build of rune")
class DryRunCase(unittest.TestCase):
    def test_dry_run_two_variants_writes_rows_and_a_decision(self):
        with tempfile.TemporaryDirectory() as tmp:
            variants = Path(tmp, "variants")
            variants.mkdir()
            (variants / "control.json").write_text("{}", encoding="utf-8")
            (variants / "no-note.json").write_text(
                json.dumps({"description": "No run note.", "env": {"RUNE_EXPERIMENT": "run_note=off"}}),
                encoding="utf-8",
            )
            out = Path(tmp, "out")
            done = subprocess.run(
                [sys.executable, str(HERE / "ab.py"), "--variants", "control,no-note",
                 "--variants-dir", str(variants), "--tasks", str(TASKS), "--split", "all",
                 "--trials", "1", "--dry-run", "--out", str(out), "--rune-bin", str(RUNE)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=600,
            )
            self.assertEqual(done.returncode, 0, done.stderr.decode())
            rows = [json.loads(line) for line in (out / "results.jsonl").read_text().splitlines()]
            self.assertEqual({r["harness"] for r in rows}, {"rune@control", "rune@no-note"})
            self.assertTrue((out / "decision.md").is_file())
            self.assertIn("candidate rune@no-note against rune@control", done.stdout.decode())
            logs = list((out / "logs" / "rune@no-note").glob("*/stdout.txt"))
            self.assertTrue(logs)
            for log in logs:
                self.assertIn('"experiment":"run_note=off"', log.read_text())


if __name__ == "__main__":
    unittest.main()
