"""Writes the Scoop, winget and npm manifests of one release.

    python3 packaging/render-manifests.py VERSION SHA256SUMS OUT_DIR
    python3 packaging/render-manifests.py --self-test

Standard library only. The release assets and their digests come from the
release's SHA256SUMS, so nothing here is downloaded. Publishing the output is
left to the owner.
"""

import filecmp
import json
import pathlib
import shutil
import sys
import tempfile

HERE = pathlib.Path(__file__).resolve().parent
REPOSITORY = "https://github.com/AadiXC0DE/Rune"
DESCRIPTION = "A tiny, native coding agent harness."
LICENSE = "Apache-2.0"
WINDOWS_TARGET = "x86_64-pc-windows-msvc"
WINGET_ID = "AadiXC0DE.Rune"
WINGET_SCHEMA = "1.6.0"


def digests(sums_text):
    """Each asset name in SHA256SUMS with its digest."""
    found = {}
    for line in sums_text.splitlines():
        fields = line.split()
        if len(fields) == 2:
            found[fields[1].lstrip("*")] = fields[0].lower()
    return found


def windows_asset(version, sums):
    name = f"rune-{version}-{WINDOWS_TARGET}.zip"
    if name not in sums:
        raise SystemExit(f"render-manifests: SHA256SUMS has no line for {name}")
    url = f"{REPOSITORY}/releases/download/v{version}/{name}"
    return url, sums[name]


def scoop(version, url, digest):
    manifest = {
        "version": version,
        "description": DESCRIPTION,
        "homepage": REPOSITORY,
        "license": LICENSE,
        "architecture": {"64bit": {"url": url, "hash": digest}},
        "bin": "rune.exe",
        "checkver": {"github": REPOSITORY},
        "autoupdate": {
            "architecture": {
                "64bit": {
                    "url": f"{REPOSITORY}/releases/download/v$version/rune-$version-{WINDOWS_TARGET}.zip"
                }
            }
        },
    }
    return json.dumps(manifest, indent=4) + "\n"


def winget(version, url, digest):
    header = f"# yaml-language-server: $schema=https://aka.ms/winget-manifest.{{kind}}.{WINGET_SCHEMA}.schema.json\n"
    common = f"PackageIdentifier: {WINGET_ID}\nPackageVersion: {version}\n"
    return {
        f"{WINGET_ID}.yaml": header.format(kind="version") + common
        + f"DefaultLocale: en-US\nManifestType: version\nManifestVersion: {WINGET_SCHEMA}\n",
        f"{WINGET_ID}.installer.yaml": header.format(kind="installer") + common
        + "InstallerType: zip\nNestedInstallerType: portable\nNestedInstallerFiles:\n"
        + "  - RelativeFilePath: rune.exe\n    PortableCommandAlias: rune\n"
        + f"Installers:\n  - Architecture: x64\n    InstallerUrl: {url}\n"
        + f"    InstallerSha256: {digest.upper()}\n"
        + f"ManifestType: installer\nManifestVersion: {WINGET_SCHEMA}\n",
        f"{WINGET_ID}.locale.en-US.yaml": header.format(kind="defaultLocale") + common
        + f"PackageLocale: en-US\nPublisher: AadiXC0DE\nPackageName: Rune\nPackageUrl: {REPOSITORY}\n"
        + f"License: {LICENSE}\nShortDescription: {DESCRIPTION}\n"
        + f"ManifestType: defaultLocale\nManifestVersion: {WINGET_SCHEMA}\n",
    }


def npm(version, out):
    package = json.loads((HERE / "npm" / "package.json").read_text())
    package["version"] = version
    (out / "bin").mkdir(parents=True)
    (out / "package.json").write_text(json.dumps(package, indent=2) + "\n")
    shutil.copyfile(HERE / "npm" / "bin" / "rune.js", out / "bin" / "rune.js")
    (out / "bin" / "rune.js").chmod(0o755)
    shutil.copyfile(HERE / "npm" / "README.md", out / "README.md")


def render(version, sums_text, out):
    version = version.lstrip("v")
    url, digest = windows_asset(version, digests(sums_text))
    (out / "scoop").mkdir(parents=True)
    (out / "scoop" / "rune.json").write_text(scoop(version, url, digest))
    (out / "winget").mkdir()
    for name, text in winget(version, url, digest).items():
        (out / "winget" / name).write_text(text)
    npm(version, out / "npm")


def differing(left, right):
    """The first file that differs between two trees, by relative path."""
    comparison = filecmp.dircmp(left, right)
    for name in comparison.left_only + comparison.right_only + comparison.diff_files:
        return name
    for name in comparison.common_files:
        if (left / name).read_bytes() != (right / name).read_bytes():
            return name
    for name in comparison.common_dirs:
        found = differing(left / name, right / name)
        if found:
            return f"{name}/{found}"
    return None


def self_test():
    fixtures = HERE / "fixtures"
    with tempfile.TemporaryDirectory() as directory:
        out = pathlib.Path(directory) / "out"
        render("v1.2.3", (fixtures / "SHA256SUMS").read_text(), out)
        found = differing(out, fixtures / "expected")
        if found:
            raise SystemExit(f"render-manifests: {found} differs from packaging/fixtures/expected")
    print("ok")


def main():
    arguments = sys.argv[1:]
    if arguments == ["--self-test"]:
        self_test()
        return
    if len(arguments) != 3:
        raise SystemExit(__doc__)
    version, sums_path, out = arguments
    out = pathlib.Path(out)
    if out.exists() and any(out.iterdir()):
        raise SystemExit(f"render-manifests: {out} is not empty")
    out.mkdir(parents=True, exist_ok=True)
    render(version, pathlib.Path(sums_path).read_text(), out)


main()
