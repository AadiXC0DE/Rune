# Changelog

Notable changes, newest first. Each entry describes what a user can observe.

## 0.1.1

### Added

- `rune connect` with no argument lists the providers, takes a choice, and asks
  for what that provider needs, so a first connection needs nothing looked up.
- OpenCode is available in both of its tiers, as `opencode` and `opencode-go`.
  They share a key and differ in endpoint; the subscription requires the header
  that names its conversation.

### Fixed

- A provider connected by name could not be read back, so a self-hosted endpoint
  appeared to connect and then had no effect.
- Connecting a second provider reused the first one's endpoint, sending requests
  to a host the user never named.
- The published download is compressed. It was named `.tar.gz` and contained an
  uncompressed tar, which `tar` opened by sniffing the format while other
  readers refused it.
- Session identifiers are generated through the same randomness crate the TLS
  stack uses, removing the second copy from the binary.

## Unreleased

### Added

- `rune models` lists the models the configured endpoint serves, read from the
  endpoint itself. A model released after this build appears, which a compiled
  table cannot offer. `rune models --offline` reports the configured model alone.
- Interactive session with a prompt, slash commands, and a status line showing
  the model, the permission mode, and the context left.

### Fixed

- A turn that called several tools at once sent only the first result back, so
  the conversation had two calls and one answer. The endpoint refuses that, and
  since it says so in an empty body the failure was reported as a bare status
  with nothing to act on. Every tool result now reaches the endpoint, the
  provider's own explanation appears in the error, and a rule written as `*`
  matches a tool that takes a glob, which it previously did not.
- An interactive session ran with no permission rules, so every action fell to
  the mode default. In automatic mode that refused every tool call rather than
  allowing the reads and in-workspace edits a fresh install is meant to permit.

### Added

- `rune ask` for one request, with a JSON result object.
- Session persistence: every turn is written to a log, sessions can be listed,
  inspected, resumed, paged, and shown as a branch tree.
- Recovery of a damaged session into a new one, and a report of the log schema
  a session was written with.
- Permission rules with a specificity comparison, an explanation for a single
  action, and automatic review of unresolved actions in auto mode.
- Sandboxed command execution on macOS and Linux, reported by `rune doctor`.
- Workspace trust: a repository can declare servers and directories, and none of
  them apply until approved, with approval recorded against the canonical path.
- Tools: file read, glob, grep, write, edit, shell with supervised sessions,
  web fetch, web search, image analysis, skill loading, skill search, skill
  install, structured questions, and subagent delegation.
- User-defined slash commands loaded from the workspace and the configuration
  directory, expanded into the composer rather than submitted.
- Prompt history with recall scoped by workspace or session, and a session title
  derived from the opening prompt.
- Skills with a name and a description advertised in the prompt, whose full
  instructions load only when asked for.
- A theme, a status footer, a scrolling transcript screen, and a composer that
  edits by character and by word.
- `rune upgrade` and `rune uninstall`, which verify an artifact against a
  checksum before replacing anything.
- Offline mode, which refuses every outbound request.
- A generated command reference at `COMMANDS.md`.

### Changed

- An error message no longer repeats its own remedy; the hint is printed on its
  own line.
- A limit switched off with `off` now applies from a configuration file.
- A session listing is scoped to the workspace it ran in unless `--all` is given.

### Fixed

- Offline mode was parsed and never enforced, so a run that asked to stay
  offline still made requests.
- The system prompt override was checked for but never read, so writing one had
  no effect.
- A prompt whose skill catalog was cut was never told anything was omitted.
- A sandboxed command could not write in its own workspace when no working
  directory was given.
- A tool result was retained across a turn without a bound on the total.
