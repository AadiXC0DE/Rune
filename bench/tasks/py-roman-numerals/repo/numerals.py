"""Conversion between integers and Roman numerals."""

SYMBOLS = [
    (1000, "M"),
    (500, "D"),
    (100, "C"),
    (50, "L"),
    (10, "X"),
    (5, "V"),
    (1, "I"),
]

VALUES = {symbol: value for value, symbol in SYMBOLS}


def to_roman(number):
    """Return the Roman numeral for a whole number."""
    out = []
    for value, symbol in SYMBOLS:
        count, number = divmod(number, value)
        out.append(symbol * count)
    return "".join(out)


def from_roman(text):
    """Return the whole number a Roman numeral stands for."""
    total = 0
    for char in text:
        total += VALUES[char]
    return total
