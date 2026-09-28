//! Capability restriction for command execution.
//!
//! An approved command still gets to write, so the sandbox is what keeps it
//! inside the workspace, away from the credential locations under the home
//! directory, and out of the repository files git later runs unsandboxed. Each backend turns a prepared command into the argv that actually
//! runs, and reports whether it can enforce the restriction on this host.
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
//!   assuming it works. The profile denies whole categories of operation and
//!   then grants the paths a command legitimately needs, so a rule for a
//!   credential path has to be narrowed by a later grant rather than by a
//!   narrower deny, because the last rule that matches decides.
//! - Linux builds a mount and network namespace through the `bwrap` helper. The
//!   helper restricts the process tree as a whole, which is the every-thread
//!   guarantee an in-process restriction would have to ask for explicitly.

use std::fmt::Write as _;
use std::process::{Command, Output, Stdio};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;

use crate::command::PreparedCommand;

/// The flag a caller passes to run a command with no sandbox.
pub const UNSANDBOXED_OVERRIDE: &str = "--allow-unsandboxed";

/// Path of the macOS sandboxing tool, as shipped with the system.
pub const SEATBELT_TOOL: &str = "/usr/bin/sandbox-exec";

/// The Linux helper that builds the namespaces.
pub const NAMESPACE_HELPER: &str = "bwrap";

/// Device a masked credential file is replaced with.
///
/// A file is masked by covering it with a node of the same kind, and the null
/// device is always present, always readable to the end of nothing, and never
/// carries anything.
const NULL_DEVICE: &str = "/dev/null";

/// Locations under the home directory that hold credentials.
///
/// A sandboxed command has no business reading any of these, so each backend
/// hides them from the command it starts. Rune's own configuration and state
/// directories are named at their default locations, and the locations this
/// process actually resolves are added to them, so a variable that moves the
/// credential file widens what is hidden without ever narrowing it.
///
/// The entries are relative: the home directory is resolved once and each is
/// joined onto it, and an entry that is not present is skipped, since a rule
/// about a path that does not exist is noise in a Seatbelt profile and an error
/// in a `bwrap` mount.
pub const CREDENTIAL_PATHS: [&str; 12] = [
    ".ssh",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".config/gh",
    ".git-credentials",
    ".netrc",
    ".gnupg",
    ".docker/config.json",
    ".kube/config",
    ".config/rune",
    ".local/state/rune",
];

/// Paths inside a writable root that stay read-only.
///
/// Git runs a repository's hooks and configured helpers outside any sandbox: a
/// hook on the next commit, a diff driver or file system monitor on the next
/// `git status`. A command able to write either would reach past the sandbox
/// the next time anyone used git in that repository.
pub const PROTECTED_REPOSITORY_PATHS: [&str; 2] = [".git/config", ".git/hooks"];

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

    /// Returns the credential locations a command under this policy has
    /// hidden, for a caller to assert against without repeating the list.
    pub fn credential_paths(&self) -> Result<Vec<Utf8PathBuf>> {
        let writable = self
            .writable()
            .map(|path| resolve(path, "a writable path"))
            .collect::<Result<Vec<_>>>()?;
        Ok(masked_paths(&existing_credential_paths(), &writable))
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

/// Returns the home directory, resolved once.
///
/// A sandbox rule is a fact about this host, so the location is resolved at the
/// point the profile is built rather than being carried on the policy. A policy
/// that named its own home would let a caller move what is hidden.
fn home_directory() -> Option<Utf8PathBuf> {
    let home = std::env::var("HOME")
        .ok()
        .filter(|value| !value.is_empty())?;
    let path = Utf8PathBuf::from(home);
    path.is_dir().then_some(path)
}

/// Returns the credential paths that exist under a home directory.
///
/// Each is resolved to the location the kernel sees, so a rule about it covers
/// the same bytes a command would open. That resolution is what makes the rule
/// match: a sandbox rule names a resolved path, and a rule about the spelling a
/// path was built from matches nothing. A path that is not present is skipped,
/// since it holds nothing to protect, and a rule about it is either dead weight
/// or, on Linux, a mount that cannot be created.
fn credential_paths_under(home: &Utf8Path) -> Vec<Utf8PathBuf> {
    CREDENTIAL_PATHS
        .iter()
        .map(|relative| home.join(relative))
        .filter(|path| path.exists())
        .filter_map(|path| path.canonicalize_utf8().ok())
        .collect()
}

/// Returns the credential paths that exist on this host.
///
/// The fixed locations are joined to the home directory. Rune's own roots are
/// added where this process resolves them, because `RUNE_HOME` and the XDG
/// variables move the credential file, and a mask that stayed at the default
/// would leave the moved file readable. The credential file is named on its own
/// as well, so it stays hidden even when its directory cannot be.
fn existing_credential_paths() -> Vec<Utf8PathBuf> {
    let mut paths = home_directory().map_or_else(Vec::new, |home| credential_paths_under(&home));
    let own = Paths::from_process();
    let credentials_file = own.credentials_file();
    for path in [own.config_root, own.state_root, credentials_file] {
        // A root built from an unset home is relative, and would resolve
        // against whatever directory this process happens to be in.
        if !path.is_absolute() {
            continue;
        }
        if let Ok(resolved) = path.canonicalize_utf8()
            && !paths.contains(&resolved)
        {
            paths.push(resolved);
        }
    }
    paths
}

/// Returns the credential paths a command under a policy has hidden.
///
/// A location that contains a writable root is left out, because covering it
/// would hide the directory the command was started to work in; a narrower
/// entry for the same secret still applies. A location inside another masked
/// directory is left out too, since the directory already hides it and a
/// second mount inside an emptied directory has nothing to cover.
fn masked_paths(credentials: &[Utf8PathBuf], writable: &[Utf8PathBuf]) -> Vec<Utf8PathBuf> {
    let reachable: Vec<&Utf8PathBuf> = credentials
        .iter()
        .filter(|credential| !writable.iter().any(|root| root.starts_with(credential)))
        .collect();
    reachable
        .iter()
        .filter(|credential| {
            !reachable.iter().any(|other| {
                other != *credential && other.is_dir() && credential.starts_with(other.as_path())
            })
        })
        .map(|credential| (*credential).clone())
        .collect()
}

/// Returns the repository paths under the writable roots that stay read-only.
///
/// Only paths that exist are named, resolved the same way the roots are, since
/// a rule about a missing path matches nothing and a `bwrap` bind of one fails.
fn protected_paths(writable: &[Utf8PathBuf]) -> Vec<Utf8PathBuf> {
    let mut paths = Vec::new();
    for root in writable {
        for relative in PROTECTED_REPOSITORY_PATHS {
            let path = root.join(relative);
            if let Ok(resolved) = path.canonicalize_utf8()
                && !paths.contains(&resolved)
            {
                paths.push(resolved);
            }
        }
    }
    paths
}

/// Returns the temporary directories a command on macOS may write.
///
/// A compiler, `mktemp`, and most test runners write a scratch file before
/// anything else, so refusing it fails the command before it does any work.
/// Linux gives a command a private `/tmp` instead, which is why only the
/// Seatbelt profile names these.
fn temporary_roots() -> Vec<Utf8PathBuf> {
    let mut roots: Vec<Utf8PathBuf> = Vec::new();
    // `temp_dir` is named as well as `TMPDIR`: with the variable unset, a
    // program asks the system for its per-user directory under /var/folders,
    // which is where a compiler's scratch files land regardless.
    let candidates = [
        std::env::var("TMPDIR").ok(),
        std::env::temp_dir().to_str().map(str::to_owned),
        Some(String::from("/tmp")),
    ];
    for candidate in candidates.into_iter().flatten() {
        if candidate.is_empty() {
            continue;
        }
        if let Ok(resolved) = Utf8Path::new(&candidate).canonicalize_utf8()
            && resolved.is_dir()
            && !roots.contains(&resolved)
        {
            roots.push(resolved);
        }
    }
    roots
}

/// Returns the Seatbelt rule that covers a resolved path.
///
/// A file is named literally, where a subpath rule would also cover a directory
/// that happened to share its name.
fn rule_for(resolved: &Utf8Path) -> &'static str {
    if resolved.is_dir() {
        "subpath"
    } else {
        "literal"
    }
}

/// Returns a path as a Seatbelt rule operand.
fn operand(path: &Utf8Path) -> Result<String> {
    Ok(format!("({} {})", rule_for(path), quote(path)?))
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
    /// permitted because no rule mentioned it. Reads are open except for the
    /// credential locations under the home directory, which are denied. Network
    /// is denied unless the policy grants it.
    ///
    /// Two details of the rule language decide the shape of this profile, both
    /// measured against `sandbox-exec` on macOS rather than assumed:
    ///
    /// - A rule matches the path the kernel evaluates, so every path is
    ///   canonicalized before it is named. A rule about the unresolved spelling
    ///   of a path matches nothing, which on macOS is the difference between
    ///   `/tmp/x` and `/private/tmp/x`.
    /// - The last rule that matches decides, so the credential deny is emitted
    ///   after the grant for the readable roots. Otherwise a workspace that
    ///   contains a credential path, which is the case when the home directory
    ///   itself is the workspace, would re-grant the very path that was denied.
    pub fn profile(policy: &SandboxPolicy) -> Result<String> {
        Self::profile_with(policy, &existing_credential_paths())
    }

    /// Returns the generated profile for a policy and an explicit credential
    /// list.
    ///
    /// The credential paths are an input rather than resolved inside, so a test
    /// can build a profile over a fixture home without moving this process's own
    /// home directory.
    fn profile_with(policy: &SandboxPolicy, credentials: &[Utf8PathBuf]) -> Result<String> {
        let writable = policy
            .writable()
            .map(|path| resolve(path, "a writable path"))
            .collect::<Result<Vec<_>>>()?;
        let mut profile = String::new();
        let _ = writeln!(profile, "(version 1)");
        let _ = writeln!(profile, "(allow default)");
        let _ = writeln!(profile, "(deny file-write*)");
        for resolved in writable.iter().chain(temporary_roots().iter()) {
            let _ = writeln!(profile, "(allow file-write* {})", operand(resolved)?);
        }
        // Redirection to these is not a way to change the machine, and denying
        // them breaks almost every command that prints.
        for device in ["/dev/null", "/dev/stdout", "/dev/stderr", "/dev/tty"] {
            let _ = writeln!(profile, "(allow file-write* (literal \"{device}\"))");
        }
        // After the grants, so the deny is the rule that decides for these.
        for protected in protected_paths(&writable) {
            let _ = writeln!(profile, "(deny file-write* {})", operand(&protected)?);
        }
        // Reading stays open everywhere else, so a command still reads its own
        // libraries and the files it was pointed at. The roots it works in are
        // named before the deny below, never after it: Seatbelt applies the last
        // matching rule, so a grant emitted after a deny would reopen it, which
        // is what happens when the home directory itself is the workspace.
        //
        // The readable roots and the writable ones are the same paths, so the
        // loop is shared.
        for resolved in &writable {
            let _ = writeln!(profile, "(allow file-read* {})", operand(resolved)?);
        }
        // Writes are denied as well as reads. When the home directory is the
        // workspace, a credential location sits under a writable grant, and a
        // command could otherwise add a key to `authorized_keys` or a
        // `ProxyCommand` to the SSH configuration without reading either.
        for credential in masked_paths(credentials, &writable) {
            let _ = writeln!(profile, "(deny file-read* {})", operand(&credential)?);
            let _ = writeln!(profile, "(deny file-write* {})", operand(&credential)?);
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
        let writable = policy
            .writable()
            .map(|path| resolve(path, "a writable path"))
            .collect::<Result<Vec<_>>>()?;
        for resolved in &writable {
            argv.push(String::from("--bind"));
            argv.push(resolved.as_str().to_owned());
            argv.push(resolved.as_str().to_owned());
        }
        // Bound again read-only after the writable binds, so the later mount is
        // the one a command sees at these paths.
        for protected in protected_paths(&writable) {
            argv.push(String::from("--ro-bind"));
            argv.push(protected.as_str().to_owned());
            argv.push(protected.as_str().to_owned());
        }
        // The credential locations are covered last, so the mask is the rule
        // that applies to a path a writable bind would otherwise expose, which
        // is the case when the home directory is itself the workspace.
        for credential in masked_paths(&existing_credential_paths(), &writable) {
            if credential.is_dir() {
                // A directory is covered by an empty filesystem, which hides
                // everything under it in one operation.
                argv.push(String::from("--tmpfs"));
                argv.push(credential.as_str().to_owned());
            } else {
                // A single file cannot take a tmpfs, because that would turn it
                // into a directory and change what the path is. It is covered
                // by the null device instead, which is the same kind of node and
                // carries no data. Read-only, so the mask cannot be replaced by
                // a write.
                argv.push(String::from("--ro-bind"));
                argv.push(String::from(NULL_DEVICE));
                argv.push(credential.as_str().to_owned());
            }
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

    /// Returns a directory outside the temporary roots a command may write,
    /// under the build's own output directory.
    #[cfg(target_os = "macos")]
    fn outside_temporary_roots() -> (TempDir, Utf8PathBuf) {
        let parent = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/sandbox-tests");
        std::fs::create_dir_all(&parent).expect("test parent");
        let dir = tempfile::tempdir_in(&parent).expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .expect("utf8")
            .canonicalize_utf8()
            .expect("resolved");
        (dir, path)
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

    #[cfg(target_os = "macos")]
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

    #[cfg(target_os = "macos")]
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
            // The remedy is carried as a hint rather than folded into the
            // message, so a caller that prints them on separate lines shows it
            // once.
            assert!(
                error
                    .hint()
                    .is_some_and(|h| h.contains(UNSANDBOXED_OVERRIDE)),
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

    #[cfg(target_os = "macos")]
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
        assert!(
            error
                .hint()
                .is_some_and(|h| h.contains(UNSANDBOXED_OVERRIDE)),
            "{error}"
        );
        assert!(error.to_string().contains(std::env::consts::OS), "{error}");
        assert_eq!(
            NullSandbox
                .wrap(&prepared, &policy(dir.as_path()), true)
                .expect("override"),
            prepared
        );
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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
        // Not under the temporary directory, which a command may write.
        let (_outside, outside) = outside_temporary_roots();
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

    /// Returns a home fixture holding one readable path at each credential
    /// location, so a profile can be built over a real directory tree.
    ///
    /// An entry with no separator is made a directory and one with a separator a
    /// file, which exercises both the subpath and the literal rule without the
    /// test restating which credential is which. The exact kinds are not what is
    /// under test; the rule that is emitted for each is.
    #[cfg(target_os = "macos")]
    fn credential_home() -> (TempDir, Utf8PathBuf) {
        let (dir, home) = tempdir();
        for relative in CREDENTIAL_PATHS {
            let path = home.join(relative);
            if let Some((parent, _)) = relative.rsplit_once('/') {
                std::fs::create_dir_all(home.join(parent)).expect("credential parent");
                std::fs::write(&path, "credential").expect("credential file");
            } else {
                std::fs::create_dir(&path).expect("credential directory");
                std::fs::write(path.join("id_rsa"), "credential").expect("credential file");
            }
        }
        (dir, home)
    }

    /// Returns a profile over a fixture home rather than this process's own.
    #[cfg(target_os = "macos")]
    fn profile_for(home: &Utf8Path, workspace: &Utf8Path) -> String {
        MacSandbox::profile_with(&policy(workspace), &credential_paths_under(home))
            .expect("profile")
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_profile_denies_reads_of_the_credential_locations() {
        let (_home_guard, home) = credential_home();
        let (_workspace_guard, workspace) = tempdir();
        let credentials = credential_paths_under(&home);
        assert_eq!(
            credentials.len(),
            CREDENTIAL_PATHS.len(),
            "the fixture did not produce every credential path: {credentials:?}"
        );
        let policy = policy(workspace.as_path());
        let profile = MacSandbox::profile_with(&policy, &credentials).expect("profile");
        for credential in &credentials {
            let resolved = std::fs::canonicalize(credential)
                .expect("a resolved credential")
                .to_string_lossy()
                .into_owned();
            let rule = if credential.is_dir() {
                format!("(deny file-read* (subpath \"{resolved}\"))")
            } else {
                format!("(deny file-read* (literal \"{resolved}\"))")
            };
            assert!(
                profile.contains(&rule),
                "no rule for {credential}:\n{profile}"
            );
        }
        // The workspace grant survives the deny, so ordinary work still reads.
        let resolved_workspace = std::fs::canonicalize(workspace.as_path())
            .expect("a resolved workspace")
            .to_string_lossy()
            .into_owned();
        assert!(
            profile.contains(&format!(
                "(allow file-read* (subpath \"{resolved_workspace}\"))"
            )),
            "{profile}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_credential_rule_names_the_resolved_path() {
        // A Seatbelt rule matches the path the kernel evaluates, so a rule about
        // the spelling a path was built from matches nothing.
        let (_home_guard, home) = credential_home();
        let (_workspace_guard, workspace) = tempdir();
        let joined = home.join(".ssh");
        let resolved = std::fs::canonicalize(&joined).expect("a canonical credential");
        assert_ne!(
            joined.as_std_path(),
            resolved.as_path(),
            "the fixture is already resolved, so this test would prove nothing"
        );

        let profile = profile_for(&home, workspace.as_path());
        let rule = format!(
            "(deny file-read* (subpath \"{}\"))",
            resolved.to_string_lossy()
        );
        assert!(
            profile.contains(&rule),
            "the profile does not name the resolved credential path:\n{profile}"
        );
        // The unresolved spelling is what a rule that never matched would carry.
        // It is a prefix of the resolved path on this host, so the check is for
        // the rule operand rather than for the substring.
        assert!(
            !profile.contains(&format!(
                "(deny file-read* (subpath \"{}\"))",
                joined.as_str()
            )),
            "the profile names the unresolved credential path:\n{profile}"
        );
    }

    #[test]
    fn a_credential_path_that_is_absent_is_not_named() {
        let (_guard, home) = tempdir();
        std::fs::create_dir(home.join(".ssh")).expect("ssh directory");
        let expected = home.join(".ssh").canonicalize_utf8().expect("resolved");
        assert_eq!(credential_paths_under(&home), vec![expected]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_linux_argv_masks_the_credential_locations() {
        let (_dir, dir) = tempdir();
        let sandbox = LinuxSandbox::with_helper(Utf8PathBuf::from("/usr/bin/bwrap"));
        let prepared = prepare("echo hello", dir.as_path(), None, environment()).expect("prepare");
        let wrapped = sandbox
            .wrap(&prepared, &policy(dir.as_path()), false)
            .expect("wrap");
        for credential in policy(dir.as_path())
            .credential_paths()
            .expect("credential paths")
        {
            let path = credential.as_str();
            // A directory mask is a two-token operation and a file mask is a
            // three-token one, so each is matched as the exact sequence rather
            // than by finding the path and looking backwards, which would also
            // match a writable bind when the home directory is the workspace.
            let masked = if credential.is_dir() {
                wrapped
                    .argv
                    .windows(2)
                    .any(|window| window[0] == "--tmpfs" && window[1] == path)
            } else {
                wrapped.argv.windows(3).any(|window| {
                    window[0] == "--ro-bind" && window[1] == NULL_DEVICE && window[2] == path
                })
            };
            assert!(masked, "no mask for {credential}: {:?}", wrapped.argv);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_seatbelt_profile_denies_a_credential_read_and_allows_the_workspace() {
        // The profile text is what the other tests assert on. This one runs the
        // tool against a profile built over a fixture home, which is the only
        // evidence that the rule is enforced rather than merely present, and it
        // pairs a refused read with a permitted one so a profile that denied
        // every read would not pass.
        let sandbox = MacSandbox::detect();
        if !sandbox.support().is_full() {
            return;
        }
        let home = credential_home();
        let workspace = tempdir();
        let private = home.1.join(".ssh/id_rsa");
        let public = workspace.1.join("input.txt");
        std::fs::write(&public, "contents").expect("workspace file");

        let profile = profile_for(&home.1, workspace.1.as_path());

        for (path, permitted) in [(&private, false), (&public, true)] {
            let prepared = prepare(
                &format!("/bin/cat {path}"),
                workspace.1.as_path(),
                None,
                environment(),
            )
            .expect("prepare");
            let mut argv = vec![
                String::from(SEATBELT_TOOL),
                String::from("-p"),
                profile.clone(),
            ];
            argv.extend(prepared.argv.iter().cloned());
            let wrapped = PreparedCommand {
                argv,
                ..prepared.clone()
            };
            let outcome = run(&wrapped, Duration::from_secs(20), &never).expect("run");
            assert_eq!(
                outcome.exit.is_success(),
                permitted,
                "reading {path} produced {outcome:?}"
            );
        }
    }

    #[test]
    fn a_credential_that_holds_the_workspace_is_left_readable() {
        // Masking a directory that contains the workspace would hide the
        // workspace itself, so the command could not do the work it was run for.
        let (_guard, home) = tempdir();
        let state = home.join("state");
        let workspace = state.join("project");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let secret = state.join("credentials.json");
        std::fs::write(&secret, "{}").expect("secret");
        let state = state.canonicalize_utf8().expect("state");
        let secret = secret.canonicalize_utf8().expect("secret");
        let workspace = workspace.canonicalize_utf8().expect("workspace");

        let masked = masked_paths(&[state, secret.clone()], &[workspace]);
        assert_eq!(masked, vec![secret], "the narrower entry must still apply");
    }

    #[test]
    fn a_path_inside_a_masked_directory_is_not_masked_twice() {
        // The directory already hides what is inside it, and a second mount
        // inside an emptied directory has nothing to cover.
        let (_guard, home) = tempdir();
        let state = home.join("state");
        std::fs::create_dir_all(&state).expect("state");
        let secret = state.join("credentials.json");
        std::fs::write(&secret, "{}").expect("secret");
        let (_other, workspace) = tempdir();
        let state = state.canonicalize_utf8().expect("state");
        let secret = secret.canonicalize_utf8().expect("secret");
        let workspace = workspace.canonicalize_utf8().expect("workspace");

        let masked = masked_paths(&[state.clone(), secret], &[workspace]);
        assert_eq!(masked, vec![state]);
    }

    #[test]
    fn the_credential_list_names_where_rune_keeps_its_own_credential() {
        // The credential file lives in the state root, not beside the
        // configuration, so a list that only named the configuration directory
        // left this program's own key readable to every command it ran.
        let own = Paths::resolve(Some("/home/u"), None, None, None, None);
        let state = own
            .state_root
            .strip_prefix("/home/u")
            .expect("the default state root is under the home directory");
        assert!(
            CREDENTIAL_PATHS.contains(&state.as_str()),
            "`{state}` holds the credential file and is not masked"
        );
    }

    #[test]
    fn a_repository_keeps_its_configuration_and_hooks_read_only() {
        let (_guard, workspace) = tempdir();
        std::fs::create_dir_all(workspace.join(".git/hooks")).expect("hooks");
        std::fs::write(workspace.join(".git/config"), "[core]\n").expect("config");
        let resolved = workspace.canonicalize_utf8().expect("workspace");
        let protected = protected_paths(std::slice::from_ref(&resolved));
        assert_eq!(
            protected,
            vec![resolved.join(".git/config"), resolved.join(".git/hooks")]
        );
        // A workspace that is not a repository has nothing to protect.
        let (_other, plain) = tempdir();
        let plain = plain.canonicalize_utf8().expect("plain");
        assert!(protected_paths(&[plain]).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_sandboxed_command_can_write_a_temporary_file_but_not_git_hooks() {
        // A compiler and `mktemp` write to the temporary directory before they
        // do anything else, so refusing it fails ordinary work. The hooks
        // directory stays closed, because git runs what is in it unsandboxed.
        let sandbox = MacSandbox::detect();
        if !sandbox.support().is_full() {
            return;
        }
        let (_guard, workspace) = tempdir();
        std::fs::create_dir_all(workspace.join(".git/hooks")).expect("hooks");
        std::fs::write(workspace.join(".git/config"), "[core]\n").expect("config");
        let policy = policy(workspace.as_path());
        for (command, permitted) in [
            ("mktemp", true),
            ("echo x > .git/hooks/pre-commit", false),
            ("echo x >> .git/config", false),
            ("echo x > kept.txt", true),
        ] {
            let prepared = prepare(
                command,
                workspace.as_path(),
                None,
                environment_with_tmpdir(),
            )
            .expect("prepare");
            let wrapped = sandbox.wrap(&prepared, &policy, false).expect("wrap");
            let outcome = run(&wrapped, Duration::from_secs(20), &never).expect("run");
            assert_eq!(
                outcome.exit.is_success(),
                permitted,
                "`{command}` produced {outcome:?}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_credential_under_a_writable_home_cannot_be_written() {
        // When the home directory is the workspace, the credential locations
        // sit under a writable grant. A command must still not be able to add
        // a key to them.
        let sandbox = MacSandbox::detect();
        if !sandbox.support().is_full() {
            return;
        }
        let (_home_guard, home) = credential_home();
        let profile = profile_for(&home, &home);
        let target = home.join(".ssh/authorized_keys");
        let prepared = prepare(
            &format!("echo planted >> {target}"),
            home.as_path(),
            None,
            environment(),
        )
        .expect("prepare");
        let mut argv = vec![String::from(SEATBELT_TOOL), String::from("-p"), profile];
        argv.extend(prepared.argv.iter().cloned());
        let wrapped = PreparedCommand {
            argv,
            ..prepared.clone()
        };
        let outcome = run(&wrapped, Duration::from_secs(20), &never).expect("run");
        assert!(!outcome.exit.is_success(), "{outcome:?}");
        assert!(
            !target.exists(),
            "a key was planted under the credential path"
        );
    }

    /// Returns the test environment with this process's temporary directory,
    /// which is where `mktemp` writes.
    #[cfg(target_os = "macos")]
    fn environment_with_tmpdir() -> BTreeMap<String, String> {
        let mut environment = environment();
        if let Ok(tmpdir) = std::env::var("TMPDIR") {
            environment.insert(String::from("TMPDIR"), tmpdir);
        }
        environment
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_linux_argv_binds_repository_hooks_read_only_after_the_workspace() {
        let (_dir, dir) = tempdir();
        std::fs::create_dir_all(dir.join(".git/hooks")).expect("hooks");
        std::fs::write(dir.join(".git/config"), "[core]\n").expect("config");
        let sandbox = LinuxSandbox::with_helper(Utf8PathBuf::from("/usr/bin/bwrap"));
        let prepared = prepare("echo hello", dir.as_path(), None, environment()).expect("prepare");
        let wrapped = sandbox
            .wrap(&prepared, &policy(dir.as_path()), false)
            .expect("wrap");
        let resolved = dir.canonicalize_utf8().expect("resolved");
        let bind = wrapped
            .argv
            .windows(3)
            .position(|window| window[0] == "--bind" && window[1] == resolved.as_str())
            .expect("the workspace bind");
        for relative in PROTECTED_REPOSITORY_PATHS {
            let path = resolved.join(relative);
            let read_only = wrapped
                .argv
                .windows(3)
                .position(|window| {
                    window[0] == "--ro-bind"
                        && window[1] == path.as_str()
                        && window[2] == path.as_str()
                })
                .unwrap_or_else(|| panic!("no read-only bind for {path}: {:?}", wrapped.argv));
            assert!(
                read_only > bind,
                "{path} is bound before the workspace, so it is writable"
            );
        }
    }
}
