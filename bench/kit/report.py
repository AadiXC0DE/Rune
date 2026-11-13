#!/usr/bin/env python3
"""Turns result files into report.md, report.json and results.csv.

  report.py RESULTS [RESULTS...] [--baseline rune] [--out DIR] [--seed N] [--resamples N]

pass@1 comes with a bootstrap interval over tasks, harnesses are compared with a baseline on
paired (task, trial) keys with an exact McNemar test.
"""

import argparse
import csv
import json
import math
import random
import sys
from datetime import datetime, timedelta, timezone
from decimal import ROUND_HALF_UP, Context
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from run import ROW_KEYS  # noqa: E402

SCHEMA = "rune-kit-report/1"
FAIRNESS = [
    "Every harness runs with all permissions granted and no approval prompts.",
    "fx runs with --full-access: its default mode sends unresolved actions to a reviewer model, "
    "which stalls on DeepSeek and would add metered requests to its row.",
    "No harness is given the verify command or the hidden tests.",
    "Codex needs the Responses API and is skipped against Chat Completions-only upstreams.",
    "Tokens and cost come from the meter, never from a harness's own report.",
]


def median(values):
    ordered = sorted(values)
    n = len(ordered)
    if n == 0:
        return None
    mid = n // 2
    return float(ordered[mid]) if n % 2 else (ordered[mid - 1] + ordered[mid]) / 2.0


def nearest_rank(values, fraction):
    ordered = sorted(values)
    if not ordered:
        return None
    return ordered[max(math.ceil(fraction * len(ordered)) - 1, 0)]


def mean(values):
    return sum(values) / len(values) if values else None


def bootstrap(groups, statistic, seed, resamples):
    """groups: list of per-task lists. Returns (low, high) of the statistic over resampled tasks."""
    if not groups:
        return None
    rng = random.Random(seed)
    n = len(groups)
    draws = []
    for _ in range(resamples):
        drawn = [groups[rng.randrange(n)] for _ in range(n)]
        draws.append(statistic(drawn))
    draws.sort()
    return (draws[int(0.025 * resamples)], draws[max(int(0.975 * resamples) - 1, 0)])


def pass_rate(groups):
    total = sum(len(g) for g in groups)
    return sum(sum(g) for g in groups) / total if total else 0.0


def mcnemar_p(b, c):
    n = b + c
    if n == 0:
        return 1.0
    tail = sum(math.comb(n, i) for i in range(min(b, c) + 1))
    return min(1.0, 2 * tail / 2 ** n)


def load_rows(paths):
    rows = {}
    for path in paths:
        for line in Path(path).read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            row = json.loads(line)
            key = (row["harness"], row["task"], row["trial"])
            if key in rows:
                print(
                    "duplicate run %s %s t%s; keeping the later row" % key, file=sys.stderr
                )
                del rows[key]
            rows[key] = row
    return list(rows.values())


def by_task(rows):
    groups = {}
    for r in rows:
        groups.setdefault(r["task"], []).append(1 if r["passed"] else 0)
    return groups


def harness_stats(name, rows, seed, resamples):
    runs = len(rows)
    passed = sum(1 for r in rows if r["passed"])
    groups = by_task(rows)
    ordered_tasks = [groups[t] for t in sorted(groups)]
    ci = bootstrap(ordered_tasks, pass_rate, seed, resamples)
    k = max((len(g) for g in groups.values()), default=0)
    pass_hat = sum(1 for g in groups.values() if all(g)) / len(groups) if groups else None
    walls = [r["wall_ms"] / 1000.0 for r in rows]
    input_total = sum(r["input_tokens"] for r in rows)
    costs = [r["cost_usd"] for r in rows]
    known = all(c is not None for c in costs)
    return {
        "harness": name,
        "runs": runs,
        "passed": passed,
        "pass_at_1": passed / runs if runs else None,
        "ci95": list(ci) if ci else None,
        "k": k,
        "pass_hat_k": pass_hat,
        "false_success": sum(1 for r in rows if r["false_success"]),
        "tampered": sum(1 for r in rows if r["tampered"]),
        "timed_out": sum(1 for r in rows if r["timed_out"]),
        "median_wall_s": median(walls),
        "p90_wall_s": nearest_rank(walls, 0.9),
        "median_requests": median([r["requests"] for r in rows]),
        "median_input_tokens": median([r["input_tokens"] for r in rows]),
        "mean_input_tokens": mean([r["input_tokens"] for r in rows]),
        "median_cached_tokens": median([r["cached_tokens"] for r in rows]),
        "median_output_tokens": median([r["output_tokens"] for r in rows]),
        "cache_share": sum(r["cached_tokens"] for r in rows) / input_total if input_total else None,
        "cost_per_run": mean(costs) if known and costs else None,
        "cost_per_solved": sum(costs) / passed if known and passed else None,
        "harness_version": next((r["harness_version"] for r in rows if r.get("harness_version")), None),
    }


def rank(stats):
    def key(s):
        cost = s["cost_per_solved"]
        return (-(s["pass_at_1"] or 0.0), cost is None, cost if cost is not None else 0.0, s["harness"])

    return sorted(stats, key=key)


def paired(base_rows, other_rows, other_name, baseline, seed, resamples):
    base = {(r["task"], r["trial"]): 1 if r["passed"] else 0 for r in base_rows}
    other = {(r["task"], r["trial"]): 1 if r["passed"] else 0 for r in other_rows}
    keys = sorted(set(base) & set(other))
    tasks = {}
    for k in keys:
        tasks.setdefault(k[0], []).append(other[k] - base[k])
    b = sum(1 for k in keys if base[k] and not other[k])
    c = sum(1 for k in keys if other[k] and not base[k])

    def diff(groups):
        total = sum(len(g) for g in groups)
        return sum(sum(g) for g in groups) / total if total else 0.0

    groups = [tasks[t] for t in sorted(tasks)]
    ci = bootstrap(groups, diff, seed, resamples)
    return {
        "harness": other_name,
        "baseline": baseline,
        "pairs": len(keys),
        "difference": diff(groups) if keys else None,
        "ci95": list(ci) if ci else None,
        "only_baseline": b,
        "only_other": c,
        "mcnemar_p": mcnemar_p(b, c),
    }


def parse_utc(text):
    return datetime.strptime(text, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)


def build_header(rows, run_infos):
    starts = [parse_utc(r["started_utc"]) for r in rows]
    ends = [parse_utc(r["started_utc"]) + timedelta(milliseconds=r["wall_ms"]) for r in rows]
    peak = sum(r["peak_requests"] for r in rows)
    versions = {}
    for r in rows:
        versions.setdefault(r["harness"], r.get("harness_version") or "unknown")
    return {
        "models": sorted({r["model"] for r in rows}),
        "window_utc": [
            min(starts).strftime("%Y-%m-%dT%H:%M:%SZ"),
            max(ends).strftime("%Y-%m-%dT%H:%M:%SZ"),
        ] if rows else None,
        "peak_pricing": "yes, %d requests at peak prices" % peak if peak > 0 else "no",
        "harness_versions": versions,
        "commands": [i["command"] for i in run_infos if i.get("command")],
        "trials": max((r["trial"] for r in rows), default=0),
        "tasks": len({r["task"] for r in rows}),
        "splits": sorted({r["split"] for r in rows if r.get("split")}),
        "fairness": FAIRNESS,
    }


def pct(v):
    return "n/a" if v is None else "%.1f%%" % (v * 100)


def secs(v):
    return "n/a" if v is None else "%.1f s" % v


def count(v):
    return "n/a" if v is None else "{:,}".format(int(round(v)))


def usd(v):
    if v is None:
        return "n/a"
    return "$%.6f" % v if abs(v) < 0.001 else "$%.4f" % v


def pvalue(p):
    d = Context(prec=3, rounding=ROUND_HALF_UP).create_decimal(repr(p))
    text = format(d, "f")
    return text.rstrip("0").rstrip(".") if "." in text else text


def interval(ci):
    return "n/a" if not ci else "%s to %s" % (pct(ci[0]), pct(ci[1]))


def signed(v):
    return "n/a" if v is None else "%+.1f pts" % (v * 100)


def table(head, body):
    lines = ["| " + " | ".join(head) + " |", "| " + " | ".join("---" for _ in head) + " |"]
    lines += ["| " + " | ".join(row) + " |" for row in body]
    return "\n".join(lines)


def render_md(header, ranked, pairs, per_task, baseline, baseline_missing):
    out = ["# Benchmark report", ""]
    window = header["window_utc"]
    out.append("- Models: %s" % (", ".join(header["models"]) or "n/a"))
    out.append("- UTC window: %s" % (" to ".join(window) if window else "n/a"))
    out.append("- Peak pricing: %s" % header["peak_pricing"])
    out.append("- Harness versions: %s" % (
        ", ".join("%s %s" % (h, v) for h, v in sorted(header["harness_versions"].items())) or "n/a"))
    for command in header["commands"]:
        out.append("- Command: `%s`" % command)
    out.append("- Trials: %d" % header["trials"])
    out.append("- Tasks: %d" % header["tasks"])
    out.append("- Splits: %s" % (", ".join(header["splits"]) or "n/a"))
    out.append("- Fairness notes:")
    out += ["  - " + note for note in header["fairness"]]
    out.append("")
    head = ["Rank", "Harness", "Runs", "pass@1", "95% CI", "pass^k", "False success", "Tampered",
            "Timeouts", "Median wall", "p90 wall", "Median requests", "Median input", "Mean input",
            "Median cached", "Median output", "Cache share", "Cost per run", "Cost per solved"]
    body = []
    for i, s in enumerate(ranked, 1):
        body.append([
            str(i), s["harness"], str(s["runs"]), pct(s["pass_at_1"]), interval(s["ci95"]),
            pct(s["pass_hat_k"]), str(s["false_success"]), str(s["tampered"]), str(s["timed_out"]),
            secs(s["median_wall_s"]), secs(s["p90_wall_s"]), count(s["median_requests"]),
            count(s["median_input_tokens"]), count(s["mean_input_tokens"]),
            count(s["median_cached_tokens"]), count(s["median_output_tokens"]),
            pct(s["cache_share"]), usd(s["cost_per_run"]), usd(s["cost_per_solved"]),
        ])
    out += [table(head, body), ""]
    out.append("## Paired against %s" % baseline)
    out.append("")
    if baseline_missing:
        out += ["No runs for %s; paired comparisons were skipped." % baseline, ""]
    else:
        body = [[
            p["harness"], str(p["pairs"]), signed(p["difference"]), interval_pts(p["ci95"]),
            "%d / %d" % (p["only_baseline"], p["only_other"]), pvalue(p["mcnemar_p"]),
        ] for p in pairs]
        head = ["Harness", "Pairs", "Difference", "95% CI", "Only %s / only other" % baseline, "McNemar p"]
        out += [table(head, body), ""]
    out += ["## Per task", ""]
    names = [s["harness"] for s in ranked]
    body = []
    for task in sorted(per_task):
        cells = []
        for h in names:
            got = per_task[task].get(h)
            cells.append("%d/%d" % (got[0], got[1]) if got else "n/a")
        body.append([task] + cells)
    out += [table(["Task"] + names, body), ""]
    return "\n".join(out)


def interval_pts(ci):
    return "n/a" if not ci else "%s to %s" % (signed(ci[0]), signed(ci[1]))


def build(paths, baseline, seed, resamples):
    rows = load_rows(paths)
    run_infos = []
    for path in paths:
        info = Path(path).parent / "run.json"
        if info.is_file():
            try:
                run_infos.append(json.loads(info.read_text(encoding="utf-8")))
            except ValueError:
                pass
    grouped = {}
    for r in rows:
        grouped.setdefault(r["harness"], []).append(r)
    ranked = rank([harness_stats(h, grouped[h], seed, resamples) for h in grouped])
    pairs = []
    if baseline in grouped:
        for s in ranked:
            if s["harness"] != baseline:
                pairs.append(paired(grouped[baseline], grouped[s["harness"]], s["harness"],
                                    baseline, seed, resamples))
    per_task = {}
    for r in rows:
        cell = per_task.setdefault(r["task"], {}).setdefault(r["harness"], [0, 0])
        cell[0] += 1 if r["passed"] else 0
        cell[1] += 1
    header = build_header(rows, run_infos)
    data = {"schema": SCHEMA, "header": header, "harnesses": ranked, "paired": pairs,
            "per_task": per_task}
    md = render_md(header, ranked, pairs, per_task, baseline, baseline not in grouped)
    return rows, data, md


def write_csv(path, rows):
    with open(str(path), "w", encoding="utf-8", newline="") as f:
        w = csv.writer(f)
        w.writerow(ROW_KEYS)
        for r in rows:
            w.writerow(["" if r.get(k) is None else r.get(k) for k in ROW_KEYS])


def main(argv=None):
    p = argparse.ArgumentParser(prog="report.py", description="Benchmark report.")
    p.add_argument("results", nargs="+")
    p.add_argument("--baseline", default="rune")
    p.add_argument("--out")
    p.add_argument("--seed", type=int, default=20261110)
    p.add_argument("--resamples", type=int, default=10000)
    args = p.parse_args(argv)
    out = Path(args.out) if args.out else Path(args.results[0]).resolve().parent
    out.mkdir(parents=True, exist_ok=True)
    rows, data, md = build(args.results, args.baseline, args.seed, args.resamples)
    (out / "report.md").write_text(md, encoding="utf-8")
    (out / "report.json").write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    write_csv(out / "results.csv", rows)
    for name in ("report.md", "report.json", "results.csv"):
        print(out / name)
    return 0


if __name__ == "__main__":
    sys.exit(main())
