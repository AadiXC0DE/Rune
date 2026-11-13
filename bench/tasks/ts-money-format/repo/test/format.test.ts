import test from "node:test";
import assert from "node:assert/strict";
import { formatMoney } from "../src/format.ts";
import { renderInvoice } from "../src/invoice.ts";

test("formats whole and fractional amounts", () => {
  assert.equal(formatMoney(0), "$0.00");
  assert.equal(formatMoney(12.5), "$12.50");
  assert.equal(formatMoney(999.99), "$999.99");
});

test("negative amounts keep their sign", () => {
  assert.equal(formatMoney(-5), "-$5.00");
  assert.equal(formatMoney(-0.5), "-$0.50");
  assert.equal(formatMoney(-42.25), "-$42.25");
});

test("thousands are grouped", () => {
  assert.equal(formatMoney(1000), "$1,000.00");
  assert.equal(formatMoney(1234.5), "$1,234.50");
  assert.equal(formatMoney(-98765.43), "-$98,765.43");
});

test("an invoice lists the rows and a total", () => {
  const text = renderInvoice([
    { description: "Design", amount: 1500 },
    { description: "Refund", amount: -250.5 },
  ]);
  assert.equal(text, "Design: $1,500.00\nRefund: -$250.50\nTotal: $1,249.50");
});
