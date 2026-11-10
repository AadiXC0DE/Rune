const catalog = require("../generated/catalog.js");
const pricing = require("../../pricing");

/**
 * Prices a bundle from the catalog. Returns the per-unit prices as shown in
 * the storefront and the bundle total in cents.
 */
function priceBundle(id) {
  const bundle = catalog.bundles[id];
  if (!bundle) throw new Error("unknown bundle: " + id);
  const lines = bundle.lines.map((line) => ({
    sku: line.sku,
    qty: line.qty,
    unitCents: catalog.items[line.sku].unitCents,
  }));
  const discount = { percent: bundle.percent };
  return {
    name: bundle.name,
    units: lines.map((line) => ({
      sku: line.sku,
      qty: line.qty,
      unitCents: pricing.discountedUnit(line.unitCents, discount),
    })),
    totalCents: pricing.discountedTotal(lines, discount, { bundle: true }),
  };
}

module.exports = { priceBundle };
