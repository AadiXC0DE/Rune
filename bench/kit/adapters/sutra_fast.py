"""Sutra, fast quality: the balanced adapter with `--quality fast`.

Command line from the Sutra report, section 2.4. VERIFIED flips to True only after a clean
real run.
"""

import importlib.util
import os

_spec = importlib.util.spec_from_file_location(
    "kit_adapter_sutra_base", os.path.join(os.path.dirname(os.path.abspath(__file__)), "sutra.py")
)
_base = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_base)

NAME = "sutra-fast"
BIN = _base.BIN
VERSION_CMD = _base.VERSION_CMD
HELP_CMD = _base.HELP_CMD
HELP_FLAGS = _base.HELP_FLAGS
DIALECTS = _base.DIALECTS
VERIFIED = False
find_bin = _base.find_bin


def prepare(ctx):
    launch = _base.prepare(ctx)
    argv = launch["argv"]
    argv[argv.index("--quality") + 1] = "fast"
    return launch
