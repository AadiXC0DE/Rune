const test = require("node:test");
const assert = require("node:assert");
const { TokenBucket } = require("../src/bucket.js");
const { manualClock } = require("../src/clock.js");

test("tryRemove(0) always succeeds and takes nothing", () => {
  const clock = manualClock();
  const bucket = new TokenBucket({ capacity: 2, refillPerSecond: 1, clock });
  bucket.take();
  bucket.take();
  assert.strictEqual(bucket.tryRemove(0), true);
  assert.strictEqual(bucket.available(), 0);
});

test("a failed tryRemove leaves the tokens alone", () => {
  const clock = manualClock();
  const bucket = new TokenBucket({ capacity: 4, refillPerSecond: 1, clock });
  assert.strictEqual(bucket.tryRemove(5), false);
  assert.strictEqual(bucket.available(), 4);
});

test("refill stops at capacity after a long pause", () => {
  const clock = manualClock(1000);
  const bucket = new TokenBucket({ capacity: 4, refillPerSecond: 10, clock });
  assert.strictEqual(bucket.tryRemove(4), true);
  clock.advance(3600 * 1000);
  assert.strictEqual(bucket.available(), 4);
  assert.strictEqual(bucket.tryRemove(5), false);
  assert.strictEqual(bucket.tryRemove(4), true);
  assert.strictEqual(bucket.tryRemove(1), false);
});

test("fractional refill accumulates across calls", () => {
  const clock = manualClock();
  const bucket = new TokenBucket({ capacity: 2, refillPerSecond: 1, clock });
  bucket.tryRemove(2);
  for (let i = 0; i < 4; i++) {
    clock.advance(250);
    assert.strictEqual(bucket.tryRemove(1), i === 3);
  }
  assert.strictEqual(bucket.available(), 0);
});

test("a slow refill rate is honoured in milliseconds", () => {
  const clock = manualClock();
  const bucket = new TokenBucket({ capacity: 1, refillPerSecond: 0.5, clock });
  assert.strictEqual(bucket.take(), true);
  clock.advance(1999);
  assert.strictEqual(bucket.take(), false);
  clock.advance(1);
  assert.strictEqual(bucket.take(), true);
});
