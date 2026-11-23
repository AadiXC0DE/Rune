const test = require("node:test");
const assert = require("node:assert");
const { invoiceTotal } = require("../src/invoice.js");

const lines = [
  { sku: "notebook", qty: 2, unitCents: 1250 },
  { sku: "pen", qty: 3, unitCents: 335 },
  { sku: "stapler", qty: 1, unitCents: 1895 },
];

test("an invoice without a discount is the subtotal", () => {
  assert.strictEqual(invoiceTotal(lines, null), 5400);
});

test("the discount is rounded once on the whole subtotal", () => {
  assert.strictEqual(invoiceTotal(lines, { percent: 15 }), 4590);
});

test("shipping is added after the discount", () => {
  assert.strictEqual(invoiceTotal(lines, { percent: 15 }, { shippingCents: 499 }), 5089);
});
