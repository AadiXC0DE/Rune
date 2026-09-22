import assert from "node:assert/strict";
import {
  accessSync,
  chmodSync,
  constants,
  mkdtempSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { after, test } from "node:test";

import {
  ask,
  resolveBinary,
  RuneBinaryNotFoundError,
  RuneExitError,
  RuneError,
  RuneOutputError,
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
