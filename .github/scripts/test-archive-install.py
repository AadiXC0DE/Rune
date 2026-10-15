#!/usr/bin/env python3
"""Run the README's Unix archive commands with entirely disposable user paths."""

import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile


def documented_commands(readme, name):
    blocks = re.findall(rf"<!-- {name} -->\s*```sh\n(.*?)\n```", readme, re.DOTALL)
    if len(blocks) != 1:
        raise RuntimeError(f"expected exactly one README {name} command block")
    return blocks[0]


def snapshot(root):
    """Include names, file types, permissions, modification times, and contents."""
    result = {}
    for path in [root, *sorted(root.rglob("*"))]:
        metadata = path.lstat()
        contents = None
        if path.is_symlink():
            contents = os.readlink(path)
        elif path.is_file():
            contents = hashlib.sha256(path.read_bytes()).hexdigest()
        result[str(path.relative_to(root))] = (
            stat.S_IFMT(metadata.st_mode),
            stat.S_IMODE(metadata.st_mode),
            metadata.st_mtime_ns,
            contents,
        )
    return result


def run(commands, environment, workspace):
    output = subprocess.run(
        ["sh", "-eu", "-c", commands],
        env=environment,
        cwd=workspace,
        text=True,
        capture_output=True,
        timeout=30,
        check=False,
    )
    print(output.stdout, end="", flush=True)
    print(output.stderr, end="", file=sys.stderr, flush=True)
    if output.returncode:
        raise RuntimeError(f"documented commands exited {output.returncode}")
    return output.stdout


def smoke(archive):
    archive = archive.resolve(strict=True)
    manifest = json.loads(archive.with_name("manifest.json").read_text())
    if manifest["archive"] != archive.name:
        raise RuntimeError("manifest names a different archive")
    readme = (Path(__file__).resolve().parents[2] / "README.md").read_text()
    install = documented_commands(readme, "archive-install")
    uninstall = documented_commands(readme, "archive-uninstall")

    with tempfile.TemporaryDirectory(prefix="rune-archive-install-") as temporary:
        root = Path(temporary)
        home = root / "user home"
        prefix = root / "install prefix"
        workspace = root / "workspace"
        workspace.mkdir()
        paths = {
            "HOME": home,
            "USERPROFILE": home,
            "XDG_CONFIG_HOME": home / "config",
            "XDG_STATE_HOME": home / "state",
            "XDG_DATA_HOME": home / "data",
            "XDG_CACHE_HOME": home / "cache",
            "TMPDIR": root / "tmp",
        }
        for path in paths.values():
            path.mkdir(parents=True, exist_ok=True)
        fixtures = {
            ".profile": b"# preserve shell configuration\n",
            "config/rune/config.toml": b"# preserve Rune configuration\n",
            "state/rune/credentials.json": b'{"fixture": "preserve credentials"}\n',
            "state/rune/sessions/fixture.jsonl": b'{"fixture": "preserve session"}\n',
            "data/rune/fixture": b"preserve data\n",
        }
        for name, contents in fixtures.items():
            path = home / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(contents)
            path.chmod(0o600)
        before = snapshot(home)
        # Ignore caller overrides, including RUNE_CONFIG, RUNE_HOME and benchmark
        # flags. No child can resolve the caller's user state or workspace.
        environment = {
            key: value for key, value in os.environ.items() if not key.startswith("RUNE_")
        }
        environment.update({key: str(value) for key, value in paths.items()})
        environment.update(archive=str(archive), prefix=str(prefix))
        version_output = run(install, environment, workspace)
        binary = prefix / "bin/rune"
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise RuntimeError("install did not produce an executable rune")
        versions = [line for line in version_output.splitlines() if line.startswith("rune ")]
        if len(versions) != 1 or not versions[0].startswith(f"rune {manifest['version']} ("):
            raise RuntimeError("installed version does not match the staged release manifest")
        if snapshot(home) != before:
            raise RuntimeError("install changed user state")
        run(uninstall, environment, workspace)
        if binary.exists() or binary.is_symlink():
            raise RuntimeError("uninstall left the installed binary behind")
        if snapshot(home) != before:
            raise RuntimeError("uninstall changed user state")
        print("Archive install smoke passed: version matched, binary removed, user state unchanged.")


if __name__ == "__main__":
    try:
        if len(sys.argv) != 2:
            raise RuntimeError("usage: test-archive-install.py <staged-archive.tar.gz>")
        smoke(Path(sys.argv[1]))
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        sys.exit(1)
