# Rune expansion audit

Audited 2026-10-04 through 2026-10-05 (Europe/Berlin) at `44a69bcc1400e829656815b3a97d0a51b5471954`, version `rune 0.1.16 (dev, 44a69bcc1400)`. Evidence commands: `git rev-parse HEAD`, `target/debug/rune --version`. Research only. The plan is committed at `AUDIT.md` in the repository root. Source files were read without editing; the single repository deliverable is this document. Builds wrote permitted `target/` artifacts; reproduction fixtures and competitor clones live under `/tmp`.

The workspace contains fourteen crates under `crates/`, plus `xtask`, fifteen workspace members (`Cargo.toml:4`). The initial reading covered `README.md`, `COMMANDS.md`, `CHANGELOG.md`, `xtask/src/main.rs`, `.github/workflows/ci.yml`, `deny.toml`, and each crate manifest. The scope includes `rune` composition, core configuration, net protocols, policy, exec, tools, agent, session, context/MCP, terminal, ACP, SDK, testkit and WASM (`Cargo.toml:4`). References to Rune source apply to the audited commit. Competitor links pin inspected commits; released binaries and current source are distinguished below.

There are three evidence levels in this report: observed binary behavior with commands/captures; executable library probes; and source findings that still need the specified integration test. A feature row proposes behavior; its evidence identifies the existing boundary or competitor behavior motivating it. Acceptance checks are future requirements, not claims that those tests already exist or passed. T1 is the first-week queue, T2 follows, T3 is optional expansion. Dependencies are explicit where a reader or host integration needs another item first.

## Build, test and budgets

The host is Linux x86_64 GNU, kernel `6.8.0-139-generic` (`uname -a`). Neither Cargo nor rustc was initially on PATH. An isolated official rustup installation under `/tmp` supplied `rustc 1.98.1 (48a229cea 2026-09-01)` (`/tmp/rune-audit-cargo/bin/rustc --version`). All Cargo runs used:

```sh
export PATH=/tmp/rune-audit-cargo/bin:$PATH
export CARGO_HOME=/tmp/rune-audit-cargo
export RUSTUP_HOME=/tmp/rune-audit-rustup
```

Two binary tests invoke `git init`, `git add` and `git commit` inside scratch repositories (`crates/rune/src/main.rs:1542`, `crates/rune/src/main.rs:1574`). To respect this run's prohibition, a PATH shim forwarded read-only git commands and rejected git writes. The required commands were run unchanged; their failures below are caused by that restriction. They are not evidence that Rune fails its normal gate. The second fixture does not assert successful repository initialization, so one fixture failed and the other passed with writes blocked (`crates/rune/src/main.rs:1585`). The later complete suite explicitly excludes both.

| Actual command | Wall seconds | Exit | Actual result |
|---|---:|---:|---|
| `cargo xtask check` | 224.000 | 1 | Formatting and Clippy completed, binary tests: 343 passed, 1 failed, 1 ignored; failed git fixture stopped the workspace run. |
| `cargo xtask gate` | 176.374 | 1 | Release budgets passed, then the same binary-test failure: 343 passed, 1 failed, 1 ignored. |
| `cargo xtask test` | 72.816 | 1 | Same binary-test failure: 343 passed, 1 failed, 1 ignored. |

These times include dependency downloads, compilation and Cargo lock contention. They are elapsed run times, not clean-build benchmarks. The test summaries took 4.49, 5.13 and 4.56 seconds respectively. Exact final check output:

```text
---- tests::pending_changes_include_a_modified_and_an_untracked_file stdout ----
thread 'tests::pending_changes_include_a_modified_and_an_untracked_file' (160659) panicked at crates/rune/src/main.rs:1569:9:

failures:
    tests::pending_changes_include_a_modified_and_an_untracked_file

test result: FAILED. 343 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 4.49s
error: test failed, to rerun pass `-p rune --bin rune`
xtask: `cargo test --workspace` failed
```

The gate's actual budget output:

```text
binary size  4717720 bytes (4.50 MiB)
process floor       1.24 ms
startup --version  2.31 ms raw, 1.07 ms work (budget 5 ms)
startup --help     2.82 ms raw, 1.58 ms work (budget 8 ms)
dependencies  accepted duplicates: winnow: v0.7.15, v1.0.4
dependencies  no unexpected duplicate versions
```

The release binary is 4,717,720 bytes, versus README's approximately 3 MiB headline (`README.md:7`). The actual ceiling is 8 MiB (`xtask/src/main.rs:17`). Startup uses the minimum of 31 samples, subtracting a separately measured minimum process floor (`xtask/src/main.rs:411`, `xtask/src/main.rs:481`), despite comments describing medians (`xtask/src/main.rs:19`). The budget passed; it does not establish a 2 ms median end-to-end startup on other machines. `gate` invokes budget and tests, omitting formatting and Clippy (`xtask/src/main.rs:174`), contrary to the documented "above plus" contribution command (`README.md:165`).

A first complete-suite attempt with the two git fixtures skipped passed unit/integration suites but failed a doctest with `error: extern location for rune_agent does not exist: /home/hermes/repos/Rune/target/debug/deps/librune_agent-29a9377f077eda8c.rlib`, elapsed 87.719 seconds, exit 1. Other Cargo builds were active, so the cause was not isolated. A second attempt, elapsed 46.299 seconds, exit 101, failed one SDK plugin test with:

```text
plugin `shipped` could not start `/home/hermes/.hermes/cache/scratch/.tmpnbPoyX/bin/plugin-fixture`: Text file busy (os error 26)
test result: FAILED. 25 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.24s
```

The isolated retry, `cargo test -p rune-sdk --test plugin a_relative_program_is_found_beside_the_manifest -- --exact`, passed, one test in 0.03 seconds. Its source copies and starts a fixture binary (`crates/rune-sdk/tests/plugin.rs:383`). One failure followed by a pass demonstrates a transient result on this host, not an established defect in production process launching.

Finally, this command passed the workspace and doctests with no competing builds:

```sh
TMPDIR=/tmp cargo test --workspace -- --test-threads=1 --skip tests::pending_changes_include_a_modified_and_an_untracked_file --skip tests::a_repository_with_no_commits_yields_no_changes
```

Real elapsed output was `AUDIT_ELAPSED=55.11 AUDIT_EXIT=0`. Summing the 43 successful test-result lines gives 2,504 passed, one ignored, two filtered out. The ignored test is a live web search (`crates/rune/src/web_client.rs:153`). `node --test bindings/node/test.mjs` also passed: `tests 10`, `pass 10`, `fail 0`, `duration_ms 537.531195`.

CI runs formatting, Clippy and workspace tests, with bubblewrap installed on Linux (`.github/workflows/ci.yml:34`). It does not invoke the declared `deny.toml` policy, the Node tests or the WASM task (`.github/workflows/ci.yml:34`, `.github/workflows/ci.yml:123`; `deny.toml:16`; `xtask/src/main.rs:110`). The reproducible lane compares two packs of the same build directory, which tests archive determinism, not independent clean-build reproducibility (`.github/workflows/ci.yml:86`). Artifact lookup hard-codes `target/release` and ignores `CARGO_TARGET_DIR` (`xtask/src/main.rs:181`, `xtask/src/main.rs:352`), while the web task honors it (`xtask/src/main.rs:118`). Release JSON interpolates channel text without JSON escaping (`xtask/src/main.rs:258`). Those latter findings are source-reviewed, not live alternate-target release runs.

## Actual terminal behavior


All repository access in this subtask was read-only. Build output was permitted under `target/`; captures, scripts, dummy credentials, and isolated workspace files were written under `/tmp/rune-audit-terminal/`. The actual binary was driven through detached `tmux` PTYs. `capture-pane -p` captured the terminal's grid, `capture-pane -p -S -10000` captured scrollback, `pipe-pane` captured emitted bytes, and `display-message` recorded the actual cursor and dimensions. PNGs render those captured grids with DejaVu Sans Mono. They are photographs of captured terminal cells, not Figma or speculative interface mockups. Snapshots are discrete, so they establish the recorded stale frames, clipping, and cursor positions; they do not establish that every intervening frame was free of flicker.

Build command actually run:

```sh
PATH=/tmp/rune-audit-cargo/bin:$PATH CARGO_HOME=/tmp/rune-audit-cargo RUSTUP_HOME=/tmp/rune-audit-rustup cargo build -p rune > /tmp/rune-audit-terminal/build.log 2>&1
```

Final output: ``Finished `dev` profile [optimized + debuginfo] target(s) in 3m 20s``. The command initially waited for another Cargo build's lock, so 3m 20s is not an uncontended compile benchmark. `target/debug/rune` was used for interaction captures; the panic reproducer additionally used `target/release/rune`.

Commands actually run for the fixture matrix:

```sh
python3 -m venv /tmp/rune-audit-terminal/venv
/tmp/rune-audit-terminal/venv/bin/pip install pillow pyte
python3 /tmp/rune-audit-terminal/mock.py
/tmp/rune-audit-terminal/venv/bin/python /tmp/rune-audit-terminal/scenarios.py > /tmp/rune-audit-terminal/scenarios.log 2>&1
python3 /tmp/rune-audit-terminal/mock2.py
AUDIT_PORT=18765 /tmp/rune-audit-terminal/venv/bin/python /tmp/rune-audit-terminal/followups.py > /tmp/rune-audit-terminal/followups.log 2>&1
AUDIT_PORT=18765 /tmp/rune-audit-terminal/venv/bin/python /tmp/rune-audit-terminal/extra.py > /tmp/rune-audit-terminal/extra.log 2>&1
python3 /tmp/rune-audit-terminal/mock3.py
AUDIT_PORT=18766 /tmp/rune-audit-terminal/venv/bin/python /tmp/rune-audit-terminal/panics.py > /tmp/rune-audit-terminal/panics.log 2>&1
```

Mock2 uses the correct registered tool name `ask_user_question` and includes shell `action: run`. Findings about those tools use corrected `question-fixed`, `tools-fixed`, and `sandbox-fixed` captures. Incorrect fixture calls in the first matrix are excluded.

### Confirmed defects

#### TF-01: a fresh connection creates a directory the following session refuses

The `fresh-clean` case created only the parent `state` directory, mode 0700, and asserted `state/rune` did not exist. It typed `chat_completions`, `http://127.0.0.1:18765/v1`, and a dummy key through first-run `rune`. Connecting succeeded, but the session failed. The shell's real `umask` output was `0002`; `state/rune` ended with mode 0775. Auth stores the key through `write_private` (`crates/rune-net/src/auth.rs:185`), whose parent `create_dir_all` does not set a private directory mode (`crates/rune-core/src/paths.rs:409`). Session state directory creation subsequently verifies and rejects that mode (`crates/rune-core/src/paths.rs:364`). The file itself remains 0600; this finding concerns the broken first-run transition, not an assertion that the key was readable.

The captured output was:

```text
connected chat_completions
endpoint  http://127.0.0.1:18765/v1

Run `rune models` to see the model in use, or `rune connect` again to add anothe
r.
starting a session
rune: unsafe_path: `/tmp/rune-audit-terminal/fresh-clean/state/rune` has mode 77
5, expected 700 or narrower
hint: run `chmod 700 /tmp/rune-audit-terminal/fresh-clean/state/rune`

EXIT:1
```

Repair should ensure Rune's own directory is private before credential writes, and cover both 0022 and 0002 umasks. The file-mode failure independently happened in `first-run`; the later `fresh-clean` run excludes reuse of that earlier fixture.

#### TF-02: long drafts cannot be edited visibly

In an 80x24 terminal, type 100 `a` characters followed by `TAIL-END`. The grid shows only 78 `a` characters after the prompt; `TAIL-END` is hidden. `cursor=79,2`. Left followed by `Z` edits the hidden suffix and still renders the same prefix. `render_prompt` always truncates the beginning of the input (`crates/rune-term/src/transcript.rs:467`), while the requested cursor uses the entire input's width (`crates/rune/src/session.rs:1755`); the column escape is not bounded to a visible input viewport (`crates/rune-term/src/inline.rs:412`). The composer needs horizontal scrolling around the caret or a multiline layout.

```text
[interaction/long-draft] 80x24 cursor=79,2 history=20
ctrl-c cancel  /help commands  esc clear
audit-model-1 | auto | ctx 0% (128.0k left) | /tmp/rune-audit-terminal/interacti
> aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
```

#### TF-03: widening leaves a stale truncated draft until another key arrives

At 32 columns, type `0123456789` five times plus `VISIBLE-END`, then grow the terminal to 80 columns. Before any key, the prompt remains `> 012345678901234567890123456789`. Press Right, and it becomes `> 01234567890123456789012345678901234567890123456789VISIBLE-END`. Both captures are 80x24. `draw_prompt` renders using `host.width()` before `paint_with_menu` refreshes the terminal dimensions (`crates/rune/src/session.rs:1754`, `crates/rune/src/session.rs:716`). Refresh before composing the row, rather than after it.

#### TF-04: short model pickers exceed the terminal and leave stale menu rows

Open `/model` at 80x24, shrink to 32x8, then press Down. All eight rows contain model entries and the list hint, with neither prompt nor status visible. Press Escape and grow back to 80x24: fragments of the old menu remain above the restored prompt. Picker default window is ten entries (`crates/rune-term/src/picker.rs:16`); title and hint add more rows (`crates/rune/src/session.rs:1792`). Inline treats the entire menu as reserved rows even when that exceeds the terminal (`crates/rune-term/src/inline.rs:212`, `crates/rune-term/src/inline.rs:236`). Cap the menu by remaining height and rebuild the region anchor after physical scrolling.

```text
[picker/short-after-key] 32x8 cursor=2,0 history=23
  audit-model-5
  audit-model-6
  audit-model-7
  audit-model-8
  audit-model-9
  audit-model-10
  1-10 of 15
type to narrow, up/down choose,
```

The independent `picker/closed-grown.txt` capture contains `models from chat_completions` and duplicated current model rows above the idle status despite Escape having closed the picker.

#### TF-05: resizing resets completion selection

At 80x24, type `/`, press Down eight times. The selected row is `/new`, and the position indicator says `4-9 of 13`. Resize to 32x10: selection jumps to `/model`, and the indicator becomes `1-6 of 13`. Resize events map to `Ignored` in the key reader (`crates/rune-term/src/input.rs:143`, `crates/rune-term/src/input.rs:174`), and `Ignored` unconditionally sets selection to zero (`crates/rune/src/session.rs:1713`). Preserve selection for non-edit events.

#### TF-06: a 12-column terminal drops output characters permanently

Send `long-word`, whose fixture emits exactly 300 `W` characters followed by `END-LONG-WORD`. Finished scrollback has seventeen rows of twelve `W` characters, 204 of the 300, and the marker is `END-LONG-WOR`. Counting only lines made of `W` characters yields 204. Rendering wraps with a minimum width of twenty (`crates/rune-term/src/transcript.rs:345`, `crates/rune-term/src/transcript.rs:389`); Inline then prefix-clips those rows at twelve columns (`crates/rune-term/src/inline.rs:388`). Wrapping must use the actual terminal width or explicitly suspend rendering below the supported width.

#### TF-07: ask mode does not ask for shell approval

The corrected fixture requests shell `{"action":"run","command":"printf AUDIT_SHELL_OK"}` in a real interactive session with mode `ask`. No permission UI opens. The model receives:

```text
Tool reply: `shell` was not run: no rule allows `printf AUDIT_SHELL_OK`, and
this session has no way to ask for approval. Do not retry it or a variation
of it. Continue without it, or tell the user what to run or which rule to add.
```

The interactive host returns unresolved `Ask` (`crates/rune/src/session.rs:388`); the agent converts it into this refusal (`crates/rune-agent/src/turn.rs:709`). Implement a terminal approval bridge with denial, one-call approval, cancellation, and an exact displayed command. Until then, `permissions` saying an action "asks first" is incorrect (`crates/rune/src/permissions.rs:131`).

#### TF-08: the advertised question tool cannot collect an answer interactively

In a PTY with `full-access`, the corrected fixture calls `ask_user_question` with two options. The actual result is:

```text
  ask_user_question: the question could not be asked: a question needs an
  answer, and this run cannot collect one

Tool reply: the question could not be asked: a question needs an answer, and
this run cannot collect one
```

The inventory always registers `AskUserQuestion::unavailable()` (`crates/rune-tools/src/inventory.rs:110`), including interactive sessions. Connect an answerer to the real terminal picker and retain the unavailable answerer for machine callers.

#### TF-09: resumption resets the context meter

The populated session `3TEygH0V-DEv` had three completed requests; `/tree` reports three turns and six user/assistant nodes. `resume 3TEygH0V-DEv` nevertheless draws `ctx 0% (128.0k left)` until another response. History is loaded (`crates/rune/src/session.rs:935`) but `context_used` is initialized to zero (`crates/rune/src/session.rs:985`). Reconstruct a truthful usage estimate or show unknown before the next provider usage count.

#### TF-10: resumption resets `/cost` totals

In that same populated session, `/cost` prints `no requests have been made in this session yet`; `/tree` immediately reports three turns. `totals` is initialized with `Totals::default()` regardless of resume (`crates/rune/src/session.rs:1004`), and `render_cost` uses it (`crates/rune/src/session.rs:2359`). Restore persisted usage rather than describing only requests since this process started as the whole session's requests.

#### TF-11: failed and cancelled partial replies disappear from persisted history

The `cancellation` fixture printed `STREAM-01` through `STREAM-05` and `[cancelled]`. Its `events.jsonl` has the user message `slow`, followed directly by the next user message `after cancelled`; there is no partial assistant message and no cancellation record. The provider-death fixture printed `STREAM-01` through `STREAM-03` and `[the turn failed: the response stream ended without a completion event]`; its log likewise omits the partial reply and failure record. Interactive errors return before `recorder.turn` (`crates/rune/src/session.rs:1244`, `crates/rune/src/session.rs:1255`), although `report_failed_turn` prints the partial text (`crates/rune/src/session.rs:2889`). Write interrupted outcome metadata and partial visible text when ending an exchange.

#### TF-12: process death preserves the prompt and lock recovery, but loses every streamed delta

The `killed` fixture sent `slow`, waited until three stream lines were visible, then sent SIGKILL to the actual Rune child. Shell capture reports `EXIT:137`. `resume last` reclaimed the writer lock and kept the same id `p1GltrC2adCt`; the next exact provider request contains `user: slow`, followed by `user: continue killed request`, with no assistant content. User prompt persistence works (`crates/rune/src/session.rs:1225`); stream checkpoints do not. Add periodic bounded stream checkpoints if README's "Sessions are written as they run" is intended to cover model text (`README.md:109`). SIGKILL cannot run a process's cleanup hooks; this case does not claim that Rune alone can restore terminal state after SIGKILL.

#### TF-13: model-controlled grep arguments abort the release process

Mock3 requests `grep_files` with `{"pattern":"x","path":"fixture.txt","context_lines":18446744073709551615}` against a one-line `x` file. `target/release/rune` aborts with capacity overflow and `EXIT:134`. `context_lines` is parsed without a maximum (`crates/rune-tools/src/grep_files.rs:224`) and used as a `VecDeque` capacity (`crates/rune-tools/src/grep_files.rs:331`); release uses panic abort (`Cargo.toml:147`). Reject the out-of-range value before allocation.

```text
thread '<unnamed>' (178634) panicked at /rustc/48a229ceaefd4985c50990b14116b6d85
6af0985/library/alloc/src/raw_vec/mod.rs:28:5:
capacity overflow
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
...
EXIT:134
```

The ellipsis excludes the shell's long diagnostic echo of the isolated launch command, not Rune output. `stty -a` before the panic had `-isig -icanon -echo`; after the release abort it had `isig icanon echo`, so the release panic hook did restore the tested terminal modes (`crates/rune-term/src/input.rs:407`).

#### TF-14: a caught worker panic is presented as user cancellation and bypasses the renderer

The debug binary catches the same worker panic, keeps the session open, and prints `[cancelled]`. The panic's raw stderr lands inside the live region, leaving the old prompt and status above the new status. A subsequent prompt succeeds. `run_on_worker` maps a failed join to `turn_interrupted()` (`crates/rune/src/session.rs:1516`), and the panic hook invokes the prior hook (`crates/rune-term/src/input.rs:415`). Report an internal tool failure, and route surviving-process diagnostics through the sole renderer.

#### TF-15: help output is clipped rather than wrapped

`/help` at 80 columns prints `/compact ... free the context win`, dropping `dow`, and `/status ... and sessio`, dropping `n`. `custom/help.txt` records these exact lines. Captured command output is split by source newline (`crates/rune/src/session.rs:2187`) then passed to Inline, which clips each settled row (`crates/rune-term/src/inline.rs:305`). Wrap finished command output at the actual width before committing it.

#### TF-16: custom slash commands are executable and listed in help but absent from completion

The isolated workspace contains `.rune/commands/audit-command.md` with body `Describe audit command files.`. `/help` lists `/audit-command (project) Describe audit command files.`. Typing `/audit` displays no completion row. The completion function matches only the builtin table (`crates/rune/src/session.rs:2073`, `crates/rune-term/src/commands.rs:137`), while discovered custom commands are held separately (`crates/rune/src/session.rs:952`). Include discovered commands in the same completion candidate set.

#### TF-17: a four-row terminal says to resize to continue while still sending prompts

`height4/initial.txt` displays `terminal is 4 rows, too small for the interface; resize to continue`. Typing `narrow height` sends a provider request, and `height4/answer.txt` returns to the same warning. The layout's `too_small` state changes footer content only (`crates/rune-term/src/footer.rs:172`); `await_submission` still reads and submits (`crates/rune/src/session.rs:1611`). Either pause submissions and genuinely wait for resize, or change the message to describe the usable compact mode.

#### TF-18: `/tree` has multiline previews inside its one-line table

In the populated resumed session, the assistant row for `unicode` continues as separate unindented `ASCII` lines, and the branch summary for the normal reply contains a separate `DONE` line. The `Node` field is documented as a one-line preview (`crates/rune-session/src/tree.rs:92`), but `bounded_preview` only limits character count (`crates/rune-session/src/tree.rs:766`). Normalize newlines and control characters before rendering tree previews.

### Passing observed paths and explicit limits

- Connection hides the key: searching `connect/hidden-key.scrollback.txt` for the literal dummy key `audit-connect-key` returned False. This matches the secret-input implementation (`crates/rune/src/connect_flow.rs:128`).
- After repairing the scratch state directory to 0700, a startup model picker was driven, filtered to `audit-model-12`, accepted, and produced a successful response with that model in the status. Captures: `first-picker/opened.txt`, `filtered.txt`, `answer.txt`; picker dispatch (`crates/rune/src/session.rs:1056`). This does not erase TF-01.
- The completion list reaches commands beyond the first six: Down eight times selected `/new`, `4-9 of 13`, before the resize in TF-05. Completion window code (`crates/rune/src/session.rs:2039`).
- A normal 60-line response is preserved in scrollback at 80 columns. Counting lines matching `^ROW-` in `interaction/long-answer.scrollback.txt` produced `60`; every one was unique. This matches finished transcript commit code (`crates/rune/src/session.rs:1281`).
- Wide CJK text, a face emoji, joined skin-tone emoji, a combining accent, and a flag were entered and rendered in `interaction/unicode-draft.txt` and `unicode-answer.txt`. The actual decoded strings remained `ASCII 世界 😀 👩🏽‍💻 é 🇩🇪 tail`. This establishes text preservation in tmux, not pixel glyph correctness in every terminal/font (`crates/rune-term/src/width.rs:64`).
- Bracketed paste kept `line one\nline two\tworld` in the composer until Enter, displayed `line one⏎line two world`, and sent one prompt containing the newline and tab. Captures: `paste/before-submit.txt`, `after-submit.txt`; event handler (`crates/rune-term/src/input.rs:182`).
- Ctrl-C first cleared a typed correction, then cancelled the empty-line streaming turn. A subsequent prompt succeeded without leaving the session. Captures: `cancellation/clear-draft.txt`, `cancelled.txt`, `after-cancel.txt`; handler (`crates/rune/src/session.rs:1456`). This does not erase the persistence issue in TF-11.
- HTTP 400, malformed SSE JSON, and a stream with no completion marker all produced a rendered error and allowed the next prompt. Exact errors include `fixture provider rejected audit request`, `stream frame is not valid JSON: key must be a string at line 1 column 2`, and `the response stream ended without a completion event`. Captures: `errors/http-error.txt`, `malformed.txt`, `provider-died.txt`, `after-error.txt`; interactive error recovery (`crates/rune/src/session.rs:1244`).
- A model absent from the endpoint listing and builtin catalog, `not-in-endpoint-or-table`, accepted a prompt and returned a successful fixture reply. Capture: `unknown-model/answer.txt`; unknown catalog metadata (`crates/rune-net/src/catalog.rs:40`).
- Offline interactive invocation rendered `outbound requests are disabled` and the configured model stayed in the footer. Capture: `offline/refusal.txt`; endpoint offline flag (`crates/rune-net/src/transport.rs:144`). No claim is made here about every network path; the egress audit below states those limits.
- With no `bwrap` on PATH, a valid shell call refused with `no usable sandbox backend is on this host: the bwrap helper is not on PATH on linux`. With `--allow-unsandboxed`, the same command returned `AUDIT_SHELL_OK` and exit status zero. Captures: `sandbox-fixed/sandbox.txt`, `shell-unsandboxed/answer.txt`. Actual Linux sandbox enforcement remains untested in these PTYs because the helper was absent.
- Up recalls the most recent workspace prompt `resume continued`; Down returns to the empty draft. Captures: `history-recall/up.txt`, `down.txt`; history navigation (`crates/rune/src/session.rs:1685`).


### Captured terminal photographs

These PNGs render the actual tmux grids above. They are embedded so this remains a one-file deliverable. Renderers that block data URIs can extract the embedded PNG bytes; the exact text captures remain readable above.

<img alt="80 columns, 108 typed chars; TAIL-END invisible; caret col 79" src="data:image/png;base64,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" />

Figure long-draft, crop from `interaction/long-draft.txt`, `80x24 cursor=79,2 history=20`.

<img alt="32x8 picker after Down: input and footer off screen" src="data:image/png;base64,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" />

Figure picker-height, crop from `picker/short-after-key.txt`, `32x8 cursor=2,0 history=30`.

<img alt="Fresh install: connection succeeds, session exits 1" src="data:image/png;base64,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" />

Figure fresh-connect, crop from `fresh-clean/failure.txt`, `80x24 cursor=0,23 history=14`.
## Composition, correctness and external interfaces

### The one-shot command is a chat request, not the shared agent loop

`ask::run` constructs only one user message and invokes `stream_completion` once (`crates/rune/src/ask.rs:183`, `crates/rune/src/ask.rs:205`). It does not advertise tools, load workspace instructions, execute a tool, retry through the agent policy, or create a session. Nevertheless it maps every returned tool call to `status: "success"` and accepts a ToolCalls finish as exit zero (`crates/rune/src/ask.rs:216`, `crates/rune/src/ask.rs:227`). Both branches of `no_save` return an empty session ID (`crates/rune/src/ask.rs:239`), although `--no-save` is documented as suppressing session creation (`COMMANDS.md:37`). `review` reuses this path (`crates/rune/src/main.rs:323`).

A real release-binary probe created an isolated AGENTS file containing `AUDIT_INSTRUCTION_MUST_BE_SENT`, returned a shell call from a local SSE provider, and then listed sessions. Exact command: `python3 /tmp/rune-audit-root/ask-wiring-probe.py`. Real output:

```text
{"argv": ["rune", "ask", "--json", "run the fixture command"], "exit": 0, "stdout": "{\"output\":\"\",\"final_output\":\"\",\"exit_code\":0,\"model\":\"audit-model\",\"resolved_provider\":null,\"session_id\":\"\",\"steps\":1,\"usage\":{},\"tool_calls\":[{\"name\":\"shell\",\"status\":\"success\"}]}\n", "stderr": ""}
{"argv": ["rune", "sessions", "--json"], "exit": 0, "stdout": "{\n  \"sessions\": [],\n  \"next\": null\n}\n", "stderr": ""}
REQUEST_COUNT 1
REQUESTS [{"model": "audit-model", "stream": true, "stream_options": {"include_usage": true}, "messages": [{"role": "user", "content": "run the fixture command"}]}]
```

The request contains neither the instruction nor any schemas, there is no second request with a tool result, and no saved exchange. Repair truthful status first; then connect a bounded tool host, instructions, retries and persistence as separate changes. The comparable fx runtime test below actually executes a file-read call and saves a session.

### Advertised interactive tools with unavailable backends

The interactive composition installs `Subagent::unsupported`, not a child runner (`crates/rune/src/session.rs:3018`). The built-in inventory installs an unavailable question answerer, an empty skill catalog and unconfigured vision (`crates/rune-tools/src/inventory.rs:110`, `crates/rune-tools/src/inventory.rs:115`, `crates/rune-tools/src/inventory.rs:136`). Interactive setup advertises and retains that registry (`crates/rune/src/session.rs:973`, `crates/rune/src/session.rs:981`); the only extra insertion is the unsupported subagent. Questions were reproduced in TF-08. Skill, vision and child-runner failures are source findings, with end-to-end fixture checks in the list. Do not describe those library implementations as usable CLI features until a configured host reaches them.

MCP transports, trust and tool selection exist under `rune-context`, but none is composed into these CLI/ACP/SDK run paths. The search `rg -n 'mcp::' crates/rune/src crates/rune-sdk/src crates/rune-acp/src` returned no matches. ACP explicitly discards client MCP definitions (`crates/rune-acp/src/server.rs:547`). The gap is runtime integration before management UX or OAuth.

### Configuration boundaries and misleading resolved values

The README's `offline = true` user-file example is rejected because `UserConfig` has no offline field (`README.md:65`, `crates/rune-core/src/config.rs:333`). The library probe below records the unknown-field diagnostic and effective offline false. Model/provider overrides can keep the previous model's capacity, including CLI overrides: `cli::apply_to_settings` replaces strings without recomputing their derived fields (`crates/rune/src/cli.rs:592`); the environment path behaves similarly (`crates/rune-core/src/config.rs:1230`). Legacy invalid project limits silently discard their validation error and can fall back to zero, meaning unlimited (`crates/rune-core/src/config.rs:1140`, `crates/rune-core/src/budget.rs:378`).

The default `web_tools` setting is true and constructs clients online (`crates/rune-core/src/config.rs:551`, `crates/rune/src/web_client.rs:94`), while README says the web tools are off until asked for (`README.md:62`). The composition adds user-layer allow rules whenever that default true value is online (`crates/rune/src/permissions.rs:40`), overriding compiled denials without an opt-in. This is an observed mismatch: in a workspace with an absent user config, `rune permissions web_fetch example.com` exited zero and printed the following actual output:

```text
web_fetch `example.com`: allowed (allow: matched `web_fetch *` at the user layer)
```

 Set an explicit opt-in default or correct the declared contract; the list chooses the documented opt-in. Offline prevents model transport and disables web backends (`crates/rune-net/src/transport.rs:144`, `crates/rune/src/web_client.rs:94`), but tool context has no offline property and shell networking follows `external_access` (`crates/rune-tools/src/shell.rs:530`). No live offline-shell escape was attempted; offline model refusal prevents an ordinary new model call. The list calls for an independently tested shell egress boundary.

`python3 /tmp/rune-audit-root/config-flags-probe.py` exercised fifteen global-flag cases through the actual release binary. These are selected decoded fields from real stdout, not fabricated expected values:

```text
rune --model small-audit-model config --json: exit 0, provider=anthropic (user), model=small-audit-model (command_line), context_window=2000000 (user)
rune --provider openai config --json: exit 0, provider=chat_completions (command_line), model=large-audit-model (user), context_window=2000000 (user)
rune --effort high config --json: exit 0, effort=high (command_line)
rune --fast config --json: exit 0, fast_mode=true (command_line)
rune --no-fast config --json: exit 0, fast_mode=false (command_line)
```

A project file attempted `base_url`, `api_key_env`, `permission_mode="full-access"` and `theme`. `rune config --json`, exit zero, kept effective default/user values and emitted `key_not_allowed_in_scope` for each of those four keys. This boundary passed; retain it (`crates/rune-core/src/config.rs:1465`). The exact project and flag harness is included below.

### Installer reproductions

`python3 /tmp/rune-audit-root/runtime-probes.py` built a disposable tar.gz with a `rune` executable, verified its checksum, and passed it to the actual `upgrade` command. Rune installed the archive bytes verbatim (`crates/rune/src/install.rs:154`), although release staging publishes tar.gz (`xtask/src/main.rs:239`). It reported success but the installed target was not executable:

```text
rune upgrade --from /tmp/rune-audit-root/runtime/fixture.tar.gz --checksum 91378ba5d0b414cd9af36ff7209050aad263d11b08a86752159e389c6b4ef670 --target /tmp/rune-audit-root/runtime/installed --json
exit: 0
{"target":"/tmp/rune-audit-root/runtime/installed","checksum":"91378ba5d0b414cd9af36ff7209050aad263d11b08a86752159e389c6b4ef670","bytes":135}
installed_magic 1f8b080856c6c26a
installed_exec_error 8 Exec format error
```

The JSON line above is a whitespace-compacted transcription of real stdout. The fixture archive timestamp/checksum varies on repetition. Installation should distinguish raw executable input from a verified release archive and extract exactly its permitted member.

The same harness put a symlink at the fixed staging filename, pointing to another disposable file. Upgrade followed it, overwrote the victim and renamed the symlink to the target. `File::create` uses the predictable `.rune-install-staged` path (`crates/rune/src/install.rs:150`, `crates/rune/src/install.rs:178`). Actual output after the successful raw-binary upgrade:

```text
victim_content '#!/bin/sh\necho STAGED\n' target_is_symlink True
```

No installed production binary or real user file was touched. Create an exclusive unique staging file, refuse symlink traversal and preserve the original target on all failures.

### ACP process findings

The runtime harness started `target/debug/rune acp --log-file /tmp/rune-audit-root/runtime/acp.log` in workspace A, sent line-delimited JSON-RPC and captured actual replies. Selected exact request/reply fields:

```text
request: session/new, cwd=/tmp/rune-audit-root/runtime/workspace-b, mcpServers=[]
response: sessionId=5AxTNaqnI4Lh
request: initialize, protocolVersion=999
response: protocolVersion=1
request: session/list
response: sessionId=5AxTNaqnI4Lh, cwd=/tmp/rune-audit-root/runtime/workspace-a
request: session/set_config_option, configId=model, value=another-unknown-model
response: currentValue=another-unknown-model
request: session/close
response: {}
```

The requested workspace is validated then ignored (`crates/rune-acp/src/server.rs:544`), and the host derives context from a fixed workspace (`crates/rune-acp/src/session.rs:394`). List also labels every session with that workspace, filters none by recorded workspace, and caps before sorting (`crates/rune-acp/src/session.rs:283`, `crates/rune-acp/src/session.rs:288`). Requested stdio MCP servers are ignored with only a diagnostic note (`crates/rune-acp/src/server.rs:547`), contrary to the [ACP session setup contract](https://agentclientprotocol.com/protocol/v1/session-setup). These are separate fixes.

Accepting `session/new` before initialization shows an unchecked lifecycle. Returning supported version 1 to a client proposing 999 is not itself a defect, because version selection is part of [ACP initialization](https://agentclientprotocol.com/protocol/v1/initialization). Test compatibility and state explicitly. Reopening restores defaults instead of saved model/effort and ignores supplied roots (`crates/rune-acp/src/session.rs:264`). Model changes do not update the fixed context-window value used in notifications (`crates/rune-acp/src/session.rs:352`, `crates/rune-acp/src/server.rs:1149`). Output-write failures only log, and unanswered permissions have no independent deadline (`crates/rune-acp/src/server.rs:377`, `crates/rune-acp/src/server.rs:785`). Those last paths were source reviewed without disconnect/deadline fault injection; the list supplies observable checks.

### SDK, Node and WASM

SDK runs a separate tool loop and directly fetches once per step (`crates/rune-sdk/src/agent.rs:968`, `crates/rune-sdk/src/agent.rs:1134`), rather than using the native loop's retry policy. Its configured BudgetSet is copied, but step enforcement uses `PromptOptions.max_steps` directly (`crates/rune-sdk/src/agent.rs:757`, `crates/rune-sdk/src/agent.rs:977`). The event channel is unbounded (`crates/rune-sdk/src/agent.rs:744`); checkpoint serialization has no matching guard for the restoration cap (`crates/rune-sdk/src/agent.rs:837`, `crates/rune-sdk/src/agent.rs:842`). These are code findings with caller-visible acceptance cases. A wholesale abstraction merger would risk existing host contracts; add parity cases and share narrow policy helpers first.

`node /tmp/rune-audit-root/node-probes.mjs` ran three subprocess-binding probes. Actual output:

```text
incomplete-result { output: 'ok', exit_code: 0 }
relative-cwd BINARY_NOT_FOUND   ./rune is not an executable file
(node:190704) TimeoutOverflowWarning: Infinity does not fit into a 32-bit signed integer.
Timeout duration was set to 1.
(Use `node --trace-warnings ...` to show where the warning was created)
infinite-timeout TIMEOUT
```

The incomplete object is accepted although required typings include more fields (`bindings/node/index.js:315`, `bindings/node/index.d.ts:86`). Relative `bin` resolution happens before applying `cwd` (`bindings/node/index.js:136`), so a valid executable in that cwd is not found. Infinity passes validation and becomes a 1 ms timer (`bindings/node/index.js:132`, `bindings/node/index.js:170`). The capture buffers have no limit (`bindings/node/index.js:154`). Windows PATH search names `rune` without `.exe` (`bindings/node/index.js:103`, `bindings/node/index.js:281`); that finding was not run on Windows.

The native unsafe boundary is resource-limit setup in `pre_exec`, with a syscall-only contract (`crates/rune-exec/src/command.rs:758`). WASM has a separate unsafe FFI surface, explicitly permitted locally (`crates/rune-web/src/exports.rs:16`). Its page bridge trusts a host-returned body length without checking against buffer capacity (`crates/rune-web/src/exports.rs:79`) and allocates from a staged length without a byte cap (`crates/rune-web/src/exports.rs:47`). No browser host or wasm target was executed. Add malformed-host boundary tests and a CI smoke run; do not infer browser safety from native tests. The workspace comment about one unsafe call site must be scoped to native code (`Cargo.toml:115`).

## Commands and documentation versus the binary

The isolation harness ran 71 CLI cases, including help for all 24 named command spellings, real dispatch for the documented top-level commands, and scratch-only project/workspace/install operations: `python3 /tmp/rune-audit-root/cli-audit.py`. Exact summary was `records 71`, `reference_matches True`. Generated `rune reference --write /tmp/rune-audit-root/cli/COMMANDS.md` matched committed `COMMANDS.md` byte-for-byte. This verifies generation consistency, not the implementation of each advertised flag.

CLI observations below use `target/debug/rune` in a credential-free scratch workspace. Successful interactive connection/model selection/resume/history cases are in TF-01 through TF-18. Configured ask and ACP are above. Help alone is marked as help coverage, never as successful provider operation. Paid Anthropic/opencode-go connections, the Homebrew installer and Git-based install commands were not executed. The README's source build was exercised by the release gate, and its doctor command by the dispatch harness (`README.md:31`).

| Surface | Actual non-help command or terminal scenario | Result and scope |
|---|---|---|
| interactive | rune | Fresh first-run and successful session after scratch state repair, TF-01; streaming, cancellation, narrow terminal and resize in TF-02 through TF-18. |
| ask | rune ask --json hello | Credential-free refusal, exit 1; configured SSE success and fake tool success reproduced above. |
| acp | rune acp --log-file /tmp/rune-audit-root/runtime/acp.log | Real stdio lifecycle replies, then normal process exit; cwd mismatch reproduced above. |
| review | rune review | Exit 0 in an empty non-repository fixture; no changed-file/provider review established by this case. |
| connect | rune connect anthropic --json | Exit 1: authentication_required: no credential found for provider anthropic; valid compatible-provider setup was driven in a PTY, TF-01. |
| sessions | rune sessions --all --limit 2 --json | Exit 0, empty page in isolated state; populated sessions exercised by the PTY suite. |
| session | rune session last --json | Exit 1, expected 12 characters, found 4. migrate/recover reject bad IDs; legacy migration against a real legacy log was not executed. |
| tree | rune tree last --json | Exit 1 for documented last selector; populated interactive /tree exposes TF-18. |
| usage | rune usage --period 7d --json | Exit 0 but period is 24h; no seven-day ledger fixture was created. |
| auth | rune auth status --json | Exit 1, status is not an action; auth logout exits 0 in isolated empty credential store. |
| models | rune models --offline --json | Exit 0 with offline catalog; online local catalog/picker exercised in first-picker PTY case. |
| permissions | rune permissions --explain run_command:pwd --json | Exit 0 with full rules rather than one explanation; actual terminal ask mode refuses without asking, TF-07. |
| projects | rune projects status --json; rune projects approve --json; rune projects reject --json; rune projects reset --json | All exit 0 in the isolated fixture; no untrusted project MCP execution was established. |
| config | rune config --explain --json | Exit 0 with values, sources, layers and diagnostics; user-only project keys separately rejected in config-flags probe. |
| limits | rune limits --json | Exit 0, declared limits and sources rendered; declaration is not proof each knob is wired, see deadline findings. |
| workspace | rune workspace list --json; rune workspace add <scratch> --json; rune workspace remove <scratch> --json; rune workspace clear --json | All exit 0, but clear returns its stale prior directory list. |
| prompt | rune prompt --show | Exit 0 and resolved instructions printed; ask sends only the user message despite this inspectable prompt. |
| status | rune status --json | Exit 0 with provider/configuration status in isolated state. |
| doctor | rune doctor --json | Exit 1, sandbox helper unavailable and Rune-created nonprivate state detected in this host fixture. |
| upgrade | rune upgrade --from /tmp/nonexistent --checksum a --target <scratch> --json | Exit 1 for absent input; successful verified archive and staging-symlink repros above are the meaningful installation checks. |
| uninstall | rune uninstall --target <scratch> --keep-state --json; rune uninstall --target <scratch> --yes --json | Exit 0 on disposable absent target; removal of a live installed Windows binary was not run. |
| reference | rune reference --write /tmp/rune-audit-root/cli/COMMANDS.md | Exit 0; generated bytes equal COMMANDS.md. |
| help | rune help not-real; rune --help resume | Both exit 0 while reporting unknown command; resume actually exists outside the shared specification. |
| version | rune version | Exit 0, rune 0.1.16 (dev, 44a69bcc1400). |
| resume | rune resume last | Exit 1, no session has been saved yet; real populated resume succeeds in PTY suite, with context/cost and transcript gaps TF-09 through TF-12. |

### Confirmed flag and reference discrepancies

- `rune usage --period 7d --json`: exit 0, JSON `"period": "24h"`. `crates/rune/src/main.rs:809` reads first positional, not `launch.flag("--period")`.
- `rune auth status --json`: exit 1, `rune: invalid_field: status is not an action for auth`; hint names auth/remove. `crates/rune/src/main.rs:473-490` supports None/remove/logout but no status.
- `rune session last --json` and `rune tree last --json`: exit 1, `invalid_field: expected 12 characters, found 4 (field session_id)`. `crates/rune/src/main.rs:753-755,790` parses literal; `--id` value also ignored.
- `rune permissions --explain run_command:pwd --json`: exit 0, prints full rule list instead of explanation. `crates/rune/src/main.rs:1008-1021` uses positionals, never reads --explain; positional explanation emits text even with --json.
- `rune help resume`: exit 0, `rune: resume is not a command`. `crates/rune/src/cli.rs:329` accepts it, `crates/rune/src/spec.rs:156-430` omits it. Parser recognizes review/pr/issue, connect/login/setup/provider, usage/cost, auth/logout, config/settings, while table aliases empty. `crates/rune/src/cli.rs:308-318`.
- `rune --effort banana config --json`: exit0 and effort auto/default without diagnostic. `crates/rune/src/cli.rs:601-604`. Invalid permission mode similarly discarded at `crates/rune/src/cli.rs:609-615`.
- `rune sessions --all=no`: exit0, no sessions stored. Declared boolean flags retain inline values unchecked at `crates/rune/src/cli.rs:493-508`; has_flag ignores values at `crates/rune/src/cli.rs:173-174`.
- `rune ask --json` with empty stdin: exit1, stdout empty, missing_field prompt stderr. JSON failure promise `crates/rune/src/main.rs:236-239` bypassed by checks `crates/rune/src/main.rs:246-255`.
- `rune ask --json hello` without provider: exit1, JSON failure, duplicate stderr: `rune: no model provider is connected` followed by `rune: authentication_required: no model provider is connected`. `ask::report` plus `crates/rune/src/main.rs:273-275` reporting.
- `workspace add /tmp/... --json` then `workspace clear --json`: clear output still lists added path; file cleared. `crates/rune/src/main.rs:1370,1378` writes empty list without clearing in-memory vector.
- Config reading honors RUNE_CONFIG `crates/rune/src/main.rs:119-125`, writing ignores it: crates/rune/src/provider_setup.rs:83,123 and crates/rune/src/main.rs:1318. A workspace mutation writes a different file from one config command reports. Run a fixture custom config to verify.

The README's native session, ask, review, resume, config, limits, prompt, doctor and model-listing entry points were exercised as indicated above (`README.md:42`, `README.md:100`, `README.md:140`). Provider-specific paid connection success was not tested. Help accepts connection spellings, but acceptance alone does not establish authentication or provider semantics. The generated reference omits resume and accepted aliases, while its handler-specific defects survive byte-for-byte regeneration (`crates/rune/src/cli.rs:308`, `crates/rune/src/spec.rs:156`).

## Deep code audit

### High value findings

1. Reasoning effort is discarded in every dialect. `RequestPlan.effort` exists (`crates/rune-net/src/provider.rs:57`) and the loop sets it (`crates/rune-agent/src/turn.rs:559`), but none of the three request builders reads it (`crates/rune-net/src/anthropic.rs:54`, `crates/rune-net/src/chat_completions.rs:66`, `crates/rune-net/src/responses.rs:52`). Conversion helpers are called only by unit tests (`anthropic.rs:886`, `chat_completions.rs:975`, `responses.rs:910`). Those tests verify a mapping that never reaches the wire. Confirmed by scratch probe: Auto and High build identical request JSON in all three dialects.
2. The total provider deadline is configuration without enforcement. `ProviderRequestTimeoutMs` is declared and advertised (`crates/rune-core/src/budget.rs:232`, `budget.rs:506`), but `rg -n ProviderRequestTimeoutMs crates` finds no consumer. Model requests set only head timeout (`crates/rune-net/src/transport.rs:508`), and the transport agent has no global timeout (`transport.rs:309`). A body that stays open can hold a request indefinitely. Code confirmed; the silent-body repro below also demonstrates no enforced total bound.
3. A silent body also defeats the head timeout. The receive-timeout branch in native `feed` does nothing (`crates/rune-net/src/transport.rs:851`). The only elapsed-time test runs when a chunk arrives in `StreamState::push` (`transport.rs:936`), and it additionally requires `decoder.is_idle()`. A server that sends headers and becomes silent is never timed out by the stream layer. Confirmed: a 50 ms head budget returned IncompleteStream after 301 ms, following delayed EOF.
4. Cancellation detaches a reader rather than ending its socket. A helper owns the `Read` and the parent returns cancellation without joining or interrupting it (`crates/rune-net/src/transport.rs:819`, `transport.rs:840`). The doc explicitly admits that a permanently blocked read survives until connection close (`transport.rs:804`). Repeated cancellations can leave one thread and connection per attempt. No controlled many-cancellation resource-count test was found in this module (`transport.rs:1025`).
5. Mid-failure steering is not applied to retries. `stream_with_retry` builds every attempt from borrowed immutable history (`crates/rune-agent/src/turn.rs:529`, `turn.rs:546`); only after successful completion does the caller apply the queued steering (`turn.rs:512`). The comment at `turn.rs:592` says steering can change the request, but it cannot do so while failures continue. Code deduced; test should inject a correction on attempt one and inspect attempt two.
6. Offline becomes a retryable network error in the transport (`crates/rune-net/src/transport.rs:489`, `crates/rune-net/src/error.rs:81`), so an embedder using the generic turn host pays the retry delay even though no network attempt can succeed (`crates/rune-agent/src/turn.rs:590`). The CLI may have an earlier refusal; this is a library-layer finding.
7. Tool batching is sequential. `execute_batch` loops over calls and synchronously invokes `host.execute` (`crates/rune-agent/src/turn.rs:651`, `turn.rs:748`), despite promising concurrent front-of-batch reads (`turn.rs:628`) and a configured `parallel_tool_calls` advertised as concurrency (`crates/rune-core/src/budget.rs:503`). This knob is used as a live shell session limit (`crates/rune-tools/src/shell.rs:225`) instead. The turn builder also leaves `RequestPlan.parallel_tool_calls` at true even with a configured limit of one (`turn.rs:543`, `crates/rune-net/src/provider.rs:83`). Code confirmed.
8. Automatic compaction is unwired in the real CLI. `rg -n 'compact|Estimate|usable_input' crates/rune/src/session.rs` finds only slash-command compaction at `session.rs:1156` and `compact_history` at `session.rs:1827`, with no automatic capacity estimate. `turn::run_turn` likewise has no trigger before requests (`crates/rune-agent/src/turn.rs:349`). The `compaction_trigger_percent` user setting is currently unused by the product (`crates/rune-agent/src/compaction.rs:235`).
9. The capacity helper infers that a retained tail cannot fit without measuring a retained tail. It maps any full estimate above capacity to `OverCapacity` (`crates/rune-agent/src/tokens.rs:234`), and `compaction::trigger` maps that to `Impossible` (`compaction.rs:240`). A long removable prefix and a small tail should remain compactable. Code deduced, and currently masked by the disconnected trigger.
10. README `offline = true` cannot be saved in the user file. `UserConfig` has `deny_unknown_fields` and no offline field (`crates/rune-core/src/config.rs:333`). Environment offline exists (`config.rs:781`, `config.rs:1308`). A TOML user file containing the documented setting is rejected wholesale by `read_user` (`config.rs:895`). Confirmed by the scratch output below.
11. Environment model or provider override can keep the previous model's capacity. User model sets capacity (`crates/rune-core/src/config.rs:1058`), then environment changes provider or model without resetting capacity (`config.rs:1230`, `config.rs:1234`). This can claim a two-million-token window for a different small model. Confirmed by the scratch output below.
12. Legacy top-level project caps swallow validation errors. Both assignments discard `BudgetSet::set` errors (`crates/rune-core/src/config.rs:1140`, `config.rs:1148`), whereas `[limits]` emits a diagnostic (`config.rs:1208`). `max_agent_steps = 10001` silently falls back to the default zero, meaning unlimited (`crates/rune-core/src/budget.rs:378`). Confirmed by the scratch output below.
13. Web address vetting recognizes one-part and four-part IPv4 but omits two and three parts (`crates/rune-tools/src/web.rs:458`). Therefore `127.1`, `127.0.1`, and `10.1` reach a backend with `allow_private` absent (`web.rs:332`, `web.rs:1096`). DNS names are judged only as strings and never by resolved destination (`web.rs:420`). Real DNS names can resolve private addresses. Confirmed with a recording backend and separate getaddrinfo output below; no live private HTTP server was contacted.
14. `allow_private` is selected by the model, while permission target includes only the domain (`crates/rune-tools/src/web.rs:1060`, `web.rs:1082`, `web.rs:1096`). A policy/approval review that sees only the target cannot distinguish a private-network opt-in. Existing default web enablement is a user setting, but private access has no separate grant at this boundary.
15. Sandbox protects only paths already present, and only directly under each writable root. `PROTECTED_REPOSITORY_PATHS` contains `.git/config` and `.git/hooks` (`crates/rune-exec/src/sandbox.rs:82`), and `protected_paths` skips unresolved names (`sandbox.rs:328`). Creating a previously absent hooks directory or configuring a nested repository stays writable. Confirmed for generated sandbox argv. A live kernel enforcement mutation test remains unrun.
16. A custom tool's long Unicode description can panic during registration. `model_spec` calls `String::truncate(1008)` without checking a character boundary (`crates/rune-tools/src/contract.rs:351`), and `Registry::insert` calls it (`registry.rs:45`). A leading ASCII byte followed by 600 `é` characters splits an `é` at byte 1008. Confirmed by the scratch output below.
17. `grep_files.context_lines` can allocate arbitrary capacity. The value is parsed without a maximum (`crates/rune-tools/src/grep_files.rs:224`) and passed directly to `VecDeque::with_capacity` (`grep_files.rs:331`). `u64::MAX` on the audited 64-bit host should panic with capacity overflow before reading content. Confirmed by the scratch output below.
18. Grep matches its own truncation annotation. `scan` truncates a source line into `shown`, then searches `shown`, not the original bytes (`crates/rune-tools/src/grep_files.rs:355`, `grep_files.rs:362`). A source with no `line truncated` phrase is reported as a match if its line exceeds one MiB; a true literal match after the one-MiB head is not found. This contradicts exact count descriptions (`grep_files.rs:100`, `crates/rune-tools/src/workspace.rs:44`). Confirmed by the scratch output below.
19. File reads do not check cancellation. `ReadFile::call` and its full-file counting `collect` have no `context.check_cancelled`, even before opening (`crates/rune-tools/src/read_file.rs:91`, `read_file.rs:220`). It scans the entire file even for a one-line page (`read_file.rs:129`, `read_file.rs:239`). Grep checks cancellation between files, not during a giant single file (`grep_files.rs:141`, `grep_files.rs:336`).
20. Tool validation accepts unknown properties despite each advertised closed schema. Default `Tool::validate` checks only required names (`crates/rune-tools/src/contract.rs:324`), file tool calls independently ignore additional keys (`crates/rune-tools/src/read_file.rs:95`). A misspelled optional argument produces a successful result with defaults. Skill tools contain a separate closed-field checker (`crates/rune-tools/src/skill.rs:63`), so the behavior is also inconsistent.
21. Registry converts errors to text and drops repair hints, stable code, and observed value (`crates/rune-tools/src/registry.rs:134`, `registry.rs:143`). The outer turn attempts to preserve hints (`crates/rune-agent/src/turn.rs:755`), but it never sees the swallowed registry error. Confirmed by the scratch output below.
22. Retained-result implementation is dormant. `rg -n 'read_tool_result|\.retain\(' crates/rune-tools` finds no production caller of `result_store::Store::retain`; the advertised inventory has no `read_tool_result` (`crates/rune-tools/src/inventory.rs:26`). The turn truncates result bodies without retaining a handle (`crates/rune-agent/src/turn.rs:459`). The README command reference describes spilling as the result cap's behavior (`COMMANDS.md:220`), but the user cannot inspect lost output.
23. Existing `Store::retain` caps retained content at `MaxToolResultBytes` and appends a marker past the nominal cap (`crates/rune-tools/src/result_store.rs:223`). Re-inserting an identical handle increments `total_bytes` again without subtracting replaced content (`result_store.rs:237`). Small previews derive a handle without storing an entry (`result_store.rs:193`), while `Preview::render` always tells the model to read that handle (`result_store.rs:132`). These library behaviors should be fixed before wiring it in.
24. Starting a new shell command removes every completed session, including unread final output (`crates/rune-tools/src/shell.rs:114`). A command returned as a live session can finish after its first yield; starting any other command then makes its last output inaccessible. Confirmed: starting OTHER made the unread LAST session return NotFound.
25. Session append can return failure after the event became durable. `write_all` and `sync_data` precede projection update and a metadata write that uses fallible `write_private` (`crates/rune-session/src/store.rs:337`, `store.rs:362`). A failing disposable projection should not make callers uncertain whether to retry a durable event. Projection writes also truncate in place (`crates/rune-core/src/paths.rs:420`) rather than replacing atomically.
26. Session writer release clears holder metadata after unlocking (`crates/rune-session/src/store.rs:402`). A new owner can acquire the lock and write its holder between those operations, then have the previous owner erase its identity. The lock still excludes writes; this is an incorrect diagnostic race, not lock bypass.
27. The event log checks size before an unbounded `std::fs::read` (`crates/rune-session/src/event.rs:407`). A concurrent growing log can exceed the announced size bound between metadata and read. Session lock-free readers are expected (`crates/rune-session/src/store.rs:13`), so a capped read is appropriate.
28. Credential writes have neither an advisory lock nor atomic replacement. `auth::store` and `remove` perform read/modify/write over shared profile JSON (`crates/rune-net/src/auth.rs:184`, `auth.rs:207`) through truncating `write_private` (`crates/rune-core/src/paths.rs:420`). Two connect processes can lose one another's additions, or a killed writer can destroy all connected credentials. `Paths::credentials_lock` is unused (`crates/rune-core/src/paths.rs:227`).
29. Settings helper is described as atomic but truncates its authority in place (`crates/rune-policy/src/settings.rs:176`, `settings.rs:218`, `crates/rune-core/src/paths.rs:420`). Its create-new lock remains on disk after SIGKILL (`settings.rs:80`, `settings.rs:117`), so every later writer waits two seconds then fails. No OS lock owns that lock lifetime. Confirmed: SIGKILL left the file, and reacquiring failed after 2008 ms.
30. Generic I/O error conversion labels disk errors as `transport_failure` (`crates/rune-core/src/error.rs:356`), including write failures that matter during session or credential persistence. `NetError::to_rune_error` drops provider_code and retry_after (`crates/rune-net/src/error.rs:229`), weakening JSON troubleshooting. `RuneError` implements `std::error::Error` with no source (`crates/rune-core/src/error.rs:350`), so typed cause chains are unavailable.
31. Endpoint derives `Debug` with a bare credential string (`crates/rune-net/src/transport.rs:76`, `transport.rs:81`), unlike the credential wrapper's deliberately redacted debug (`crates/rune-net/src/auth.rs:100`). This creates a library diagnostic leak. Confirmed by the scratch output below.
32. URL validation does not verify a nonempty authority or reject all whitespace (`crates/rune-net/src/transport.rs:196`, `transport.rs:200`). `https:///`, `https://?broken`, and a URL containing a newline pass this validator. Confirmed by the scratch output below.
33. Session tree repeats the provider `Role` enum (`crates/rune-session/src/tree.rs:39`, `crates/rune-net/src/message.rs:22`), with another role-to-string mapping. Tool names are repeated in activity inference despite registry activities (`crates/rune-agent/src/turn.rs:777`, `crates/rune-tools/src/registry.rs:151`); custom Read tools are displayed as Execute. This is concrete drift, not a request to abstract every repeated line.

### What to retain

The conversation pairing validator checks duplicate IDs, orphan results, and missing results with named invariants (`crates/rune-net/src/message.rs:255`, `message.rs:271`). The session log keeps a valid prefix, reports torn tails, and refuses sequence gaps (`crates/rune-session/src/event.rs:497`, `event.rs:503`). Mutations stage a file, sync it, revalidate identity and hash, then rename (`crates/rune-tools/src/mutation.rs:420`). The command environment is an allowlist (`crates/rune-exec/src/command.rs:293`). Those are useful boundaries, each backed by nearby tests.

The production-prefix search for `unwrap(`, `expect(`, `panic!`, and `unsafe {` in the reviewed native library crates found one unsafe site, `pre_exec` resource-limit setup at `crates/rune-exec/src/command.rs:758`, with an explicit async-signal-safe syscall-only contract. Other apparent `unwrap` hits are the command parser's function named `unwrap`, not Option or Result panics. The confirmed panic candidates are container/string APIs rather than `.unwrap()`.

### Confirmed scratch probe output

Commands: `python /tmp/rune-code-compile.py`, `/tmp/rune-code-probe > /tmp/rune-code-probe.out`, `python /tmp/rune-code-compile2.py`, `/tmp/rune-code-probe2 > /tmp/rune-code-probe2.out`. Both final probe executions exited zero. The compiler selected completed workspace rlibs without invoking Cargo or changing source.

```text
unicode_description_panics=true
grep_context_capacity_panics=true
anthropic effort_auto_equals_high=true request={"model":"fixture-model","max_tokens":8192,"stream":true,"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}
chat_completions effort_auto_equals_high=true request={"model":"fixture-model","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hello"}]}
responses effort_auto_equals_high=true request={"model":"fixture-model","store":false,"stream":true,"instructions":"You are a helpful assistant.","input":[{"role":"user","content":[{"type":"input_text","text":"hello"}]}],"include":["reasoning.encrypted_content"]}
web host=127.1 local=false backend_called=1 is_ok=true
web host=10.1 local=false backend_called=2 is_ok=true
web host=127.0.1 local=false backend_called=3 is_ok=true
web host=127.0.0.1.nip.io local=false backend_called=4 is_ok=true
grep needle=hiddenneedle result=[0 matching lines in 0 files, searched 1 files under `long.txt`] | 
grep needle=line truncated result=[1 matching lines in 1 files, searched 1 files under `long.txt`] | 
read_cancelled_call_ok=true
read_unknown_field_call_ok=true
registry_error_output=missing
config label=offline offline=false model= context=None step_limit=Bounded(0) diagnostics=[Diagnostic { layer: User, path: Some("/tmp/rune-code-fixture/config.toml"), code: InvalidConfiguration, key: None, message: "could not parse: TOML parse error at line 1, column 1\n  |\n1 | offline = true\n  | ^^^^^^^\nunknown field `offline`, expected one of `provider`, `web_tools`, `models`, `base_url`, `api_key_env`, `permission_mode`, `effort`, `fast_mode`, `theme`, `auto_upgrade`, `collapse_tool_calls`, `session_titles`, `context`, `additional_directories`, `limits`, `permission`, `review_model`, `provider_order`, `provider_strict`\n", hint: Some("run `rune doctor` for the resolved configuration") }]
config label=context_env offline=false model=small context=Some(2000000) step_limit=Bounded(0) diagnostics=[]
config label=steps_invalid offline=false model= context=None step_limit=Bounded(0) diagnostics=[]
endpoint_debug=Endpoint { base_url: "https://example.com", credential: "this-is-a-real-secret", auth: Bearer, headers: [], offline: false }
validate_url https:/// ok=true
validate_url https://?broken ok=true
validate_url https://example.com\nheader ok=true
silent_body deadline_ms=50 elapsed_ms=301 failure=Some(IncompleteStream)
```

```text
permission target=.env outcome=Deny
permission target=./.env outcome=Allow
permission target=/tmp/rune-code-fixture/.env outcome=Allow
literal_file_grant_wildcard_matches_other=true
shell_first=$ sleep 0.1; printf LAST | [session shell-178670-1 running, process group 178671] | 
shell_second=$ printf OTHER | OTHER | [exited with status 0] | 
shell_unread_first=Err(RuneError { code: NotFound, message: "no session `shell-178670-1`", detail: ErrorDetail { field: None, invariant: None, hint: Some("no session is running"), observed: Some("shell-178670-1") } })
small_preview=tiny

[4 bytes retained; read more with read_tool_result and handle result-95de2362e742099ae5e4c2f0] readable=false
retained_duplicate_entries=1 total_bytes=10000
credentials_concurrent attempted=32 errors=27 stored_entries=Ok(3)
session_metadata_failure append_ok=false durable_events=1 next_seq=2
settings_killed_holder reacquire_ok=false elapsed_ms=2008 lock_exists=true
sandbox_argv=["/usr/bin/bwrap", "--die-with-parent", "--new-session", "--unshare-pid", "--unshare-net", "--ro-bind", "/", "/", "--dev-bind", "/dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--bind", "/tmp/rune-code-fixture", "/tmp/rune-code-fixture", "--ro-bind", "/tmp/rune-code-fixture/.git/config", "/tmp/rune-code-fixture/.git/config", "--tmpfs", "/home/hermes/.ssh", "--tmpfs", "/home/hermes/.config/gh", "--tmpfs", "/home/hermes/.gnupg", "--tmpfs", "/home/hermes/.local/state/rune", "--", "printf", "hi"]
sandbox_existing_nested_hooks_protected=false missing_root_hooks_protected=false
```

DNS command: `python` with `socket.getaddrinfo(host, 80, type=socket.SOCK_STREAM)` for the four hosts below. Real output:

```text
127.1 ['127.0.0.1']
127.0.1 ['127.0.0.1']
10.1 ['10.0.0.1']
127.0.0.1.nip.io ['127.0.0.1']
```

The WebFetch probe uses its recording backend and proves acceptance before any HTTP request. The DNS command separately proves these accepted strings resolve to private addresses on this host. The sandbox probe checks generated argv, not live kernel enforcement. Credential concurrency produced a real failure in this run; the exact counts depend on scheduling.

### Third scratch probe

Commands: `python /tmp/rune-code-compile3.py`, `/tmp/rune-code-probe3 > /tmp/rune-code-probe3.out`. Both exited zero. Output:

```text
credential_budget write=1 success=true
credential_budget write=2 success=true
credential_budget write=3 success=true
credential_budget write=4 success=true
credential_budget bytes=65749 read_ok=false
zero_context resolved=Some(0) diagnostics=[]
provider_override provider=chat_completions model=anthropic-selected diagnostics=[]
model_tag_choice=None
model_plain_choice=One("qwen3")
summary_finish=Some(MaxTokens) success=false validate_summary_ok=true
```

Additional confirmed findings: four individually valid credentials can create a file beyond the reader's total cap (`crates/rune-net/src/auth.rs:130`, `auth.rs:168`, `auth.rs:195`); zero model context is accepted (`crates/rune-core/src/config.rs:323`); a provider environment override retains the old provider's model (`config.rs:1230`); unknown tagged local model identifiers are refused (`crates/rune-net/src/catalog.rs:295`). A provider's legal MaxTokens finish still returns an outcome and passes summary text validation, while the CLI compact path checks only text (`crates/rune/src/session.rs:1869`, `session.rs:1872`), so it can replace history with a truncated summary.

### Coverage distinctions and remaining untested paths

The existing large-description test uses only ASCII (`crates/rune-tools/src/contract.rs:596`), so it does not catch the confirmed UTF-8 boundary panic. The existing long-line grep test is about 200 KiB, shorter than the one-MiB bounded line reader, and puts its match at the beginning (`crates/rune-tools/src/grep_files.rs:1182`, `grep_files.rs:1181`); it does not exercise either confirmed long-line matching defect. The credential tests cover several providers sequentially (`crates/rune-net/src/auth.rs:504`) and reject one oversized credential (`auth.rs:554`), while the scratch probes exercise concurrent modification and total serialized-file size. The silent-stream cancellation test checks prompt return (`crates/rune-net/src/transport.rs:1139`, `transport.rs:1159`), not destruction of the blocked reader. The shell finished-session test reads the final output before asserting removal (`crates/rune-tools/src/shell.rs:1938`, `shell.rs:1978`), while the scratch repro starts another process before reading the first process's final output. Session metadata tests rebuild deleted metadata and reopen valid metadata (`crates/rune-session/src/store.rs:764`, `store.rs:791`), while the scratch repro makes projection refresh fail after authoritative sync.

The live Linux sandbox mutation paths remain unrun in this audit; the generated argv proves the omission but not an exploit under kernel enforcement (`crates/rune-exec/src/sandbox.rs:325`, scratch probe2 argv). Windows and macOS execution backends were not run on this Linux workspace. The wasm/web host was read at its trait boundary, but no browser host was run (`crates/rune-web/src/host.rs:320`); the WASM boundary review above covers the source findings. Blocked-reader resource accumulation, holder-record handover race, and read-size time-of-check growth have code evidence rather than empirical fault injection (`crates/rune-net/src/transport.rs:819`, `crates/rune-session/src/store.rs:402`, `crates/rune-session/src/event.rs:407`). Their future resource-count, lock-handover and capped-read checks are in the list. Parallel-read overlap and mid-retry steering also have code evidence, not standalone loop probes (`crates/rune-agent/src/turn.rs:651`, `turn.rs:552`), with future overlap and retry-wire checks in the list.

## Competitive source and runtime research

This file contains source review and local CLI observations. Version/help runs do not establish model quality, successful paid-provider operation, or the absence of terminal defects. All model traffic used for the fx rendering probe went to a local fixture, which returned a fixed response. No paid-provider request was made. The recorded fx fixture request was `{"path":"/v1/chat/completions","model":"audit-model","stream":true,"tools":15}`; its process exited 0 (`/tmp/rune-audit-fx-repro.py` output).

### Snapshots and runtime coverage

The following hashes came from `git -C /tmp/rune-audit-<tool> rev-parse HEAD` after shallow clones. The clone URLs are primary repositories. Installed released versions are a separate surface from each repository's current HEAD.

| Tool | Repository and inspected HEAD | Binary used | Runs and limitation |
| --- | --- | --- | --- |
| fx | https://github.com/vercel-labs/fx, `4d966e272cfc4296cdf703f409480088cf2e72ba` | `/tmp/rune-audit-fx-bin/fx`, GitHub v0.0.12 Linux x86_64 release | `--version`, `--help`, `ask --help`, `permissions --help`, `status --json`, `doctor --json`; fresh PTY onboarding and slash menu; local-provider PTY streaming and Ctrl-O transcript; headless ask executes a fixture read_file and persists a session. HEAD changelog already contains 0.0.13 work, so newer code must not be presented as released v0.0.12 behavior. [FX-CHANGELOG] |
| Codex CLI | https://github.com/openai/codex, `4ad985e2caaf877b96dafd8138dae2def467e01e` | `codex` already installed on PATH | `codex --version` and `codex --help`; no authenticated model run. Captured help includes sandbox, approvals, resume/fork, MCP, plugins, shared-server controls and local providers. [CX-CLI] |
| Claude Code | https://github.com/anthropics/claude-code, `2bfb629dfaff0c8318047a4beb93cf1dc5b58b18` | `/tmp/rune-audit-npm/node_modules/@anthropic-ai/claude-code-linux-x64/claude`, 2.1.289 | `--version`, `--help`; public repository has documentation, plugins and changelog, not the core agent implementation. Core claims below use official docs and observed flags, not an invented source audit. [CC-README] |
| opencode | https://github.com/anomalyco/opencode, `907b3bc518fa48e90e8ec24dd327d13eee71c36c` | `/tmp/rune-audit-npm/node_modules/opencode-linux-x64/bin/opencode`, 1.18.34 | `--version`, `--help`; source inspected for tools, plan agent, plugins, rewind and session handling. No model run. [OC-TOOLS] |
| crush | https://github.com/charmbracelet/crush, `8da349060b7df148d209979be0a5e9c9281d1f15` | `/tmp/rune-audit-npm/node_modules/@charmland/crush/bin/crush`, v0.97.1 | npm wrapper `--version`, `--help`; wrapper downloaded the released binary on first invocation. Source inspected for LSP, permissions, models, notifications and session protocol. No model run. [CR-README] |
| goose | https://github.com/block/goose, redirects to aaif-goose, `591edd47cf2cfea4957d720c607cf2a4def8673d` | `/tmp/rune-audit-goose-bin/goose`, 1.53.0 Linux musl release | `--version`, `--help`; source inspected for CLI, recipes, providers, extension transports and permission configuration. No model run. [GS-CLI] |
| aider | https://github.com/Aider-AI/aider, `5dc9490bb35f9729ef2c95d00a19ccd30c26339c` | `/tmp/rune-audit-aider312/bin/aider`, 0.86.2 | `--version`, `--help`; source inspected for repository map, commands, Git integration and lint/test flags. Python 3.14 installation first failed with `BackendUnavailable: Cannot import 'setuptools.build_meta'`; Python 3.12 virtualenv succeeded. No model run. [AI-ARGS] |
| gemini-cli | https://github.com/google-gemini/gemini-cli, `fb972b2f87fe7d5b06d37eac711490162d98de2c` | `/tmp/rune-audit-npm/node_modules/.bin/gemini`, 0.62.0 | `--version`, `--help`; source inspected for registry, search grounding, hooks, restore and accessibility flags. No Google sign-in or model run. [GM-CONFIG] |
| amp | Official docs: https://ampcode.com/docs | `/tmp/rune-audit-npm/node_modules/@ampcode/cli-linux-x64/amp`, `0.0.1791146283-g5c3f72` | `--version`, `--help`; downloaded npm package contains a native executable. No public core repository was established, so terminal/session/provider/extension claims use official docs and observed flags. No account or model run. [AM-KEYS] [AM-THREADS] |

Exact successful version outputs:

```text
$ /tmp/rune-audit-fx-bin/fx --version
0.0.12
$ codex --version
codex-cli 0.160.0
$ /tmp/rune-audit-npm/node_modules/@anthropic-ai/claude-code-linux-x64/claude --version
2.1.289 (Claude Code)
$ /tmp/rune-audit-npm/node_modules/opencode-linux-x64/bin/opencode --version
1.18.34
$ /tmp/rune-audit-npm/node_modules/.bin/crush --version
crush version v0.97.1
$ /tmp/rune-audit-goose-bin/goose --version
 1.53.0
$ /tmp/rune-audit-aider312/bin/aider --version
aider 0.86.2
$ /tmp/rune-audit-npm/node_modules/.bin/gemini --version
0.62.0
$ /tmp/rune-audit-npm/node_modules/@ampcode/cli-linux-x64/amp --version
0.0.1791146283-g5c3f72 (released 2026-10-04T20:38:03.000Z, 55m ago)
```

All successful version/help subprocesses exited 0. Full stdout/stderr captures are under `/tmp/rune-audit-<tool>--help.txt` and `/tmp/rune-audit-<tool>--version.txt`, except fx's filenames use `/tmp/rune-audit-fx--help.txt` and `/tmp/rune-audit-fx--version.txt`.

Runtime help excerpts, produced by those exact executables with `--help`. Lines between the selected excerpts are omitted; the retained lines are verbatim.

```text
$ /tmp/rune-audit-npm/node_modules/opencode-linux-x64/bin/opencode --help
  opencode acp                 start ACP (Agent Client Protocol) server
  opencode attach <url>        attach to a running opencode server
  opencode export [sessionID]  export session data as JSON
  opencode import <file>       import session data from JSON file or URL
      --fork          fork the session when continuing (use with --continue or --session)  [boolean]
```

```text
$ /tmp/rune-audit-goose-bin/goose --help
  run         Execute commands from an instruction file or stdin
  recipe      Recipe utilities for validation and deeplinking
  schedule    Manage scheduled jobs [alias: sched]
  completion  Generate the autocompletion script or Nushell module for the specified shell
```

```text
$ /tmp/rune-audit-npm/node_modules/.bin/gemini --help
      --approval-mode             Set the approval mode: default (prompt for approval), auto_edit (auto-approve edit tools), yolo (auto-approve all tools), plan (read-only mode)  [string] [choices: "default", "auto_edit", "yolo", "plan"]
      --screen-reader             Enable screen reader mode for accessibility.  [boolean]
  -o, --output-format             The format of the CLI output.  [string] [choices: "text", "json", "stream-json"]
```

### fx, the most important comparison

The cloned fx repository uses Apache-2.0, verified in its license text [FX-LICENSE].

#### Implemented libraries are not CLI parity

Rune's assembled library inventory contains filesystem, shell, question, web, vision and skill implementations (`crates/rune-tools/src/inventory.rs:27`), and its agent library includes subagents and steering (`crates/rune-agent/src/subagent_tool.rs:128`, `crates/rune-agent/src/turn.rs:340`). The interactive CLI obtains its registry through `builtin_with_web` and then inserts `Subagent::unsupported` (`crates/rune/src/session.rs:3009`, `:3018`). It advertises that registry and moves it unchanged into the host (`:973`, `:981`). The registered question tool uses `AskUserQuestion::unavailable`, skill and capability search share an empty catalog, and vision is unconfigured (`crates/rune-tools/src/inventory.rs:110`, `:114`, `:136`). These are advertised implementations with missing runtime backends, so this document does not count them as working interactive features.

Rune's MCP transport implementation is present in `rune-context`, but the ACP composition explicitly ignores `mcpServers` because no MCP client is wired in (`crates/rune-acp/src/server.rs:547`). The exact read-only search below found only the empty interactive authority field, not MCP construction in CLI, SDK or ACP source:

```text
$ rg -n 'mcp_view|McpView|connect_all|mcp::|ServerConfig::parse' crates/rune/src crates/rune-sdk/src crates/rune-acp/src
crates/rune/src/session.rs:3027:            mcp_view: None,
```

Rune's SDK does have checkpoints and event iteration (`crates/rune-sdk/src/agent.rs:508`, `:663`, `:816`), and fx documents its SDK counterpart [FX-SDK] [FX-CHECKPOINT]. Both render inline and preserve terminal scrollback: Rune `README.md:121`; fx [FX-README]. These are real capabilities to preserve, while the missing CLI backends above require integration tests rather than more schema tests.

#### Where Rune loses

1. **The headless command runs the agent.** Rune `ask` constructs a request with only the supplied user message, sends one provider completion, converts returned tool-call names into `status: "success"` without executing them, and always returns an empty session ID (`crates/rune/src/ask.rs:183`, `:206`, `:217`, `:239`). It does not install the interactive registry or instructions in this path. fx v0.0.12 was tested with a local model that first requested `read_file("fixture.txt")`, then answered after receiving its result. It made two requests, sent the actual file content in the second, returned a nonempty saved session ID, and exited 0. Exact harness command and output:

```text
$ python3 /tmp/rune-audit-fx-headless.py
COMMAND ["/tmp/rune-audit-fx-bin/fx", "ask", "--json", "Read fixture.txt and report its sentinel."]
STDOUT {"output":"fixture-read-ok","final_output":"fixture-read-ok","exit_code":0,"model":"audit-model","resolved_provider":null,"session_id":"udPM5qzPysHG","steps":1,"tool_calls":[{"name":"read_file","status":"success"}],"usage":{"input_tokens":200,"output_tokens":40}}
STDERR Reading fixture.txt
EXIT 0
REQUESTS 2
FIRST_TOOL_NAMES ["read_file", "glob_files", "grep_files", "edit_file", "write_file", "shell", "subagent", "capability_search", "skill", "install_skill", "mcp_select_tool", "mcp_features", "ask_user_question", "web_fetch", "read_tool_result"]
SECOND_TOOL_RESULTS [{"role": "tool", "content": "<path>fixture.txt</path>\n<content>\n1\tlocal-fixture-sentinel\n</content>", "tool_call_id": "call_fixture"}]
```

The harness source and request bodies are `/tmp/rune-audit-fx-headless.py` and `/tmp/rune-audit-fx-headless-requests.json`. The dynamic session ID above is one real run, not a stable expected ID. This is the largest demonstrated CLI gap against fx in this research.


2. **Structured output presentation.** Rune wraps sanitized text for every assistant entry, with no Markdown interpretation at `crates/rune-term/src/transcript.rs:344`. fx's v0.0.12 local PTY fixture rendered `# Heading` as `Heading`, a Markdown table with cell borders, a language-labelled Rust code block, and a link as its label. The raw response supplied `# Heading`, `| Name | Value |`, `` ```rust `` and `[Docs](https://example.com)`. Capture command: `/tmp/rune-audit-aider-venv/bin/python /tmp/rune-audit-fx-repro.py`. The capture below is a grid replay, not a claim about font-level Unicode correctness. [FX-CHANGELOG]

```text
CAPTURE ready cursor 2 2
𝒇x v0.0.12 · Run /help for commands
┃
auto · audit-model
CAPTURE response cursor 2 20
┃ Render the fixture
  Heading
  ┌──────┬─────────┐
  │ Name │ Value   │
  ├──────┼─────────┤
  │ wide │ 界 👩   │
  └──────┴─────────┘
  ─ rust ─────────────────────────
  fn main() { println!("hello"); }
  ────────────────────────────────
  Docs
REQUESTS [{"path": "/v1/chat/completions", "model": "audit-model", "stream": true, "tools": 15}]
EXIT 0
```

The fixture actually included `界 👩‍💻 é`. pyte does not faithfully model all grapheme/emoji cells, and the Ctrl-O capture showed repeated rows, so neither is evidence of an fx Unicode or transcript defect. The byte capture is `/tmp/rune-audit-fx-render.bin`. This distinction is necessary before treating terminal-emulator screenshots as authoritative.

3. **Detail on demand.** fx has a Ctrl-O full transcript with navigation, page scrolling and explicit surface ownership [FX-TRANSCRIPT]. Rune's input action enum and mapping do not expose a full transcript toggle (`crates/rune-term/src/input.rs:21`, `:196`), while tool output is reduced to `display.tool_lines` and a remaining-line count (`crates/rune-term/src/transcript.rs:367`). Copy the interaction while keeping the main shell inline.

4. **Composer layout and input.** fx models rows, hard newlines, soft wraps, attachment badges and selection [FX-LAYOUT], and its PTY tests cover narrow-pane movement, word wrapping and Unicode right-margin repaint [FX-INPUT-TEST]. Rune calls its composer a single-line editor (`crates/rune-term/src/editor.rs:70`), maps all Enter events to submit (`crates/rune-term/src/input.rs:238`), and sends Up/Down to prompt recall or picker movement (`:259`). This is a concrete scope difference, not proof that every Rune Unicode path is broken.

5. **Image access.** fx supports repeated `ask --image PATH`, pasted images and image-returning `read_file` [FX-README]. Rune `read_file` deliberately returns an image description at `crates/rune-tools/src/read_file.rs:114`, and its launch parser has no image attachment flag (`crates/rune/src/cli.rs:207`). Rune also registers an unconfigured vision backend (`crates/rune-tools/src/inventory.rs:136`), so its vision schema does not establish a working CLI path. Image propagation needs a provider contract test, not only image-format detection tests.

6. **Retained context retrieval.** fx implements `read_tool_result` ranges, queries and archived-compaction search [FX-RESULT]. Rune has an in-memory result store whose preview tells the model to call `read_tool_result` (`crates/rune-tools/src/result_store.rs:132`), but the assembled inventory at `crates/rune-tools/src/inventory.rs:27` does not register that tool. Rune's compaction replaces an old prefix with an unconstrained prose summary and validates only emptiness/32-byte minimum (`crates/rune-agent/src/compaction.rs:25`, `:168`, `:189`). fx HEAD preserves original messages/final replies, numbered source records, unchanged user rules and searchable earlier compactions [FX-MEMORY]; this is 0.0.13 source, while v0.0.12's changelog only supports the earlier compaction claims [FX-CHANGELOG]. Copy the saved source records and retrieval separately from the summarizer policy.

7. **MCP management.** fx exposes `mcp` on its observed help and `/mcp` management, with a Servers menu and authorization flow [FX-MCP]. Rune has stdio/HTTP/SSE transport library code (`crates/rune-context/src/mcp/config.rs:68`, `crates/rune-context/src/mcp/client.rs:1091`), but its ACP runtime explicitly ignores requested MCP servers (`crates/rune-acp/src/server.rs:547`) and the interactive composition has no MCP registry integration (`crates/rune/src/session.rs:3009`). Its top-level command enumeration and dispatch do not contain `mcp` (`crates/rune/src/cli.rs:19`, `:305`), and the slash table has none (`crates/rune-term/src/commands.rs:24`). Connect the existing transport to the runtime before treating management or OAuth as the only remaining gaps.

8. **Terminal regression surface.** fx includes real tmux PTY tests for input navigation and full-transcript transitions, including live output, resize storms and resume [FX-INPUT-TEST] [FX-TRANSCRIPT-TEST]. The source demonstrates test coverage exists; those tests were not run here because the Bun-based harness was not provisioned; tmux was later used for Rune captures. Rune should use the specific defects found by the runtime agent as fixed fixtures, not claim parity from pure renderer unit tests.

#### Where Rune already wins

1. **OS-enforced execution restrictions.** Rune prepares Linux bwrap namespaces and macOS Seatbelt profiles, probes support and refuses silent degradation (`crates/rune-exec/src/sandbox.rs:3`, `:23`, `:37`). fx deliberately retired sandbox configuration and runs approved subprocesses on the host [FX-NO-SANDBOX]. Keep this distinction visible. It is meaningful even where a Rune host reports unsupported sandbox support; the exact supported-host enforcement still needs real tests.

2. **Explicit offline switch.** Rune `README.md:127` documents a switch that refuses egress, with host-supplied outbound backends (`crates/rune-tools/src/inventory.rs:76`). The released fx command below exits 1:

```text
$ /tmp/rune-audit-fx-bin/fx --offline ask hello
fx: unknown subcommand: --offline
```

The actual output continues with help. This proves the missing flag, not that every fx operation attempts network access.

3. **Direct protocol selection.** Rune's ACP dialect supports Chat Completions, Responses and Anthropic (`crates/rune-acp/src/server.rs:67`). fx's built-in provider set contains Gateway, Codex, Grok and a configured Chat Completions adapter [FX-PROVIDERS]; its custom-connection documentation requires `protocol: openai-chat-completions` and says provider-specific reasoning controls are unsupported [FX-CONNECTIONS]. Rune can connect directly to an Anthropic Messages endpoint rather than requiring a compatible gateway for that protocol.

4. **Inspectable configuration contract.** Rune prints `config --explain`, `limits --json` and `prompt --show` (`README.md:140`), with project-safe/profile-only keys (`crates/rune-core/src/config.rs:1465`). fx has a useful observed `status --json`, and also excludes custom endpoints from project files [FX-CONNECTIONS]. The Rune advantage is the dedicated per-setting provenance and limits surfaces, not a claim that fx has no trust boundary.

5. **Measured released Linux binary size.** Exact command used:

```text
$ python3 -c 'from pathlib import Path; p=Path("/tmp/rune-audit-fx-bin/fx"); print(p.stat().st_size, p.stat().st_size/1048576)'
12520232 11.940223693847656
```

The binary came from https://github.com/vercel-labs/fx/releases/download/v0.0.12/fx-linux-x86_64.tar.gz. fx.sh currently displays 6.51 MiB [FX-SITE] without a target beside that label. This audit measured Rune's built Linux release at 4,717,720 bytes. Do not compare either published headline to another target or compressed archive. Same-host startup results follow below.

#### Copy, adapt, reject

Copy the contextual transcript viewer, Markdown readability, explicit image attachments and PTY replay tests ([FX-TRANSCRIPT], [FX-INPUT-TEST], [FX-README]). Adapt archived context retrieval and compaction provenance into small changes before copying the whole compactor ([FX-RESULT], [FX-MEMORY]). Preserve Rune's direct provider dialects, inspectable limits and fail-closed sandbox (`crates/rune-acp/src/server.rs:67`, `README.md:140`, `crates/rune-exec/src/sandbox.rs:8`). Reject fx's removal of the execution sandbox for Rune's stated product contract [FX-NO-SANDBOX]. Gateway-specific Slack account setup and premium serving-tier flags are not necessary for Rune's first-week expansion; their code exists in fx [FX-README], but they do not fix the observed Rune terminal or command defects.

### Wider field

#### Codex CLI

**Terminal and sessions:** The observed `codex --help` offers `--no-alt-screen` to preserve scrollback, resume/fork, and `--worktree`. Source CLI defines MCP, plugins, completion generation, sandbox and shared app-server controls [CX-CLI]. Rune already offers inline output and an append-only session tree (`README.md:109`, `crates/rune-session/src/tree.rs:1`), but its launch command set lacks shell completion generation and worktree management (`crates/rune/src/cli.rs:19`). This is feature inventory, not a terminal rendering contest.

**Tools, approvals and extensions:** Codex's platform selector includes Linux, macOS and Windows restrictions [CX-SANDBOX], while Rune's documented native backends are Linux/macOS (`crates/rune-exec/src/sandbox.rs:14`). Codex exposes persisted goals through `get_goal`, `create_goal` and `update_goal`, with lifecycle/accounting rules [CX-GOAL] and an official `/goal` lifecycle [CX-GOAL-DOC]. Codex's home instructions prefer `AGENTS.override.md` before `AGENTS.md` and retain a previous good value after a failed refresh [CX-AGENTS]. Rune has AGENTS loading (`crates/rune-context/src/instructions.rs:1`) but no goal commands in its command inventory (`crates/rune-term/src/commands.rs:24`).

**Providers and Rune advantage:** Observed help supports OSS/local-provider routing (Ollama and LM Studio) in addition to the configured model. Rune's declared direct Anthropic, Responses and compatible-endpoint dialects are a useful different scope (`crates/rune-acp/src/server.rs:67`); do not claim Codex is exclusively OpenAI without auditing provider configuration. Rune does not require a shared server for its normal CLI (`crates/rune/src/main.rs:108`), while Codex's observed `--no-daemon` explicitly disables its shared background server.

**Action:** Copy persisted objectives with explicit budgets, shell completions and selected-turn fork UX. Adapt network/filesystem policy explanations from the sandbox surfaces. Reject making a daemon a prerequisite for Rune's inline CLI. [CX-CLI] [CX-GOAL] `README.md:119`.

#### Claude Code

The public clone was reviewed for changelog and plugins; no core source audit is possible from that repository alone [CC-README]. Observed 2.1.289 help includes `--agent`, `--agents`, `--fork-session`, `--json-schema`, `--output-format stream-json`, `--mcp-config`, `--plugin-dir` and `--permission-mode`. Official hooks cover before/after tool, permission requests, session startup/end and compaction; a PreToolUse hook may allow, deny, ask or defer [CC-HOOKS]. Official subagents have declared tools, skills and permissions, with parent-mode restrictions on bypass behavior [CC-SUBAGENTS]. Rune already supplies library host tools/plugins (`crates/rune-sdk/src/plugin.rs:1`) and subagent abstractions (`crates/rune-agent/src/subagent.rs:1`), but its CLI installs the unsupported delegate (`crates/rune/src/session.rs:3018`); its CLI command/schema surfaces have no hooks, output schema or named-agent profile selection (`crates/rune/src/cli.rs:19`, `crates/rune-tools/src/inventory.rs:27`).

Claude's permission docs distinguish allow/ask/deny rules and mode settings [CC-PERMISSIONS]. Official setup supports direct Anthropic and third-party providers, so it is incorrect to describe it as always requiring an Anthropic subscription [CC-OVERVIEW]. Rune's direct protocol declarations remain simpler to inspect (`crates/rune-acp/src/server.rs:67`) and Rune's local storage claim (`README.md:128`) differs from Claude's explicit public data-collection disclosure [CC-README]. This is a privacy-contract comparison, not a claim that Claude uploads every transcript.

The inspected 2.1.289 changelog records recent terminal freezing, symlink-read permission, compound-command permission and UI control-character fixes [CC-CHANGELOG]. Copy the classes of regression tests, hook schema and headless event contract. Adapt agent profiles without allowing a child configuration to expand parent authority. Reject uncontrolled repository hooks that execute merely because a file was checked out. [CC-HOOKS] [CC-SUBAGENTS] `crates/rune-core/src/config.rs:1465`.

#### opencode

Observed help exposes ACP, remote attachment, `serve`, `web`, provider/model management, session export/import and `--fork`; these are independently accessible surfaces, not proof every session needs a persistent daemon. Source registers shell, read/write/edit, glob/grep, web fetch/search, task, question, todo, skill and apply-patch; its LSP tool is conditional on an experimental flag [OC-TOOLS]. It has build and read-only plan agents [OC-README], plugin-provided tools with a host/context permission bridge [OC-TOOLS], selected-turn snapshot revert and unrevert [OC-REVERT], and a local todo tool [OC-TODO]. Rune's assembled built-ins lack todo/LSP/apply-patch (`crates/rune-tools/src/inventory.rs:27`), and its `/undo` is described as restoring the last turn (`crates/rune-term/src/commands.rs:50`).

Rune already has a real execution sandbox (`crates/rune-exec/src/sandbox.rs:3`), direct providers and an inline CLI with no listening API server required (`crates/rune/src/main.rs:108`). opencode's source reviewed here emphasizes permission prompts and plugin bridges; no equivalent OS sandbox was established from the reviewed tool registry, so do not assert there is none across the whole repository. Copy plan-mode affordances, export/import and todos. Adapt selected-turn rewind with explicit changed-file conflict checks. Treat server/web UI expansion as a separate optional package rather than expanding Rune's mandatory binary. [OC-README] [OC-REVERT] `README.md:119`.

#### crush

Crush source/docs provide direct provider selection, custom OpenAI/Anthropic-compatible providers, LSP server configuration and definition/symbol/rename tools [CR-README] [CR-LSP]. Its explicit permission request protocol carries session ID, call ID, tool, action, parameters and path [CR-PERMISSION]. It offers conditional notifications for permission waits and turn completion, with native/OSC/bell choices [CR-NOTIFICATIONS]. Its recent release removed a Git-branch glyph that required Nerd Fonts [CR-RELEASE]. These provide concrete terminal/accessibility examples Rune can adapt; Rune's input and command inventory has no notification or keymap setting (`crates/rune-term/src/input.rs:196`, `crates/rune-core/src/config.rs:1`).

Rune's declarative TOML with project-safe keys (`README.md:132`, `crates/rune-core/src/config.rs:1465`) differs from Crush's Bash-interpreted crushrc [CR-CONFIG]. Rune's no-telemetry contract (`README.md:17`) differs from Crush's documented pseudonymous metrics and `CRUSH_DISABLE_METRICS`/`DO_NOT_TRACK` opt-out [CR-METRICS]. Preserve Rune's no-telemetry/declarative defaults, copy focus-aware notifications and LSP as optional tools, and avoid adding a shell interpreter to configuration. These are choices backed by the two explicit configuration/metrics contracts, not measured size claims.

#### goose

Observed 1.53.0 help includes `session`, `run`, `recipe`, `schedule`, `plugin`, `acp`, `serve`, terminal integration, review and shell completion. Source `SessionOptions` includes debug visibility, repeated-identical-tool-call limits, turn limits and a container selector [GS-CLI]. Provider source enumerates native APIs, gateways, local models and ACP-backed agents [GS-PROVIDERS]. Recipes contain instructions, prompt, extension selection, parameters, JSON response schema, subrecipes and retry configuration [GS-RECIPE]. Extension management has separate stdio, Streamable HTTP and built-in backends [GS-EXTENSIONS]; permission configuration distinguishes user and smart-approve tools [GS-PERMISSIONS].

Rune has library host-supplied tools and an ACP endpoint (`crates/rune-sdk/src/agent.rs:1`, `crates/rune-acp/src/server.rs:114`), while CLI ask omits the agent loop (`crates/rune/src/ask.rs:206`) and ACP ignores supplied MCP servers (`crates/rune-acp/src/server.rs:547`), while its custom commands are Markdown prompt templates (`README.md:110`). Goose's typed recipe/schema/parameter contract is a useful extension to this narrower surface. Rune has an explicit small-binary budget in `xtask/src/main.rs:147`; the downloaded goose musl archive was 51,095,198 bytes (GitHub release API), which is archive size and must not be confused with executable size. Copy recipe validation, bounded repetitive-tool detection and shell completion. Adapt schedules into an external scheduler example first. Reject putting a server/scheduler into every Rune invocation. [GS-CLI] `README.md:119`.

#### aider

Aider's repository map parses language tags and ranks file relations using PageRank [AI-MAP]. It is a terminal editing workflow with explicit chat/editable/read-only file handling and `/map`, `/tokens`, `/lint`, `/test`, `/architect`, `/save`, `/load`, `/copy`, `/paste` and `/undo` commands [AI-COMMANDS]. Its flags enable auto-lint and auto-test and default auto-commits on [AI-ARGS]. Its source/docs model connections include local/cloud backends through configurable adapters [AI-README]. Rune's glob/grep tools are a different, literal navigation baseline (`crates/rune-tools/src/grep_files.rs:1`), and no symbol map tool appears in `crates/rune-tools/src/inventory.rs:27`.

Rune's execution sandbox and interactive bounded tool loop (`crates/rune-exec/src/sandbox.rs:3`, `crates/rune/src/session.rs:16`) provide a stronger explicit harness boundary than the reviewed aider editing/command workflow. No equivalent OS sandbox was established in this source review, so do not claim a complete absence from every aider integration. Copy a budgeted repository-symbol map and explicit verification commands. Keep Git commits opt-in rather than copying aider's default auto-commit behavior [AI-ARGS], because Rune permission boundaries are explicit (`README.md:123`).

#### gemini-cli

Observed 0.62.0 help has plan/default/auto_edit/yolo modes, resume/list/delete session flags, screen reader mode, JSON and stream-JSON, plus MCP/skills/hooks/extensions management. Source tool registration includes standard filesystem/shell/search/question tools, MCP resources, background-process inspection, plans and todos [GM-TOOLS]. Web search uses a utility model and retains grounding chunks/supports [GM-SEARCH]. Its hooks are explicit typed lifecycle events, including before/after model/tool and pre-compression [GM-HOOKS]. Restore lists saved tool-call checkpoints and restores through Git-backed services [GM-RESTORE].

Rune already exposes a model-agnostic direct protocol selection (`crates/rune-acp/src/server.rs:67`), while Gemini's built-in search explicitly calls Gemini's generated-content/grounding API [GM-SEARCH]. Rune's plain `ask --json` provides one result object (`crates/rune/src/ask.rs:3`), not the observed stream-JSON surface. Copy screen-reader rendering, checkpoint browsing and typed streaming events. Adapt search source records to all Rune providers through its existing backend trait (`crates/rune-tools/src/web.rs:1`). Reject coupling the whole harness to Google's grounding service. [GM-SEARCH] [GM-CONFIG].

#### amp

Amp was run for version/help and inspected through official docs. It offers a command palette, external-editor prompt editing, image paste, searchable prompt history, configurable keymaps and separate steering/queue behavior [AM-KEYS]. Its threads support keyword/file/date search, labels, export, references to older threads and cross-client continuation [AM-THREADS]. Its execute mode handles piped prompts, stream-JSON and per-invocation MCP configuration [AM-EXECUTE]. Official model routing now supports personal/workspace connections with own keys/subscriptions/gateways, so the outdated claim that Amp cannot use customer provider credentials would be wrong [AM-MODELS]. Extension docs expose MCP and plugin APIs [AM-MCP].

Rune's local-only session contract (`README.md:128`), explicit filesystem sandbox and no account (`README.md:17`) are different from Amp's account/thread URL/cloud-runner workflow [AM-INTRO] [AM-THREADS]. Copy prompt history search, external editor, local session labels/search and streaming JSON. Adapt cross-session referencing to local session handles with permission checks. Deliberately reject account-required thread synchronization and cloud executors as mandatory Rune features [AM-INTRO], because they conflict with the declared local one-binary product (`README.md:119`, `:128`).

### Release history reviewed

The installed CLI versions and repository HEADs differ where noted above. The following are changes claimed by each primary changelog/release note, not defects reproduced here or proof Rune has the same bug.

| Tool | Reviewed history and concrete changes | Use for Rune |
| --- | --- | --- |
| fx | v0.0.12 includes GFM tables and transcript/input fixes; HEAD 0.0.13 adds retained source records and custom-model changes [FX-CHANGELOG]. | Treat released UX and unreleased memory behavior separately. |
| Codex CLI | rust-v0.160.0 records queued-input reconnection without duplicate sends, restored provider settings, SQLite initialization errors and authoritative explicit model catalogs [CX-RELEASE]. | Add focused reconnect/resume/model-cache fixtures before copying larger server infrastructure. |
| Claude Code | 2.1.289 records terminal freeze, symlink-read permissions, compound-command deny and terminal-control fixes [CC-CHANGELOG]. | Keep terminal input/output and permission parsing regressions as fixture cases. |
| opencode | v1.18.34 adds model-request session/parent identity headers and Developer ID signing of macOS releases [OC-RELEASE]. | Document intentional identity headers and test signed release execution on macOS. |
| crush | v0.97.1 removes a Git-branch glyph requiring Nerd Fonts; its release gives checksums and Sigstore verification instructions [CR-RELEASE]. | Use portable glyph fallbacks and publish verifiable artifacts. |
| goose | v1.53.0 records session rename, lean ACP-only binary, preserved reply context, tool-result image ordering, MCP redirect protection and tool-boundary compaction [GS-RELEASE]. | Keep optional protocols separate and test context/image continuity across tool boundaries. |
| aider | Inspected main HISTORY adds language tags, handled provider errors and circular-symlink fixes, while recorded v0.86.1 history differs from installed 0.86.2 [AI-HISTORY]. | Avoid presenting main-branch changes as the installed binary's tested behavior. |
| gemini-cli | v0.62.0 records negative terminal-dimension guards, PTY descriptor/exit cleanup, preserved refresh tokens and quieter cancellation [GM-RELEASE]. | Add tiny-width and process-cancellation fixtures that assert resources are released. |
| amp | Official Chronicle, not a public core-code changelog, is the release-history surface [AM-CHRONICLE]. | Keep closed-core implementation uncertainty explicit. |

### Native executable payloads measured

These are the downloaded executable files actually invoked, all Linux x86_64 artifacts. They include each distributor's bundled runtime choices; no additional stripping or rebuilding was performed. This comparison establishes download/executable payload differences, not semantic complexity, peak memory or startup speed. Rune's release size is 4,717,720 bytes, as recorded in the gate output above.

```text
$ python3 - <<'PY'
from pathlib import Path
files={'fx':'/tmp/rune-audit-fx-bin/fx','claude':'/tmp/rune-audit-npm/node_modules/@anthropic-ai/claude-code-linux-x64/claude','amp':'/tmp/rune-audit-npm/node_modules/@ampcode/cli-linux-x64/amp','opencode':'/tmp/rune-audit-npm/node_modules/opencode-linux-x64/bin/opencode','crush':'/tmp/rune-audit-npm/node_modules/@charmland/crush/bin/crush','goose':'/tmp/rune-audit-goose-bin/goose'}
for tool,path in files.items():
 p=Path(path);print(tool,path,p.stat().st_size,round(p.stat().st_size/1048576,3))
PY
fx /tmp/rune-audit-fx-bin/fx 12520232 11.94
claude /tmp/rune-audit-npm/node_modules/@anthropic-ai/claude-code-linux-x64/claude 246107320 234.706
amp /tmp/rune-audit-npm/node_modules/@ampcode/cli-linux-x64/amp 133768672 127.572
opencode /tmp/rune-audit-npm/node_modules/opencode-linux-x64/bin/opencode 185632896 177.033
crush /tmp/rune-audit-npm/node_modules/@charmland/crush/bin/crush 90824864 86.617
goose /tmp/rune-audit-goose-bin/goose 148842792 141.948
```

No equivalent single native-executable measurement was made for installed Python aider or Node Gemini CLI. Their installed runtime environments are a different denominator.


### Source links

[FX-SITE]: https://fx.sh/
[FX-README]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/README.md#L14-L78
[FX-CHANGELOG]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/CHANGELOG.md#L3-L88
[FX-NO-SANDBOX]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/CHANGELOG.md#L455-L477
[FX-TOOLS]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/src/builtins/tools.zig#L18-L53
[FX-PROVIDERS]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/src/builtins/providers.zig#L11-L32
[FX-CONNECTIONS]: https://fx.sh/docs/configure-fx/custom-model-connections
[FX-LAYOUT]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/src/ui/input/visual_layout.zig#L9-L80
[FX-INPUT-TEST]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/tests/e2e/tui-input-navigation.test.ts#L392-L559
[FX-TRANSCRIPT]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/src/core/app/input_full_transcript_runtime.zig#L14-L100
[FX-TRANSCRIPT-TEST]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/tests/e2e/tui-full-transcript-brutal.test.ts#L1504-L1565
[FX-RESULT]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/src/tools/session/read_tool_result.zig#L16-L175
[FX-MEMORY]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/README.md#L147-L153
[FX-MCP]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/src/builtins/mcp.zig#L25-L60
[FX-CHECKPOINT]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/sdk/README.md#L286-L309
[FX-QUESTION]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/src/core/app/input_question_runtime.zig#L16-L95
[FX-SDK]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/sdk/README.md#L3-L36
[CX-CLI]: https://github.com/openai/codex/blob/4ad985e2caaf877b96dafd8138dae2def467e01e/codex-rs/cli/src/main.rs#L146-L205
[CX-SANDBOX]: https://github.com/openai/codex/blob/4ad985e2caaf877b96dafd8138dae2def467e01e/codex-rs/sandboxing/src/manager.rs#L49-L62
[CX-GOAL]: https://github.com/openai/codex/blob/4ad985e2caaf877b96dafd8138dae2def467e01e/codex-rs/ext/goal/src/spec.rs#L9-L94
[CX-GOAL-DOC]: https://developers.openai.com/cookbook/examples/codex/using_goals_in_codex
[CX-AGENTS]: https://github.com/openai/codex/blob/4ad985e2caaf877b96dafd8138dae2def467e01e/codex-rs/codex-home/src/instructions/mod.rs#L12-L105
[CC-README]: https://github.com/anthropics/claude-code/blob/2bfb629dfaff0c8318047a4beb93cf1dc5b58b18/README.md#L48-L72
[CC-CHANGELOG]: https://github.com/anthropics/claude-code/blob/2bfb629dfaff0c8318047a4beb93cf1dc5b58b18/CHANGELOG.md#L3-L46
[CC-OVERVIEW]: https://code.claude.com/docs/en/overview
[CC-HOOKS]: https://code.claude.com/docs/en/hooks
[CC-SUBAGENTS]: https://code.claude.com/docs/en/sub-agents
[CC-PERMISSIONS]: https://code.claude.com/docs/en/permissions
[OC-README]: https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/README.md#L100-L111
[OC-TOOLS]: https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/tool/registry.ts#L101-L170
[OC-REVERT]: https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/session/revert.ts#L35-L95
[OC-TODO]: https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/tool/todo.ts#L14-L42
[OC-LSP]: https://github.com/anomalyco/opencode/blob/907b3bc518fa48e90e8ec24dd327d13eee71c36c/packages/opencode/src/tool/lsp.ts#L25-L95
[CR-README]: https://github.com/charmbracelet/crush/blob/8da349060b7df148d209979be0a5e9c9281d1f15/README.md#L194-L237
[CR-CONFIG]: https://github.com/charmbracelet/crush/blob/8da349060b7df148d209979be0a5e9c9281d1f15/README.md#L247-L277
[CR-LSP]: https://github.com/charmbracelet/crush/blob/8da349060b7df148d209979be0a5e9c9281d1f15/internal/agent/tools/lsp_definition.go#L17-L67
[CR-PERMISSION]: https://github.com/charmbracelet/crush/blob/8da349060b7df148d209979be0a5e9c9281d1f15/internal/proto/permission.go#L7-L35
[CR-NOTIFICATIONS]: https://github.com/charmbracelet/crush/blob/8da349060b7df148d209979be0a5e9c9281d1f15/README.md#L687-L699
[CR-METRICS]: https://github.com/charmbracelet/crush/blob/8da349060b7df148d209979be0a5e9c9281d1f15/README.md#L988-L1006
[CR-RELEASE]: https://github.com/charmbracelet/crush/releases/tag/v0.97.1
[GS-CLI]: https://github.com/aaif-goose/goose/blob/591edd47cf2cfea4957d720c607cf2a4def8673d/crates/goose-cli/src/cli.rs#L76-L146
[GS-PROVIDERS]: https://github.com/aaif-goose/goose/blob/591edd47cf2cfea4957d720c607cf2a4def8673d/crates/goose/src/providers/mod.rs#L1-L100
[GS-RECIPE]: https://github.com/aaif-goose/goose/blob/591edd47cf2cfea4957d720c607cf2a4def8673d/crates/goose/src/recipe/mod.rs#L42-L129
[GS-EXTENSIONS]: https://github.com/aaif-goose/goose/tree/591edd47cf2cfea4957d720c607cf2a4def8673d/crates/goose/src/agents/extension_manager
[GS-PERMISSIONS]: https://github.com/aaif-goose/goose/blob/591edd47cf2cfea4957d720c607cf2a4def8673d/crates/goose/src/config/permission.rs#L27-L115
[AI-README]: https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/README.md#L40-L94
[AI-ARGS]: https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/args.py#L438-L565
[AI-MAP]: https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/repomap.py#L279-L382
[AI-COMMANDS]: https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/commands.py#L993-L1545
[GM-CONFIG]: https://github.com/google-gemini/gemini-cli/blob/fb972b2f87fe7d5b06d37eac711490162d98de2c/packages/cli/src/config/config.ts#L463-L476
[GM-TOOLS]: https://github.com/google-gemini/gemini-cli/blob/fb972b2f87fe7d5b06d37eac711490162d98de2c/packages/core/src/config/config.ts#L4009-L4123
[GM-SEARCH]: https://github.com/google-gemini/gemini-cli/blob/fb972b2f87fe7d5b06d37eac711490162d98de2c/packages/core/src/tools/web-search.ts#L89-L109
[GM-HOOKS]: https://github.com/google-gemini/gemini-cli/blob/fb972b2f87fe7d5b06d37eac711490162d98de2c/packages/core/src/hooks/types.ts#L43-L100
[GM-RESTORE]: https://github.com/google-gemini/gemini-cli/blob/fb972b2f87fe7d5b06d37eac711490162d98de2c/packages/cli/src/ui/commands/restoreCommand.ts#L39-L110
[AM-INTRO]: https://ampcode.com/docs
[AM-KEYS]: https://ampcode.com/docs/cli/keybindings
[AM-THREADS]: https://ampcode.com/docs/threads
[AM-EXECUTE]: https://ampcode.com/docs/cli/execute-mode
[AM-MODELS]: https://ampcode.com/docs/customize/model-routing
[AM-MCP]: https://ampcode.com/docs/customize/mcp

[CX-RELEASE]: https://github.com/openai/codex/releases/tag/rust-v0.160.0
[OC-RELEASE]: https://github.com/anomalyco/opencode/releases/tag/v1.18.34
[GS-RELEASE]: https://github.com/aaif-goose/goose/releases/tag/v1.53.0
[AI-HISTORY]: https://github.com/Aider-AI/aider/blob/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/HISTORY.md#L3-L29
[GM-RELEASE]: https://github.com/google-gemini/gemini-cli/releases/tag/v0.62.0
[AM-CHRONICLE]: https://ampcode.com/chronicle

[FX-LICENSE]: https://github.com/vercel-labs/fx/blob/4d966e272cfc4296cdf703f409480088cf2e72ba/LICENSE#L1-L4

### Same-host startup comparison

Actual command method: Python `subprocess.run([binary, flag], stdout=DEVNULL, stderr=DEVNULL)` timed with `perf_counter_ns`, 31 serial samples per flag, no process-floor subtraction. Rune was `target/release/rune`; fx was `/tmp/rune-audit-fx-bin/fx` v0.0.12. Both ran on the Linux host described above. All samples exited zero. This is a warm-host process-latency comparison, not a cold-filesystem, power, memory or cross-platform measurement. Exact recorded results:

```json
[
  {
    "binary": "rune",
    "flag": "--version",
    "exit": 0,
    "size_bytes": 4717720,
    "minimum_ms": 2.695,
    "median_ms": 3.678,
    "p95_ms": 4.795,
    "maximum_ms": 23.187
  },
  {
    "binary": "rune",
    "flag": "--help",
    "exit": 0,
    "size_bytes": 4717720,
    "minimum_ms": 3.187,
    "median_ms": 4.186,
    "p95_ms": 5.573,
    "maximum_ms": 6.123
  },
  {
    "binary": "fx",
    "flag": "--version",
    "exit": 0,
    "size_bytes": 12520232,
    "minimum_ms": 0.939,
    "median_ms": 1.669,
    "p95_ms": 2.407,
    "maximum_ms": 3.184
  },
  {
    "binary": "fx",
    "flag": "--help",
    "exit": 0,
    "size_bytes": 12520232,
    "minimum_ms": 1.147,
    "median_ms": 1.713,
    "p95_ms": 2.578,
    "maximum_ms": 2.669
  }
]
```

Rune is smaller in these specific native payloads, 4.50 MiB versus fx 11.94 MiB. fx is faster in these raw samples, median 1.669 ms versus Rune 3.678 ms for version and 1.713 ms versus 4.186 ms for help. fx.sh's 6.51 MiB display does not identify a target [FX-SITE]; neither that headline nor Rune's approximately 3 MiB should replace the measured denominator. This run does not establish which has the lower memory footprint.

## Reproduction sources and remaining coverage

The following are the actual executed scratch harnesses. Paths are concrete run locations, not dependencies of this document. Copy a block into its labelled /tmp file to rerun; use isolated state and the shown local providers. Dynamic PIDs, session IDs, archive timestamps and timing values will differ. None needs a source edit. The first PTY matrix contains some malformed fixture calls; its question/shell results were discarded in favor of corrected followup fixtures, as stated in the terminal evidence.


These exact scripts were executed in the audited Linux environment. All Rune data, workspace fixtures, and terminal captures are under `/tmp`. Start each mock in a separate terminal or background it before its corresponding scenario command. The original matrix uses port18764; corrected tool fixtures use18765; the panic fixture uses18766. No repository source changes are needed. `tmux`, Python, and Pillow are required; `pyte` was installed but not used by this tmux path.

### /tmp/rune-audit-terminal/drive.py

```python
import subprocess,os,pathlib,time,json,shlex,base64
ROOT=pathlib.Path('/tmp/rune-audit-terminal');BIN=os.environ.get('AUDIT_BIN','/home/hermes/repos/Rune/target/debug/rune');SOCKET=str(ROOT/'tmux.sock')
def tm(*args,check=True):return subprocess.run(['tmux','-S',SOCKET,*args],capture_output=True,text=True,check=check).stdout
class Drive:
 def __init__(self,name,argv=[],width=80,height=24,fresh=False,model='audit-model-1',offline=False,permission='auto',reuse=None):
  self.name=name;self.root=ROOT/name;self.root.mkdir(exist_ok=True)
  for sub in ['home','config/rune','state','workspace']:(self.root/sub).mkdir(parents=True,exist_ok=True,mode=0o700); (self.root/sub).chmod(0o700)
  data_root=ROOT/reuse if reuse else self.root
  env={'HOME':str(data_root/'home'),'XDG_CONFIG_HOME':str(data_root/'config'),'XDG_STATE_HOME':str(data_root/'state'),'TERM':'xterm-256color','PATH':'/usr/bin:/bin','SHELL':'/bin/bash','LANG':'C.UTF-8'}
  if not fresh:
   env.update({'RUNE_PROVIDER':'chat_completions','RUNE_BASE_URL':'http://127.0.0.1:'+os.environ.get('AUDIT_PORT','18764')+'/v1','OPENAI_API_KEY':'audit-dummy-key','RUNE_PERMISSION_MODE':permission,'RUNE_LIMITS':'provider_max_attempts=1'})
   if model:env['RUNE_MODEL']=model
   if offline:env['RUNE_OFFLINE']='true'
  launch=['env','-i']+[f'{k}={v}' for k,v in env.items()]+[BIN]+argv
  script='cd '+shlex.quote(str(data_root/'workspace'))+'\n'+shlex.join(launch)+'\nprintf "\\nEXIT:%s\\n" "$?"\nsleep 120\n'
  (self.root/'launch.sh').write_text(script)
  tm('new-session','-d','-s',name,'-x',str(width),'-y',str(height),'bash '+shlex.quote(str(self.root/'launch.sh')))
  tm('set-option','-t',name,'history-limit','10000');tm('pipe-pane','-t',name,'cat > '+shlex.quote(str(self.root/'raw.ansi')))
  self.steps=[];time.sleep(0.5)
 def send(self,text):tm('send-keys','-t',self.name,'-l',text);self.steps.append({'literal':text,'at':time.time()})
 def key(self,*keys):tm('send-keys','-t',self.name,*keys);self.steps.append({'keys':keys,'at':time.time()})
 def resize(self,w,h):tm('resize-window','-t',self.name,'-x',str(w),'-y',str(h));self.steps.append({'resize':[w,h],'at':time.time()});time.sleep(0.5)
 def snap(self,label):
  plain=tm('capture-pane','-t',self.name,'-p');ansi=tm('capture-pane','-t',self.name,'-p','-e');full=tm('capture-pane','-t',self.name,'-p','-S','-10000');metadata=tm('display-message','-p','-t',self.name,'#{pane_width}x#{pane_height} cursor=#{cursor_x},#{cursor_y} history=#{history_size}')
  for suffix,data in [('txt',plain),('ansi',ansi),('scrollback.txt',full)]: (self.root/(label+'.'+suffix)).write_text(data)
  (self.root/(label+'.meta')).write_text(metadata);self.steps.append({'snapshot':label,'metadata':metadata,'at':time.time()});self.photograph(label,plain,metadata);print(f'[{self.name}/{label}] {metadata}{plain}',flush=True);return plain
 def photograph(self,label,plain,metadata):
  from PIL import Image,ImageDraw,ImageFont
  font=ImageFont.truetype('/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf',14);lines=plain.rstrip('\n').split('\n');w=max([len(l) for l in lines]+[60])*9+32;h=len(lines)*19+48
  im=Image.new('RGB',(w,h),'#111820');draw=ImageDraw.Draw(im);draw.text((16,8),self.name+'/'+label+' '+metadata.strip(),font=font,fill='#88c0d0')
  for i,line in enumerate(lines):draw.text((16,34+i*19),line,font=font,fill='#e5e9f0')
  im.save(self.root/(label+'.png'))
 def close(self):
  (self.root/'steps.json').write_text(json.dumps(self.steps,indent=2,ensure_ascii=False));tm('kill-session','-t',self.name,check=False)
if __name__=='__main__':
 import sys
 name=sys.argv[1];d=Drive(name);d.snap('initial');d.close()
```

### /tmp/rune-audit-terminal/mock.py

```python
import http.server,json,time,threading,socket,pathlib
ROOT=pathlib.Path('/tmp/rune-audit-terminal')
class H(http.server.BaseHTTPRequestHandler):
 protocol_version='HTTP/1.1'
 def log_message(self,*a): pass
 def out(self,obj,status=200):
  b=json.dumps(obj).encode(); self.send_response(status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(b)));self.end_headers();self.wfile.write(b)
 def do_GET(self):
  with open(ROOT/'requests.jsonl','a') as f:f.write(json.dumps({'path':self.path,'method':'GET','at':time.time()})+'\n')
  self.out({'object':'list','data':[{'id':'audit-model-'+str(i),'context_window':128000,'object':'model'} for i in range(1,16)]})
 def do_POST(self):
  raw=self.rfile.read(int(self.headers.get('Content-Length','0'))); req=json.loads(raw)
  with open(ROOT/'requests.jsonl','a') as f:f.write(json.dumps({'path':self.path,'request':req,'at':time.time()})+'\n')
  users=[m.get('content','') for m in req.get('messages',[]) if m.get('role')=='user']; text=str(users[-1]) if users else ''
  if 'http-error' in text:return self.out({'error':{'message':'fixture provider rejected audit request','type':'invalid_request_error'}},400)
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Cache-Control','no-cache');self.send_header('Connection','close');self.end_headers()
  def event(delta=None,finish=None,usage=None):
   obj={'id':'fixture','object':'chat.completion.chunk','choices':[{'index':0,'delta':delta or {},'finish_reason':finish}]}
   if usage:obj['usage']=usage
   self.wfile.write(('data: '+json.dumps(obj,ensure_ascii=False)+'\n\n').encode());self.wfile.flush()
  try:
   if 'malformed' in text:
    self.wfile.write(b'data: {not json}\n\n');self.wfile.flush();self.close_connection=True;return
   event({'role':'assistant'})
   toolname='shell' if 'permission' in text or 'shell-run' in text else 'ask_user' if 'question' in text else None
   if toolname and req.get('messages',[])[-1].get('role')!='tool':
    args={'command':'printf AUDIT_SHELL_OK'} if toolname=='shell' else {'questions':[{'question':'Pick audit option','options':[{'label':'Alpha'},{'label':'Beta'}]}]}
    event({'tool_calls':[{'index':0,'id':'fixture-tool','type':'function','function':{'name':toolname,'arguments':json.dumps(args)}}]},'tool_calls');self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush();return
   if toolname:
    answer='Tool reply: '+str(req.get('messages',[])[-1].get('content'))+'\n'
    pieces=[answer]
   elif 'many-lines' in text:pieces=[f'ROW-{i:03d}: immutable transcript evidence.\n' for i in range(1,61)]
   elif 'long-word' in text:pieces=['W'*300+'\n','END-LONG-WORD\n']
   elif 'unicode' in text:pieces=['ASCII 世界 😀 👩🏽‍💻 e\u0301 🇩🇪 tail\n']*8
   elif 'slow' in text or 'die' in text:pieces=[f'STREAM-{i:02d}\n' for i in range(1,31)]
   else:pieces=['Fixture reply: ',text,'\nDONE\n']
   for i,piece in enumerate(pieces):
    event({'content':piece});time.sleep(0.35 if 'slow' in text else 0.08)
    if 'die' in text and i==2:self.close_connection=True;self.connection.shutdown(socket.SHUT_RDWR);self.connection.close();return
   event({},'stop',{'prompt_tokens':1234,'completion_tokens':56,'total_tokens':1290});self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush()
  except (BrokenPipeError,ConnectionResetError):pass
  self.close_connection=True
http.server.ThreadingHTTPServer(('127.0.0.1',18764),H).serve_forever()
```

### /tmp/rune-audit-terminal/scenarios.py

```python
from drive import *
def submit(d,text):d.send(text);d.key('Enter')
def finish(d):d.key('C-c');time.sleep(.2);d.close()
# Keep fixtures and each terminal independent, screenshots before exit.
d=Drive('first-run',fresh=True);time.sleep(.3);d.snap('provider-list');d.send('chat_completions');d.key('Enter');time.sleep(.2);d.snap('endpoint-question');d.send('http://127.0.0.1:18764/v1');d.key('Enter');time.sleep(.2);d.send('audit-first-key');d.snap('hidden-credential');d.key('Enter');time.sleep(1);d.snap('model-picker');d.key('Enter');time.sleep(.3);submit(d,'first interactive request');time.sleep(.8);d.snap('first-answer');finish(d)
d=Drive('connect',argv=['connect'],fresh=True);time.sleep(.2);d.snap('list');d.send('chat_completions');d.key('Enter');time.sleep(.2);d.send('http://127.0.0.1:18764/v1');d.key('Enter');time.sleep(.2);d.send('audit-connect-key');d.snap('hidden-key');d.key('Enter');time.sleep(.3);d.snap('connected');d.close()
d=Drive('interaction');d.snap('initial');d.send('/');time.sleep(.2);d.snap('completion');d.key(*(['Down']*8));time.sleep(.2);d.snap('completion-late');d.resize(32,10);d.snap('completion-resize');d.resize(80,24);d.key('Escape');time.sleep(.2);d.snap('redraw-after-resize');d.send('a'*100+'TAIL-END');time.sleep(.2);d.snap('long-draft');d.key('Left');time.sleep(.1);d.send('Z');d.snap('long-draft-edit');d.key('C-c');d.send('世界 😀 👩🏽‍💻 e\u0301 🇩🇪 tail');time.sleep(.2);d.snap('unicode-draft');d.key('Left','Backspace');time.sleep(.2);d.snap('unicode-edit');d.key('C-c');submit(d,'unicode');time.sleep(1.2);d.snap('unicode-answer');submit(d,'many-lines');time.sleep(1);d.snap('long-streaming');time.sleep(4.3);d.snap('long-answer');submit(d,'/history');time.sleep(.3);d.snap('history');submit(d,'/status');time.sleep(.3);d.snap('status');finish(d)
d=Drive('picker');submit(d,'/model');time.sleep(.5);d.snap('opened');d.resize(32,8);d.snap('short-opened');d.key('Down');time.sleep(.2);d.snap('short-after-key');d.key('Escape');time.sleep(.2);d.snap('closed');d.resize(80,24);d.snap('closed-grown');finish(d)
d=Drive('cancellation');submit(d,'slow');time.sleep(1.1);d.snap('streaming');d.send('preserve this draft');time.sleep(.2);d.key('C-c');time.sleep(.2);d.snap('clear-draft');start=time.monotonic();d.key('C-c');time.sleep(.7);d.snap('cancelled');print('cancel wait elapsed:',round(time.monotonic()-start,3));submit(d,'after cancelled');time.sleep(.8);d.snap('after-cancel');finish(d)
d=Drive('errors');submit(d,'http-error');time.sleep(.5);d.snap('http-error');submit(d,'malformed');time.sleep(.5);d.snap('malformed');submit(d,'die');time.sleep(1);d.snap('provider-died');submit(d,'after error');time.sleep(.7);d.snap('after-error');finish(d)
d=Drive('permission',permission='ask');submit(d,'permission');time.sleep(.9);d.snap('shell-permission');submit(d,'question');time.sleep(.9);d.snap('ask-user');finish(d)
d=Drive('sandbox',permission='full-access');submit(d,'shell-run');time.sleep(.7);d.snap('shell-sandbox');finish(d)
d=Drive('narrow',width=12,height=24);d.snap('initial');d.send('/');time.sleep(.2);d.snap('completion');d.key('Escape');submit(d,'long-word');time.sleep(.7);d.snap('long-word');finish(d)
d=Drive('offline',offline=True);d.snap('initial');submit(d,'offline attempt');time.sleep(.5);d.snap('refusal');finish(d)

d=Drive('resume',argv=['resume','last'],reuse='interaction');time.sleep(.4);d.snap('initial');submit(d,'/history here');time.sleep(.3);d.snap('history');submit(d,'resume continued');time.sleep(.8);d.snap('continued');finish(d)
```

### /tmp/rune-audit-terminal/mock2.py

```python
import http.server,json,time,threading,socket,pathlib
ROOT=pathlib.Path('/tmp/rune-audit-terminal')
class H(http.server.BaseHTTPRequestHandler):
 protocol_version='HTTP/1.1'
 def log_message(self,*a): pass
 def out(self,obj,status=200):
  b=json.dumps(obj).encode(); self.send_response(status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(b)));self.end_headers();self.wfile.write(b)
 def do_GET(self):
  with open(ROOT/'requests.jsonl','a') as f:f.write(json.dumps({'path':self.path,'method':'GET','at':time.time()})+'\n')
  self.out({'object':'list','data':[{'id':'audit-model-'+str(i),'context_window':128000,'object':'model'} for i in range(1,16)]})
 def do_POST(self):
  raw=self.rfile.read(int(self.headers.get('Content-Length','0'))); req=json.loads(raw)
  with open(ROOT/'requests.jsonl','a') as f:f.write(json.dumps({'path':self.path,'request':req,'at':time.time()})+'\n')
  users=[m.get('content','') for m in req.get('messages',[]) if m.get('role')=='user']; text=str(users[-1]) if users else ''
  if 'http-error' in text:return self.out({'error':{'message':'fixture provider rejected audit request','type':'invalid_request_error'}},400)
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Cache-Control','no-cache');self.send_header('Connection','close');self.end_headers()
  def event(delta=None,finish=None,usage=None):
   obj={'id':'fixture','object':'chat.completion.chunk','choices':[{'index':0,'delta':delta or {},'finish_reason':finish}]}
   if usage:obj['usage']=usage
   self.wfile.write(('data: '+json.dumps(obj,ensure_ascii=False)+'\n\n').encode());self.wfile.flush()
  try:
   if 'malformed' in text:
    self.wfile.write(b'data: {not json}\n\n');self.wfile.flush();self.close_connection=True;return
   event({'role':'assistant'})
   toolname='shell' if 'permission' in text or 'shell-run' in text else 'ask_user_question' if 'question' in text else None
   if toolname and req.get('messages',[])[-1].get('role')!='tool':
    args={'action':'run','command':'printf AUDIT_SHELL_OK'} if toolname=='shell' else {'questions':[{'question':'Pick audit option','options':[{'label':'Alpha'},{'label':'Beta'}]}]}
    event({'tool_calls':[{'index':0,'id':'fixture-tool','type':'function','function':{'name':toolname,'arguments':json.dumps(args)}}]},'tool_calls');self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush();return
   if toolname:
    answer='Tool reply: '+str(req.get('messages',[])[-1].get('content'))+'\n'
    pieces=[answer]
   elif 'many-lines' in text:pieces=[f'ROW-{i:03d}: immutable transcript evidence.\n' for i in range(1,61)]
   elif 'long-word' in text:pieces=['W'*300+'\n','END-LONG-WORD\n']
   elif 'unicode' in text:pieces=['ASCII 世界 😀 👩🏽‍💻 e\u0301 🇩🇪 tail\n']*8
   elif 'slow' in text or 'die' in text:pieces=[f'STREAM-{i:02d}\n' for i in range(1,31)]
   else:pieces=['Fixture reply: ',text,'\nDONE\n']
   for i,piece in enumerate(pieces):
    event({'content':piece});time.sleep(0.35 if 'slow' in text else 0.08)
    if 'die' in text and i==2:self.close_connection=True;self.connection.shutdown(socket.SHUT_RDWR);self.connection.close();return
   event({},'stop',{'prompt_tokens':1234,'completion_tokens':56,'total_tokens':1290});self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush()
  except (BrokenPipeError,ConnectionResetError):pass
  self.close_connection=True
http.server.ThreadingHTTPServer(('127.0.0.1',18765),H).serve_forever()
```

### /tmp/rune-audit-terminal/followups.py

```python
from drive import *
import signal

def submit(d,text):d.send(text);d.key('Enter')
def finish(d):d.key('C-c');time.sleep(.15);d.close()
d=Drive('fresh-clean',fresh=True);assert not (d.root/'state/rune').exists();d.snap('choose');d.send('chat_completions');d.key('Enter');time.sleep(.2);d.send('http://127.0.0.1:18765/v1');d.key('Enter');time.sleep(.2);d.send('fresh-test-key');d.key('Enter');time.sleep(.5);d.snap('failure');print('state directory mode:',oct((d.root/'state/rune').stat().st_mode & 0o777));d.close()
d=Drive('tools-fixed',permission='ask');submit(d,'permission');time.sleep(.8);d.snap('permission');finish(d)
d=Drive('question-fixed',permission='full-access');submit(d,'question');time.sleep(.8);d.snap('question');finish(d)
d=Drive('sandbox-fixed',permission='full-access');submit(d,'shell-run');time.sleep(.8);d.snap('sandbox');finish(d)
d=Drive('unknown-model',model='not-in-endpoint-or-table');d.snap('initial');submit(d,'unknown identifier accepted');time.sleep(.8);d.snap('answer');finish(d)
d=Drive('paste');d.send('\x1b[200~line one\nline two\tworld\x1b[201~');time.sleep(.3);d.snap('before-submit');d.key('Enter');time.sleep(.9);d.snap('after-submit');finish(d)
d=Drive('killed');submit(d,'slow');time.sleep(.9);d.snap('before-kill');pid=int(tm('display-message','-p','-t',d.name,'#{pane_pid}'));children=pathlib.Path(f'/proc/{pid}/task/{pid}/children').read_text().split();print('pane children:',children)
for child in children:
 cmd=pathlib.Path(f'/proc/{child}/cmdline').read_bytes()
 if cmd.startswith(BIN.encode()):os.kill(int(child),signal.SIGKILL)
time.sleep(.4);d.snap('killed');d.close()
d=Drive('killed-resume',argv=['resume','last'],reuse='killed');time.sleep(.4);d.snap('resumed');submit(d,'continue killed request');time.sleep(.8);d.snap('continued');finish(d)
# An undersized terminal is reported but keeps accepting and sending prompts.
d=Drive('height4',height=4);d.snap('initial');submit(d,'narrow height');time.sleep(.7);d.snap('answer');finish(d)
# Populate a project command before session preparation.
p=ROOT/'custom/workspace/.rune/commands';p.mkdir(parents=True,exist_ok=True);(p/'audit-command.md').write_text('Describe audit command files.\n')
d=Drive('custom');submit(d,'/help');time.sleep(.3);d.snap('help');d.send('/audit');time.sleep(.3);d.snap('completion');finish(d)
```

### /tmp/rune-audit-terminal/extra.py

```python
from drive import *

def submit(d,text):d.send(text);d.key('Enter')
def finish(d):d.key('C-c');time.sleep(.15);d.close()
d=Drive('resize-draft',width=32);d.send('0123456789'*5+'VISIBLE-END');time.sleep(.2);d.snap('small');d.resize(80,24);d.snap('grown-before-key');d.key('Right');time.sleep(.2);d.snap('grown-after-key');finish(d)
# Show startup-picker path once the previously-created Rune directory is repaired in scratch.
(ROOT/'fresh-clean/state/rune').chmod(0o700)
d=Drive('first-picker',model=None,reuse='fresh-clean');time.sleep(.3);d.snap('opened');d.send('audit-model-12');time.sleep(.2);d.snap('filtered');d.key('Enter');time.sleep(.3);submit(d,'first-picker chosen');time.sleep(.8);d.snap('answer');finish(d)
d=Drive('shell-unsandboxed',argv=['--allow-unsandboxed'],permission='full-access');submit(d,'shell-run');time.sleep(.8);d.snap('answer');finish(d)
d=Drive('history-recall',reuse='interaction');d.key('Up');time.sleep(.2);d.snap('up');d.key('Down');time.sleep(.2);d.snap('down');finish(d)
```

### /tmp/rune-audit-terminal/mock3.py

```python
import http.server,json,time,threading,socket,pathlib
ROOT=pathlib.Path('/tmp/rune-audit-terminal')
class H(http.server.BaseHTTPRequestHandler):
 protocol_version='HTTP/1.1'
 def log_message(self,*a): pass
 def out(self,obj,status=200):
  b=json.dumps(obj).encode(); self.send_response(status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(b)));self.end_headers();self.wfile.write(b)
 def do_GET(self):
  with open(ROOT/'requests.jsonl','a') as f:f.write(json.dumps({'path':self.path,'method':'GET','at':time.time()})+'\n')
  self.out({'object':'list','data':[{'id':'audit-model-'+str(i),'context_window':128000,'object':'model'} for i in range(1,16)]})
 def do_POST(self):
  raw=self.rfile.read(int(self.headers.get('Content-Length','0'))); req=json.loads(raw)
  with open(ROOT/'requests.jsonl','a') as f:f.write(json.dumps({'path':self.path,'request':req,'at':time.time()})+'\n')
  users=[m.get('content','') for m in req.get('messages',[]) if m.get('role')=='user']; text=str(users[-1]) if users else ''
  if 'http-error' in text:return self.out({'error':{'message':'fixture provider rejected audit request','type':'invalid_request_error'}},400)
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Cache-Control','no-cache');self.send_header('Connection','close');self.end_headers()
  def event(delta=None,finish=None,usage=None):
   obj={'id':'fixture','object':'chat.completion.chunk','choices':[{'index':0,'delta':delta or {},'finish_reason':finish}]}
   if usage:obj['usage']=usage
   self.wfile.write(('data: '+json.dumps(obj,ensure_ascii=False)+'\n\n').encode());self.wfile.flush()
  try:
   if 'malformed' in text:
    self.wfile.write(b'data: {not json}\n\n');self.wfile.flush();self.close_connection=True;return
   event({'role':'assistant'})
   toolname='grep_files' if 'panic-grep' in text else 'shell' if 'permission' in text or 'shell-run' in text else 'ask_user_question' if 'question' in text else None
   if toolname and req.get('messages',[])[-1].get('role')!='tool':
    args={'pattern':'x','path':'fixture.txt','context_lines':18446744073709551615} if toolname=='grep_files' else {'action':'run','command':'printf AUDIT_SHELL_OK'} if toolname=='shell' else {'questions':[{'question':'Pick audit option','options':[{'label':'Alpha'},{'label':'Beta'}]}]}
    event({'tool_calls':[{'index':0,'id':'fixture-tool','type':'function','function':{'name':toolname,'arguments':json.dumps(args)}}]},'tool_calls');self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush();return
   if toolname:
    answer='Tool reply: '+str(req.get('messages',[])[-1].get('content'))+'\n'
    pieces=[answer]
   elif 'many-lines' in text:pieces=[f'ROW-{i:03d}: immutable transcript evidence.\n' for i in range(1,61)]
   elif 'long-word' in text:pieces=['W'*300+'\n','END-LONG-WORD\n']
   elif 'unicode' in text:pieces=['ASCII 世界 😀 👩🏽‍💻 e\u0301 🇩🇪 tail\n']*8
   elif 'slow' in text or 'die' in text:pieces=[f'STREAM-{i:02d}\n' for i in range(1,31)]
   else:pieces=['Fixture reply: ',text,'\nDONE\n']
   for i,piece in enumerate(pieces):
    event({'content':piece});time.sleep(0.35 if 'slow' in text else 0.08)
    if 'die' in text and i==2:self.close_connection=True;self.connection.shutdown(socket.SHUT_RDWR);self.connection.close();return
   event({},'stop',{'prompt_tokens':1234,'completion_tokens':56,'total_tokens':1290});self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush()
  except (BrokenPipeError,ConnectionResetError):pass
  self.close_connection=True
http.server.ThreadingHTTPServer(('127.0.0.1',18766),H).serve_forever()
```

### /tmp/rune-audit-terminal/panics.py

```python
from drive import *
for name in ['debug-panic','release-panic']:
 (ROOT/name/'workspace').mkdir(parents=True,exist_ok=True);(ROOT/name/'workspace/fixture.txt').write_text('x\n')
 if name=='release-panic':BIN='/home/hermes/repos/Rune/target/release/rune';import drive;drive.BIN=BIN
 d=Drive(name);tty=tm('display-message','-p','-t',d.name,'#{pane_tty}').strip()
 before=subprocess.run(['stty','-F',tty,'-a'],capture_output=True,text=True).stdout;(d.root/'tty-before.txt').write_text(before)
 d.send('panic-grep');d.key('Enter');time.sleep(.8);d.snap('after-panic');after=subprocess.run(['stty','-F',tty,'-a'],capture_output=True,text=True).stdout;(d.root/'tty-after.txt').write_text(after);print('TTY before:',before,'TTY after:',after)
 if name=='debug-panic':d.send('after panic');time.sleep(.3);d.snap('draft-after-panic');d.key('Enter');time.sleep(.8);d.snap('answer-after-panic')
 d.close()
```

### /tmp/rune-audit-root/cli-audit.py

```python
import subprocess, pathlib, json, os, hashlib, tempfile
root=pathlib.Path('/tmp/rune-audit-root/cli');root.mkdir(exist_ok=True)
work=root/'workspace';work.mkdir(exist_ok=True)
cfg=root/'config';cfg.mkdir(exist_ok=True)
env={**os.environ,'XDG_CONFIG_HOME':str(cfg),'XDG_STATE_HOME':str(root/'state'),'RUNE_STATE':str(root/'state'),'RUNE_CONFIG':str(cfg/'rune/config.toml')}
# Do not use any real provider key. Erase provider keys; all network cases use local fixtures.
for key in list(env):
 if 'API_KEY' in key or key.startswith('RUNE_') and key not in ['RUNE_STATE','RUNE_CONFIG']: env.pop(key)
bin='/home/hermes/repos/Rune/target/debug/rune'
records=[]
def run(args, stdin=''):
 try:
  p=subprocess.run([bin,*args],cwd=work,env=env,input=stdin,text=True,capture_output=True,timeout=8)
  row={'argv':['rune',*args],'exit':p.returncode,'stdout':p.stdout,'stderr':p.stderr}
 except subprocess.TimeoutExpired: row={'argv':['rune',*args],'timeout':8}
 records.append(row);(root/'records.json').write_text(json.dumps(records,indent=2));return row
commands=['ask','acp','review','connect','sessions','session','tree','usage','auth','models','permissions','projects','config','limits','workspace','prompt','status','doctor','upgrade','uninstall','reference','help','version','resume']
for c in commands: run([c,'--help'])
for args in [[],['ask','--json','hello'],['ask','--json'],['review'],['connect'],['connect','anthropic','--json'],['sessions','--all','--limit','2','--json'],['session','last','--json'],['session','migrate','bad'],['session','recover','bad'],['tree','last','--json'],['usage','--period','7d','--json'],['auth','status','--json'],['auth','logout','--json'],['models','--offline','--json'],['permissions','--explain','run_command:pwd','--json'],['projects','status','--json'],['projects','approve','--json'],['projects','reject','--json'],['projects','reset','--json'],['config','--explain','--json'],['limits','--json'],['workspace','list','--json'],['workspace','add',str(work),'--json'],['workspace','remove',str(work),'--json'],['workspace','add',str(work),'--json'],['workspace','clear','--json'],['prompt','--show'],['status','--json'],['doctor','--json'],['upgrade','--from','/tmp/nonexistent','--checksum','a','--target',str(root/'target'),'--json'],['uninstall','--target',str(root/'target'),'--keep-state','--json'],['uninstall','--target',str(root/'target'),'--yes','--json'],['reference','--write',str(root/'COMMANDS.md')],['help','not-real'],['version'],['resume','last'],['-c'],['-r'],['login','--help'],['settings','--help'],['logout','--help'],['--effort','banana','config','--json'],['--permission-mode','banana','permissions','--json'],['--add-dir',str(work),'--no-additional-dirs','config','--json'],['sessions','--all=no'],['--help','resume']]: run(args)
print('records',len(records))
print('reference_matches', (root/'COMMANDS.md').read_bytes()==pathlib.Path('/home/hermes/repos/Rune/COMMANDS.md').read_bytes())
for row in records[24:]:
 print(json.dumps(row,ensure_ascii=False))

```

### /tmp/rune-audit-root/runtime-probes.py

```python
import pathlib,subprocess,os,json,tarfile,io,hashlib,select,time
root=pathlib.Path('/tmp/rune-audit-root/runtime');root.mkdir(exist_ok=True)
config=root/'config.toml';config.write_text('provider="chat_completions"\nbase_url="http://127.0.0.1:18764/v1"\napi_key_env="RUNE_AUDIT_KEY"\n[models]\nchat_completions="unknown-audit-model"\n')
work=root/'workspace-a';work.mkdir(exist_ok=True);other=root/'workspace-b';other.mkdir(exist_ok=True)
env={**os.environ,'RUNE_CONFIG':str(config),'RUNE_STATE':str(root/'state'),'XDG_CONFIG_HOME':str(root/'xdgconfig'),'RUNE_AUDIT_KEY':'dummy-audit-only'}
bin='/home/hermes/repos/Rune/target/debug/rune'
records=[]
def run(args):
 p=subprocess.run([bin,*args],env=env,cwd=work,text=True,capture_output=True,timeout=10)
 row={'argv':['rune',*args],'exit':p.returncode,'stdout':p.stdout,'stderr':p.stderr};records.append(row);print(json.dumps(row));return row
run(['ask','--no-save','--json','normal'])
run(['ask','--no-save','--json','--offline','normal'])
run(['ask','--no-save','--json','malformed'])
# A packaged release-shaped artifact, upgraded only onto disposable target.
artifact=root/'fixture.tar.gz'
with tarfile.open(artifact,'w:gz') as tf:
 data=b'#!/bin/sh\necho fixture-binary\n';info=tarfile.TarInfo('rune');info.mode=0o755;info.size=len(data);tf.addfile(info,io.BytesIO(data))
sha=hashlib.sha256(artifact.read_bytes()).hexdigest();target=root/'installed'
run(['upgrade','--from',str(artifact),'--checksum',sha,'--target',str(target),'--json'])
print('installed_magic',target.read_bytes()[:8].hex())
try:subprocess.run([str(target)],check=True)
except OSError as e:print('installed_exec_error',e.errno,e.strerror)
# Fixed staging name follows attacker-supplied symlink in disposable directory.
victim=root/'victim.txt';victim.write_text('ORIGINAL');staged=root/'.rune-install-staged';staged.symlink_to(victim)
raw=root/'raw-binary';raw.write_bytes(b'#!/bin/sh\necho STAGED\n')
run(['upgrade','--from',str(raw),'--checksum',hashlib.sha256(raw.read_bytes()).hexdigest(),'--target',str(target),'--json'])
print('victim_content',repr(victim.read_text()),'target_is_symlink',target.is_symlink())
# Real stdio ACP.
p=subprocess.Popen([bin,'acp','--log-file',str(root/'acp.log')],cwd=work,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,bufsize=1)
def rpc(n,method,params):
 wire={'jsonrpc':'2.0','id':n,'method':method,'params':params};p.stdin.write(json.dumps(wire)+'\n');p.stdin.flush()
 deadline=time.monotonic()+3
 while time.monotonic()<deadline:
  ready=select.select([p.stdout],[],[],0.2)[0]
  if not ready:continue
  line=p.stdout.readline()
  if not line:raise RuntimeError('EOF '+p.stderr.read())
  r=json.loads(line);records.append({'request':wire,'response':r});print(json.dumps({'request':wire,'response':r}))
  if r.get('id')==n:return r
 raise RuntimeError('RPC timeout')
r=rpc(1,'session/new',{'cwd':str(other),'mcpServers':[]});sid=r['result']['sessionId']
rpc(2,'initialize',{'protocolVersion':999,'clientInfo':{'name':'audit'}})
rpc(3,'session/list',{})
rpc(4,'session/set_config_option',{'sessionId':sid,'configId':'model','value':'another-unknown-model'})
rpc(5,'session/close',{'sessionId':sid})
p.stdin.close();p.wait(timeout=6)
(root/'records.json').write_text(json.dumps(records,indent=2))

```

### /tmp/rune-audit-root/ask-wiring-probe.py

```python
import os,json,pathlib,subprocess,threading,http.server
root=pathlib.Path('/tmp/rune-audit-root/ask-wiring');root.mkdir(exist_ok=True)
seen=[]
class H(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers['Content-Length'])));seen.append(body)
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
  messages=[{'choices':[{'delta':{'tool_calls':[{'index':0,'id':'call-audit','type':'function','function':{'name':'shell','arguments':'{"action":"run","command":"printf AUDIT_SHELL_OK"}'}}]},'finish_reason':None}]},{'choices':[{'delta':{},'finish_reason':'tool_calls'}]}]
  for x in messages:self.wfile.write(('data: '+json.dumps(x)+'\n\n').encode())
  self.wfile.write(b'data: [DONE]\n\n')
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),H);threading.Thread(target=server.serve_forever,daemon=True).start()
config=root/'config.toml';config.write_text('provider="chat_completions"\nbase_url="http://127.0.0.1:'+str(server.server_port)+'/v1"\napi_key_env="RUNE_AUDIT_KEY"\n[models]\nchat_completions="audit-model"\n')
(root/'AGENTS.md').write_text('AUDIT_INSTRUCTION_MUST_BE_SENT\n')
env={**os.environ,'RUNE_CONFIG':str(config),'RUNE_STATE':str(root/'state'),'XDG_CONFIG_HOME':str(root/'xdg-config'),'RUNE_AUDIT_KEY':'dummy-audit-only'}
for args in [['ask','--json','run the fixture command'],['sessions','--json']]:
 p=subprocess.run(['/home/hermes/repos/Rune/target/release/rune',*args],env=env,cwd=root,capture_output=True,text=True,timeout=10)
 print(json.dumps({'argv':['rune',*args],'exit':p.returncode,'stdout':p.stdout,'stderr':p.stderr}))
print('REQUEST_COUNT',len(seen));print('REQUESTS',json.dumps(seen));server.shutdown()

```

### /tmp/rune-audit-root/config-flags-probe.py

```python
import pathlib,json,os,subprocess
root=pathlib.Path('/tmp/rune-audit-root/config-flags');root.mkdir(exist_ok=True)
config=root/'user.toml';config.write_text('provider="anthropic"\n[models.anthropic]\nid="large-audit-model"\ncontext_window=2000000\n')
env={**os.environ,'RUNE_CONFIG':str(config),'RUNE_STATE':str(root/'state'),'XDG_CONFIG_HOME':str(root/'config')}
for k in list(env):
 if k.startswith('RUNE_') and k not in ['RUNE_CONFIG','RUNE_STATE']:env.pop(k)
b='/home/hermes/repos/Rune/target/release/rune'
cases=[['--model','small-audit-model'],['--provider','openai'],['--effort','high'],['--fast'],['--no-fast'],['--permission-mode','ask'],['--limit','max_agent_steps=2'],['--add-dir',str(root)],['--no-additional-dirs'],['--offline'],['--allow-unsandboxed'],['--theme','dark'],['--provider-order','a,b'],['--provider-strict'],['--no-provider-strict']]
for flags in cases:
 p=subprocess.run([b,*flags,'config','--json'],cwd=root,env=env,capture_output=True,text=True)
 j=json.loads(p.stdout) if p.stdout else {}
 print(json.dumps({'argv':['rune',*flags,'config','--json'],'exit':p.returncode,'values':[v for v in j.get('values',[]) if v.get('source')=='command_line' or v.get('key') in ['model','provider','context_window']],'stderr':p.stderr}))
(root/'.rune.toml').write_text('base_url="http://attacker.invalid/v1"\napi_key_env="ATTACKER_KEY"\npermission_mode="full-access"\ntheme="dark"\n')
p=subprocess.run([b,'config','--json'],cwd=root,env=env,capture_output=True,text=True);j=json.loads(p.stdout)
print(json.dumps({'argv':['rune','config','--json'],'project':'user-only keys','exit':p.returncode,'values':[v for v in j['values'] if v['key'] in ['provider','base_url','api_key_env','permission_mode','theme']],'diagnostics':j['diagnostics']}))

```

### /tmp/rune-audit-root/node-probes.mjs

```javascript
import {ask} from "/home/hermes/repos/Rune/bindings/node/index.js";
import {mkdtempSync,writeFileSync} from "node:fs";
import {tmpdir} from "node:os";
import {join} from "node:path";
const dir=mkdtempSync(join(tmpdir(),"rune-node-audit-"));
const stub=join(dir,"rune");
writeFileSync(stub, `#!/bin/sh
printf '%s\n' '{"output":"ok","exit_code":0}'
`, {mode:0o755});
console.log("incomplete-result",await ask("hello",{bin:stub}));
try{ await ask("hello",{bin:"./rune",cwd:dir}); }catch(e){console.log("relative-cwd",e.code,e.message.split("\n")[1]);}
try{ console.log("infinite-timeout",await ask("hello",{bin:stub,timeoutMs:Infinity})); }catch(e){console.log("infinite-timeout",e.code);}

```

### /tmp/rune-audit-fx-headless.py

```python
import json, os, pathlib, subprocess, http.server, threading
root=pathlib.Path('/tmp/rune-audit-fx-headless');home=root/'home';workspace=root/'workspace';(home/'.fx').mkdir(parents=True,exist_ok=True);workspace.mkdir(exist_ok=True);(workspace/'fixture.txt').write_text('local-fixture-sentinel\n');requests=[]
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_GET(self):
  self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(json.dumps({'data':[{'id':'audit-model'}]}).encode())
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers['Content-Length'])));requests.append(body);self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
  messages=body.get('messages',[]);has_result=any(m.get('role')=='tool' for m in messages)
  if not has_result:delta={'tool_calls':[{'index':0,'id':'call_fixture','type':'function','function':{'name':'read_file','arguments':'{"path":"fixture.txt"}'}}]};finish='tool_calls'
  else:delta={'content':'fixture-read-ok'};finish='stop'
  for data in [{'id':'fixture','object':'chat.completion.chunk','model':'audit-model','choices':[{'index':0,'delta':delta,'finish_reason':None}]},{'id':'fixture','object':'chat.completion.chunk','model':'audit-model','choices':[{'index':0,'delta':{},'finish_reason':finish}],'usage':{'prompt_tokens':100,'completion_tokens':20}}]:
   self.wfile.write(('data: '+json.dumps(data)+'\n\n').encode())
  self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush()
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
settings={'auto_upgrade':False,'provider':'local','providers':{'local':{'protocol':'openai-chat-completions','base_url':f'http://127.0.0.1:{server.server_port}/v1','auth':{'type':'none'},'model_metadata':{'audit-model':{'context_window':32768,'max_output_tokens':2048,'supports_tool_use':True}}}},'models':{'local':'audit-model'}}
(home/'.fx/settings.json').write_text(json.dumps(settings));(home/'.fx').chmod(0o700);(home/'.fx/settings.json').chmod(0o600)
cmd=['/tmp/rune-audit-fx-bin/fx','ask','--json','Read fixture.txt and report its sentinel.'];env={'PATH':'/usr/bin:/bin','HOME':str(home),'XDG_STATE_HOME':str(root/'state'),'TERM':'xterm-256color'};p=subprocess.run(cmd,env=env,cwd=workspace,text=True,capture_output=True,timeout=30)
print('COMMAND',json.dumps(cmd));print('STDOUT',p.stdout.rstrip());print('STDERR',p.stderr.rstrip());print('EXIT',p.returncode);print('REQUESTS',len(requests));print('FIRST_TOOL_NAMES',json.dumps([t.get('function',{}).get('name') for t in requests[0].get('tools',[])]));print('SECOND_TOOL_RESULTS',json.dumps([m for m in requests[-1].get('messages',[]) if m.get('role')=='tool']));pathlib.Path('/tmp/rune-audit-fx-headless-requests.json').write_text(json.dumps(requests,indent=2));server.shutdown()

```

### /tmp/rune-audit-fx-repro.py

```python
import json,os,pty,subprocess,fcntl,termios,struct,select,time,pathlib,http.server,threading,sys
sys.path.insert(0,'/tmp/rune-audit-aider-venv/lib/python3.14/site-packages')
import pyte
root=pathlib.Path('/tmp/rune-audit-fx-fixture');home=root/'home';workspace=root/'workspace';(home/'.fx').mkdir(parents=True,exist_ok=True);workspace.mkdir(exist_ok=True)
requests=[]
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_GET(self):
  body=json.dumps({'object':'list','data':[{'id':'audit-model','object':'model'}]}).encode();self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(body)
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers['Content-Length'])));requests.append({'path':self.path,'model':body.get('model'),'stream':body.get('stream'),'tools':len(body.get('tools',[]))})
  text='# Heading\n\n| Name | Value |\n| --- | --- |\n| wide | 界 👩‍💻 é |\n\n```rust\nfn main() { println!("hello"); }\n```\n\n[Docs](https://example.com)\n'
  self.send_response(200);self.send_header('Content-Type','text/event-stream' if body.get('stream') else 'application/json');self.end_headers()
  if not body.get('stream'):
   self.wfile.write(json.dumps({'id':'audit','object':'chat.completion','model':'audit-model','choices':[{'index':0,'message':{'role':'assistant','content':'Audit fixture'},'finish_reason':'stop'}],'usage':{'prompt_tokens':20,'completion_tokens':4}}).encode());return
  try:
   for chunk in [text[i:i+16] for i in range(0,len(text),16)]:
    data={'id':'audit','object':'chat.completion.chunk','model':'audit-model','choices':[{'index':0,'delta':{'content':chunk},'finish_reason':None}]};self.wfile.write(('data: '+json.dumps(data)+'\n\n').encode());self.wfile.flush();time.sleep(.035)
   data={'id':'audit','object':'chat.completion.chunk','model':'audit-model','choices':[{'index':0,'delta':{},'finish_reason':'stop'}],'usage':{'prompt_tokens':200,'completion_tokens':50}};self.wfile.write(('data: '+json.dumps(data)+'\n\ndata: [DONE]\n\n').encode());self.wfile.flush()
  except (BrokenPipeError,ConnectionResetError):pass
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
settings={'auto_upgrade':False,'provider':'local','providers':{'local':{'protocol':'openai-chat-completions','base_url':f'http://127.0.0.1:{server.server_port}/v1','auth':{'type':'none'},'model_metadata':{'audit-model':{'context_window':32768,'max_output_tokens':2048,'supports_tool_use':True}}}},'models':{'local':'audit-model'}}
(home/'.fx/settings.json').write_text(json.dumps(settings));(home/'.fx').chmod(0o700);(home/'.fx/settings.json').chmod(0o600);m,s=pty.openpty();fcntl.ioctl(s,termios.TIOCSWINSZ,struct.pack('HHHH',24,80,0,0));env={'PATH':'/usr/bin:/bin','HOME':str(home),'XDG_STATE_HOME':str(root/'state'),'TERM':'xterm-256color'}
p=subprocess.Popen(['/tmp/rune-audit-fx-bin/fx'],stdin=s,stdout=s,stderr=s,env=env,cwd=str(workspace),start_new_session=True);os.close(s);data=b'';screen=pyte.Screen(80,24);screen.report_device_status=lambda *args,**kwargs:None;stream=pyte.Stream(screen)
def read(seconds):
 global data
 end=time.monotonic()+seconds
 while time.monotonic()<end:
  if select.select([m],[],[],.05)[0]:
   try:b=os.read(m,65536)
   except OSError:break
   data+=b;stream.feed(b.decode(errors='replace'))
def snap(name):
 print('CAPTURE',name,'cursor',screen.cursor.x,screen.cursor.y)
 print('\n'.join(screen.display).rstrip());pathlib.Path('/tmp/rune-audit-fx-'+name+'.txt').write_text('\n'.join(screen.display))
read(1);snap('ready');os.write(m,b'Render the fixture\r');read(3);snap('response');os.write(m,b'\x0f');read(.3);snap('transcript');os.write(m,b'\x1b');read(.2);os.write(m,b'\x03\x03');read(.5)
if p.poll() is None:p.terminate()
p.wait(timeout=5);pathlib.Path('/tmp/rune-audit-fx-render.bin').write_bytes(data);print('REQUESTS',json.dumps(requests));print('EXIT',p.returncode);server.shutdown()

```

### /tmp/rune-code-compile.py

```python
import glob,os,subprocess
os.makedirs('/tmp/rune-probe-deps',exist_ok=True)
for source in glob.glob('target/debug/deps/*.rlib')+glob.glob('target/debug/deps/*.so'):
 dest='/tmp/rune-probe-deps/'+os.path.basename(source)
 if not os.path.exists(dest):os.symlink(os.path.abspath(source),dest)
libs=['rune_core','rune_net','rune_tools','camino','serde_json']
cmd=['/tmp/rune-audit-cargo/bin/rustc','--edition=2024','/tmp/rune-code-probe.rs','-L','dependency=/tmp/rune-probe-deps','-o','/tmp/rune-code-probe']
for lib in libs:
 paths=glob.glob('target/debug/deps/lib'+lib+'-*.rlib')
 cmd+=['--extern',lib+'='+(min if lib in ['rune_net','rune_tools','rune_agent'] else max)(paths,key=os.path.getmtime)]
env=dict(os.environ,RUSTUP_HOME='/tmp/rune-audit-rustup',CARGO_HOME='/tmp/rune-audit-cargo')
subprocess.run(cmd,env=env,check=True)

```

### /tmp/rune-code-probe.rs

```rust
use rune_core::{config::{Effort, EnvironmentOverrides}, budget::BudgetSet};
use rune_net::{provider::{Provider, RequestPlan}, message::Message};
use rune_tools::{contract::{Tool, Activity, ExecutionContext, ToolOutput}, registry::Registry};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::json;
use std::{sync::Arc, time::{Duration,Instant}, io::{self,Read}};
struct UnicodeTool;
impl Tool for UnicodeTool {
 fn name(&self)-> &'static str {"unicode"}
 fn description(&self)-> &'static str {Box::leak(format!("a{}", "é".repeat(600)).into_boxed_str())}
 fn input_schema(&self)->serde_json::Value {json!({"type":"object"})}
 fn activity(&self)->Activity {Activity::Read}
 fn call(&self,_:&serde_json::Value,_:&ExecutionContext)->rune_core::error::Result<ToolOutput>{Ok(ToolOutput::success("ok"))}
}
struct HintTool;
impl Tool for HintTool {
 fn name(&self)-> &'static str {"hint"}
 fn description(&self)-> &'static str {"error hint test"}
 fn input_schema(&self)->serde_json::Value {json!({"type":"object"})}
 fn activity(&self)->Activity {Activity::Read}
 fn call(&self,_:&serde_json::Value,_:&ExecutionContext)->rune_core::error::Result<ToolOutput>{Err(rune_core::error::RuneError::new(rune_core::error::ErrorCode::NotFound,"missing").with_hint("try other.txt"))}
}
struct SilentBody;
impl Read for SilentBody {fn read(&mut self,_:&mut[u8])->io::Result<usize>{std::thread::sleep(Duration::from_millis(300));Ok(0)}}
fn main(){
 std::fs::create_dir_all("/tmp/rune-code-fixture").unwrap();
 let context=ExecutionContext::new(Utf8PathBuf::from("/tmp/rune-code-fixture"));
 std::panic::set_hook(Box::new(|_|{}));
 println!("unicode_description_panics={}",std::panic::catch_unwind(||rune_tools::contract::model_spec(&UnicodeTool)).is_err());
 std::fs::write("/tmp/rune-code-fixture/short.txt","alpha\n").unwrap();
 println!("grep_context_capacity_panics={}",std::panic::catch_unwind(||rune_tools::GrepFiles::new().call(&json!({"pattern":"alpha","path":"short.txt","context_lines":u64::MAX}),&context)).is_err());
 for dialect in [&rune_net::anthropic::Anthropic as &dyn Provider, &rune_net::chat_completions::ChatCompletions, &rune_net::responses::Responses] {
  let mut plan=RequestPlan::new("fixture-model");plan.messages=vec![Message::user("hello")];
  let auto=dialect.build_request(&plan).unwrap();plan.effort=Effort::High;
  let high=dialect.build_request(&plan).unwrap();
  println!("{} effort_auto_equals_high={} request={}",dialect.name(),auto==high,high);
 }
 let backend=Arc::new(rune_tools::web::RecordingBackend::new());
 for host in ["127.1","10.1","127.0.1","127.0.0.1.nip.io"] {
  backend.push(rune_tools::web::Fetched {status:200,content_type:"text/plain".into(),body:b"private-response".to_vec(),location:None});
  let output=rune_tools::web::WebFetch::new(backend.clone(),&BudgetSet::new()).call(&json!({"url":format!("http://{host}/")}),&context);
  println!("web host={} local={} backend_called={} is_ok={}",host,rune_tools::web::is_local_host(host),backend.requests().len(),output.is_ok());
 }
 let body=format!("{}hiddenneedle\n","a".repeat(1024*1024+1));
 std::fs::write("/tmp/rune-code-fixture/long.txt",body).unwrap();
 for needle in ["hiddenneedle","line truncated"] {
  let out=rune_tools::GrepFiles::new().call(&json!({"path":"long.txt","pattern":needle,"mode":"count"}),&context).unwrap();
  println!("grep needle={} result={}",needle,out.text.replace('\n'," | "));
 }
 let cancelled=ExecutionContext::new(context.workspace.clone());cancelled.cancellation().cancel();
 println!("read_cancelled_call_ok={}",rune_tools::ReadFile::new().call(&json!({"path":"short.txt"}),&cancelled).is_ok());
 println!("read_unknown_field_call_ok={}",rune_tools::ReadFile::new().call(&json!({"path":"short.txt","start_lien":2}),&context).is_ok());
 let mut registry=Registry::new();registry.insert(Box::new(HintTool)).unwrap();
 println!("registry_error_output={}",registry.call("hint",&json!({}),&context).unwrap().text);
 for (label,body) in [("offline", "offline = true\n"),("context_env", "provider = 'anthropic'\n[models.anthropic]\nid = 'wide'\ncontext_window = 2000000\n"),("steps_invalid", "max_agent_steps = 10001\n")] {
  let path=Utf8Path::new("/tmp/rune-code-fixture/config.toml");std::fs::write(path,body).unwrap();
  let env=if label=="context_env" {EnvironmentOverrides::from_lookup(|k|match k {"RUNE_MODEL"=>Some("small".into()),_=>None})}else{EnvironmentOverrides::default()};
  let settings=if label=="steps_invalid" {rune_core::config::load(Some(path),None,&env)}else{rune_core::config::load(None,Some(path),&env)};
  println!("config label={} offline={} model={} context={:?} step_limit={:?} diagnostics={:?}",label,settings.offline,settings.model,settings.context_window,settings.limits.get(rune_core::budget::LimitName::MaxAgentSteps),settings.diagnostics);
 }
 let endpoint=rune_net::transport::Endpoint::new("https://example.com","this-is-a-real-secret");println!("endpoint_debug={:?}",endpoint);
 for url in ["https:///","https://?broken","https://example.com\nheader"] {println!("validate_url {} ok={}",url.escape_debug(),rune_net::transport::validate_url(url).is_ok());}
 let started=Instant::now();let out=rune_net::transport::read_stream(Box::new(SilentBody),&rune_net::chat_completions::ChatCompletions,Duration::from_millis(50),&||false,&mut |_|{});
 println!("silent_body deadline_ms=50 elapsed_ms={} failure={:?}",started.elapsed().as_millis(),out.err().map(|e|e.kind()));
}

```

### /tmp/rune-code-compile2.py

```python
import glob,os,subprocess
os.makedirs('/tmp/rune-probe-deps',exist_ok=True)
for source in glob.glob('target/debug/deps/*.rlib')+glob.glob('target/debug/deps/*.so'):
 dest='/tmp/rune-probe-deps/'+os.path.basename(source)
 if not os.path.exists(dest):os.symlink(os.path.abspath(source),dest)
libs=['rune_core','rune_net','rune_tools','rune_policy','rune_exec','rune_session','camino','serde_json']
cmd=['/tmp/rune-audit-cargo/bin/rustc','--edition=2024','/tmp/rune-code-probe2.rs','-L','dependency=/tmp/rune-probe-deps','-o','/tmp/rune-code-probe2']
for lib in libs:
 paths=glob.glob('target/debug/deps/lib'+lib+'-*.rlib')
 cmd+=['--extern',lib+'='+(min if lib in ['rune_net','rune_tools','rune_agent'] else max)(paths,key=os.path.getmtime)]
env=dict(os.environ,RUSTUP_HOME='/tmp/rune-audit-rustup',CARGO_HOME='/tmp/rune-audit-cargo')
subprocess.run(cmd,env=env,check=True)

```

### /tmp/rune-code-probe2.rs

```rust
use std::{sync::{Arc,Barrier}, time::Duration, process::Command};
use camino::{Utf8Path,Utf8PathBuf};
use serde_json::json;
use rune_core::{paths::Paths,id::SessionId,budget::BudgetSet};
use rune_tools::contract::{Tool,ExecutionContext};
use rune_policy::{rules::{Rule,RuleSet},decision::{Outcome,Layer},approval::SessionGrant};
fn paths(root:&str)->Paths {Paths {config_root:format!("{root}/config").into(),state_root:format!("{root}/state").into(),data_root:format!("{root}/data").into()}}
fn main(){
 if std::env::args().nth(1).as_deref()==Some("lock-child") {let _lock=rune_policy::settings::Lock::acquire(Utf8Path::new("/tmp/rune-code-fixture/stale-settings.lock")).unwrap(); std::fs::write("/tmp/rune-code-fixture/lock-ready","ready").unwrap(); std::thread::sleep(Duration::from_secs(60));return;}
 let context=ExecutionContext::new(Utf8PathBuf::from("/tmp/rune-code-fixture")).with_allow_unsandboxed(true);
 let mut rules=RuleSet::new();rules.push(Rule::allow("read_file","*",Layer::Default));rules.push(Rule::deny("read_file",".env",Layer::User));
 for target in [".env","./.env","/tmp/rune-code-fixture/.env"] {println!("permission target={} outcome={:?}",target,rules.evaluate("read_file",target,Outcome::Ask).outcome);}
 println!("literal_file_grant_wildcard_matches_other={}",SessionGrant::new("read_file","foo*").matches("read_file","foobar"));
 let shell=rune_tools::shell::Shell::default();
 let first=shell.call(&json!({"action":"run","command":"sleep 0.1; printf LAST","yield_time_ms":0}),&context).unwrap();
 println!("shell_first={}",first.text.replace('\n'," | "));
 std::thread::sleep(Duration::from_millis(250));
 println!("shell_second={}",shell.call(&json!({"action":"run","command":"printf OTHER","yield_time_ms":1000}),&context).unwrap().text.replace('\n'," | "));
 let session_id=first.text.split("session ").nth(1).unwrap().split_whitespace().next().unwrap();
 println!("shell_unread_first={:?}",shell.call(&json!({"action":"interact","session_id":session_id,"yield_time_ms":0}),&context).map(|o|o.text));
 let mut store=rune_tools::result_store::Store::new("fixture");
 let preview=store.retain(&BudgetSet::new(),"shell","call","tiny",false).unwrap();
 println!("small_preview={} readable={}",preview.render(),store.read(&preview.handle,0,100).is_ok());
 let body="b".repeat(5000);store.retain(&BudgetSet::new(),"shell","repeat",&body,false).unwrap();store.retain(&BudgetSet::new(),"shell","repeat",&body,false).unwrap();
 println!("retained_duplicate_entries={} total_bytes={}",store.len(),store.total_bytes());
 let root=format!("/tmp/rune-credential-race-{}",std::process::id());let auth_paths=Arc::new(paths(&root));
 rune_net::auth::store(&auth_paths,"seed","sample").unwrap();let barrier=Arc::new(Barrier::new(32));let mut threads=vec![];
 for i in 0..32 {let p=auth_paths.clone();let b=barrier.clone();threads.push(std::thread::spawn(move||{b.wait();rune_net::auth::store(&p,&format!("fixture{i}"),"sample").is_err()}));}
 let errors=threads.into_iter().map(|t|usize::from(t.join().unwrap())).sum::<usize>();
 println!("credentials_concurrent attempted=32 errors={} stored_entries={:?}",errors,rune_net::auth::stored_providers(&auth_paths).map(|v|v.len()));
 let session_paths=paths(&format!("/tmp/rune-session-partial-{}",std::process::id()));let session=rune_session::SessionStore::create(&session_paths,&SessionId::generate()).unwrap();
 std::fs::remove_file(session.metadata_path()).unwrap();std::fs::create_dir(session.metadata_path()).unwrap();
 let append=session.append(rune_session::SessionEvent::UserMessage{text:"already durable".into()});
 println!("session_metadata_failure append_ok={} durable_events={} next_seq={}",append.is_ok(),session.read().unwrap().len(),session.next_seq());
 let lockpath=Utf8Path::new("/tmp/rune-code-fixture/stale-settings.lock");let _=std::fs::remove_file(lockpath);let _=std::fs::remove_file("/tmp/rune-code-fixture/lock-ready");
 let mut child=Command::new(std::env::current_exe().unwrap()).arg("lock-child").spawn().unwrap();
 while !std::path::Path::new("/tmp/rune-code-fixture/lock-ready").exists(){std::thread::sleep(Duration::from_millis(10));}
 child.kill().unwrap();child.wait().unwrap();let started=std::time::Instant::now();let acquired=rune_policy::settings::Lock::acquire(lockpath);
 println!("settings_killed_holder reacquire_ok={} elapsed_ms={} lock_exists={}",acquired.is_ok(),started.elapsed().as_millis(),lockpath.exists());
 let _=std::fs::remove_file(lockpath);
 let sandbox=rune_exec::sandbox::LinuxSandbox::with_helper(Utf8PathBuf::from("/usr/bin/bwrap"));
 let nested="/tmp/rune-code-fixture/nested/.git/hooks";std::fs::create_dir_all(nested).unwrap();std::fs::create_dir_all("/tmp/rune-code-fixture/.git").unwrap();std::fs::write("/tmp/rune-code-fixture/.git/config","[core]\n").unwrap();
 let prepared=rune_exec::command::prepare("printf hi",context.workspace(),None,Default::default()).unwrap();
 use rune_exec::sandbox::Sandbox;
 let wrapped=sandbox.wrap(&prepared,&rune_exec::SandboxPolicy::new(context.workspace.clone(),vec![],false),false).unwrap();
 println!("sandbox_argv={:?}",wrapped.argv);
 println!("sandbox_existing_nested_hooks_protected={} missing_root_hooks_protected={}",wrapped.argv.iter().any(|v|v==nested),wrapped.argv.iter().any(|v|v=="/tmp/rune-code-fixture/.git/hooks"));
}

```

### /tmp/rune-code-compile3.py

```python
import glob,os,subprocess
os.makedirs('/tmp/rune-probe-deps',exist_ok=True)
for source in glob.glob('target/debug/deps/*.rlib')+glob.glob('target/debug/deps/*.so'):
 dest='/tmp/rune-probe-deps/'+os.path.basename(source)
 if not os.path.exists(dest):os.symlink(os.path.abspath(source),dest)
libs=['rune_core','rune_net','rune_tools','rune_policy','rune_exec','rune_session','camino','serde_json','rune_agent']
cmd=['/tmp/rune-audit-cargo/bin/rustc','--edition=2024','/tmp/rune-code-probe3.rs','-L','dependency=/tmp/rune-probe-deps','-o','/tmp/rune-code-probe3']
for lib in libs:
 paths=glob.glob('target/debug/deps/lib'+lib+'-*.rlib')
 cmd+=['--extern',lib+'='+(min if lib in ['rune_net','rune_tools','rune_agent'] else max)(paths,key=os.path.getmtime)]
env=dict(os.environ,RUSTUP_HOME='/tmp/rune-audit-rustup',CARGO_HOME='/tmp/rune-audit-cargo')
subprocess.run(cmd,env=env,check=True)

```

### /tmp/rune-code-probe3.rs

```rust
use std::{io::Cursor,time::Duration};
use rune_core::{config::EnvironmentOverrides,paths::Paths};
use camino::Utf8Path;
fn main(){
 let root=format!("/tmp/rune-code-budget-{}",std::process::id());
 let paths=Paths{config_root:format!("{root}/config").into(),state_root:format!("{root}/state").into(),data_root:format!("{root}/data").into()};
 for i in 0..4 {let result=rune_net::auth::store(&paths,&format!("provider{i}"),&"x".repeat(rune_net::auth::MAX_CREDENTIAL_BYTES));println!("credential_budget write={} success={}",i+1,result.is_ok());}
 println!("credential_budget bytes={} read_ok={}",std::fs::metadata(paths.credentials_file()).unwrap().len(),rune_net::auth::stored_providers(&paths).is_ok());
 let path=Utf8Path::new("/tmp/rune-code-fixture/zero-context.toml");std::fs::write(path,"provider = 'anthropic'\n[models.anthropic]\nid = 'small'\ncontext_window = 0\nmax_output_tokens = 1234\n").unwrap();
 let settings=rune_core::config::load(None,Some(path),&EnvironmentOverrides::default());println!("zero_context resolved={:?} diagnostics={:?}",settings.context_window,settings.diagnostics);
 let path=Utf8Path::new("/tmp/rune-code-fixture/provider-env.toml");std::fs::write(path,"provider = 'anthropic'\n[models]\nanthropic = 'anthropic-selected'\nopenai = 'openai-selected'\n").unwrap();
 let env=EnvironmentOverrides::from_lookup(|k|if k=="RUNE_PROVIDER"{Some("openai".into())}else{None});
 let settings=rune_core::config::load(None,Some(path),&env);println!("provider_override provider={} model={} diagnostics={:?}",settings.provider,settings.model,settings.diagnostics);
 let catalog=rune_net::catalog::Catalog::new("local");println!("model_tag_choice={:?}",catalog.choose("qwen3:latest"));println!("model_plain_choice={:?}",catalog.choose("qwen3"));
 let stream="data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"This summary was stopped halfway through its account of the work completed.\"},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n";
 let outcome=rune_net::transport::read_stream(Box::new(Cursor::new(stream.as_bytes().to_vec())),&rune_net::chat_completions::ChatCompletions,Duration::from_secs(1),&||false,&mut |_|{}).unwrap();
 println!("summary_finish={:?} success={} validate_summary_ok={}",outcome.finish,outcome.finish.unwrap().is_success(),rune_agent::compaction::validate_summary(&outcome.text()).is_ok());
}

```

### Explicit coverage gaps

Live Linux sandbox enforcement was not run because bwrap was absent on this host; only refusal, unsandboxed opt-in and argv construction were observed (TF sandbox-fixed; code probe2; `crates/rune-exec/src/sandbox.rs:325`). A supported-runner network-isolation smoke check is in the list. macOS/Windows sandboxing, Windows replacement/PATH behavior, real browser-host execution, paid-provider authentication/model behavior, MCP OAuth, and legacy-session migration were not executed here. Relevant source boundaries and future checks are sandbox missing-hook protection, Node Windows lookup, WASM host length caps, native Gemini/local provider fixtures, MCP callback validation, and the archive installer smoke check in the list. Existing cross-platform CI is declared in `.github/workflows/ci.yml:95`; its existence is not a local test result.

Resource accumulation after cancellation, session holder handover and concurrent log growth remain source-derived rather than measured failures (`crates/rune-net/src/transport.rs:819`, `crates/rune-session/src/store.rs:402`, `crates/rune-session/src/event.rs:407`). Their acceptance checks explicitly demand the fault injection that is missing from this run. Terminal snapshots prove the visible clipping/stale rows/cursor positions; they do not measure all transient flicker. No cross-tool paid task success rate or coding-quality ranking was measured.

## The list

210 numbered increments: T1 37, T2 141, T3 32; 134 fixes and 76 features. Each row is one scoped change with an observable acceptance check. The check may name a future flag or mode; it is not an existing API claim. Source-only defects are described with their verification limits above. Retained-result reads follow retention wiring; archived-record search follows archival; optional streaming bindings follow the process event format. Each can be landed and tested against a fixture before its caller is connected.

| ID | Tier | Kind | Area | One-commit title | Evidence | Acceptance |
|---|---|---|---|---|---|---|
| R-001 | T1 | fix | config | Create private state parents during connect | crates/rune-core/src/paths.rs:409; fresh-clean PTY capture | Connect in a fresh HOME with umask 002 and observe state/rune mode 700 and an open session. |
| R-002 | T1 | fix | tools | Bound grep context allocation before allocating | crates/rune-tools/src/grep_files.rs:224; crates/rune-tools/src/grep_files.rs:331; TF-13 | Submit context_lines 18446744073709551615 through grep_files and observe a tool validation error while the session remains usable. |
| R-003 | T1 | fix | permissions | Display and resolve terminal approval requests | crates/rune/src/session.rs:388; TF-07 | In ask mode submit permission and approve the displayed shell request, then observe AUDIT_SHELL_OK. |
| R-004 | T1 | fix | input | Scroll long drafts to keep the caret visible | crates/rune-term/src/transcript.rs:467; crates/rune/src/session.rs:1755; TF-02 | At 80 columns enter 160 a characters plus TAIL-END and observe TAIL-END while editing the final character. |
| R-005 | T1 | fix | permissions | Normalize equivalent file paths before rule matching | crates/rune-tools/src/read_file.rs:84; crates/rune-policy/src/rules.rs:281; code probe | A denial for .env must also deny ./.env and its equivalent absolute workspace path without denying an unrelated file. |
| R-006 | T1 | fix | permissions | Escape literal filenames in remembered grants | crates/rune-policy/src/approval.rs:185; code probe | Approve the literal filename foo* and observe foo* allowed while foobar remains unapproved after restart. |
| R-007 | T1 | fix | scripting | Stop reporting unexecuted ask tool calls as successful | crates/rune/src/ask.rs:216; ask-wiring probe | Return a tool call to the current text-only ask path and observe a named unsupported-tool-call error rather than status success. |
| R-008 | T1 | fix | tools | Connect structured questions to terminal input | crates/rune-tools/src/inventory.rs:110; TF-08 | Choose Beta in the question fixture and observe Beta in the next provider request. |
| R-009 | T1 | fix | packaging | Extract verified release archives before installation | xtask/src/main.rs:239; crates/rune/src/install.rs:154; upgrade runtime probe | Upgrade a disposable target from a staged tar.gz release and successfully execute its version command. |
| R-010 | T1 | fix | packaging | Create an exclusive unique upgrade staging file | crates/rune/src/install.rs:150; staged-symlink runtime probe | Precreate .rune-install-staged as a symlink and observe its target unchanged after upgrade. |
| R-011 | T1 | fix | tui | Bound completion and model menus by terminal height | crates/rune-term/src/inline.rs:212; picker PTY capture | At 32x8 open /model, reach the last choice, close the picker and observe no stale rows. |
| R-012 | T1 | fix | tui | Wrap transcript text at the actual narrow width | crates/rune-term/src/transcript.rs:345; narrow PTY capture | At 12 columns render 300 Ws and END-LONG-WORD and recover all 300 Ws and the complete marker from scrollback. |
| R-013 | T1 | fix | tui | Refresh dimensions before laying out a draft | crates/rune/src/session.rs:1754; crates/rune/src/session.rs:716; TF-03 | Grow a 32-column draft to 80 columns and observe VISIBLE-END without another keypress. |
| R-014 | T1 | fix | providers | Enforce the total provider request deadline | crates/rune-core/src/budget.rs:232; crates/rune-net/src/transport.rs:519 | Set provider_request_timeout_ms=1000 and observe a slow ongoing stream stop within 1500 ms with a timeout cause. |
| R-015 | T1 | fix | providers | Enforce the first-event deadline during silent reads | crates/rune-net/src/transport.rs:851; silent-body probe | A response that sends headers and no events must stop at its configured head deadline without waiting for EOF. |
| R-016 | T1 | fix | config | Accept and enforce the documented offline file setting | crates/rune-core/src/config.rs:331; README.md:62; code probe | Load offline=true from the user config and observe offline enabled without discarding the remaining config. |
| R-017 | T1 | fix | sandbox | Carry offline mode into shell network policy | crates/rune-tools/src/shell.rs:530; crates/rune-tools/src/contract.rs:111 | With a Linux sandbox and offline full-access context, a shell connection to a local fixture must fail before the fixture receives data. |
| R-018 | T1 | fix | config | Align the web default with the documented opt-in | README.md:62; crates/rune-core/src/config.rs:551; crates/rune/src/permissions.rs:40; web-default command | With no web setting, the documented opt-in behavior must refuse a fixture web call; explicitly enabling web_tools must allow that same permitted fixture call. |
| R-019 | T1 | fix | web | Reject abbreviated private IPv4 addresses | crates/rune-tools/src/web.rs:458; recording-backend probe | Fetch 127.1, 127.0.1 and 10.1 without a private grant and observe zero backend calls. |
| R-020 | T1 | fix | web | Validate the resolved web destination before connecting | crates/rune-tools/src/web.rs:420; recording-backend probe | A hostname resolving to loopback must be refused before HTTP, while a permitted public destination connects to the vetted address rather than resolving again unchecked. |
| R-021 | T1 | fix | providers | Serialize Anthropic reasoning effort | crates/rune-net/src/anthropic.rs:54; request-body probe | Build Auto and High Anthropic requests and assert the provider-supported thinking fields differ as specified. |
| R-022 | T1 | fix | providers | Serialize Chat Completions reasoning effort | crates/rune-net/src/chat_completions.rs:66; request-body probe | Build Auto and High compatible requests and assert reasoning_effort reaches a supporting endpoint. |
| R-023 | T1 | fix | providers | Serialize Responses reasoning effort | crates/rune-net/src/responses.rs:52; request-body probe | Build Auto and High Responses requests and assert reasoning.effort reaches the fixture. |
| R-024 | T1 | fix | sessions | Persist cancellation with its visible partial answer | crates/rune/src/session.rs:1244; TF-11 | Cancel after STREAM-03, resume and observe STREAM-01 through STREAM-03 plus the cancelled boundary exactly once. |
| R-025 | T1 | fix | sessions | Record provider failure with visible partial output | crates/rune/src/session.rs:1244; TF-11 | Cut a provider after three deltas and observe those deltas and the failure boundary in the resumed log. |
| R-026 | T1 | fix | scripting | Save ask exchanges unless no-save is selected | crates/rune/src/ask.rs:239; COMMANDS.md:37; ask-wiring probe | Run rune ask --json fixture, resolve its nonempty session_id and inspect the exchange; --no-save creates no session. |
| R-027 | T1 | fix | tools | Retain oversized tool results instead of discarding them | crates/rune-agent/src/turn.rs:459; crates/rune-tools/src/result_store.rs:171 | Return output larger than max_tool_result_bytes and observe a preview plus a retrievable retained handle. |
| R-028 | T1 | feature | tools | Expose a bounded read_tool_result tool | crates/rune-tools/src/result_store.rs:132; crates/rune-tools/src/inventory.rs:26 | Read two pages of a retained fixture and reconstruct its full bytes without exceeding either response cap. |
| R-029 | T1 | fix | tools | Keep completed shell output until it is consumed | crates/rune-tools/src/shell.rs:114; shell-session probe | Start a yielding command, let it finish, start another command and still retrieve the first command's final output. |
| R-030 | T1 | fix | tools | Truncate Unicode tool descriptions at a character boundary | crates/rune-tools/src/contract.rs:352; code probe | Register a description of one ASCII byte plus 600 e-acute characters and observe no panic and a valid UTF-8 schema. |
| R-031 | T1 | fix | config | Replace credential files atomically | crates/rune-net/src/auth.rs:184; crates/rune-core/src/paths.rs:420 | Kill a credential writer before replacement and observe either complete old or complete new JSON, never truncated authority. |
| R-032 | T1 | fix | config | Serialize concurrent credential updates | crates/rune-net/src/auth.rs:184; crates/rune-core/src/paths.rs:227 | Connect two scratch provider profiles concurrently and observe both credentials after reopening. |
| R-033 | T1 | fix | docs | Report the measured binary size and actual ceiling | README.md:7; xtask/src/main.rs:17; budget output | README and budget help must distinguish the measured 4.50 MiB GNU build from the 8 MiB ceiling and name the target. |
| R-034 | T1 | feature | tui | Add a real PTY replay gate for observed defects | crates/rune-term/tests/frame_stream.rs:1; terminal evidence appendix | CI must replay long drafts, 12-column text, short menus and draft resize through the actual rune process and compare captured grids. |
| R-035 | T1 | feature | sandbox | Explain a command's effective sandbox policy | crates/rune-exec/src/sandbox.rs:122; fx sandbox comparison | Run rune sandbox explain on a fixture command and observe backend, writable roots, network decision and protected paths with sources. |
| R-036 | T1 | feature | tui | Render fenced code with stable continuation wrapping | crates/rune-term/src/transcript.rs:355; crush terminal comparison | Stream a fenced code fixture with long lines and observe preserved indentation and a readable continuation marker. |
| R-037 | T1 | feature | tui | Add an on-demand full transcript viewer | crates/rune-term/src/input.rs:21; [FX-TRANSCRIPT] | Open the transcript by shortcut, scroll recorded tools and replies, then close it with the draft text and caret unchanged. |
| R-038 | T2 | feature | sessions | Replay saved exchanges when resuming interactively | crates/rune/src/session.rs:935; crates/rune/src/session.rs:1035; TF-09 | Save a fixture reply, resume last and observe the prior user and assistant exchange before typing. |
| R-039 | T2 | fix | sessions | Initialize the resumed context meter from history | crates/rune/src/session.rs:985; TF-09 | Resume a session with 1234 input tokens and observe a nonzero initial context estimate labelled with its source. |
| R-040 | T2 | feature | web | Require user authority for private network access | crates/rune-tools/src/web.rs:1060; crates/rune-tools/src/web.rs:1096 | A model setting allow_private=true must trigger a distinct private-network decision and cannot grant itself access. |
| R-041 | T2 | fix | providers | Close the stream socket when a request is cancelled | crates/rune-net/src/transport.rs:804; crates/rune-net/src/transport.rs:840 | Cancel 50 silent streams and observe reader-thread and open-socket counts return to baseline. |
| R-042 | T2 | fix | config | Reset capacity when an environment override changes model | crates/rune-core/src/config.rs:1229; context_env probe | Override a declared 2000000-token model with RUNE_MODEL=small and observe that the old window is discarded. |
| R-043 | T2 | fix | config | Reset provider-derived fields on CLI overrides | crates/rune/src/cli.rs:592; config-flags probe | Switch provider with a CLI flag and observe its model, endpoint and capacity resolved without using the previous provider's values. |
| R-044 | T2 | fix | context | Invoke automatic compaction before an oversized CLI request | crates/rune-agent/src/compaction.rs:235; crates/rune/src/session.rs:1827 | Cross compaction_trigger_percent with fixture turns and observe a compaction event before the next model request. |
| R-045 | T2 | fix | cli | Read usage periods from the documented flag | crates/rune/src/main.rs:809; CLI usage output | Run rune usage --period 7d --json and observe period 7d with a seven-day interval. |
| R-046 | T2 | fix | cli | Accept the documented auth status action | crates/rune/src/main.rs:485; CLI auth status output | Run rune auth status --json and observe a status object with exit 0. |
| R-047 | T2 | fix | cli | Resolve session last before parsing an exact ID | crates/rune/src/main.rs:790; CLI session last output | Save a session and run rune session last --json, then observe that session's identifier. |
| R-048 | T2 | fix | cli | Resolve tree last before parsing an exact ID | crates/rune/src/main.rs:754; CLI tree last output | Save a session and run rune tree last --json, then observe its branch structure. |
| R-049 | T2 | fix | cli | Use the documented permission explanation flag | crates/rune/src/main.rs:1008; CLI permissions output | Run rune permissions --explain shell:pwd --json and observe one structured decision with its deciding rule. |
| R-050 | T2 | fix | cli | Emit an ask JSON error for an empty prompt | crates/rune/src/main.rs:253; CLI empty ask output | Run rune ask --json with empty stdin and observe one valid failure object on stdout and exit 1. |
| R-051 | T2 | fix | config | Write settings to the active RUNE_CONFIG path | crates/rune/src/provider_setup.rs:83; crates/rune/src/main.rs:1318 | Set RUNE_CONFIG to a scratch file, run workspace add and observe only that file updated. |
| R-052 | T2 | fix | xtask | Make gate include formatting and lint checks | xtask/src/main.rs:174; README.md:165 | On a scratch checkout with an intentional formatting violation, cargo xtask gate must fail before budget and tests. |
| R-053 | T2 | fix | cli | Return an empty directory list after workspace clear | crates/rune/src/main.rs:1370; CLI workspace clear output | Add one directory then run rune workspace clear --json and observe directories=[] in both response and following list. |
| R-054 | T2 | fix | cli | Reject unknown effort values | crates/rune/src/cli.rs:601; CLI banana output | Run rune --effort banana config and observe invalid_field with the accepted efforts and exit 1. |
| R-055 | T2 | fix | cli | Reject unknown permission modes | crates/rune/src/cli.rs:609; CLI banana mode output | Run rune --permission-mode banana permissions and observe invalid_field with accepted modes and exit 1. |
| R-056 | T2 | fix | tools | Search original grep lines before truncating display | crates/rune-tools/src/grep_files.rs:354; long-line grep probe | Search an over-one-MiB line that lacks the phrase line truncated and observe zero matches for that phrase even though its rendered output is annotated. |
| R-057 | T2 | fix | observability | Redact credentials from Endpoint Debug | crates/rune-net/src/transport.rs:77; endpoint_debug probe | Format an Endpoint containing an audit secret with Debug and assert no secret bytes appear. |
| R-058 | T2 | feature | sessions | Journal assistant deltas before a process can be killed | crates/rune/src/session.rs:1225; README.md:109; TF-12 | SIGKILL after STREAM-03 must leave a replayable partial assistant entry without a false completed turn. |
| R-059 | T2 | fix | tui | Route caught worker panic diagnostics through the renderer | crates/rune-term/src/input.rs:415; TF-14 | Trigger the bounded panic fixture in a debug worker and observe one diagnostic without duplicated status rows. |
| R-060 | T2 | fix | observability | Report worker panics as internal failures | crates/rune/src/session.rs:1516; TF-14 | A caught worker panic must report internal failure rather than the user-cancelled label. |
| R-061 | T2 | fix | tui | Make the minimum-height warning match submission behavior | crates/rune-term/src/footer.rs:172; crates/rune/src/session.rs:1611; TF-17 | At four rows either refuse prompt submission until resize or describe the supported compact mode, with the displayed statement matching request traffic. |
| R-062 | T2 | fix | input | Bind the existing yank operation to Ctrl-Y | crates/rune-term/src/editor.rs:289; crates/rune-term/src/input.rs:274 | Type abc, Ctrl-U, Ctrl-Y and observe abc restored with the caret at its end. |
| R-063 | T2 | fix | input | Bind the existing composer undo operation | crates/rune-term/src/editor.rs:299; crates/rune-term/src/input.rs:274 | Insert and delete a Unicode grapheme, invoke the documented undo binding and observe the original draft and caret. |
| R-064 | T2 | fix | input | Bind the existing composer redo operation | crates/rune-term/src/editor.rs:310; crates/rune-term/src/input.rs:274 | Undo a draft edit, invoke the documented redo binding and observe the edited draft and caret. |
| R-065 | T2 | feature | input | Add an explicit newline key binding | crates/rune-term/src/input.rs:238 | Press the documented newline binding and submit a two-line prompt whose exact newline reaches the provider fixture. |
| R-066 | T2 | feature | input | Lay out multiline drafts with a visible caret | crates/rune/src/session.rs:1753 | Paste three lines, navigate across them and observe each edited line and the correct terminal caret row. |
| R-067 | T2 | feature | input | Edit a draft using the configured external editor | crates/rune-term/src/input.rs:200 | Invoke the editor binding with a stub editor that changes the scratch draft and observe the changed composer text after terminal restoration. |
| R-068 | T2 | feature | input | Complete workspace paths in tool-oriented prompts | crates/rune/src/session.rs:2055; fx input comparison | Complete a fixture path containing spaces and observe a single correctly quoted selection without reading outside workspace roots. |
| R-069 | T2 | feature | input | Search prompt history by substring | crates/rune/src/prompt_history.rs:91; fx history comparison | Find an older fixture prompt by its middle word and restore it into the composer without submitting. |
| R-070 | T2 | feature | accessibility | Offer a transcript mode suitable for screen readers | crates/rune-term/src/inline.rs:252 | Run the accessible mode and observe append-only labelled output without cursor movement or continuously rewritten status rows. |
| R-071 | T2 | feature | accessibility | Offer a named high-contrast theme | crates/rune/src/session.rs:2448; crates/rune-term/src/theme.rs:287 | Select high-contrast with NO_COLOR unset and observe each defined foreground/background pair meeting a documented contrast target; NO_COLOR still suppresses colors. |
| R-072 | T2 | feature | accessibility | Provide ASCII status and selection markers | crates/rune-term/src/footer.rs:96; crates/rune-term/src/transcript.rs:249 | Select the ASCII setting and observe no non-ASCII decoration in status, completions or tool summaries. |
| R-073 | T2 | feature | accessibility | Expose the active bindings in interactive help | crates/rune-term/src/input.rs:200; crates/rune-term/src/commands.rs:19 | Run /help and verify every documented binding with a PTY action, including cancellation precedence and paste. |
| R-074 | T2 | feature | tui | Make tool summaries expandable on request | crates/rune-term/src/transcript.rs:367; crush terminal comparison | Expand a collapsed fixture tool result and observe bounded contents, then collapse it without losing surrounding transcript. |
| R-075 | T2 | feature | tui | Show the active provider retry and next delay | crates/rune-agent/src/turn.rs:590 | Serve two retryable failures and observe attempt counts and the pending delay in one renderer-owned status row. |
| R-076 | T2 | fix | cli | Add resume to the shared command specification | crates/rune/src/cli.rs:329; crates/rune/src/spec.rs:156 | Run rune help resume and rune resume --help and observe valid usage included in rune reference. |
| R-077 | T2 | fix | cli | Declare accepted aliases in the shared table | crates/rune/src/cli.rs:308; crates/rune/src/spec.rs:159 | For login, setup, provider, pr, issue, cost, logout and settings, help and generated reference must describe the actual alias behavior. |
| R-078 | T2 | fix | cli | Reject values attached to declared boolean flags | crates/rune/src/cli.rs:493; CLI sessions --all=no output | Run rune sessions --all=no and observe a takes-no-value error with exit 1. |
| R-079 | T2 | fix | cli | Honor exact session selectors for inspect | crates/rune/src/spec.rs:224; crates/rune/src/main.rs:787 | Run rune session --id with a valid fixture identifier and observe that session without requiring a positional argument. |
| R-080 | T2 | fix | cli | Honor exact session selectors for tree | crates/rune/src/spec.rs:241; crates/rune/src/main.rs:753 | Run rune tree --id with a valid fixture identifier and observe that session's tree. |
| R-081 | T2 | fix | observability | Print each ask failure once | crates/rune/src/main.rs:273; missing-provider runtime output | Run an unauthenticated rune ask --json and observe exactly one stderr error line plus one hint. |
| R-082 | T2 | fix | cli | Make unknown-command help return failure | crates/rune/src/main.rs:88; CLI help not-real output | Run rune help not-real and observe a nonzero exit while valid help still exits 0. |
| R-083 | T2 | fix | config | Validate legacy project limits instead of swallowing errors | crates/rune-core/src/config.rs:1144; steps_invalid probe | Load max_agent_steps=10001 and observe a named diagnostic rather than a silent unlimited default. |
| R-084 | T2 | fix | config | Recover a settings lock after its writer is killed | crates/rune-policy/src/settings.rs:80; crates/rune-policy/src/settings.rs:117 | Kill a writer holding the settings lock and observe that the next writer completes without manually deleting a lock file. |
| R-085 | T2 | fix | config | Replace policy settings atomically | crates/rune-policy/src/settings.rs:218; crates/rune-core/src/paths.rs:420 | Interrupt a policy-settings write before replacement and observe a complete old or new rules file. |
| R-086 | T2 | fix | tools | Validate closed tool schemas consistently | crates/rune-tools/src/contract.rs:324; read_unknown_field probe | Call read_file with a misspelled optional property and observe a schema error naming that property. |
| R-087 | T2 | fix | tools | Preserve tool failure codes and repair hints | crates/rune-tools/src/registry.rs:134; registry_error probe | Return NotFound with hint try other.txt from a fixture tool and observe the code and hint in the model result and session event. |
| R-088 | T2 | fix | tools | Check cancellation during paged file reads | crates/rune-tools/src/read_file.rs:220; read_cancelled probe | Cancel before and during a large read_file call and observe bounded work plus a cancelled result. |
| R-089 | T2 | fix | tools | Check cancellation within a single large grep input | crates/rune-tools/src/grep_files.rs:335 | Cancel a grep over one large file and observe it stop before scanning the remaining file. |
| R-090 | T2 | fix | tools | Correct retained-result accounting on replacement | crates/rune-tools/src/result_store.rs:237 | Retain the same handle twice and observe total_bytes equal the actual stored bytes rather than twice that amount. |
| R-091 | T2 | fix | tools | Avoid advertising nonexistent handles for small results | crates/rune-tools/src/result_store.rs:193; crates/rune-tools/src/result_store.rs:132 | Render a small result preview and observe either its stored readable handle or no read_tool_result instruction. |
| R-092 | T2 | fix | tools | Enforce result caps including truncation markers | crates/rune-tools/src/result_store.rs:223 | Retain cap-plus-one bytes and assert the retained record including its marker is no larger than the configured cap. |
| R-093 | T2 | fix | tools | Use registered tool activity in event summaries | crates/rune-agent/src/turn.rs:784; crates/rune-tools/src/registry.rs:151 | Register a custom Read tool and observe read activity rather than Execute in its event and summary. |
| R-094 | T2 | feature | tools | Add atomic multiple literal edits to one file | crates/rune-tools/src/mutation.rs:420; Codex tool comparison | Apply two literal replacement hunks to one workspace fixture atomically; a missing or ambiguous hunk leaves the entire file unchanged and produces a repair hint. |
| R-095 | T2 | feature | tools | Add a permission-checked file rename tool | crates/rune-tools/src/inventory.rs:26 | Rename a workspace fixture and observe one mutation event, while an outside-root destination is refused. |
| R-096 | T2 | feature | tools | Add a permission-checked directory creation tool | crates/rune-tools/src/inventory.rs:26 | Create nested workspace directories and observe bounded, idempotent results plus an outside-root refusal. |
| R-097 | T2 | feature | tools | Expose a bounded git diff inspection tool | crates/rune/src/main.rs:347; aider repository comparison | Read a fixture staged and unstaged diff with a byte cap and observe labelled truncation without invoking writable git operations. |
| R-098 | T2 | fix | context | Measure retained-tail capacity separately from full history | crates/rune-agent/src/tokens.rs:234; crates/rune-agent/src/compaction.rs:240 | A fixture with a large removable prefix and small retained tail must select compaction rather than Impossible. |
| R-099 | T2 | fix | providers | Apply steering before a retry request is rebuilt | crates/rune-agent/src/turn.rs:546; crates/rune-agent/src/turn.rs:512 | Enqueue a correction during attempt one and assert attempt two contains it even when attempt one fails. |
| R-100 | T2 | fix | providers | Treat offline refusal as a nonretryable policy outcome | crates/rune-net/src/transport.rs:496; crates/rune-net/src/error.rs:81 | Run a generic offline turn host and observe no retry delay and zero outbound fetch calls. |
| R-101 | T2 | fix | providers | Validate endpoint authority and control characters | crates/rune-net/src/transport.rs:196; validate_url probe | Reject https:///, https://?broken and a URL containing newline before creating a client. |
| R-102 | T2 | fix | providers | Honor one-call mode in model request serialization | crates/rune-agent/src/turn.rs:543; crates/rune-net/src/provider.rs:83 | Set parallel_tool_calls=1 and assert the provider request disables parallel tool calls. |
| R-103 | T2 | feature | providers | Execute eligible read calls concurrently within the configured cap | crates/rune-agent/src/turn.rs:644; crates/rune-core/src/budget.rs:503 | Use barrier-backed fixture reads to prove two run concurrently at cap 2 and never overlap a pending mutation. |
| R-104 | T2 | fix | sandbox | Protect absent Linux git hooks destinations | crates/rune-exec/src/sandbox.rs:328 | With .git/hooks absent, a sandboxed fixture command must fail to create an executable hook there. |
| R-105 | T2 | fix | sandbox | Protect absent macOS git hooks destinations | crates/rune-exec/src/sandbox.rs:328 | On macOS with .git/hooks absent, a sandboxed fixture command must fail to create an executable hook there. |
| R-106 | T2 | fix | sandbox | Protect nested repositories within writable roots | crates/rune-exec/src/sandbox.rs:82 | A sandboxed command must be refused when writing a nested repository's config or hooks, while normal source writes still work. |
| R-107 | T2 | fix | sessions | Write disposable session metadata atomically | crates/rune-session/src/store.rs:381; crates/rune-core/src/paths.rs:420 | Kill during metadata replacement and observe valid old or new metadata and replayable authoritative events. |
| R-108 | T2 | fix | sessions | Distinguish durable append success from projection failure | crates/rune-session/src/store.rs:343; crates/rune-session/src/store.rs:381 | Force metadata write failure after event sync and observe a reported durable event with a repair warning rather than retry ambiguity. |
| R-109 | T2 | fix | sessions | Clear lock-holder diagnostics before releasing the lock | crates/rune-session/src/store.rs:402 | Race old-owner release with new-owner acquisition and observe the new holder's diagnostic record preserved. |
| R-110 | T2 | fix | sessions | Cap event reads despite concurrent log growth | crates/rune-session/src/event.rs:407 | Grow a log between metadata and read and observe bounded bytes plus a named size error. |
| R-111 | T2 | feature | sessions | Export a session as a stable JSON stream | crates/rune-session/src/event.rs:44; goose session comparison | Export a fixture twice and observe identical ordered event records without credentials or transport headers. |
| R-112 | T2 | feature | sessions | Search sessions by title and prompt text | crates/rune/src/session_log.rs:235; fx session comparison | Find a fixture session using a title fragment and a user-message fragment with workspace scoping intact. |
| R-113 | T2 | feature | context | Search archived compaction records | crates/rune-agent/src/compaction.rs:77; fx compacted-record comparison | Compact a fixture then search for a removed symbol and observe its original record and session position. |
| R-114 | T2 | feature | context | Preview the records a manual compaction would remove | crates/rune/src/session.rs:1827 | Invoke a compact preview and observe exact retained and summarized record counts without changing history. |
| R-115 | T2 | feature | context | Expose token-estimate provenance in status | crates/rune-agent/src/tokens.rs:56; crates/rune-term/src/footer.rs:96 | Observe provider-reported versus estimated counts labelled distinctly before and after the first fixture response. |
| R-116 | T2 | feature | mcp | List resolved MCP servers and startup failures | crates/rune-context/src/mcp/client.rs:163; fx MCP comparison | Run rune mcp list --json and observe configured servers, transport, trust state and each startup error without secret headers. |
| R-117 | T2 | feature | mcp | Add a server connectivity check without starting an agent turn | crates/rune-context/src/mcp/client.rs:163 | Run rune mcp check against a local fixture and observe its protocol, tools and timeout result without a model request. |
| R-118 | T2 | fix | acp | Use the session cwd supplied by the client | crates/rune-acp/src/server.rs:544; ACP runtime transcript | Launch ACP from workspace A, create a session for B and observe relative reads, listing and instructions rooted in B. |
| R-119 | T2 | fix | acp | Implement requested stdio MCP servers | crates/rune-acp/src/server.rs:547; ACP session setup specification | Create an ACP session with a stdio MCP fixture and observe its tool advertised and callable in that session. |
| R-120 | T2 | fix | acp | Filter session listings by stored workspace | crates/rune-acp/src/session.rs:283 | Create fixture sessions in A and B and observe each connection list only its intended workspace with truthful cwd. |
| R-121 | T2 | fix | acp | Sort sessions before applying the listing cap | crates/rune-acp/src/session.rs:288 | Create more sessions than the cap in shuffled directory order and observe the newest sessions consistently returned. |
| R-122 | T2 | fix | acp | Apply lifecycle root parameters when reopening a session | crates/rune-acp/src/session.rs:271; crates/rune-acp/src/server.rs:570 | Load a session with a new additional-root list and observe exactly that effective list, including removal when omitted. |
| R-123 | T2 | fix | acp | Update context capacity when the ACP model changes | crates/rune-acp/src/session.rs:352; crates/rune-acp/src/server.rs:1149 | Switch between fixture models with different windows and observe matching subsequent usage-update size values. |
| R-124 | T2 | fix | acp | Persist ACP model and effort changes for reopening | crates/rune-acp/src/session.rs:268; crates/rune-acp/src/session.rs:337 | Change model and effort, close and load the session and observe the chosen values restored. |
| R-125 | T2 | fix | acp | Stop turns when protocol output can no longer be written | crates/rune-acp/src/server.rs:377 | Close the client's output pipe mid-turn and observe provider and tool work cancelled within the shutdown grace. |
| R-126 | T2 | feature | acp | Bound unanswered client permission requests | crates/rune-acp/src/server.rs:785 | Leave a permission request unanswered and observe a configured deadline error plus a usable connection for a later prompt. |
| R-127 | T2 | fix | acp | Reject malformed text blocks instead of silently skipping them | crates/rune-acp/src/server.rs:1333 | Submit one valid text block plus one text block without text and observe invalid_params naming the malformed block. |
| R-128 | T2 | feature | acp | Negotiate and track initialization state | crates/rune-acp/src/server.rs:493; ACP initialization specification | Observe a documented initialization result and refusal of session lifecycle methods until compatibility is established. |
| R-129 | T2 | fix | sdk | Retry retryable SDK provider failures through shared policy | crates/rune-sdk/src/agent.rs:1134 | Serve one 429 with Retry-After then success and observe one bounded retry with the caller's cancellation honored. |
| R-130 | T2 | fix | sdk | Make checkpoints obey their restoration size cap | crates/rune-sdk/src/agent.rs:837; crates/rune-sdk/src/agent.rs:842 | Build a checkpoint exceeding the configured bound and observe an early named error rather than bytes the same agent cannot restore. |
| R-131 | T2 | fix | sdk | Honor configured SDK max-agent-steps when prompt options omit it | crates/rune-sdk/src/agent.rs:977; crates/rune-sdk/src/agent.rs:757 | Set MaxAgentSteps=2 with default PromptOptions and observe the SDK stop after two model steps. |
| R-132 | T2 | fix | sdk | Bound the SDK event queue for a stalled consumer | crates/rune-sdk/src/agent.rs:744 | Stream 100000 fixture deltas without consuming events and observe bounded memory plus an explicit backpressure outcome. |
| R-133 | T2 | fix | node | Resolve explicit relative binaries against options.cwd | bindings/node/index.js:136; Node relative-cwd probe | Place rune only in a scratch cwd, call ask with bin ./rune and observe the stub result. |
| R-134 | T2 | fix | node | Reject nonfinite and overflowing timeout values | bindings/node/index.js:132; Node infinite-timeout probe | Infinity, NaN and values beyond the timer range must fail validation without spawning a child or emitting TimeoutOverflowWarning. |
| R-135 | T2 | fix | node | Validate every required ask-result field | bindings/node/index.js:315; bindings/node/index.d.ts:86 | A stub returning only output and exit_code must raise RuneOutputError naming missing fields. |
| R-136 | T2 | fix | node | Bound captured child stdout and stderr | bindings/node/index.js:154 | Run a stub that writes beyond the configured output cap and observe a typed output-limit failure with bounded memory. |
| R-137 | T2 | fix | node | Find rune.exe on Windows | bindings/node/index.js:103; bindings/node/index.js:281 | On Windows with only rune.exe in a fixture PATH, resolveBinary must return its executable path. |
| R-138 | T2 | feature | node | Expose provider and offline process options | bindings/node/index.d.ts:121 | Call ask with provider and offline options and assert both flags reach the stub before the command delimiter. |
| R-139 | T2 | fix | xtask | Honor CARGO_TARGET_DIR in budget and release artifact lookup | xtask/src/main.rs:181; xtask/src/main.rs:352 | Build with a scratch CARGO_TARGET_DIR and observe budget and release read its binary rather than target/release. |
| R-140 | T2 | fix | xtask | Serialize release manifests as JSON | xtask/src/main.rs:258 | Stage a channel containing a quote and observe valid JSON with the exact channel value, or reject it before writing. |
| R-141 | T2 | feature | ci | Run the declared dependency advisory and license policy | deny.toml:16; .github/workflows/ci.yml:34 | CI must run cargo deny check and fail on a scratch dependency violating the declared policy. |
| R-142 | T2 | feature | ci | Run the Node binding tests in CI | bindings/node/README.md:65; .github/workflows/ci.yml:44 | CI must execute node --test bindings/node/test.mjs and fail when its stub contract is intentionally broken. |
| R-143 | T2 | feature | ci | Build and smoke-test the WASM target in CI | xtask/src/main.rs:110; .github/workflows/ci.yml:123 | CI must compile rune-web for wasm32-wasip1 and run a configured fixture prompt through the exported boundary. |
| R-144 | T2 | feature | ci | Compare releases from two independent clean build directories | .github/workflows/ci.yml:86 | Build the same source independently with two scratch target directories and assert extracted binaries and archives have equal hashes. |
| R-145 | T2 | fix | performance | Label startup minima and expose raw distribution metrics | xtask/src/main.rs:19; xtask/src/main.rs:411; comparative startup output | Budget output must label minimum-of-31, show raw median and p95, and distinguish baseline-subtracted work from process latency. |
| R-146 | T2 | fix | docs | Document the real offline enforcement boundary | README.md:62; crates/rune-context/src/mcp/client.rs:163 | Documentation must map model, web, remote MCP, local MCP and shell traffic to their tested offline behavior. |
| R-147 | T2 | fix | docs | Align repository links and document package version policy | Cargo.toml:25; bindings/node/package.json:3; README.md:29 | Cargo and README links must name the intended maintained repository; a check must enforce the documented independent-or-shared Node version policy. |
| R-148 | T2 | fix | observability | Preserve typed I/O causes in RuneError | crates/rune-core/src/error.rs:350; crates/rune-core/src/error.rs:356 | A fixture permission-denied file write must retain its source error and report a filesystem failure rather than transport_failure. |
| R-149 | T2 | fix | observability | Preserve provider codes and retry metadata in JSON errors | crates/rune-net/src/error.rs:229 | Serve a fixture provider code and Retry-After and observe both retained in structured error detail with secrets redacted. |
| R-150 | T2 | fix | input | Preserve completion selection across resize events | crates/rune-term/src/input.rs:174; crates/rune/src/session.rs:1713; TF-05 | Select /new, resize from 80x24 to 32x10 and observe /new still selected. |
| R-151 | T2 | fix | tui | Wrap settled slash-command output | crates/rune/src/session.rs:2187; crates/rune-term/src/inline.rs:305; TF-15 | Run /help at 40 and 80 columns and recover every full command description from scrollback. |
| R-152 | T2 | fix | input | Complete discovered custom slash commands | crates/rune/src/session.rs:952; crates/rune/src/session.rs:2073; TF-16 | Create .rune/commands/audit-command.md, type /audit and select the displayed /audit-command candidate. |
| R-153 | T2 | fix | sessions | Restore saved token totals for /cost | crates/rune/src/session.rs:1004; crates/rune/src/session.rs:2359; TF-10 | Resume a three-turn fixture and observe /cost reporting its saved requests and tokens before a new request. |
| R-154 | T2 | fix | sessions | Normalize session-tree previews to one logical line | crates/rune-session/src/tree.rs:92; crates/rune-session/src/tree.rs:766; TF-18 | Inspect a fixture containing newline and control characters and observe one escaped preview per node with no injected table rows. |
| R-155 | T2 | fix | permissions | Resolve custom-tool targets through the host registry | crates/rune-agent/src/turn.rs:805; crates/rune-tools/src/registry.rs:151 | A custom tool with target secret.txt must reach policy with that exact target and be refused by its explicit denial. |
| R-156 | T2 | fix | providers | Accept tagged unknown model IDs in the picker | crates/rune-net/src/catalog.rs:295; code probe | Choose qwen3:latest absent from the endpoint catalog and observe that exact ID sent, while an invalid empty ID is refused. |
| R-157 | T2 | feature | scripting | Run ask through a bounded host tool loop | crates/rune/src/ask.rs:205; crates/rune-agent/src/turn.rs:349; ask-wiring probe | A local fixture requests one allowed read, receives its result in the second request and returns a final answer within the step cap. |
| R-158 | T2 | fix | scripting | Include resolved workspace instructions in ask requests | crates/rune/src/ask.rs:183; README.md:143; ask-wiring probe | Place AUDIT_INSTRUCTION_MUST_BE_SENT in AGENTS.md and observe it in the captured ask request according to prompt --show. |
| R-159 | T2 | fix | scripting | Use bounded retry policy for ask requests | crates/rune/src/ask.rs:205; crates/rune-agent/src/turn.rs:529 | Serve one 429 with Retry-After and then success; ask retries once within its configured deadline and never retries an invalid credential. |
| R-160 | T2 | fix | extensions | Connect a narrowed synchronous subagent runner | crates/rune/src/session.rs:3018; crates/rune-agent/src/subagent_tool.rs:128 | A local parent fixture delegates one read task and receives a child result; the child cannot call a tool outside its inherited allowlist. |
| R-161 | T2 | fix | extensions | Connect skill tools to the discovered skill catalog | crates/rune-tools/src/inventory.rs:115; crates/rune/src/session.rs:930 | Place an isolated workspace skill, call capability_search and skill, and observe its discovered name and exact bounded instructions. |
| R-162 | T2 | fix | tools | Connect the advertised vision tool to a configured host | crates/rune-tools/src/inventory.rs:136; crates/rune-tools/src/vision.rs:1 | A configured vision fixture receives one permitted image and returns its result; absent configuration omits or explicitly labels the unavailable tool. |
| R-163 | T2 | fix | mcp | Connect a trusted stdio MCP server in the interactive CLI | crates/rune/src/session.rs:3009; crates/rune-acp/src/server.rs:547; [FX-MCP] | A trusted local stdio fixture starts, its schema is discoverable, and one model call executes through the server with bounded startup and shutdown. |
| R-164 | T2 | fix | mcp | Connect a trusted HTTP MCP server in the interactive CLI | crates/rune-context/src/mcp/client.rs:1091; crates/rune/src/session.rs:3009 | A local HTTP fixture becomes callable online and receives zero requests with --offline, while an untrusted project definition stays blocked. |
| R-165 | T2 | fix | context | Reject compaction summaries stopped at the token limit | crates/rune/src/session.rs:1869; code probe | Return a nonempty MaxTokens summary and observe original history preserved with an explicit incomplete-summary error. |
| R-166 | T2 | fix | config | Bound the complete serialized credential file before writing | crates/rune-net/src/auth.rs:130; crates/rune-net/src/auth.rs:195; code probe | Attempt credential additions exceeding the reader cap and observe an early size error with the previous credential file still readable. |
| R-167 | T2 | fix | config | Select the configured model after an environment provider override | crates/rune-core/src/config.rs:1230; code probe | Configure Anthropic and Chat models, set RUNE_PROVIDER=openai and observe the Chat model selected with its own capacity. |
| R-168 | T2 | fix | config | Reject zero context windows with a named diagnostic | crates/rune-core/src/config.rs:323; code probe | Load context_window=0 and observe a diagnostic naming that field and no zero-capacity effective session. |
| R-169 | T2 | fix | tools | Find grep matches beyond the retained long-line head | crates/rune-tools/src/grep_files.rs:339; code probe | Put hiddenneedle after one MiB on a line and observe one exact match with bounded rendered output. |
| R-170 | T2 | feature | context | Persist an explicit session objective | crates/rune-session/src/event.rs:44; [CX-GOAL] | Set a goal explicitly, restart and observe the identical active objective; an ordinary prompt creates no goal. |
| R-171 | T2 | feature | scripting | Emit bounded JSONL events during ask | crates/rune/src/ask.rs:3; [AM-EXECUTE]; [GM-CONFIG] | Run a streaming local fixture and observe start, text_delta and final objects on separate stdout lines before completion, with diagnostics only on stderr. |
| R-172 | T2 | feature | context | Archive original records before manual compaction | crates/rune-agent/src/compaction.rs:168; [FX-MEMORY] | Compact then restart and retrieve each removed original message by stable record ID, including complete tool-call/result pairs. |
| R-173 | T2 | feature | tools | Stop consecutive identical failing tool calls | crates/rune-agent/src/turn.rs:644; [GS-CLI] | Repeat an identical failed call past a configured cap and observe a named stop without executing the next duplicate. |
| R-174 | T2 | feature | cli | Generate shell completion from the shared command table | crates/rune/src/spec.rs:156; [CX-CLI]; [GS-CLI] | Generate Bash completion and observe all declared commands and flags offered, including resume once added to the specification. |
| R-175 | T2 | fix | ci | Assert git fixture setup succeeded before testing review | crates/rune/src/main.rs:1546; crates/rune/src/main.rs:1580; blocked git fixture | Run the review fixtures with a git stub returning failure and observe an explicit setup failure, rather than a passing no-repository test. |
| R-176 | T2 | feature | sandbox | Require a live Linux network-isolation smoke check | crates/rune-exec/src/sandbox.rs:23; TF sandbox-fixed; .github/workflows/ci.yml:40 | On a runner declaring Linux sandbox support, a disposable sandboxed socket-connect fixture must be refused and source editing must still work; unavailable isolation must be reported explicitly. |
| R-177 | T2 | fix | docs | Scope the one-unsafe-site statement to the native binary | Cargo.toml:115; crates/rune-exec/src/command.rs:758; crates/rune-web/src/exports.rs:16 | Documentation must distinguish native pre_exec from the WASM FFI sites and link each reviewed boundary. |
| R-178 | T2 | feature | packaging | Smoke-test the documented archive install path | README.md:25; xtask/src/main.rs:239; installer runtime probe | In a disposable prefix, install the staged release archive through the documented path, execute rune --version, and uninstall without changing user state. |
| R-179 | T3 | feature | providers | Support anonymous local model endpoints | crates/rune-sdk/src/agent.rs:618; crates/rune/src/provider_setup.rs:189 | Connect an explicitly anonymous loopback fixture and complete a request without storing a dummy credential. |
| R-180 | T3 | feature | providers | Add a native Gemini request dialect | crates/rune-net/src/providers.rs:18; Gemini comparison | A native Gemini fixture must round-trip text, one tool call, usage and cancellation without a compatibility gateway. |
| R-181 | T3 | feature | providers | Import an Ollama endpoint and its reported model windows | crates/rune-net/src/providers.rs:18; opencode provider comparison | Discover two local fixture models and observe explicit endpoint and context provenance without a remote catalog request. |
| R-182 | T3 | feature | providers | Expose provider capability diagnostics before the first turn | crates/rune-net/src/provider.rs:149 | Run a capability check and observe supported effort, images, tool streaming and context source, with unsupported selections rejected. |
| R-183 | T3 | feature | context | Allow an explicit SDK compaction operation | crates/rune-sdk/src/agent.rs:811 | Compact a fixture SDK conversation using a supplied summarizer and observe tool-pair invariants and checkpoint replay preserved. |
| R-184 | T3 | feature | context | Add a bounded repository symbol map | crates/rune-context/src/prompt.rs:49; aider repository-map comparison | Generate a capped map for a fixture Rust repository and observe symbol references refreshed after an edit without including ignored files. |
| R-185 | T3 | feature | context | Keep explicit user constraints pinned across compaction | crates/rune-agent/src/compaction.rs:77 | Pin three fixture constraints, compact repeatedly and observe their exact text in each following provider request. |
| R-186 | T3 | feature | sessions | Preview an undo as a per-file diff | crates/rune/src/session.rs:1234; aider undo comparison | Preview an undo and observe affected file paths and diffs without changing bytes or the mutation journal. |
| R-187 | T3 | feature | sessions | Fork a saved session at an explicit event boundary | crates/rune-session/src/tree.rs:109 | Fork a fixture at a completed tool-pair boundary and observe independent branches with unchanged original history. |
| R-188 | T3 | feature | sessions | Export usage as JSONL with stable dimensions | crates/rune-session/src/usage.rs:39 | Export fixture usage and reconcile main, review and vision counts to the original ledger without fabricated missing values. |
| R-189 | T3 | feature | tools | Add a supervised PTY shell mode on Linux | crates/rune-exec/src/command.rs:502; Codex exec comparison | Run a Linux fixture that requires a tty, send input, resize and cancel its complete process group. |
| R-190 | T3 | feature | tools | Add image attachment input to rune ask | crates/rune-net/src/message.rs:129; fx attachment comparison | Attach a fixture PNG to ask and assert one image content part reaches a supporting dialect with a size error for oversized input. |
| R-191 | T3 | feature | tools | Expose a model-callable plan update tool | crates/rune-tools/src/inventory.rs:26; Codex goal comparison | Update a three-step fixture plan and observe versioned status events without file mutations or duplicate steps. |
| R-192 | T3 | feature | tools | Expose a syntax diagnostic tool for Rust fixtures | crates/rune-tools/src/inventory.rs:26; opencode LSP comparison | Request diagnostics for a fixture Rust file and observe file, line and message from a supervised local checker. |
| R-193 | T3 | feature | permissions | Allow a read-only tool mode for review | crates/rune/src/main.rs:323; aider architect comparison | Run a review fixture whose model requests a write and observe refusal by a named read-only rule while reads still work. |
| R-194 | T3 | feature | permissions | Provide a dry-run policy simulator for a recorded tool sequence | crates/rune-policy/src/decision.rs:61 | Replay a fixture sequence in dry-run mode and observe deciding rule and outcome for every call without execution or approval writes. |
| R-195 | T3 | feature | mcp | Validate MCP OAuth authorization metadata | crates/rune-context/src/mcp/config.rs:397; [FX-MCP] | Reject mismatched protected-resource metadata and insecure non-loopback endpoints before launching an authorization flow. |
| R-196 | T3 | feature | mcp | Import one supported MCP configuration format | crates/rune-context/src/mcp/config.rs:19; goose MCP comparison | Import a fixture external config and observe a reviewable Rune config with trust required for project commands. |
| R-197 | T3 | feature | extensions | Run a user-defined pre-tool veto hook | crates/rune-agent/src/turn.rs:644; Claude hooks comparison | A fixture pre-tool hook veto must prevent execution, preserve a tool-result pair and report the hook's named reason. |
| R-198 | T3 | feature | extensions | Run a user-defined post-tool event hook | crates/rune-agent/src/turn.rs:741; Claude hooks comparison | A post-tool fixture hook must receive a redacted result event exactly once and obey its timeout. |
| R-199 | T3 | feature | extensions | Load a named subagent role from a project-safe definition | crates/rune-agent/src/subagent_tool.rs:60; Claude subagents comparison | Load a fixture role with instructions and a narrowed tool set, then assert the child cannot widen its parent's permissions. |
| R-200 | T3 | feature | sdk | Provide a host fetch cancellation and deadline contract | crates/rune-sdk/src/agent.rs:131 | A cooperative fixture HostFetch must receive cancellation and deadline values and stop a silent read within the declared bound. |
| R-201 | T3 | feature | sdk | Expose effective SDK limits and capability provenance | crates/rune-sdk/src/agent.rs:687 | Query a configured SDK agent and observe effective limits, dialect capabilities and host-supplied sources without credentials. |
| R-202 | T3 | feature | node | Expose streamed ask events with bounded buffering | bindings/node/index.js:117 | Consume fixture deltas through an async iterator and observe incremental text, a final result and cancellation with bounded buffering. |
| R-203 | T3 | feature | acp | Support image blocks through ACP | crates/rune-acp/src/server.rs:521 | Advertise image support only when an image fixture reaches the selected model and unsupported dialects return a named error. |
| R-204 | T3 | feature | packaging | Publish target-specific size and startup measurements | xtask/src/main.rs:202; .github/workflows/ci.yml:49 | Each release manifest must include its target, raw measured size and reproducible benchmark method for that artifact. |
| R-205 | T3 | feature | packaging | Add verified macOS release signing | xtask/src/main.rs:202 | Verify a macOS release signature using the published identity and observe tampered artifacts rejected by the packaging check. |
| R-206 | T3 | feature | packaging | Add a Windows archive format and replacement smoke test | xtask/src/main.rs:365; crates/rune/src/install.rs:165 | Extract a Windows release, run its version command, upgrade a disposable executable and run the new version. |
| R-207 | T3 | fix | web | Validate host-returned WASM body lengths | crates/rune-web/src/exports.rs:79 | Have the page bridge return a length larger than buffer capacity and observe a named I/O error before any slice is constructed. |
| R-208 | T3 | fix | web | Bound foreign WASM staging allocations | crates/rune-web/src/exports.rs:47 | Return a staged length beyond the configured bridge cap and observe a controlled error without allocating that length. |
| R-209 | T3 | feature | context | Enforce an explicit objective token budget | crates/rune-agent/src/turn.rs:362; [CX-GOAL] | Set a fixture goal budget, consume it over multiple turns and observe no further continuation once the accounted budget is exhausted. |
| R-210 | T3 | feature | mcp | Complete an OAuth loopback callback with state and PKCE | crates/rune-context/src/mcp/config.rs:68; [FX-MCP] | Reject wrong state, reused callback and incorrect verifier; one correct fixture exchange stores tokens privately without diagnostic leakage. |

## Execution order

- [x] R-001 | T1 | fix | config | Create private state parents during connect | acceptance: Connect in a fresh HOME with umask 002 and observe state/rune mode 700 and an open session.
- [x] R-002 | T1 | fix | tools | Bound grep context allocation before allocating | acceptance: Submit context_lines 18446744073709551615 through grep_files and observe a tool validation error while the session remains usable.
- [x] R-003 | T1 | fix | permissions | Display and resolve terminal approval requests | acceptance: In ask mode submit permission and approve the displayed shell request, then observe AUDIT_SHELL_OK.
- [ ] R-004 | T1 | fix | input | Scroll long drafts to keep the caret visible | acceptance: At 80 columns enter 160 a characters plus TAIL-END and observe TAIL-END while editing the final character.
- [ ] R-005 | T1 | fix | permissions | Normalize equivalent file paths before rule matching | acceptance: A denial for .env must also deny ./.env and its equivalent absolute workspace path without denying an unrelated file.
- [ ] R-006 | T1 | fix | permissions | Escape literal filenames in remembered grants | acceptance: Approve the literal filename foo* and observe foo* allowed while foobar remains unapproved after restart.
- [ ] R-007 | T1 | fix | scripting | Stop reporting unexecuted ask tool calls as successful | acceptance: Return a tool call to the current text-only ask path and observe a named unsupported-tool-call error rather than status success.
- [ ] R-008 | T1 | fix | tools | Connect structured questions to terminal input | acceptance: Choose Beta in the question fixture and observe Beta in the next provider request.
- [ ] R-009 | T1 | fix | packaging | Extract verified release archives before installation | acceptance: Upgrade a disposable target from a staged tar.gz release and successfully execute its version command.
- [ ] R-010 | T1 | fix | packaging | Create an exclusive unique upgrade staging file | acceptance: Precreate .rune-install-staged as a symlink and observe its target unchanged after upgrade.
- [ ] R-011 | T1 | fix | tui | Bound completion and model menus by terminal height | acceptance: At 32x8 open /model, reach the last choice, close the picker and observe no stale rows.
- [ ] R-012 | T1 | fix | tui | Wrap transcript text at the actual narrow width | acceptance: At 12 columns render 300 Ws and END-LONG-WORD and recover all 300 Ws and the complete marker from scrollback.
- [ ] R-013 | T1 | fix | tui | Refresh dimensions before laying out a draft | acceptance: Grow a 32-column draft to 80 columns and observe VISIBLE-END without another keypress.
- [ ] R-014 | T1 | fix | providers | Enforce the total provider request deadline | acceptance: Set provider_request_timeout_ms=1000 and observe a slow ongoing stream stop within 1500 ms with a timeout cause.
- [ ] R-015 | T1 | fix | providers | Enforce the first-event deadline during silent reads | acceptance: A response that sends headers and no events must stop at its configured head deadline without waiting for EOF.
- [ ] R-016 | T1 | fix | config | Accept and enforce the documented offline file setting | acceptance: Load offline=true from the user config and observe offline enabled without discarding the remaining config.
- [ ] R-017 | T1 | fix | sandbox | Carry offline mode into shell network policy | acceptance: With a Linux sandbox and offline full-access context, a shell connection to a local fixture must fail before the fixture receives data.
- [ ] R-018 | T1 | fix | config | Align the web default with the documented opt-in | acceptance: With no web setting, the documented opt-in behavior must refuse a fixture web call; explicitly enabling web_tools must allow that same permitted fixture call.
- [ ] R-019 | T1 | fix | web | Reject abbreviated private IPv4 addresses | acceptance: Fetch 127.1, 127.0.1 and 10.1 without a private grant and observe zero backend calls.
- [ ] R-020 | T1 | fix | web | Validate the resolved web destination before connecting | acceptance: A hostname resolving to loopback must be refused before HTTP, while a permitted public destination connects to the vetted address rather than resolving again unchecked.
- [ ] R-021 | T1 | fix | providers | Serialize Anthropic reasoning effort | acceptance: Build Auto and High Anthropic requests and assert the provider-supported thinking fields differ as specified.
- [ ] R-022 | T1 | fix | providers | Serialize Chat Completions reasoning effort | acceptance: Build Auto and High compatible requests and assert reasoning_effort reaches a supporting endpoint.
- [ ] R-023 | T1 | fix | providers | Serialize Responses reasoning effort | acceptance: Build Auto and High Responses requests and assert reasoning.effort reaches the fixture.
- [ ] R-024 | T1 | fix | sessions | Persist cancellation with its visible partial answer | acceptance: Cancel after STREAM-03, resume and observe STREAM-01 through STREAM-03 plus the cancelled boundary exactly once.
- [ ] R-025 | T1 | fix | sessions | Record provider failure with visible partial output | acceptance: Cut a provider after three deltas and observe those deltas and the failure boundary in the resumed log.
- [ ] R-026 | T1 | fix | scripting | Save ask exchanges unless no-save is selected | acceptance: Run rune ask --json fixture, resolve its nonempty session_id and inspect the exchange; --no-save creates no session.
- [ ] R-027 | T1 | fix | tools | Retain oversized tool results instead of discarding them | acceptance: Return output larger than max_tool_result_bytes and observe a preview plus a retrievable retained handle.
- [ ] R-028 | T1 | feature | tools | Expose a bounded read_tool_result tool | acceptance: Read two pages of a retained fixture and reconstruct its full bytes without exceeding either response cap.
- [ ] R-029 | T1 | fix | tools | Keep completed shell output until it is consumed | acceptance: Start a yielding command, let it finish, start another command and still retrieve the first command's final output.
- [ ] R-030 | T1 | fix | tools | Truncate Unicode tool descriptions at a character boundary | acceptance: Register a description of one ASCII byte plus 600 e-acute characters and observe no panic and a valid UTF-8 schema.
- [ ] R-031 | T1 | fix | config | Replace credential files atomically | acceptance: Kill a credential writer before replacement and observe either complete old or complete new JSON, never truncated authority.
- [ ] R-032 | T1 | fix | config | Serialize concurrent credential updates | acceptance: Connect two scratch provider profiles concurrently and observe both credentials after reopening.
- [ ] R-033 | T1 | fix | docs | Report the measured binary size and actual ceiling | acceptance: README and budget help must distinguish the measured 4.50 MiB GNU build from the 8 MiB ceiling and name the target.
- [ ] R-034 | T1 | feature | tui | Add a real PTY replay gate for observed defects | acceptance: CI must replay long drafts, 12-column text, short menus and draft resize through the actual rune process and compare captured grids.
- [ ] R-035 | T1 | feature | sandbox | Explain a command's effective sandbox policy | acceptance: Run rune sandbox explain on a fixture command and observe backend, writable roots, network decision and protected paths with sources.
- [ ] R-036 | T1 | feature | tui | Render fenced code with stable continuation wrapping | acceptance: Stream a fenced code fixture with long lines and observe preserved indentation and a readable continuation marker.
- [ ] R-037 | T1 | feature | tui | Add an on-demand full transcript viewer | acceptance: Open the transcript by shortcut, scroll recorded tools and replies, then close it with the draft text and caret unchanged.
- [ ] R-038 | T2 | feature | sessions | Replay saved exchanges when resuming interactively | acceptance: Save a fixture reply, resume last and observe the prior user and assistant exchange before typing.
- [ ] R-039 | T2 | fix | sessions | Initialize the resumed context meter from history | acceptance: Resume a session with 1234 input tokens and observe a nonzero initial context estimate labelled with its source.
- [ ] R-040 | T2 | feature | web | Require user authority for private network access | acceptance: A model setting allow_private=true must trigger a distinct private-network decision and cannot grant itself access.
- [ ] R-041 | T2 | fix | providers | Close the stream socket when a request is cancelled | acceptance: Cancel 50 silent streams and observe reader-thread and open-socket counts return to baseline.
- [ ] R-042 | T2 | fix | config | Reset capacity when an environment override changes model | acceptance: Override a declared 2000000-token model with RUNE_MODEL=small and observe that the old window is discarded.
- [ ] R-043 | T2 | fix | config | Reset provider-derived fields on CLI overrides | acceptance: Switch provider with a CLI flag and observe its model, endpoint and capacity resolved without using the previous provider's values.
- [ ] R-044 | T2 | fix | context | Invoke automatic compaction before an oversized CLI request | acceptance: Cross compaction_trigger_percent with fixture turns and observe a compaction event before the next model request.
- [ ] R-045 | T2 | fix | cli | Read usage periods from the documented flag | acceptance: Run rune usage --period 7d --json and observe period 7d with a seven-day interval.
- [ ] R-046 | T2 | fix | cli | Accept the documented auth status action | acceptance: Run rune auth status --json and observe a status object with exit 0.
- [ ] R-047 | T2 | fix | cli | Resolve session last before parsing an exact ID | acceptance: Save a session and run rune session last --json, then observe that session's identifier.
- [ ] R-048 | T2 | fix | cli | Resolve tree last before parsing an exact ID | acceptance: Save a session and run rune tree last --json, then observe its branch structure.
- [ ] R-049 | T2 | fix | cli | Use the documented permission explanation flag | acceptance: Run rune permissions --explain shell:pwd --json and observe one structured decision with its deciding rule.
- [ ] R-050 | T2 | fix | cli | Emit an ask JSON error for an empty prompt | acceptance: Run rune ask --json with empty stdin and observe one valid failure object on stdout and exit 1.
- [ ] R-051 | T2 | fix | config | Write settings to the active RUNE_CONFIG path | acceptance: Set RUNE_CONFIG to a scratch file, run workspace add and observe only that file updated.
- [ ] R-052 | T2 | fix | xtask | Make gate include formatting and lint checks | acceptance: On a scratch checkout with an intentional formatting violation, cargo xtask gate must fail before budget and tests.
- [ ] R-053 | T2 | fix | cli | Return an empty directory list after workspace clear | acceptance: Add one directory then run rune workspace clear --json and observe directories=[] in both response and following list.
- [ ] R-054 | T2 | fix | cli | Reject unknown effort values | acceptance: Run rune --effort banana config and observe invalid_field with the accepted efforts and exit 1.
- [ ] R-055 | T2 | fix | cli | Reject unknown permission modes | acceptance: Run rune --permission-mode banana permissions and observe invalid_field with accepted modes and exit 1.
- [ ] R-056 | T2 | fix | tools | Search original grep lines before truncating display | acceptance: Search an over-one-MiB line that lacks the phrase line truncated and observe zero matches for that phrase even though its rendered output is annotated.
- [ ] R-057 | T2 | fix | observability | Redact credentials from Endpoint Debug | acceptance: Format an Endpoint containing an audit secret with Debug and assert no secret bytes appear.
- [ ] R-058 | T2 | feature | sessions | Journal assistant deltas before a process can be killed | acceptance: SIGKILL after STREAM-03 must leave a replayable partial assistant entry without a false completed turn.
- [ ] R-059 | T2 | fix | tui | Route caught worker panic diagnostics through the renderer | acceptance: Trigger the bounded panic fixture in a debug worker and observe one diagnostic without duplicated status rows.
- [ ] R-060 | T2 | fix | observability | Report worker panics as internal failures | acceptance: A caught worker panic must report internal failure rather than the user-cancelled label.
- [ ] R-061 | T2 | fix | tui | Make the minimum-height warning match submission behavior | acceptance: At four rows either refuse prompt submission until resize or describe the supported compact mode, with the displayed statement matching request traffic.
- [ ] R-062 | T2 | fix | input | Bind the existing yank operation to Ctrl-Y | acceptance: Type abc, Ctrl-U, Ctrl-Y and observe abc restored with the caret at its end.
- [ ] R-063 | T2 | fix | input | Bind the existing composer undo operation | acceptance: Insert and delete a Unicode grapheme, invoke the documented undo binding and observe the original draft and caret.
- [ ] R-064 | T2 | fix | input | Bind the existing composer redo operation | acceptance: Undo a draft edit, invoke the documented redo binding and observe the edited draft and caret.
- [ ] R-065 | T2 | feature | input | Add an explicit newline key binding | acceptance: Press the documented newline binding and submit a two-line prompt whose exact newline reaches the provider fixture.
- [ ] R-066 | T2 | feature | input | Lay out multiline drafts with a visible caret | acceptance: Paste three lines, navigate across them and observe each edited line and the correct terminal caret row.
- [ ] R-067 | T2 | feature | input | Edit a draft using the configured external editor | acceptance: Invoke the editor binding with a stub editor that changes the scratch draft and observe the changed composer text after terminal restoration.
- [ ] R-068 | T2 | feature | input | Complete workspace paths in tool-oriented prompts | acceptance: Complete a fixture path containing spaces and observe a single correctly quoted selection without reading outside workspace roots.
- [ ] R-069 | T2 | feature | input | Search prompt history by substring | acceptance: Find an older fixture prompt by its middle word and restore it into the composer without submitting.
- [ ] R-070 | T2 | feature | accessibility | Offer a transcript mode suitable for screen readers | acceptance: Run the accessible mode and observe append-only labelled output without cursor movement or continuously rewritten status rows.
- [ ] R-071 | T2 | feature | accessibility | Offer a named high-contrast theme | acceptance: Select high-contrast with NO_COLOR unset and observe each defined foreground/background pair meeting a documented contrast target; NO_COLOR still suppresses colors.
- [ ] R-072 | T2 | feature | accessibility | Provide ASCII status and selection markers | acceptance: Select the ASCII setting and observe no non-ASCII decoration in status, completions or tool summaries.
- [ ] R-073 | T2 | feature | accessibility | Expose the active bindings in interactive help | acceptance: Run /help and verify every documented binding with a PTY action, including cancellation precedence and paste.
- [ ] R-074 | T2 | feature | tui | Make tool summaries expandable on request | acceptance: Expand a collapsed fixture tool result and observe bounded contents, then collapse it without losing surrounding transcript.
- [ ] R-075 | T2 | feature | tui | Show the active provider retry and next delay | acceptance: Serve two retryable failures and observe attempt counts and the pending delay in one renderer-owned status row.
- [ ] R-076 | T2 | fix | cli | Add resume to the shared command specification | acceptance: Run rune help resume and rune resume --help and observe valid usage included in rune reference.
- [ ] R-077 | T2 | fix | cli | Declare accepted aliases in the shared table | acceptance: For login, setup, provider, pr, issue, cost, logout and settings, help and generated reference must describe the actual alias behavior.
- [ ] R-078 | T2 | fix | cli | Reject values attached to declared boolean flags | acceptance: Run rune sessions --all=no and observe a takes-no-value error with exit 1.
- [ ] R-079 | T2 | fix | cli | Honor exact session selectors for inspect | acceptance: Run rune session --id with a valid fixture identifier and observe that session without requiring a positional argument.
- [ ] R-080 | T2 | fix | cli | Honor exact session selectors for tree | acceptance: Run rune tree --id with a valid fixture identifier and observe that session's tree.
- [ ] R-081 | T2 | fix | observability | Print each ask failure once | acceptance: Run an unauthenticated rune ask --json and observe exactly one stderr error line plus one hint.
- [ ] R-082 | T2 | fix | cli | Make unknown-command help return failure | acceptance: Run rune help not-real and observe a nonzero exit while valid help still exits 0.
- [ ] R-083 | T2 | fix | config | Validate legacy project limits instead of swallowing errors | acceptance: Load max_agent_steps=10001 and observe a named diagnostic rather than a silent unlimited default.
- [ ] R-084 | T2 | fix | config | Recover a settings lock after its writer is killed | acceptance: Kill a writer holding the settings lock and observe that the next writer completes without manually deleting a lock file.
- [ ] R-085 | T2 | fix | config | Replace policy settings atomically | acceptance: Interrupt a policy-settings write before replacement and observe a complete old or new rules file.
- [ ] R-086 | T2 | fix | tools | Validate closed tool schemas consistently | acceptance: Call read_file with a misspelled optional property and observe a schema error naming that property.
- [ ] R-087 | T2 | fix | tools | Preserve tool failure codes and repair hints | acceptance: Return NotFound with hint try other.txt from a fixture tool and observe the code and hint in the model result and session event.
- [ ] R-088 | T2 | fix | tools | Check cancellation during paged file reads | acceptance: Cancel before and during a large read_file call and observe bounded work plus a cancelled result.
- [ ] R-089 | T2 | fix | tools | Check cancellation within a single large grep input | acceptance: Cancel a grep over one large file and observe it stop before scanning the remaining file.
- [ ] R-090 | T2 | fix | tools | Correct retained-result accounting on replacement | acceptance: Retain the same handle twice and observe total_bytes equal the actual stored bytes rather than twice that amount.
- [ ] R-091 | T2 | fix | tools | Avoid advertising nonexistent handles for small results | acceptance: Render a small result preview and observe either its stored readable handle or no read_tool_result instruction.
- [ ] R-092 | T2 | fix | tools | Enforce result caps including truncation markers | acceptance: Retain cap-plus-one bytes and assert the retained record including its marker is no larger than the configured cap.
- [ ] R-093 | T2 | fix | tools | Use registered tool activity in event summaries | acceptance: Register a custom Read tool and observe read activity rather than Execute in its event and summary.
- [ ] R-094 | T2 | feature | tools | Add atomic multiple literal edits to one file | acceptance: Apply two literal replacement hunks to one workspace fixture atomically; a missing or ambiguous hunk leaves the entire file unchanged and produces a repair hint.
- [ ] R-095 | T2 | feature | tools | Add a permission-checked file rename tool | acceptance: Rename a workspace fixture and observe one mutation event, while an outside-root destination is refused.
- [ ] R-096 | T2 | feature | tools | Add a permission-checked directory creation tool | acceptance: Create nested workspace directories and observe bounded, idempotent results plus an outside-root refusal.
- [ ] R-097 | T2 | feature | tools | Expose a bounded git diff inspection tool | acceptance: Read a fixture staged and unstaged diff with a byte cap and observe labelled truncation without invoking writable git operations.
- [ ] R-098 | T2 | fix | context | Measure retained-tail capacity separately from full history | acceptance: A fixture with a large removable prefix and small retained tail must select compaction rather than Impossible.
- [ ] R-099 | T2 | fix | providers | Apply steering before a retry request is rebuilt | acceptance: Enqueue a correction during attempt one and assert attempt two contains it even when attempt one fails.
- [ ] R-100 | T2 | fix | providers | Treat offline refusal as a nonretryable policy outcome | acceptance: Run a generic offline turn host and observe no retry delay and zero outbound fetch calls.
- [ ] R-101 | T2 | fix | providers | Validate endpoint authority and control characters | acceptance: Reject https:///, https://?broken and a URL containing newline before creating a client.
- [ ] R-102 | T2 | fix | providers | Honor one-call mode in model request serialization | acceptance: Set parallel_tool_calls=1 and assert the provider request disables parallel tool calls.
- [ ] R-103 | T2 | feature | providers | Execute eligible read calls concurrently within the configured cap | acceptance: Use barrier-backed fixture reads to prove two run concurrently at cap 2 and never overlap a pending mutation.
- [ ] R-104 | T2 | fix | sandbox | Protect absent Linux git hooks destinations | acceptance: With .git/hooks absent, a sandboxed fixture command must fail to create an executable hook there.
- [ ] R-105 | T2 | fix | sandbox | Protect absent macOS git hooks destinations | acceptance: On macOS with .git/hooks absent, a sandboxed fixture command must fail to create an executable hook there.
- [ ] R-106 | T2 | fix | sandbox | Protect nested repositories within writable roots | acceptance: A sandboxed command must be refused when writing a nested repository's config or hooks, while normal source writes still work.
- [ ] R-107 | T2 | fix | sessions | Write disposable session metadata atomically | acceptance: Kill during metadata replacement and observe valid old or new metadata and replayable authoritative events.
- [ ] R-108 | T2 | fix | sessions | Distinguish durable append success from projection failure | acceptance: Force metadata write failure after event sync and observe a reported durable event with a repair warning rather than retry ambiguity.
- [ ] R-109 | T2 | fix | sessions | Clear lock-holder diagnostics before releasing the lock | acceptance: Race old-owner release with new-owner acquisition and observe the new holder's diagnostic record preserved.
- [ ] R-110 | T2 | fix | sessions | Cap event reads despite concurrent log growth | acceptance: Grow a log between metadata and read and observe bounded bytes plus a named size error.
- [ ] R-111 | T2 | feature | sessions | Export a session as a stable JSON stream | acceptance: Export a fixture twice and observe identical ordered event records without credentials or transport headers.
- [ ] R-112 | T2 | feature | sessions | Search sessions by title and prompt text | acceptance: Find a fixture session using a title fragment and a user-message fragment with workspace scoping intact.
- [ ] R-113 | T2 | feature | context | Search archived compaction records | acceptance: Compact a fixture then search for a removed symbol and observe its original record and session position.
- [ ] R-114 | T2 | feature | context | Preview the records a manual compaction would remove | acceptance: Invoke a compact preview and observe exact retained and summarized record counts without changing history.
- [ ] R-115 | T2 | feature | context | Expose token-estimate provenance in status | acceptance: Observe provider-reported versus estimated counts labelled distinctly before and after the first fixture response.
- [ ] R-116 | T2 | feature | mcp | List resolved MCP servers and startup failures | acceptance: Run rune mcp list --json and observe configured servers, transport, trust state and each startup error without secret headers.
- [ ] R-117 | T2 | feature | mcp | Add a server connectivity check without starting an agent turn | acceptance: Run rune mcp check against a local fixture and observe its protocol, tools and timeout result without a model request.
- [ ] R-118 | T2 | fix | acp | Use the session cwd supplied by the client | acceptance: Launch ACP from workspace A, create a session for B and observe relative reads, listing and instructions rooted in B.
- [ ] R-119 | T2 | fix | acp | Implement requested stdio MCP servers | acceptance: Create an ACP session with a stdio MCP fixture and observe its tool advertised and callable in that session.
- [ ] R-120 | T2 | fix | acp | Filter session listings by stored workspace | acceptance: Create fixture sessions in A and B and observe each connection list only its intended workspace with truthful cwd.
- [ ] R-121 | T2 | fix | acp | Sort sessions before applying the listing cap | acceptance: Create more sessions than the cap in shuffled directory order and observe the newest sessions consistently returned.
- [ ] R-122 | T2 | fix | acp | Apply lifecycle root parameters when reopening a session | acceptance: Load a session with a new additional-root list and observe exactly that effective list, including removal when omitted.
- [ ] R-123 | T2 | fix | acp | Update context capacity when the ACP model changes | acceptance: Switch between fixture models with different windows and observe matching subsequent usage-update size values.
- [ ] R-124 | T2 | fix | acp | Persist ACP model and effort changes for reopening | acceptance: Change model and effort, close and load the session and observe the chosen values restored.
- [ ] R-125 | T2 | fix | acp | Stop turns when protocol output can no longer be written | acceptance: Close the client's output pipe mid-turn and observe provider and tool work cancelled within the shutdown grace.
- [ ] R-126 | T2 | feature | acp | Bound unanswered client permission requests | acceptance: Leave a permission request unanswered and observe a configured deadline error plus a usable connection for a later prompt.
- [ ] R-127 | T2 | fix | acp | Reject malformed text blocks instead of silently skipping them | acceptance: Submit one valid text block plus one text block without text and observe invalid_params naming the malformed block.
- [ ] R-128 | T2 | feature | acp | Negotiate and track initialization state | acceptance: Observe a documented initialization result and refusal of session lifecycle methods until compatibility is established.
- [ ] R-129 | T2 | fix | sdk | Retry retryable SDK provider failures through shared policy | acceptance: Serve one 429 with Retry-After then success and observe one bounded retry with the caller's cancellation honored.
- [ ] R-130 | T2 | fix | sdk | Make checkpoints obey their restoration size cap | acceptance: Build a checkpoint exceeding the configured bound and observe an early named error rather than bytes the same agent cannot restore.
- [ ] R-131 | T2 | fix | sdk | Honor configured SDK max-agent-steps when prompt options omit it | acceptance: Set MaxAgentSteps=2 with default PromptOptions and observe the SDK stop after two model steps.
- [ ] R-132 | T2 | fix | sdk | Bound the SDK event queue for a stalled consumer | acceptance: Stream 100000 fixture deltas without consuming events and observe bounded memory plus an explicit backpressure outcome.
- [ ] R-133 | T2 | fix | node | Resolve explicit relative binaries against options.cwd | acceptance: Place rune only in a scratch cwd, call ask with bin ./rune and observe the stub result.
- [ ] R-134 | T2 | fix | node | Reject nonfinite and overflowing timeout values | acceptance: Infinity, NaN and values beyond the timer range must fail validation without spawning a child or emitting TimeoutOverflowWarning.
- [ ] R-135 | T2 | fix | node | Validate every required ask-result field | acceptance: A stub returning only output and exit_code must raise RuneOutputError naming missing fields.
- [ ] R-136 | T2 | fix | node | Bound captured child stdout and stderr | acceptance: Run a stub that writes beyond the configured output cap and observe a typed output-limit failure with bounded memory.
- [ ] R-137 | T2 | fix | node | Find rune.exe on Windows | acceptance: On Windows with only rune.exe in a fixture PATH, resolveBinary must return its executable path.
- [ ] R-138 | T2 | feature | node | Expose provider and offline process options | acceptance: Call ask with provider and offline options and assert both flags reach the stub before the command delimiter.
- [ ] R-139 | T2 | fix | xtask | Honor CARGO_TARGET_DIR in budget and release artifact lookup | acceptance: Build with a scratch CARGO_TARGET_DIR and observe budget and release read its binary rather than target/release.
- [ ] R-140 | T2 | fix | xtask | Serialize release manifests as JSON | acceptance: Stage a channel containing a quote and observe valid JSON with the exact channel value, or reject it before writing.
- [ ] R-141 | T2 | feature | ci | Run the declared dependency advisory and license policy | acceptance: CI must run cargo deny check and fail on a scratch dependency violating the declared policy.
- [ ] R-142 | T2 | feature | ci | Run the Node binding tests in CI | acceptance: CI must execute node --test bindings/node/test.mjs and fail when its stub contract is intentionally broken.
- [ ] R-143 | T2 | feature | ci | Build and smoke-test the WASM target in CI | acceptance: CI must compile rune-web for wasm32-wasip1 and run a configured fixture prompt through the exported boundary.
- [ ] R-144 | T2 | feature | ci | Compare releases from two independent clean build directories | acceptance: Build the same source independently with two scratch target directories and assert extracted binaries and archives have equal hashes.
- [ ] R-145 | T2 | fix | performance | Label startup minima and expose raw distribution metrics | acceptance: Budget output must label minimum-of-31, show raw median and p95, and distinguish baseline-subtracted work from process latency.
- [ ] R-146 | T2 | fix | docs | Document the real offline enforcement boundary | acceptance: Documentation must map model, web, remote MCP, local MCP and shell traffic to their tested offline behavior.
- [ ] R-147 | T2 | fix | docs | Align repository links and document package version policy | acceptance: Cargo and README links must name the intended maintained repository; a check must enforce the documented independent-or-shared Node version policy.
- [ ] R-148 | T2 | fix | observability | Preserve typed I/O causes in RuneError | acceptance: A fixture permission-denied file write must retain its source error and report a filesystem failure rather than transport_failure.
- [ ] R-149 | T2 | fix | observability | Preserve provider codes and retry metadata in JSON errors | acceptance: Serve a fixture provider code and Retry-After and observe both retained in structured error detail with secrets redacted.
- [ ] R-150 | T2 | fix | input | Preserve completion selection across resize events | acceptance: Select /new, resize from 80x24 to 32x10 and observe /new still selected.
- [ ] R-151 | T2 | fix | tui | Wrap settled slash-command output | acceptance: Run /help at 40 and 80 columns and recover every full command description from scrollback.
- [ ] R-152 | T2 | fix | input | Complete discovered custom slash commands | acceptance: Create .rune/commands/audit-command.md, type /audit and select the displayed /audit-command candidate.
- [ ] R-153 | T2 | fix | sessions | Restore saved token totals for /cost | acceptance: Resume a three-turn fixture and observe /cost reporting its saved requests and tokens before a new request.
- [ ] R-154 | T2 | fix | sessions | Normalize session-tree previews to one logical line | acceptance: Inspect a fixture containing newline and control characters and observe one escaped preview per node with no injected table rows.
- [ ] R-155 | T2 | fix | permissions | Resolve custom-tool targets through the host registry | acceptance: A custom tool with target secret.txt must reach policy with that exact target and be refused by its explicit denial.
- [ ] R-156 | T2 | fix | providers | Accept tagged unknown model IDs in the picker | acceptance: Choose qwen3:latest absent from the endpoint catalog and observe that exact ID sent, while an invalid empty ID is refused.
- [ ] R-157 | T2 | feature | scripting | Run ask through a bounded host tool loop | acceptance: A local fixture requests one allowed read, receives its result in the second request and returns a final answer within the step cap.
- [ ] R-158 | T2 | fix | scripting | Include resolved workspace instructions in ask requests | acceptance: Place AUDIT_INSTRUCTION_MUST_BE_SENT in AGENTS.md and observe it in the captured ask request according to prompt --show.
- [ ] R-159 | T2 | fix | scripting | Use bounded retry policy for ask requests | acceptance: Serve one 429 with Retry-After and then success; ask retries once within its configured deadline and never retries an invalid credential.
- [ ] R-160 | T2 | fix | extensions | Connect a narrowed synchronous subagent runner | acceptance: A local parent fixture delegates one read task and receives a child result; the child cannot call a tool outside its inherited allowlist.
- [ ] R-161 | T2 | fix | extensions | Connect skill tools to the discovered skill catalog | acceptance: Place an isolated workspace skill, call capability_search and skill, and observe its discovered name and exact bounded instructions.
- [ ] R-162 | T2 | fix | tools | Connect the advertised vision tool to a configured host | acceptance: A configured vision fixture receives one permitted image and returns its result; absent configuration omits or explicitly labels the unavailable tool.
- [ ] R-163 | T2 | fix | mcp | Connect a trusted stdio MCP server in the interactive CLI | acceptance: A trusted local stdio fixture starts, its schema is discoverable, and one model call executes through the server with bounded startup and shutdown.
- [ ] R-164 | T2 | fix | mcp | Connect a trusted HTTP MCP server in the interactive CLI | acceptance: A local HTTP fixture becomes callable online and receives zero requests with --offline, while an untrusted project definition stays blocked.
- [ ] R-165 | T2 | fix | context | Reject compaction summaries stopped at the token limit | acceptance: Return a nonempty MaxTokens summary and observe original history preserved with an explicit incomplete-summary error.
- [ ] R-166 | T2 | fix | config | Bound the complete serialized credential file before writing | acceptance: Attempt credential additions exceeding the reader cap and observe an early size error with the previous credential file still readable.
- [ ] R-167 | T2 | fix | config | Select the configured model after an environment provider override | acceptance: Configure Anthropic and Chat models, set RUNE_PROVIDER=openai and observe the Chat model selected with its own capacity.
- [ ] R-168 | T2 | fix | config | Reject zero context windows with a named diagnostic | acceptance: Load context_window=0 and observe a diagnostic naming that field and no zero-capacity effective session.
- [ ] R-169 | T2 | fix | tools | Find grep matches beyond the retained long-line head | acceptance: Put hiddenneedle after one MiB on a line and observe one exact match with bounded rendered output.
- [ ] R-170 | T2 | feature | context | Persist an explicit session objective | acceptance: Set a goal explicitly, restart and observe the identical active objective; an ordinary prompt creates no goal.
- [ ] R-171 | T2 | feature | scripting | Emit bounded JSONL events during ask | acceptance: Run a streaming local fixture and observe start, text_delta and final objects on separate stdout lines before completion, with diagnostics only on stderr.
- [ ] R-172 | T2 | feature | context | Archive original records before manual compaction | acceptance: Compact then restart and retrieve each removed original message by stable record ID, including complete tool-call/result pairs.
- [ ] R-173 | T2 | feature | tools | Stop consecutive identical failing tool calls | acceptance: Repeat an identical failed call past a configured cap and observe a named stop without executing the next duplicate.
- [ ] R-174 | T2 | feature | cli | Generate shell completion from the shared command table | acceptance: Generate Bash completion and observe all declared commands and flags offered, including resume once added to the specification.
- [ ] R-175 | T2 | fix | ci | Assert git fixture setup succeeded before testing review | acceptance: Run the review fixtures with a git stub returning failure and observe an explicit setup failure, rather than a passing no-repository test.
- [ ] R-176 | T2 | feature | sandbox | Require a live Linux network-isolation smoke check | acceptance: On a runner declaring Linux sandbox support, a disposable sandboxed socket-connect fixture must be refused and source editing must still work; unavailable isolation must be reported explicitly.
- [ ] R-177 | T2 | fix | docs | Scope the one-unsafe-site statement to the native binary | acceptance: Documentation must distinguish native pre_exec from the WASM FFI sites and link each reviewed boundary.
- [ ] R-178 | T2 | feature | packaging | Smoke-test the documented archive install path | acceptance: In a disposable prefix, install the staged release archive through the documented path, execute rune --version, and uninstall without changing user state.
- [ ] R-179 | T3 | feature | providers | Support anonymous local model endpoints | acceptance: Connect an explicitly anonymous loopback fixture and complete a request without storing a dummy credential.
- [ ] R-180 | T3 | feature | providers | Add a native Gemini request dialect | acceptance: A native Gemini fixture must round-trip text, one tool call, usage and cancellation without a compatibility gateway.
- [ ] R-181 | T3 | feature | providers | Import an Ollama endpoint and its reported model windows | acceptance: Discover two local fixture models and observe explicit endpoint and context provenance without a remote catalog request.
- [ ] R-182 | T3 | feature | providers | Expose provider capability diagnostics before the first turn | acceptance: Run a capability check and observe supported effort, images, tool streaming and context source, with unsupported selections rejected.
- [ ] R-183 | T3 | feature | context | Allow an explicit SDK compaction operation | acceptance: Compact a fixture SDK conversation using a supplied summarizer and observe tool-pair invariants and checkpoint replay preserved.
- [ ] R-184 | T3 | feature | context | Add a bounded repository symbol map | acceptance: Generate a capped map for a fixture Rust repository and observe symbol references refreshed after an edit without including ignored files.
- [ ] R-185 | T3 | feature | context | Keep explicit user constraints pinned across compaction | acceptance: Pin three fixture constraints, compact repeatedly and observe their exact text in each following provider request.
- [ ] R-186 | T3 | feature | sessions | Preview an undo as a per-file diff | acceptance: Preview an undo and observe affected file paths and diffs without changing bytes or the mutation journal.
- [ ] R-187 | T3 | feature | sessions | Fork a saved session at an explicit event boundary | acceptance: Fork a fixture at a completed tool-pair boundary and observe independent branches with unchanged original history.
- [ ] R-188 | T3 | feature | sessions | Export usage as JSONL with stable dimensions | acceptance: Export fixture usage and reconcile main, review and vision counts to the original ledger without fabricated missing values.
- [ ] R-189 | T3 | feature | tools | Add a supervised PTY shell mode on Linux | acceptance: Run a Linux fixture that requires a tty, send input, resize and cancel its complete process group.
- [ ] R-190 | T3 | feature | tools | Add image attachment input to rune ask | acceptance: Attach a fixture PNG to ask and assert one image content part reaches a supporting dialect with a size error for oversized input.
- [ ] R-191 | T3 | feature | tools | Expose a model-callable plan update tool | acceptance: Update a three-step fixture plan and observe versioned status events without file mutations or duplicate steps.
- [ ] R-192 | T3 | feature | tools | Expose a syntax diagnostic tool for Rust fixtures | acceptance: Request diagnostics for a fixture Rust file and observe file, line and message from a supervised local checker.
- [ ] R-193 | T3 | feature | permissions | Allow a read-only tool mode for review | acceptance: Run a review fixture whose model requests a write and observe refusal by a named read-only rule while reads still work.
- [ ] R-194 | T3 | feature | permissions | Provide a dry-run policy simulator for a recorded tool sequence | acceptance: Replay a fixture sequence in dry-run mode and observe deciding rule and outcome for every call without execution or approval writes.
- [ ] R-195 | T3 | feature | mcp | Validate MCP OAuth authorization metadata | acceptance: Reject mismatched protected-resource metadata and insecure non-loopback endpoints before launching an authorization flow.
- [ ] R-196 | T3 | feature | mcp | Import one supported MCP configuration format | acceptance: Import a fixture external config and observe a reviewable Rune config with trust required for project commands.
- [ ] R-197 | T3 | feature | extensions | Run a user-defined pre-tool veto hook | acceptance: A fixture pre-tool hook veto must prevent execution, preserve a tool-result pair and report the hook's named reason.
- [ ] R-198 | T3 | feature | extensions | Run a user-defined post-tool event hook | acceptance: A post-tool fixture hook must receive a redacted result event exactly once and obey its timeout.
- [ ] R-199 | T3 | feature | extensions | Load a named subagent role from a project-safe definition | acceptance: Load a fixture role with instructions and a narrowed tool set, then assert the child cannot widen its parent's permissions.
- [ ] R-200 | T3 | feature | sdk | Provide a host fetch cancellation and deadline contract | acceptance: A cooperative fixture HostFetch must receive cancellation and deadline values and stop a silent read within the declared bound.
- [ ] R-201 | T3 | feature | sdk | Expose effective SDK limits and capability provenance | acceptance: Query a configured SDK agent and observe effective limits, dialect capabilities and host-supplied sources without credentials.
- [ ] R-202 | T3 | feature | node | Expose streamed ask events with bounded buffering | acceptance: Consume fixture deltas through an async iterator and observe incremental text, a final result and cancellation with bounded buffering.
- [ ] R-203 | T3 | feature | acp | Support image blocks through ACP | acceptance: Advertise image support only when an image fixture reaches the selected model and unsupported dialects return a named error.
- [ ] R-204 | T3 | feature | packaging | Publish target-specific size and startup measurements | acceptance: Each release manifest must include its target, raw measured size and reproducible benchmark method for that artifact.
- [ ] R-205 | T3 | feature | packaging | Add verified macOS release signing | acceptance: Verify a macOS release signature using the published identity and observe tampered artifacts rejected by the packaging check.
- [ ] R-206 | T3 | feature | packaging | Add a Windows archive format and replacement smoke test | acceptance: Extract a Windows release, run its version command, upgrade a disposable executable and run the new version.
- [ ] R-207 | T3 | fix | web | Validate host-returned WASM body lengths | acceptance: Have the page bridge return a length larger than buffer capacity and observe a named I/O error before any slice is constructed.
- [ ] R-208 | T3 | fix | web | Bound foreign WASM staging allocations | acceptance: Return a staged length beyond the configured bridge cap and observe a controlled error without allocating that length.
- [ ] R-209 | T3 | feature | context | Enforce an explicit objective token budget | acceptance: Set a fixture goal budget, consume it over multiple turns and observe no further continuation once the accounted budget is exhausted.
- [ ] R-210 | T3 | feature | mcp | Complete an OAuth loopback callback with state and PKCE | acceptance: Reject wrong state, reused callback and incorrect verifier; one correct fixture exchange stores tokens privately without diagnostic leakage.

## Honest assessment

Rune's measured native payload is small, 4.50 MiB on this host, and its declared policy/sandbox boundaries are worth preserving (`xtask` budget output above; `crates/rune-exec/src/sandbox.rs:23`). It currently loses to fx in demonstrated headless tool execution and session saving, Markdown presentation, transcript inspection and composer layout (ask-wiring probe; fx headless/PTY captures; [FX-TRANSCRIPT]; [FX-LAYOUT]). The same-host startup samples favor fx, although Rune has the smaller measured binary. That conclusion applies to these Linux artifacts, not all targets or cold starts. The reviewed source establishes meaningful extra APIs across Codex, Claude, opencode, Crush, Goose, aider, Gemini and Amp; version/help runs do not establish their paid-provider reliability or superiority on coding tasks.

The three largest structural weaknesses are composition gaps, untested authority/persistence edges, and terminal layout ownership. Composition advertises unsupported questions, children, vision and an empty skill catalog, while ask bypasses the real tool loop and MCP remains a library (`crates/rune-tools/src/inventory.rs:110`; `crates/rune/src/session.rs:3018`; `crates/rune/src/ask.rs:205`; `crates/rune-acp/src/server.rs:547`). Authority/persistence probes show equivalent-path denial bypass, literal-grant widening, concurrent credential loss and failure after a durable event (`crates/rune-policy/src/rules.rs:281`; `crates/rune-policy/src/approval.rs:185`; credential/session probe2). Layout uses the wrong width or uncapped menu height and permits diagnostics outside its renderer, causing measured lost text and stale regions (TF-02 through TF-06 and TF-14).

Do R-001, R-002 and R-003 first: make first connect enter a session, stop a model argument aborting the release process, and make the interactive permission mode collect an actual approval. These remove an onboarding blocker, a demonstrated crash and an unusable core interaction. R-004, composer caret visibility, is the next direct improvement to the rough terminal experience. The following headless and result-retrieval items close demonstrated fx gaps; broad hooks, cloud synchronization and an obligatory server should wait (ask-wiring/fx headless probes; [FX-RESULT]; `README.md:119`).

Do not replace working boundaries merely to enlarge the feature list. Preserve conversation tool-pair validation, valid-prefix session replay, staged mutation identity/hash checks, and the command environment allowlist (`crates/rune-net/src/message.rs:255`; `crates/rune-session/src/event.rs:497`; `crates/rune-tools/src/mutation.rs:420`; `crates/rune-exec/src/command.rs:293`). The final isolated native suite passed 2,504 tests, but the newly reproduced Unicode, allocation, terminal and composition defects show why helper tests are insufficient. The next standard should be a fixture reaching the actual entry point and asserting the provider request, side effect, persisted event and terminal result appropriate to that change.
