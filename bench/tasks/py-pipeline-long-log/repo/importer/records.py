from decimal import ROUND_HALF_UP, Decimal


def parse_price(text):
    """Return a price as whole cents. A leading dollar sign is allowed."""
    amount = Decimal(text.strip().lstrip("$"))
    return int((amount * 100).quantize(Decimal(1), rounding=ROUND_HALF_UP))


def parse_record(raw):
    """Turn a raw dictionary into a record with id, name and price_cents."""
    return {
        "id": int(raw["id"]),
        "name": raw["name"].strip(),
        "price_cents": parse_price(raw["price"]),
    }
