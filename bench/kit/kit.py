"""Shared helpers for the benchmark runner: task loading, digests, processes, meter sums."""

import hashlib
import json
import os
import signal
import struct
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

DEFAULT_TIMEOUT_MS = 600000
DEFAULT_VERIFY_TIMEOUT_MS = 180000
SETUP_TIMEOUT_S = 300
GIT_DATE = "2026-01-01T00:00:00+00:00"
EXCLUDES = [
    ".rune/",
    ".rune.toml",
    ".fx/",
    ".sutra/",
    ".opencode/",
    ".claude/",
    ".codex/",
    ".aider*",
    ".goose/",
    ".pi/",
    "node_modules/",
]
COUNTERS = (
    "input_tokens",
    "cached_tokens",
    "cache_write_tokens",
    "output_tokens",
    "reasoning_tokens",
)


def utc_now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


class Task:
    def __init__(self, directory, data):
        self.dir = Path(directory)
        self.name = self.dir.name
        self.data = data
        self.prompt = data.get("prompt")
        self.verify = data.get("verify")
        self.protected = list(data.get("protected") or [])
        self.timeout_ms = data.get("timeoutMs", DEFAULT_TIMEOUT_MS)
        self.tests = data.get("tests")
        self.setup = data.get("setup")
        self.verify_timeout_ms = data.get("verifyTimeoutMs", DEFAULT_VERIFY_TIMEOUT_MS)
        self.split = data.get("split", "dev")
        self.tier = data.get("tier")
        self.language = data.get("language")

    def problem(self):
        """Return a short description of what is wrong with the task, or None."""
        if not isinstance(self.prompt, str) or not self.prompt.strip():
            return "missing prompt"
        if not isinstance(self.verify, str) or not self.verify.strip():
            return "missing verify"
        if self.split not in ("dev", "heldout"):
            return "split must be dev or heldout"
        if self.tier is not None and self.tier not in (1, 2, 3):
            return "tier must be 1, 2 or 3"
        if self.tests is not None and not (self.dir / self.tests).is_dir():
            return "tests directory %s does not exist" % self.tests
        if not (self.dir / "solution").is_dir():
            return "solution/ is missing"
        return None


def discover(root, only=None, split="all"):
    tasks = []
    for entry in sorted(Path(root).iterdir()):
        if not (entry / "task.json").is_file() or not (entry / "repo").is_dir():
            continue
        if only and entry.name not in only:
            continue
        try:
            data = json.loads((entry / "task.json").read_text(encoding="utf-8"))
        except ValueError as err:
            raise SystemExit("run.py: %s/task.json is not valid JSON: %s" % (entry.name, err))
        task = Task(entry.resolve(), data)
        if split != "all" and task.split != split:
            continue
        tasks.append(task)
    return tasks


def tree_digest(path):
    """SHA-256 over sorted (relative path, NUL, 8-byte length, bytes); None when absent."""
    path = Path(path)
    if not path.exists() and not path.is_symlink():
        return None
    if path.is_file():
        files = [("", path)]
    else:
        files = []
        for base, dirs, names in os.walk(str(path)):
            dirs.sort()
            for name in names:
                full = Path(base) / name
                if full.is_file() and not full.is_symlink():
                    files.append((full.relative_to(path).as_posix(), full))
        files.sort(key=lambda item: item[0])
    digest = hashlib.sha256()
    for rel, full in files:
        data = full.read_bytes()
        digest.update(rel.encode("utf-8"))
        digest.update(b"\0")
        digest.update(struct.pack("<Q", len(data)))
        digest.update(data)
    return digest.hexdigest()


def minimal_env(home, tmpdir):
    home = Path(home)
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(home),
        "USER": os.environ.get("USER", "bench"),
        "LANG": "C.UTF-8",
        "TERM": "dumb",
        "CI": "1",
        "NO_COLOR": "1",
        "TMPDIR": str(tmpdir),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_DATA_HOME": str(home / ".local" / "share"),
        "XDG_STATE_HOME": str(home / ".local" / "state"),
        "XDG_CACHE_HOME": str(home / ".cache"),
    }
    # Toolchain locators that normally live under the real home directory.
    real = Path(os.path.expanduser("~"))
    for var, sub in (("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")):
        value = os.environ.get(var) or (str(real / sub) if (real / sub).is_dir() else None)
        if value:
            env[var] = value
    return env


def kill_group(pid, sig):
    try:
        os.killpg(pid, sig)
    except (ProcessLookupError, PermissionError):
        pass


def run_group(argv, cwd, env, stdout, stderr, limit_s):
    """Run argv in its own process group. Returns (exit code or None when killed, timed_out)."""
    proc = subprocess.Popen(
        argv,
        cwd=str(cwd),
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=stdout,
        stderr=stderr,
        start_new_session=True,
    )
    timed_out = False
    try:
        proc.wait(timeout=limit_s)
    except subprocess.TimeoutExpired:
        timed_out = True
        kill_group(proc.pid, signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            kill_group(proc.pid, signal.SIGKILL)
            proc.wait()
    kill_group(proc.pid, signal.SIGKILL)
    code = proc.returncode
    if timed_out or (code is not None and code < 0):
        return None, timed_out
    return code, timed_out


def sum_meter_log(path):
    totals = {"requests": 0, "cost_usd": 0.0, "peak_requests": 0}
    for name in COUNTERS:
        totals[name] = 0
    unknown_cost = False
    try:
        lines = Path(path).read_text(encoding="utf-8").splitlines()
    except OSError:
        lines = []
    for line in lines:
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except ValueError:
            continue
        totals["requests"] += 1
        tokens = 0
        for name in COUNTERS:
            value = row.get(name) or 0
            totals[name] += value
            tokens += value
        cost = row.get("cost_usd")
        if cost is None:
            if tokens:
                unknown_cost = True
        else:
            totals["cost_usd"] += cost
        if row.get("price_window") == "peak":
            totals["peak_requests"] += 1
    if unknown_cost:
        totals["cost_usd"] = None
    return totals


def redact_files(paths, secret):
    if not secret:
        return
    for path in paths:
        path = Path(path)
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if secret in text:
            path.write_text(text.replace(secret, "[redacted]"), encoding="utf-8")


def python_exe():
    return sys.executable
