#!/usr/bin/env python3
"""Cross-harness benchmark runner.

Runs harnesses over a task suite, each run in a fresh workspace and empty home behind its own
metering proxy, judges it with hidden tests and appends one result row per run.

  run.py [run] --tasks DIR --harnesses a,b --model ID --upstream URL --key-env VAR [...]
  run.py validate --tasks DIR [--only ids]
"""

import argparse
import concurrent.futures
import importlib
import importlib.util
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import fake_upstream  # noqa: E402
import kit  # noqa: E402

ROOT = HERE.parent.parent
ROW_KEYS = [
    "harness", "harness_version", "model", "task", "split", "tier", "language", "trial",
    "passed", "tampered", "timed_out", "false_success", "wall_ms", "harness_exit", "verify_exit",
    "requests", "input_tokens", "cached_tokens", "cache_write_tokens", "output_tokens",
    "reasoning_tokens", "cost_usd", "peak_requests", "files_changed", "lines_added",
    "lines_removed", "started_utc", "error",
]


class Adapters:
    def __init__(self, extra_paths):
        self.paths = [Path(p) for p in extra_paths] + [HERE / "adapters"]
        self.cache = {}

    def load(self, name):
        if name in self.cache:
            return self.cache[name]
        module_name = name.replace("-", "_")
        for directory in self.paths:
            candidate = directory / (module_name + ".py")
            if candidate.is_file():
                spec = importlib.util.spec_from_file_location("kit_adapter_" + module_name, str(candidate))
                module = importlib.util.module_from_spec(spec)
                spec.loader.exec_module(module)
                self.cache[name] = module
                return module
        raise SystemExit("run.py: no adapter named %s" % name)


class Runner:
    def __init__(self, args, adapters):
        self.args = args
        self.adapters = adapters
        self.out = Path(args.out)
        self.lock = threading.Lock()
        self.spent = 0.0
        self.key = os.environ.get(args.key_env, "") if args.key_env else ""
        self.quiet = getattr(args, "quiet", False)

    def binary(self, module):
        return resolve_binary(module, self.args.rune_bin)

    def version_of(self, module):
        found = self.binary(module)
        if not found or not os.path.isfile(found):
            return "unknown"
        try:
            done = subprocess.run(
                [found] + list(module.VERSION_CMD),
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                timeout=30,
            )
            lines = done.stdout.decode("utf-8", "replace").splitlines()
            return lines[0].strip() if lines and lines[0].strip() else "unknown"
        except (OSError, subprocess.SubprocessError):
            return "unknown"

    def remaining_cost(self):
        if self.args.max_cost is None:
            return None
        with self.lock:
            return max(self.args.max_cost - self.spent, 0.0)

    def start_meter(self, logdir, harness, task, trial, tmp):
        a = self.args
        argv = [
            sys.executable, str(HERE / "meter.py"), "--upstream", a.upstream,
            "--log", str(logdir / "requests.jsonl"), "--prices", a.prices,
            "--label", "harness=" + harness, "--label", "task=" + task, "--label", "trial=%d" % trial,
        ]
        if a.upstream_anthropic:
            argv += ["--upstream-anthropic", a.upstream_anthropic]
        if a.key_env:
            argv += ["--key-env", a.key_env]
        remaining = self.remaining_cost()
        if remaining is not None:
            argv += ["--max-cost", "%.6f" % remaining]
        env = dict(os.environ)
        if a.key_env:
            env[a.key_env] = self.key
        proc = subprocess.Popen(
            argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=open(str(tmp / "meter-stderr.txt"), "wb"), env=env,
        )
        line = proc.stdout.readline()
        try:
            return proc, json.loads(line)["port"]
        except (ValueError, KeyError):
            stop_process(proc)
            raise RuntimeError("the metering proxy did not start")

    def git(self, workspace, *args, env_extra=None):
        env = dict(os.environ)
        env.update(env_extra or {})
        return subprocess.run(
            ["git"] + list(args), cwd=str(workspace), env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )

    def run_one(self, task, module, trial):
        a = self.args
        harness = module.NAME
        logdir = self.out / "logs" / harness / ("%s-t%d" % (task.name, trial))
        logdir.mkdir(parents=True, exist_ok=True)
        tmp = Path(tempfile.mkdtemp(prefix="rune-kit-"))
        workspace = tmp / "workspace"
        home = tmp / "home"
        row = dict.fromkeys(ROW_KEYS)
        row.update({
            "harness": harness, "harness_version": self.versions.get(harness, "unknown"),
            "model": a.model, "task": task.name, "split": task.split, "tier": task.tier,
            "language": task.language, "trial": trial, "passed": False, "tampered": False,
            "timed_out": False, "false_success": False, "wall_ms": 0, "requests": 0,
            "input_tokens": 0, "cached_tokens": 0, "cache_write_tokens": 0, "output_tokens": 0,
            "reasoning_tokens": 0, "cost_usd": 0.0, "peak_requests": 0, "files_changed": 0,
            "lines_added": 0, "lines_removed": 0, "started_utc": kit.utc_now(),
        })
        meter = None
        stdout_path, stderr_path = logdir / "stdout.txt", logdir / "stderr.txt"
        verify_path, diff_path = logdir / "verify.txt", logdir / "diff.patch"
        for p in (stdout_path, stderr_path, verify_path, diff_path):
            p.write_text("")
        try:
            shutil.copytree(str(task.dir / "repo"), str(workspace), symlinks=True)
            for sub in (".config", ".local/share", ".local/state", ".cache"):
                (home / sub).mkdir(parents=True, exist_ok=True)
            (tmp / "tmp").mkdir()
            dates = {"GIT_AUTHOR_DATE": kit.GIT_DATE, "GIT_COMMITTER_DATE": kit.GIT_DATE}
            self.git(workspace, "init", "-q")
            self.git(workspace, "add", "-A")
            self.git(
                workspace, "-c", "user.name=bench", "-c", "user.email=bench@localhost",
                "-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", "task start",
                env_extra=dates,
            )
            info = workspace / ".git" / "info"
            info.mkdir(parents=True, exist_ok=True)
            (info / "exclude").write_text("\n".join(kit.EXCLUDES) + "\n")
            env = kit.minimal_env(home, tmp / "tmp")

            if task.setup:
                with open(str(verify_path), "ab") as sout:
                    code, _ = kit.run_group(
                        ["/bin/sh", "-c", task.setup], workspace, env,
                        sout, subprocess.STDOUT, kit.SETUP_TIMEOUT_S,
                    )
                if code != 0:
                    row["error"] = "setup failed with exit %s" % code
                    return self.finish(row, task, tmp, logdir)

            before = {p: kit.tree_digest(workspace / p) for p in task.protected}
            meter, port = self.start_meter(logdir, harness, task.name, trial, tmp)
            ctx = {
                "bin": self.binary(module), "home": home, "workspace": workspace,
                "task_dir": task.dir, "proxy_url": "http://127.0.0.1:%d" % port,
                "model": a.model, "key": self.key, "prompt": task.prompt, "timeout_s": 0,
            }
            limit = a.timeout if a.timeout else task.timeout_ms / 1000.0
            ctx["timeout_s"] = limit
            launch = module.prepare(ctx)
            for rel, text in (launch.get("files") or {}).items():
                target = home / rel
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(text)
                os.chmod(str(target), 0o600)
            hv = dict(env)
            hv.update(launch.get("env") or {})

            started = time.monotonic()
            try:
                with open(str(stdout_path), "wb") as out, open(str(stderr_path), "wb") as err:
                    code, timed_out = kit.run_group(launch["argv"], workspace, hv, out, err, limit)
            except OSError as err:
                code, timed_out = None, False
                row["error"] = "could not start: %s" % err
            row["wall_ms"] = int((time.monotonic() - started) * 1000)
            row["harness_exit"], row["timed_out"] = code, timed_out
            stop_process(meter)
            meter = None

            after = {p: kit.tree_digest(workspace / p) for p in task.protected}
            row["tampered"] = any(before[p] != after[p] for p in task.protected)
            for p in task.protected:
                target = workspace / p
                if target.is_dir() and not target.is_symlink():
                    shutil.rmtree(str(target))
                elif target.exists() or target.is_symlink():
                    target.unlink()
                source = task.dir / "repo" / p
                if source.is_dir():
                    shutil.copytree(str(source), str(target), symlinks=True)
                elif source.exists():
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(str(source), str(target))
            self.git(workspace, "read-tree", "HEAD")
            self.git(workspace, "add", "-A")
            diff_path.write_bytes(self.git(workspace, "diff", "--cached", "HEAD").stdout)
            stat = self.git(workspace, "diff", "--cached", "--numstat", "HEAD").stdout.decode("utf-8", "replace")
            for line in stat.splitlines():
                parts = line.split("\t")
                if len(parts) >= 3:
                    row["files_changed"] += 1
                    row["lines_added"] += int(parts[0]) if parts[0].isdigit() else 0
                    row["lines_removed"] += int(parts[1]) if parts[1].isdigit() else 0

            if task.tests:
                hidden = task.dir / task.tests
                for entry in sorted(hidden.iterdir()):
                    target = workspace / entry.name
                    if entry.is_dir():
                        shutil.copytree(str(entry), str(target), symlinks=True, dirs_exist_ok=True)
                    else:
                        shutil.copy2(str(entry), str(target))
            with open(str(verify_path), "ab") as vout:
                vcode, vtimed = kit.run_group(
                    ["/bin/sh", "-c", task.verify], workspace, env, vout, subprocess.STDOUT,
                    task.verify_timeout_ms / 1000.0,
                )
            row["verify_exit"] = vcode
            row["passed"] = vcode == 0 and not row["tampered"]
            row["false_success"] = code == 0 and vcode != 0
            if vtimed and not row["error"]:
                row["error"] = "verify timed out"
        except Exception as err:
            row["error"] = ("%s: %s" % (type(err).__name__, err))[:200]
        finally:
            if meter is not None:
                stop_process(meter)
        return self.finish(row, task, tmp, logdir)

    def finish(self, row, task, tmp, logdir):
        row.update(kit.sum_meter_log(logdir / "requests.jsonl"))
        kit.redact_files(
            [logdir / n for n in ("stdout.txt", "stderr.txt", "verify.txt", "diff.patch")], self.key
        )
        if self.args.keep:
            row["kept_dir"] = str(tmp)
        ordered = {k: row[k] for k in ROW_KEYS}
        if "kept_dir" in row:
            ordered["kept_dir"] = row["kept_dir"]
        line = json.dumps(ordered, separators=(",", ":"), ensure_ascii=False)
        with self.lock:
            if isinstance(ordered["cost_usd"], (int, float)):
                self.spent += ordered["cost_usd"]
            with open(str(self.out / "results.jsonl"), "a", encoding="utf-8") as f:
                f.write(line + "\n")
                f.flush()
                os.fsync(f.fileno())
            if not self.quiet:
                print(console_line(ordered), flush=True)
        if not self.args.keep:
            shutil.rmtree(str(tmp), ignore_errors=True)
        return ordered

    def execute(self, tasks, modules):
        self.versions = {m.NAME: self.version_of(m) for m in modules}
        self.out.mkdir(parents=True, exist_ok=True)
        a = self.args
        (self.out / "run.json").write_text(json.dumps({
            "command": shlex.join(sys.argv), "started_utc": kit.utc_now(), "model": a.model,
            "harnesses": [m.NAME for m in modules], "harness_versions": self.versions,
            "trials": a.trials, "tasks": len(tasks), "split": a.split, "dry_run": False,
        }, indent=2) + "\n")
        groups = [(t, task) for t in range(1, a.trials + 1) for task in tasks]

        def work(group):
            trial, task = group
            return [self.run_one(task, m, trial) for m in modules]

        rows = []
        if a.jobs <= 1:
            for group in groups:
                rows.extend(work(group))
        else:
            with concurrent.futures.ThreadPoolExecutor(max_workers=a.jobs) as pool:
                for result in [pool.submit(work, g) for g in groups]:
                    rows.extend(result.result())
        return rows


def resolve_binary(module, rune_bin):
    """Absolute path of the harness executable, or None when it cannot be found."""
    if getattr(module, "NAME", "") == "rune" and rune_bin:
        path = os.path.abspath(rune_bin)
        return path if os.path.isfile(path) else None
    return shutil.which(module.BIN)


def stop_process(proc):
    if proc.poll() is None:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    if proc.stdout:
        proc.stdout.close()


def console_line(r):
    cost = "$?" if r["cost_usd"] is None else "$%.4f" % r["cost_usd"]
    line = "%-14s %-28s t%d  %s  %.1fs  %d req  %d in  %d cached  %d out  %s" % (
        r["harness"], r["task"], r["trial"], "PASS" if r["passed"] else "FAIL",
        r["wall_ms"] / 1000.0, r["requests"], r["input_tokens"], r["cached_tokens"],
        r["output_tokens"], cost,
    )
    if r["tampered"]:
        line += "  [tampered]"
    if r["timed_out"]:
        line += "  [timed out]"
    if r["false_success"]:
        line += "  [false success]"
    if r["error"]:
        line += "  " + r["error"]
    return line


def build_parser():
    p = argparse.ArgumentParser(prog="run.py", description="Cross-harness benchmark runner.")
    p.add_argument("--tasks", required=True)
    p.add_argument("--harnesses", default="")
    p.add_argument("--trials", type=int, default=1)
    p.add_argument("--model")
    p.add_argument("--upstream")
    p.add_argument("--upstream-anthropic")
    p.add_argument("--key-env")
    p.add_argument("--out")
    p.add_argument("--only")
    p.add_argument("--split", choices=["dev", "heldout", "all"], default="all")
    p.add_argument("--timeout", type=float)
    p.add_argument("--max-cost", type=float)
    p.add_argument("--jobs", type=int, default=1)
    p.add_argument("--keep", action="store_true")
    p.add_argument("--upstream-dialects")
    p.add_argument("--rune-bin", default=str(ROOT / "target" / "release" / "rune"))
    p.add_argument("--prices", default=str(HERE / "prices.json"))
    p.add_argument("--adapter-path", action="append", default=[])
    return p


def split_names(text):
    return [n for n in (text or "").split(",") if n]


def cmd_run(argv):
    parser = build_parser()
    args = parser.parse_args(argv)
    for name in ("model", "upstream", "key_env"):
        if not getattr(args, name):
            parser.error("--%s is required" % name.replace("_", "-"))
    if not args.harnesses:
        parser.error("--harnesses is required")
    require_git()
    if args.out is None:
        args.out = str(ROOT / "bench" / "results" / datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ"))
    adapters = Adapters(args.adapter_path)
    modules = [adapters.load(n) for n in split_names(args.harnesses)]
    served = set(split_names(args.upstream_dialects)) or {"chat"}
    if args.upstream_anthropic and not args.upstream_dialects:
        served.add("anthropic")
    runnable = []
    for module in modules:
        needs = list(getattr(module, "DIALECTS", ["chat"]))
        if not served.intersection(needs):
            print("skip %s: needs the %s API" % (module.NAME, "/".join(needs)), flush=True)
            continue
        if resolve_binary(module, args.rune_bin) is None and module.NAME == "rune":
            print(
                "run.py: rune binary not found at %s; build it with cargo build --release"
                % os.path.abspath(args.rune_bin),
                file=sys.stderr,
            )
            return 2
        runnable.append(module)
    modules = runnable
    if not modules:
        return 0
    tasks = kit.discover(args.tasks, split_names(args.only), args.split)
    if not tasks:
        print("run.py: no tasks selected", file=sys.stderr)
        return 2
    rows = Runner(args, adapters).execute(tasks, modules)
    return 0 if rows else 1


def require_git():
    if shutil.which("git") is None:
        print("run.py: git is required", file=sys.stderr)
        sys.exit(2)


def cmd_validate(argv):
    p = argparse.ArgumentParser(prog="run.py validate")
    p.add_argument("--tasks", required=True)
    p.add_argument("--only")
    p.add_argument("--adapter-path", action="append", default=[])
    opts = p.parse_args(argv)
    require_git()
    tasks = kit.discover(opts.tasks, split_names(opts.only), "all")
    server, _thread = fake_upstream.serve()
    os.environ["RUNE_KIT_VALIDATE_KEY"] = "validate-key"
    bad = 0
    with tempfile.TemporaryDirectory(prefix="rune-kit-validate-") as out:
        args = argparse.Namespace(
            tasks=opts.tasks, trials=1, model="fake", upstream="http://127.0.0.1:%d" % server.server_address[1],
            upstream_anthropic=None, key_env="RUNE_KIT_VALIDATE_KEY", out=out, timeout=None,
            max_cost=None, jobs=1, keep=False, rune_bin="", prices=str(HERE / "prices.json"),
            split="all", quiet=True,
        )
        adapters = Adapters(opts.adapter_path)
        modules = [adapters.load("oracle"), adapters.load("noop")]
        runner = Runner(args, adapters)
        runner.versions = {}
        Path(out).mkdir(parents=True, exist_ok=True)
        for task in tasks:
            reason = task.problem()
            if reason is None:
                oracle = runner.run_one(task, modules[0], 1)
                noop = runner.run_one(task, modules[1], 1)
                if not oracle["passed"]:
                    reason = "oracle failed"
                elif noop["passed"]:
                    reason = "noop passed"
            if reason is None:
                print("ok    %s" % task.name, flush=True)
            else:
                bad += 1
                print("BAD   %s (%s)" % (task.name, reason), flush=True)
    server.shutdown()
    return 1 if bad else 0


def cmd_check(argv):
    p = argparse.ArgumentParser(prog="run.py check")
    p.add_argument("--harnesses", required=True)
    p.add_argument("--rune-bin", default=str(ROOT / "target" / "release" / "rune"))
    p.add_argument("--adapter-path", action="append", default=[])
    opts = p.parse_args(argv)
    adapters = Adapters(opts.adapter_path)
    bad = 0
    for name in split_names(opts.harnesses):
        module = adapters.load(name)
        exe = resolve_binary(module, opts.rune_bin)
        if exe is None:
            bad += 1
            where = os.path.abspath(opts.rune_bin) if module.NAME == "rune" else module.BIN
            print("%-14s missing  %s" % (module.NAME, where))
            continue
        version = Runner.version_of(argparse.Namespace(binary=lambda m: exe), module)
        help_text = ""
        commands = [list(getattr(module, "HELP_CMD", ["--help"]))]
        commands += [list(c) for c in getattr(module, "HELP_EXTRA_CMDS", [])]
        for command in commands:
            try:
                done = subprocess.run(
                    [exe] + command, stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10,
                )
                help_text += done.stdout.decode("utf-8", "replace") + "\n"
            except (OSError, subprocess.SubprocessError):
                pass
        absent = [f for f in module.HELP_FLAGS if f not in help_text]
        flags = "flags ok" if not absent else "flags missing: " + ", ".join(absent)
        line = "%-14s found    %s  %s  %s" % (module.NAME, exe, version, flags)
        if not getattr(module, "VERIFIED", True):
            line += "  [unverified]"
        if absent:
            bad += 1
        print(line)
    return 1 if bad else 0


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv and argv[0] == "validate":
        return cmd_validate(argv[1:])
    if argv and argv[0] == "check":
        return cmd_check(argv[1:])
    if argv and argv[0] == "run":
        argv = argv[1:]
    return cmd_run(argv)


if __name__ == "__main__":
    sys.exit(main())
