# Benchmark tasks

Tasks for `rune bench`, in the `task.json` and `repo/` format other harnesses'
suites also use, so the same tasks can measure each of them.

```sh
rune bench bench/tasks                 # every task, once
rune bench bench/tasks --trials 3      # three runs of each
python3 bench/kit/run.py validate --tasks bench/tasks   # each task: oracle passes, noop fails
```

| Task | Language | What it takes |
|---|---|---|
| `js-lru-cache` | JavaScript | Implement a least-recently-used cache from its tests, including recency on reads and falsy values |
| `js-retry-backoff` | JavaScript | Two bugs across two modules: retry counting and an uncapped, off-by-one backoff |
| `py-merge-intervals` | Python | Touching and unsorted intervals, input left unmodified, invalid intervals refused |
| `py-ini-parser` | Python | Values containing `=`, inline comments outside quotes, keys before any section |
| `go-wordfreq` | Go | Unicode-aware word splitting, case folding and alphabetical ties |
| `rust-semver` | Rust | Pre-release precedence from Semantic Versioning 2.0.0, strict parsing |
| `js-event-emitter-once` | JavaScript | `once` listeners that never unregister and removal while emitting |
| `ts-money-format` | TypeScript | Sign loss, rounding of negative halves, digit grouping and a total that disagrees with its rows |
| `js-csv-export` | JavaScript | Quoting rules and column order from a schema |
| `ts-config-merge` | TypeScript | Layered settings: deep merge, array replacement, null removal and layer order |
| `js-rate-limiter` | JavaScript | Token bucket refill units and cap, plus a multi-token removal |
| `js-router-params` | JavaScript | Trailing slashes, route specificity and percent-decoding |
| `js-monorepo-instructions` | JavaScript | A shared pricing bug in a monorepo with nested instructions; the obvious fix breaks another package |
| `ts-scheduler-stable` | TypeScript | A stable priority queue where the obvious fix breaks re-queued job order |
| `py-roman-numerals` | Python | Strict Roman numerals for 1 to 3999 with correct subtractive forms |
| `py-duration-parse` | Python | Duration text with days, hours, minutes and seconds, strict input and a round-trip printed form |
| `py-inventory-ledger` | Python | Exact cents in stock totals and returns that are not sales |
| `py-log-cli-since` | Python | A `--since` option with time zones and a case-insensitive `--level` |
| `py-ttl-cache-refactor` | Python | Two copies of expiry logic merged into one with a single boundary rule |
| `py-graph-topo` | Python | Three layered faults in a dependency ordering tool, one after another |
| `py-pipeline-long-log` | Python | A record format hidden in about 400 lines of test output |
| `py-background-server` | Python | A byte versus character length bug between a JSON client and a server run in a thread |
| `go-humanize-bytes` | Go | IEC byte units with one decimal, unit switching at 1024 and carry on rounding |
| `go-ring-buffer` | Go | Eviction timing and iteration order of a ring buffer after wrap-around |
| `go-config-env` | Go | Layered settings from defaults, a file and the environment, with empty variables ignored |
| `go-worker-pool` | Go | A generic worker pool with three layered faults: a hang, result order and shutdown on error |
| `go-diff-lines` | Go | Unified diff hunk headers; the obvious start fix breaks the empty-file form |
| `sh-log-rotate` | Shell | Log rotation count and unsafe quoting of names with spaces and leading dashes |
| `sh-makefile-deps` | Shell | A makefile missing a shared header dependency and an always-run phony target |
| `sh-awk-report` | Shell | A sales report from CSV: columns by header name, quoted fields and blank lines |
| `sh-env-render` | Shell | A template renderer with reversed precedence, `${NAME:-fallback}` and chained references |
| `rust-tokenizer` | Rust | Unary minus, and decimals such as `.5` and `2.`, in an expression tokenizer |
| `rust-bitset` | Rust | Set operations and iteration at 64-bit word boundaries |
| `rust-path-normalize` | Rust | Path cleaning: `..` at the root, repeated slashes, trailing dots and relative parents |
| `rust-matrix-refactor` | Rust | Row and column helpers merged into one, and transpose for non-square matrices |
| `rust-expr-eval` | Rust | A calculator with wrong associativity, panics on division by zero and integer overflow |
| `java-stack-calculator` | Java | An RPN calculator that crashes on division by zero and reads negative numbers as operators |
| `java-bank-transfer` | Java | Overdrafts from a balance check made after debiting, and cents lost in currency conversion |
| `java-text-wrap` | Java | Word wrapping that splits words needlessly, drops long words and keeps repeated spaces |
| `java-event-replay` | Java | Rebuilding a balance from a snapshot; the obvious timestamp fix breaks events sharing a millisecond |

The tasks were proven on these toolchains:

| Toolchain | Version |
|---|---|
| Node | v22.21.1 |
| Python | 3.13.2 |
| Go | 1.23.4 |
| cargo | 1.98.1 |
| OpenJDK | 17.0.15 |
| POSIX `sh` | macOS 15 `/bin/sh` (bash 3.2.57 in POSIX mode) |
| `awk` | BSD awk 20200816 |
| GNU Make | 3.81 |

Each task directory holds `task.json` and these paths:

| Path | What it is |
|---|---|
| `repo/` | The project the agent works in, with failing tests |
| `solution/` | Files that, copied over `repo/`, make `verify` pass; never shown to the agent |
| `tests/` | Optional hidden tests (the directory `tests` names); copied into the workspace only after the agent stops |

Fields of `task.json`:

| Field | Required | Meaning | Default |
|---|---|---|---|
| `title` | no | Short name for people | none |
| `kind` | no | What sort of work it is, for example `fix` or `implement` | none |
| `prompt` | yes | What the agent is asked to do | none |
| `verify` | yes | Command whose exit 0 is a pass | none |
| `protected` | no | Paths restored from `repo/` before verifying; edits are detected | none |
| `timeoutMs` | no | Agent time limit in milliseconds | 600000 |
| `tests` | no | Directory in the task whose entries are copied into the workspace just before verify | none |
| `setup` | no | Shell command run in the workspace before the agent starts; a failure fails the run | none |
| `verifyTimeoutMs` | no | Verify time limit in milliseconds | 180000 |
| `tier` | no | Difficulty: 1 one file, 2 several files, 3 long or tricky | none |
| `split` | no | `dev` or `heldout`; `rune bench --split` filters on it | `dev` |
| `language` | no | Main language of the task | none |
| `tags` | no | Free-form labels | none |

Writing a task:

- Nothing may need the network at run time: standard library only, or vendored single files.
- repo/ may contain some tests; the grading tests live in the hidden tests directory and are
  copied in only after the agent stops.
- protected lists every path the agent must not edit; edits are detected and undone.
- solution/ holds the files that, copied over repo/, make verify pass.
- Tests are deterministic and finish in under 20 seconds.
- Write the prompt the way a user would ask; do not name the grading command.
- A v1 task (prompt, verify, protected, timeoutMs) is a valid v2 task.

The JavaScript tasks run `node --test`, which finds `*.test.js` files on every
supported Node release. Passing a directory such as `node --test test/` is read
as a file on Node 22 and fails before any test runs.

## Cross-harness runs

`python3 bench/kit/run.py` runs any set of harnesses over these tasks behind a metering proxy and
judges them with hidden tests. See `bench/kit/README.md` for the commands and the result format.
