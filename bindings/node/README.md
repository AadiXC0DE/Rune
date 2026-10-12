# rune for Node

Runs the `rune` binary as a subprocess and returns the result object that
`rune ask --json` prints, or streams events from `rune ask --jsonl` through an
async iterator. The binary owns configuration, credentials, and the
network path, so this package has no dependencies and no build step.

## Version policy

This package shares the Rust workspace version. Its `package.json` version must
equal `workspace.package.version` in the root `Cargo.toml`; update both on every
release, including binary-only changes. There is no independent Node version
track. The maintained repository is
[AadiXC0DE/Rune](https://github.com/AadiXC0DE/Rune).
The test suite below checks version equality and repository links in CI.

## Building the binary

Requires Rust 1.98 or newer.

```sh
cargo build --release
```

That writes `target/release/rune` (`target/release/rune.exe` on Windows).
The binding finds the binary through the
`bin` option first, then the `RUNE_BIN` environment variable, then an executable
named `rune` on `PATH`, also checking `rune.exe` on Windows.

## Using it

```js
import { ask, RuneExitError } from "./index.js";

try {
  const result = await ask("Summarize the staged changes", {
    bin: "./target/release/rune",
    model: "gpt-5",
    noSave: true,
  });
  console.log(result.output);
  console.log(result.usage.input_tokens, result.usage.output_tokens);
} catch (error) {
  if (error instanceof RuneExitError) {
    console.error(`rune failed (${error.exitCode}): ${error.message}`);
  } else {
    throw error;
  }
}
```

Point `bin` at `target/release/rune`, at `target/debug/rune` after
`cargo build`, or at any `rune` on `PATH`. An explicit relative path such as
`./rune` resolves against `options.cwd`, or the current working directory when
`cwd` is omitted. This applies to both `ask` and `resolveBinary`. A bare name
such as `rune` is searched on `PATH`, with an `.exe` fallback on Windows.

A run that exits non-zero throws `RuneExitError`, whose `result` field carries
the validated object the binary printed, including `error` and `error_code`, or
`null` if no valid result was printed. A lookup that finds no binary throws
`RuneBinaryNotFoundError`, which names the option, `RUNE_BIN`, and `PATH`.

## API

| Export | Purpose |
| --- | --- |
| `ask(prompt, options)` | Runs one bounded turn, resolves with the parsed result object |
| `askStream(prompt, options)` | Yields JSONL events through an async iterator with bounded buffering |
| `resolveBinary(options)` | Returns the path the next `ask` would run |
| `RuneError` | Base class, with a `code` of `BINARY_NOT_FOUND`, `EXIT`, `OUTPUT`, `OUTPUT_LIMIT`, `TIMEOUT`, `ABORTED`, or `SIGNALED` |
| `RuneBinaryNotFoundError` | No executable through the option, `RUNE_BIN`, or `PATH` |
| `RuneExitError` | Non-zero exit, with `exitCode`, `result`, and `stderr` |
| `RuneOutputError` | Invalid result object or malformed JSONL stream; names missing or invalid fields |
| `RuneOutputLimitError` | Output exceeded a byte cap; carries `maxOutputBytes`, the `stream`, and the `scope` (`combined` or `record`) |

`options` accepts `bin`, `provider`, `offline`, `model`, `effort`, `cwd`, `env`,
`noSave`, `timeoutMs`, `maxOutputBytes`, and `signal`. Types for every field of
the result object are in `index.d.ts`.
Set `provider` to a non-empty provider name to pass `--provider <name>`.
Set `offline: true` to pass `--offline`; false or omission adds no offline flag
and leaves the binary's configured offline setting in effect. Both flags are
passed before `ask`, as are `model` and `effort`. Invalid provider values and
non-boolean offline values throw `TypeError` before resolving or spawning the
binary.
The binding validates required fields, token counts when present, and tool call
names and statuses before returning a result. `resolved_provider` may be `null`,
and unreported token counts and failure fields may be absent. Extra fields are
preserved. Wrapper progress lines are allowed; the last valid result is used.
`timeoutMs` must be a finite positive number at most 2,147,483,647 milliseconds.
Invalid values throw `TypeError` before resolving or spawning the binary.

Captured stdout and stderr share an 8 MiB byte cap by default, including wrapper
progress lines and diagnostics. Set `maxOutputBytes` to a positive safe integer
to change it. Exactly the cap is allowed; the next byte triggers
`RuneOutputLimitError` with code `OUTPUT_LIMIT`, discards captured output, and
kills the child. Further output is drained without retention while the child
stops. A child that ignores SIGTERM is sent SIGKILL after a two-second grace.
Invalid caps throw `TypeError` before resolving or spawning the binary.

## Streaming events

```js
import { askStream } from "./index.js";

const controller = new AbortController();
for await (const event of askStream("Summarize the staged changes", {
  bin: "./target/release/rune",
  noSave: true,
  signal: controller.signal,
})) {
  if (event.type === "text_delta") process.stdout.write(event.delta);
  if (event.type === "final") console.log(event.result.usage);
}
```

`askStream` starts the subprocess on the first iterator pull and yields `start`,
`step_start`, `step_restart`, `text_delta`, and `plan_updated` records as they
arrive. The successful `final` event carries the same validated result as `ask`
and is yielded after the subprocess finishes. A failed run throws `RuneExitError`
with its validated final result in `error.result`. Malformed records, invalid
event fields, events after `final`, and incomplete streams throw `RuneOutputError`.
Streaming requires JSONL output from the binary; wrapper progress lines are
not accepted.

The iterator reads on demand, so a slow consumer applies backpressure to stdout.
It retains one current pipe chunk and one record, plus Node's pipe buffer and
bounded stderr diagnostics. It does not accumulate a queue of parsed events.
`maxEventBytes` caps each record including its newline, defaults to 1 MiB, and
accepts positive integers up to 1 MiB, the CLI wire cap. Oversized records,
including unterminated lines, throw `RuneOutputLimitError`; its `maxOutputBytes`
field holds the record cap and `scope` is `record` in that case. The shared `maxOutputBytes` option also
caps total stdout and stderr bytes over the entire run, including events already
consumed, and defaults to 8 MiB. Increase it for longer streams. Exactly either
cap is allowed.

Call `controller.abort()` to cancel, which kills the child even while the
iterator is suspended at a yield. Its next pull rejects with code `ABORTED`.
Breaking out of the loop or calling `return()` also kills the child and waits
for it to exit. `timeoutMs` includes time spent waiting for a slow consumer.
Cancellation, timeout, and iterator cleanup close the pipes and send SIGTERM,
then SIGKILL after two seconds if needed. Consume or close every started iterator.

## Running the tests

```sh
node --test bindings/node/test.mjs
```

Run this command from the repository root. CI runs it with Node.js 22 in the
fast job on pushes to `main` and pull requests; a failing test fails the job.
The suite drives stub executables, so it needs no provider, no credential, and
no network. Streaming fixtures verify incremental UTF-8 deltas, final results,
byte caps, backpressure, cancellation, and iterator cleanup.
