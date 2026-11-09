const test = require("node:test");
const assert = require("node:assert");
const { LRUCache } = require("../src/lru.js");

test("stores and returns values", () => {
  const cache = new LRUCache(2);
  cache.set("a", 1);
  assert.strictEqual(cache.get("a"), 1);
  assert.strictEqual(cache.get("missing"), undefined);
});

test("evicts the least recently used entry", () => {
  const cache = new LRUCache(2);
  cache.set("a", 1);
  cache.set("b", 2);
  cache.set("c", 3);
  assert.strictEqual(cache.get("a"), undefined);
  assert.deepStrictEqual(cache.keys(), ["b", "c"]);
});

test("a read makes an entry recent", () => {
  const cache = new LRUCache(2);
  cache.set("a", 1);
  cache.set("b", 2);
  cache.get("a");
  cache.set("c", 3);
  assert.strictEqual(cache.get("b"), undefined);
  assert.deepStrictEqual(cache.keys(), ["a", "c"]);
});

test("overwriting refreshes recency without growing", () => {
  const cache = new LRUCache(2);
  cache.set("a", 1);
  cache.set("b", 2);
  cache.set("a", 10);
  cache.set("c", 3);
  assert.strictEqual(cache.size, 2);
  assert.strictEqual(cache.get("a"), 10);
  assert.strictEqual(cache.get("b"), undefined);
});

test("stored falsy values are hits", () => {
  const cache = new LRUCache(3);
  cache.set("zero", 0);
  cache.set("empty", "");
  cache.set("none", null);
  assert.strictEqual(cache.get("zero"), 0);
  assert.strictEqual(cache.get("empty"), "");
  assert.strictEqual(cache.get("none"), null);
  assert.strictEqual(cache.size, 3);
});

test("delete removes an entry and reports whether it existed", () => {
  const cache = new LRUCache(2);
  cache.set("a", 1);
  assert.strictEqual(cache.delete("a"), true);
  assert.strictEqual(cache.delete("a"), false);
  assert.strictEqual(cache.size, 0);
});

test("a capacity below one is refused", () => {
  assert.throws(() => new LRUCache(0), RangeError);
  assert.throws(() => new LRUCache(1.5), RangeError);
});
