import re

SECONDS = {"s": 1, "m": 60, "h": 3600, "d": 86400}

WHOLE = re.compile(r"(?:\d+[dhms])+")
PART = re.compile(r"(\d+)([dhms])")


def parse_duration(text):
    """Return the number of seconds in a text such as 1h30m, 90s or 2d."""
    if not WHOLE.fullmatch(text):
        raise ValueError("not a duration: %r" % (text,))
    total = 0
    for amount, unit in PART.findall(text):
        total += int(amount) * SECONDS[unit]
    return total
