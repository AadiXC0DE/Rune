<div align="center">

# Rune

**A tiny, native coding agent harness.**

The audited stripped release build measured 4.50 MiB for
`x86_64-unknown-linux-gnu` (GNU/Linux). The build enforces an 8 MiB ceiling
and startup budgets. Written in Rust, with about 2 ms to start. One binary for
the terminal, for scripts, and for embedding in other systems. Every limit,
rule, and setting names the source that set it.

[![CI](https://github.com/AadiXC0DE/Rune/actions/workflows/ci.yml/badge.svg)](https://github.com/AadiXC0DE/Rune/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.98%2B-orange.svg)](rust-toolchain.toml)

</div>

Rune is model and provider agnostic. No hosted service, no background daemon, no
telemetry, and no account. You connect the endpoint you want to use and it works
with that.

## Install

Homebrew:

```sh
brew install aadixc0de/tap/rune
```

From source:

```sh
git clone https://github.com/AadiXC0DE/Rune
cd Rune
cargo build --release
./target/release/rune doctor
```

Then connect a provider. There is no default: a fresh install has none, and any
command that needs one says so and names how to connect it.

```sh
rune connect                # lists the providers and asks for what it needs
rune doctor                 # check what your machine supports
```

Running `connect` with no argument lists the providers and takes a choice, then
asks for the credential, so nothing has to be looked up first. Name one to skip
the list:

```sh
rune connect anthropic
rune connect opencode-go    # a subscription over the OpenAI-compatible route
rune connect chat_completions
```

It runs again for each provider you add, and what you have connected is reported
by `rune auth`. The models the endpoint serves are listed by `rune models`, which
asks the endpoint rather than a table compiled into the binary, so a model
released after this build still appears. Add `--offline` to report the configured
model without contacting anything.

The web tools are off until asked for. `rune doctor` reports what this machine
supports, and turning them on is one setting:

```toml
web_tools = true
```

They are refused by default because they send your queries to a search engine,
which is your call rather than a repository's, so a project file cannot enable
them. Set `offline = true` in the user configuration to refuse every outbound
request, including requests to the model.

A model larger than the default window declares its capacity, which is what
`rune config` reports and what the status line budgets against:

```toml
[models.opencode-go]
id = "grok-4.7"
context_window = 2000000
```

A bare identifier still works for a model whose window the endpoint reports.

The providers that ship in the table are:

| Provider | What it is |
|---|---|
| `anthropic` | Anthropic Messages API |
| `openai` | OpenAI Responses API |
| `opencode` | OpenCode Zen, pay-per-use |
| `opencode-go` | OpenCode Go, a subscription |
| `chat_completions` | Any OpenAI-compatible endpoint, with the URL you give |

Anything else is reachable by naming it and giving its endpoint, which is what
`chat_completions` does with a URL and what a self-hosted gateway needs. A
credential is read from the provider's own variable when it is already exported
(`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENCODE_API_KEY`), and pasted
otherwise.

## Use

```sh
rune                        # interactive session
rune ask "..."              # one request, prints the answer
rune review                 # review the pending changes in this repository
rune resume last            # continue the most recent session here
```

`rune ask` sends one text-only request. If the model requests tools, it exits 1
and names the unsupported calls. With `--json`, `error_code` is
`unsupported_tool_call`, each requested tool has status `error`, and
`final_output` is empty. Any accompanying text remains in `output`.
Completed exchanges are saved as sessions for the current workspace, and
`--json` reports their `session_id`. Pass `--no-save` to create no session and
return an empty ID.

Sessions are written as they run, so an interrupted session resumes. Prompts are
remembered and recallable with `/history`. Custom slash commands live in
`.rune/commands/*.md` in a repository, so a team can ship a workflow with the
code.

Run `rune help` for everything, or see [COMMANDS.md](COMMANDS.md) for the full
reference.

The shared agent turn loop spills tool output exceeding `max_tool_result_bytes`
or the remaining `max_turn_result_bytes` into the live conversation's memory.
The model receives a bounded preview with the retained byte count and handle,
including that metadata within both limits. If the limit cannot fit the handle,
the model receives an empty result while the full body is still retained.
Models can call `read_tool_result` with the handle, a byte `offset` (default 0),
and a positive `length` (default 65536). It returns JSON containing `text`,
`offset`, `next_offset`, `total_bytes`, and `eof`. Continue at `next_offset` until
`eof` to reconstruct the full bytes. Pages stay within 64 KiB, the configured
tool cap, and the turn's remaining result budget, including JSON escaping and
metadata. An insufficient budget returns a bounded tool error; start a new turn
to replenish the turn budget. Offsets must be UTF-8 character boundaries.
Embedding callers can also use `History::result_store()` and `Store::read`.
Retention lasts across live turns and compaction, but is not saved or restored
with transcripts.
The store holds at most 256 results and 64 MiB; a full store reports a retention
failure within the available output budget.

## Design

- **Small and fast.** Binary size and startup are budgets enforced in CI, not
  aspirations. The argument and configuration paths never load the agent runtime.
- **Shell-like output.** The session renders inline and preserves terminal
  scrollback. Ctrl-O opens a full transcript snapshot on the alternate screen,
  including recorded tool arguments and result bodies. Up/Down scroll by row,
  Page Up/Page Down by page, and Home/End jump to the beginning/end. Escape or
  Ctrl-O closes it with the draft and caret preserved. A running turn continues
  while the snapshot is open; reopen it to see newer output. Provider and tool
  result limits still apply to what is recorded.
- **Permission first.** Every sensitive action passes a policy gate, and
  `rune permissions` explains exactly which rule decided it.
  Interactive requests show the complete scope with Run once and Deny choices.
  Use Up/Down and Enter to answer; Escape, Control-C, or Control-D cancels the
  turn. Run once approves only the displayed call.
- **Sandboxed execution.** Commands run under the platform sandbox where one
  exists. `rune doctor` reports what your host can enforce.
  `rune sandbox explain -- 'git status'` previews the backend, writable roots,
  network decision and existing protected paths with sources, without running
  the command. The preview uses the current workspace and configured additional
  directories (`--add-dir` adds roots; `--no-additional-dirs` ignores saved roots).
  Network access requires an explicit shell context grant, previewed with
  `--external-access`; offline mode overrides it. `--json` emits the same report.
  Refused commands apply no protections; an unsandboxed fallback has unrestricted
  access. This explains sandbox policy, independently of command approval.
- **Offline mode.** `--offline` refuses every outbound request.
- **Local.** Sessions, usage, and credentials stay on the machine.

## Configuration

Five layers, highest first: command-line flags, `RUNE_*` environment variables,
the project file `.rune.toml`, the user file at `$XDG_CONFIG_HOME/rune/config.toml`,
then built-in defaults.

Only repository-safe keys are accepted in a project file. A user setting placed
there is ignored and reported rather than applied, because a repository can be
changed by anyone who can open a pull request.

```sh
rune config --explain       # where each setting came from
rune limits --json          # every limit and its source
rune prompt --show          # the exact instructions the model receives
```

## Embedding

`rune-acp` serves the Agent Client Protocol over stdin and stdout, so an editor
can drive a session. `rune-sdk` is the library an embedder links against: the
host supplies the credential, the tools, and the network path.

A Node binding lives in [`bindings/node`](bindings/node).

## Status

Early development, and usable for a real session. The command surface,
configuration, agent loop, tools, permissions, sessions, and terminal interface
are in place. See [CHANGELOG.md](CHANGELOG.md) for what has landed.

## Contributing

Issues and pull requests are welcome. Before opening a pull request:

```sh
cargo xtask check      # format, lint, and test
cargo xtask gate       # the above plus the size and startup budgets
```

On Unix, workspace tests also run the real PTY replay gate. It starts the built
`rune` binary against an isolated local provider and compares captured grids
and caret positions for long draft edits, 12-column output, short menus, and
draft resizing without keystrokes. The narrow case also checks the complete
answer in scrollback. Python 3 with its standard-library PTY modules is required;
CI checks this prerequisite on Linux and macOS. Run just this gate with:

```sh
cargo test -p rune --test terminal_replay
```

The reviewed grids live in `crates/rune/tests/fixtures/terminal_replay`.
Only temporary workspace paths and session identifiers are masked. If intended
terminal behavior changes, review the failing grid and caret differences before
updating a fixture. There is no automatic snapshot update mode.

## License

Apache-2.0. See [LICENSE](LICENSE).
