/**
 * Typings for the Node binding that drives the `rune` binary as a subprocess.
 *
 * `AbortSignal` and the other Node globals require `@types/node`.
 */

/** Reasoning effort names accepted by `rune`. */
export type RuneEffort =
  | "auto"
  | "none"
  | "minimal"
  | "low"
  | "medium"
  | "high"
  | "xhigh"
  | "max";

/**
 * Failure codes carried by `RuneError.code`.
 *
 * - `BINARY_NOT_FOUND`: no executable through the option, `RUNE_BIN`, or `PATH`.
 * - `EXIT`: the binary exited non-zero.
 * - `OUTPUT`: the binary exited cleanly without printing the result object.
 * - `TIMEOUT`: the process was killed after `timeoutMs`.
 * - `ABORTED`: the caller aborted through `signal`.
 * - `SIGNALED`: the process was terminated by another signal.
 */
export type RuneErrorCode =
  | "BINARY_NOT_FOUND"
  | "EXIT"
  | "OUTPUT"
  | "TIMEOUT"
  | "ABORTED"
  | "SIGNALED";

/** One executable location considered during lookup. */
export interface BinaryCandidate {
  /** `bin` for the option, `RUNE_BIN` for the environment variable, `PATH` for the search. */
  source: "bin" | "RUNE_BIN" | "PATH";
  /** The value considered, or null when that source was not set. */
  value: string | null;
}

/** Token counts. A count the provider did not report is absent. */
export interface AskUsage {
  input_tokens?: number;
  output_tokens?: number;
}

/** One tool call made during the run. */
export interface AskToolCall {
  name: string;
  status: "success" | "error";
}

/**
 * The object `rune ask --json` prints.
 *
 * Field names match the wire format; `error` and `error_code` are present only
 * on a failed run.
 */
export interface AskResult {
  /** Assistant text produced during the request. */
  output: string;
  /** The completed final response, or an empty string. */
  final_output: string;
  /** Process exit code the run used. */
  exit_code: number;
  /** Model identifier that served the request. */
  model: string;
  /** Upstream provider that served it, when the endpoint reported one. */
  resolved_provider: string | null;
  /** Session identifier, empty when the run was not saved. */
  session_id: string;
  /** Model steps taken. */
  steps: number;
  /** Token counts. */
  usage: AskUsage;
  /** Tool calls made, in order. */
  tool_calls: AskToolCall[];
  /** Failure detail, present only when the run failed. */
  error?: string;
  /** Stable failure code from the binary, present only when the run failed. */
  error_code?: string;
}

/** Options for `ask`. */
export interface AskOptions {
  /** Path to the executable, or a name to find on `PATH`. */
  bin?: string;
  /** Model override for this run, passed before the command name. */
  model?: string;
  /** Reasoning effort override for this run, passed before the command name. */
  effort?: RuneEffort;
  /** Working directory for the process. */
  cwd?: string;
  /** Extra environment variables merged over the current environment. */
  env?: Record<string, string>;
  /** Pass `--no-save` so the run creates no session. */
  noSave?: boolean;
  /** Kill the process after this many milliseconds. */
  timeoutMs?: number;
  /** Kill the process when this signal aborts. */
  signal?: AbortSignal;
}

/** Base class for every failure the binding reports. */
export class RuneError extends Error {
  constructor(code: RuneErrorCode, message: string, options?: { cause?: unknown });
  /** Machine-readable failure code. */
  code: RuneErrorCode;
}

/** No executable was found through the option, `RUNE_BIN`, or `PATH`. */
export class RuneBinaryNotFoundError extends RuneError {
  constructor(candidates: BinaryCandidate[], detail: string | null);
  /** Every location considered, in lookup order. */
  candidates: BinaryCandidate[];
}

/** The binary exited non-zero. */
export class RuneExitError extends RuneError {
  constructor(exitCode: number, result: AskResult | null, stderr: string);
  /** Exit code reported by the process. */
  exitCode: number;
  /** The parsed result object, or null when standard output held none. */
  result: AskResult | null;
  /** Everything the process wrote to standard error. */
  stderr: string;
}

/** The binary exited cleanly without printing the result object. */
export class RuneOutputError extends RuneError {
  constructor(stdout: string, stderr: string);
  /** Everything the process wrote to standard output. */
  stdout: string;
  /** Everything the process wrote to standard error. */
  stderr: string;
}

/**
 * Resolves the executable to run: the `bin` option, then `RUNE_BIN`, then an
 * executable named `rune` on `PATH`.
 *
 * @throws {TypeError} When `bin` is not a non-empty string.
 * @throws {RuneBinaryNotFoundError} When no location holds an executable.
 */
export function resolveBinary(options?: { bin?: string }): string;

/**
 * Runs one request and returns the parsed result object.
 *
 * @throws {TypeError} When an argument is outside its accepted range.
 * @throws {RuneBinaryNotFoundError} When the executable cannot be found or run.
 * @throws {RuneExitError} When the process exits non-zero.
 * @throws {RuneOutputError} When the process exits cleanly without the object.
 * @throws {RuneError} With code `TIMEOUT`, `ABORTED`, or `SIGNALED`.
 */
export function ask(prompt: string, options?: AskOptions): Promise<AskResult>;
