import contextlib
import csv
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import report  # noqa: E402
from run import ROW_KEYS  # noqa: E402


def row(harness, task, trial, passed, **extra):
    r = dict.fromkeys(ROW_KEYS)
    r.update({
        "harness": harness, "harness_version": "1.0", "model": "m", "task": task, "split": "dev",
        "tier": 1, "language": "py", "trial": trial, "passed": passed, "tampered": False,
        "timed_out": False, "false_success": False, "wall_ms": 1000, "harness_exit": 0,
        "verify_exit": 0 if passed else 1, "requests": 2, "input_tokens": 100,
        "cached_tokens": 50, "cache_write_tokens": 0, "output_tokens": 10,
        "reasoning_tokens": 0, "cost_usd": 0.01, "peak_requests": 0, "files_changed": 1,
        "lines_added": 1, "lines_removed": 0, "started_utc": "2026-01-01T00:00:00Z",
    })
    r.update(extra)
    return r


def stats(rows, name="h"):
    return report.harness_stats(name, rows, 1, 200)


class ReportTests(unittest.TestCase):
    def test_pass_at_1_and_pass_hat_k(self):
        rows = [row("h", "a", 1, True), row("h", "a", 2, True),
                row("h", "b", 1, True), row("h", "b", 2, False)]
        s = stats(rows)
        self.assertEqual(s["pass_at_1"], 0.75)
        self.assertEqual(s["pass_hat_k"], 0.5)
        self.assertEqual(s["k"], 2)

    def test_bootstrap_is_deterministic_and_brackets_the_estimate(self):
        rows = [row("h", "t%d" % i, 1, i % 3 != 0) for i in range(9)]
        a, b = stats(rows), stats(rows)
        self.assertEqual(a["ci95"], b["ci95"])
        self.assertLessEqual(a["ci95"][0], a["pass_at_1"])
        self.assertGreaterEqual(a["ci95"][1], a["pass_at_1"])
        allpass = stats([row("h", "t%d" % i, 1, True) for i in range(5)])
        self.assertEqual(allpass["ci95"], [1.0, 1.0])

    def test_a_tie_on_pass_rate_is_broken_by_cost_per_solved_task(self):
        cheap = stats([row("b-cheap", "a", 1, True, cost_usd=0.01)], "b-cheap")
        dear = stats([row("a-dear", "a", 1, True, cost_usd=0.05)], "a-dear")
        self.assertEqual([s["harness"] for s in report.rank([dear, cheap])], ["b-cheap", "a-dear"])

    def test_zero_discordant_pairs_give_p_one_and_no_difference(self):
        base = [row("rune", "a", 1, True), row("rune", "b", 1, False)]
        other = [row("x", "a", 1, True), row("x", "b", 1, False)]
        p = report.paired(base, other, "x", "rune", 1, 100)
        self.assertEqual(p["mcnemar_p"], 1.0)
        self.assertEqual(p["difference"], 0.0)
        self.assertEqual((p["only_baseline"], p["only_other"]), (0, 0))

    def test_keys_the_baseline_never_ran_are_not_pairs(self):
        base = [row("rune", "a", 1, True)]
        other = [row("x", "a", 1, True), row("x", "z", 1, True)]
        self.assertEqual(report.paired(base, other, "x", "rune", 1, 100)["pairs"], 1)

    def test_mcnemar_matches_the_exact_binomial(self):
        self.assertAlmostEqual(report.mcnemar_p(0, 6), 0.03125)
        self.assertAlmostEqual(report.mcnemar_p(2, 8), 0.109375)
        self.assertEqual(report.pvalue(0.03125), "0.0313")

    def test_p90_uses_nearest_rank(self):
        rows = [row("h", "t%d" % i, 1, True, wall_ms=(i + 1) * 1000) for i in range(10)]
        self.assertEqual(stats(rows)["p90_wall_s"], 9.0)
        self.assertEqual(stats(rows)["median_wall_s"], 5.5)

    def test_null_costs_make_cost_columns_unknown(self):
        s = stats([row("h", "a", 1, True), row("h", "b", 1, True, cost_usd=None)])
        self.assertIsNone(s["cost_per_run"])
        self.assertIsNone(s["cost_per_solved"])
        self.assertEqual(report.usd(None), "n/a")
        self.assertEqual(report.usd(0.0015), "$0.0015")
        self.assertEqual(report.usd(0.0005), "$0.000500")

    def test_csv_columns_follow_the_row_order(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "r.csv"
            report.write_csv(path, [row("h", "a", 1, True, kept_dir="/x")])
            with open(str(path), newline="") as f:
                rows = list(csv.reader(f))
        self.assertEqual(rows[0], ROW_KEYS)
        self.assertNotIn("kept_dir", rows[0])
        self.assertEqual(rows[1][0], "h")

    def test_peak_requests_are_reported_in_the_header(self):
        quiet = report.build_header([row("h", "a", 1, True)], [])
        peak = report.build_header([row("h", "a", 1, True, peak_requests=3)], [])
        self.assertEqual(quiet["peak_pricing"], "no")
        self.assertEqual(peak["peak_pricing"], "yes, 3 requests at peak prices")

    def test_later_file_wins_and_main_writes_three_files(self):
        with tempfile.TemporaryDirectory() as d:
            first, second = Path(d) / "a.jsonl", Path(d) / "b.jsonl"
            first.write_text(json.dumps(row("rune", "a", 1, False)) + "\n")
            second.write_text(
                json.dumps(row("rune", "a", 1, True)) + "\n" + json.dumps(row("x", "a", 1, True)) + "\n"
            )
            out = Path(d) / "out"
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                code = report.main([str(first), str(second), "--out", str(out), "--resamples", "50"])
            self.assertEqual(code, 0)
            data = json.loads((out / "report.json").read_text())
            self.assertEqual(data["schema"], "rune-kit-report/1")
            self.assertEqual(data["per_task"]["a"]["rune"], [1, 1])
            self.assertIn("## Paired against rune", (out / "report.md").read_text())
            self.assertTrue((out / "results.csv").is_file())


if __name__ == "__main__":
    unittest.main()
