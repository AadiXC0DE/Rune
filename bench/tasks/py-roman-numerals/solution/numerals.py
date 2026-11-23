"""Conversion between integers and Roman numerals."""

SYMBOLS = [
    (1000, "M"),
    (900, "CM"),
    (500, "D"),
    (400, "CD"),
    (100, "C"),
    (90, "XC"),
    (50, "L"),
    (40, "XL"),
    (10, "X"),
    (9, "IX"),
    (5, "V"),
    (4, "IV"),
    (1, "I"),
]


def to_roman(number):
    """Return the Roman numeral for a whole number from 1 to 3999."""
    if isinstance(number, bool) or not isinstance(number, int):
        raise ValueError("expected an integer")
    if not 1 <= number <= 3999:
        raise ValueError("out of range: %r" % (number,))
    out = []
    for value, symbol in SYMBOLS:
        count, number = divmod(number, value)
        out.append(symbol * count)
    return "".join(out)


def from_roman(text):
    """Return the whole number a Roman numeral stands for."""
    pos = 0
    total = 0
    for value, symbol in SYMBOLS:
        while text.startswith(symbol, pos):
            total += value
            pos += len(symbol)
    if pos != len(text) or total == 0 or to_roman(total) != text:
        raise ValueError("not a valid Roman numeral: %r" % (text,))
    return total
