"""Runs scripts/install.sh (and, with --powershell, scripts/install.ps1) against
a local release server: fixture archives, SHA256SUMS and a /latest redirect.

Standard library only. Prints `ok <case>` for every case and exits 1 on the
first failure.
"""

import hashlib
import http.server
import io
import os
import pathlib
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import threading
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
VERSION = "0.0.1"
BAD_VERSION = "0.0.2"
FAKE_RUNE = b"#!/bin/sh\necho 'rune 0.0.1 (stable, test)'\n"


def tar_archive(data):
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        info = tarfile.TarInfo("rune")
        info.size = len(data)
        info.mode = 0o755
        archive.addfile(info, io.BytesIO(data))
    return buffer.getvalue()


def zip_archive(data):
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        archive.writestr("rune.exe", data)
    return buffer.getvalue()


def release_files():
    """Every file the server answers, by path."""
    files = {}
    for version, honest in ((VERSION, True), (BAD_VERSION, False)):
        sums = []
        for target, archive in (
            ("aarch64-apple-darwin", tar_archive(FAKE_RUNE)),
            ("x86_64-unknown-linux-musl", tar_archive(FAKE_RUNE)),
            ("x86_64-pc-windows-msvc", zip_archive(FAKE_RUNE)),
        ):
            extension = "zip" if target.endswith("msvc") else "tar.gz"
            name = f"rune-{version}-{target}.{extension}"
            files[f"/download/v{version}/{name}"] = archive
            digest = hashlib.sha256(archive if honest else archive + b"x").hexdigest()
            sums.append(f"{digest}  {name}\n")
        files[f"/download/v{version}/SHA256SUMS"] = "".join(sums).encode()
    files[f"/tag/v{VERSION}"] = b"release page"
    return files


def serve(files):
    class Release(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            if self.path == "/latest":
                self.send_response(302)
                self.send_header("Location", f"/tag/v{VERSION}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            body = files.get(self.path)
            if body is None:
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Release)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def fail(case, result):
    print(f"FAILED {case}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}")
    sys.exit(1)


def run_sh(arguments, environment):
    return subprocess.run(
        # INSTALL_TEST_SHELL picks another POSIX shell, such as dash.
        [os.environ.get("INSTALL_TEST_SHELL", "sh"), str(ROOT / "scripts/install.sh"), *arguments],
        capture_output=True, text=True, env=environment, timeout=60,
    )


def shell_cases(base_url, scratch):
    environment = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(scratch / "home"),
        "RUNE_INSTALL_BASE_URL": base_url,
        "RUNE_INSTALL_OS": "Linux",
        "RUNE_INSTALL_ARCH": "x86_64",
    }

    target = scratch / "good"
    result = run_sh(["--version", f"v{VERSION}", "--dir", str(target)], environment)
    installed = target / "rune"
    if result.returncode != 0 or not installed.exists():
        fail("good digest installs and runs", result)
    if not os.access(installed, os.X_OK):
        fail("good digest installs and runs", result)
    ran = subprocess.run([str(installed)], capture_output=True, text=True, timeout=10)
    if ran.stdout.strip() != "rune 0.0.1 (stable, test)":
        fail("good digest installs and runs", ran)
    if f"installed rune {VERSION} to {target}/rune" not in result.stdout:
        fail("good digest installs and runs", result)
    print("ok good digest installs and runs")

    if f"add {target} to PATH, for example: echo 'export PATH=\"{target}:$PATH\"' >> ~/.profile" not in result.stdout:
        fail("the PATH hint is printed", result)
    on_path = dict(environment, PATH=f"{target}:{environment['PATH']}")
    again = run_sh(["--version", VERSION, "--dir", str(target)], on_path)
    if again.returncode != 0 or "add " in again.stdout:
        fail("the PATH hint is printed", again)
    print("ok the PATH hint is printed")

    target = scratch / "bad"
    result = run_sh(["--version", BAD_VERSION, "--dir", str(target)], environment)
    expected = f"rune: checksum mismatch for rune-{BAD_VERSION}-x86_64-unknown-linux-musl.tar.gz; nothing was installed"
    if result.returncode != 1 or expected not in result.stderr or (target / "rune").exists():
        fail("bad digest installs nothing", result)
    print("ok bad digest installs nothing")

    unsupported = dict(environment, RUNE_INSTALL_OS="SunOS", RUNE_INSTALL_ARCH="sparc")
    result = run_sh(["--dir", str(scratch / "unsupported")], unsupported)
    if result.returncode != 1 or "rune: no release for SunOS sparc" not in result.stderr:
        fail("unsupported platform exits 1", result)
    print("ok unsupported platform exits 1")

    target = scratch / "latest"
    result = run_sh(["--dir", str(target)], environment)
    if result.returncode != 0 or f"installed rune {VERSION}" not in result.stdout:
        fail("latest resolves through the redirect", result)
    print("ok latest resolves through the redirect")

    # Without a test server the download is HTTPS only. A stub curl records
    # what it was asked for and fails, so nothing is fetched.
    stubs = scratch / "stubs"
    stubs.mkdir()
    record = scratch / "curl-arguments"
    stub = stubs / "curl"
    stub.write_text(f"#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{record}'\nexit 22\n")
    stub.chmod(stub.stat().st_mode | stat.S_IXUSR)
    real = dict(environment, PATH=f"{stubs}:{environment['PATH']}")
    del real["RUNE_INSTALL_BASE_URL"]
    result = run_sh(["--version", VERSION, "--dir", str(scratch / "https")], real)
    arguments = record.read_text().splitlines() if record.exists() else []
    if result.returncode == 0 or "--proto" not in arguments or "=https" not in arguments \
            or "--tlsv1.2" not in arguments \
            or not any(argument.startswith("https://github.com/AadiXC0DE/Rune/releases/download/") for argument in arguments):
        fail("the default download is https only", result)
    print("ok the default download is https only")


def powershell_cases(base_url, scratch):
    shell = shutil.which("pwsh") or shutil.which("powershell")
    if shell is None:
        print("skipped powershell: neither pwsh nor powershell is on PATH")
        return

    def run_ps(version, directory, arch="AMD64"):
        environment = dict(os.environ, RUNE_INSTALL_BASE_URL=base_url,
                           RUNE_INSTALL_DIR=str(directory), RUNE_INSTALL_ARCH=arch)
        environment.pop("RUNE_VERSION", None)
        if version:
            environment["RUNE_VERSION"] = version
        return subprocess.run(
            [shell, "-NoProfile", "-NonInteractive", "-File", str(ROOT / "scripts/install.ps1")],
            capture_output=True, text=True, env=environment, timeout=120,
        )

    target = scratch / "ps-good"
    result = run_ps(f"v{VERSION}", target)
    if result.returncode != 0 or (target / "rune.exe").read_bytes() != FAKE_RUNE:
        fail("powershell: good digest installs", result)
    if "[Environment]::SetEnvironmentVariable('Path'" not in result.stdout:
        fail("powershell: the PATH hint is printed", result)
    print("ok powershell: good digest installs")
    print("ok powershell: the PATH hint is printed")

    target = scratch / "ps-bad"
    result = run_ps(BAD_VERSION, target)
    if result.returncode != 1 or "checksum mismatch" not in result.stderr or (target / "rune.exe").exists():
        fail("powershell: bad digest installs nothing", result)
    print("ok powershell: bad digest installs nothing")

    result = run_ps(VERSION, scratch / "ps-arm", arch="ARM64")
    if result.returncode != 1 or "rune: no release for Windows ARM64" not in result.stderr:
        fail("powershell: unsupported platform exits 1", result)
    print("ok powershell: unsupported platform exits 1")

    target = scratch / "ps-latest"
    result = run_ps(None, target)
    if result.returncode != 0 or f"installed rune {VERSION}" not in result.stdout:
        fail("powershell: latest resolves through the redirect", result)
    print("ok powershell: latest resolves through the redirect")


def main():
    server = serve(release_files())
    base_url = f"http://127.0.0.1:{server.server_port}"
    with tempfile.TemporaryDirectory(prefix="rune-install-test-") as directory:
        scratch = pathlib.Path(directory)
        if "--powershell" in sys.argv[1:]:
            powershell_cases(base_url, scratch)
        else:
            shell_cases(base_url, scratch)


main()
