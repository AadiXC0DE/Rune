# rune for Node

Runs the `rune` binary as a subprocess and returns the result object that
`rune ask --json` prints. The binary owns configuration, credentials, and the
network path, so this package has no dependencies and no build step.

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
| `ask(prompt, options)` | Runs one request, resolves with the parsed result object |
| `resolveBinary(options)` | Returns the path the next `ask` would run |
| `RuneError` | Base class, with a `code` of `BINARY_NOT_FOUND`, `EXIT`, `OUTPUT`, `OUTPUT_LIMIT`, `TIMEOUT`, `ABORTED`, or `SIGNALED` |
| `RuneBinaryNotFoundError` | No executable through the option, `RUNE_BIN`, or `PATH` |
| `RuneExitError` | Non-zero exit, with `exitCode`, `result`, and `stderr` |
| `RuneOutputError` | Clean exit with no valid result object on standard output; names missing or invalid fields |
| `RuneOutputLimitError` | Combined stdout and stderr exceeded the capture cap; carries `maxOutputBytes` and the `stream` that exceeded it |

`options` accepts `bin`, `model`, `effort`, `cwd`, `env`, `noSave`, `timeoutMs`,
`maxOutputBytes`, and `signal`. Types for every field of the result object are in
`index.d.ts`.
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

## Running the tests

```sh
node --test bindings/node/test.mjs
```

The suite drives a stub executable, so it needs no provider, no credential, and
no network.
