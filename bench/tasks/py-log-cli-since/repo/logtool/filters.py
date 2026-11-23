import re

LEVELS = ["DEBUG", "INFO", "WARN", "ERROR"]

LINE = re.compile(r"^(\S+)\s+(DEBUG|INFO|WARN|ERROR)\s+(.*)$")


def parse_line(line):
    """Return (stamp, level, message) for a log line, or None if it is not an entry."""
    match = LINE.match(line.rstrip("\n"))
    if match is None:
        return None
    return match.group(1), match.group(2), match.group(3)


def keep_level(level, minimum):
    """True when level is at or above minimum in severity."""
    return LEVELS.index(level) >= LEVELS.index(minimum)
