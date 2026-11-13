const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const { priceBundle } = require("../src/cart.js");
const { render } = require("../scripts/gen.js");

test("the desk set total matches its unit prices", () => {
  const bundle = priceBundle("desk-set");
  const fromUnits = bundle.units.reduce((sum, unit) => sum + unit.unitCents * unit.qty, 0);
  assert.strictEqual(bundle.totalCents, fromUnits);
  assert.strictEqual(bundle.totalCents, 4592);
});

test("unit prices are the discounted catalog prices", () => {
  const bundle = priceBundle("desk-set");
  assert.deepStrictEqual(
    bundle.units.map((unit) => unit.unitCents),
    [1063, 285, 1611],
  );
});

test("the pen pack total matches its unit price", () => {
  const bundle = priceBundle("pen-pack");
  assert.strictEqual(bundle.units[0].unitCents, 302);
  assert.strictEqual(bundle.totalCents, 1812);
});

test("an unknown bundle is an error", () => {
  assert.throws(() => priceBundle("nope"), /unknown bundle/);
});

test("the generated catalog is in sync with catalog.json", () => {
  const file = fs.readFileSync(path.join(__dirname, "..", "generated", "catalog.js"), "utf8");
  assert.strictEqual(file, render());
});
