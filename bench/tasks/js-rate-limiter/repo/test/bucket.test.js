const test = require("node:test");
const assert = require("node:assert");
const { TokenBucket } = require("../src/bucket.js");
const { manualClock, systemClock } = require("../src/clock.js");

test("the system clock reports milliseconds", () => {
  const before = Date.now();
  const reading = systemClock.now();
  const after = Date.now();
  assert.ok(reading >= before && reading <= after, `reading ${reading}`);
});

test("a new bucket is full and then runs dry", () => {
  const clock = manualClock();
  const bucket = new TokenBucket({ capacity: 3, refillPerSecond: 1, clock });
  assert.deepStrictEqual([bucket.take(), bucket.take(), bucket.take(), bucket.take()], [true, true, true, false]);
});

test("tokens come back at the refill rate", () => {
  const clock = manualClock(5000);
  const bucket = new TokenBucket({ capacity: 10, refillPerSecond: 2, clock });
  for (let i = 0; i < 10; i++) bucket.take();
  assert.strictEqual(bucket.take(), false);
  clock.advance(1000);
  assert.strictEqual(bucket.take(), true);
  assert.strictEqual(bucket.take(), true);
  assert.strictEqual(bucket.take(), false);
});

test("a pause never lets more than the capacity through", () => {
  const clock = manualClock();
  const bucket = new TokenBucket({ capacity: 5, refillPerSecond: 1, clock });
  for (let i = 0; i < 5; i++) bucket.take();
  clock.advance(60000);
  let granted = 0;
  while (bucket.take()) granted++;
  assert.strictEqual(granted, 5);
});

test("tryRemove takes several tokens or none", () => {
  const clock = manualClock();
  const bucket = new TokenBucket({ capacity: 5, refillPerSecond: 1, clock });
  assert.strictEqual(bucket.tryRemove(3), true);
  assert.strictEqual(bucket.tryRemove(3), false);
  assert.strictEqual(bucket.available(), 2);
  assert.strictEqual(bucket.tryRemove(2), true);
});
