UNITS = (("d", 86400), ("h", 3600), ("m", 60), ("s", 1))


def format_duration(seconds):
    """Return a short text for a number of seconds."""
    if seconds < 0:
        raise ValueError("negative duration")
    out = ""
    for unit, size in UNITS:
        count, seconds = divmod(seconds, size)
        if count:
            out += "%d%s" % (count, unit)
    return out or "0s"
