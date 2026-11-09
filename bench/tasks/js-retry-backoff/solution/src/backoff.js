/**
 * Delay before retry number `retry` (1 for the first retry): `base * 2 **
 * (retry - 1)`, never more than `cap`.
 */
function delayFor(retry, { base = 100, cap = 2000 } = {}) {
  return Math.min(base * 2 ** (retry - 1), cap);
}

module.exports = { delayFor };
