def summarize(ledger):
    """Return per-sku totals: units_sold, units_returned and revenue_cents."""
    out = {}
    for move in ledger.movements:
        row = out.setdefault(
            move["sku"], {"units_sold": 0, "units_returned": 0, "revenue_cents": 0}
        )
        quantity = move["quantity"]
        if quantity > 0:
            row["units_sold"] += quantity
        else:
            row["units_returned"] += -quantity
        row["revenue_cents"] += quantity * move["price_cents"]
    return out
