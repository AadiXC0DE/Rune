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
    this.tokens += ((now - this.last) / 1000) * this.refillPerSecond;
    this.last = now;
  }

  /** Takes one token and reports whether there was one. */
  take() {
    this.refill();
    if (this.tokens >= 1) {
      this.tokens -= 1;
      return true;
    }
    return false;
  }

  /** Tokens available right now, which may be fractional. */
  available() {
    this.refill();
    return this.tokens;
  }
}

module.exports = { TokenBucket };
