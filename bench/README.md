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
