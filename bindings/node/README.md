# rune for Node

Runs the `rune` binary as a subprocess and returns the result object that
`rune ask --json` prints. The binary owns configuration, credentials, and the
network path, so this package has no dependencies and no build step.

## Building the binary

Requires Rust 1.98 or newer.

```sh
cargo build --release
```

That writes `target/release/rune`. The binding finds the binary through the
`bin` option first, then the `RUNE_BIN` environment variable, then an executable
named `rune` on `PATH`.

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
`cargo build`, or at any `rune` on `PATH`. A relative `bin` resolves against the
working directory, so run the example from the repository root, or pass an
absolute path.

A run that exits non-zero throws `RuneExitError`, whose `result` field carries
the same object the binary printed, including `error` and `error_code`. A
lookup that finds no binary throws `RuneBinaryNotFoundError`, which names the
option, `RUNE_BIN`, and `PATH`.

## API

| Export | Purpose |
| --- | --- |
| `ask(prompt, options)` | Runs one request, resolves with the parsed result object |
| `resolveBinary(options)` | Returns the path the next `ask` would run |
| `RuneError` | Base class, with a `code` of `BINARY_NOT_FOUND`, `EXIT`, `OUTPUT`, `TIMEOUT`, `ABORTED`, or `SIGNALED` |
| `RuneBinaryNotFoundError` | No executable through the option, `RUNE_BIN`, or `PATH` |
| `RuneExitError` | Non-zero exit, with `exitCode`, `result`, and `stderr` |
| `RuneOutputError` | Clean exit with no result object on standard output |

`options` accepts `bin`, `model`, `effort`, `cwd`, `env`, `noSave`, `timeoutMs`,
and `signal`. Types for every field of the result object are in `index.d.ts`.

## Running the tests

```sh
node --test bindings/node/test.mjs
```

The suite drives a stub executable, so it needs no provider, no credential, and
no network.
