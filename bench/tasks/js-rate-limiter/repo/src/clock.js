/** A clock reports the current time in milliseconds through `now()`. */
const systemClock = {
  now() {
    return Date.now() / 1000;
  },
};

/** A clock that only moves when `advance(ms)` is called, for tests. */
function manualClock(start = 0) {
  let current = start;
  return {
    now() {
      return current;
    },
    advance(ms) {
      current += ms;
    },
  };
}

module.exports = { systemClock, manualClock };
