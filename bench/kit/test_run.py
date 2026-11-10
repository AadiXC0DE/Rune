import json
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import fake_upstream  # noqa: E402
import run  # noqa: E402

RUN = str(HERE / "run.py")
TASKS = HERE / "testdata" / "tasks"
ADAPTERS = HERE / "testdata" / "adapters"
KEYS = [
    "harness", "harness_version", "model", "task", "split", "tier", "language", "trial",
    "passed", "tampered", "timed_out", "false_success", "wall_ms", "harness_exit", "verify_exit",
    "requests", "input_tokens", "cached_tokens", "cache_write_tokens", "output_tokens",
    "reasoning_tokens", "cost_usd", "peak_requests", "files_changed", "lines_added",
    "lines_removed", "started_utc", "error",
]


class RunnerCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.fake, _ = fake_upstream.serve()
        self.url = "http://127.0.0.1:%d" % self.fake.server_address[1]

    def tearDown(self):
        self.fake.shutdown()
        self.tmp.cleanup()

    def validate(self, tasks):
        return subprocess.run(
            [sys.executable, RUN, "validate", "--tasks", str(tasks)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True,
        )

    def do_run(self, harnesses, tasks=TASKS, extra=()):
        out = self.dir / "out"
        env = {"PATH": "/usr/bin:/bin:/usr/local/bin", "KIT_KEY": "secret-key"}
        proc = subprocess.run(
            [sys.executable, RUN, "--tasks", str(tasks), "--harnesses", harnesses, "--model", "fake",
             "--upstream", self.url, "--key-env", "KIT_KEY", "--out", str(out),
             "--adapter-path", str(ADAPTERS)] + list(extra),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True, env=env,
        )
        rows = []
        results = out / "results.jsonl"
        if results.exists():
            rows = [json.loads(line) for line in results.read_text().splitlines()]
        return proc, rows, out

    def test_validate_accepts_both_fixture_tasks(self):
        proc = self.validate(TASKS)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        lines = proc.stdout.splitlines()
        self.assertEqual(lines, ["ok    hidden-check", "ok    value-file"])

    def test_validate_rejects_a_task_whose_noop_passes(self):
        shutil.copytree(str(TASKS / "value-file"), str(self.dir / "tasks" / "always-true"))
        path = self.dir / "tasks" / "always-true" / "task.json"
        data = json.loads(path.read_text())
        data["verify"] = "true"
        path.write_text(json.dumps(data))
        proc = self.validate(self.dir / "tasks")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("BAD   always-true (noop passed)", proc.stdout)

    def test_validate_names_task_file_problems(self):
        shutil.copytree(str(TASKS / "value-file"), str(self.dir / "tasks" / "bad-split"))
        path = self.dir / "tasks" / "bad-split" / "task.json"
        data = json.loads(path.read_text())
        data["split"] = "test"
        path.write_text(json.dumps(data))
        shutil.rmtree(str(self.dir / "tasks" / "bad-split" / "solution"))
        proc = self.validate(self.dir / "tasks")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("BAD   bad-split (split must be dev or heldout)", proc.stdout)

    def test_oracle_rows_pass_and_noop_rows_fail(self):
        proc, rows, out = self.do_run("oracle,noop")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(len(rows), 4)
        for row in rows:
            self.assertEqual(list(row.keys()), KEYS)
            self.assertEqual(row["passed"], row["harness"] == "oracle")
            self.assertFalse(row["tampered"])
            self.assertEqual(row["verify_exit"], 0 if row["harness"] == "oracle" else 1)
        self.assertEqual([r["harness"] for r in rows], ["oracle", "noop", "oracle", "noop"])
        oracle = [r for r in rows if r["harness"] == "oracle"][0]
        self.assertEqual(oracle["files_changed"], 1)
        self.assertEqual((oracle["lines_added"], oracle["lines_removed"]), (1, 1))
        run_json = json.loads((out / "run.json").read_text())
        self.assertEqual(run_json["tasks"], 2)
        for name in ("stdout.txt", "stderr.txt", "verify.txt", "requests.jsonl", "diff.patch"):
            self.assertTrue((out / "logs" / "oracle" / "hidden-check-t1" / name).exists(), name)

    def test_hidden_tests_are_absent_during_the_run_and_not_in_the_diff(self):
        proc, rows, out = self.do_run("lister", extra=["--only", "hidden-check"])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        log = out / "logs" / "lister" / "hidden-check-t1"
        listing = (log / "stdout.txt").read_text()
        self.assertIn("value.txt", listing)
        self.assertNotIn("check.sh", listing)
        self.assertEqual((log / "diff.patch").read_text(), "")
        self.assertEqual(rows[0]["verify_exit"], 1)

    def test_a_timed_out_harness_is_killed_with_its_children(self):
        started = time.monotonic()
        proc, rows, _ = self.do_run("sleeper", extra=["--only", "value-file", "--timeout", "1"])
        self.assertLess(time.monotonic() - started, 10)
        self.assertTrue(rows[0]["timed_out"])
        self.assertIsNone(rows[0]["harness_exit"])
        self.assertFalse(rows[0]["passed"])

    def test_a_protected_path_created_by_the_harness_counts_as_tampering(self):
        shutil.copytree(str(TASKS / "value-file"), str(self.dir / "tasks" / "guarded"))
        path = self.dir / "tasks" / "guarded" / "task.json"
        data = json.loads(path.read_text())
        data["protected"] = ["extra.txt"]
        path.write_text(json.dumps(data))
        (self.dir / "tasks" / "guarded" / "solution" / "extra.txt").write_text("x")
        proc, rows, _ = self.do_run("oracle", tasks=self.dir / "tasks")
        self.assertTrue(rows[0]["tampered"])
        self.assertFalse(rows[0]["passed"])
        self.assertEqual(rows[0]["verify_exit"], 0)


if __name__ == "__main__":
    unittest.main()
