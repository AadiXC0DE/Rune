import test from "node:test";
import assert from "node:assert/strict";
import { formatMoney } from "../src/format.ts";
import { renderInvoice } from "../src/invoice.ts";

test("a negative half cent rounds away from zero and keeps its sign", () => {
  assert.equal(formatMoney(-0.005), "-$0.01");
  assert.equal(formatMoney(-0.125), "-$0.13");
});

test("a value that rounds to zero has no sign", () => {
  assert.equal(formatMoney(-0.004), "$0.00");
});

test("large numbers are rounded and grouped", () => {
  assert.equal(formatMoney(1234567.891), "$1,234,567.89");
  assert.equal(formatMoney(1000000), "$1,000,000.00");
  assert.equal(formatMoney(999999.999), "$1,000,000.00");
});

test("the total equals the sum of the rows as displayed", () => {
  const rows = [
    { description: "A", amount: 0.125 },
    { description: "B", amount: 0.375 },
    { description: "C", amount: 0.625 },
  ];
  assert.equal(renderInvoice(rows), "A: $0.13\nB: $0.38\nC: $0.63\nTotal: $1.14");
});

test("floating point sums do not leak into the total", () => {
  const rows = [
    { description: "A", amount: 0.1 },
    { description: "B", amount: 0.2 },
    { description: "C", amount: 0.7 },
  ];
  assert.ok(renderInvoice(rows).endsWith("Total: $1.00"));
});
