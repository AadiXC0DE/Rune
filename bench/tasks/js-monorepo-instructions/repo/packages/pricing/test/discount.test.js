const test = require("node:test");
const assert = require("node:assert");
const { subtotalCents, discountedUnit, discountedTotal } = require("../src/discount.js");

const lines = [
  { sku: "a", qty: 3, unitCents: 1005 },
  { sku: "b", qty: 1, unitCents: 200 },
];

test("the subtotal is unit price times quantity", () => {
  assert.strictEqual(subtotalCents(lines), 3215);
});

test("a unit price is rounded to a whole cent", () => {
  assert.strictEqual(discountedUnit(1005, { percent: 15 }), 854);
  assert.strictEqual(discountedUnit(1005, null), 1005);
});

test("by default the discount is rounded once on the subtotal", () => {
  assert.strictEqual(discountedTotal(lines, { percent: 15 }), 2733);
});

test("a bundle total is the rounded unit prices times quantities", () => {
  assert.strictEqual(discountedTotal(lines, { percent: 15 }, { bundle: true }), 854 * 3 + 170);
});
