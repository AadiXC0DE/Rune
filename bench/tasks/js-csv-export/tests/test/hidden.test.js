const test = require("node:test");
const assert = require("node:assert");
const { escapeField } = require("../src/escape.js");
const { toCsv } = require("../src/writer.js");

const schema = {
  columns: [
    { key: "id", header: "ID" },
    { key: "name", header: "Display name" },
    { key: "note", header: "Note, long" },
  ],
};

test("an embedded CRLF keeps the field in one quoted cell", () => {
  assert.strictEqual(escapeField("a\r\nb"), '"a\r\nb"');
  assert.strictEqual(escapeField("a\rb"), '"a\rb"');
});

test("null is an empty cell and an empty string is a quoted empty string", () => {
  assert.strictEqual(escapeField(null), "");
  assert.strictEqual(escapeField(undefined), "");
  assert.strictEqual(escapeField(""), '""');
  const rows = [{ id: 1, name: "", note: null }];
  assert.strictEqual(toCsv(rows, schema), 'ID,Display name,"Note, long"\r\n1,"",\r\n');
});

test("a row missing a schema column gets an empty cell there", () => {
  const rows = [{ id: 7, note: "kept" }, { name: "only name" }];
  assert.strictEqual(
    toCsv(rows, schema),
    'ID,Display name,"Note, long"\r\n7,,kept\r\n,only name,\r\n',
  );
});

test("zero and false are values, not empty cells", () => {
  assert.strictEqual(escapeField(0), "0");
  assert.strictEqual(escapeField(false), "false");
});

test("a quote at the start and end of a field is doubled", () => {
  assert.strictEqual(escapeField('"x"'), '"""x"""');
});

test("the first row does not decide the columns", () => {
  const rows = [{ id: 1 }, { id: 2, name: "B", note: "c", extra: 1 }];
  assert.strictEqual(
    toCsv(rows, schema),
    'ID,Display name,"Note, long"\r\n1,,\r\n2,B,c\r\n',
  );
});
