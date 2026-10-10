# Changelog

Notable changes, newest first. Each entry describes what a user can observe.

## 0.1.16

### Added

- Interactive resume replays saved user and assistant exchanges before the first
  input, preserving them in terminal scrollback.
- Ctrl-O opens a scrollable full transcript snapshot, including recorded tool
  calls and result bodies. Escape or Ctrl-O returns to the same draft and caret.
- Fenced code preserves indentation and spaces while streaming and in the
  finished transcript. Long lines wrap with an indented `↪` continuation marker;
  code tabs use eight-column stops and wrapping adapts to terminal resizing.
- `rune sandbox explain` previews a shell command without executing it, showing
  the backend, writable roots, network policy, protected paths and their sources.
  It reports refusal or unsandboxed fallback when enforcement is unavailable.

- CI replays the actual Rune process in Unix PTYs and compares captured grids
  for long drafts, 12-column output, short menus, and draft resizing. The gate
  checks caret positions and that narrow output survives in scrollback.
- Models can read retained tool output with `read_tool_result`, paging by byte
  offset until EOF. JSON page metadata and escaping fit within the tool cap and
  the turn's remaining result budget; handles stay scoped to the live conversation.
- A running turn can be steered from the keyboard. Enter sends what was typed to
  the turn at its next step, Escape twice cancels it, and Control-C clears a
  typed line before cancelling. A cancelled or failed turn ends that exchange
  and returns to the prompt instead of ending the session.
- The landing page runs the harness itself, compiled to WebAssembly, against
  files held in the page: with a scripted model, a model running in the browser,
  or an endpoint the visitor names.
- A known provider is reached at its published endpoint when no `base_url` is
  configured, so `RUNE_PROVIDER=opencode-go` is enough on its own.

### Security

- `Endpoint` debug formatting redacts the provider credential.
- `rune upgrade` creates a unique staging file exclusively, so a preexisting
  staging symlink cannot overwrite another file or become the installed binary.
- Commands receive an allowlisted environment and no longer inherit the
  provider credential. They run with CPU, process, and file size ceilings.
- A sandboxed command can no longer read or write the credential stores under
  the home directory, including the state directory that holds Rune's own
  credential and transcripts, or write a repository's `.git/config` and hooks.
- Path containment is checked where links lead, and `write_file` and
  `edit_file` are held to the same roots as reads.
- A shell rule is judged against every command a line runs, so `ls; rm -rf .git`
  no longer matches an allow for `ls*`.
- Native web fetch checks every redirect hop and its resolved addresses before
  connecting, then connects only to those vetted addresses. Private destinations
  still require `allow_private`. Proxy requests are refused unless `NO_PROXY`
  permits a direct connection, because proxy DNS can bypass address vetting.
- Web fetch refuses abbreviated private IPv4 addresses such as `127.1`,
  `127.0.1`, and `10.1` before calling the backend, unless `allow_private` is true.
- Model output is stripped of terminal control sequences while it streams.

### Fixed

- `grep_files` searches source text before adding long-line truncation annotations,
  so the annotation cannot create a match for text absent from the source.
- Unknown `--permission-mode` values fail with `invalid_field`, list the
  accepted modes, and exit 1 instead of silently keeping the configured mode.
- Unknown `--effort` values fail with `invalid_field`, list the accepted
  efforts, and exit 1 instead of silently keeping the configured effort.
- `rune workspace clear --json` reports an empty directory list, matching the
  saved configuration and subsequent `rune workspace list --json` output.
- `cargo xtask gate` checks formatting and Clippy with warnings denied before
  running the size and startup budgets and workspace tests.
- Workspace edits and provider selections honor `RUNE_CONFIG`, preserving
  unrelated settings in that file and leaving the default config untouched.
- `rune ask --json` emits one failure object and exits 1 when the prompt is
  empty or contains only whitespace, including when read from standard input.
- `rune permissions --explain shell:pwd --json` reports one structured decision
  with its outcome, deciding rule and source layer. Positional explanations also
  honor `--json`.
- `rune tree last` shows the branch structure of the most recent saved session
  in the current workspace, including JSON output.
- `rune session last` inspects the most recent saved session in the current
  workspace, including JSON output, instead of parsing `last` as an exact ID.
- `rune auth status` reports the same connection status as `rune auth`, including
  JSON output, and exits successfully.
- `rune usage` honors `--period` when selecting the reporting interval, while
  preserving positional periods and the default 24-hour report.
- CLI sessions automatically compact earlier conversation before sending a model request
  when its estimate reaches `compaction_trigger_percent`, including after tool results. A failed
  summary preserves history and stops the pending request.
- Changing the provider with `--provider` selects its configured model and
  capacity and discards the previous provider's endpoint and credential variable.
  Changing the model with `--model` discards the previous model's capacity;
  overrides naming the same provider or model preserve their settings.
- Changing the model with `RUNE_MODEL` discards the previous model's declared
  context window and reports the default capacity and source. An override naming
  the same model preserves its declared window.
- Resumed sessions initialize the context meter from saved usage or history
  bytes and label the estimate with its source until live usage arrives.
- README and budget help distinguish the audited 4.50 MiB stripped release
  build for `x86_64-unknown-linux-gnu` from the enforced 8 MiB binary size ceiling.
- Concurrent credential updates and removals hold an OS lock across the full
  update, preserving other provider profiles when multiple processes connect.
- Credential updates and removals replace the complete file atomically, so
  interrupting a writer preserves the previous credentials until replacement.
- Long Unicode tool descriptions truncate at a character boundary, so registering
  a custom tool preserves valid UTF-8 without panicking.
- Completed shell sessions keep their final output until `interact` or `stop`
  reads it, even when another command starts. Completed sessions release their
  running slot, and reading their final output removes the session.
- The shared agent turn loop retains oversized tool output in the live
  conversation and returns a bounded preview with a readable retained handle.
  Both output limits include preview metadata; retained bytes can be retrieved
  through the store API across turns and compaction.
- `rune ask` saves completed exchanges with their prompt, reply, and reported
  usage. JSON reports a resolvable `session_id`; `--no-save` creates no session
  and keeps the identifier empty.
- Failed terminal turns save their visible partial answer and failure cause.
  Resuming retains the answer, and `/tree` shows the failure boundary and code once.
- Cancelled terminal turns save their visible partial answer and a cancellation
  boundary. Resuming retains the answer, and `/tree` shows the boundary once.
- Responses requests now send explicit reasoning effort as `reasoning.effort`.
  Auto keeps provider defaults.
- Chat Completions requests now send explicit reasoning effort as
  `reasoning_effort` to compatible endpoints. Auto keeps provider defaults.
- Anthropic requests now serialize reasoning effort for models that support
  manual extended thinking. Auto keeps provider defaults, None disables thinking,
  and explicit output ceilings bound the thinking budget without being raised.
- Web fetch and search are disabled by default, matching the documented opt-in.
  Set `web_tools = true` or `RUNE_WEB_TOOLS=true` to enable them; offline mode
  still refuses outbound requests.
- Offline mode disables sandboxed shell networking even when external access
  is granted.
- The user configuration accepts `offline = true` and enforces offline mode
  without discarding the other settings in the file. `RUNE_OFFLINE` still takes
  precedence over the file setting.
- Cancelling a native provider stream releases its socket and reader thread,
  including when the response body stays silent.
- Native provider streams enforce `provider_head_timeout_ms` when response
  headers arrive but the body stays silent, without waiting for the connection
  to close.
- Native provider requests enforce `provider_request_timeout_ms` across the
  response head and streamed body, returning a timeout even while output keeps
  arriving.
- Transcript text wraps to fit narrow terminals, preserving long words and
  their complete endings when finished answers enter scrollback.
- Completion and model menus shrink to fit the terminal height while keeping
  the highlighted choice visible. Closing a menu clears its rows without
  moving the prompt's caret off its row.
- `rune upgrade` verifies a release tar.gz before extracting its single Rune
  executable, so upgrading from a staged release installs a runnable binary.
- Structured questions in terminal sessions now collect choices with the arrow
  keys and Enter, then send the selected answers to the model. Escape or
  Control-C cancels the waiting turn; piped sessions still report input unavailable.
- `rune ask` fails with `unsupported_tool_call` when the model requests tools
  that its text-only path cannot execute. JSON lists those calls with status
  `error` instead of reporting them as successful.
- File permission rules also match normalized absolute and workspace-relative
  paths, so denying `.env` also denies `./.env` and its absolute workspace path.
- Long drafts scroll horizontally to keep the caret and the text being edited
  visible, both at the prompt and while a turn is running.
- Interactive permission requests display their complete scope and offer Run
  once or Deny. Escape or Control-C cancels the waiting turn without executing
  the request.
- `grep_files` rejects context windows above 1,000 lines with a tool validation
  error, keeping the session usable instead of aborting on an oversized allocation.
- Connecting a provider creates Rune's state directory with mode 0700, so a
  fresh install can start a session with umask 022 or 002.
- Anthropic sessions send the key in the header Anthropic reads.
- Streamed tool calls on the Responses API, reasoning replay, retries of
  errors reported mid-stream, `Retry-After`, unrecognized stop reasons, and
  tool calls ended by a `stop` finish all behave correctly.
- Token usage is summed across the requests of a turn rather than taking the
  largest.
- A paste arrives as text, the terminal is restored after a crash, and the live
  region survives a resize and the bottom row.
- The macOS sandbox lets a command write its temporary directory, so compilers
  and test runners work inside it.
- `/compact` finds a cut when recent prompts took several tool steps.
- A call to a tool that does not exist, or one no rule allows, is answered with
  the reason instead of retried blindly.
- Global flags after the command apply, and `rune connect` hides the key.

## 0.1.15

### Fixed

- The completions for a slash command, and the model picker, are drawn under the
  line being typed. Both were drawn above it, between the input and the answer
  arriving, which is the opposite of where a list opened by typing belongs and of
  what every comparable harness does. An answer growing upward and a list opening
  downward now have their own places, so neither moves the other.
- The list shows six rows at a time with a row saying how far through it is,
  rather than every match: a bare slash matches every command, and drawing all of
  them swallowed the screen.
- Every command in the list is reachable. The arrows were bounded by the number
  of drawn rows rather than the number of matches, so the position row counted as
  a command and everything past the first screenful could not be selected.

## 0.1.14

### Changed

- Web tools received working outbound clients and user-level permission rules,
  so explicitly enabling them could overrule the built-in denial. This release
  enabled web tools by default; 0.1.16 restores the documented opt-in via
  `web_tools = true`. `offline = true` still refuses everything, including the
  model.
- The site's canonical and Open Graph URLs name `rune.heyaadi.com`.

## 0.1.13

### Added

- A slash command shows what can be typed next. Typing `/` lists every command
  with the line describing it, typing narrows the list, the arrows move the
  highlight, and tab or enter accepts it. The list comes from the same table the
  help text and the dispatcher use, so a command that is offered is one that
  works.

### Fixed

- The cursor no longer jumps upward. A frame that changed nothing placed the
  cursor by walking to the top of the region first, which only works while the
  caret is on the first row; anywhere else it landed on a status row, so pressing
  down at an empty prompt moved the cursor up.
- An answer is drawn under the question it answers, above the status block, with
  the line being typed pinned underneath. It previously grew downward from the
  input, and because the region is anchored at the bottom of the screen every
  extra row pushed the input upward: the transcript read as question, status bar,
  empty input, then answer, and the input crept up as the reply arrived.

## 0.1.12

### Fixed

- The context window is resolved before the first frame is drawn. The session
  announced itself and corrected the figure once the endpoint answered, so the
  status line showed the compiled default for as long as the lookup took and
  changed under the reader. A number that corrects itself is worse than one
  that arrives late, because while it is on screen it is indistinguishable from
  a right one.

## 0.1.11

### Fixed

- `rune` with no argument starts. Connecting a provider writes the provider and
  the endpoint and no model, and a session then required one before it would
  open a terminal, so the only way in was to pass `--model`. A session now asks
  for the model before its first turn, which is what the picker is for.
- Every model reported a hundred and twenty-eight thousand tokens. The opencode
  endpoint returns an identifier and no capacity at all, and the reader that
  parsed the listing kept only the identifier while discarding any capacity
  beside it. The capacity now comes from the published models.dev catalog, which
  is keyed by the same model identifiers the endpoint lists: `grok-4.7` reports
  five hundred thousand, `kimi-k3` a million and forty-eight thousand, `glm-5.3`
  a million. A figure the endpoint states is kept, because the endpoint
  describing its own model is the better authority.
- The catalog is cached under the state root, so a session pays the download
  once, and an offline run still knows the window of the model it is talking to.

## 0.1.10

### Fixed

- The interface no longer writes over itself. When the live region grew taller
  than the space below the cursor, the rows that were gone were erased a row at
  a time with a newline, and a newline at the bottom of the screen scrolls: the
  rows just written were pushed off the top. Nothing capped the region to the
  terminal either, so a long answer could be taller than the screen. The status
  block was drawn a second time from the previous frame and the answer was
  written over in the middle. Erasing is now one erase-to-end-of-screen, which
  never moves the cursor, and the renderer is told the terminal height so the
  region it draws is one it can also repaint.
- The context meter reads the model's real window. The session never asked the
  endpoint what the selected model serves, and the reader that parses an
  endpoint listing kept only each model's identifier while discarding the
  capacity beside it. A model serving a million tokens was budgeted against a
  compiled default of 128k and reported as nearly full before anything had been
  said. The listing now keeps the capacity under any of its usual names, and a
  window declared in the configuration still wins over the endpoint's figure.

### Added

- `rune` with no argument starts a session on a fresh install. There is no
  provider yet, so the connection flow runs first and the session starts on what
  it chose. A piped or machine invocation is still not asked anything.
- `/models` is accepted as a spelling of `/model`. `/model` with no argument
  opens the picker, and choosing a model adopts that model's own window.

## 0.1.9

### Added

- Choose a model from inside a session with `/model`. It lists what the endpoint
  serves, narrows as you type, and is driven with the up and down arrows. The
  choice applies to the next turn and is written to the configuration, so the
  next session starts on it. Naming one directly, `/model <id>`, still works.
- `/status` reports the model, provider, endpoint, permission mode, effort,
  session, workspace, and how much of the context window is in use. `/cost`
  reports what this session has spent.
- `/compact` summarizes older turns to free the context window, `/undo` puts
  back the files the session changed, `/copy` puts the last reply on the
  clipboard, `/new` starts a fresh conversation, `/rename` titles the session,
  and `/tree` shows the turns it holds.
- The up and down arrows recall earlier prompts and move a selection. Neither key
  did anything before: the line editor had a history walk that nothing called.

### Fixed

- Command output is drawn where it belongs. Every command wrote straight to the
  terminal, which put its output at the cursor position inside the region the
  renderer repaints, so the next frame drew over it and the screen showed a
  mixture of the two. Output is now committed through the renderer like any
  other finished text.

## 0.1.8

### Added

- The web tools work. Both were registered with a backend that refused every
  request and were then denied by a built-in rule that nothing could override,
  so `web_search` and `web_fetch` failed whatever the user asked for. They now
  reach the network through the one module that is allowed to, and
  `web_tools = true` enables them.

### Fixed

- A finished tool call shows one line in the conversation instead of its whole
  result. Reading a four-hundred-line file used to print all four hundred lines
  into the chat, burying the answer under the material it was drawn from. The
  result still reaches the model and the session log in full.

## 0.1.7

### Added

- A model can declare its context window in the configuration, as either a bare
  identifier or a table naming the window. `rune config` reports which is in
  force and where it came from. A model with a larger window used to be budgeted
  against a compiled default of 128k, so a million-token model showed as nearly
  a fifth full before anything was said.

### Fixed

- The context meter no longer overstates what has been used. It summed the input
  count of every turn, but each turn resends the whole conversation, so the same
  history was counted once per turn and a session appeared to fill its window
  several times over. It reports the size of the conversation now.
- Streaming no longer stutters. Every delta re-wrapped the entire answer and
  rewrote every row of the live region, which is quadratic in the length of the
  response: a three-thousand character answer wrote six hundred and eighty
  thousand bytes to the terminal. Only what changed is written, which brings the
  same answer to sixty-four thousand bytes, and wrapping is done once per line
  rather than once per delta.

## 0.1.6

### Added

- Reasoning streams in its own lane, indented and in a secondary colour, above
  the answer. It used to disappear the moment a turn ended, because it was shown
  while arriving and then dropped rather than kept.

### Fixed

- The cursor no longer overlaps the status line. After a prompt was submitted the
  frame carried no input row, so the cursor fell back to the last row of the
  region, which is a status row, and stayed there until streaming happened to
  supply a row of its own. Every frame now draws the input row.

## 0.1.5

### Added

- Responses stream. Text appears as the model produces it rather than after the
  whole answer has arrived, so a long reply starts showing immediately.

### Fixed

- The status line sits above the input, where a reader looks for it and where it
  does not move while an answer arrives. The answer grows downward from the
  input, so a long reply never pushes the line being typed off the screen.
- A protocol client no longer receives the answer twice. The server sent the
  finished text as one chunk because there was nothing to stream; with the
  deltas arriving as they are produced, sending the whole answer again
  duplicated what the client had already seen.

## 0.1.4

### Added

- Text wraps between words instead of at the column, so model output no longer
  breaks a word in half. A single word longer than the line is still split,
  because the alternative is overflowing the terminal.

## 0.1.3

### Fixed

- The interactive screen no longer garbles. Two components were writing to the
  terminal: finished text was printed in the flow while the live region was
  drawn at absolute rows, so the printed text moved the rows the region was
  placed at and characters were overwritten. There is now one writer.
- The width comes from the terminal rather than a fixed 100 columns, so a line
  no longer wraps onto a row the renderer did not count. The transcript wraps at
  the width the reader actually has instead of mid-word at 80.
- The cursor is visible where it is typing. The line being edited is drawn by the
  program with the cursor placed inside it, rather than relying on the
  terminal's echo, and the line is committed to the screen when it is submitted
  instead of being erased with the region it was typed in.

### Added

- The prompt is a full line editor: cursor movement by character and by word,
  home and end, delete by character and by word, kill to either end, and paste.

## 0.1.2

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
