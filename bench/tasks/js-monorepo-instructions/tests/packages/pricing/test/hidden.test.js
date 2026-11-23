const test = require("node:test");
const assert = require("node:assert");
const { discountedUnit, discountedTotal } = require("../src/discount.js");

test("a bundle total is the sum of rounded units for several lines", () => {
  const lines = [
    { sku: "x", qty: 4, unitCents: 999 },
    { sku: "y", qty: 2, unitCents: 101 },
  ];
  const discount = { percent: 25 };
  const expected = discountedUnit(999, discount) * 4 + discountedUnit(101, discount) * 2;
  assert.strictEqual(discountedTotal(lines, discount, { bundle: true }), expected);
  assert.strictEqual(expected, 749 * 4 + 76 * 2);
});

test("without the bundle option the discount is rounded once", () => {
  const lines = [
    { sku: "x", qty: 4, unitCents: 999 },
    { sku: "y", qty: 2, unitCents: 101 },
  ];
  assert.strictEqual(discountedTotal(lines, { percent: 25 }), 3149);
  assert.strictEqual(discountedTotal(lines, { percent: 25 }, {}), 3149);
  assert.strictEqual(discountedTotal(lines, { percent: 25 }, { bundle: false }), 3149);
});

test("no discount gives the subtotal either way", () => {
  const lines = [{ sku: "x", qty: 3, unitCents: 333 }];
  assert.strictEqual(discountedTotal(lines, null), 999);
  assert.strictEqual(discountedTotal(lines, null, { bundle: true }), 999);
  assert.strictEqual(discountedTotal(lines, { percent: 0 }, { bundle: true }), 999);
});

test("an empty cart is zero", () => {
  assert.strictEqual(discountedTotal([], { percent: 10 }, { bundle: true }), 0);
  assert.strictEqual(discountedTotal([], { percent: 10 }), 0);
});
