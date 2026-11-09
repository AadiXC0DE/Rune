"""Line-level helpers for the INI reader."""


def strip_comment(line):
    """Remove a full-line or inline comment, returning '' for a comment line.

    An inline comment starts at a `#` or `;` preceded by whitespace and outside
    double quotes.
    """
    stripped = line.strip()
    if stripped.startswith(("#", ";")):
        return ""
    quoted = False
    for index, char in enumerate(stripped):
        if char == '"':
            quoted = not quoted
        elif char in "#;" and not quoted and index > 0 and stripped[index - 1].isspace():
            return stripped[:index].rstrip()
    return stripped


def split_pair(line):
    """Split `key = value` into a stripped key and value, unquoting the value."""
    if "=" not in line:
        raise ValueError(f"expected key = value, got {line!r}")
    key, value = line.split("=", 1)
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] == '"':
        value = value[1:-1]
    return key.strip(), value
