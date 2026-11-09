# Rune improvement plan

Started 2026-11-09 from `main` at `844ee56` (0.1.17). Supersedes the open items of
`NEXT-PHASE.md` where they overlap. Each task is one commit; the commit that lands a task
also ticks it here, so this file is always the resume point.

## Goal

Beat two products on their own measurements while staying small:

- **fx** (vercel-labs/fx, Zig, v0.0.16 on 2026-10-29). Leads on prompt-cache discipline
  (0.0.16: 41% fewer uncached input tokens, skills sent only on the turn that names them),
  Markdown rendering, `/fork`, `/effort`, memory bounds. About 6 MiB.
- **Sutra** (cneuralnetwork/sutra, TypeScript on Node, first release 2026-11-05). Its
  published benchmark runs 4 small JS bug-fix tasks x 3 trials on `deepseek-flash` through a
  metering proxy and ranks harnesses on pass rate, median wall time, cost per solved task and
  input tokens per run. Its best row: 12/12, 9.4 s, $0.0015, 29,181 input tokens per run.
  Its wins come from test-output deltas, pointer re-reads, skipped identical test runs, a
  verification gate, diff approvals, and a thread-style TUI with an end-of-turn summary.

Rune must win on those axes (tokens, cost, time, pass rate) and keep what neither can match:
a native binary under the size and startup budgets, an OS sandbox, offline mode, and
per-setting provenance.

## Baseline (measured 2026-11-09 at `844ee56`, aarch64-apple-darwin)

| Measure | Value | How |
| --- | --- | --- |
| Release binary | 3,851,632 bytes (3.67 MiB) | `cargo xtask budget` |
| `--version` / `--help` work | 1.50 ms / 1.52 ms (budgets 5 / 8) | `cargo xtask budget` |
| Instructions, this repository | 4,943 bytes (map included) | `cargo xtask budget` |
| Instructions, a JS repository, empty home | 992 bytes | `rune prompt --show` |
| Instructions, a JS repository, this user's home | 10,323 bytes (18 user skills, one listed twice) | `rune prompt --show` |
| Tool schemas | 17,379 bytes over 20 tools (about 4,344 tokens per request) | `rune prompt --tools` |
| Input tokens per run, Sutra task set | not yet measured | 9.3 |

Largest schemas: `shell` 2,042, `edit_file` 1,998, `grep_files` 1,308, `syntax_diagnostics`
1,039, `ask_user_question` 1,005, `subagent` 982. Dead by default: `web_fetch` 738,
`web_search` 771, `vision` 491; `skill` 588 and `capability_search` 409 with no skills.

## Where Rune stands (verified against the tree at `844ee56`)

Already present: interactive approval picker (run once or deny), effort on the wire,
auto-compaction at 80%, parallel read-only calls (up to 8), result spill with
`read_tool_result`, MCP in the interactive session, subagents, skills, sandbox, offline.

Gaps that cost tokens, pass rate or user trust:

1. Tool schemas are roughly 15 to 20 KB per request, about three times the instructions,
   and web, vision and question tools are advertised where they cannot run.
2. No Anthropic `cache_control` and no OpenAI `prompt_cache_key`. On Rust workspaces the
   repository map is regenerated inside the system text on every request, so any edit breaks
   the cached prefix.
3. Shell output keeps the head and drops the tail, where test failures and summaries live.
   Full test logs re-enter the context on every run.
4. Anthropic output is capped at 8192 tokens because the loop never sets a ceiling.
5. Narration between tool steps never reaches scrollback; tool rows are printed after the
   turn, twice, with no target or result; no spinner, elapsed time, Markdown or cost.
6. Approvals show no diff and offer no allow-for-session choice.
7. In `rune ask` under the default mode, `npm test` and similar are refused, so a headless
   run cannot verify outside Rust.
8. No price table: `total_cost` is never filled.
9. `rune-sdk` carries a second turn loop beside `rune-agent`.
10. No harness that runs real tasks against a model.

## Working rules

- Build with `CARGO_INCREMENTAL=0`; keep one `target/`; check `du -sh target` and run
  `cargo clean` when it passes about 6 GiB. The machine has little free disk.
- Run the focused test for the touched crate per task; run the full workspace suite before
  each push.
- Push to `origin/main` after 15 to 20 local commits, then check CI with `gh run list`.
  A push deploys the site, so pushes are batched.
- Commit subjects follow `type(scope): summary`; no em dashes in committed text.
- Numbers in README, site or CHANGELOG come from a measurement taken in this repository.

## Known local failures (macOS host, present on `844ee56` before any change)

- `rune-acp` `output_failure_tests`: 3 tests exceed the shutdown grace.
- `rune` `terminal_question` and `terminal_accessible`: the PTY child does not exit in 10 s.

- `rune` `mcp_http`, `mcp_stdio`, and every PTY suite (`terminal_*`, 17 binaries): the
  harness times out waiting for exit after `/quit`. A minimal PTY session with a turn exits
  in 0.2 s, so the cause is in the harness on this host; CI is the arbiter for these.

These are compared against `main` before a push rather than counted as regressions.

## Phases

### Phase 1: Baseline and guardrails

- [x] 1.1 Measure and record the baseline table above (size, startup, instruction bytes).
- [x] 1.2 Report tool schema bytes in `rune prompt` and gate instructions plus schemas (the
      request floor) in `cargo xtask budget`.
- [x] 1.3 Skip site deployments for pushes that do not touch `site/` (Vercel `ignoreCommand`).
- [x] 1.4 Run the budget job on pushes to `main`, not only on pull requests.

Done when the request floor is a printed, gated number.

### Phase 2: Request floor diet

- [x] 2.1 Advertise a tool only when this host can run it: no web tools while web tools are
      off, no vision without a vision model, no question tool without a question channel, no
      skill tools with an empty catalog.
- [x] 2.1a List each skill once when the same name is discovered in more than one root.
- [x] 2.2 Rewrite tool descriptions and schemas for brevity without losing a constraint the
      model needs; record before and after bytes.
- [x] 2.3 Keep the instructions byte-stable for a whole session: build the repository map
      once at session start instead of on every request.
- [x] 2.4 State the environment once in the stable prefix: platform, shell, workspace root,
      and whether it is a git repository.
- [x] 2.5 Bound each skill catalog entry (first sentence, a fixed byte cap) so a home with
      many installed skills does not pay kilobytes per request; the body still loads in full.

Done when the request floor is at most half of the 1.2 baseline and a test pins prefix
stability across steps.

### Phase 3: Prompt caching and accounting

- [x] 3.1 Anthropic: send `cache_control` on the tool list, the system text and the latest
      user turn.
- [x] 3.2 Responses: send a `prompt_cache_key` derived from the instructions and tools.
      Chat Completions is left alone: generic servers differ on unknown fields.
- [x] 3.3 Count cached input in the context meter, and let `/compact` lower the meter.
- [x] 3.4 Take the output ceiling from the model catalog instead of a fixed 8192.
- [ ] 3.5 Return `reasoning_content` on Chat Completions assistant turns where the endpoint
      requires it during tool use. Deferred to 9.5: older DeepSeek reasoners reject the
      field and newer thinking-with-tools modes require it, so it needs a live check.

Done when a recorded Anthropic exchange shows cache reads on the second request.

### Phase 4: Tool output economy

- [x] 4.1 Shell truncation keeps head and tail with an omitted-bytes marker.
- [x] 4.1b Spill previews keep head and tail too.
- [ ] 4.2 Recognize test runners (cargo, node --test, npm, vitest, jest, pytest, go test) and
      lead the result with parsed pass, fail and skip counts.
- [ ] 4.3 A repeated test command returns a delta against its previous run: new and changed
      failures in full, unchanged failures by name, newly passing tests, and a handle to the
      full log.
- [ ] 4.4 A re-read of an unchanged range returns a pointer to the earlier call; an edit
      invalidates the file's earlier reads.
- [ ] 4.5 Edit results report `+N -M` lines and the changed hunk, compactly.

Done when a scripted debugging session sends measurably fewer result bytes with every fact
still one call away.

### Phase 5: Turn loop quality

- [ ] 5.1 Record each step's narration in order with the tool calls, for scrollback, `ask`
      and sessions.
- [ ] 5.2 Verification check with no extra model call: when a turn edited files and no test
      or build ran after the last edit, ask the model once to verify; report `verified` or
      `unverified` in the turn result and in `ask --json`.
- [ ] 5.3 Stop repeated identical successful calls, not only repeated failures.
- [ ] 5.4 Recover an empty reply cut by an output limit with one retry, then report the limit.

Done when the turn result carries ordered steps and a verification verdict.

### Phase 6: Live turn rendering

- [x] 6.1 Tool results carry structured metadata: target, exit code, duration, line and
      match counts, `+N -M`.
- [x] 6.2 Draw tool rows live while a turn runs: status glyph, tool, target, right-aligned
      result; a spinner with elapsed time in the live region.
- [x] 6.3 Scrollback keeps narration and tool rows interleaved in the order they happened,
      one row per call.
- [x] 6.4 End-of-turn summary line: files changed, tests, tokens with cache share, cost, time.
- [ ] 6.2b Animate the running row (spinner and elapsed time) from a ticker.
- [ ] 6.5 Footer shows session cost and the running turn's elapsed time.

Done when a PTY test pins the live rows and the summary.

### Phase 7: Markdown, code and diffs

- [ ] 7.1 Streaming-safe Markdown: headings, emphasis, inline code, lists, quotes, links,
      tables, rules.
- [ ] 7.2 Code-fence highlighting from a small lexer (keywords, strings, comments, numbers)
      for the common languages, within the size budget.
- [ ] 7.3 Coloured diffs for edit results and `git_diff`.

Done when the binary stays inside its ceiling and a snapshot test pins each element.

### Phase 8: Approvals and control

- [ ] 8.1 The approval prompt shows the diff for an edit and the full command for a shell
      call, with `y` once, `a` for the session (never offered for destructive actions), `n`.
- [ ] 8.2 Routine test and build commands for common ecosystems are allowed in auto mode
      inside the sandbox, so headless runs can verify.
- [ ] 8.3 `/plan` read-only mode enforced by policy, and `/permissions` to switch mode.
- [ ] 8.4 `/effort` and `/fork` slash commands.

Done when a headless run on a JS task can run its tests without full access.

### Phase 9: Cost, budgets and benchmarks

- [ ] 9.1 Prices from the models.dev catalog already fetched for limits; fill `total_cost`;
      show cost in `/cost`, the summary line and `ask --json`.
- [ ] 9.2 A session spend limit that pauses before the next request.
- [ ] 9.3 `rune bench`: run tasks in the Sutra task format (`task.json`, `repo/`), each in a
      fresh copy, judged by `verify` after restoring protected paths; report pass, time,
      tokens, cache share and cost.
- [ ] 9.4 A harder task set of our own: multi-file, long debugging, several languages.
- [ ] 9.5 Head-to-head run against Sutra and fx on one model and key through one metering
      proxy; publish only measured rows.

Done when a reproducible table exists with Rune in it.

### Phase 10: Consolidation and release

- [ ] 10.1 Move `rune-sdk` onto the `rune-agent` turn loop so every surface shares one loop.
- [ ] 10.2 Re-measure size and startup; trim anything that grew without paying for itself.
- [ ] 10.3 README, COMMANDS, CHANGELOG and site updated to measured numbers only.
- [ ] 10.4 Release 0.2.0.

## Log

| Date | Commit | Task | Note |
| --- | --- | --- | --- |
| 2026-11-09 | 3a45e19 | 1.1 | Plan written, baseline measured. |
| 2026-11-09 | c7edc18 | 1.2 | `rune prompt --tools`; budget gates schemas at 18 KiB (measured 17,379). |
| 2026-11-09 | e15f68e | 1.3 | Pushes that change crates used by the demo still deploy, because `site/demo/rune.wasm` must be regenerated; regenerate it once per push, in the last commit. |
| 2026-11-09 | a7a89fc | 1.4 | Budget job no longer limited to pull requests. |
| 2026-11-09 | e775938 | 2.1 | Interactive default: 20 tools, 17,379 bytes to 15 tools, 14,382 bytes. A call to a hidden tool is answered as never offered. |
| 2026-11-09 | 8ec7540 | 2.1a | A byte-identical skill in two user roots is listed once. This home: 10,323 to 9,354 bytes. |
| 2026-11-09 | 428cd0b | 2.2 | 14,382 to 9,154 bytes (17,379 to 9,154 with 2.1, -47%). Ceiling lowered to 10 KiB. Demo wasm is stale until regenerated before the push. |
| 2026-11-09 | 84d6fa4 | 2.3 | Map built at the first request and kept until compaction; session, `ask` and the web host share `MappedInstructions`. |
| 2026-11-09 | e618740 | 2.4 | One environment line, 145 bytes; this repository's prompt 4,943 to 5,087 bytes. |
| 2026-11-09 | abea25b | 2.5 | `skill_description_bytes` default 1024 to 256. This home: catalog 8,290 to 5,892 bytes; JS-repo prompt 9,354 to 7,196. Phase 2 closes with tools at 52.7% of baseline (target was 50%); the remainder is `edit_file` and `shell`. |
| 2026-11-09 | 355f228 | 3.1 | Three markers: last tool, system block, newest message. Cache reads against the live API not yet observed (needs a key). |
| 2026-11-09 | 3a26d7e | 3.2 | Key is `rune-` plus 32 hex of SHA-256 over the prefix; same prefix, same key, no session identifier sent. |
| 2026-11-09 | 620afb1 | 3.3 | `Usage.input_tokens` now means the whole prompt in every dialect; Anthropic adds its cache counts back. Manual `/compact` resets and re-estimates the meter. |
| 2026-11-09 | a8a3d7a | 3.4 | Anthropic only (`requires_output_ceiling`); min(catalog or config, 32,768); unknown stays 8,192. Objective accounting no longer re-adds cache counts. |
| 2026-11-09 | 388cd3f | 4.1 | Render: a third head, the rest tail, cut at line breaks. Capture: a quarter-cap ring of the newest bytes past the head, shown once the command ends. |
| 2026-11-09 | 8da55ca | 4.1b | Preview names the exact omitted byte range for `read_tool_result`. |
| 2026-11-09 | 811b10f | push | First batch of 17 commits pushed; CI run 42609046209. |
| 2026-11-09 | 432ea66 | 6.1 | `ToolFinished` carries output and elapsed time. |
| 2026-11-09 | (this) | 6.2-6.4 | Narration and `✓/✗ tool target · result · time` rows committed live; running rows in the live region (static, no animated spinner yet); one dim summary line per turn. Words instead of glyphs for screen readers. |
