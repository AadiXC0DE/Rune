const test = require("node:test");
const assert = require("node:assert");
const { LRUCache } = require("../src/lru.js");

test("a capacity of one keeps only the newest entry", () => {
  const cache = new LRUCache(1);
  cache.set("a", 1);
  cache.set("b", 2);
  assert.strictEqual(cache.get("a"), undefined);
  assert.strictEqual(cache.get("b"), 2);
  assert.strictEqual(cache.size, 1);
});

test("a missing key stays missing after its eviction", () => {
  const cache = new LRUCache(2);
  cache.set("a", 1);
  cache.set("b", 2);
  cache.set("c", 3);
  assert.strictEqual(cache.get("a"), undefined);
  assert.strictEqual(cache.get("a"), undefined);
  assert.strictEqual(cache.size, 2);
  assert.deepStrictEqual(cache.keys(), ["b", "c"]);
});

test("overwriting a key refreshes its recency", () => {
  const cache = new LRUCache(3);
  cache.set("a", 1);
  cache.set("b", 2);
  cache.set("c", 3);
  cache.set("a", 9);
  cache.set("d", 4);
  assert.strictEqual(cache.get("b"), undefined);
  assert.deepStrictEqual(cache.keys(), ["c", "a", "d"]);
});
