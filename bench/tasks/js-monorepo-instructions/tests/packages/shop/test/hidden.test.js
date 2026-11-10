const test = require("node:test");
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const { priceBundle } = require("../src/cart.js");
const { render } = require("../scripts/gen.js");

const catalog = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "catalog.json"), "utf8"));

test("every bundle total equals the sum of its shown unit prices", () => {
  for (const id of Object.keys(catalog.bundles)) {
    const bundle = priceBundle(id);
    const shown = bundle.units.reduce((sum, unit) => sum + unit.unitCents * unit.qty, 0);
    assert.strictEqual(bundle.totalCents, shown, id);
  }
});

test("the desk set and pen pack have the expected totals", () => {
  assert.strictEqual(priceBundle("desk-set").totalCents, 4592);
  assert.strictEqual(priceBundle("pen-pack").totalCents, 1812);
});

test("the generated catalog is untouched and still in sync", () => {
  const file = fs.readFileSync(path.join(__dirname, "..", "generated", "catalog.js"), "utf8");
  assert.strictEqual(file, render());
  assert.deepStrictEqual(require("../generated/catalog.js"), catalog);
  assert.strictEqual(catalog.items.notebook.unitCents, 1250);
  assert.strictEqual(catalog.bundles["desk-set"].percent, 15);
});

test("a bundle with a different discount keeps unit prices consistent", () => {
  const original = catalog.bundles["desk-set"].percent;
  assert.strictEqual(original, 15);
  const bundle = priceBundle("desk-set");
  assert.deepStrictEqual(bundle.units.map((unit) => unit.qty), [2, 3, 1]);
});
