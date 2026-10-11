import assert from "node:assert/strict";
import childProcess from "node:child_process";
import {
  accessSync,
  chmodSync,
  constants,
  mkdtempSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import { tmpdir } from "node:os";
import { delimiter, join, relative } from "node:path";
import { after, test } from "node:test";

import {
  ask,
  resolveBinary,
  RuneBinaryNotFoundError,
  RuneExitError,
  RuneError,
  RuneOutputError,
  RuneOutputLimitError,
} from "./index.js";

// The binding drives a process, so these tests drive a stub instead of the real
// binary: no provider, no credential, and no network are involved.
//
// The stub reproduces the one argument rule the binding must respect. A global
// flag is only read before the command name, and a bare `--` makes everything
// after it positional, so the prompt survives a leading dash.
const STUB = `#!/bin/sh
model=""
effort=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --model) model="$2"; shift 2; continue ;;
    --effort) effort="$2"; shift 2; continue ;;
    -*) shift; continue ;;
    *) break ;;
  esac
done
command="$1"
shift
prompt=""
separator=""
positional=0
for argument in "$@"; do
  if [ "$positional" = 0 ]; then
    case "$argument" in
      --) positional=1; continue ;;
      -*) continue ;;
    esac
  fi
  prompt="$prompt$separator$argument"
  separator=" "
done
if [ "\${RUNE_STUB_MODE:-ok}" = "result" ]; then
  printf '%s\\n' "$RUNE_STUB_STDOUT"
  printf '%s' "$RUNE_STUB_STDERR" >&2
  exit "\${RUNE_STUB_EXIT_CODE:-0}"
fi
if [ "\${RUNE_STUB_MODE:-ok}" = "fail" ]; then
  echo "rune: no model provider is connected" >&2
  printf '{"output":"","final_output":"","exit_code":1,"model":"%s","resolved_provider":null,"session_id":"","steps":0,"usage":{},"tool_calls":[],"error":"no model provider is connected","error_code":"authentication_required"}\\n' "$model"
  exit 1
fi
if [ "\${RUNE_STUB_MODE:-ok}" = "silent" ]; then
  exit 0
fi
printf '{"output":"%s","final_output":"%s","exit_code":0,"model":"%s","resolved_provider":"stub","session_id":"","steps":1,"usage":{"input_tokens":3,"output_tokens":5},"tool_calls":[{"name":"read_file","status":"success"}],"effort":"%s","command":"%s"}\\n' "$prompt" "$prompt" "$model" "$effort" "$command"
`;

function stubDirectory() {
  const directory = mkdtempSync(join(tmpdir(), "rune-node-stub-"));
  const executable = join(directory, "rune");
  writeFileSync(executable, STUB, { mode: 0o755 });
  chmodSync(executable, 0o755);
  return { directory, executable };
}

const first = stubDirectory();
const second = stubDirectory();
const stub = first.executable;
const emptyPath = mkdtempSync(join(tmpdir(), "rune-node-empty-"));

const savedPath = process.env.PATH;
const savedBinary = process.env.RUNE_BIN;

// Resolved from the inherited `PATH` so a stub that blocks does not depend on
// the empty `PATH` some tests install.
function sleepCommand() {
  for (const directory of (savedPath ?? "").split(delimiter)) {
    if (directory === "") continue;
    const candidate = join(directory, "sleep");
    try {
      accessSync(candidate, constants.X_OK);
      return candidate;
    } catch {
      // Try the next directory.
    }
  }
  return "sleep";
}

/** Runs `fn` with `PATH` replaced, restoring it before returning. */
function withPath(value, fn) {
  process.env.PATH = value;
  try {
    return fn();
  } finally {
    process.env.PATH = savedPath;
  }
}

/** Runs `fn` with `RUNE_BIN` replaced, restoring it before returning. */
function withBinary(value, fn) {
  if (value === undefined) delete process.env.RUNE_BIN;
  else process.env.RUNE_BIN = value;
  try {
    return fn();
  } finally {
    if (savedBinary === undefined) delete process.env.RUNE_BIN;
    else process.env.RUNE_BIN = savedBinary;
  }
}

after(() => {
  process.env.PATH = savedPath;
  if (savedBinary === undefined) delete process.env.RUNE_BIN;
  else process.env.RUNE_BIN = savedBinary;

  for (const directory of [first.directory, second.directory, emptyPath]) {
    rmSync(directory, { recursive: true, force: true });
  }
});

/** Captures the error a call throws, so it can be inspected. */
function capture(fn) {
  try {
    fn();
    return null;
  } catch (thrown) {
    return thrown;
  }
}

test("a successful run returns the result object", async () => {
  const prompt = "summarize the readme";
  const result = await ask(prompt, { bin: stub, model: "stub-model", noSave: true });

  assert.equal(result.output, prompt);
  assert.equal(result.final_output, prompt);
  assert.equal(result.exit_code, 0);
  assert.equal(result.model, "stub-model");
  assert.equal(result.resolved_provider, "stub");
  assert.equal(result.session_id, "");
  assert.equal(result.steps, 1);
  assert.equal(result.usage.input_tokens, 3);
  assert.equal(result.usage.output_tokens, 5);
  assert.deepEqual(result.tool_calls, [{ name: "read_file", status: "success" }]);
  assert.equal(result.error, undefined);
  assert.equal(result.error_code, undefined);
});

test("an explicit relative binary runs from options.cwd", async (t) => {
  for (const cwd of [first.directory, relative(process.cwd(), first.directory)]) {
    await t.test(`cwd ${cwd}`, async () => {
      assert.equal(resolveBinary({ bin: "./rune", cwd }), stub);
      const result = await ask("relative cwd stub", { bin: "./rune", cwd });
      assert.equal(result.output, "relative cwd stub");
      assert.equal(result.resolved_provider, "stub");
    });
  }
});

test("an explicit relative binary defaults to the caller's working directory", async () => {
  const bin = `./${relative(process.cwd(), stub)}`;
  const result = await ask("default cwd stub", { bin });
  assert.equal(result.output, "default cwd stub");
});

test("a relative binary missing from cwd does not fall back to the caller's directory", () => {
  const savedCwd = process.cwd();
  try {
    process.chdir(first.directory);
    const error = capture(() => resolveBinary({ bin: "./rune", cwd: emptyPath }));
    assert.ok(error instanceof RuneBinaryNotFoundError);
    assert.equal(error.candidates[0].value, "./rune");
  } finally {
    process.chdir(savedCwd);
  }
});

test("an absolute binary runs with a different cwd", async () => {
  const result = await ask("absolute binary stub", { bin: stub, cwd: emptyPath });
  assert.equal(result.output, "absolute binary stub");
});

test("a bare binary name still uses PATH with a different cwd", async () => {
  const result = await withPath(first.directory, () =>
    ask("PATH binary stub", { bin: "rune", cwd: emptyPath }),
  );
  assert.equal(result.output, "PATH binary stub");
});

test("the model and effort options reach the binary as global flags", async () => {
  const result = await ask("hi", {
    bin: stub,
    model: "stub-model",
    effort: "high",
    noSave: true,
  });

  // The stub reads these the way the binary does, before the command name, and
  // keeps them out of the prompt.
  assert.equal(result.command, "ask");
  assert.equal(result.model, "stub-model");
  assert.equal(result.effort, "high");
  assert.equal(result.output, "hi");
});

test("a prompt starting with a dash stays the prompt", async () => {
  const result = await ask("--model not-a-flag", { bin: stub });
  assert.equal(result.output, "--model not-a-flag");
  assert.equal(result.model, "");
});

test("a non-zero exit becomes a typed error carrying the result", async () => {
  const error = await ask("hello", {
    bin: stub,
    noSave: true,
    env: { RUNE_STUB_MODE: "fail" },
  }).then(
    () => null,
    (thrown) => thrown,
  );

  assert.ok(error instanceof RuneExitError, `expected RuneExitError, got ${error}`);
  assert.ok(error instanceof RuneError);
  assert.equal(error.code, "EXIT");
  assert.equal(error.exitCode, 1);
  assert.equal(error.result.exit_code, 1);
  assert.equal(error.result.error, "no model provider is connected");
  assert.equal(error.result.error_code, "authentication_required");
  assert.match(error.message, /exited with code 1: no model provider is connected/);
  assert.match(error.stderr, /no model provider is connected/);
});

test("a clean exit with no result object becomes a typed error", async () => {
  const error = await ask("hello", {
    bin: stub,
    env: { RUNE_STUB_MODE: "silent" },
  }).then(
    () => null,
    (thrown) => thrown,
  );

  assert.ok(error instanceof RuneOutputError, `expected RuneOutputError, got ${error}`);
  assert.equal(error.code, "OUTPUT");
});

const validResult = {
  output: "ok",
  final_output: "ok",
  exit_code: 0,
  model: "stub-model",
  resolved_provider: null,
  session_id: "",
  steps: 1,
  usage: {},
  tool_calls: [],
};

function askWithOutput(stdout, exitCode = 0, options = {}) {
  return ask("hello", {
    ...options,
    bin: stub,
    env: {
      RUNE_STUB_MODE: "result",
      RUNE_STUB_STDOUT: stdout,
      RUNE_STUB_STDERR: "stub diagnostic",
      RUNE_STUB_EXIT_CODE: String(exitCode),
    },
  });
}

function nodeStub(name, source) {
  const executable = join(first.directory, name);
  writeFileSync(executable, `#!${process.execPath}\n${source}`, { mode: 0o755 });
  chmodSync(executable, 0o755);
  return executable;
}

test("stdout and stderr each trigger a typed output-limit failure in bytes", async (t) => {
  for (const stream of ["stdout", "stderr"]) {
    await t.test(stream, async () => {
      // Fifty characters are 150 UTF-8 bytes, even though they fit in 100 code units.
      const bin = nodeStub(`rune-large-${stream}`, `process.${stream}.write("界".repeat(50));`);
      await assert.rejects(ask("hello", { bin, maxOutputBytes: 100 }), (error) => {
        assert.ok(error instanceof RuneOutputLimitError);
        assert.ok(error instanceof RuneError);
        assert.equal(error.code, "OUTPUT_LIMIT");
        assert.equal(error.name, "RuneOutputLimitError");
        assert.equal(error.maxOutputBytes, 100);
        assert.equal(error.stream, stream);
        assert.match(error.message, /combined output limit of 100 bytes/);
        return true;
      });
    });
  }
});

test("the combined cap allows exactly the limit and rejects the next byte", async () => {
  const result = { ...validResult, output: "界", final_output: "界" };
  const stdout = JSON.stringify(result);
  const maxOutputBytes = Buffer.byteLength(`${stdout}\nstub diagnostic`);
  assert.deepEqual(await askWithOutput(stdout, 0, { maxOutputBytes }), result);
  await assert.rejects(askWithOutput(stdout, 0, { maxOutputBytes: maxOutputBytes - 1 }), (error) => {
    assert.ok(error instanceof RuneOutputLimitError);
    assert.equal(error.maxOutputBytes, maxOutputBytes - 1);
    assert.ok(["stdout", "stderr"].includes(error.stream));
    return true;
  });
});

test("the default output cap rejects oversized output without an option", async () => {
  const bin = nodeStub("rune-default-cap", 'process.stderr.write(Buffer.alloc(9 * 1024 * 1024));');
  await assert.rejects(ask("hello", { bin }), (error) => {
    assert.ok(error instanceof RuneOutputLimitError);
    assert.equal(error.maxOutputBytes, 8 * 1024 * 1024);
    assert.equal(error.stream, "stderr");
    return true;
  });
});

test("output from an inherited pipe is bounded after the parent exits", { timeout: 10_000 }, async () => {
  const writer = `
    process.stdout.on("error", () => process.exit(0));
    setTimeout(() => {
      process.stdout.write(Buffer.alloc(1024));
      setInterval(() => process.stdout.write("x"), 50);
    }, 200);
  `;
  const bin = nodeStub("rune-inherited-output", `
    const { spawn } = require("node:child_process");
    spawn(process.execPath, ["-e", ${JSON.stringify(writer)}], { stdio: "inherit" }).unref();
    process.exit(0);
  `);
  await assert.rejects(ask("hello", { bin, maxOutputBytes: 100, timeoutMs: 5000 }), (error) => {
    assert.ok(error instanceof RuneOutputLimitError);
    assert.equal(error.stream, "stdout");
    return true;
  });
});

test("invalid output caps fail before binary lookup or spawning", async () => {
  const originalSpawn = childProcess.spawn;
  let spawnCalls = 0;
  childProcess.spawn = (...args) => {
    spawnCalls += 1;
    return originalSpawn(...args);
  };
  syncBuiltinESMExports();
  try {
    for (const maxOutputBytes of [0, -1, 0.5, Infinity, -Infinity, NaN, 2 ** 53, "100", null]) {
      await assert.rejects(
        ask("hello", { bin: join(first.directory, "absent"), maxOutputBytes }),
        (error) => error instanceof TypeError && /maxOutputBytes/.test(error.message),
      );
    }
    assert.equal(spawnCalls, 0);
  } finally {
    childProcess.spawn = originalSpawn;
    syncBuiltinESMExports();
  }
});

test("output-limit shutdown drains both streams with bounded retained memory", { timeout: 15_000 }, async (t) => {
  const bin = nodeStub("rune-flood", `
    process.on("SIGTERM", () => {});
    const chunk = Buffer.alloc(64 * 1024);
    async function flood() {
      for (;;) {
        await new Promise((resolve) => process.stdout.write(chunk, resolve));
        await new Promise((resolve) => process.stderr.write(chunk, resolve));
      }
    }
    flood();
  `);
  // Isolate the heap from other tests and collect before measuring retained
  // buffers. The stub continues emitting far beyond the cap during SIGTERM grace.
  const source = `
    import assert from "node:assert/strict";
    import childProcess from "node:child_process";
    import { syncBuiltinESMExports } from "node:module";
    import { ask, RuneOutputLimitError } from ${JSON.stringify(new URL("./index.js", import.meta.url).href)};
    global.gc();
    const baseline = process.memoryUsage().arrayBuffers;
    const maxOutputBytes = 1024 * 1024;
    let observed = 0;
    let samples = 0;
    let retained = 0;
    let nextSample = 4 * 1024 * 1024;
    const kills = [];
    const originalSpawn = childProcess.spawn;
    childProcess.spawn = (...args) => {
      const child = originalSpawn(...args);
      const originalKill = child.kill.bind(child);
      child.kill = (signal) => {
        kills.push(signal ?? "SIGTERM");
        return originalKill(signal);
      };
      for (const stream of [child.stdout, child.stderr]) {
        stream.on("data", (chunk) => {
          observed += chunk.length;
          if (observed < nextSample) return;
          nextSample = observed + 4 * 1024 * 1024;
          global.gc();
          global.gc();
          samples += 1;
          retained = Math.max(retained, process.memoryUsage().arrayBuffers - baseline);
        });
      }
      return child;
    };
    syncBuiltinESMExports();
    await assert.rejects(ask("hello", { bin: ${JSON.stringify(bin)}, maxOutputBytes, timeoutMs: 6000 }), RuneOutputLimitError);
    assert.deepEqual(kills, ["SIGTERM", "SIGKILL"]);
    assert.ok(observed >= 32 * 1024 * 1024, "stub must emit well beyond the cap");
    assert.ok(samples >= 8, "measure retained memory repeatedly during grace");
    assert.ok(retained <= maxOutputBytes + 2 * 1024 * 1024, "retained buffers exceeded cap plus pipe allowance: " + retained);
    console.log(JSON.stringify({ observed, samples, retained, maxOutputBytes }));
  `;
  const runner = childProcess.spawn(process.execPath, ["--expose-gc", "--input-type=module", "-e", source], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "";
  let stderr = "";
  runner.stdout.on("data", (chunk) => {
    stdout += chunk;
  });
  runner.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  const status = await new Promise((resolve, reject) => {
    runner.once("error", reject);
    runner.once("close", resolve);
  });
  assert.equal(status, 0, stderr);
  const measurements = JSON.parse(stdout);
  assert.ok(measurements.observed > measurements.maxOutputBytes);
  t.diagnostic(JSON.stringify(measurements));
});

test("a result containing only output and exit_code names every missing field", async () => {
  const stdout = JSON.stringify({ output: "ok", exit_code: 0 });
  await assert.rejects(askWithOutput(stdout), (error) => {
    assert.ok(error instanceof RuneOutputError);
    assert.equal(error.code, "OUTPUT");
    assert.equal(error.stdout, `${stdout}\n`);
    assert.equal(error.stderr, "stub diagnostic");
    assert.match(
      error.message,
      /missing fields: final_output, model, resolved_provider, session_id, steps, usage, tool_calls/,
    );
    return true;
  });
});

test("every required result field is checked for presence", async (t) => {
  for (const field of Object.keys(validResult)) {
    await t.test(field, async () => {
      const result = { ...validResult };
      delete result[field];
      await assert.rejects(askWithOutput(JSON.stringify(result)), (error) => {
        assert.ok(error instanceof RuneOutputError);
        assert.ok(error.message.includes(`missing fields: ${field}`));
        return true;
      });
    });
  }
});

test("result fields must have their declared types", async (t) => {
  const cases = [
    ["output", null],
    ["final_output", 0],
    ["exit_code", "0"],
    ["model", null],
    ["resolved_provider", 3],
    ["session_id", false],
    ["steps", "1"],
    ["usage", null],
    ["usage", []],
    ["tool_calls", {}],
    ["error", null],
    ["error_code", 0],
    ["usage.input_tokens", { usage: { input_tokens: "3" } }],
    ["usage.output_tokens", { usage: { output_tokens: null } }],
    ["tool_calls[0]", { tool_calls: [null] }],
    ["tool_calls[0].name", { tool_calls: [{ status: "success" }] }],
    ["tool_calls[0].name", { tool_calls: [{ name: 0, status: "success" }] }],
    ["tool_calls[0].status", { tool_calls: [{ name: "read_file" }] }],
    ["tool_calls[0].status", { tool_calls: [{ name: "read_file", status: "pending" }] }],
  ];
  for (const [field, value] of cases) {
    await t.test(field, async () => {
      const override = field.includes(".") || field.includes("[") ? value : { [field]: value };
      const result = { ...validResult, ...override };
      await assert.rejects(askWithOutput(JSON.stringify(result)), (error) => {
        assert.ok(error instanceof RuneOutputError);
        assert.ok(error.message.split("; standard output")[0].includes(field));
        return true;
      });
    });
  }
  for (const field of ["exit_code", "steps", "usage.input_tokens", "usage.output_tokens"]) {
    await t.test(`nonfinite ${field}`, async () => {
      const result = field.startsWith("usage.")
        ? { ...validResult, usage: { [field.slice(6)]: "OVERFLOW" } }
        : { ...validResult, [field]: "OVERFLOW" };
      const stdout = JSON.stringify(result).replace('"OVERFLOW"', "1e400");
      await assert.rejects(askWithOutput(stdout), (error) => {
        assert.ok(error instanceof RuneOutputError);
        assert.ok(error.message.split("; standard output")[0].includes(field));
        return true;
      });
    });
  }
});

test("valid results preserve optional fields and wrapper output parsing", async () => {
  const extended = {
    ...validResult,
    resolved_provider: "stub",
    usage: { input_tokens: 0, output_tokens: 5 },
    tool_calls: [{ name: "read_file", status: "error" }],
    error: "tool failed",
    error_code: "tool_execution_failed",
    extra: true,
  };
  for (const result of [validResult, extended]) {
    assert.deepEqual(await askWithOutput(JSON.stringify(result, null, 2)), result);
    const stdout = `wrapper progress\n${JSON.stringify(result)}\n{"output":"partial","exit_code":0}\n`;
    assert.deepEqual(await askWithOutput(stdout), result);
  }
});

test("an invalid result on a failed exit preserves RuneExitError and stderr", async () => {
  await assert.rejects(askWithOutput('{"output":"partial","exit_code":3}', 3), (error) => {
    assert.ok(error instanceof RuneExitError);
    assert.equal(error.exitCode, 3);
    assert.equal(error.result, null);
    assert.equal(error.stderr, "stub diagnostic");
    assert.equal(error.message, "rune ask exited with code 3: stub diagnostic");
    return true;
  });
});

test("a failed exit with nothing on either stream is reported without a gap", async () => {
  const quiet = join(first.directory, "rune-quiet");
  writeFileSync(quiet, "#!/bin/sh\nexit 3\n", { mode: 0o755 });
  chmodSync(quiet, 0o755);

  const error = await ask("hello", { bin: quiet }).then(
    () => null,
    (thrown) => thrown,
  );

  assert.ok(error instanceof RuneExitError);
  assert.equal(error.exitCode, 3);
  assert.equal(error.result, null);
  assert.equal(error.message, "rune ask exited with code 3");
});

test("the option, RUNE_BIN, and PATH are searched in that order", () => {
  withBinary(undefined, () => {
    withPath(emptyPath, () => {
      assert.equal(resolveBinary({ bin: stub }), stub);
    });
    // RUNE_BIN outranks PATH, and an explicit option outranks both.
    withPath(second.directory, () => {
      withBinary(stub, () => {
        assert.equal(resolveBinary(), stub);
        assert.equal(resolveBinary({ bin: second.executable }), second.executable);
      });
    });
    withPath(first.directory, () => {
      assert.equal(resolveBinary(), stub);
    });
  });
});

test("a binary that is nowhere names the three locations", () => {
  const error = withBinary(undefined, () =>
    withPath(emptyPath, () => capture(() => resolveBinary())),
  );

  assert.ok(error instanceof RuneBinaryNotFoundError);
  assert.equal(error.code, "BINARY_NOT_FOUND");
  assert.deepEqual(error.candidates, [
    { source: "bin", value: null },
    { source: "RUNE_BIN", value: null },
    { source: "PATH", value: emptyPath },
  ]);
  assert.match(error.message, /^the rune executable was not found$/m);
  assert.match(error.message, /^ {2}bin: not set$/m);
  assert.match(error.message, /^ {2}RUNE_BIN: not set$/m);
  assert.ok(error.message.includes(`PATH: ${emptyPath}`));
  assert.match(error.message, /cargo build --release/);
});

test("a bin option that is not executable is reported, not skipped", () => {
  const missing = join(first.directory, "absent");
  const error = capture(() => resolveBinary({ bin: missing }));

  assert.ok(error instanceof RuneBinaryNotFoundError);
  assert.match(error.message, new RegExp(`${missing} is not an executable file`));
});

test("invalid timeoutMs values fail before spawning and emit no overflow warnings", async (t) => {
  const originalSpawn = childProcess.spawn;
  let spawnCalls = 0;
  const warnings = [];
  const onWarning = (warning) => {
    if (warning.name === "TimeoutOverflowWarning") warnings.push(warning);
  };
  childProcess.spawn = (...args) => {
    spawnCalls += 1;
    return originalSpawn(...args);
  };
  // Update the binding's named import so every spawn is observed.
  syncBuiltinESMExports();
  process.on("warning", onWarning);
  try {
    for (const timeoutMs of [
      Infinity,
      NaN,
      -Infinity,
      2 ** 31,
      2 ** 31 - 0.5,
      Number.MAX_VALUE,
      0,
      -1,
      "1000",
      null,
    ]) {
      await t.test(String(timeoutMs), async () => {
        spawnCalls = 0;
        warnings.length = 0;
        const error = await ask("hello", { bin: stub, timeoutMs }).then(
          () => null,
          (thrown) => thrown,
        );
        // Node delivers timer warnings on a later tick.
        await new Promise((resolve) => setImmediate(resolve));
        assert.ok(error instanceof TypeError, `expected TypeError, got ${error}`);
        assert.match(error.message, /timeoutMs/);
        assert.equal(spawnCalls, 0);
        assert.deepEqual(warnings, []);
      });
    }
    await assert.rejects(
      ask("hello", { bin: join(first.directory, "absent"), timeoutMs: Infinity }),
      TypeError,
    );
  } finally {
    childProcess.spawn = originalSpawn;
    syncBuiltinESMExports();
    process.removeListener("warning", onWarning);
  }
});

test("timeoutMs accepts the timer limit and positive fractional values", async () => {
  const warnings = [];
  const onWarning = (warning) => {
    if (warning.name === "TimeoutOverflowWarning") warnings.push(warning);
  };
  process.on("warning", onWarning);
  try {
    for (const timeoutMs of [2 ** 31 - 1, 1000.5]) {
      const result = await ask("valid timeout", { bin: stub, timeoutMs });
      assert.equal(result.output, "valid timeout");
    }
    await new Promise((resolve) => setImmediate(resolve));
    assert.deepEqual(warnings, []);
  } finally {
    process.removeListener("warning", onWarning);
  }
});

test("timeoutMs bounds a binary that outlives it", async () => {
  const slow = join(first.directory, "rune-slow");
  // The sleeping helper is named by absolute path so the stub does not depend
  // on `PATH`, which other tests replace.
  writeFileSync(slow, `#!/bin/sh\n"${sleepCommand()}" 30\n`, { mode: 0o755 });
  chmodSync(slow, 0o755);

  const started = Date.now();
  const error = await ask("hello", { bin: slow, timeoutMs: 300 }).then(
    () => null,
    (thrown) => thrown,
  );
  const elapsed = Date.now() - started;

  assert.ok(error instanceof RuneError, `expected RuneError, got ${error}`);
  assert.equal(error.code, "TIMEOUT");
  // A sleep of thirty seconds must not hold the caller: the reported bound is
  // the point of the option.
  assert.ok(elapsed < 10_000, `settled after ${elapsed} ms, past the bound`);
});
