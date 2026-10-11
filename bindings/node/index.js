// Drives the `rune` binary as a subprocess.
//
// The binary owns configuration, credentials, and the network path; this module
// only builds an argument list, runs it, and returns the result object that
// `rune ask --json` prints. No provider, credential, or session state is read
// or written here.

import { spawn } from "node:child_process";
import { accessSync, constants, statSync } from "node:fs";
import { delimiter, extname, isAbsolute, join, resolve } from "node:path";

/**
 * Reasoning effort names the command line accepts.
 *
 * An unrecognized value is silently treated as `auto` by the binary, so it is
 * rejected here instead of changing behavior without a report.
 */
const EFFORTS = ["auto", "none", "minimal", "low", "medium", "high", "xhigh", "max"];

// Node clamps larger delays to 1 ms and emits TimeoutOverflowWarning.
const MAX_TIMEOUT_MS = 2 ** 31 - 1;
const DEFAULT_MAX_OUTPUT_BYTES = 8 * 1024 * 1024;

/** Base class for every failure this module reports. */
export class RuneError extends Error {
  constructor(code, message, options) {
    super(message, options);
    this.name = "RuneError";
    this.code = code;
  }
}

/** No executable was found through the option, `RUNE_BIN`, or `PATH`. */
export class RuneBinaryNotFoundError extends RuneError {
  constructor(candidates, detail) {
    super("BINARY_NOT_FOUND", notFoundMessage(candidates, detail));
    this.name = "RuneBinaryNotFoundError";
    this.candidates = candidates;
  }
}

/** The binary exited non-zero. */
export class RuneExitError extends RuneError {
  constructor(exitCode, result, stderr) {
    super("EXIT", exitMessage(exitCode, result, stderr));
    this.name = "RuneExitError";
    this.exitCode = exitCode;
    this.result = result;
    this.stderr = stderr;
  }
}

/** The binary exited cleanly without printing a valid result object. */
export class RuneOutputError extends RuneError {
  constructor(stdout, stderr, detail = null) {
    super(
      "OUTPUT",
      `rune ask exited cleanly without printing a valid result object${detail === null ? "" : `; ${detail}`}; standard output was ${describe(stdout)}`,
    );
    this.name = "RuneOutputError";
    this.stdout = stdout;
    this.stderr = stderr;
  }
}

/** The child's combined stdout and stderr exceeded the capture budget. */
export class RuneOutputLimitError extends RuneError {
  constructor(maxOutputBytes, stream) {
    super(
      "OUTPUT_LIMIT",
      `rune ask exceeded the combined output limit of ${maxOutputBytes} bytes while reading ${stream}`,
    );
    this.name = "RuneOutputLimitError";
    this.maxOutputBytes = maxOutputBytes;
    this.stream = stream;
  }
}

/**
 * Resolves the executable to run.
 *
 * The `bin` option wins, then `RUNE_BIN`, then an executable named `rune` on
 * `PATH` (also `rune.exe` on Windows). A source that is configured but unusable
 * is an error rather than a silent fallback, so a typo is reported instead of
 * running another binary.
 * Explicit relative `bin` paths are resolved against `cwd` when provided.
 *
 * @param {{ bin?: string, cwd?: string }} [options]
 * @returns {string} Path to the executable.
 */
export function resolveBinary(options = {}) {
  if (typeof options !== "object" || options === null) {
    throw new TypeError("the options must be an object");
  }

  const bin = options.bin;
  if (bin !== undefined && (typeof bin !== "string" || bin.length === 0)) {
    throw new TypeError("the bin option must be a non-empty string");
  }

  const candidates = binaryCandidates(bin);
  if (bin !== undefined) {
    // Check and spawn the same file even when the child changes directories.
    const candidate =
      options.cwd !== undefined && !isAbsolute(bin) && (bin.includes("/") || bin.includes("\\"))
        ? resolve(options.cwd, bin)
        : bin;
    const found = findExecutable(candidate);
    if (found === null) {
      throw new RuneBinaryNotFoundError(candidates, `${bin} is not an executable file`);
    }
    return found;
  }

  const fromEnv = candidates.find((candidate) => candidate.source === "RUNE_BIN").value;
  if (fromEnv !== null) {
    const found = findExecutable(fromEnv);
    if (found === null) {
      throw new RuneBinaryNotFoundError(
        candidates,
        `RUNE_BIN is set to ${fromEnv}, which is not an executable file`,
      );
    }
    return found;
  }

  const found = searchPath("rune");
  if (found === null) {
    throw new RuneBinaryNotFoundError(candidates, null);
  }
  return found;
}

/**
 * Runs one request and returns the parsed result object.
 *
 * @param {string} prompt Non-empty prompt text.
 * @param {object} [options]
 * @returns {Promise<object>} The parsed `rune ask --json` object.
 */
export async function ask(prompt, options = {}) {
  if (typeof prompt !== "string" || prompt.trim().length === 0) {
    throw new TypeError("the prompt must be a non-empty string");
  }
  if (typeof options !== "object" || options === null) {
    throw new TypeError("the options must be an object");
  }

  const {
    provider,
    offline = false,
    model,
    effort,
    cwd,
    env,
    noSave = false,
    timeoutMs,
    signal,
    maxOutputBytes = DEFAULT_MAX_OUTPUT_BYTES,
  } = options;
  if (provider !== undefined && (typeof provider !== "string" || provider.length === 0)) {
    throw new TypeError("the provider option must be a non-empty string");
  }
  if (typeof offline !== "boolean") {
    throw new TypeError("the offline option must be a boolean");
  }
  if (model !== undefined && (typeof model !== "string" || model.length === 0)) {
    throw new TypeError("the model option must be a non-empty string");
  }
  if (effort !== undefined && !EFFORTS.includes(effort)) {
    throw new TypeError(`the effort option must be one of ${EFFORTS.join(", ")}`);
  }
  if (
    timeoutMs !== undefined &&
    !(Number.isFinite(timeoutMs) && timeoutMs > 0 && timeoutMs <= MAX_TIMEOUT_MS)
  ) {
    throw new TypeError(
      `the timeoutMs option must be a finite positive number no greater than ${MAX_TIMEOUT_MS}`,
    );
  }
  if (!(Number.isSafeInteger(maxOutputBytes) && maxOutputBytes > 0)) {
    throw new TypeError("the maxOutputBytes option must be a positive safe integer");
  }

  const executable = resolveBinary(options);
  const args = [];
  // Options that apply to the whole process are only read before the command
  // name; everything after it belongs to the command.
  if (provider !== undefined) args.push("--provider", provider);
  if (offline) args.push("--offline");
  if (model !== undefined) args.push("--model", model);
  if (effort !== undefined) args.push("--effort", effort);
  args.push("ask", "--json");
  if (noSave) args.push("--no-save");
  // A bare `--` ends flag parsing, so a prompt starting with a dash is read as
  // the prompt.
  args.push("--", prompt);

  const child = spawn(executable, args, {
    cwd,
    env: env === undefined ? process.env : { ...process.env, ...env },
    stdio: ["ignore", "pipe", "pipe"],
  });

  const stdout = [];
  const stderr = [];
  let capturedBytes = 0;
  let outputLimitStream = null;

  let interruption = null;
  let grace = null;
  const interrupt = (reason) => {
    if (interruption !== null) return;
    interruption = reason;
    child.kill();
    // A process that ignores SIGTERM would otherwise leave the promise pending
    // forever.
    grace = setTimeout(() => child.kill("SIGKILL"), 2000);
    grace.unref();
  };
  const capture = (chunks, chunk, stream) => {
    // Keep draining after interruption so an uncooperative child cannot block
    // on a full pipe during the kill grace, but retain no more output.
    if (interruption !== null) return;
    if (chunk.length > maxOutputBytes - capturedBytes) {
      outputLimitStream = stream;
      stdout.length = 0;
      stderr.length = 0;
      interrupt("OUTPUT_LIMIT");
      // Output may arrive after the parent exited while an inherited pipe
      // remains open. In that case there will be no later exit event to await.
      if (child.exitCode !== null || child.signalCode !== null) {
        child.stdout.destroy();
        child.stderr.destroy();
      }
      return;
    }
    capturedBytes += chunk.length;
    chunks.push(chunk);
  };
  child.stdout.on("data", (chunk) => capture(stdout, chunk, "stdout"));
  child.stderr.on("data", (chunk) => capture(stderr, chunk, "stderr"));
  const timer = timeoutMs === undefined ? null : setTimeout(() => interrupt("TIMEOUT"), timeoutMs);
  const onAbort = () => interrupt("ABORTED");
  if (signal !== undefined) {
    if (signal.aborted) {
      interrupt("ABORTED");
    } else {
      signal.addEventListener("abort", onAbort, { once: true });
    }
  }

  let code = null;
  let term = null;
  try {
    ({ code, signal: term } = await new Promise((resolve, reject) => {
      child.once("error", reject);
      child.once("close", (status, reason) => resolve({ code: status, signal: reason }));
      // `close` waits for the pipes to end, and a grandchild that inherited
      // them outlives the killed process, so a kill settles on process exit
      // instead and does not hold the caller for as long as that grandchild
      // lives.
      child.once("exit", (status, reason) => {
        if (interruption !== null) {
          child.stdout.destroy();
          child.stderr.destroy();
          resolve({ code: status, signal: reason });
        }
      });
    }));
  } catch (error) {
    // A path that resolved but cannot be executed, such as a script whose
    // interpreter is missing.
    if (error?.code === "ENOENT") {
      throw new RuneBinaryNotFoundError(
        binaryCandidates(options.bin),
        `${executable} could not be executed`,
      );
    }
    throw error;
  } finally {
    clearTimeout(timer);
    clearTimeout(grace);
    if (signal !== undefined) signal.removeEventListener("abort", onAbort);
  }

  if (interruption === "OUTPUT_LIMIT") {
    throw new RuneOutputLimitError(maxOutputBytes, outputLimitStream);
  }
  if (interruption === "TIMEOUT") {
    throw new RuneError("TIMEOUT", `rune ask did not finish within ${timeoutMs} ms`);
  }
  if (interruption === "ABORTED") {
    throw new RuneError("ABORTED", "rune ask was aborted before it finished");
  }
  if (code === null) {
    throw new RuneError("SIGNALED", `rune ask was terminated by signal ${term ?? "unknown"}`);
  }

  const stdoutText = Buffer.concat(stdout).toString("utf8");
  const stderrText = Buffer.concat(stderr).toString("utf8");
  const { result, detail } = parseResult(stdoutText);
  if (code !== 0) {
    throw new RuneExitError(code, result, stderrText);
  }
  if (result === null) {
    throw new RuneOutputError(stdoutText, stderrText, detail);
  }
  return result;
}

/** Collects the three locations the executable is looked up in, in order. */
function binaryCandidates(bin) {
  return [
    { source: "bin", value: bin === undefined ? null : bin },
    { source: "RUNE_BIN", value: emptyToNull(process.env.RUNE_BIN) },
    { source: "PATH", value: emptyToNull(process.env.PATH) },
  ];
}

/** Builds the message for a failed lookup, naming every location tried. */
function notFoundMessage(candidates, detail) {
  const lines = ["the rune executable was not found"];
  if (detail !== null) lines.push(`  ${detail}`);
  for (const candidate of candidates) {
    lines.push(`  ${candidate.source}: ${candidate.value ?? "not set"}`);
  }
  lines.push(
    "build it with `cargo build --release` and pass its path as the bin option, or set RUNE_BIN",
  );
  return lines.join("\n");
}

function emptyToNull(value) {
  return value === undefined || value === "" ? null : value;
}

/** Returns the path when it is an executable file, otherwise null. */
function findExecutable(candidate) {
  if (!candidate.includes("/") && !candidate.includes("\\")) {
    return searchPath(candidate);
  }
  return isExecutable(candidate) ? candidate : null;
}

function isExecutable(path) {
  try {
    if (!statSync(path).isFile()) return false;
    accessSync(path, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function searchPath(name) {
  const pathValue = process.env.PATH ?? "";
  const names = process.platform === "win32" && extname(name) === "" ? [name, `${name}.exe`] : [name];
  for (const directory of pathValue.split(delimiter)) {
    if (directory === "") continue;
    for (const executableName of names) {
      const candidate = join(directory, executableName);
      if (isExecutable(candidate)) return candidate;
    }
  }
  return null;
}

/** Reads the result object out of standard output. */
function parseResult(text) {
  const trimmed = text.trim();
  if (trimmed === "") return { result: null, detail: null };

  const whole = asResult(trimmed);
  if (whole !== null && whole.result !== null) return whole;

  // A wrapper script around the binary may print its own progress lines before
  // the object, so the last line with a valid result is used. Retain validation
  // details from the last object when no valid result was printed.
  const lines = trimmed.split("\n");
  let invalid = whole;
  for (let index = lines.length - 1; index >= 0; index -= 1) {
    const line = lines[index].trim();
    if (line === "") continue;
    const value = asResult(line);
    if (value === null) continue;
    if (value.result !== null) return value;
    invalid ??= value;
  }
  return invalid ?? { result: null, detail: null };
}

function asResult(text) {
  let value;
  try {
    value = JSON.parse(text);
  } catch {
    return null;
  }
  if (!isObject(value)) return null;

  const missing = [];
  const invalid = [];
  const check = (object, field, accepts, prefix = "", required = true) => {
    const path = `${prefix}${field}`;
    if (!Object.hasOwn(object, field)) {
      if (required) missing.push(path);
    } else if (!accepts(object[field])) {
      invalid.push(path);
    }
  };
  const isString = (entry) => typeof entry === "string";
  check(value, "output", isString);
  check(value, "final_output", isString);
  check(value, "exit_code", Number.isFinite);
  check(value, "model", isString);
  check(value, "resolved_provider", (entry) => entry === null || isString(entry));
  check(value, "session_id", isString);
  check(value, "steps", Number.isFinite);
  check(value, "usage", isObject);
  check(value, "tool_calls", Array.isArray);
  check(value, "error", isString, "", false);
  check(value, "error_code", isString, "", false);
  if (isObject(value.usage)) {
    check(value.usage, "input_tokens", Number.isFinite, "usage.", false);
    check(value.usage, "output_tokens", Number.isFinite, "usage.", false);
  }
  if (Array.isArray(value.tool_calls)) {
    value.tool_calls.forEach((call, index) => {
      const path = `tool_calls[${index}]`;
      if (!isObject(call)) {
        invalid.push(path);
        return;
      }
      check(call, "name", isString, `${path}.`);
      check(call, "status", (entry) => entry === "success" || entry === "error", `${path}.`);
    });
  }
  const details = [];
  if (missing.length > 0) details.push(`missing fields: ${missing.join(", ")}`);
  if (invalid.length > 0) details.push(`invalid fields: ${invalid.join(", ")}`);
  return details.length === 0
    ? { result: value, detail: null }
    : { result: null, detail: details.join("; ") };
}

function isObject(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function exitMessage(exitCode, result, stderr) {
  const detail = result?.error ?? firstLine(stderr);
  return detail === null || detail === ""
    ? `rune ask exited with code ${exitCode}`
    : `rune ask exited with code ${exitCode}: ${detail}`;
}

function firstLine(text) {
  const line = text
    .split("\n")
    .map((entry) => entry.trim())
    .find((entry) => entry !== "");
  return line ?? null;
}

function describe(text) {
  const trimmed = text.trim();
  return trimmed === "" ? "empty" : JSON.stringify(trimmed);
}
