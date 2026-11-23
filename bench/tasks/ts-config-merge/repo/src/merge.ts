export type Config = { [key: string]: unknown };

/**
 * Merges `override` onto `base` and returns a new object; neither input is
 * changed. Nested objects merge key by key, arrays are replaced as a whole,
 * and a key set to null in `override` is removed from the result.
 */
export function merge(base: Config, override: Config): Config {
  const result: Config = { ...base };
  for (const key of Object.keys(override)) {
    const value = override[key];
    const existing = result[key];
    if (Array.isArray(existing) && Array.isArray(value)) {
      result[key] = [...existing, ...value];
    } else {
      result[key] = value;
    }
  }
  return result;
}
