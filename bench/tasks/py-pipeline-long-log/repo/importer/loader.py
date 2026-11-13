import json

from .records import parse_record


def load(path):
    """Yield one parsed record per non-empty line of a JSON lines file."""
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if line:
                yield parse_record(json.loads(line))
