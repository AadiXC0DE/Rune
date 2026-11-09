# Benchmark tasks

Tasks for `rune bench`, in the `task.json` and `repo/` format other harnesses'
suites also use, so the same tasks can measure each of them.

```sh
rune bench bench/tasks                 # every task, once
rune bench bench/tasks --trials 3      # three runs of each
bench/check.sh                         # prove each task is solvable and unsolved as shipped
```

| Task | Language | What it takes |
|---|---|---|
| `js-lru-cache` | JavaScript | Implement a least-recently-used cache from its tests, including recency on reads and falsy values |
| `js-retry-backoff` | JavaScript | Two bugs across two modules: retry counting and an uncapped, off-by-one backoff |
| `py-merge-intervals` | Python | Touching and unsorted intervals, input left unmodified, invalid intervals refused |
| `py-ini-parser` | Python | Values containing `=`, inline comments outside quotes, keys before any section |
| `go-wordfreq` | Go | Unicode-aware word splitting, case folding and alphabetical ties |
| `rust-semver` | Rust | Pre-release precedence from Semantic Versioning 2.0.0, strict parsing |

Each task directory holds:

| Path | What it is |
|---|---|
| `task.json` | `prompt`, the `verify` command (exit 0 is a pass), the `protected` paths restored before verifying, and `timeoutMs` |
| `repo/` | The project the agent works in, with failing tests |
| `solution/` | Files that, copied over `repo/`, make `verify` pass; never shown to the agent |

The JavaScript tasks run `node --test`, which finds `*.test.js` files on every
supported Node release. Passing a directory such as `node --test test/` is read
as a file on Node 22 and fails before any test runs.
