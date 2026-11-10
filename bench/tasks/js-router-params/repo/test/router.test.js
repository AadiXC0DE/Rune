const test = require("node:test");
const assert = require("node:assert");
const { Router } = require("../src/router.js");

test("literal and parameter routes match", () => {
  const router = new Router();
  router.add("/users", "list").add("/users/:id", "show");
  assert.deepStrictEqual(router.resolve("/users"), { status: 200, handler: "list", params: {} });
  assert.deepStrictEqual(router.resolve("/users/42"), { status: 200, handler: "show", params: { id: "42" } });
});

test("an unknown path is a 404", () => {
  const router = new Router().add("/users", "list");
  assert.deepStrictEqual(router.resolve("/nothing"), { status: 404 });
  assert.deepStrictEqual(router.resolve("/users/1/extra"), { status: 404 });
});

test("a trailing slash still finds the route", () => {
  const router = new Router().add("/users/:id", "show");
  assert.strictEqual(router.resolve("/users/7/").status, 200);
  assert.deepStrictEqual(router.resolve("/users/7/").params, { id: "7" });
});

test("a more specific route beats a wildcard added earlier", () => {
  const router = new Router();
  router.add("/files/*", "any").add("/files/:id", "one").add("/files/new", "create");
  assert.strictEqual(router.resolve("/files/new").handler, "create");
  assert.strictEqual(router.resolve("/files/abc").handler, "one");
  const deep = router.resolve("/files/a/b/c");
  assert.strictEqual(deep.handler, "any");
  assert.deepStrictEqual(deep.params, { "*": "a/b/c" });
});

test("encoded parameters are decoded", () => {
  const router = new Router().add("/search/:term", "search");
  assert.deepStrictEqual(router.resolve("/search/hello%20world").params, { term: "hello world" });
  assert.deepStrictEqual(router.resolve("/search/a%2Fb").params, { term: "a/b" });
});
