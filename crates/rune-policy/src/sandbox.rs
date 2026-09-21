//! Capability restriction for command execution.
//!
//! An approved command still gets to write, so the sandbox is what keeps an
//! approved command inside the workspace. Each backend turns a command string
//! into the argv that actually runs, and reports whether it can enforce the
//! restriction on this host.
//!
//! The module never degrades quietly. A backend that is absent or that cannot
//! restrict every thread returns an error, and the only way past it is an
//! explicit `allow_unsandboxed`, which the caller has to pass deliberately.
//! A sandbox that runs the command anyway would be indistinguishable from no
//! sandbox while looking like one.
//!
//! Backend notes:
//!
//! - macOS uses `sandbox-exec` with a generated Seatbelt profile. Apple
//!   deprecated the tool but has not removed it, and it is the only option for
//!   a non-bundled CLI, so the backend probes it at runtime rather than
//!   assuming it.
//! - Linux installs its rules through the `bwrap` helper. Landlock has no
//!   command-line form, so enforcing it in-process needs the `landlock` crate,
//!   and a crate that has to reach syscalls is one this module cannot depend on.
//!   The helper is where the mount namespace and the network namespace come
//!   from, and it restricts the exec'd process tree as a whole, which is the
//!   every-thread guarantee the in-process path has to ask for explicitly.

use std::fmt::Write as _;

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::command::shell_argv;

/// The flag a caller passes to run a command with no sandbox.
pub const UNSANDBOXED_OVERRIDE: &str = "--allow-unsandboxed";

/// Path of the macOS sandboxing tool, as shipped with the system.
const SEATBELT_TOOL: &str = "/usr/bin/sandbox-exec";

/// The Linux helper that builds the namespaces.
const NAMESPACE_HELPER: &str = "bwrap";

/// What a backend can enforce on this host.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Support {
    /// The backend restricts every thread of the process it starts.
    Full,
    /// The backend runs, but part of the process tree would stay unrestricted.
    /// A caller must fail closed: a partial restriction is a false promise.
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

    /// Returns the reason attached to a non-full status.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Full => None,
            Self::Partial { reason } | Self::Unsupported { reason } => Some(reason),
        }
    }
}

/// The result of probing the host for what a backend needs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Enforcement {
    /// The host grants the backend everything it needs.
    Full,
    /// The capability is present but cannot cover every thread.
    Incomplete {
        /// What the host refused.
        detail: String,
    },
    /// The capability is not on this host.
    Absent {
        /// What was looked for and not found.
        detail: String,
    },
}

impl Enforcement {
    /// Reports the enforcement this probe result implies.
    #[must_use]
    pub fn support(&self) -> Support {
        match self {
            Self::Full => Support::Full,
            Self::Incomplete { detail } => Support::Partial {
                reason: detail.clone(),
            },
            Self::Absent { detail } => Support::Unsupported {
                reason: detail.clone(),
            },
        }
    }
}

/// A way to restrict what a command can reach.
///
/// `wrap` returns the argv to run instead of the command, or an error when the
/// requested restriction cannot be applied. Returning the command unchanged is
/// never an implicit outcome: it requires `allow_unsandboxed`.
pub trait Sandbox {
    /// Returns the backend name, for diagnostics.
    fn name(&self) -> &'static str;

    /// Reports what this host can enforce.
    fn support(&self) -> Support;

    /// Returns the argv that runs a command under the restriction.
    ///
    /// `workspace` is writable, and so is every entry of `roots`. `network`
    /// grants network access; when false, the command runs without it. When the
    /// restriction cannot be applied, the result is an error unless
    /// `allow_unsandboxed` is set, in which case the command runs as written.
    fn wrap(
        &self,
        command: &str,
        workspace: &Utf8Path,
        roots: &[Utf8PathBuf],
        network: bool,
        allow_unsandboxed: bool,
    ) -> Result<Vec<String>>;
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
    let refused = if support.is_full() {
        "the backend reported full enforcement but produced no argv"
    } else if matches!(support, Support::Partial { .. }) {
        "the backend cannot restrict every thread"
    } else {
        "no sandbox backend is available for this platform"
    };
    RuneError::new(
        ErrorCode::Unsupported,
        format!("{backend} refused to run the command: {refused}: {reason}"),
    )
    .with_hint(format!(
        "pass {UNSANDBOXED_OVERRIDE} to run the command with no sandbox"
    ))
}

/// Returns the error for a workspace or root that does not resolve.
fn unresolved(path: &Utf8Path, what: &str) -> RuneError {
    RuneError::new(
        ErrorCode::NotFound,
        format!("{what} `{path}` does not exist or cannot be resolved"),
    )
    .with_hint("the restriction is built from resolved paths, so the path has to exist")
}

/// Resolves a path to its real location.
///
/// The restriction is expressed against the path the kernel sees. On macOS a
/// grant for `/tmp/x` does not cover `/private/tmp/x`, so an unresolved path
/// would produce a profile that denies the very directory it meant to allow.
fn resolve(path: &Utf8Path, what: &str) -> Result<Utf8PathBuf> {
    path.canonicalize_utf8().map_err(|_| unresolved(path, what))
}

/// Returns the argv for a command that runs with no restriction.
fn unsandboxed(command: &str) -> Vec<String> {
    shell_argv(command)
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
            reason: format!(
                "{} has no sandbox backend; commands run under this backend are not restricted",
                std::env::consts::OS
            ),
        }
    }

    fn wrap(
        &self,
        command: &str,
        _workspace: &Utf8Path,
        _roots: &[Utf8PathBuf],
        _network: bool,
        allow_unsandboxed: bool,
    ) -> Result<Vec<String>> {
        if allow_unsandboxed {
            return Ok(unsandboxed(command));
        }
        Err(unavailable(self.name(), &self.support()))
    }
}

/// The macOS Seatbelt backend.
#[derive(Clone, Debug)]
pub struct MacSandbox {
    enforcement: Enforcement,
}

impl MacSandbox {
    /// Probes the host.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            enforcement: probe_seatbelt(),
        }
    }

    /// Builds a backend with a known probe result.
    #[must_use]
    pub const fn with_enforcement(enforcement: Enforcement) -> Self {
        Self { enforcement }
    }

    /// Returns the generated profile for a workspace and its roots.
    ///
    /// The profile denies every write and then re-allows the workspace and each
    /// root, so an action has to be granted deliberately rather than being
    /// permitted by an unhandled operation. Reading is left alone: the point is
    /// to keep an approved command from reaching the rest of the machine, not to
    /// rebuild a container.
    pub fn profile(workspace: &Utf8Path, roots: &[Utf8PathBuf], network: bool) -> Result<String> {
        let workspace = resolve(workspace, "the workspace")?;
        let mut profile = String::new();
        let _ = writeln!(profile, "(version 1)");
        let _ = writeln!(profile, "(allow default)");
        let _ = writeln!(profile, "(deny file-write*)");
        for path in std::iter::once(&workspace).chain(roots.iter()) {
            let resolved = resolve(path, "a writable root")?;
            let rule = if resolved.is_dir() {
                "subpath"
            } else {
                // A file root grants the file and not a directory that happens
                // to share its name, which a subpath rule would allow.
                "literal"
            };
            let _ = writeln!(
                profile,
                "(allow file-write* ({rule} \"{}\"))",
                quote(&resolved)?
            );
        }
        // Output redirection to these is not a way to change the machine, and
        // denying them breaks almost every command that prints.
        for device in ["/dev/null", "/dev/stdout", "/dev/stderr", "/dev/tty"] {
            let _ = writeln!(profile, "(allow file-write* (literal \"{device}\"))");
        }
        if !network {
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
        self.enforcement.support()
    }

    fn wrap(
        &self,
        command: &str,
        workspace: &Utf8Path,
        roots: &[Utf8PathBuf],
        network: bool,
        allow_unsandboxed: bool,
    ) -> Result<Vec<String>> {
        if !self.support().is_full() {
            if allow_unsandboxed {
                return Ok(unsandboxed(command));
            }
            return Err(unavailable(self.name(), &self.support()));
        }
        let profile = Self::profile(workspace, roots, network)?;
        let mut argv = vec![
            SEATBELT_TOOL.to_owned(),
            "-p".to_owned(),
            profile,
            // Ends the tool's own options, so a command whose first word starts
            // with a hyphen cannot be read as a flag to the sandbox.
            "--".to_owned(),
        ];
        argv.extend(unsandboxed(command));
        Ok(argv)
    }
}

/// A path in a profile, refused when it cannot be quoted exactly.
///
/// A quote or a backslash inside the path would end the string early and turn
/// the rest of the path into policy, so the profile is refused instead. A path
/// holding either is not one this backend can express.
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

/// Runs the Seatbelt tool once to see whether this host still enforces it.
fn probe_seatbelt() -> Enforcement {
    let tool = Utf8Path::new(SEATBELT_TOOL);
    if !tool.is_file() {
        return Enforcement::Absent {
            detail: format!(
                "{SEATBELT_TOOL} is not installed on {}",
                std::env::consts::OS
            ),
        };
    }
    let probe = std::process::Command::new(SEATBELT_TOOL)
        .arg("-p")
        .arg("(version 1)(allow default)")
        .arg("/usr/bin/true")
        .status();
    match probe {
        Ok(status) if status.success() => Enforcement::Full,
        Ok(status) => Enforcement::Incomplete {
            detail: format!("{SEATBELT_TOOL} exited with {status} for an empty policy"),
        },
        Err(err) => Enforcement::Absent {
            detail: format!("{SEATBELT_TOOL} could not be run: {err}"),
        },
    }
}

/// The Linux namespace backend.
#[derive(Clone, Debug)]
pub struct LinuxSandbox {
    enforcement: Enforcement,
}

impl LinuxSandbox {
    /// Probes the host.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            enforcement: probe_namespaces(),
        }
    }

    /// Builds a backend with a known probe result.
    #[must_use]
    pub const fn with_enforcement(enforcement: Enforcement) -> Self {
        Self { enforcement }
    }

    /// Returns the argv for a workspace and its roots.
    fn argv(
        helper: &Utf8Path,
        command: &str,
        workspace: &Utf8Path,
        roots: &[Utf8PathBuf],
        network: bool,
    ) -> Result<Vec<String>> {
        let workspace = resolve(workspace, "the workspace")?;
        let mut argv = vec![
            helper.as_str().to_owned(),
            // The namespaces are torn down with the process, so a killed
            // command cannot leave restricted state behind.
            "--die-with-parent".to_owned(),
            // A new session detaches the command from the caller's terminal, so
            // it cannot drive it.
            "--new-session".to_owned(),
            "--unshare-pid".to_owned(),
        ];
        if !network {
            argv.push("--unshare-net".to_owned());
        }
        argv.push("--ro-bind".to_owned());
        argv.push("/".to_owned());
        argv.push("/".to_owned());
        argv.push("--dev-bind".to_owned());
        argv.push("/dev".to_owned());
        argv.push("/dev".to_owned());
        argv.push("--proc".to_owned());
        argv.push("/proc".to_owned());
        // The host's temporary directory is deliberately not carried over, so a
        // temporary file is written somewhere the command cannot inspect later.
        argv.push("--tmpfs".to_owned());
        argv.push("/tmp".to_owned());
        for path in std::iter::once(&workspace).chain(roots.iter()) {
            let resolved = resolve(path, "a writable root")?;
            argv.push("--bind".to_owned());
            argv.push(resolved.as_str().to_owned());
            argv.push(resolved.as_str().to_owned());
        }
        argv.push("--".to_owned());
        argv.extend(unsandboxed(command));
        Ok(argv)
    }
}

impl Sandbox for LinuxSandbox {
    fn name(&self) -> &'static str {
        "namespaces"
    }

    fn support(&self) -> Support {
        self.enforcement.support()
    }

    fn wrap(
        &self,
        command: &str,
        workspace: &Utf8Path,
        roots: &[Utf8PathBuf],
        network: bool,
        allow_unsandboxed: bool,
    ) -> Result<Vec<String>> {
        if !self.support().is_full() {
            if allow_unsandboxed {
                return Ok(unsandboxed(command));
            }
            return Err(unavailable(self.name(), &self.support()));
        }
        let helper = find_on_path(NAMESPACE_HELPER)
            .ok_or_else(|| unavailable(self.name(), &self.support()))?;
        Self::argv(&helper, command, workspace, roots, network)
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

/// Runs the helper once to see whether this host grants it a namespace.
///
/// A helper that is installed but cannot build a namespace is the case that
/// must not be mistaken for an absent one: the command would still run, with
/// whatever restriction the host happened to allow.
fn probe_namespaces() -> Enforcement {
    let Some(helper) = find_on_path(NAMESPACE_HELPER) else {
        return Enforcement::Absent {
            detail: format!(
                "the `{NAMESPACE_HELPER}` helper is not on PATH on {}",
                std::env::consts::OS
            ),
        };
    };
    let probe = std::process::Command::new(helper.as_str())
        .args(["--ro-bind", "/", "/", "--proc", "/proc", "/bin/true"])
        .status();
    match probe {
        Ok(status) if status.success() => Enforcement::Full,
        Ok(status) => Enforcement::Incomplete {
            detail: format!(
                "`{helper}` exited with {status}, so this host does not grant an unprivileged namespace"
            ),
        },
        Err(err) => Enforcement::Absent {
            detail: format!("`{helper}` could not be run: {err}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    fn tempdir() -> TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    fn utf8(path: &Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(path.to_path_buf()).expect("utf8 path")
    }

    fn full_workspace() -> (TempDir, Utf8PathBuf) {
        let dir = tempdir();
        let path = utf8(dir.path()).canonicalize_utf8().expect("resolve");
        (dir, path)
    }

    #[test]
    fn a_probe_result_maps_to_the_reported_support() {
        assert_eq!(Enforcement::Full.support(), Support::Full);
        assert_eq!(
            Enforcement::Incomplete {
                detail: "the kernel grants no process-wide restriction".to_owned()
            }
            .support(),
            Support::Partial {
                reason: "the kernel grants no process-wide restriction".to_owned()
            }
        );
        assert_eq!(
            Enforcement::Absent {
                detail: "the helper is not installed".to_owned()
            }
            .support(),
            Support::Unsupported {
                reason: "the helper is not installed".to_owned()
            }
        );
    }

    #[test]
    fn a_partial_backend_fails_closed_without_the_override() {
        let (_dir, workspace) = full_workspace();
        let sandbox = LinuxSandbox::with_enforcement(Enforcement::Incomplete {
            detail: "the kernel cannot restrict every thread".to_owned(),
        });
        assert!(matches!(sandbox.support(), Support::Partial { .. }));

        let err = sandbox
            .wrap("rm -rf build", &workspace, &[], false, false)
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::Unsupported);
        assert!(err.message().contains("every thread"));
        assert!(
            err.detail()
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains(UNSANDBOXED_OVERRIDE)),
            "the refusal names the override: {err}"
        );

        let argv = sandbox
            .wrap("rm -rf build", &workspace, &[], false, true)
            .expect("override");
        assert_eq!(
            argv,
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "rm -rf build".to_owned()
            ]
        );
    }

    #[test]
    fn an_absent_backend_fails_closed_without_the_override() {
        let (_dir, workspace) = full_workspace();
        let sandbox = MacSandbox::with_enforcement(Enforcement::Absent {
            detail: "sandbox-exec was removed".to_owned(),
        });
        let err = sandbox
            .wrap("echo hi", &workspace, &[], true, false)
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::Unsupported);
        assert!(err.message().contains("sandbox-exec was removed"));
        let argv = sandbox
            .wrap("echo hi", &workspace, &[], true, true)
            .expect("override");
        assert_eq!(argv.len(), 3);
    }

    #[test]
    fn the_null_backend_names_the_platform_and_the_override() {
        let (_dir, workspace) = full_workspace();
        let sandbox = NullSandbox;
        assert_eq!(sandbox.name(), "null");
        let Support::Unsupported { reason } = sandbox.support() else {
            panic!("no platform has a null backend by accident");
        };
        assert!(reason.contains(std::env::consts::OS));

        let err = sandbox
            .wrap("echo hi", &workspace, &[], false, false)
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::Unsupported);
        assert!(
            err.detail()
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains(UNSANDBOXED_OVERRIDE))
        );
        assert_eq!(
            sandbox
                .wrap("echo hi", &workspace, &[], false, true)
                .expect("override")
                .len(),
            3
        );
    }

    #[test]
    fn a_full_seatbelt_backend_wraps_the_command_in_the_tool() {
        let (_dir, workspace) = full_workspace();
        let sandbox = MacSandbox::with_enforcement(Enforcement::Full);
        let argv = sandbox
            .wrap("git status", &workspace, &[], false, false)
            .expect("wrapped");
        assert_eq!(argv[0], SEATBELT_TOOL);
        assert_eq!(argv[1], "-p");
        assert!(argv[2].contains("(deny file-write*)"));
        assert!(argv[2].contains("(deny network*)"));
        assert!(argv[2].contains(&format!("(allow file-write* (subpath \"{workspace}\"))")));
        assert_eq!(argv[3], "--");
        assert_eq!(argv[4..], ["/bin/sh", "-c", "git status"]);
    }

    #[test]
    fn a_granted_network_leaves_the_profile_without_a_denial() {
        let (_dir, workspace) = full_workspace();
        let argv = MacSandbox::with_enforcement(Enforcement::Full)
            .wrap("curl https://example.test", &workspace, &[], true, false)
            .expect("wrapped");
        assert!(!argv[2].contains("network"));
    }

    #[test]
    fn each_root_is_granted_as_a_subpath() {
        let (_dir, workspace) = full_workspace();
        let extra = tempdir();
        let root = utf8(extra.path()).canonicalize_utf8().expect("resolve");
        let argv = MacSandbox::with_enforcement(Enforcement::Full)
            .wrap("ls", &workspace, &[root.clone()], false, false)
            .expect("wrapped");
        assert!(argv[2].contains(&format!("(allow file-write* (subpath \"{root}\"))")));
    }

    #[test]
    fn a_file_root_is_granted_by_name_rather_than_by_prefix() {
        let (_dir, workspace) = full_workspace();
        let extra = tempdir();
        let file = utf8(extra.path()).join("cache.json");
        std::fs::write(&file, "{}").expect("write");
        let profile = MacSandbox::profile(&workspace, &[file.clone()], false).expect("profile");
        let resolved = file.canonicalize_utf8().expect("resolve");
        assert!(profile.contains(&format!("(allow file-write* (literal \"{resolved}\"))")));
        assert!(!profile.contains(&format!("(subpath \"{resolved}\"))")));
    }

    #[test]
    fn a_workspace_that_does_not_exist_is_refused() {
        let dir = tempdir();
        let missing = utf8(dir.path()).join("absent");
        let err = MacSandbox::with_enforcement(Enforcement::Full)
            .wrap("ls", &missing, &[], false, false)
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.message().contains("the workspace"));
    }

    #[test]
    fn a_root_that_does_not_exist_is_refused() {
        let (_dir, workspace) = full_workspace();
        let dir = tempdir();
        let missing = utf8(dir.path()).join("absent");
        let err = MacSandbox::with_enforcement(Enforcement::Full)
            .wrap("ls", &workspace, &[missing], false, false)
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.message().contains("a writable root"));
    }

    #[test]
    fn a_path_a_profile_cannot_carry_is_refused() {
        let (_dir, workspace) = full_workspace();
        let dir = tempdir();
        let awkward = utf8(dir.path()).join("a\"b");
        std::fs::create_dir(&awkward).expect("create");
        let err = MacSandbox::with_enforcement(Enforcement::Full)
            .wrap("ls", &awkward, &[], false, false)
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.detail().hint.is_some());
        assert!(workspace.is_dir());
    }

    #[test]
    fn a_full_linux_backend_wraps_the_command_in_the_helper() {
        let (_dir, workspace) = full_workspace();
        let helper = find_on_path(NAMESPACE_HELPER);
        let sandbox = LinuxSandbox::with_enforcement(Enforcement::Full);
        let argv = sandbox
            .wrap("git status", &workspace, &[], false, true)
            .expect("wrapped");
        if let Some(helper) = helper {
            assert_eq!(argv[0], helper.as_str());
        }
        assert!(argv.contains(&"--unshare-net".to_owned()));
        assert!(argv.contains(&"--die-with-parent".to_owned()));
        assert!(argv.windows(2).any(|pair| pair == ["--", "/bin/sh"]));
        assert_eq!(argv.last().map(String::as_str), Some("git status"));
        let bind = argv
            .windows(3)
            .find(|window| window[0] == "--bind")
            .expect("the workspace is bound");
        assert_eq!(bind[1], workspace.as_str());
    }

    #[test]
    fn a_granted_network_leaves_the_namespace_shared() {
        let (_dir, workspace) = full_workspace();
        let argv = LinuxSandbox::with_enforcement(Enforcement::Full)
            .wrap("curl https://example.test", &workspace, &[], true, true)
            .expect("wrapped");
        assert!(!argv.contains(&"--unshare-net".to_owned()));
    }

    #[test]
    fn every_root_is_bound_writable() {
        let (_dir, workspace) = full_workspace();
        let extra = tempdir();
        let root = utf8(extra.path()).canonicalize_utf8().expect("resolve");
        let argv = LinuxSandbox::with_enforcement(Enforcement::Full)
            .wrap("ls", &workspace, &[root.clone()], true, true)
            .expect("wrapped");
        assert!(
            argv.windows(3)
                .any(|window| window == ["--bind", root.as_str(), root.as_str()])
        );
    }

    #[test]
    fn the_host_backend_is_selected_by_platform() {
        let sandbox = detect();
        let expected = if cfg!(target_os = "macos") {
            "seatbelt"
        } else if cfg!(target_os = "linux") {
            "namespaces"
        } else {
            "null"
        };
        assert_eq!(sandbox.name(), expected);
    }

    /// Proof against the real tool on this host: a write inside the workspace
    /// succeeds and a write outside it is refused.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_seatbelt_sandbox_refuses_a_write_outside_the_workspace() {
        let (dir, workspace) = full_workspace();
        let outside = tempdir();
        let sandbox = MacSandbox::detect();
        assert_eq!(
            sandbox.support(),
            Support::Full,
            "the host still enforces seatbelt policies"
        );

        let inside = workspace.join("inside.txt");
        let argv = sandbox
            .wrap(
                &format!("printf ok > {inside}"),
                &workspace,
                &[],
                true,
                false,
            )
            .expect("wrapped");
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .expect("run");
        assert!(status.success(), "a workspace write is allowed: {status}");
        assert_eq!(std::fs::read_to_string(&inside).expect("read"), "ok");

        let blocked = utf8(outside.path()).join("outside.txt");
        let argv = sandbox
            .wrap(
                &format!("printf no > {blocked}"),
                &workspace,
                &[],
                true,
                false,
            )
            .expect("wrapped");
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .expect("run");
        assert!(!status.success(), "a write outside is refused: {status}");
        assert!(!blocked.exists());
        drop(dir);
    }
}
