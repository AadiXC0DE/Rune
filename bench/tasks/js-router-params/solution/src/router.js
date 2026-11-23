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
    let segments;
    try {
      segments = splitPath(path).map(decodeURIComponent);
    } catch (error) {
      if (error instanceof URIError) return { status: 400 };
      throw error;
    }
    let best = null;
    for (const route of this.routes) {
      const match = matchPattern(splitPath(route.pattern), segments);
      if (match && (best === null || isBetter(match.score, best.score))) {
        best = { handler: route.handler, params: match.params, score: match.score };
      }
    }
    if (best === null) return { status: 404 };
    return { status: 200, handler: best.handler, params: best.params };
  }
}

function splitPath(path) {
  return path.split("/").filter((segment) => segment !== "");
}

function matchPattern(want, have) {
  const params = {};
  const score = [];
  for (let i = 0; i < want.length; i++) {
    if (want[i] === "*") {
      params["*"] = have.slice(i).join("/");
      score.push(1);
      return { params, score };
    }
    if (i >= have.length) return null;
    if (want[i].startsWith(":")) {
      params[want[i].slice(1)] = have[i];
      score.push(2);
    } else if (want[i] === have[i]) {
      score.push(3);
    } else {
      return null;
    }
  }
  return want.length === have.length ? { params, score } : null;
}

function isBetter(candidate, current) {
  const length = Math.max(candidate.length, current.length);
  for (let i = 0; i < length; i++) {
    const a = candidate[i] || 0;
    const b = current[i] || 0;
    if (a !== b) return a > b;
  }
  return false;
}

module.exports = { Router };
