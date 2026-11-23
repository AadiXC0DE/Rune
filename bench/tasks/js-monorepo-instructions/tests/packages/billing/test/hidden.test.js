const test = require("node:test");
const assert = require("node:assert");
const { invoiceTotal } = require("../src/invoice.js");

test("an invoice rounds the discount once, not per unit", () => {
  const lines = [{ sku: "pen", qty: 6, unitCents: 335 }];
  assert.strictEqual(invoiceTotal(lines, { percent: 10 }), 1809);
});

test("a discount that lands on whole cents is the same as the subtotal minus the discount", () => {
  const lines = [{ sku: "a", qty: 2, unitCents: 2500 }];
  assert.strictEqual(invoiceTotal(lines, { percent: 20 }), 4000);
});

test("shipping is not discounted", () => {
  const lines = [{ sku: "a", qty: 1, unitCents: 1000 }];
  assert.strictEqual(invoiceTotal(lines, { percent: 50 }, { shippingCents: 300 }), 800);
});

test("many odd-priced lines round once on the subtotal", () => {
  const lines = [
    { sku: "a", qty: 1, unitCents: 101 },
    { sku: "b", qty: 1, unitCents: 101 },
    { sku: "c", qty: 1, unitCents: 101 },
  ];
  assert.strictEqual(invoiceTotal(lines, { percent: 50 }), 152);
});
