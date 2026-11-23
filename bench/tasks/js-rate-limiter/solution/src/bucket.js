const { systemClock } = require("./clock.js");

/**
 * A token bucket that starts full. Tokens come back at `refillPerSecond`,
 * continuously, and the bucket never holds more than `capacity`.
 */
class TokenBucket {
  constructor({ capacity, refillPerSecond, clock = systemClock }) {
    this.capacity = capacity;
    this.refillPerSecond = refillPerSecond;
    this.clock = clock;
    this.tokens = capacity;
    this.last = clock.now();
  }

  refill() {
    const now = this.clock.now();
    const gained = ((now - this.last) / 1000) * this.refillPerSecond;
    this.tokens = Math.min(this.capacity, this.tokens + gained);
    this.last = now;
  }

  /** Takes `n` tokens at once, or none at all when fewer are available. */
  tryRemove(n) {
    if (!Number.isFinite(n) || n < 0) {
      throw new RangeError("n must be a non-negative number");
    }
    this.refill();
    if (this.tokens >= n) {
      this.tokens -= n;
      return true;
    }
    return false;
  }

  /** Takes one token and reports whether there was one. */
  take() {
    return this.tryRemove(1);
  }

  /** Tokens available right now, which may be fractional. */
  available() {
    this.refill();
    return this.tokens;
  }
}

module.exports = { TokenBucket };
