const pricing = require("../../pricing");

/**
 * Amount to invoice in cents: the discounted total of the lines plus
 * shipping. Invoices round the discount once, on the whole subtotal.
 */
function invoiceTotal(lines, discount, { shippingCents = 0 } = {}) {
  return pricing.discountedTotal(lines, discount) + shippingCents;
}

module.exports = { invoiceTotal };
