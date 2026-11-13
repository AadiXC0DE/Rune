export type Config = { [key: string]: unknown };

function isPlainObject(value: unknown): value is Config {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function copy(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(copy);
  if (isPlainObject(value)) return merge({}, value);
  return value;
}

/**
 * Merges `override` onto `base` and returns a new object; neither input is
 * changed. Nested objects merge key by key, arrays are replaced as a whole,
 * and a key set to null in `override` is removed from the result.
 */
export function merge(base: Config, override: Config): Config {
  const result: Config = {};
  for (const key of Object.keys(base)) {
    result[key] = copy(base[key]);
  }
  for (const key of Object.keys(override)) {
    const value = override[key];
    if (value === null) {
      delete result[key];
    } else if (isPlainObject(value)) {
      const existing = result[key];
      result[key] = merge(isPlainObject(existing) ? existing : {}, value);
    } else if (value !== undefined) {
      result[key] = copy(value);
    }
  }
  return result;
}
