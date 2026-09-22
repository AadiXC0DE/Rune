//! Capability restriction for command execution.
//!
//! An approved command still gets to write, so the sandbox is what keeps it
//! inside the workspace. Each backend turns a prepared command into the argv
//! that actually runs, and reports whether it can enforce the restriction on
//! this host.
//!
//! The module never degrades quietly. A backend that is absent, or that cannot
//! restrict the process tree it starts, returns an error, and the only way past
//! it is an explicit `allow_unsandboxed` that a caller has to pass deliberately.
//! Running the command as written would be indistinguishable from having no
//! sandbox while looking like one.
//!
//! Backend notes:
//!
//! - macOS uses `sandbox-exec` with a generated Seatbelt profile. Apple
//!   deprecated the tool but has not removed it, and it is the only option for
//!   a non-bundled CLI, so the backend probes it by running it rather than
//!   assuming it works.
//! - Linux builds a mount and network namespace through the `bwrap` helper. The
//!   helper restricts the process tree as a whole, which is the every-thread
//!   guarantee an in-process restriction would have to ask for explicitly.

use std::fmt::Write as _;
use std::process::{Command, Output, Stdio};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::command::PreparedCommand;

/// The flag a caller passes to run a command with no sandbox.
pub const UNSANDBOXED_OVERRIDE: &str = "--allow-unsandboxed";

/// Path of the macOS sandboxing tool, as shipped with the system.
pub const SEATBELT_TOOL: &str = "/usr/bin/sandbox-exec";

/// The Linux helper that builds the namespaces.
pub const NAMESPACE_HELPER: &str = "bwrap";

/// Profile the macOS probe runs: it denies every write and nothing else.
const PROBE_PROFILE: &str = "(version 1)(allow default)(deny file-write*)";

/// Program the macOS probe runs. It only has to start and exit.
const PROBE_PROGRAM: &str = "/usr/bin/true";

/// Program the Linux probe runs inside the namespace.
const PROBE_NAMESPACE_PROGRAM: &str = "/bin/true";

/// What a backend can enforce on this host.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Support {
    /// The backend restricts every process the command starts.
    Full,
    /// The backend runs, but part of the process tree would stay unrestricted.
    /// A caller has to fail closed: a partial restriction is a false promise.
    Partial {
        /// What cannot be restricted.
        reason: String,
    },
    /// The platform does not provide the capability.
    Unsupported {
        /// Why the capability is missing.
        reason: String,
    },
}

impl Support {
    /// Returns true when the backend can enforce the whole restriction.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    /// Returns the reason attached to a status that is not full.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Full => None,
            Self::Partial { reason } | Self::Unsupported { reason } => Some(reason),
        }
    }
}

/// What a command is allowed to reach.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SandboxPolicy {
    /// The workspace, which is writable.
    pub workspace: Utf8PathBuf,
    /// Further directories that are writable.
    pub writable_roots: Vec<Utf8PathBuf>,
    /// Whether the command is granted network access.
    pub network: bool,
}

impl SandboxPolicy {
    /// Builds a policy from a workspace and its extra writable roots.
    #[must_use]
    pub fn new(workspace: Utf8PathBuf, writable_roots: Vec<Utf8PathBuf>, network: bool) -> Self {
        Self {
            workspace,
            writable_roots,
            network,
        }
    }

    /// Returns every writable path, the workspace first.
    fn writable(&self) -> impl Iterator<Item = &Utf8PathBuf> {
        std::iter::once(&self.workspace).chain(self.writable_roots.iter())
    }
}

/// A way to restrict what a command can reach.
///
/// `wrap` returns the command to run instead of the prepared one, or an error
/// when the requested restriction cannot be applied. Returning the command
/// unchanged is never an implicit outcome: it requires `allow_unsandboxed`.
pub trait Sandbox {
    /// Returns the backend name, for diagnostics.
    fn name(&self) -> &'static str;

    /// Reports what this host can enforce.
    fn support(&self) -> Support;

    /// Returns the command that runs under the restriction.
    ///
    /// The returned command keeps the working directory, the environment, and
    /// the reviewed string of the input. Only the argv changes, so a caller can
    /// still verify the route it authorized.
    fn wrap(
        &self,
        prepared: &PreparedCommand,
        policy: &SandboxPolicy,
        allow_unsandboxed: bool,
    ) -> Result<PreparedCommand>;
}

/// Returns the backend for the host.
#[must_use]
pub fn detect() -> Box<dyn Sandbox> {
    if cfg!(target_os = "macos") {
        return Box::new(MacSandbox::detect());
    }
    if cfg!(target_os = "linux") {
        return Box::new(LinuxSandbox::detect());
    }
    Box::new(NullSandbox)
}

/// Builds the error for a restriction that cannot be applied.
fn unavailable(backend: &str, support: &Support) -> RuneError {
    let reason = support.reason().unwrap_or("the backend cannot enforce it");
    let refused = match support {
        Support::Full => "the backend reported full enforcement but produced no command",
        Support::Partial { .. } => "the backend cannot restrict every process the command starts",
        Support::Unsupported { .. } => "no usable sandbox backend is on this host",
    };
    RuneError::new(
        ErrorCode::Unsupported,
        format!("{backend} refused to run the command: {refused}: {reason}"),
    )
    .with_hint(format!(
        "pass {UNSANDBOXED_OVERRIDE} to run the command with no sandbox"
    ))
}

/// Returns the error for a path the backend cannot express a rule about.
fn unresolved(path: &Utf8Path, what: &str) -> RuneError {
    RuneError::new(
        ErrorCode::NotFound,
        format!("{what} `{path}` does not exist or cannot be resolved"),
    )
    .with_hint("the restriction is built from resolved paths, so the path has to exist")
}

/// Resolves a path to the location the kernel sees.
///
/// On macOS a rule for `/tmp/x` does not cover `/private/tmp/x`, so an
/// unresolved path produces a profile that denies the very directory it meant to
/// allow.
fn resolve(path: &Utf8Path, what: &str) -> Result<Utf8PathBuf> {
    path.canonicalize_utf8().map_err(|_| unresolved(path, what))
}

/// Quotes a path for a Seatbelt profile.
fn quote(path: &Utf8Path) -> Result<String> {
    for bad in ['"', '\\', '\n'] {
        if path.as_str().contains(bad) {
            return Err(RuneError::invalid_field(
                "path",
                format!("`{path}` holds a character a sandbox profile cannot carry"),
            )
            .with_hint("use a workspace path without a quote, a backslash, or a newline"));
        }
    }
    Ok(format!("\"{}\"", path.as_str()))
}

/// Returns the first line of a probe's diagnostic output.
fn observed(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no diagnostic was written");
    format!("{}: {line}", output.status)
}

/// A backend for a platform that has neither capability.
#[derive(Clone, Copy, Debug)]
pub struct NullSandbox;

impl Sandbox for NullSandbox {
    fn name(&self) -> &'static str {
        "null"
    }

    fn support(&self) -> Support {
        Support::Unsupported {
            reason: format!("{} has no supported sandbox backend", std::env::consts::OS),
        }
    }

    fn wrap(
        &self,
        prepared: &PreparedCommand,
        _policy: &SandboxPolicy,
        allow_unsandboxed: bool,
    ) -> Result<PreparedCommand> {
        if allow_unsandboxed {
            return Ok(prepared.clone());
        }
        Err(unavailable(self.name(), &self.support()))
    }
}

/// The macOS Seatbelt backend.
#[derive(Clone, Debug)]
pub struct MacSandbox {
    support: Support,
    tool: Utf8PathBuf,
}

impl MacSandbox {
    /// Probes the host by running the tool against a deny-every-write profile.
    ///
    /// The tool is deprecated by Apple, so its presence proves nothing: only an
    /// executed probe says whether this host still enforces the profile.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            support: probe_seatbelt(),
            tool: Utf8PathBuf::from(SEATBELT_TOOL),
        }
    }

    /// Builds a backend with a known probe result.
    #[must_use]
    pub fn with_support(support: Support) -> Self {
        Self {
            support,
            tool: Utf8PathBuf::from(SEATBELT_TOOL),
        }
    }

    /// Returns the generated profile for a policy.
    ///
    /// The profile denies every write and then re-allows the workspace and each
    /// writable root, so an action is granted deliberately rather than being
    /// permitted because no rule mentioned it. Reading stays open, and network
    /// is denied unless the policy grants it.
    pub fn profile(policy: &SandboxPolicy) -> Result<String> {
        let mut profile = String::new();
        let _ = writeln!(profile, "(version 1)");
        let _ = writeln!(profile, "(allow default)");
        let _ = writeln!(profile, "(deny file-write*)");
        for path in policy.writable() {
            let resolved = resolve(path, "a writable path")?;
            let rule = if resolved.is_dir() {
                "subpath"
            } else {
                // A file grant covers the file, where a subpath rule would also
                // cover a directory that happened to share its name.
                "literal"
            };
            let _ = writeln!(
                profile,
                "(allow file-write* ({rule} {}))",
                quote(&resolved)?
            );
        }
        // Redirection to these is not a way to change the machine, and denying
        // them breaks almost every command that prints.
        for device in ["/dev/null", "/dev/stdout", "/dev/stderr", "/dev/tty"] {
            let _ = writeln!(profile, "(allow file-write* (literal \"{device}\"))");
        }
        if !policy.network {
            let _ = writeln!(profile, "(deny network*)");
        }
        Ok(profile)
    }
}

impl Sandbox for MacSandbox {
    fn name(&self) -> &'static str {
        "seatbelt"
    }

    fn support(&self) -> Support {
        self.support.clone()
    }

    fn wrap(
        &self,
        prepared: &PreparedCommand,
        policy: &SandboxPolicy,
        allow_unsandboxed: bool,
    ) -> Result<PreparedCommand> {
        if !self.support.is_full() {
            if allow_unsandboxed {
                return Ok(prepared.clone());
            }
            return Err(unavailable(self.name(), &self.support));
        }
        let profile = Self::profile(policy)?;
        let mut argv = Vec::with_capacity(prepared.argv.len().saturating_add(3));
        argv.push(self.tool.as_str().to_owned());
        argv.push(String::from("-p"));
        argv.push(profile);
        argv.extend(prepared.argv.iter().cloned());
        Ok(PreparedCommand {
            argv,
            ..prepared.clone()
        })
    }
}

/// Runs the Seatbelt tool once to see whether this host still enforces it.
fn probe_seatbelt() -> Support {
    let tool = Utf8Path::new(SEATBELT_TOOL);
    if !tool.is_file() {
        return Support::Unsupported {
            reason: format!(
                "{SEATBELT_TOOL} is not installed on {}",
                std::env::consts::OS
            ),
        };
    }
    match Command::new(SEATBELT_TOOL)
        .args(["-p", PROBE_PROFILE, PROBE_PROGRAM])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => Support::Full,
        Ok(output) => Support::Unsupported {
            reason: format!(
                "`{SEATBELT_TOOL}` did not enforce a deny-every-write profile: {}",
                observed(&output)
            ),
        },
        Err(err) => Support::Unsupported {
            reason: format!("`{SEATBELT_TOOL}` could not be run: {err}"),
        },
    }
}

/// The Linux namespace backend.
#[derive(Clone, Debug)]
pub struct LinuxSandbox {
    support: Support,
    helper: Option<Utf8PathBuf>,
}

impl LinuxSandbox {
    /// Probes the host for the namespace helper.
    #[must_use]
    pub fn detect() -> Self {
        let helper = find_on_path(NAMESPACE_HELPER);
        let support = probe_namespaces(helper.as_deref());
        Self { support, helper }
    }

    /// Builds a backend with a known probe result.
    #[must_use]
    pub fn with_support(support: Support) -> Self {
        Self {
            support,
            helper: None,
        }
    }

    /// Builds a backend that reports full support and runs the given helper.
    ///
    /// Used by tests to exercise the argv a full-enforcement host would produce
    /// without depending on the helper being installed, since the wrapping is
    /// what is under test rather than whether this machine has the helper.
    #[must_use]
    pub fn with_helper(helper: Utf8PathBuf) -> Self {
        Self {
            support: Support::Full,
            helper: Some(helper),
        }
    }

    /// Returns the argv for a policy.
    fn argv(
        helper: &Utf8Path,
        prepared: &PreparedCommand,
        policy: &SandboxPolicy,
    ) -> Result<Vec<String>> {
        let mut argv = vec![
            helper.as_str().to_owned(),
            // The namespaces are torn down with the process, so a command that
            // is killed leaves no restricted state behind.
            String::from("--die-with-parent"),
            // A new session detaches the command from the caller's terminal, so
            // it cannot drive it.
            String::from("--new-session"),
            String::from("--unshare-pid"),
        ];
        if !policy.network {
            argv.push(String::from("--unshare-net"));
        }
        argv.extend([
            String::from("--ro-bind"),
            String::from("/"),
            String::from("/"),
            String::from("--dev-bind"),
            String::from("/dev"),
            String::from("/dev"),
            String::from("--proc"),
            String::from("/proc"),
            // The host's temporary directory is deliberately not carried over,
            // so a temporary file is written somewhere the command cannot
            // inspect later.
            String::from("--tmpfs"),
            String::from("/tmp"),
        ]);
        for path in policy.writable() {
            let resolved = resolve(path, "a writable path")?;
            argv.push(String::from("--bind"));
            argv.push(resolved.as_str().to_owned());
            argv.push(resolved.as_str().to_owned());
        }
        argv.push(String::from("--"));
        argv.extend(prepared.argv.iter().cloned());
        Ok(argv)
    }
}

impl Sandbox for LinuxSandbox {
    fn name(&self) -> &'static str {
        "namespaces"
    }

    fn support(&self) -> Support {
        self.support.clone()
    }

    fn wrap(
        &self,
        prepared: &PreparedCommand,
        policy: &SandboxPolicy,
        allow_unsandboxed: bool,
    ) -> Result<PreparedCommand> {
        if !self.support.is_full() {
            if allow_unsandboxed {
                return Ok(prepared.clone());
            }
            return Err(unavailable(self.name(), &self.support));
        }
        let Some(helper) = self
            .helper
            .clone()
            .or_else(|| find_on_path(NAMESPACE_HELPER))
        else {
            return Err(unavailable(self.name(), &self.support));
        };
        Ok(PreparedCommand {
            argv: Self::argv(&helper, prepared, policy)?,
            ..prepared.clone()
        })
    }
}

/// Looks a program up on `PATH`.
fn find_on_path(program: &str) -> Option<Utf8PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = Utf8PathBuf::from_path_buf(dir.join(program)).ok()?;
        candidate.is_file().then_some(candidate)
    })
}

/// Reports what the namespace helper can do on this host.
///
/// A helper that is installed but cannot build a namespace has to be told apart
/// from an absent one: the command would still run, with whatever restriction
/// the host happened to allow.
fn probe_namespaces(helper: Option<&Utf8Path>) -> Support {
    let Some(helper) = helper else {
        return Support::Unsupported {
            reason: format!(
                "the `{NAMESPACE_HELPER}` helper is not on PATH on {}",
                std::env::consts::OS
            ),
        };
    };
    match Command::new(helper.as_str())
        .args([
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            PROBE_NAMESPACE_PROGRAM,
        ])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => Support::Full,
        Ok(output) => Support::Partial {
            reason: format!(
                "`{helper}` could not build a namespace: {}",
                observed(&output)
            ),
        },
        Err(err) => Support::Unsupported {
            reason: format!("`{helper}` could not be run: {err}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use tempfile::TempDir;

    #[cfg(target_os = "macos")]
    use std::time::Duration;

    #[cfg(target_os = "macos")]
    use crate::command::run;
    use crate::command::{prepare, verify_unchanged};

    /// Returns a condition that never cancels.
    #[cfg(target_os = "macos")]
    fn never() -> bool {
        false
    }

    /// Returns a temporary workspace and the guard that owns it.
    fn tempdir() -> (TempDir, Utf8PathBuf) {
        let dir = TempDir::new().expect("temporary directory");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("a UTF-8 path");
        (dir, path)
    }

    /// Returns the parent `PATH`, the only variable the tests pass on.
    fn environment() -> BTreeMap<String, String> {
        let mut environment = BTreeMap::new();
        let path = std::env::var("PATH").unwrap_or_else(|_| String::from("/usr/bin:/bin"));
        environment.insert(String::from("PATH"), path);
        environment
    }

    /// Returns a policy over one workspace.
    fn policy(workspace: &Utf8Path) -> SandboxPolicy {
        SandboxPolicy::new(workspace.to_owned(), Vec::new(), false)
    }

    #[test]
    fn only_full_support_is_full() {
        assert!(Support::Full.is_full());
        assert!(
            !Support::Partial {
                reason: String::from("half")
            }
            .is_full()
        );
        assert!(
            !Support::Unsupported {
                reason: String::from("none")
            }
            .is_full()
        );
        assert_eq!(
            Support::Partial {
                reason: String::from("half")
            }
            .reason(),
            Some("half")
        );
        assert_eq!(Support::Full.reason(), None);
    }

    #[test]
    fn a_full_backend_produces_the_wrapped_argv() {
        let (_dir, dir) = tempdir();
        let sandbox = MacSandbox::with_support(Support::Full);
        let prepared = prepare("echo hello", dir.as_path(), None, environment()).expect("prepare");
        let wrapped = sandbox
            .wrap(&prepared, &policy(dir.as_path()), false)
            .expect("wrap");
        assert_eq!(wrapped.argv[0], SEATBELT_TOOL);
        assert_eq!(wrapped.argv[1], "-p");
        assert!(wrapped.argv[2].contains("(deny file-write*)"));
        assert!(wrapped.argv[2].contains("(deny network*)"));
        assert_eq!(&wrapped.argv[3..], prepared.argv.as_slice());
        assert_eq!(wrapped.reviewed, prepared.reviewed);
        assert_eq!(wrapped.cwd, prepared.cwd);
        assert_eq!(wrapped.environment, prepared.environment);
        assert!(verify_unchanged(&prepared).is_ok());
    }

    #[test]
    fn a_profile_grants_writes_under_each_writable_path_and_nowhere_else() {
        let (_dir, dir) = tempdir();
        let extra = dir.join("extra");
        std::fs::create_dir(&extra).expect("extra root");
        let workspace = dir.as_path();
        let policy = SandboxPolicy::new(workspace.to_owned(), vec![extra.clone()], true);
        let profile = MacSandbox::profile(&policy).expect("profile");
        let resolved = std::fs::canonicalize(workspace)
            .expect("canonical workspace")
            .to_string_lossy()
            .into_owned();
        let resolved_extra = std::fs::canonicalize(&extra)
            .expect("canonical root")
            .to_string_lossy()
            .into_owned();
        assert!(
            profile.contains(&format!("(allow file-write* (subpath \"{resolved}\"))")),
            "{profile}"
        );
        assert!(
            profile.contains(&format!(
                "(allow file-write* (subpath \"{resolved_extra}\"))"
            )),
            "{profile}"
        );
        assert!(!profile.contains("(deny network*)"), "{profile}");
    }

    #[test]
    fn a_backend_without_full_support_fails_closed() {
        let (_dir, dir) = tempdir();
        let prepared = prepare("echo hello", dir.as_path(), None, environment()).expect("prepare");
        let backends: Vec<(&str, Box<dyn Sandbox>)> = vec![
            (
                "seatbelt-partial",
                Box::new(MacSandbox::with_support(Support::Partial {
                    reason: String::from("the host refuses the profile"),
                })),
            ),
            (
                "namespaces-unsupported",
                Box::new(LinuxSandbox::with_support(Support::Unsupported {
                    reason: String::from("no helper"),
                })),
            ),
            ("null", Box::new(NullSandbox)),
        ];
        for (name, sandbox) in backends {
            let policy = policy(dir.as_path());
            let error = sandbox
                .wrap(&prepared, &policy, false)
                .expect_err("a backend that cannot enforce");
            assert_eq!(error.code(), ErrorCode::Unsupported, "{name}: {error}");
            assert!(
                error.to_string().contains(UNSANDBOXED_OVERRIDE),
                "the refusal does not name the override: {error}"
            );
            assert_eq!(
                sandbox
                    .wrap(&prepared, &policy, true)
                    .expect("the override"),
                prepared,
                "{name} changed the command under the override"
            );
        }
    }

    #[test]
    fn a_full_backend_refuses_a_policy_it_cannot_express() {
        let (_dir, dir) = tempdir();
        let prepared = prepare("echo hello", dir.as_path(), None, environment()).expect("prepare");
        let missing = SandboxPolicy::new(Utf8PathBuf::from("/rune-exec/absent"), Vec::new(), false);
        let error = MacSandbox::with_support(Support::Full)
            .wrap(&prepared, &missing, false)
            .expect_err("an unresolvable workspace");
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert!(error.to_string().contains("rune-exec/absent"), "{error}");
    }

    #[test]
    fn the_null_backend_refuses_and_names_the_override() {
        let (_dir, dir) = tempdir();
        let prepared = prepare("echo hello", dir.as_path(), None, environment()).expect("prepare");
        let error = NullSandbox
            .wrap(&prepared, &policy(dir.as_path()), false)
            .expect_err("no backend");
        assert_eq!(error.code(), ErrorCode::Unsupported);
        assert!(error.to_string().contains(UNSANDBOXED_OVERRIDE), "{error}");
        assert!(error.to_string().contains(std::env::consts::OS), "{error}");
        assert_eq!(
            NullSandbox
                .wrap(&prepared, &policy(dir.as_path()), true)
                .expect("override"),
            prepared
        );
    }

    #[test]
    fn the_linux_argv_keeps_the_route_and_carries_the_restriction() {
        let (_dir, dir) = tempdir();
        let helper = Utf8PathBuf::from("/usr/bin/bwrap");
        let sandbox = LinuxSandbox::with_helper(helper.clone());
        let policy = SandboxPolicy::new(dir.as_path().to_owned(), Vec::new(), false);
        let prepared =
            prepare("echo hello | cat", dir.as_path(), None, environment()).expect("prepare");
        let wrapped = sandbox.wrap(&prepared, &policy, false).expect("wrap");
        assert_eq!(wrapped.argv[0], helper.as_str());
        assert!(wrapped.argv.contains(&String::from("--die-with-parent")));
        assert!(wrapped.argv.contains(&String::from("--unshare-pid")));
        assert!(
            wrapped.argv.contains(&String::from("--unshare-net")),
            "network was not restricted: {:?}",
            wrapped.argv
        );
        let separator = wrapped
            .argv
            .iter()
            .position(|argument| argument == "--")
            .expect("a command separator");
        assert_eq!(
            &wrapped.argv[separator.saturating_add(1)..],
            prepared.argv.as_slice(),
            "the wrapped argv changed the command it wraps"
        );
        let bind = wrapped
            .argv
            .iter()
            .position(|argument| argument == "--bind")
            .expect("a writable bind");
        assert_eq!(
            wrapped.argv[bind.saturating_add(1)],
            wrapped.argv[bind.saturating_add(2)],
            "the writable bind is not a pair"
        );
        assert!(wrapped.argv.iter().any(|argument| argument == "/bin/sh"));
        assert!(verify_unchanged(&prepared).is_ok());
    }

    #[test]
    fn the_linux_argv_grants_network_only_when_the_policy_does() {
        let (_dir, dir) = tempdir();
        let sandbox = LinuxSandbox::with_helper(Utf8PathBuf::from("/usr/bin/bwrap"));
        let prepared = prepare("echo hello", dir.as_path(), None, environment()).expect("prepare");
        let granted = SandboxPolicy::new(dir.as_path().to_owned(), Vec::new(), true);
        let wrapped = sandbox.wrap(&prepared, &granted, false).expect("wrap");
        assert!(!wrapped.argv.contains(&String::from("--unshare-net")));
    }

    #[test]
    fn a_full_backend_without_a_helper_refuses() {
        let (_dir, dir) = tempdir();
        let sandbox = LinuxSandbox::with_support(Support::Full);
        let prepared = prepare("echo hello", dir.as_path(), None, environment()).expect("prepare");
        // The helper is looked up on PATH, which the parent process here has
        // without the helper, so this either refuses or wraps. It must never
        // return the command as written.
        match sandbox.wrap(&prepared, &policy(dir.as_path()), false) {
            Ok(wrapped) => assert_ne!(wrapped, prepared, "an unstripped command was returned"),
            Err(error) => assert_eq!(error.code(), ErrorCode::Unsupported),
        }
    }

    #[test]
    fn an_absent_namespace_helper_is_reported_by_name() {
        let support = probe_namespaces(None);
        assert!(!support.is_full());
        assert!(
            support
                .reason()
                .is_some_and(|reason| reason.contains(NAMESPACE_HELPER)),
            "{support:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_detected_support_matches_what_the_host_does() {
        let sandbox = MacSandbox::detect();
        let (_dir, dir) = tempdir();
        let prepared =
            prepare("echo sandboxed", dir.as_path(), None, environment()).expect("prepare");
        match sandbox.support() {
            Support::Full => {
                let wrapped = sandbox
                    .wrap(&prepared, &policy(dir.as_path()), false)
                    .expect("a full backend wraps");
                let outcome = run(&wrapped, Duration::from_secs(20), &never).expect("run");
                assert_eq!(outcome.exit, crate::command::Exit::Code(0), "{outcome:?}");
                assert!(outcome.stdout.contains("sandboxed"), "{outcome:?}");
            }
            other => assert!(
                other.reason().is_some_and(|reason| !reason.is_empty()),
                "a non-full result has to say what was observed: {other:?}"
            ),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_full_seatbelt_backend_denies_a_write_outside_the_workspace() {
        let sandbox = MacSandbox::detect();
        if !sandbox.support().is_full() {
            return;
        }
        let (_dir, dir) = tempdir();
        let (_outside, outside) = tempdir();
        let outside_file = outside.join("escaped.txt");
        let command = format!("/bin/sh -c 'echo escaped > {outside_file}'");
        let prepared = prepare(&command, dir.as_path(), None, environment()).expect("prepare");
        let wrapped = sandbox
            .wrap(&prepared, &policy(dir.as_path()), false)
            .expect("wrap");
        let outcome = run(&wrapped, Duration::from_secs(20), &never).expect("run");
        assert!(
            !outcome.exit.is_success(),
            "a write outside the workspace succeeded: {outcome:?}"
        );
        assert!(
            !outside_file.exists(),
            "the sandboxed command wrote outside the workspace"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_sandboxed_command_can_still_write_inside_the_workspace() {
        // Without this, a backend that denied every write would pass the test
        // above. The pair is what shows the restriction is scoped rather than
        // blanket.
        let sandbox = MacSandbox::detect();
        if !sandbox.support().is_full() {
            return;
        }
        let (_dir, dir) = tempdir();
        let inside = dir.join("written.txt");
        let command = format!("/bin/sh -c 'echo kept > {inside}'");
        let prepared = prepare(&command, dir.as_path(), None, environment()).expect("prepare");
        let wrapped = sandbox
            .wrap(&prepared, &policy(dir.as_path()), false)
            .expect("wrap");
        let outcome = run(&wrapped, Duration::from_secs(20), &never).expect("run");
        assert!(
            outcome.exit.is_success(),
            "a write inside the workspace was refused: {outcome:?}"
        );
        assert!(
            inside.exists(),
            "the sandboxed command did not write inside the workspace"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_sandboxed_command_still_reads_a_path_it_may_read() {
        let sandbox = MacSandbox::detect();
        if !sandbox.support().is_full() {
            return;
        }
        let (_dir, dir) = tempdir();
        let readable = dir.join("input.txt");
        std::fs::write(&readable, "contents").expect("write");
        let command = format!("/bin/cat {readable}");
        let prepared = prepare(&command, dir.as_path(), None, environment()).expect("prepare");
        let wrapped = sandbox
            .wrap(&prepared, &policy(dir.as_path()), false)
            .expect("wrap");
        let outcome = run(&wrapped, Duration::from_secs(20), &never).expect("run");
        assert!(
            outcome.exit.is_success(),
            "reading a permitted path was refused: {outcome:?}"
        );
        assert!(
            outcome.stdout.contains("contents"),
            "the read produced nothing: {outcome:?}"
        );
    }
}
