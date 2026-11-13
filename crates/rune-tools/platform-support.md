# Platform capability audit

Scope: production platform branches and capability-dependent I/O in `rune-tools`,
including the `rune-exec` services its tools call. Test-only `cfg` branches are
not product capabilities. This review was performed on Linux; native macOS and
Windows execution is not established by this host's checks.

## PTY conclusion and fix

The existing off-Linux test is correct. This is a Linux-only implementation,
not a BSD PTY implementation with a macOS allocation defect:

- `shell.rs`, `Shell::input_schema`: `pty` is described as "Linux only";
  `Shell::description` also names Linux.
- `../../CHANGELOG.md`: "Linux shell calls can set `pty: true`".
- `../rune-exec/src/lib.rs`: the entire `pty` module is compiled only on Linux.
- `../rune-exec/src/pty.rs`, `configure`: allocation uses `openpt`, `grantpt`,
  `unlockpt`, and Linux `ioctl_tiocgptpeer`; child setup uses `setsid` and
  `ioctl_tiocsctty`. No `forkpty` or BSD allocation path exists.
- `../rune-exec/src/session.rs`, `Input::Pty`: documented as a Linux PTY.
  `Process::start_argv` explicitly returns `io::ErrorKind::Unsupported` on every
  other OS before constructing or spawning a command.

Previously, an unconfined macOS `run` selected `Input::Pty`, reserved a slot,
prepared shell argv, bypassed sandbox wrapping, and canonicalized its working
directory. The process layer then refused the input mode. `shell.rs`, `start`,
converted this through `RuneError::from(io::Error)`; the wildcard in
`../rune-core/src/error.rs` classified `Unsupported` as `IoFailure`. The shell
also attached a working-directory hint. No PTY syscall or user command ran.
The test fixture explicitly uses an unconfined context, explaining the observed
failure. A confined macOS request instead reached the default
`Sandbox::wrap_pty`, which already returned `ErrorCode::Unsupported`.

The product is fixed: `Shell::run` now refuses off-Linux PTY mode before reserving
a slot, preparing argv, or detecting a sandbox, with `Unsupported` and a hint to
omit `pty` and use `interactive: true` for piped input. A controlling terminal
still requires Linux. The existing test keeps its expectation and additionally
checks all sandbox/fallback combinations, the hint, and absence of command
side effects, sessions, and reserved slots. The README explains the alternative;
the schema retains its existing Linux-only contract. The README's Unix PTY
replay gate is a test harness for Rune's UI; it does not promise shell-tool PTY
support on macOS.

## Remaining capabilities

| Capability | Attempt, refusal, and evidence | Disposition |
| --- | --- | --- |
| PTY resize | `shell.rs`, `Shell::interact`, calls `Process::resize`. In `../rune-exec/src/session.rs`, only a live Linux terminal reaches `pty::resize`; pipes and other platforms return `Unsupported` with "resizing requires a Linux PTY session". The tool reports `InvalidState` and retains that explanation, since the selected session cannot resize. | Explicit refusal; no terminal I/O is attempted for pipe sessions. |
| Piped and closed shell input | `shell.rs`, `Shell::run`, selects `Input::Piped` for `interactive` and `Input::Closed` by default. `../rune-exec/src/session.rs` uses standard pipes/null input on all native platforms. `../rune-exec/src/command.rs`, `prepare_shell`, selects `/bin/sh -c` on Unix and `cmd.exe /C` on Windows. | Supported native paths are attempted. A missing shell, invalid working directory, or spawn failure is a runtime failure, not evidence that an OS is unsupported. Platforms without native processes have no explicit tool-level refusal; process/thread failures remain a limitation. |
| Sandbox and network isolation, including offline shell policy | `shell.rs`, `sandbox_policy` and `confined`, remove the network grant offline. `../rune-exec/src/sandbox.rs`, `detect`, selects macOS Seatbelt, Linux bubblewrap, or `NullSandbox`. The native helpers are probed; `unavailable` returns `Unsupported` plus `--allow-unsandboxed` guidance for missing/partial support. `NullSandbox::wrap` refuses unless fallback is explicitly enabled. Linux overrides `wrap_pty`; the default method refuses PTYs. | Explicit refusal before the user command. Helper probes themselves execute processes to establish support. An opted-in fallback or unconfined context supplies no sandbox/network isolation. A helper that passes its probe can still fail later on a particular invocation; that is an actual execution failure. |
| Atomic no-replace file rename | `rename_file.rs`, `rename_no_replace`, uses rustix `renameat_with(NOREPLACE)` on Linux/Android/Apple targets and `rune_exec::rename_no_replace` (`MoveFileExW` with zero flags) on Windows. Other targets return an `Unsupported` I/O error without a filesystem mutation. Supported OSes can also fail at the syscall because of filesystem/kernel capabilities, cross-device paths, or races. | Fixed the tool mapping: an `Unsupported` I/O error previously became `UnsafePath` with an irrelevant directory hint; it now becomes `Unsupported` with platform/filesystem guidance. Existing-destination and other path errors remain distinct. Metadata/path validation still precedes capability refusal. No unsafe copy/delete fallback is added. |
| Child resource limits | `../rune-exec/src/command.rs`, `apply_child_setup`, `ResourceLimits::apply`, and `apply_limits`: Unix children apply CPU, process-count, and file-size limits before exec; Linux alone also applies a requested address-space limit. All defaults leave address space unbounded. Other Unix targets omit address-space limits; values are clamped to the system hard maximum. On non-Unix, applying resource limits is a no-op. | Existing limitation: there is no refusal or tool-level disclosure for absent non-Unix limits. Unix setup errors fail spawn, so an I/O failure there is honest; a command is not executed after failed setup. The `syntax_diagnostics` description's unqualified resource-limit claim is broader than Windows enforcement. No new enforcement mechanism is introduced in this fix. |
| Process-group/tree termination | `shell.rs`, `ended_by`, distinguishes Unix groups and other process trees. `../rune-exec/src/session.rs`, `signal_group`, invokes `/bin/kill` or `kill` on Unix and `taskkill /PID ... /T /F` off Unix. `terminate`, `end_group`, and `Inner::shutdown` attempt termination and can fall back to killing the direct child. | Existing late-failure limitation: helper absence/failure is not rejected at session creation; descendants may survive fallback. Stop reports a failed result if the leader remains alive, but leader exit does not prove every descendant died. Requires native Windows tests for `taskkill`; not claimed as full tree verification here. |
| Canonical paths / symlink containment | `workspace.rs`, `real_path`, skips canonicalization on WASI and uses the lexical path if canonicalization returns `Unsupported`. Elsewhere it resolves existing ancestors and rejects unresolved links. | Existing limitation: no capability refusal when resolution is unavailable; correctness then relies on the filesystem's path/link model. This review does not establish that every filesystem returning `Unsupported` lacks links. |
| Hard-link refusal in text mutations | `mutation.rs`, `read_preimage` and `is_multilinked`, reject Unix files with `nlink > 1` as `UnsafePath`. Off Unix, `is_multilinked` always returns false. | Existing limitation: Windows hard-linked files are attempted without this refusal, so replacing one name can leave another name with old content. No claim of platform parity. |
| File identity for stale-write detection | `mutation.rs`, `FileIdentity::of`, hashes a `same_file::Handle` (device/inode or volume/file index). A failure to obtain that handle silently leaves a constant identity digest; preimage content hashing remains active. | Existing fallback, not an explicit refusal. An identical-content replacement can escape identity detection when the handle is unavailable. |
| Mutation permissions and durability | `mutation.rs`, `apply`, tries to preserve permissions but intentionally ignores `set_permissions` returning `Unsupported`; other errors fail before rename. File data is synced before rename. `sync_directory` is best effort on Unix and a no-op elsewhere after the rename has committed. | Existing weaker guarantees where these capabilities are absent, without a capability refusal. A post-commit directory-sync failure is not reported as a failed edit. |
| WASM staging-file names | `mutation.rs`, `process_id`, uses zero on WASM rather than calling the unsupported process-ID API. A clock and monotonic counter still distinguish staging names; creation uses `create_new`. | Intentional alternative implementation, no process-ID I/O attempt or refusal. |
| Rust syntax diagnostics | `syntax_diagnostics.rs`, `EmptyConfig::create`, sets mode `0600` only on Unix; other targets use default file permissions. `checker_environment` uses `USERPROFILE` on Windows or `HOME` elsewhere for rustup discovery. `inspect` attempts local `rustfmt` and reports startup failure as `TransportFailure` with install/PATH guidance. | Native helper requirement is explicit. No blanket macOS/Windows refusal is warranted. Unsupported process hosts fail at runtime as described above. The empty temporary config contains no source or secrets. |
| Git inspection / platform null device | `git_diff.rs`, `null_device`, selects Windows `NUL` or Unix `/dev/null` for global Git config isolation. `inspect` starts Git through `Process::start_argv`, reporting startup failures as `TransportFailure` with install/workspace guidance. | Native paths are intentional; missing Git is an honest helper failure. No blanket macOS/Windows refusal is warranted. |
| Home-directory expansion | `workspace.rs`, `home`, checks `HOME` and `USERPROFILE`. If neither is set, `~` expansion returns `Unsupported` with an explanation. | Explicit refusal; callers can use a workspace-relative or absolute path. |

No other production OS-specific branches were found in this crate. The other
`Unsupported` results in web, vision, and catalog-backed tools describe protocol,
format, or configured-backend availability rather than an operating-system
capability.

## Verification boundary

Linux checks exercise supported PTY allocation/input/resize/termination, output
retention, native filesystem operations, and the new rename error-mapping test.
The strengthened off-Linux PTY test still needs a native macOS or Windows runner;
it is not compiled into the Linux test binary. Native macOS CI must confirm the
original failure now passes and the complete platform suite remains green.
There is no claim that BSD PTYs cannot work in principle: adding them would
require a new allocator plus terminal, I/O, resize, cancellation, and sandbox
integration tests on a Mac.
