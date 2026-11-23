def summarize(ledger):
    """Return per-sku totals: units_sold, units_returned and revenue_cents."""
    totals = {}
    for move in ledger.movements:
        row = totals.setdefault(
            move["sku"], {"units_sold": 0, "units_returned": 0, "revenue": 0.0}
        )
        row["units_sold"] += abs(move["quantity"])
        row["revenue"] += abs(move["quantity"]) * move["price"]
    out = {}
    for sku, row in totals.items():
        out[sku] = {
            "units_sold": row["units_sold"],
            "units_returned": row["units_returned"],
            "revenue_cents": int(row["revenue"] * 100),
        }
    return out
