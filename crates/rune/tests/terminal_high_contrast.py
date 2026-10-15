"""R-071: select high-contrast, render its footer/menu, and honor NO_COLOR."""

import fcntl
import json
import os
import pathlib
import pty
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def capture(binary, root, environment):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    child = subprocess.Popen(
        [binary, "--offline", "--theme", "high-contrast"],
        stdin=slave, stdout=slave, stderr=slave,
        cwd=root, env=environment, preexec_fn=controlling_terminal,
    )
    os.close(slave)
    transcript = bytearray()

    def wait_for(text, start=0):
        deadline = time.monotonic() + 10
        while text not in transcript[start:]:
            assert time.monotonic() < deadline, (
                f"waiting for {text!r}:\n" + transcript.decode(errors="replace")
            )
            assert child.poll() is None, transcript.decode(errors="replace")
            if select.select([master], [], [], 0.05)[0]:
                transcript.extend(os.read(master, 65536))

    try:
        wait_for(b"ctrl-c cancel")
        start = len(transcript)
        os.write(master, b"/")
        wait_for(b"/model", start)
        deadline = time.monotonic() + 10
        while select.select([master], [], [], 0.1)[0]:
            assert time.monotonic() < deadline, "terminal never finished drawing"
            transcript.extend(os.read(master, 65536))
        result = list(transcript)
        os.write(master, b"\x03/quit\r")
        assert child.wait(timeout=10) == 0, transcript.decode(errors="replace")
        return result
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
        os.close(master)


with tempfile.TemporaryDirectory(prefix="rune-r071-") as directory:
    root = pathlib.Path(directory)
    config = root / "config" / "rune"
    config.mkdir(parents=True, mode=0o700)
    config_file = config / "config.toml"
    config_file.write_text("auto_upgrade = false\n")
    config_file.chmod(0o600)
    environment = {
        key: value for key, value in os.environ.items()
        if not key.startswith("RUNE_") and key not in {"NO_COLOR", "COLORTERM"}
    }
    environment.update({
        "HOME": directory,
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_STATE_HOME": str(root / "xdg-state"),
        "RUNE_HOME": str(root / "state"),
        "RUNE_PROVIDER": "chat_completions",
        "RUNE_BASE_URL": "http://127.0.0.1:9/v1",
        "RUNE_MODEL": "contrast-fixture",
        "RUNE_API_KEY_ENV": "RUNE_CONTRAST_TEST_KEY",
        "RUNE_CONTRAST_TEST_KEY": "fixture-key",
        "TERM": "xterm-256color",
    })
    captures = []
    for stage, overrides in [
        ("truecolor", {"COLORTERM": "truecolor"}),
        ("indexed", {}),
        ("no-color", {"COLORTERM": "truecolor", "NO_COLOR": "1"}),
        ("no-color-empty", {"COLORTERM": "truecolor", "NO_COLOR": ""}),
    ]:
        output = capture(sys.argv[1], root, environment | overrides)
        if stage.startswith("no-color"):
            for sequence in re.findall(rb"\x1b\[([0-9;]*)m", bytes(output)):
                parameters = [int(value or b"0") for value in sequence.split(b";")]
                assert not any(
                    value in {38, 48}
                    or 30 <= value <= 37 or 40 <= value <= 47
                    or 90 <= value <= 97 or 100 <= value <= 107
                    for value in parameters
                ), f"{stage} emitted colors: {sequence!r}"
        captures.append((stage, output))
    print(json.dumps(captures))
