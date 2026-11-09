"""A small INI reader: sections, `key = value` pairs and comments."""

from .lines import split_pair, strip_comment


def parse(text):
    """Return {section: {key: value}}. Keys before any section go in ''."""
    sections = {}
    current = ""
    for raw in text.splitlines():
        line = strip_comment(raw)
        if not line:
            continue
        if line.startswith("[") and line.endswith("]"):
            current = line[1:-1].strip()
            sections.setdefault(current, {})
            continue
        key, value = split_pair(line)
        sections.setdefault(current, {})[key] = value
    return sections
