import re
from decimal import ROUND_HALF_UP, Decimal

TEXT_PRICE = re.compile(r"^\$?\s*(\d+(?:\.\d+)?)(?:\s+[A-Za-z]{3})?$")


def to_cents(amount):
    return int((amount * 100).quantize(Decimal(1), rounding=ROUND_HALF_UP))


def parse_price(value):
    """Return a price as whole cents, or None when there is no price.

    Accepts numbers, and text with an optional leading dollar sign and an
    optional three letter currency suffix.
    """
    if value is None:
        return None
    if isinstance(value, bool):
        raise ValueError("not a price: %r" % (value,))
    if isinstance(value, (int, float)):
        return to_cents(Decimal(str(value)))
    match = TEXT_PRICE.match(str(value).strip())
    if match is None:
        raise ValueError("not a price: %r" % (value,))
    return to_cents(Decimal(match.group(1)))


def parse_record(raw):
    """Turn a raw dictionary into a record with id, name and price_cents."""
    return {
        "id": int(raw["id"]),
        "name": raw["name"].strip(),
        "price_cents": parse_price(raw.get("price")),
    }
