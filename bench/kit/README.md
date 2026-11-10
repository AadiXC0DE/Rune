# Benchmark kit

A small, standard-library-only Python kit for comparing coding-agent harnesses on the same tasks.
Each run gets a fresh copy of the task repository, an empty home directory and its own metering
proxy, so tokens and cost come from the requests that actually left the machine. Hidden tests are
copied in only after the harness has stopped.

## Scripts

```sh
python3 bench/kit/run.py --tasks bench/tasks --harnesses rune,oracle --model ID \
  --upstream URL --key-env VAR [--upstream-anthropic URL] [--trials N] [--jobs N] \
  [--max-cost USD] [--resume] [--dry-run] [--out DIR]
python3 bench/kit/run.py validate --tasks bench/tasks
python3 bench/kit/run.py check --harnesses rune,fx,sutra
python3 bench/kit/report.py RESULTS... [--baseline rune] [--out DIR]
python3 bench/kit/meter.py --upstream URL --log FILE [...]
python3 -m unittest discover -s bench/kit -p 'test_*.py'
```

`run.py` writes `results.jsonl` (one row per run), `run.json` and per-run logs. `report.py` turns
result files into `report.md`, `report.json` and `results.csv`. `check` prints one line per
harness: found or `missing`, its version, and whether the flags its adapter relies on are still in
its help text. It exits 1 when any harness is missing or a flag has gone.

## Comparing Rune variants

```sh
python3 bench/kit/ab.py --variants control,CANDIDATE [--tasks bench/tasks] [--split dev] \
  --trials N --model ID --upstream URL --key-env VAR --out DIR \
  [--only ids] [--max-cost USD] [--jobs N] [--rune-bin PATH] [--resume] [--dry-run] [--log PATH]
```

The first variant is the control. Each variant is a file `bench/kit/variants/<name>.json`:

```json
{
  "description": "One line saying what differs from the control.",
  "env": {"RUNE_EXPERIMENT": "run_note=off", "RUNE_TOOL_TEXT": "{home}/tool-text.json"},
  "flags": [],
  "files": {"tool-text.json": "{\"shell\": {\"description\": \"Run a command.\"}}"}
}
```

`env` overrides the Rune adapter's variables, `flags` are extra global flags placed before `ask`,
and `files` are written under the run's home (the runner sets `XDG_CONFIG_HOME` to
`<home>/.config`, so `.config/rune/SYSTEM.md` replaces the system prompt). `{home}` in an `env`
value or a `files` key is the run's home. Unknown keys are refused. Each variant runs as its own
harness row, `rune@<name>`, interleaved with the others by trial and task.

After the run `ab.py` writes the usual `report.md` and `report.json` and prints, and appends to
`decision.md`, one block per candidate with pass@1, the paired difference and its 95% interval,
the exact McNemar p, the median input tokens and the cost per solved task. A candidate is adopted
when the lower end of the interval is above -3 points and it saves at least 5% of median input
tokens or of cost per solved task, or when the lower end is above 0. Otherwise the control is
kept. `--log PATH` also appends one table row per candidate to that file. At least two tasks are
needed; a variant whose every run fails before its first request (for example an unknown
experiment key) stops the comparison with exit 3.

Experiment keys (`RUNE_EXPERIMENT`, comma-separated `key=value`, values `on` or `off`):

| Key | Default | Effect |
| --- | --- | --- |
| `run_note` | `on` | The note on non-interactive runs in the first message of `ask`. |
| `question_nudge` | `on` | One note to an `ask` run whose reply ends on a question: decide and continue. |
| `map` | unset | Forces the repository map on or off. |

## Adapters

An adapter is a module in `bench/kit/adapters/` (or any directory given with `--adapter-path`).
Module-level names:

- `NAME`, `BIN`, `VERSION_CMD` (arguments that print the version), `HELP_FLAGS` (flags that must
  appear in the help text), `VERIFIED`.
- `DIALECTS`: the upstream APIs the harness speaks, from `chat`, `responses`, `anthropic`
  (default `chat`). A harness whose API is not served is skipped with one line.
- `HELP_CMD` (default `["--help"]`) and `HELP_EXTRA_CMDS` (more help commands whose output is
  joined to it).
- `find_bin()` (optional): returns `(argv_prefix, note)` or `None`. It replaces the lookup of
  `BIN` on `PATH`. A non-empty `note` is appended to the harness version as ` (<note>)`.
- `prepare(ctx)`: returns `{"argv": [...], "env": {...}, "files": {...}}`. `files` maps paths
  relative to the temporary home to their text; the runner writes them. `ctx` holds `bin`, `cmd`
  (the argv prefix, `[bin]` for simple adapters), `home`, `workspace`, `task_dir`, `proxy_url`,
  `model`, `key`, `prompt` and `timeout_s`.

Every adapter for another product is `VERIFIED = False` until a clean real run has been made with
it. The unit tests check only the argv, environment and files each adapter produces.

## Installing other harnesses

The kit never installs anything. Installing another harness is your decision, and its code runs
with your permissions outside any sandbox. These are the commands the adapters expect:

| Harness | Install |
| --- | --- |
| fx | `curl -fsSL https://fx.sh/setup.sh \| bash` |
| Sutra | clone its repository, run `npm install` in the clone, set `BENCH_SUTRA_REPO` to the clone |
| opencode | `npm i -g opencode-ai` |
| pi | `npm install -g --ignore-scripts @earendil-works/pi-coding-agent` |
| Claude Code | `npm install -g @anthropic-ai/claude-code` |
| Codex | `npm install -g @openai/codex` |
| mini-swe-agent | `uv tool install mini-swe-agent` |
| Goose | the `download_cli.sh` script from `https://github.com/block/goose/releases/download/<version>/` |

## Fairness notes

These are copied into the header of every report.

- Every harness runs with all permissions granted and no approval prompts.
- fx runs with --full-access: its default mode sends unresolved actions to a reviewer model,
  which stalls on DeepSeek and would add metered requests to its row.
- No harness is given the verify command or the hidden tests.
- Codex needs the Responses API and is skipped against Chat Completions-only upstreams.
- Tokens and cost come from the meter, never from a harness's own report.
