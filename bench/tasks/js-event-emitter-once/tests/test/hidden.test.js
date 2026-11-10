const test = require("node:test");
const assert = require("node:assert");
const { Emitter } = require("../src/emitter.js");

test("removing a different listener during emit does not skip a later one", () => {
  const emitter = new Emitter();
  const seen = [];
  const second = () => seen.push("second");
  emitter.on("x", () => {
    seen.push("first");
    emitter.off("x", second);
  });
  emitter.on("x", second);
  emitter.on("x", () => seen.push("third"));
  emitter.emit("x");
  assert.ok(seen.includes("first"));
  assert.ok(seen.includes("third"));
  seen.length = 0;
  emitter.emit("x");
  assert.deepStrictEqual(seen, ["first", "third"]);
});

test("a once listener that re-emits the same event runs once", () => {
  const emitter = new Emitter();
  let calls = 0;
  emitter.once("x", () => {
    calls++;
    if (calls < 5) emitter.emit("x");
  });
  emitter.emit("x");
  assert.strictEqual(calls, 1);
  assert.strictEqual(emitter.listenerCount("x"), 0);
});

test("once listeners added during an emit wait for the next emit", () => {
  const emitter = new Emitter();
  const seen = [];
  emitter.once("x", () => {
    seen.push("outer");
    emitter.once("x", () => seen.push("inner"));
  });
  emitter.emit("x");
  assert.deepStrictEqual(seen, ["outer"]);
  emitter.emit("x");
  assert.deepStrictEqual(seen, ["outer", "inner"]);
});

test("two once listeners and a regular one all run on a single emit", () => {
  const emitter = new Emitter();
  const seen = [];
  emitter.once("x", () => seen.push(1));
  emitter.once("x", () => seen.push(2));
  emitter.on("x", () => seen.push(3));
  emitter.emit("x");
  emitter.emit("x");
  assert.deepStrictEqual(seen, [1, 2, 3, 3]);
});
