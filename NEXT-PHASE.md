# Rune next phase: audit and plan

Written 2026-10-15 for branch `docs/next-phase`, off `main` at `611f436`. Sources: the five research
files under `/home/hermes/vault/rune-audit/research/` (referenced below as `00` CI reality, `01`
competitors, `02` benchmarks, `03` mainstream gaps, `04` repo findings), the previous `AUDIT.md`, and
Rune's own `README.md`, `COMMANDS.md`, `CHANGELOG.md` and `site/index.html`. Each borrowed fact names
its file. Where research and the repository disagree, the repository measurement wins and the
disagreement is stated. Nothing here is a claim that a test already exists or passed.

## 1. Where Rune stands

Rune is a single-binary, model-agnostic coding-agent harness in Rust, Apache-2.0, with no daemon, no
account and no telemetry (`README.md:19`, `03` section 2). It ships a terminal UI, one-shot `ask`,
`review`, sessions, an ACP server, a Rust SDK, a Node binding and a WASM module (`03` section 2).

**Published numbers.** `README.md:7` says "the audited stripped release build measured 5.43 MiB for
`x86_64-unknown-linux-gnu`", with an 8 MiB ceiling and startup work budgets from baseline-subtracted
minimum-of-31 measurements. `site/index.html:470,491` repeats 5.43 MiB and "5 ms version work
ceiling". The ceilings are coded at `xtask/src/main.rs:20` (8 MiB), `:23` (5 ms `--version`) and
`:26` (8 ms `--help`).

**Measured now.** At `611f436` the stripped release binary is 5,695,824 bytes, or 5.432 MiB (`04`
section 0), which agrees with the published 5.43. `04` F-9a notes `CHANGELOG.md:738` still says
"5.40 MiB", so the published story is internally inconsistent by 0.03 MiB. The previous audit at an
earlier commit recorded 4,717,720 bytes (4.50 MiB) and a README headline near 3 MiB (`AUDIT.md:60,68`),
so the tree and the published number both moved this phase.

**Startup.** The budget run measured a 1.24 ms process floor, `--version` 2.31 ms raw and 1.07 ms work
against a 5 ms budget, and `--help` 2.82 ms raw and 1.58 ms work against an 8 ms budget
(`AUDIT.md:60-66`). Those are one host's numbers, not a cross-host claim.

**What changed in the last phase.** The expansion plan landed 209 of 210 items (`AUDIT.md:9-21`);
only R-006 remains open. `CHANGELOG.md:192` and `README.md:464` say `rune ask` now runs the shared
host tool loop, which retires the previous audit's finding that `ask` reported unexecuted tool calls
as successful. CI was re-enabled after being disabled manually on 11 Oct, and its first honest run on
the merged tree shows 5 of 7 jobs failing (`00`, `04` F-1): the phase's local green and the runner's
red had no chance to disagree until then. Feature-wise Rune already matches the 2025-era table stakes
that all eight mainstream harnesses converged on (`03` sections 4 and 8).

## 2. What is broken or unproven, ranked

Ordered by user and developer impact. Every item cites the evidence, the owning file and line, the
impact, and the smallest fix.

**1. CI is red in 5 of 7 jobs; every branch is blocked (F-1).** `gh run list` shows all 20 recent runs
failed; `main` and PR run `38802902074` / `38801709142` fail in the same way (`00`, `04` F-1). `budget`
is PR-only (`ci.yml:124`), so on a `main` push the gate does not run at all. Owning files
`.github/workflows/ci.yml:26-27,85-86,210-211`. Impact: there is no green signal, so a new regression
is indistinguishable from standing breakage, and the four independent sub-causes follow as items 2, 4,
5 and 6 below. Smallest fix: fix those four sub-causes rather than suppressing jobs; if a platform test
truly cannot run, `#[ignore]` that one test with a linked issue.

**2. Four live RUSTSEC advisories reach the shipped binary (F-2).** `cargo deny check` fails with
`advisories FAILED` on four rustls advisories (TLS secret exposure through `Debug`, an ECH-rejection
data leak, a pre-authentication 1 GiB allocation, a ServerHello downgrade); dependency path
`rustls v0.23.45 <- ureq v3.4.2` (`04` F-2). Owning files `Cargo.lock:1217-1219` and `Cargo.toml:66`;
`deny.toml:18` has `ignore = []`. Whether each precondition applies to Rune's exact TLS configuration
is `UNVERIFIED` (`04` F-2), but the crate is in the binary and the policy job treats it as blocking.
Impact: the HTTPS path to every provider, web fetch and MCP HTTP. Smallest fix:
`cargo update -p rustls --precise 0.23.46`, commit the lockfile, confirm `cargo deny check` passes;
`ureq = "3"` accepts the bump.

**3. The per-turn prompt floor is 9,108 bytes, and 89 percent is one crate's symbol map (F-3).**
`rune prompt --show` emits 9,108 bytes over 62 lines: a 994-byte `SYSTEM_PROMPT`, an 8,114-byte
repository map (70-byte header, 40 declaration rows, 73-byte notice) (`04` section 2.2). The map stops
at 1,024 declarations in path order, and `crates/rune` alone supplies that many, so the other 13
crates are never reached; the rendered names are alphabetically limited to A through G (`04` F-3).
Owning files `crates/rune-context/src/symbol_map.rs:29-40,108-128,156-166,228-276`,
`crates/rune-context/src/prompt.rs:261,312`. The assembled prompt is not CI-gated: `xtask` checks only
size, startup and duplicate deps, `budget` is PR-only, and the one size test asserts a 512-byte cap on
a synthetic fixture (`04` F-3). Impact: every turn of every session pays about 2,277 tokens by Rune's
own bytes-divided-by-four estimate (`README.md:577`), most of it a slice of a single crate. Smallest
fix: add an assembled-prompt byte assertion to `xtask budget`, and allocate the declaration budget
round-robin or per crate instead of "first 1,024 in path order". A fix exists uncommitted on the audit
host but is uncommitted and unbuilt (`04` addendum).

**4. Ubuntu CI fails 5 process-group shell tests that pass locally (F-4).** The `fast` and
`ubuntu-latest` jobs fail 484 passed / 5 failed, all in `crates/rune-tools/src/shell.rs`
(`00` cause 2, `04` F-4); assertions expect `killed by signal 9 (SIGKILL)` but see `[exited with status
137]`, and two expect a forked child that never started. The same suite is 489 passed locally on the
same revision. Owning files `shell.rs:950-979,1009-1023,1161-1179` and the five tests at `:1280`,
`:1664`, `:1730`, `:2282`, `:2335`. The root cause is `UNVERIFIED` (three ranked hypotheses in
`04` F-4). Impact: the primary platform's CI is red on a step that is documented as the PTY gate.
Smallest fix: first make the tests print the observed pid, `ps` output and raw exit status, and
serialize them, so the next run yields a diagnosis; do not `#[ignore]` without a tracked issue.

**5. macOS CI never exits after `/exit` (F-5).** `interactive_http_mcp_obeys_trust_policy_and_offline_mode`
times out after 5 seconds and the PTY child never terminates; a sibling test failed the same way
(`00` cause 3, `04` F-5). Owning files `crates/rune/tests/mcp_http.rs:7,16` and
`crates/rune/tests/terminal_accessible.rs:16`, with the `/exit` handler in `crates/rune/src/session.rs`.
Whether this is a product bug or a harness artifact is `UNVERIFIED` (`04` F-5). Impact: macOS CI is red;
if it is the product, a macOS user cannot leave a PTY session. Smallest fix: instrument and raise the
wait to decide product versus harness, then fix the correct side.

**6. Windows CI fails 5 tests, including a sandbox report that says "refused" where the test expects
"enforced" (F-6).** The five failures are in `permissions`, `sandbox` and three `session` tests
(`00` cause 4, `04` F-6). The sandbox failure feeds a Windows path through the macOS Seatbelt backend,
and `quote` rejects any path containing `\` (`crates/rune-exec/src/sandbox.rs:499-515`), so profile
generation fails; the other four are `Inferred` to be Windows path separators and a real console width
overwriting the injected test width. Owning files `crates/rune-exec/src/sandbox.rs:499-515`,
`crates/rune/src/sandbox.rs:293-305`, `crates/rune/src/session.rs:1220,5741,5920,7828`,
`crates/rune/src/permissions.rs:386`. Impact: Windows is a supported release target and its CI is red,
and the session failures may hide real Windows UI bugs. Smallest fix: normalize `\` inside `quote` or
`#[cfg(unix)]`-gate the macOS backend entry in the test; instrument the other four before fixing.

**7. One PTY test is flaky at roughly 14 percent (F-7).** `terminal_question` failed about 3 of 21
isolated runs; the test types while the previous turn is still streaming, so the question prompt never
renders within the wait window (`04` F-7). Owning files `crates/rune/tests/terminal_question.rs` and
`terminal_question.py` (`wait_for("answer required (1/1)")`). Impact: intermittent red on the test that
guards the R-008 structured-questions feature. Smallest fix: synchronize the test on an explicit idle
marker (the `ctx ... left` footer after the previous turn) before sending the prompt.

**8. CI declares cross-platform coverage that never executes (F-8).** Because `cargo test --workspace`
fails first, the Windows ZIP replacement smoke test (`ci.yml:219-222`) and the native release staging
(`ci.yml:213-217`) are skipped on every platform and have never run in the current red state (`04` F-8).
Impact: the workflow reads as if those checks are gates when they are dead steps. Smallest fix: fix
items 4, 5 and 6, then confirm these steps run.

**9. Documentation and naming drift (F-9).** `04` F-9 lists six: (a) `CHANGELOG.md:738` says 5.40 MiB
while `README.md:7` and the measured binary say 5.43; (b) `README.md:146` gives a `capabilities`
example that exits for a missing model before reaching the effort check; (c) `site/index.html:483,575`
offers `cargo install --git`, which the README install section does not; (d) CI step names say "PTY
replay gate" and "on Unix" while they run on Windows; (e) `ci.yml:119` says the cross-platform lane is
PR-only when it has no `if:`; (f) the README's map documentation omits the single-crate, A-G
concentration of item 3. Impact: reader trust in the published numbers and the gate description.
Smallest fix: one documentation pass.

**Unproven, not broken.** `04` section 4 lists what could not be established on the audit host and
should not be read as clean: macOS and Windows runtime behavior (root causes above are `Inferred`),
the Ubuntu shell-test root cause (`UNVERIFIED`), whether the macOS exit hang is a product bug
(`UNVERIFIED`), exploitability of the rustls advisories, paid-provider behavior, real browser WASM
execution, legacy-session migration, and fault injection for the resource-accumulation, session-holder
and concurrent-log paths in `AUDIT.md:1660`.

## 3. How it compares

**fx is the head-to-head that matters.** fx is the closest same-shape product: a Vercel Labs
single-binary agent with no runtime dependency, written in Zig, Apache-2.0 (`01`, fx section). Its
changelog publishes measured size per release, 6.74 MiB for v0.0.13 and 6.51 MiB for v0.0.12, and its
site displays 6.51 MiB with no target beside the label (`01` fx). This audit measured the downloaded
fx Linux x86_64 v0.0.12 binary at 12,520,232 bytes, or 11.94 MiB, against Rune's measured 4,717,720
bytes at the time (`01` fx). The caveat is explicit in `01`: do not compare either published headline
to another target or compressed archive, because fx's published figure is platform-specific. On the
same measured Linux target as this audit, Rune's 5.43 MiB is smaller than fx's 11.94 MiB binary, but
the honest statement is "smaller than the one measured fx build", not "smaller than fx".

Rune's advantages over fx (`01`, "Where Rune already wins"): an OS-enforced execution sandbox, where
fx deliberately retired sandbox configuration and runs approved subprocesses on the host; an explicit
`--offline` switch, which the released fx rejects as an unknown subcommand; direct Anthropic and
Responses dialects alongside Chat Completions; and per-setting provenance (`config --explain`,
`limits --json`, `prompt --show`). fx's advantages: it enforces a 2 ms startup budget in coded CI
(`benchmarks/check_budgets.py`, six commands), executes tools in headless `ask` and persists a
session, and renders Markdown (headings, tables, code fences) rather than wrapping text (`01` fx).
The earlier "fx executes tools in ask, Rune does not" gap is closed in the current tree by
`CHANGELOG.md:192`; `01` was written against an older tree, so the repository wins there.

**Mainstream feature matrix, in brief.** Against `03` section 4, Rune is at parity on the 2025-era
checklist and differs in these cells: provider breadth (Rune is dialect-based, against OpenCode's 75+
catalog and Cline's 30+); MCP OAuth end to end (Rune has host libraries only, and all eight rivals
ship a login path); OS sandbox (Rune is Linux/macOS only, while Codex, Copilot and Gemini sandbox
Windows); code intelligence (Rune has `rustfmt` syntax and a Rust-only map, while OpenCode bundles
about 30 LSP servers and Qwen ships an experimental one); first-party IDE extension (Rune has an ACP
server only, every rival has an extension); persistent memory (Rune has pins and instruction files,
Claude, Codex and Copilot have learned memory); non-bash completion (Codex and Copilot generate all
shells, Rune is bash-only); live cost display (Rune has a ledger, rivals show limit bars); and an
enterprise control plane (Rune has none, every commercial rival does).

**Anti-gaps: what Rune should advertise not having** (`03` section 5). No telemetry by default, where
Cline is on by default and Claude and Codex are opt-out, and Gemini and Qwen default `logPrompts` to
true. No daemon or background service, where OpenCode V2 runs a shared service, Qwen `serve` is a
daemon, and Cline has a hub. One binary, no account, no hosted service, against Copilot's 90 to 115 MB
executables and Cursor's stored repo copies. No enterprise control plane, with permission provenance
being the useful individual-auditor substitute. These are the reasons to choose Rune, so they should
be stated as such rather than treated as gaps.

## 4. Benchmarks and claims

Rune appears in no published harness comparison (`02` subject). The field measures harnesses on
tokens, cost, wall clock and pass rate; none of the credible rigs measures binary size or startup, so
those two axes have no best number to beat (`02` section 2).

**Pass rate is dominated by the model, so Rune may not claim it.** Liu & Han run the same model through
three harnesses on 447 SWE-bench Verified tasks and find Claude Code and mini-SWE-agent equivalent
within 5 points, with swapping the harness flipping 13 percent of a hard 45-task subset, the same rate
as rerunning the same harness (`02` A2). HarnessTax, crossing 7 models with 3 harnesses, finds a
harness effect of about plus or minus 2 points on SWE-bench Lite and about plus or minus 5 points on
Terminal-Bench 2.0 (`02` A1). Vats & Golev find paired within-model pass-rate differences of 0 to 8
points with bootstrap intervals that include zero (`02` A4). The resolution caveat is stated by the
strongest study: 45 tasks catch a 13-point gap only half the time, while 447 tasks resolve about 5
points (`02` section 4). A Rune pass-rate win smaller than about 5 points would need at least 447 paired
tasks to be believed. The counterweight is that a bare or mistuned harness can move pass rate a lot
(`02` A6), so Rune should measure its own parity, not assume it.

**The winnable axes.** First-call prompt overhead: the best published floor is 829 tokens of harness
preamble and 1,382 tokens of first-call cost including the 554-token issue, both mini-SWE-agent
(`02` section 2, Liu & Han). Pi is 1,817 to 1,894, and PI.DEV is 1,147 to 1,642. Rune's 9,108-byte
floor is about 2,277 tokens by its own estimator, so Rune does not lead this axis today. Binary size:
no published benchmark measures it, and Rune's 5.43 MiB is unmatched by any measured native harness,
with Pi's standalone build at 29.6 to 43 MB (`01` pi). Startup: no published benchmark measures it
either, but fx codes a 2 ms budget, so Rune cannot claim the fastest startup (`01` fx, `02` section 2).
Robustness and cache discipline: 93 to 95 percent cache hit rates depend on prefix stability, and the
published finding is "never touch the front of the prompt" (`02` section 2). Rune's README says the
repository map refreshes before each model request, including after edits (`README.md:452-455`), which
is exactly the front of the prompt, so its cache behavior is a real risk to measure rather than claim.

**What Rune may claim, strictly.** The measured stripped Linux binary size on this target, with the
target named; the CI-enforced 8 MiB ceiling and 5 ms and 8 ms startup work ceilings, with the
minimum-of-31 method disclosed; the per-turn prompt floor in bytes as measured by
`rune prompt --show`; and its sandbox and offline boundaries. It may not claim pass-rate or
coding-quality leadership, "fastest startup", or cross-platform size parity.

**Entry routes** (`02` section 3). openbench is the recommended first move: MIT, entry is a TOML
`--candidate` manifest plus `obench gate`, results are sealed, and the open-model 72-cell matrix cost
about $1.02. Claw-SWE-Bench is MIT and SWE-bench-native but needs a Linux host with Docker, iptables
and an SNI-proxy service, which is a high infrastructure cost. FrontierHarness Eval is skill-driven and
easiest to start, but it is vendor-run and needs a matched control run before a rank. PawBench is
Apache-2.0 and needs Python, Docker and a provider key. The numbers marked `UNVERIFIED` in `02`
(including Claw-SWE-Bench's 19.1 to 73.4 percent secondary figure) must not be used as targets.

## 5. The smallest-harness thesis, measured

Concretely, "smallest harness" means four measurable properties, three of which Rune already tracks:

- **Binary size:** 5,695,824 bytes, or 5.432 MiB, for a stripped `x86_64-unknown-linux-gnu` release
  (`04` section 0), against an 8 MiB CI ceiling (`xtask/src/main.rs:20`).
- **Startup work:** 1.07 ms work for `--version` and 1.58 ms work for `--help`, over a 1.24 ms process
  floor, against 5 ms and 8 ms ceilings (`AUDIT.md:60-66`; `xtask/src/main.rs:23,26`).
- **Per-turn prompt floor:** 9,108 bytes, about 2,277 tokens by Rune's bytes-divided-by-four estimate,
  of which 8,114 bytes are the repository map (`04` section 2.2). The uncommitted change on the audit
  host would cap the map at 4,096 bytes, putting the floor near 5.1 KiB, or about 1,270 tokens; it is
  uncommitted and unbuilt, so treat that as a projection, not a measurement (`04` addendum).
- **Robustness under bloated input:** the tools bound oversized output and the previous grep
  capacity-overflow abort is now rejected at 1,000 context lines (`04` section 3), but the assembled
  prompt itself has no ceiling, so prompt bloat is invisible.

**Gated in CI today:** binary size, startup work and duplicate dependency versions, all in
`cargo xtask budget` and all only on a pull request (`ci.yml:124,141-142`). **Not gated:** the
assembled prompt floor, per-crate map fairness, and the robustness PTY replay, whose declared steps
are currently failing or skipped (items 4, 5, 6 and 8 above). On a `main` push, even the size and
startup budgets do not run. The thesis is therefore enforced for two of its four properties on pull
requests only.

## 6. The plan

Tags: `[local]` the free model can land it by editing this repository with no runner; `[runner]` needs
a machine with cargo, CI or GitHub access, or a paid provider key; `[owner]` needs a human decision
before or instead of code. Ordered by value per unit of risk.

**Phase A: restore a trustworthy gate.** Highest value, lowest risk, and a prerequisite for trusting
anything later.
- A1. Bump rustls to 0.23.46, commit `Cargo.lock`, confirm `cargo deny check` passes (fixes F-2).
  `[runner]` to fetch and verify; the manifest edit itself is `[local]`.
- A2. Instrument the five Ubuntu shell tests to print pid, `ps` and raw exit status, serialize them,
  and land a root-cause fix (F-4). `[runner]`.
- A3. Decide and fix the macOS `/exit` hang (F-5). `[runner]` plus `[owner]` on whether it is product
  or harness.
- A4. Normalize `\` in `quote` or `#[cfg(unix)]`-gate the macOS sandbox test entry, then instrument and
  fix the other four Windows failures (F-6). `[runner]`.
- A5. Make `terminal_question` wait on an idle marker (F-7). `[local]`, verified `[runner]`.
- A6. Re-run CI and confirm the previously skipped Windows replacement and release-staging steps
  execute (F-8). `[runner]`.
- Acceptance: all seven jobs green on `main` and on a pull request, with the skipped steps observed to
  run.

**Phase B: bound and shrink the per-turn floor.**
- B1. Re-land the prompt-floor change here (`repository_map_bytes` default to 4096, skip tests and
  benches, module-scope declarations only). `[local]`.
- B2. Add an assembled-prompt byte ceiling to `xtask budget` and make the budget job also run on
  `main` pushes. `[local]` plus `[owner]` on the ceiling value.
- B3. Decide and implement fair map allocation, round-robin or per crate, so `crates/rune` cannot
  consume the whole declaration budget (F-3). `[owner]` policy, `[local]` implementation.
- B4. Re-measure `rune prompt --show`, then correct the README, site and CHANGELOG figures (F-9a, F-9f).
  `[local]`.
- Acceptance: `rune prompt --show` is under the chosen ceiling, the ceiling is enforced in CI on both
  push and pull request, and no single crate supplies more than its share.

**Phase C: enter one public benchmark.**
- C1. Write the openbench `--candidate` TOML manifest for Rune and pass `obench gate`, publishing
  solve rate with Wilson intervals and the token and wall columns it already prints. `[runner]` plus
  `[owner]` on the provider key and spend.
- C2. Decide whether to contribute a startup and binary-size lens to openbench or publish Rune's own
  rig with proxy-metered tokens, Wilson intervals and medians of at least three runs. `[owner]`.
- Acceptance: Rune appears as a row with a result SHA, or the decision to stay out is recorded with
  the reason.

**Phase D: close the noticed adoption gaps.** Each is `[owner]` to choose, then agent or `[runner]`.
- D1. End-to-end `rune mcp login <server>` over the existing loopback PKCE and token store (`03` A1).
  Rune has the library pieces; this is wiring, not new cryptography.
- D2. Pick a Windows sandbox stance (D-refuse, degrade with a named reason, or implement) and match
  the CI test to it (`00` cause 4; `03` A2).
- D3. A permission-gated LSP `diagnostics` or `definition` tool that spawns a user-configured server,
  plus a tree-sitter map for more languages (`03` A3).
- D4. Make the web-search backend pluggable, with DuckDuckGo Lite as the keyless default and a keyed
  provider optional (`03` A4; `crates/rune/src/web_client.rs:24`).
- Acceptance: each chosen item has one acceptance test on the platform it claims to support.

**Phase E: claims and cleanup.**
- E1. One pass over F-9 (a) through (f). `[local]`.
- E2. State the anti-gaps plainly on the landing page (`03` section 5). `[local]` plus `[owner]` on
  wording.
- Acceptance: no published number disagrees with a measurement, and the site's differentiators
  match the four anti-gaps.

## 7. Open decisions for the owner

**R-006, escaping literal filenames in remembered grants.** `AUDIT.md:21,1673`. The acceptance asks
that an approved grant survive a restart and that approving `foo*` not also approve `foobar`. The
first half cannot be met as written because grants are session-only by design
(`crates/rune-policy/src/approval.rs`), and the second contradicts the documented contract that a
non-command grant may be a pattern (`a_non_command_grant_may_use_a_pattern`). Options: (a) drop the
restart half and fix only the escaping, so a literal filename approves only itself within the session;
(b) keep pattern semantics and close R-006 as won't-fix with the reason recorded; (c) change grants to
persist across restart, which is a larger design change. Recommendation: (a), because it fixes the
narrower real bug without reopening the grant-lifetime design.

**Windows sandbox stance.** `00` cause 4, `03` A2, `04` F-6. Rune refuses commands on a host with no
usable sandbox unless `--allow-unsandboxed`, and its only backends are Linux namespaces and macOS
Seatbelt. Codex ships a native Windows sandbox and Copilot covers Windows 11. Options: (a) implement a
Windows backend (restricted token, job object or AppContainer); (b) refuse with a named reason and
recommend WSL2, which is Claude's documented answer; (c) degrade explicitly with a named reason and
an opt-in. Recommendation: (b) first, because it is honest and cheap, with (a) as a later phase.

**The prompt-floor ceiling and map policy.** `04` F-3. What byte ceiling should CI enforce, and should
the map be capped per crate or allocated round-robin? A tighter ceiling risks dropping useful symbols;
a per-crate cap changes which symbols the model sees. Recommendation: set the ceiling at the landing
value after B1 is measured, and adopt a per-crate cap so the map is representative rather than
alphabetical.

**Should the budget job run on `main` pushes?** `04` F-1, `ci.yml:124`. Today size and startup are
enforced only on pull requests, so a direct push can regress them unmeasured. Recommendation: run it on
both, accepting the cost.

**Which benchmark to enter, and the spend.** `02` section 3. openbench is cheapest and most
reproducible; Claw-SWE-Bench is the strongest SWE-bench-native option but needs Docker, iptables and an
SNI proxy; FrontierHarness needs a matched control. Recommendation: openbench first.

**The macOS `/exit` hang.** `04` F-5. Is it a product bug or a harness artifact? Recommendation:
instrument before committing engineering time, since the local Linux PTY paths pass.

**Is the 5.40 MiB CHANGELOG line a typo or a policy?** `04` F-9a. Recommendation: treat 5.43 as
canonical because it matches the measured binary and the README, and correct the changelog line.
