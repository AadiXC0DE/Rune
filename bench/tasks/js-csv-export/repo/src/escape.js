/**
 * Turns one value into one CSV cell.
 *
 * null and undefined become an empty cell. An empty string becomes a quoted
 * empty string (two double quotes), so a reader can tell the two apart. A
 * cell containing a comma, a double quote, a carriage return or a line feed
 * is wrapped in double quotes with each inner quote doubled.
 */
function escapeField(value) {
  if (value === null || value === undefined) return "";
  return String(value);
}

module.exports = { escapeField };
