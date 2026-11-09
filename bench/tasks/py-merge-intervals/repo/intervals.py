"""Merging of closed integer intervals."""


def merge(intervals):
    """Return the union of closed intervals as sorted, non-overlapping pairs.

    Intervals that overlap or touch, such as (1, 3) and (3, 5), become one.
    The input is not modified.
    """
    result = []
    for start, end in intervals:
        if result and start < result[-1][1]:
            result[-1][1] = max(result[-1][1], end)
        else:
            result.append([start, end])
    return [tuple(pair) for pair in result]
