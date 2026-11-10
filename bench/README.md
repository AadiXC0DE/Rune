# Benchmark tasks

Tasks for `rune bench`, in the `task.json` and `repo/` format other harnesses'
suites also use, so the same tasks can measure each of them.

```sh
rune bench bench/tasks                 # every task, once
rune bench bench/tasks --trials 3      # three runs of each
python3 bench/kit/run.py validate --tasks bench/tasks   # prove each task is solvable and unsolved as shipped
```

| Task | Language | What it takes |
|---|---|---|
| `js-lru-cache` | JavaScript | Implement a least-recently-used cache from its tests, including recency on reads and falsy values |
| `js-retry-backoff` | JavaScript | Two bugs across two modules: retry counting and an uncapped, off-by-one backoff |
| `py-merge-intervals` | Python | Touching and unsorted intervals, input left unmodified, invalid intervals refused |
| `py-ini-parser` | Python | Values containing `=`, inline comments outside quotes, keys before any section |
| `go-wordfreq` | Go | Unicode-aware word splitting, case folding and alphabetical ties |
| `rust-semver` | Rust | Pre-release precedence from Semantic Versioning 2.0.0, strict parsing |

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
