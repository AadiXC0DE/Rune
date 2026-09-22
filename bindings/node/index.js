// Drives the `rune` binary as a subprocess.
//
// The binary owns configuration, credentials, and the network path; this module
// only builds an argument list, runs it, and returns the result object that
// `rune ask --json` prints. No provider, credential, or session state is read
// or written here.

import { spawn } from "node:child_process";
import { accessSync, constants, statSync } from "node:fs";
import { delimiter, join } from "node:path";

/**
 * Reasoning effort names the command line accepts.
 *
 * An unrecognized value is silently treated as `auto` by the binary, so it is
 * rejected here instead of changing behavior without a report.
 */
const EFFORTS = ["auto", "none", "minimal", "low", "medium", "high", "xhigh", "max"];

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

/** The binary exited cleanly without printing the result object. */
export class RuneOutputError extends RuneError {
  constructor(stdout, stderr) {
    super(
      "OUTPUT",
      `rune ask exited cleanly without printing the result object; standard output was ${describe(stdout)}`,
    );
    this.name = "RuneOutputError";
    this.stdout = stdout;
    this.stderr = stderr;
  }
}

/**
 * Resolves the executable to run.
 *
 * The `bin` option wins, then `RUNE_BIN`, then an executable named `rune` on
 * `PATH`. A source that is configured but unusable is an error rather than a
 * silent fallback, so a typo is reported instead of running another binary.
 *
 * @param {{ bin?: string }} [options]
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
    const found = findExecutable(bin);
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

  const { model, effort, cwd, env, noSave = false, timeoutMs, signal } = options;
  if (model !== undefined && (typeof model !== "string" || model.length === 0)) {
    throw new TypeError("the model option must be a non-empty string");
  }
  if (effort !== undefined && !EFFORTS.includes(effort)) {
    throw new TypeError(`the effort option must be one of ${EFFORTS.join(", ")}`);
  }
  if (timeoutMs !== undefined && !(typeof timeoutMs === "number" && timeoutMs > 0)) {
    throw new TypeError("the timeoutMs option must be a positive number");
  }

  const executable = resolveBinary(options);
  const args = [];
  // Options that apply to the whole process are only read before the command
  // name; everything after it belongs to the command.
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
  child.stdout.on("data", (chunk) => stdout.push(chunk));
  child.stderr.on("data", (chunk) => stderr.push(chunk));

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

  const stdoutText = Buffer.concat(stdout).toString("utf8");
  const stderrText = Buffer.concat(stderr).toString("utf8");

  if (interruption === "TIMEOUT") {
    throw new RuneError("TIMEOUT", `rune ask did not finish within ${timeoutMs} ms`);
  }
  if (interruption === "ABORTED") {
    throw new RuneError("ABORTED", "rune ask was aborted before it finished");
  }
  if (code === null) {
    throw new RuneError("SIGNALED", `rune ask was terminated by signal ${term ?? "unknown"}`);
  }

  const result = parseResult(stdoutText);
  if (code !== 0) {
    throw new RuneExitError(code, result, stderrText);
  }
  if (result === null) {
    throw new RuneOutputError(stdoutText, stderrText);
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
  for (const directory of pathValue.split(delimiter)) {
    if (directory === "") continue;
    const candidate = join(directory, name);
    if (isExecutable(candidate)) return candidate;
  }
  return null;
}

/** Reads the result object out of standard output. */
function parseResult(text) {
  const trimmed = text.trim();
  if (trimmed === "") return null;

  const whole = asResult(trimmed);
  if (whole !== null) return whole;

  // A wrapper script around the binary may print its own progress lines before
  // the object, so the last line that parses is used.
  const lines = trimmed.split("\n");
  for (let index = lines.length - 1; index >= 0; index -= 1) {
    const line = lines[index].trim();
    if (line === "") continue;
    const value = asResult(line);
    if (value !== null) return value;
  }
  return null;
}

function asResult(text) {
  try {
    const value = JSON.parse(text);
    const isObject = typeof value === "object" && value !== null && !Array.isArray(value);
    if (!isObject || typeof value.output !== "string" || typeof value.exit_code !== "number") {
      return null;
    }
    return value;
  } catch {
    return null;
  }
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
