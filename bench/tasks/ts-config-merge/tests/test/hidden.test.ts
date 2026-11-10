import test from "node:test";
import assert from "node:assert/strict";
import { merge } from "../src/merge.ts";
import { loadConfig } from "../src/load.ts";

test("three layers apply defaults, then user, then project", () => {
  const config = loadConfig({
    defaults: { log: { level: "info", file: "a.log" }, retries: 3 },
    user: { log: { level: "debug" }, retries: 5 },
    project: { log: { file: "b.log" } },
  });
  assert.deepEqual(config, { log: { level: "debug", file: "b.log" }, retries: 5 });
});

test("a project null removes a key set by the user layer", () => {
  const config = loadConfig({
    defaults: { proxy: { host: "h", port: 1 } },
    user: { proxy: { port: 2 } },
    project: { proxy: null },
  });
  assert.deepEqual(config, {});
});

test("null at depth three removes only that key", () => {
  const base = { a: { b: { c: 1, d: 2 }, e: 3 } };
  assert.deepEqual(merge(base, { a: { b: { c: null } } }), { a: { b: { d: 2 }, e: 3 } });
  assert.deepEqual(base, { a: { b: { c: 1, d: 2 }, e: 3 } });
});

test("null for an absent key and null inside a new object leave nothing behind", () => {
  assert.deepEqual(merge({ a: 1 }, { missing: null }), { a: 1 });
  assert.deepEqual(merge({}, { fresh: { keep: 1, drop: null } }), { fresh: { keep: 1 } });
});

test("an array of objects is replaced, not merged by index", () => {
  const base = { servers: [{ host: "a", port: 1 }, { host: "b", port: 2 }] };
  const result = merge(base, { servers: [{ host: "c" }] });
  assert.deepEqual(result, { servers: [{ host: "c" }] });
});

test("an object replaces a scalar and a scalar replaces an object", () => {
  assert.deepEqual(merge({ a: 1 }, { a: { b: 2 } }), { a: { b: 2 } });
  assert.deepEqual(merge({ a: { b: 2 } }, { a: 7 }), { a: 7 });
});

test("the result does not share nested objects with the layers", () => {
  const defaults = { list: [1, 2], deep: { x: { y: 1 } } };
  const config = loadConfig({ defaults });
  (config.deep as { x: { y: number } }).x.y = 99;
  (config.list as number[]).push(3);
  assert.deepEqual(defaults, { list: [1, 2], deep: { x: { y: 1 } } });
});
