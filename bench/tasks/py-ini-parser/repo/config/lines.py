"""Line-level helpers for the INI reader."""


def strip_comment(line):
    """Remove a full-line comment, returning '' for one."""
    stripped = line.strip()
    if stripped.startswith(("#", ";")):
        return ""
    return stripped


def split_pair(line):
    """Split `key = value` into a stripped key and value."""
    parts = line.split("=")
    return parts[0].strip(), parts[1].strip()
