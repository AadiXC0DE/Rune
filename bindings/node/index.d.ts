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
 * - `OUTPUT`: invalid result output or a malformed JSONL stream.
 * - `OUTPUT_LIMIT`: combined output exceeded `maxOutputBytes` or a record exceeded `maxEventBytes`.
 * - `TIMEOUT`: the process was killed after `timeoutMs`.
 * - `ABORTED`: the caller aborted through `signal`.
 * - `SIGNALED`: the process was terminated by another signal.
 */
export type RuneErrorCode =
  | "BINARY_NOT_FOUND"
  | "EXIT"
  | "OUTPUT"
  | "OUTPUT_LIMIT"
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
  /** Path to the executable, relative to `cwd`, or a name to find on `PATH`. */
  bin?: string;
  /** Provider override for this run; non-empty string passed before the command name. */
  provider?: string;
  /** Pass `--offline` before the command name when true; defaults to false. */
  offline?: boolean;
  /** Model override for this run, passed before the command name. */
  model?: string;
  /** Reasoning effort override for this run, passed before the command name. */
  effort?: RuneEffort;
  /** Working directory for the process and relative `bin` paths; defaults to the current directory. */
  cwd?: string;
  /** Extra environment variables merged over the current environment. */
  env?: Record<string, string>;
  /** Pass `--no-save` so the run creates no session. */
  noSave?: boolean;
  /** Kill the process after this many milliseconds; finite, positive, and at most 2147483647. */
  timeoutMs?: number;
  /** Combined stdout/stderr capture cap in bytes; positive safe integer, defaults to 8388608 (8 MiB). */
  maxOutputBytes?: number;
  /** Kill the process when this signal aborts. */
  signal?: AbortSignal;
}

/** Options for the pull-based JSONL event iterator. */
export interface AskStreamOptions extends AskOptions {
  /** Per-record byte cap including newline; positive integer at most 1048576, the default. */
  maxEventBytes?: number;
}

/** Wire events emitted by `rune ask --jsonl`. */
export type AskEvent =
  | { type: "start" }
  | { type: "step_start" | "step_restart"; step: number }
  | { type: "text_delta"; step: number; delta: string }
  | {
      type: "plan_updated";
      update: {
        version: number;
        explanation: string | null;
        plan: { step: string; status: "pending" | "in_progress" | "completed" }[];
      };
    }
  | { type: "final"; result: AskResult };

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
  /** The validated result object, or null when standard output held none. */
  result: AskResult | null;
  /** Everything the process wrote to standard error. */
  stderr: string;
}

/** Invalid result output or a malformed JSONL stream. */
export class RuneOutputError extends RuneError {
  constructor(stdout: string, stderr: string, detail?: string | null, format?: "result" | "jsonl");
  /** Captured standard output, or the offending JSONL record for streaming errors. */
  stdout: string;
  /** Everything the process wrote to standard error. */
  stderr: string;
}

/** Combined output or one streaming record exceeded its byte budget; the process is killed. */
export class RuneOutputLimitError extends RuneError {
  constructor(maxOutputBytes: number, stream: "stdout" | "stderr", scope?: "combined" | "record");
  /** Configured byte cap for the indicated scope. */
  maxOutputBytes: number;
  /** The stream whose next chunk exceeded the cap. */
  stream: "stdout" | "stderr";
  /** Whether the cap applies to total output or one JSONL record. */
  scope: "combined" | "record";
}

/**
 * Resolves the executable to run: the `bin` option, then `RUNE_BIN`, then an
 * executable named `rune` on `PATH` (also `rune.exe` on Windows).
 * Explicit relative `bin` paths resolve against `cwd`, or the current directory
 * when `cwd` is omitted.
 *
 * @throws {TypeError} When `bin` is not a non-empty string.
 * @throws {RuneBinaryNotFoundError} When no location holds an executable.
 */
export function resolveBinary(options?: { bin?: string; cwd?: string }): string;

/**
 * Runs one request and returns the parsed result object.
 *
 * @throws {TypeError} When an argument is outside its accepted range.
 * @throws {RuneBinaryNotFoundError} When the executable cannot be found or run.
 * @throws {RuneExitError} When the process exits non-zero.
 * @throws {RuneOutputError} When the process exits cleanly without a valid object;
 * missing or invalid fields are named in the message.
 * @throws {RuneOutputLimitError} When combined stdout and stderr exceed `maxOutputBytes`.
 * @throws {RuneError} With code `TIMEOUT`, `ABORTED`, or `SIGNALED`.
 */
export function ask(prompt: string, options?: AskOptions): Promise<AskResult>;

/**
 * Starts the process on first next() and yields events as the caller pulls them.
 * Successful completion yields a validated final result after process exit.
 * Failures throw the same errors as ask(); a failed final is in RuneExitError.result.
 * A malformed or incomplete JSONL stream throws RuneOutputError.
 * Total stdout/stderr bytes are capped by maxOutputBytes; each record is capped
 * by maxEventBytes. Early return or a for-await loop break kills the child.
 */
export function askStream(prompt: string, options?: AskStreamOptions): AsyncGenerator<AskEvent, void, unknown>;
