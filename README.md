<div align="center">

# Rune

**A tiny, native coding agent harness.**

The audited stripped release build measured 4.50 MiB for
`x86_64-unknown-linux-gnu` (GNU/Linux). The build enforces an 8 MiB ceiling
and startup work budgets based on baseline-subtracted minimum-of-31 measurements.
Written in Rust. One binary for the terminal, for scripts, and for embedding
in other systems. Every limit, rule, and setting names the source that set it.

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

From a release archive on Linux or macOS: download the `.tar.gz` for your
target and its `.sha256` sidecar from the same release. Set `archive` to its
absolute path and `prefix` to a writable installation directory, for example:

```sh
archive="$PWD/rune-<version>-<target>.tar.gz"
prefix="$HOME/.local"
```

Verify the checksum before extracting, then install the executable:

<!-- archive-install -->
```sh
(
    set -eu
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$(dirname "$archive")" && sha256sum -c "$(basename "$archive").sha256")
    else
        (cd "$(dirname "$archive")" && shasum -a 256 -c "$(basename "$archive").sha256")
    fi
    extracted=$(mktemp -d)
    trap 'rm -rf "$extracted"' EXIT
    tar -xzf "$archive" -C "$extracted"
    mkdir -p "$prefix/bin"
    install -m 755 "$extracted/rune" "$prefix/bin/rune"
    "$prefix/bin/rune" --version
)
```

Add `$prefix/bin` to your `PATH` to run `rune` by name. To uninstall this
binary while preserving configuration, credentials, and sessions:

<!-- archive-uninstall -->
```sh
"$prefix/bin/rune" uninstall --target "$prefix/bin/rune" --keep-state
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
asks for any required credential, so nothing has to be looked up first. Name one
to skip the list:

```sh
rune connect anthropic
rune connect gemini         # native Google Gemini API
rune connect opencode-go    # a subscription over the OpenAI-compatible route
rune connect chat_completions
```

It runs again for each provider you add, and what you have connected is reported
by `rune auth`. The models the endpoint serves are listed by `rune models`, which
asks the endpoint rather than a table compiled into the binary, so a model
released after this build still appears. Add `--offline` to report the configured
model without contacting anything.

Run `rune capabilities` before the first turn to inspect accepted Rune effort
selections, image content, tool streaming, fast mode, routing, and the context
window's source. Add `--json` for a machine-readable report. Configured context
windows take precedence; otherwise the check uses endpoint discovery and the
published catalog, falling back to the compiled default. `--offline` uses only
configuration and the existing catalog cache. The check creates no conversation
and sends no completion request.

Capabilities describe Rune's dialect implementation. Individual servers and
models may impose further restrictions. Gemini reports complete tool calls in
stream frames; the other dialects support incremental tool arguments. Use
`--require-images` or `--require-tool-streaming` to require image content or
incremental tool arguments. An unsupported requirement or selected control,
such as `rune capabilities --provider gemini --effort high`, exits 1 and reports
the rejection. Actual requests reject unsupported effort, fast mode, image
content, and routing selections before sending them.

For Ollama, `rune connect ollama` selects the local endpoint
`http://127.0.0.1:11434/v1` and saves anonymous loopback mode without a key.
To import another local endpoint, put `provider = "ollama"` and its `/v1`
`base_url` in the user config before connecting. Select a model with
`rune --model qwen3:latest` or set `models.ollama` in the config.

`rune models` discovers Ollama models through `/api/tags` and `/api/show`,
without a remote catalog request. Terminal and JSON listings include the
endpoint and each model's `context_source`: an explicit `parameters.num_ctx`
when present, otherwise the architecture's `model_info` context length.
An absent or invalid window stays unknown and names the compiled fallback.
Architecture metadata describes the model's window, not a measurement of the
server's running context allocation. See the [Ollama model details API](https://github.com/ollama/ollama/blob/main/docs/api.md#show-model-information).

For a local server that needs no credential, explicitly set `anonymous = true`
in the user config alongside its provider and loopback endpoint:

```toml
provider = "chat_completions"
base_url = "http://127.0.0.1:11434/v1"
anonymous = true
[models]
chat_completions = "qwen3:latest"
```

Then `rune connect chat_completions --json`, `rune models`, and `rune ask` skip
credential lookup and storage and send no authentication header. Anonymous mode
accepts only `127.0.0.1`, `localhost`, or `::1`; project config cannot enable it.
The Rust SDK exposes `Agent::new_anonymous` with an empty `api_key` for the same
explicit loopback mode. `Agent::new` continues to require a credential.

`rune mcp list --json` lists the workspace's `.mcp.json` declarations in name
order with transport, trust, enabled/required flags, startup status and errors.
It attempts each approved, enabled server independently, then closes it; no
provider or agent turn is needed. Pending and rejected servers stay blocked,
and `--offline` skips remote servers. Approve a workspace with
`rune projects approve`. Listing succeeds even when individual servers fail.
Headers, environment values, commands and URLs are omitted from the report,
and configured secrets are redacted from startup messages. Startup and operation
waits use `mcp_startup_timeout_ms` and `mcp_operation_timeout_ms`.

`rune mcp check [server] [--json]` checks one named server, or all declarations
when the name is omitted. It also reports the negotiated protocol revision,
discovered tool names, projection warnings, configured timeouts and whether a
timeout occurred. It initializes and lists tools without calling a tool or
requesting a model response, then closes each connection. The same trust and
offline rules apply. Check exits with code 1 if any selected server fails or
is blocked, disabled or skipped offline; an unknown name is an error. JSON adds
a `check` object to each server entry. An empty workspace reports no servers
and succeeds.

Interactive sessions connect approved, enabled stdio and HTTP servers from `.mcp.json`
once at startup. Their discovered schemas are advertised as
`mcp_<server>_<tool>`, and each call still goes through tool permission checks.
Full-access mode does not grant project startup trust. Optional startup failures
appear as notices; a required server failure prevents the session from starting.
Startup and calls use the configured MCP timeouts. Ctrl-C cancels a pending
stdio call, and exiting the session terminates its server process groups with a
bounded termination grace. HTTP servers are skipped with `--offline`, including
loopback endpoints, before resolving credentials or connecting. SSE servers are
not connected by interactive sessions.

The web tools are off until asked for. `rune doctor` reports what this machine
supports, and turning them on is one setting:

```toml
web_tools = true
```

They are refused by default because they send your queries to a search engine,
which is your call rather than a repository's, so a project file cannot enable
them. Set `offline = true` in the user configuration to enable the
[offline enforcement boundary](#offline-enforcement-boundary), including refusal
of requests to the model.

For image analysis, set `vision_model = "<image-capable-model>"` in the user
configuration. Interactive sessions, `rune ask`, and ACP use that model with
the active provider's endpoint and credential. A permitted `vision` call with
`paths` uploads workspace images as inline bytes, capped at 8 MiB per image;
every path in a batch must be allowed by policy before any upload. The configured
`vision_batch_images` and `image_adapter_output_bytes` limits bound the batch
and returned text. Main turn usage totals do not include vision helper requests.
This host accepts file paths; session image IDs are unavailable. Without
`vision_model`, or while offline, the advertised tool is explicitly labelled
unavailable.

A fetch with `allow_private = true` requests separate user approval for loopback,
private, and link-local access, including redirects. Enabling web tools,
full-access mode, and automatic model review do not grant this authority. The
terminal asks under `web_fetch_private`. Hosts can also supply user rules that
explicitly grant that permission for a `domain:host` target. Project rules and
general web grants cannot grant private access. Without an approval path or user grant,
the fetch is refused before contacting the network.

A model larger than the default window declares its capacity, which is what
`rune config` reports and what the status line budgets against:

```toml
[models.opencode-go]
id = "grok-4.7"
context_window = 2000000
```

A bare identifier still works for a model whose window the endpoint reports.
`context_window` must be greater than zero. A zero value is ignored with an
`invalid_field` diagnostic naming `models.<provider>.context_window`; the session
uses the endpoint's reported capacity or the default 128000-token window.
Changing `RUNE_PROVIDER` selects the new provider's entry in `models`, falling
back to `models.default`, together with that entry's declared window. If neither
entry exists, no model is selected and the previous model's window is discarded.
Setting `RUNE_MODEL` to a different identifier discards the configured model's
declared window. Until the new model's capacity is resolved, configuration
reports the default 128000-token window.

The providers that ship in the table are:

| Provider | What it is |
|---|---|
| `anthropic` | Anthropic Messages API |
| `openai` | OpenAI Responses API |
| `gemini` | Native Google Gemini Generate Content API |
| `opencode` | OpenCode Zen, pay-per-use |
| `opencode-go` | OpenCode Go, a subscription |
| `chat_completions` | Any OpenAI-compatible endpoint, with the URL you give |

Anything else is reachable by naming it and giving its endpoint, which is what
`chat_completions` does with a URL and what a self-hosted gateway needs. A
credential is read from the provider's own variable when it is already exported
(`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `OPENCODE_API_KEY`), and pasted
otherwise.

`gemini` (alias `google`) uses the native
[Generate Content API](https://ai.google.dev/api/generate-content) at
`https://generativelanguage.googleapis.com/v1beta`, with the key in
`x-goog-api-key`. `GOOGLE_API_KEY` is also accepted when `GEMINI_API_KEY` is
unset. Set `models.gemini` to the model identifier, with or without `models/`.
Text, function calls and results, thought-signature replay, and reported token
usage use the native wire format. The Rust SDK selects it with `Dialect::Gemini`.
This dialect uses the model's automatic thinking settings; explicit reasoning
effort and fast mode currently return `unsupported`.

## Use

```sh
rune                        # interactive session
rune ask "..."              # one bounded turn, prints the answer
rune review                 # review the pending changes in this repository
rune resume last            # continue the most recent session here
```

`rune review` can read files and search the workspace through a bounded tool
loop. Its named `read-only review` rule refuses tools that do not declare
themselves read-only, including file mutations, shell commands, and skill
installation. This restriction applies even in full-access mode; ordinary
permission rules still apply to reads. The review exchange is saved in session state.

Enable Bash completion for command names, aliases, and flags in the current
shell with `source <(rune completion bash)`. To keep it across shells, save
`rune completion bash > ~/.rune-completion.bash` and source that file from
your `~/.bashrc`. The script is generated from the same table as command help,
including `resume`, and does not load configuration or contact providers.

Use `rune --accessible` for a screen reader transcript, or
`rune --accessible resume last` to replay and continue a saved conversation.
Configure a provider and model first. This mode keeps the terminal's line input:
Enter submits one line, and `/exit` or end of input quits. Replies are printed
once after each turn, with explicit User, Assistant, Reasoning, Tool, and Notice
labels. Output is append-only, without cursor controls, colours, live status
rows, or menus. Permission prompts accept `yes` to run once and deny any other
answer. Questions accept a numbered option or `/cancel`. `/model <id>` changes
models; `/copy` points to terminal scrollback. Composer shortcuts and steering
while a turn runs belong to the default interactive mode.

Select `rune --ascii`, set `RUNE_ASCII=true`, or set `ascii = true` in profile
configuration to use ASCII decorations. Status fields use `|`, selections use
`>`, and tool summaries use `...`. Code continuation arrows become `>` and
history preview and prompt echo newline arrows become `/`. Unicode in prompts, replies, paths,
and tool output is preserved. This setting works with any theme and `NO_COLOR`.

Select `rune --theme high-contrast`, set `RUNE_THEME=high-contrast`, or set
`theme = "high-contrast"` in configuration for bright themed text on an explicit
black background. Every defined foreground/background pair targets at least
7:1 using the [WCAG relative luminance contrast formula](https://www.w3.org/WAI/WCAG22/Understanding/contrast-enhanced.html).
The palette uses exact colors from the fixed xterm 256-color cube, so the
same ratios apply in truecolor and 256-color terminals:

| Foreground slots | Foreground | Background | Contrast ratio (approximate) |
|---|---|---|---|
| `fg`, `variable` (also text on `bg`) | `#ffffff` | `#000000` | 21.00:1 |
| `dim`, `divider`, `comment` | `#d7d7d7` | `#000000` | 14.59:1 |
| `accent`, `link`, `function`, `operator` | `#5fd7ff` | `#000000` | 12.65:1 |
| `error` | `#ff8787` | `#000000` | 9.07:1 |
| `success`, `string` | `#87ff87` | `#000000` | 16.68:1 |
| `user_rail`, `keyword` | `#d7afff` | `#000000` | 11.47:1 |
| `number` | `#ffd75f` | `#000000` | 15.14:1 |

High-contrast footer hints and metadata do not use the terminal's faint
attribute. These targets describe Rune's defined theme colors; unstyled text
uses terminal defaults, and terminal palette customization can affect indexed
colors. `NO_COLOR`, including an empty value, suppresses colors even when
high-contrast is selected. Accessible mode continues to produce colorless output.

Run `/help` to list commands and the active input bindings, including menu and
transcript controls, cancellation precedence, and bracketed paste behavior.
Line-input sessions report their input mode instead of composer shortcuts.

In the default interactive composer, Alt-Enter inserts a newline at the caret; Enter
submits the whole prompt. Draft newlines appear on separate rows and reach
the provider as newline characters. Left and Right move across line breaks;
the terminal caret follows the line being edited. Long lines scroll horizontally
and tall drafts scroll vertically to keep the caret visible.

At the idle prompt, Tab completes the workspace path at the caret. A unique
match is inserted immediately; multiple matches open a menu where Up/Down
select and Tab or Enter inserts one path without submitting. Escape closes
the menu. Paths with spaces or shell metacharacters are quoted as one literal
selection, and Ctrl-_ undoes the completion. Directories end in `/` so another
Tab lists their children. Completion only lists names inside the primary or
configured additional workspace roots, including where symlinks lead.
Use forward slashes in paths. Slash command completion keeps its existing keys.

At the idle prompt, Ctrl-R searches recorded prompts from the current workspace,
newest first. Type any substring, ignoring case, and use Up/Down to choose a
match. Enter or Tab restores its exact text into the composer with the caret at
the end, without submitting it. Escape or Ctrl-C cancels and preserves the draft
and caret. Ctrl-_ undoes a restored prompt.

Ctrl-G opens the draft in `$VISUAL`, falling back to `$EDITOR` and then `vi`.
Editor commands may include arguments. Save and exit to reload the draft with
the caret at its end; Ctrl-_ undoes the replacement. Rune restores terminal
input after the editor exits and removes the private scratch file. A failed
editor leaves the draft and caret intact. The binding also edits steering
drafts while a turn runs.

Ctrl-_ (Ctrl-Shift-minus) undoes the last draft
edit and restores its caret. Repeated presses undo earlier edits, including
Unicode grapheme deletion and pasted text. Alt-R reapplies an undone edit and
restores its caret. Repeated presses redo later edits; a new edit clears redo
history. Press Alt with lowercase `r`.

`rune ask --image screenshot.png "Describe this screenshot"` attaches a local
image to the prompt for the selected model. Repeat `--image <path>` to attach
several files, including when the prompt comes from stdin. PNG, JPEG, GIF, and
WebP signatures are accepted, with at most 16 images and 8 MiB of total file
bytes per turn. Oversized input fails with `too_large` before any model request.
Attachments remain available across tool steps in this ask turn. Saved sessions
retain the prompt and response text; image bytes are not retained for replay.

`rune ask` sends the resolved instructions shown by `rune prompt --show`, including
workspace instruction files and the configured system prompt, on every request.

Workspaces containing `Cargo.toml` also receive a Rust repository symbol map with
declaration locations and lexical identifier references. The map refreshes before
each model request, including after tool edits. It respects `.gitignore`,
`.git/info/exclude`, ancestor ignore rules and `.ignore`, and skips symlinks.
References do not resolve Rust scopes, types, or macro expansion; comments and
literals are excluded. The default output cap is 8192 bytes, including its
incomplete-map notice. Configure `repository_map_bytes` in `[limits]` or with
`--limit repository_map_bytes=N`; zero disables the map. Scans stop at 4096
entries, 128 source files, 128 KiB per file, 1 MiB of total source, 1024
declarations, depth 32, and eight reference locations per symbol. These work caps
remain in force when the output limit is raised or set to `off`.

`rune ask` runs a bounded turn with the built-in host tools. Allowed calls run
under the configured permission mode and workspace boundaries; calls requiring
interactive approval return an error to the model without waiting for input.
The shared host loop allows three consecutive failures of the same tool name
and exact argument string per turn, ignoring call IDs. The next duplicate stops
with `limit_exceeded` naming `max_consecutive_tool_failures`, before execution.
A successful call or a different call resets the streak. Configure the cap in
`[limits]` or with `--limit max_consecutive_tool_failures=N`; `off` disables it.
Adjacent duplicate reads run in order to enforce this cap.
The next request includes the tool results. A positive `max_agent_steps` sets
the model step cap; zero or an unlimited setting uses a cap of 16 for `ask`.
Transient provider failures retry within each step up to `provider_max_attempts`,
honoring `Retry-After` or using bounded backoff. Each attempt uses the configured
`provider_head_timeout_ms` and `provider_request_timeout_ms` deadlines.
Rejected credentials never retry. Retries consume no extra model steps or usage.
Reaching the cap before a final answer exits 1 with `limit_exceeded` and an
empty `final_output`. JSON reports the steps, summed usage, and each tool's
`success` or `error` status.
Completed exchanges are saved as sessions for the current workspace, and
`--json` reports their `session_id`. Pass `--no-save` to create no session and
return an empty ID.

`rune ask --jsonl "prompt"` streams one JSON object per stdout line, flushing
each record as it arrives. `--jsonl` takes precedence over `--json`. The stream
begins with `{"type":"start"}` before prompt validation, then emits `step_start`
with a `step` number and `text_delta` records with `step` and `delta`. A
`step_restart` tells consumers to discard deltas from the failed attempt of
that step. Deltas from earlier tool steps are intermediate text; the final
answer is in the last record, `{"type":"final","result":{...}}`, whose `result`
has the same fields as `--json`, including failures and saved session IDs.
Diagnostics go only to stderr. Each record is capped at 1 MiB including its
newline; deltas are split at UTF-8 boundaries into at most 8 KiB of raw text.
Serialization stages at most one bounded record, and a slow stdout reader
applies backpressure instead of growing an event queue. An oversized final
result produces a small `too_large` final failure and exit 1, after any deltas
already delivered. Its model and session ID are empty; a completed exchange
may already have been saved unless `--no-save` was used.

`rune ask --json` failures include `error_detail` when structured context is available.
Provider failures retain the redacted `provider_code` and numeric
`retry_after_ms` from a supported `Retry-After` header, when reported, alongside
the existing HTTP status in `observed` and repair `hint`.

`rune sessions --search "fragment"` finds sessions by a case-insensitive literal
fragment in their current title or any recorded user message. Results stay scoped
to the current workspace unless `--all` is passed. `--limit`, `--cursor`, and
`--json` work as for ordinary listings; repeat the same search and scope when
continuing with a cursor.

`rune session <last|id> --search "symbol"` searches the saved conversation,
including original records removed from model context by compaction. It matches
case-insensitive literal fragments in user and assistant messages, saved streaming
fragments, tool names and arguments, tool results, and compaction summaries.
Results contain each complete original event with its sequence position, schema
version, and timestamp, in log order. `--json` returns `session_id`, `query`,
`matches`, and `truncated_at`. `last` selects the current workspace's latest
session; `--id <id>` selects an exact session. An incomplete final frame is omitted
with a warning on stderr; other log corruption fails the search.

`/compact` saves every original message in its removed prefix before changing
live history. The private archive retains all content parts, including complete
tool-call/result pairs and provider replay state. An archive write failure keeps
history unchanged. `rune session <last|id> --record all` lists these originals;
`--record <id>` retrieves one by its stable, session-local archive ID, including
after restart or later compactions. `--json` returns `session_id` and `records`,
each containing `id` and the exact original `message`. Archive IDs are separate
from journal sequence positions and from live positions that compaction renumbers.
The archive is capped at 256 MiB per session; it covers manual compaction only.
Summary failures do not add archive records. Search and export continue to read
the session journal; use `--record` for exact archived message parts.

`rune session export <last|id>` writes stable JSONL to stdout, one saved event
per line in sequence order, with its schema version and original timestamp.
Credentials, metadata, and transport headers are excluded. Recorded message
and tool content is preserved verbatim. An incomplete final frame is omitted
with a warning on stderr; other log corruption fails the export.

`rune usage export` writes the global usage ledger to stdout as versioned JSONL,
one retained record per line in ledger order. `--period <24h|7d|30d>` selects
the same inclusive time window as the usage report; without it all retained
records are exported. Each line uses `schema_version: 1`, `created_at_ms`,
`model`, `helper` (`main`, `permission_review`, or `vision`), and `request_count`.
Reported token counts and the exact `total_cost` string are included; unreported
values stay absent and reported zeroes stay zero. No session or provider
dimensions are inferred. An incomplete ledger tail is omitted with a warning
on stderr; other ledger corruption fails before writing any records. Export
does not rewrite or lock the ledger. `rune usage --json` remains a summary.

`rune session fork <last|id> --at <seq>` creates an independent saved branch
through the selected event, inclusive. Use sequence positions from `session
export` or `session --search`, rather than tree turn numbers. `--id <id>` also
selects an exact source; `--json` returns the new `id`, `source`, and `through`.
Resume either session by its ID. The copied events retain their sequence numbers
and timestamps, followed by a `forked_from` provenance event. The original is
never rewritten. Boundaries that leave any tool call unanswered are refused,
including partial result groups; choose the final result of the group or a later
event. Child sessions cannot be forked. An unfinished assistant answer retains
the usual interrupted replay behavior. Forking copies journal history only,
without changing workspace files or copying undo journals, manual compaction
archives, or global usage ledger entries.

Sessions are written as they run, so an interrupted session resumes. Use
`/goal <objective>` in an interactive session to explicitly set or replace the
active objective. `/goal` shows it without changing the log. The exact objective is
restored on resume and included in subsequent model instructions, independently
of conversation compaction. Ordinary prompts create no objective; `/new` starts
a session without one.

Interactive assistant text is journalled in bounded, durable frames before it is
displayed.
After process death, resume replays the partial answer once with an
`[interrupted]` boundary, without inventing a completed turn or token usage.
Interactive resume replays the saved exchanges above the composer before you
type. The footer and `/status` distinguish `provider-reported` counts from
estimates. Fresh sessions and responses without complete token usage use a
`request bytes` estimate that includes instructions and tool schemas. On resume,
the initial estimate is labelled `saved usage` or `history bytes` until the next
request. Prompts are remembered and recallable with
`/history`. Custom slash commands live in `.rune/commands/*.md` in a repository,
so a team can ship a workflow with the code. Discovered project and user commands
appear alongside builtins in slash-command completion, with descriptions and
argument hints. Up/Down selects a candidate; Tab or Enter completes a partial
name. Enter on the complete name shows its expanded prompt for review.
Before each CLI session request, Rune estimates the serialized input, including
instructions and tool schemas,
and summarizes earlier turns at `compaction_trigger_percent` of usable input
capacity. Requests above capacity remain eligible for compaction when the retained
tail fits with instructions, tool schemas, and the output reserve. A failed summary,
including one stopped at the token limit, preserves the conversation and stops
that request with an explicit error.

Use `/compact preview` to see the exact counts of conversation records that
manual compaction would summarize and retain verbatim. Records are the current
user, assistant, and tool-result turns, including an existing handoff summary;
they exclude session metadata and turn boundary events. The preview uses the same
cut as `/compact`, keeps tool calls with their results, and works offline without
a model request or history changes. Retained counts exclude the new summary that
`/compact` would insert. A short conversation reports zero records to summarize.

Use `/pin <constraint>` to retain an explicit user constraint verbatim in every
following model request, including after repeated manual or automatic compaction
and session restart. `/pin` lists the current pins. Exact duplicates are ignored;
ordinary prompts, assistant responses, and tool results are never pinned
automatically. Pins remain user content and count toward request capacity.
`/new` starts a conversation without the previous pins.

Use `/undo preview` to see affected file paths and diffs from current contents
to the bytes `/undo` would restore. It leaves files and the mutation journal
unchanged and works offline. `/undo` restores each recorded file to its state
before this session first changed it, removing files the session created.
Renames show both paths. Binary files show a difference notice and byte counts;
files that cannot be read show a per-file error. Shell changes are not tracked.

Run `rune help` for everything, or see [COMMANDS.md](COMMANDS.md) for the full
reference.

Models can call `update_plan` with the complete ordered `plan` array of
`{"step": "description", "status": "pending"}` objects and an optional
`explanation`. Status is `pending`, `in_progress`, or `completed`; at most one
step can be in progress. Steps must be distinct after trimming whitespace.
Updates replace the previous plan, and an empty array clears it. Changed
snapshots emit versioned status events, including `plan_updated` records in
`rune ask --jsonl`; identical retries retain the version and emit no update.
Plans are bounded to 64 steps, 1024 bytes per description, and 4096 bytes per
explanation. Plan state stays in memory across live turns and compaction,
changes no workspace files, and resets for a new or reopened conversation.

Models can call `syntax_diagnostics` with `{"path": "fixtures/example.rs"}`
to check one Rust file using local `rustfmt` on PATH. Results include syntax
messages with file, line, and column; syntax errors return a tool error. Valid
syntax reports no diagnostics even if the file needs formatting. The optional
`edition` is `2015`, `2018`, `2021`, or `2024` (default). This checks syntax,
without type checking, compilation, procedural macro expansion, or child module
inspection. Rustfmt configuration is ignored and files stay unchanged. Workspace
path permissions apply. The checker uses the existing process supervisor with
resource limits, cancellation, a 30-second deadline, and bounded output. Optional
`max_bytes` lowers the host output budget; incomplete output has a truncation
marker. Missing rustfmt returns an explicit checker startup error.

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
  Ctrl-O closes it with the draft and caret preserved. Tab/Shift-Tab selects a
  tool; Space collapses it to a preview or expands up to 16 KiB and 200 wrapped
  content rows. Reopen the snapshot to restore full recorded output.
  A running turn continues while the snapshot is open; reopen it to see newer
  output. Provider and tool
  result limits still apply to what is recorded.
- **Permission first.** Every sensitive action passes a policy gate, and
  `rune permissions` explains exactly which rule decided it.
  Interactive requests show the complete scope with Run once and Deny choices.
  Use Up/Down and Enter to answer; Escape, Control-C, or Control-D cancels the
  turn. Run once approves only the displayed call.
- **Sandboxed execution.** Commands run under the platform sandbox where one
  exists. `rune doctor` reports what your host can enforce.
  `rune sandbox explain -- 'git status'` previews the backend, writable roots,
  network decision and protected paths with sources, including nested repository
  config and hooks and absent macOS hooks destinations, without running the
  command. The preview uses the current workspace and configured additional
  directories (`--add-dir` adds roots;
  `--no-additional-dirs` ignores saved roots).
  Network access requires an explicit shell context grant, previewed with
  `--external-access`; offline mode overrides it. `--json` emits the same report.
  Refused commands apply no protections; an unsandboxed fallback has unrestricted
  access. This explains sandbox policy, independently of command approval.
- **Offline mode.** `--offline` refuses provider requests, disables web tools,
  skips remote MCP connections in CLI checks and restricts sandboxed shell
  networking. Local MCP processes and unsandboxed commands have separate limits
  described below.
- **Local.** Sessions, usage, and credentials stay on the machine.

## Offline enforcement boundary

Enable offline mode with `--offline`, `RUNE_OFFLINE=true`, or `offline = true`
in the user configuration. This is enforced at the paths below; it is not a
process-wide network filter. Loopback endpoints are included in the provider
and remote MCP refusals, so offline mode also blocks a locally hosted model.

| Traffic | Offline behavior | Test evidence |
|---|---|---|
| Model/provider requests and model discovery | Provider transport refuses before contacting the endpoint, with `permission_denied` and no retry delay. `rune models --offline` reports only the configured model. | [`an_offline_endpoint_refuses_before_any_network_call` and `an_offline_model_list_is_a_policy_refusal_before_any_network_call`](crates/rune-net/src/transport.rs); [`offline_mode_produces_no_connection`](crates/rune/tests/egress.rs); [`listing_models_offline_reports_the_configured_model_without_a_request`](crates/rune/tests/cli.rs); [`an_offline_turn_refuses_without_fetch_calls_or_retry_delay`](crates/rune-agent/tests/turn_loop.rs) |
| Web fetch and search | The CLI installs neither web backend and keeps the tools denied, even with `web_tools = true`. | [`an_offline_run_gets_no_clients`](crates/rune/src/web_client.rs); [`an_offline_run_does_not_enable_the_web_tools`](crates/rune/src/permissions.rs) |
| Image analysis | The CLI omits the vision backend and labels the tool unavailable, even with `vision_model` set. Configured tools also refuse an offline execution context before reading images. | [`absent_configuration_and_offline_runs_have_no_vision_client`](crates/rune/src/vision_client.rs); [`an_offline_context_does_not_reach_a_configured_backend`](crates/rune-tools/src/vision.rs) |
| Remote MCP over HTTP or SSE | `rune mcp list`, `rune mcp check` and interactive sessions skip remote declarations before resolving credentials or connecting, including approved loopback servers. Online interactive sessions connect approved, enabled HTTP servers; SSE remains unsupported there. | [`pending_rejected_disabled_and_offline_servers_are_never_contacted` and `check_respects_trust_disabled_and_offline_states_without_contact_or_expansion`](crates/rune/tests/mcp_list.rs); [interactive HTTP MCP fixture](crates/rune/tests/mcp_http.rs) |
| Local MCP over stdio | Approved, enabled servers still start during offline listing, checking and interactive sessions. These child processes do not use the shell sandbox; their own network traffic remains unrestricted by Rune offline mode. Project trust approval authorizes startup, not network isolation. | [`approved_stdio_can_start_during_offline_listing`, `check_local_stdio_works_offline_without_a_provider` and `offline_stdio_mcp_can_send_network_traffic`](crates/rune/tests/mcp_list.rs); [interactive MCP fixture](crates/rune/tests/mcp_stdio.rs) |
| Shell commands | Offline mode disables the sandbox's network grant, overriding external access and full-access permission mode. Linux uses a network namespace; macOS generates a network-denying Seatbelt profile. Without a usable sandbox, commands are refused unless `--allow-unsandboxed` permits fallback. That fallback has no network isolation, including offline. | [`offline_shell_policy_restricts_network_even_with_external_access` and `an_offline_full_access_shell_cannot_reach_a_local_fixture`](crates/rune-tools/src/shell.rs); [sandbox profile and argv tests](crates/rune-exec/src/sandbox.rs); [`offline_shell_fallback_requires_opt_in_and_applies_no_isolation`](crates/rune/tests/egress.rs) |

Linux CI requires a live socket-connect smoke check with source editing:
`RUNE_REQUIRE_LINUX_SANDBOX=1 cargo test -p rune-exec --test linux_network live_linux_network_isolation -- --exact --nocapture`.
It proves the loopback fixture is reachable, refuses its connection in the
sandbox, verifies a source edit, and checks an explicit sandbox network grant.
Missing or unusable isolation fails this gate with its reason. Local runs without
`RUNE_REQUIRE_LINUX_SANDBOX=1` report unavailable isolation and skip the live check;
policy and generated-argument tests still run. Use `rune doctor` and
`rune sandbox explain --json -- 'command'` to inspect your host's capability
and the resolved policy. For isolation of local MCP processes or an unsandboxed
fallback, apply an external OS/container network restriction.

Library hosts must carry offline settings into provider endpoints and shell
execution contexts and omit web backends themselves. The MCP `Client` has no
offline switch: hosts must filter remote configurations before `connect_all`
and arrange any isolation required for stdio children. Host-supplied backends
and child processes do not acquire offline enforcement merely by using Rune.

## Unsafe code boundaries

The [workspace lint](Cargo.toml) denies unsafe code by default and permits
documented exceptions at the reviewed boundaries below. This describes Rune's
own code, not unsafe code inside dependencies.

The native binary on Unix has one unsafe call site:
[`pre_exec` resource-limit setup](crates/rune-exec/src/command.rs). It runs in
the child between fork and exec under an async-signal-safe contract: resource
limit syscalls, no allocation, and an error returned if setup fails. This site
is excluded on non-Unix targets.

The WASM module has separate unsafe FFI sites in its
[page imports and exported entry points](crates/rune-web/src/exports.rs),
compiled only for [WASM targets](crates/rune-web/src/lib.rs). Imports pass raw
pointers and lengths: the host must copy input bytes before returning, retain
no pointers, and keep writes and reported read lengths within buffer capacity.
Staged replies use `take` to copy into an allocated buffer. Exported string
arguments must come from `rune_alloc` with exactly the allocated length;
`argument` takes ownership and frees them once, so the host must not reuse them.
The module's local unsafe allowance, per-call safety comments, and exported
safety contracts document this boundary. Native tests do not validate these
WASM FFI obligations; the WASM fixture smoke check and its browser-testing
limits are described under [Contributing](#contributing).

## Configuration

Five layers, highest first: command-line flags, `RUNE_*` environment variables,
the project file `.rune.toml`, the user file at `$XDG_CONFIG_HOME/rune/config.toml`,
then built-in defaults.

Set `RUNE_CONFIG` to use a different user configuration file. Workspace edits
and provider selections read and write that file. An empty or unset value uses
the default path.

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

At a settled turn boundary, `Agent::compact(|request| summarizer(request))`
explicitly summarizes an older conversation prefix. The host callback receives
summary instructions, the removed records including tool activity, and explicit
pinned user constraints kept independently of its output. The callback
returns `Result<String>` containing a complete summary. Report cancellation or
truncation as an error. The synchronous operation returns the removed record
count, or zero without calling the host for a short conversation. It retains
recent tool calls with their results, signed provider replay data, and accumulated
usage. Errors and unusable or oversized summaries leave the conversation intact.
Closed agents and running turns refuse compaction. Compacted checkpoints restore
through the existing checkpoint API and obey the same `PromptHistoryBytes` cap.

`Agent::pin_constraint(text)` pins an explicit user constraint without changing
its text. It returns true for a new pin and false for an exact duplicate. Pins
survive compaction and checkpoint restoration and appear as user content in each
provider request. Empty pins, running turns, closed agents, and additions that
exceed `PromptHistoryBytes` are refused without changing the conversation.

SDK turns retain at most 1,024 unread events (`EVENT_QUEUE_CAPACITY`). Consume
`Turn::events()` or `Turn::next_event()` while a long response runs. If the
queue fills, the SDK cancels provider and tool work cooperatively and
`Turn::result()` returns `limit_exceeded` with invariant `sdk_event_queue` and
a backpressure message. Queued events remain readable; draining them later
does not clear the failure. Calling only `result()` works for turns whose
events fit in the queue.

ACP clients must send an `initialize` request with an integer `protocolVersion`.
Rune supports version 1 and returns `protocolVersion: 1`, its `agentInfo`,
`agentCapabilities`, and an empty `authMethods` list. It advertises session
loading, listing, resuming, and closing; text and embedded context prompts;
and stdio MCP support. Image, audio, HTTP MCP, and SSE MCP support are false.
Omitted client capabilities are treated as unsupported; Rune uses its own
filesystem and shell tools.

Following [ACP version negotiation](https://agentclientprotocol.com/protocol/v1/initialization),
an unsupported client version still receives Rune's version 1 result. Session
methods remain blocked until an `initialize` request explicitly selects version
1. A client that supports it can retry with version 1; otherwise it should close
the connection. Before compatibility is established, all implemented `session/*`
requests return JSON-RPC `-32600` with `data.code: "not_initialized"` and
`data.protocolVersion: 1`; session notifications are ignored and logged.
Missing or malformed versions return `-32602` naming `protocolVersion`.
An `initialize` notification cannot initialize the connection. After successful
initialization, further initialization requests return `-32600` with
`data.code: "already_initialized"`, leaving the established connection usable.

ACP session creation, loading, and resuming connect the client's requested
stdio `mcpServers` using each server's absolute `command`, `args`, and `env`
name/value pairs. Their tools are advertised as `mcp_<server>_<tool>` and use
the session's normal permission checks. Connections belong to that session;
clients supply them again when loading or resuming. Startup failures fail the
request, and startup and calls use the configured MCP timeouts. HTTP and SSE
MCP transports remain unsupported in ACP.

ACP permission requests wait up to `acp_permission_timeout_ms` in `[limits]`
(120000 ms by default). An unanswered request ends its prompt with a `timeout`
error and leaves the tool unexecuted. The connection remains usable for later
prompts, and late approval replies are ignored. Zero expires immediately;
`off` still applies the limits system's hard ceiling of 67108864 ms.

A Node binding lives in [`bindings/node`](bindings/node).

## Package versions

The maintained repository is [AadiXC0DE/Rune](https://github.com/AadiXC0DE/Rune).
Cargo package metadata, the Node package, and the installation and CI links
all refer to that repository.

The Rust workspace and `@rune-sh/node` use a shared version policy:
`bindings/node/package.json`'s `version` must equal `Cargo.toml`'s
`workspace.package.version`. Update both for every release, including releases
that only change the binary. The Node package has no independent version track.
`node --test bindings/node/test.mjs` enforces this policy and checks repository
links in the existing CI fast job.

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

`gate` checks formatting and lint first, then the size and startup budgets,
then the workspace tests. A failed step stops the gate immediately.

`budget` reports raw process latency for `--version` and `--help`, including
minimum-of-31, median and p95 (nearest rank, the 30th sorted sample). Each run
measures wall time from process spawn through exit with stdout and stderr discarded.
The enforced work budgets are 5 ms for `--version` and 8 ms for `--help`: the
minimum-of-31 minus the system `true` process baseline minimum-of-31, clamped to
zero. If no baseline is available, the raw minimum is used and labeled accordingly.
Baseline-subtracted work is an estimate of additional work, not process latency.

CI also builds `rune-web` for `wasm32-wasip1` and runs a fixture prompt through
`rune_alloc`, `rune_configure`, and `rune_prompt` with Node.js 22's WASI runtime.
The smoke test checks the configured provider request, streamed UTF-8 answer,
and completion event using synchronous fixture imports. It requires no API key
or network access to a provider. This checks the compiled boundary; browser
Promise Integration and the page UI still require browser testing. Run it locally
with Node.js 22 or newer:

```sh
rustup target add wasm32-wasip1
cargo build --release --locked -p rune-web --target wasm32-wasip1
node .github/scripts/test-wasm.mjs target/wasm32-wasip1/release/rune_web.wasm
```

CI also runs `cargo deny check` against `deny.toml` for dependency advisories,
licenses, bans, and sources. The existing `webpki-roots` 1.0.9 trust-store data
has a specific `CDLA-Permissive-2.0` license exception; that license remains
rejected for other crates. To run the same check and its scratch dependency
policy test locally:

```sh
cargo install cargo-deny --version 0.19.9 --locked
cargo deny check
bash .github/scripts/test-dependency-policy.sh
```

`budget` and `release` honor `CARGO_TARGET_DIR` when locating built binaries.
Relative values resolve from the command's working directory. Release archives
are still staged under `target/dist`.

The reproducible CI job builds the same source with two fresh scratch
`CARGO_TARGET_DIR` directories, extracts each release, and compares SHA-256
hashes of both the binaries and archives. Run this check on Linux from a
workspace with no old releases in `target/dist`:

```sh
bash .github/scripts/check-reproducible-release.sh
```

CI also installs the staged Unix release using the archive commands above,
checks `rune --version`, and uninstalls in a disposable prefix. It isolates
HOME and XDG paths and checks that seeded user files, contents, and permissions
stay unchanged after both install and uninstall. Run it on a staged archive:

```sh
cargo xtask release dev
python3 .github/scripts/test-archive-install.py target/dist/*/*.tar.gz
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
