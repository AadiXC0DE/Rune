const test = require("node:test");
const assert = require("node:assert");
const { Router } = require("../src/router.js");

test("/users/ and /users are the same route, and the root still works", () => {
  const router = new Router().add("/users", "list").add("/", "home");
  assert.strictEqual(router.resolve("/users/").handler, "list");
  assert.strictEqual(router.resolve("/users").handler, "list");
  assert.strictEqual(router.resolve("/").handler, "home");
});

test("a wildcard route loses to a parameter route whatever the order", () => {
  const a = new Router().add("/files/*", "any").add("/files/:id", "one");
  const b = new Router().add("/files/:id", "one").add("/files/*", "any");
  for (const router of [a, b]) {
    assert.strictEqual(router.resolve("/files/x").handler, "one");
    assert.strictEqual(router.resolve("/files/x/y").handler, "any");
  }
});

test("specificity is decided segment by segment from the left", () => {
  const router = new Router();
  router.add("/a/:x/c", "param-first").add("/a/b/:y", "literal-first");
  assert.strictEqual(router.resolve("/a/b/c").handler, "literal-first");
  assert.strictEqual(router.resolve("/a/z/c").handler, "param-first");
});

test("malformed percent-encoding is a 400 and never throws", () => {
  const router = new Router().add("/users/:id", "show").add("/files/*", "any");
  assert.deepStrictEqual(router.resolve("/users/%E0%A4%A"), { status: 400 });
  assert.deepStrictEqual(router.resolve("/users/100%"), { status: 400 });
  assert.deepStrictEqual(router.resolve("/files/ok/%zz"), { status: 400 });
});

test("multi-byte sequences decode and an encoded slash stays in one parameter", () => {
  const router = new Router().add("/tags/:name", "tag");
  assert.strictEqual(router.resolve("/tags/%E2%9C%93").params.name, "✓");
  assert.strictEqual(router.resolve("/tags/a%2Fb").params.name, "a/b");
});

test("a wildcard may match an empty rest", () => {
  const router = new Router().add("/static/*", "files");
  const result = router.resolve("/static/");
  assert.strictEqual(result.status, 200);
  assert.deepStrictEqual(result.params, { "*": "" });
});
