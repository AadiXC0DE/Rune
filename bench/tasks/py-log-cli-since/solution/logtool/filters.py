import re
from datetime import datetime, timezone

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


def parse_stamp(text):
    """Parse an ISO 8601 timestamp; a missing zone means UTC."""
    if text.endswith(("Z", "z")):
        text = text[:-1] + "+00:00"
    moment = datetime.fromisoformat(text)
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=timezone.utc)
    return moment


def keep_since(stamp, since):
    """True when the entry stamp is at or after since. Unreadable stamps are dropped."""
    try:
        return parse_stamp(stamp) >= since
    except ValueError:
        return False
