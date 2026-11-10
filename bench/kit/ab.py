#!/usr/bin/env python3
"""Compare Rune variants on the suite and decide whether each candidate is adopted.

Each variant is a file in variants/: environment, extra global flags and files
written into the run's home. The first variant is the control. Variants run as
separate harness rows (rune@<name>) through the normal runner, so trials, tasks
and variants interleave the same way harnesses do.
"""

import argparse
import copy
import json
import os
import sys
import types
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import kit  # noqa: E402
import report  # noqa: E402
import run  # noqa: E402

VARIANTS = HERE / "variants"
VARIANT_KEYS = {"description", "env", "flags", "files"}
LOG_HEADER = (
    "| date (UTC) | control | candidate | split | trials | model | pass@1 c -> k | CI "
    "| input change | cost change | decision |\n"
    "|---|---|---|---|---|---|---|---|---|---|---|\n"
)


class VariantError(Exception):
    """A variant file that is missing or malformed."""


def load_variant(name, directory=VARIANTS):
    path = Path(directory) / (name + ".json")
    if not path.is_file():
        raise VariantError("error: no variant file bench/kit/variants/%s.json" % name)
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except ValueError as err:
        raise VariantError("error: variant %s is not JSON: %s" % (name, err))
    if not isinstance(data, dict):
        raise VariantError("error: variant %s must be an object" % name)
    unknown = sorted(set(data) - VARIANT_KEYS)
    if unknown:
        raise VariantError("error: variant %s has unknown key %s" % (name, unknown[0]))
    env = data.get("env", {})
    flags = data.get("flags", [])
    files = data.get("files", {})
    if not (isinstance(env, dict) and all(isinstance(v, str) for v in env.values())):
        raise VariantError("error: variant %s: env must map names to strings" % name)
    if not (isinstance(flags, list) and all(isinstance(f, str) for f in flags)):
        raise VariantError("error: variant %s: flags must be a list of strings" % name)
    if not (isinstance(files, dict) and all(isinstance(v, str) for v in files.values())):
        raise VariantError("error: variant %s: files must map paths to strings" % name)
    return {"description": data.get("description", ""), "env": env, "flags": flags, "files": files}


def merge(prepared, variant, home):
    """What the Rune adapter prepared, with the variant applied: env entries
    override, flags go right after the binary, files are added."""
    out = copy.deepcopy(prepared)

    def sub(text):
        return text.replace("{home}", str(home))

    out.setdefault("env", {})
    out.setdefault("files", {})
    for name, value in variant.get("env", {}).items():
        out["env"][name] = sub(value)
    flags = [sub(flag) for flag in variant.get("flags", [])]
    out["argv"] = list(out["argv"][:1]) + flags + list(out["argv"][1:])
    for rel, text in variant.get("files", {}).items():
        out["files"][sub(rel)] = text
    return out


def variant_adapter(name, variant, base, rune_bin):
    """An adapter module for one variant, built on the Rune adapter."""
    module = types.SimpleNamespace(
        NAME="rune@" + name,
        BIN=base.BIN,
        VERSION_CMD=base.VERSION_CMD,
        HELP_CMD=getattr(base, "HELP_CMD", ["--help"]),
        HELP_EXTRA_CMDS=getattr(base, "HELP_EXTRA_CMDS", []),
        HELP_FLAGS=base.HELP_FLAGS,
        DIALECTS=base.DIALECTS,
        VERIFIED=getattr(base, "VERIFIED", True),
    )
    binary = os.path.abspath(rune_bin)

    def find_bin():
        return ([binary], "") if os.path.isfile(binary) else None

    def prepare(ctx):
        return merge(base.prepare(ctx), variant, ctx["home"])

    module.find_bin = find_bin
    module.prepare = prepare
    return module


def decide(diff, lo, hi, input_change, cost_change):
    """(adopt, reason) for a candidate. diff, lo and hi are percentage points of
    pass@1 against the control; the changes are fractions, None when unknown."""
    cheaper = any(change is not None and change <= -0.05 for change in (input_change, cost_change))
    if lo > -3.0 and cheaper:
        return True, "not worse and cheaper"
    if lo > 0.0:
        return True, "better"
    if lo <= -3.0:
        return False, "pass rate may be lower"
    return False, "not shown cheaper and not worse"


def change(candidate, control):
    if candidate is None or not control:
        return None
    return candidate / control - 1


def fmt_change(value):
    return "n/a" if value is None else "%+.1f%%" % (value * 100)


def fmt_number(value):
    if value is None:
        return "n/a"
    return "%.0f" % value if float(value).is_integer() else "%.4f" % value


def decision_block(control, candidate, stats, pair):
    """The decision text for one candidate, and the values behind it."""
    c, k = stats[control], stats[candidate]
    diff = (pair["difference"] or 0.0) * 100
    lo, hi = [v * 100 for v in pair["ci95"]] if pair.get("ci95") else (diff, diff)
    input_change = change(k["median_input_tokens"], c["median_input_tokens"])
    cost_change = change(k["cost_per_solved"], c["cost_per_solved"])
    adopt, reason = decide(diff, lo, hi, input_change, cost_change)
    text = "\n".join([
        "candidate %s against %s" % (candidate, control),
        "  pass@1          %.1f%% -> %.1f%%   paired diff %+.1f pts, 95%% CI [%+.1f, %+.1f], McNemar p=%.3f"
        % ((c["pass_at_1"] or 0.0) * 100, (k["pass_at_1"] or 0.0) * 100, diff, lo, hi,
           pair.get("mcnemar_p", 1.0)),
        "  input tokens    median %s -> %s (%s)"
        % (fmt_number(c["median_input_tokens"]), fmt_number(k["median_input_tokens"]),
           fmt_change(input_change)),
        "  cost per solved %s -> %s (%s)"
        % (fmt_number(c["cost_per_solved"]), fmt_number(k["cost_per_solved"]), fmt_change(cost_change)),
        "  decision        %s (%s)" % ("ADOPT" if adopt else "KEEP CONTROL", reason),
    ])
    values = {"lo": lo, "hi": hi, "input_change": input_change, "cost_change": cost_change,
              "adopt": adopt, "reason": reason,
              "pass": ((c["pass_at_1"] or 0.0) * 100, (k["pass_at_1"] or 0.0) * 100)}
    return text + "\n", values


def append_log(path, control, candidate, args, values):
    path = Path(path)
    new = not path.is_file()
    with open(str(path), "a", encoding="utf-8") as log:
        if new:
            log.write(LOG_HEADER)
        log.write("| %s | %s | %s | %s | %d | %s | %.1f%% -> %.1f%% | [%+.1f, %+.1f] | %s | %s | %s |\n" % (
            datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M"), control, candidate, args.split,
            args.trials, args.model, values["pass"][0], values["pass"][1], values["lo"], values["hi"],
            fmt_change(values["input_change"]), fmt_change(values["cost_change"]),
            ("ADOPT, " if values["adopt"] else "KEEP, ") + values["reason"],
        ))


def build_parser():
    p = argparse.ArgumentParser(prog="ab.py", description="Compare Rune variants.")
    p.add_argument("--variants", required=True)
    p.add_argument("--tasks", default=str(HERE.parent / "tasks"))
    p.add_argument("--split", choices=["dev", "heldout", "all"], default="dev")
    p.add_argument("--trials", type=int, default=1)
    p.add_argument("--model")
    p.add_argument("--upstream")
    p.add_argument("--key-env")
    p.add_argument("--out")
    p.add_argument("--only")
    p.add_argument("--max-cost", type=float)
    p.add_argument("--jobs", type=int, default=1)
    p.add_argument("--rune-bin", default=str(HERE.parent.parent / "target" / "release" / "rune"))
    p.add_argument("--resume", action="store_true")
    p.add_argument("--dry-run", action="store_true")
    p.add_argument("--log")
    p.add_argument("--variants-dir", default=str(VARIANTS), help=argparse.SUPPRESS)
    return p


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    names = run.split_names(args.variants)
    if len(names) < 2:
        print("ab.py: give at least two variants, the control first", file=sys.stderr)
        return 2
    if len(set(names)) != len(names):
        print("ab.py: variant names must be unique", file=sys.stderr)
        return 2
    try:
        variants = {name: load_variant(name, args.variants_dir) for name in names}
    except VariantError as err:
        print(str(err), file=sys.stderr)
        return 2
    tasks = kit.discover(args.tasks, run.split_names(args.only), args.split)
    if len(tasks) < 2:
        print("ab.py: need at least 2 tasks for a paired interval", file=sys.stderr)
        return 2

    run_args = run.build_parser().parse_args(["--tasks", args.tasks])
    for key in ("split", "trials", "model", "upstream", "key_env", "out", "only", "max_cost",
                "jobs", "rune_bin", "resume", "dry_run"):
        setattr(run_args, key, getattr(args, key))
    adapters = run.Adapters([])
    base = adapters.load("rune")
    modules = [variant_adapter(name, variants[name], base, args.rune_bin) for name in names]
    code, out = run.run_suite(run_args, modules, adapters, parser.error)
    if code != 0:
        return code

    out = Path(out)
    results = out / "results.jsonl"
    rows = report.load_rows([str(results)])
    labels = ["rune@" + name for name in names]
    for name, label in zip(names, labels):
        mine = [r for r in rows if r["harness"] == label]
        if mine and all(r["harness_exit"] == 1 and r["requests"] == 0 for r in mine):
            print("variant %s: every run failed before its first request; see %s"
                  % (name, out / "logs" / label), file=sys.stderr)
            return 3

    control = labels[0]
    _rows, data, md = report.build([str(results)], control, 20261110, 10000)
    (out / "report.md").write_text(md, encoding="utf-8")
    (out / "report.json").write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n",
                                     encoding="utf-8")
    stats = {s["harness"]: s for s in data["harnesses"]}
    pairs = {p["harness"]: p for p in data["paired"]}
    blocks = []
    for label in labels[1:]:
        if label not in stats or control not in stats or label not in pairs:
            continue
        text, values = decision_block(control, label, stats, pairs[label])
        blocks.append(text)
        print(text)
        if args.log:
            append_log(args.log, control, label, args, values)
    with open(str(out / "decision.md"), "a", encoding="utf-8") as decision:
        decision.write("\n".join(blocks))
    return 0


if __name__ == "__main__":
    sys.exit(main())
