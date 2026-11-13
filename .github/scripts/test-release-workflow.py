"""Checks the shape of .github/workflows/release.yml as text.

Standard library only, so it runs before anything is built. Prints `ok` when
every rule holds and exits 1 naming the first one that does not.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOW = (ROOT / ".github/workflows/release.yml").read_text()


def job(name):
    """The text of one job, from its key to the next job's key."""
    found = re.search(rf"^  {re.escape(name)}:\n(.*?)(?=^  [a-z][a-z-]*:\n|\Z)", WORKFLOW, re.S | re.M)
    if not found:
        fail(f"release.yml has no `{name}` job")
    return found.group(1)


def fail(message):
    print(f"release workflow: {message}")
    sys.exit(1)


def check():
    if not WORKFLOW.startswith("name: release\n"):
        fail("the workflow is not named release")
    for name in ("macos", "linux-static", "windows"):
        job(name)
    publish = job("publish")
    if "needs: [macos, linux-static, windows]" not in publish:
        fail("publish does not need every build job")
    if "if: inputs.channel == 'stable'" not in publish:
        fail("publish runs for a channel other than stable")
    create = [line for line in publish.splitlines() if "gh release create" in line]
    if not create or not all("--draft" in line for line in create):
        fail("a release is created without --draft")
    if "is already published; bump the version before releasing again" not in publish:
        fail("publish does not refuse a published version")
    if "x86_64-pc-windows-msvc" not in job("windows"):
        fail("the windows job does not build x86_64-pc-windows-msvc")
    if "sha256sum rune-*.tar.gz rune-*.zip > SHA256SUMS" not in publish:
        fail("publish does not write SHA256SUMS")
    attest = publish.find("actions/attest-build-provenance@v2")
    upload = publish.find("gh release upload")
    if attest < 0 or upload < 0 or attest > upload:
        fail("the provenance attestation does not come before the upload")
    notarise = re.search(r"- name: Notarise the signed binary\n\s+if: (.*)\n", job("macos"))
    if not notarise:
        fail("the macos job does not notarise")
    for secret in ("APPLE_NOTARY_KEY_P8", "APPLE_NOTARY_KEY_ID", "APPLE_NOTARY_ISSUER_ID"):
        if f"env.{secret} != ''" not in notarise.group(1):
            fail(f"notarisation is not guarded by {secret}")
        if re.search(rf"echo[^\n]*\${{?{secret}", WORKFLOW):
            fail(f"{secret} is printed")
    for permission in ("id-token: write", "attestations: write"):
        if permission not in publish:
            fail(f"publish lacks `{permission}`")


check()
print("ok")
