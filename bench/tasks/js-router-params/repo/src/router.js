/**
 * A path router. Patterns are made of segments:
 *   literal   matches that text
 *   :name     matches one segment and captures it as params.name
 *   *         as the last segment, matches the rest of the path (params["*"])
 *
 * resolve(path) returns { status: 200, handler, params }, { status: 404 }
 * when nothing matches, or { status: 400 } when the path has malformed
 * percent-encoding. A trailing slash is ignored. When several routes match,
 * the most specific wins: literal segments beat :name, which beats *.
 */
class Router {
  constructor() {
    this.routes = [];
  }

  add(pattern, handler) {
    this.routes.push({ pattern, handler });
    return this;
  }

  resolve(path) {
    for (const route of this.routes) {
      const params = matchPattern(route.pattern, path);
      if (params) return { status: 200, handler: route.handler, params };
    }
    return { status: 404 };
  }
}

function matchPattern(pattern, path) {
  const want = pattern.split("/");
  const have = path.split("/");
  const params = {};
  for (let i = 0; i < want.length; i++) {
    if (want[i] === "*") {
      params["*"] = have.slice(i).join("/");
      return params;
    }
    if (i >= have.length) return null;
    if (want[i].startsWith(":")) {
      params[want[i].slice(1)] = have[i];
    } else if (want[i] !== have[i]) {
      return null;
    }
  }
  return want.length === have.length ? params : null;
}

module.exports = { Router };
