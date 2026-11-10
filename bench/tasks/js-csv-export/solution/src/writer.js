const { escapeField } = require("./escape.js");

/**
 * Renders rows as CSV text. `schema.columns` lists `{ key, header }` entries
 * in output order; the first line holds the headers. Lines end with CRLF,
 * including the last one.
 */
function toCsv(rows, schema) {
  const keys = schema.columns.map((column) => column.key);
  const header = schema.columns.map((column) => escapeField(column.header));
  const lines = [header.join(",")];
  for (const row of rows) {
    lines.push(keys.map((key) => escapeField(row[key])).join(","));
  }
  return lines.join("\r\n") + "\r\n";
}

module.exports = { toCsv };
