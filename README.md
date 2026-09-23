<div align="center">

# Rune

**A tiny, native coding agent harness.**

About 3 MiB and 2 ms to start, written in Rust, with a build that fails if either
grows. One binary for the terminal, for scripts, and for embedding in other
systems. Every limit, rule, and setting names the source that set it.

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
by `rune auth`. The providers that ship in the table are:

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

Sessions are written as they run, so an interrupted session resumes. Prompts are
remembered and recallable with `/history`. Custom slash commands live in
`.rune/commands/*.md` in a repository, so a team can ship a workflow with the
code.

Run `rune help` for everything, or see [COMMANDS.md](COMMANDS.md) for the full
reference.

## Design

- **Small and fast.** Binary size and startup are budgets enforced in CI, not
  aspirations. The argument and configuration paths never load the agent runtime.
- **Shell-like output.** The session renders inline and preserves terminal
  scrollback rather than taking over the screen.
- **Permission first.** Every sensitive action passes a policy gate, and
  `rune permissions` explains exactly which rule decided it.
- **Sandboxed execution.** Commands run under the platform sandbox where one
  exists. `rune doctor` reports what your host can enforce.
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

## License

Apache-2.0. See [LICENSE](LICENSE).
