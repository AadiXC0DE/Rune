/** Sum of unit price times quantity over the lines, in cents. */
function subtotalCents(lines) {
  return lines.reduce((sum, line) => sum + line.unitCents * line.qty, 0);
}

/** Price of one unit after a percent discount, rounded to a whole cent. */
function discountedUnit(unitCents, discount) {
  const percent = discount && discount.percent ? discount.percent : 0;
  return Math.round((unitCents * (100 - percent)) / 100);
}

/**
 * Total in cents after a percent discount.
 *
 * `options.bundle` marks a total that is shown next to per-unit prices: each
 * unit is priced after the discount, rounded to a whole cent, and then
 * multiplied by its quantity. Without it the discount is taken once off the
 * subtotal and rounded once.
 */
function discountedTotal(lines, discount, options = {}) {
  const percent = discount && discount.percent ? discount.percent : 0;
  return Math.round((subtotalCents(lines) * (100 - percent)) / 100);
}

module.exports = { subtotalCents, discountedUnit, discountedTotal };
