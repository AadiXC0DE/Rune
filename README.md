# Rune

A native coding agent harness: one small binary for the terminal, for scripts,
and for embedding in other systems.

Rune is model and provider agnostic. It has no hosted service, no background
daemon, and no telemetry, and it requires no account. You connect the endpoint
you want to use and it works with that.

## Status

Usable for a real session against the endpoint you connect. The command surface,
configuration, agent loop, tools, permissions, sessions, and terminal interface
are in place. `COMMANDS.md` is the generated command reference, and `rune doctor`
reports what your machine supports.

## Building

Requires Rust 1.98 or newer.

```sh
cargo build --release
./target/release/rune doctor
```

## Design

- **Small and fast.** Startup and binary size are budgets enforced in CI, not
  aspirations. The argument and configuration paths never load the agent runtime.
- **Shell-like output.** The interactive session renders inline and preserves
  terminal scrollback rather than taking over the screen.
- **A small prompt.** The system prompt has a size ceiling and is inspectable.
- **Permission first.** Every sensitive action passes a policy gate that can
  explain exactly which rule decided it.
- **Local.** Sessions, usage, and credentials stay on the machine.

## Commands

```sh
rune                    # start an interactive session
rune ask "..."          # run one request
rune connect            # connect a model provider
rune doctor             # check the local setup
rune status --json      # show resolved configuration and state
rune limits --json      # list every limit and its source
rune config --explain   # show where each setting came from
rune acp                # serve the Agent Client Protocol
```

Run `rune help` for the full list.

## Configuration

Configuration resolves from five layers, highest wins: command-line flags,
`RUNE_*` environment variables, the project file `.rune.toml`, the user file at
`$XDG_CONFIG_HOME/rune/config.toml`, then built-in defaults.

Only repository-safe keys are accepted in a project file. A user setting placed
there is ignored and reported rather than applied, because a repository can be
changed by anyone who can open a pull request.

## License

Apache-2.0.
