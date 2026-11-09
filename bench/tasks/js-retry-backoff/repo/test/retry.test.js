const test = require("node:test");
const assert = require("node:assert");
const { delayFor } = require("../src/backoff.js");
const { withRetry } = require("../src/retry.js");

test("delays double from the base and stop at the cap", () => {
  assert.deepStrictEqual(
    [1, 2, 3, 4, 5, 6].map((retry) => delayFor(retry, { base: 100, cap: 1000 })),
    [100, 200, 400, 800, 1000, 1000],
  );
});

test("retries the given number of times after the first attempt", async () => {
  const delays = [];
  let calls = 0;
  await assert.rejects(
    withRetry(
      async () => {
        calls++;
        throw new Error(`fail ${calls}`);
      },
      { retries: 3, base: 10, cap: 1000, sleep: async (ms) => delays.push(ms) },
    ),
    /fail 4/,
  );
  assert.strictEqual(calls, 4);
  // No delay follows the final failure.
  assert.deepStrictEqual(delays, [10, 20, 40]);
});

test("returns as soon as an attempt succeeds", async () => {
  const delays = [];
  const result = await withRetry(
    async (attempt) => {
      if (attempt < 3) throw new Error("not yet");
      return "ok";
    },
    { retries: 5, base: 10, sleep: async (ms) => delays.push(ms) },
  );
  assert.strictEqual(result, "ok");
  assert.deepStrictEqual(delays, [10, 20]);
});

test("zero retries means exactly one attempt", async () => {
  let calls = 0;
  await assert.rejects(
    withRetry(async () => { calls++; throw new Error("once"); }, { retries: 0, sleep: async () => {} }),
    /once/,
  );
  assert.strictEqual(calls, 1);
});
