const { delayFor } = require("./backoff.js");

/**
 * Calls `operation` until it succeeds, retrying up to `retries` times after
 * the first attempt. `sleep` receives each delay. Rethrows the last error.
 */
async function withRetry(operation, { retries = 3, base, cap, sleep } = {}) {
  let lastError;
  for (let attempt = 1; attempt <= retries + 1; attempt++) {
    try {
      return await operation(attempt);
    } catch (error) {
      lastError = error;
      if (attempt <= retries) {
        await sleep(delayFor(attempt, { base, cap }));
      }
    }
  }
  throw lastError;
}

module.exports = { withRetry };
