const test = require("node:test");
const assert = require("node:assert");
const { escapeField } = require("../src/escape.js");
const { toCsv } = require("../src/writer.js");

const schema = {
  columns: [
    { key: "id", header: "ID" },
    { key: "name", header: "Name" },
    { key: "note", header: "Note" },
  ],
};

test("plain values are written as they are", () => {
  assert.strictEqual(escapeField("abc"), "abc");
  assert.strictEqual(escapeField(42), "42");
  assert.strictEqual(escapeField(true), "true");
});

test("fields with a comma or a quote are quoted", () => {
  assert.strictEqual(escapeField("a,b"), '"a,b"');
  assert.strictEqual(escapeField('say "hi"'), '"say ""hi"""');
});

test("a field with a line feed is quoted", () => {
  assert.strictEqual(escapeField("one\ntwo"), '"one\ntwo"');
});

test("rows follow the schema column order", () => {
  const rows = [{ note: "n1", name: "Ann", id: 1 }];
  assert.strictEqual(toCsv(rows, schema), "ID,Name,Note\r\n1,Ann,n1\r\n");
});

test("columns outside the schema are left out", () => {
  const rows = [{ id: 2, name: "Bo", note: "x", secret: "hidden" }];
  assert.strictEqual(toCsv(rows, schema), "ID,Name,Note\r\n2,Bo,x\r\n");
});

test("an export with no rows is just the header line", () => {
  assert.strictEqual(toCsv([], schema), "ID,Name,Note\r\n");
});
