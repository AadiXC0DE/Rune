const test = require("node:test");
const assert = require("node:assert");
const { Emitter } = require("../src/emitter.js");

test("listeners run in the order they were added", () => {
  const emitter = new Emitter();
  const seen = [];
  emitter.on("x", () => seen.push("a"));
  emitter.on("x", () => seen.push("b"));
  assert.strictEqual(emitter.emit("x"), true);
  assert.deepStrictEqual(seen, ["a", "b"]);
});

test("emit passes arguments and reports whether anyone listened", () => {
  const emitter = new Emitter();
  let received;
  emitter.on("x", (...args) => { received = args; });
  emitter.emit("x", 1, "two");
  assert.deepStrictEqual(received, [1, "two"]);
  assert.strictEqual(emitter.emit("nobody"), false);
});

test("a once listener runs for the first emit only", () => {
  const emitter = new Emitter();
  let calls = 0;
  emitter.once("x", () => { calls++; });
  emitter.emit("x");
  emitter.emit("x");
  assert.strictEqual(calls, 1);
  assert.strictEqual(emitter.listenerCount("x"), 0);
});

test("off removes a once listener before it runs", () => {
  const emitter = new Emitter();
  let calls = 0;
  const listener = () => { calls++; };
  emitter.once("x", listener);
  emitter.off("x", listener);
  emitter.emit("x");
  assert.strictEqual(calls, 0);
});

test("a listener that removes itself does not make the next one skip", () => {
  const emitter = new Emitter();
  const seen = [];
  const first = () => {
    seen.push("first");
    emitter.off("x", first);
  };
  emitter.on("x", first);
  emitter.on("x", () => seen.push("second"));
  emitter.emit("x");
  assert.deepStrictEqual(seen, ["first", "second"]);
  emitter.emit("x");
  assert.deepStrictEqual(seen, ["first", "second", "second"]);
});
