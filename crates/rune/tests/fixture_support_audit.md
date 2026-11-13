# PTY and provider fixture portability audit

Scope: every Python file in this directory and its Rust launchers, including
`terminal_replay.rs`, which also runs the draft and menu scripts. No fixture,
assertion, scenario, or existing timeout was removed or relaxed.

## Approval failure mechanism

1. After the last cancelled turn, the driver waits for the committed
   `SESSION_STILL_USABLE\r\n` line. That line can arrive before the remainder of
   the closing frame has been read.
2. It writes `permission-draft\r` to the master. Rune is already in raw mode:
   `session.rs` constructs `KeyReader` before announcing the session. Earlier
   successful turns also rule out a startup-only failure here.
3. Rune consumes keys and redraws the composer; submission records the user
   message and starts `run_turn_steerable`. The worker should POST a
   chat-completions request whose last message is the user text
   `permission-draft`.
4. Only that POST sets the provider's `draft_request`. Previously the driver
   waited for it with `Event.wait(10)`, without reading any terminal output.
   Rune can block on a repaint before reaching the provider. Darwin's small
   output queue exposes this dependency; Linux can buffer more of these frames.
5. Keeping another slave open prevents last-close output loss. It does not
   provide queue capacity or consume the unread closing/composer frames.
   The explicit 24-by-100 size provides a deterministic layout, not flow control.

`wait_for_condition` now pumps terminal output until the provider event is set,
under the original ten-second deadline. The provider still waits for the driver
to observe the full correction and all three Left keys before releasing its
tool call. Approval/question keys therefore cannot consume a pending caret move.

The blocked-write explanation is an inference from this ordering and the
previous Darwin queue evidence, not a captured macOS stack trace. A Mac run is
required to establish the exact blocked write and confirm this reported failure
is gone. The Linux regression forces the same dependency with a 4-MiB write
before a child signals the driver, exceeding either platform's queue.

## Shared rules

- `write_terminal` handles partial writes and input backpressure while draining
  output. All product PTY key writes use it. Delivery is not acknowledgment;
  fixtures still wait for product output or a provider condition.
- Initial input follows a product banner/footer/input announcement, never a
  startup sleep. Raw mode is enabled before the interactive announcement.
  Accessible sessions intentionally retain canonical input and send LF.
- Every product PTY has an explicit `TIOCSWINSZ` before spawn and retains a slave
  through capture. Resize scenarios record their ioctl boundaries. `COLUMNS`
  and `LINES` do not determine Rune's layout: `shell::terminal_size` reads the
  terminal, and `SessionHost::refresh_size` refreshes it for each frame.
- Text from a live row does not acknowledge turn completion. Subsequent
  commands/prompts wait for the committed CRLF line emitted by `close_turn`
  after the worker joins. Silence only settles bytes for screen replay.
- Shutdown always pumps through `wait_for_exit`, including kill cleanup. The
  remaining `child.wait` branches have `master is None`; normal stop clears
  both handles together. They cannot wait on a writing PTY child.
- Inheriting fixtures use `fixture_environment`, clearing ambient `RUNE_*`,
  proxy, color, editor and dimension settings before setting scenario values.
  Other fixtures construct an explicit environment. Credentials, endpoints,
  config/data/state directories and terminal capabilities are fixture-owned.
- Rust launchers prepend the helper and use `Command::output`, which captures
  Python's pipes while waiting. They do not wait for provider events or write
  product keys themselves. Their Rune executable is supplied by Cargo, not
  found by a Linux-specific installation path. No launcher changes are needed.

## Fixture-by-fixture findings

| Python fixture (and corresponding Rust launcher) | Changes or evidence for retained behavior |
| --- | --- |
| `terminal_approval` | Replaced the undrained draft-provider wait. Retained the explicit final Left/caret acknowledgment and every approval, denial, cancellation and piped refusal assertion. |
| `terminal_question` | Replaced the identical provider wait; added the missing final Left/caret acknowledgment before releasing the provider; cancellation now waits for the committed boundary. |
| `terminal_accessible` | Provider-first-delta wait now drains canonical echo and product output. Fixed-duration drains intentionally test absence of partial/idle output and keep reading throughout. Reads use the shared EOF handling, without suppressing unrelated I/O errors. Accessible assistant lines are emitted after the worker, not streamed live. |
| `terminal_draft` | Editor file/provider handshakes now pump. Captures acknowledge a frame after expected text; initial text-plus-Left captures also acknowledge the final caret. Final submission records its offset before writing and waits for a fresh committed reply. The editor subprocess's sleep polls its own release file while the driver keeps draining; it does not hold the driver's PTY reader. Seeded history uses a resolved root to match the product's real workspace. Editor commands quote both the Python executable and paths with spaces. |
| `terminal_help` | Added final caret and viewer-frame acknowledgments to captures. Standalone cancel/clear keys acknowledge the return to the composer before the next write, including isolated Escape. Provider releases remain in server threads; the driver reads frames and committed replies. All external editor paths are quoted. |
| `terminal_ascii` | Added the committed reply acknowledgment before opening history. Provider pauses after streaming text while the driver reads; tool-summary and code assertions remain. Color/proxy/editor inheritance is now isolated in every scenario. |
| `terminal_high_contrast` | Key delivery and environment isolation use the helper. Each initial/menu capture waits for product output and then drains; it uses an offline provider and explicitly supplies truecolor/indexed/NO_COLOR variants, including empty NO_COLOR. |
| `terminal_menu` | Tagged-model reply now waits for completion before quit. All other menu captures wait for the selected row or close-frame output. Keys buffered during model discovery are consumed by the picker after discovery; no sleep or provider-event wait blocks the driver. |
| `terminal_replay` | Provider-finished wait pumps; both captured-byte equality and the original no-paint readiness assertion protect the open viewer. Text-plus-Left captures and completion acceptance/editing wait for final caret positions; help waits for its committed ending. Other captures wait for explicit text, a frame, or resize output. Provider-held code/live steps are released by the draining driver. F2 deliberately causes no edit but does acknowledge the unchanged composer frame. |
| `terminal_cancellation` | Cancellation and subsequent replies wait for committed lines before examining the journal, continuing or quitting. STREAM-03 intentionally remains a live marker for cancellation; the provider holds STREAM-04 until released. |
| `terminal_process_death` | Continuation replies wait for committed lines before quit/relaunch. STREAM-03 intentionally remains live: the scenario kills an unfinished turn and asserts the saved deltas, not unread shutdown output. Replay text precedes the initial footer in the same ordered output stream. |
| `terminal_provider_failure` | Failure and continuation waits use committed lines. The provider's cut-stream wait runs on its own thread while the driver reads STREAM-03; journal assertions follow the saved failure boundary. |
| `terminal_resume` | Continuation waits for a committed reply. Launch waits for a footer after saved replay output, then drains. A flag-shaped session ID is already pinned; neither random ID spelling nor a startup delay controls resume. |
| `terminal_retry` | Completed capture waits for the committed reply rather than silence after a live delta. Pending/active retry frames have explicit frame terminators and provider release events. Existing Retry-After values and attempt count are the behavior under test, not added fixture retries. |
| `terminal_token_provenance` | Already waits for committed numbered replies before status or another prompt. Status captures wait for explicit source labels; key delivery and environment isolation now use the helper. |
| `mcp_stdio` | Already pumps during the tools/call file wait, provider replies and shutdown. Cancellation now explicitly waits for the committed boundary too. The MCP child's deliberate infinite sleep tests deadlines/cancellation; it cannot block the driver from reading Rune. Workspace/trust paths already resolve `/var` aliases, and the declared server uses `sys.executable`. |
| `mcp_http` | Already pumps initial/provider/exit waits and waits for a committed reply. Explicit environment and resolved workspace; bind and client URLs use the same listening socket address, with the existing Connection-close probe. Only key delivery needed migration. |
| `skills` | Pipe-only: `subprocess.run` communicates and captures both output pipes. Explicit environment; resolved workspace and resolved returned skill path prevent `/var` versus `/private/var` equality failures. No startup sleeps or driver event waits. |
| `subagent` | Pipe-only with `subprocess.run`, explicit environment and relative workspace reads. Assertions compare tool results, permissions, roles and usage, never the spelling of the temporary root. Server binds before Rune launches. |
| `vision` | Pipe-only with `subprocess.run` and explicit environment. Image paths are workspace-relative; outside-root and byte-limit checks compare results, not absolute path spelling. Server binds before Rune launches. |
| `fixture_support_test` / `fixture_support.rs` | Retained EOF/last-close/shutdown coverage; added provider-condition backpressure, timeout diagnostics, simultaneous input/output backpressure with forced short writes and a byte-order digest, and inherited-environment coverage. Its deliberate sleeps occur in a concurrent producer or a bounded poll simulation, while the driver continues reading. |

Other temporary roots above are used to open the same files through a filesystem
alias, not compared with product-reported absolute paths. Their state/config
directories remain isolated. Rust grid snapshots already mask the variable
workspace suffix while preserving model, prompt, menu and caret assertions.

The stdio MCP fixture's existing 300-ms protocol limits and eight-second lifecycle
assertion remain intentional deadline coverage. The client starts pipe readers
at spawn and waits for actual JSON-RPC responses; there is no guessed startup
sleep to replace. Successful Python server handshakes must still meet those
limits on the Mac runner. This audit does not claim they are immune to machine
starvation, and does not extend them to obtain a pass.

## Verification boundary

The host used for this change is Linux. Driver tests force bounded-queue
dependencies independently of its larger PTY queue; product fixtures and the
workspace check validate the changed ordering here. Darwin last-close/revoke,
small-queue behavior, key decoding and the complete fixture suite still need
the macOS CI runner. No Mac result is inferred from the Linux pass.
