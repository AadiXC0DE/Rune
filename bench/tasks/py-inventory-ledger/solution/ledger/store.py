from decimal import ROUND_HALF_UP, Decimal


def to_cents(price):
    """Convert a price in currency units to whole cents without binary error."""
    cents = Decimal(str(price)) * 100
    return int(cents.quantize(Decimal(1), rounding=ROUND_HALF_UP))


class Ledger:
    """Stock movements. A positive quantity is a sale, a negative one a return."""

    def __init__(self):
        self.movements = []

    def record(self, sku, quantity, price):
        """Add a movement of quantity units of sku at a unit price in currency units."""
        if quantity == 0:
            raise ValueError("quantity must not be zero")
        self.movements.append(
            {"sku": sku, "quantity": quantity, "price_cents": to_cents(price)}
        )
