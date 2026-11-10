const test = require("node:test");
const assert = require("node:assert");
const { delayFor } = require("../src/backoff.js");
const { withRetry } = require("../src/retry.js");

test("the delay equals the cap exactly when doubling lands on it", () => {
  assert.strictEqual(delayFor(4, { base: 125, cap: 1000 }), 1000);
  assert.strictEqual(delayFor(3, { base: 125, cap: 1000 }), 500);
  assert.strictEqual(delayFor(5, { base: 125, cap: 1000 }), 1000);
});

test("zero retries never sleeps and rethrows the first error", async () => {
  const delays = [];
  const failure = new Error("first");
  await assert.rejects(
    withRetry(async () => { throw failure; }, { retries: 0, sleep: async (ms) => delays.push(ms) }),
    (error) => error === failure,
  );
  assert.deepStrictEqual(delays, []);
});

test("the cap applies to delays inside withRetry", async () => {
  const delays = [];
  await assert.rejects(
    withRetry(async () => { throw new Error("x"); }, {
      retries: 5, base: 100, cap: 300, sleep: async (ms) => delays.push(ms),
    }),
  );
  assert.deepStrictEqual(delays, [100, 200, 300, 300, 300]);
});
