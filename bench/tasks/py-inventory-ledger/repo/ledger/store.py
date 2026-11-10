class Ledger:
    """Stock movements. A positive quantity is a sale, a negative one a return."""

    def __init__(self):
        self.movements = []

    def record(self, sku, quantity, price):
        """Add a movement of quantity units of sku at a unit price in currency units."""
        if quantity == 0:
            raise ValueError("quantity must not be zero")
        self.movements.append({"sku": sku, "quantity": quantity, "price": float(price)})
