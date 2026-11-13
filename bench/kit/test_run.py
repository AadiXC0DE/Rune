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

    def do_run(self, harnesses, tasks=TASKS, extra=(), env_extra=None):
        out = self.dir / "out"
        env = {"PATH": "/usr/bin:/bin:/usr/local/bin", "KIT_KEY": "secret-key"}
        env.update(env_extra or {})
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

    def test_setup_changes_belong_to_the_starting_state(self):
        tasks = self.dir / "tasks"
        repo = tasks / "setup-file" / "repo"
        repo.mkdir(parents=True)
        (repo / "a.txt").write_text("a\n")
        (tasks / "setup-file" / "task.json").write_text(json.dumps({
            "prompt": "p", "verify": "test -f made-by-setup.txt",
            "setup": "printf made > made-by-setup.txt",
        }))
        proc, rows, out = self.do_run("noop", tasks=tasks)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(rows[0]["verify_exit"], 0)
        self.assertEqual(rows[0]["files_changed"], 0)
        self.assertEqual((out / "logs" / "noop" / "setup-file-t1" / "diff.patch").read_text(), "")

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

    def test_rune_adapter_builds_the_documented_command(self):
        sys.path.insert(0, str(HERE / "adapters"))
        try:
            import rune as adapter
        finally:
            sys.path.pop(0)
        ctx = {
            "bin": "/opt/rune", "home": "/tmp/h", "workspace": "/tmp/w", "task_dir": "/t",
            "proxy_url": "http://127.0.0.1:9", "model": "m1", "key": "k", "prompt": "fix it",
            "timeout_s": 5,
        }
        launch = adapter.prepare(ctx)
        self.assertEqual(launch["argv"], [
            "/opt/rune", "--permission-mode", "full-access", "ask", "--json", "--no-save",
            "--", "fix it",
        ])
        self.assertEqual(launch["env"], {
            "RUNE_HOME": "/tmp/h/.rune-state", "RUNE_PROVIDER": "chat_completions",
            "RUNE_BASE_URL": "http://127.0.0.1:9/v1", "RUNE_MODEL": "m1",
            "RUNE_API_KEY_ENV": "BENCH_KEY", "BENCH_KEY": "k",
        })
        self.assertTrue(launch["env"]["RUNE_BASE_URL"].endswith("/v1"))

    def test_check_reports_missing_binaries_and_missing_flags(self):
        proc = subprocess.run(
            [sys.executable, RUN, "check", "--harnesses", "nobin,noflag,oracle",
             "--adapter-path", str(ADAPTERS)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True,
        )
        self.assertEqual(proc.returncode, 1, proc.stderr)
        lines = proc.stdout.splitlines()
        self.assertTrue(lines[0].startswith("nobin") and "missing" in lines[0], lines[0])
        self.assertIn("flags missing: --absent, --also-absent", lines[1])
        self.assertTrue(lines[1].endswith("[unverified]"), lines[1])
        self.assertIn("found", lines[2])
        self.assertIn("flags ok", lines[2])

    def test_a_missing_rune_binary_stops_run_before_the_first_run(self):
        proc, rows, _ = self.do_run("rune", extra=["--rune-bin", str(self.dir / "nope")])
        self.assertEqual(proc.returncode, 2)
        self.assertIn("run.py: rune binary not found at", proc.stderr)
        self.assertIn("build it with cargo build --release", proc.stderr)
        self.assertEqual(rows, [])

    def test_a_harness_needing_an_unserved_dialect_is_skipped(self):
        proc, rows, _ = self.do_run("anthropic_only,oracle", extra=["--only", "value-file"])
        self.assertIn("skip anthropic_only: needs the anthropic API", proc.stdout)
        self.assertEqual([r["harness"] for r in rows], ["oracle"])

    def dry_run(self, harnesses, extra=(), tasks=TASKS):
        out = self.dir / "out"
        proc = subprocess.run(
            [sys.executable, RUN, "--tasks", str(tasks), "--harnesses", harnesses, "--dry-run",
             "--out", str(out), "--adapter-path", str(ADAPTERS)] + list(extra),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True,
            env={"PATH": "/usr/bin:/bin:/usr/local/bin"},
        )
        results = out / "results.jsonl"
        rows = [json.loads(l) for l in results.read_text().splitlines()] if results.exists() else []
        return proc, rows, out

    def test_dry_run_meters_a_ping_adapter(self):
        proc, rows, out = self.dry_run("ping", ["--only", "value-file"])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["requests"], 1)
        self.assertEqual(rows[0]["input_tokens"], 100)
        self.assertEqual(rows[0]["model"], "fake")
        self.assertTrue(json.loads((out / "run.json").read_text())["dry_run"])

    def test_dry_run_writes_eight_rows_for_oracle_and_noop(self):
        proc, rows, _ = self.dry_run("oracle,noop", ["--trials", "2"])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(len(rows), 8)

    def test_resume_skips_recorded_runs(self):
        proc, rows, out = self.do_run("oracle", extra=["--trials", "2"])
        self.assertEqual(len(rows), 4, proc.stderr)
        results = out / "results.jsonl"
        lines = results.read_text().splitlines()
        results.write_text("\n".join(lines[:-1]) + "\n")
        proc, rows, _ = self.do_run("oracle", extra=["--trials", "2", "--resume"])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("resume: 3 runs already recorded", proc.stdout)
        self.assertEqual(len(rows), 4)
        self.assertEqual(rows[-1]["trial"], 2)
        self.assertEqual(len(proc.stdout.strip().splitlines()), 2)

    def test_resume_with_another_model_is_refused(self):
        self.do_run("oracle", extra=["--only", "value-file"])
        out = self.dir / "out"
        proc = subprocess.run(
            [sys.executable, RUN, "--tasks", str(TASKS), "--harnesses", "oracle", "--model", "other",
             "--upstream", self.url, "--key-env", "KIT_KEY", "--out", str(out), "--resume"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True,
            env={"PATH": "/usr/bin:/bin", "KIT_KEY": "k"},
        )
        self.assertEqual(proc.returncode, 2)
        self.assertIn("--resume needs the same model as the recorded run (fake)", proc.stderr)

    def test_spend_cap_stops_new_runs(self):
        out = self.dir / "out"
        proc = subprocess.run(
            [sys.executable, RUN, "--tasks", str(TASKS), "--harnesses", "ping", "--model",
             "deepseek-flash", "--upstream", self.url, "--key-env", "KIT_KEY", "--out", str(out),
             "--adapter-path", str(ADAPTERS), "--max-cost", "0.0000001"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True,
            env={"PATH": "/usr/bin:/bin:/usr/local/bin", "KIT_KEY": "k"},
        )
        rows = [json.loads(l) for l in (out / "results.jsonl").read_text().splitlines()]
        self.assertEqual(len(rows), 1, proc.stdout + proc.stderr)
        self.assertGreater(rows[0]["cost_usd"], 0)
        self.assertIn("spend cap reached:", proc.stdout)
        self.assertIn("no new runs start", proc.stdout)

    def test_jobs_never_overlap_harnesses_on_one_task(self):
        events = self.dir / "events.txt"
        proc, rows, _ = self.do_run(
            "events_a,events_b", extra=["--jobs", "4"], env_extra={"KIT_EVENTS": str(events)},
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(len(rows), 4)
        spans = {}
        for line in events.read_text().splitlines():
            harness, task, kind, stamp = line.split()
            spans.setdefault((harness, task), {})[kind] = float(stamp)
        for task in ("value-file", "hidden-check"):
            a, b = spans[("events_a", task)], spans[("events_b", task)]
            self.assertTrue(a["end"] <= b["start"] or b["end"] <= a["start"], task)
        first, second = spans[("events_a", "value-file")], spans[("events_a", "hidden-check")]
        self.assertLess(max(first["start"], second["start"]), min(first["end"], second["end"]))


class SuiteMix(unittest.TestCase):
    def test_no_task_ships_live_instruction_files(self):
        # A file with one of these names inside a task would be read as this
        # repository's own instructions by any agent working here. Tasks store
        # them as AGENTS.task.md and rename them in their setup command.
        root = HERE.parent / "tasks"
        live = [
            str(path.relative_to(root))
            for name in ("AGENTS.md", "CLAUDE.md")
            for path in root.rglob(name)
        ]
        self.assertEqual(live, [])

    def test_the_suite_has_the_planned_mix(self):
        root = HERE.parent / "tasks"
        tasks = []
        for path in sorted(root.glob("*/task.json")):
            tasks.append((path.parent, json.loads(path.read_text(encoding="utf-8"))))
        self.assertEqual(len(tasks), 40)
        for key in ("tests", "tier", "split", "language"):
            for folder, spec in tasks:
                self.assertIn(key, spec, "%s lacks %s" % (folder.name, key))

        def count(key):
            counts = {}
            for _, spec in tasks:
                counts[spec[key]] = counts.get(spec[key], 0) + 1
            return counts

        languages = count("language")
        self.assertEqual(languages.get("javascript", 0) + languages.get("typescript", 0), 10)
        self.assertEqual(languages["python"], 10)
        self.assertEqual(languages["go"], 6)
        self.assertEqual(languages["rust"], 6)
        self.assertEqual(languages["java"], 4)
        self.assertEqual(languages["shell"], 4)
        self.assertEqual(count("tier"), {1: 10, 2: 20, 3: 10})
        self.assertEqual(count("split"), {"dev": 24, "heldout": 16})

        unnamed = 0
        for folder, spec in tasks:
            repo = folder / "repo"
            files = [p.relative_to(repo).as_posix() for p in repo.rglob("*") if p.is_file()]
            names = set(files) | {Path(f).name for f in files}
            if not any(name in spec["prompt"] for name in names):
                unnamed += 1
        self.assertGreaterEqual(unnamed, 28)


if __name__ == "__main__":
    unittest.main()
