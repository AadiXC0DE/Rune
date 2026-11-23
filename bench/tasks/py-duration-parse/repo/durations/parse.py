import re

SECONDS = {"s": 1, "m": 60, "h": 3600}

PART = re.compile(r"(\d+)([hms])")


def parse_duration(text):
    """Return the number of seconds in a text such as 1h30m, 90s or 2d."""
    total = 0
    for amount, unit in PART.findall(text):
        total += int(amount) * SECONDS[unit]
    return total
