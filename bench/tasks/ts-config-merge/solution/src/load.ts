import { merge } from "./merge.ts";
import type { Config } from "./merge.ts";

export interface Layers {
  defaults: Config;
  user?: Config;
  project?: Config;
}

/**
 * Builds the effective settings. Later layers win: the project file
 * overrides the user file, which overrides the defaults.
 */
export function loadConfig(layers: Layers): Config {
  return merge(merge(layers.defaults, layers.user ?? {}), layers.project ?? {});
}
