#!/usr/bin/env python3
"""Extract a Windows ZIP and replace a stopped, disposable executable."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import zipfile


def run(arguments, environment, workspace):
    result = subprocess.run(
        [str(argument) for argument in arguments],
        env=environment, cwd=workspace, capture_output=True, text=True,
        timeout=60, check=False,
    )
    if result.returncode:
        raise RuntimeError(f"command failed: {arguments}\n{result.stdout}{result.stderr}")
    return result.stdout.strip()


def smoke(archive):
    archive = archive.resolve(strict=True)
    manifest = json.loads(archive.with_name("manifest.json").read_text())
    packed = archive.read_bytes()
    digest = hashlib.sha256(packed).hexdigest()
    if (manifest["archive"] != archive.name
            or manifest["bytes"] != len(packed)
            or manifest["sha256"] != digest
            or "-windows-" not in manifest["target"]):
        raise RuntimeError("Windows release manifest or archive checksum mismatch")
    if archive.with_suffix(".zip.sha256").read_text().strip() != f"{digest}  {archive.name}":
        raise RuntimeError("Windows release checksum sidecar mismatch")

    with tempfile.TemporaryDirectory(prefix="rune-windows-release-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        # Keep every child away from the caller's Rune configuration and state.
        environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith("RUNE_")
        }
        environment["RUSTUP_HOME"] = os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup"))
        for key in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "XDG_CONFIG_HOME",
                    "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "RUNE_HOME"):
            path = root / key.lower()
            path.mkdir()
            environment[key] = str(path)
        with zipfile.ZipFile(archive) as release:
            if release.namelist() != ["rune.exe"]:
                raise RuntimeError("Windows ZIP must contain only rune.exe at its root")
            executable = release.read("rune.exe")
        if (manifest["binary"]["name"] != "rune.exe"
                or manifest["binary"]["bytes"] != len(executable)
                or manifest["binary"]["sha256"] != hashlib.sha256(executable).hexdigest()):
            raise RuntimeError("Windows release executable checksum mismatch")
        extracted = root / "extracted" / "rune.exe"
        extracted.parent.mkdir()
        extracted.write_bytes(executable)
        extracted.chmod(0o755)
        version = run([extracted, "--version"], environment, workspace)
        if not version.startswith(f"rune {manifest['version']} ("):
            raise RuntimeError("extracted version does not match the release manifest")

        # Compile an actual previous executable with different version output.
        # The replacement command runs from the extracted release, so the target
        # is stopped and can be replaced under Windows executable locking rules.
        old_source = root / "old.rs"
        old_source.write_text('fn main() { println!("rune disposable-old (fixture)"); }\n')
        target = root / "installed" / "rune.exe"
        target.parent.mkdir()
        run(["rustc", "--crate-name", "old", old_source, "-o", target], environment, workspace)
        if run([target, "--version"], environment, workspace) != "rune disposable-old (fixture)":
            raise RuntimeError("old executable fixture did not run")
        original = target.read_bytes()
        refused = subprocess.run(
            [str(extracted), "upgrade", "--from", str(archive), "--checksum", "0" * 64,
             "--target", str(target)], env=environment, cwd=workspace,
            capture_output=True, text=True, timeout=60, check=False,
        )
        if refused.returncode == 0 or target.read_bytes() != original:
            raise RuntimeError("checksum refusal did not preserve the old executable")
        run([extracted, "upgrade", "--from", archive, "--checksum", digest,
             "--target", target], environment, workspace)
        if target.read_bytes() != executable:
            raise RuntimeError("replacement differs from the extracted executable")
        if run([target, "--version"], environment, workspace) != version:
            raise RuntimeError("replacement did not run the new version")
        if list(target.parent.glob(".rune-install-staged*")):
            raise RuntimeError("replacement left a staging file behind")
        print("Windows ZIP smoke passed: extracted version matched, old executable replaced, new version ran.")


if __name__ == "__main__":
    try:
        if len(sys.argv) != 2:
            raise RuntimeError("usage: test-windows-release.py <staged-archive.zip>")
        smoke(Path(sys.argv[1]))
    except (OSError, ValueError, KeyError, RuntimeError, zipfile.BadZipFile,
            subprocess.TimeoutExpired) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        sys.exit(1)
