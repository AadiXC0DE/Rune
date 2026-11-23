import test from "node:test";
import assert from "node:assert/strict";
import { merge } from "../src/merge.ts";
import { loadConfig } from "../src/load.ts";

test("scalars in the override win", () => {
  assert.deepEqual(merge({ a: 1, b: 2 }, { b: 3 }), { a: 1, b: 3 });
});

test("nested objects merge key by key", () => {
  const base = { editor: { tabSize: 2, wrap: true }, name: "x" };
  const result = merge(base, { editor: { tabSize: 4 } });
  assert.deepEqual(result, { editor: { tabSize: 4, wrap: true }, name: "x" });
});

test("arrays are replaced, not joined", () => {
  assert.deepEqual(merge({ paths: ["a", "b"] }, { paths: ["c"] }), { paths: ["c"] });
});

test("null removes a key", () => {
  assert.deepEqual(merge({ a: 1, b: 2 }, { a: null }), { b: 2 });
});

test("the inputs are not changed", () => {
  const base = { nested: { x: 1 } };
  const override = { nested: { y: 2 } };
  merge(base, override);
  assert.deepEqual(base, { nested: { x: 1 } });
  assert.deepEqual(override, { nested: { y: 2 } });
});

test("the user file overrides the defaults", () => {
  const config = loadConfig({
    defaults: { theme: "light", editor: { tabSize: 2 } },
    user: { theme: "dark" },
  });
  assert.deepEqual(config, { theme: "dark", editor: { tabSize: 2 } });
});
