def format_duration(seconds):
    """Return a short text for a number of seconds."""
    minutes, rest = divmod(seconds, 60)
    out = ""
    if minutes:
        out += "%dm" % minutes
    if rest or not out:
        out += "%ds" % rest
    return out
