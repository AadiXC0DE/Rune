//! Read-only explanation of a shell command's sandbox policy.

use std::fmt::Write as _;

use camino::Utf8Path;
use rune_core::config::{Layer, Settings};
use rune_core::error::{Result, RuneError};
use rune_exec::sandbox::{Sandbox, Support};
use rune_tools::ExecutionContext;
use serde::Serialize;

use crate::cli::Launch;

#[derive(Debug, Serialize)]
pub struct Sourced<T> {
    value: T,
    source: String,
}

#[derive(Debug, Serialize)]
pub struct PathRule {
    path: String,
    access: &'static str,
    source: String,
}

/// One writable cache directory and what added it.
#[derive(Debug, Serialize)]
pub struct CacheRule {
    path: String,
    source: String,
}

/// A cache directory entry that was not made writable, and why.
#[derive(Debug, Serialize)]
pub struct RefusedCache {
    entry: String,
    reason: &'static str,
}

/// The tool cache directories for these settings, resolved once: the
/// writable roots and the entries refused.
#[must_use]
pub fn cache_roots(
    settings: &Settings,
) -> (Vec<rune_exec::CacheRoot>, Vec<(String, &'static str)>) {
    let home = rune_core::paths::home_directory();
    rune_exec::cache_roots(
        settings.sandbox_cache_roots.as_deref(),
        home.as_deref(),
        &|name| std::env::var(name).ok(),
    )
}

/// The writable cache directories for these settings.
#[must_use]
pub fn cache_paths(settings: &Settings) -> Vec<camino::Utf8PathBuf> {
    cache_roots(settings)
        .0
        .into_iter()
        .map(|root| root.path)
        .collect()
}

/// How commands run on this machine with these settings.
#[derive(Debug, PartialEq, Eq)]
pub struct Status {
    /// The detected backend, such as `seatbelt` or `namespaces`.
    pub backend: String,
    /// `enforced`, `off` (none works and the user allowed that) or `refused`.
    pub state: &'static str,
    /// Why commands are refused, when they are.
    pub reason: Option<String>,
}

/// Decides the sandbox status from the detected backend and the settings.
#[must_use]
pub fn status(settings: &Settings) -> Status {
    let backend = rune_exec::detect();
    let support = backend.support();
    let (state, reason) = if support.is_full() {
        ("enforced", None)
    } else if settings.allow_unsandboxed {
        ("off", None)
    } else {
        (
            "refused",
            Some(
                support
                    .reason()
                    .unwrap_or("the backend cannot enforce a sandbox")
                    .to_owned(),
            ),
        )
    };
    Status {
        backend: backend.name().to_owned(),
        state,
        reason,
    }
}

/// The `rune status` line for the sandbox.
#[must_use]
pub fn status_line(settings: &Settings, status: &Status) -> String {
    match status.state {
        "enforced" => format!("{}, enforced", status.backend),
        "off" => format!(
            "off, commands run without a sandbox (source: {})",
            settings.source_of("allow_unsandboxed")
        ),
        _ => format!(
            "{}, refusing commands: {}",
            status.backend,
            status.reason.as_deref().unwrap_or("no reason given")
        ),
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    command: String,
    backend: Sourced<String>,
    support: &'static str,
    enforcement: &'static str,
    reason: Option<String>,
    allow_unsandboxed: Sourced<bool>,
    writable_roots: Vec<PathRule>,
    temporary_roots: Vec<PathRule>,
    writable_device_paths: Vec<PathRule>,
    network: Sourced<bool>,
    effective_network: &'static str,
    protected_paths: Vec<PathRule>,
    protections_applied: bool,
    cache_roots: Vec<CacheRule>,
    cache_roots_refused: Vec<RefusedCache>,
}

/// Builds a diagnostic report without starting the supplied command.
pub fn explain(
    settings: &Settings,
    saved_directory_source: Layer,
    launch: &Launch,
    workspace: &Utf8Path,
) -> Result<Report> {
    let skip = if launch.separator == Some(0) {
        0
    } else if launch.args.first().map(String::as_str) == Some("explain") {
        1
    } else {
        return Err(RuneError::invalid_field(
            "action",
            "expected `sandbox explain -- <command>` or `sandbox -- <command>`",
        ));
    };
    let command = launch
        .args
        .iter()
        .skip(skip)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let context = ExecutionContext::new(workspace.to_owned())
        .with_offline(settings.offline)
        .with_allow_unsandboxed(settings.allow_unsandboxed)
        .with_external_access(launch.has_flag("--external-access"))
        .with_network(settings.shell_network);
    let (caches, refused_caches) = cache_roots(settings);
    let mut context =
        context.with_cache_roots(caches.iter().map(|root| root.path.clone()).collect());
    if launch.no_additional_dirs {
        context.additional_roots = launch.add_dirs.iter().map(Into::into).collect();
    } else {
        context
            .additional_roots
            .clone_from(&settings.additional_directories);
    }
    let policy = rune_tools::shell::sandbox_policy(&context);
    let prepared = rune_exec::command::prepare_shell(
        &command,
        workspace,
        None,
        rune_exec::minimal_environment(),
    )?;
    let backend = rune_exec::detect();
    report(
        &*backend,
        &prepared,
        &policy,
        settings,
        saved_directory_source,
        launch,
        (&caches, &refused_caches),
    )
}

#[allow(clippy::too_many_arguments)]
fn report(
    backend: &dyn Sandbox,
    prepared: &rune_exec::PreparedCommand,
    policy: &rune_exec::SandboxPolicy,
    settings: &Settings,
    saved_directory_source: Layer,
    launch: &Launch,
    (caches, refused_caches): (&[rune_exec::CacheRoot], &[(String, &'static str)]),
) -> Result<Report> {
    let mut paths = policy.resolved_paths()?;
    // Cache roots are reported on their own lines, with their own sources.
    paths
        .writable_roots
        .retain(|path| !caches.iter().any(|cache| &cache.path == path));
    let support = backend.support();
    let (enforcement, reason) = match backend.wrap(prepared, policy, settings.allow_unsandboxed) {
        Ok(_) if support.is_full() => ("enforced", None),
        Ok(_) => ("unsandboxed", support.reason().map(str::to_owned)),
        Err(error) => ("refused", Some(error.to_string())),
    };
    let protected_paths = paths.protected_paths.iter().map(|path| PathRule {
        path: path.to_string(),
        access: "read_only",
        source: "sandbox PROTECTED_REPOSITORY_PATHS (paths under writable roots; Linux protects .git when hooks are absent; macOS protects absent hooks destinations)".into(),
    }).chain(paths.credential_paths.iter().map(|path| PathRule {
        path: path.to_string(),
        access: "hidden",
        source: "sandbox CREDENTIAL_PATHS and process-resolved Rune config/state/credentials locations (existing paths)".into(),
    })).collect();
    let cli_start = paths
        .writable_roots
        .len()
        .saturating_sub(launch.add_dirs.len());
    let writable_roots = paths
        .writable_roots
        .iter()
        .enumerate()
        .map(|(index, path)| PathRule {
            path: path.to_string(),
            access: "writable_except_protected_paths",
            source: if index == 0 {
                "process working directory (workspace)".into()
            } else if index >= cli_start {
                "command_line --add-dir".into()
            } else {
                format!("{saved_directory_source} additional_directories")
            },
        })
        .collect();
    let network_source = if settings.offline {
        format!(
            "{} offline=true overrides external access",
            settings.source_of("offline")
        )
    } else if launch.has_flag("--external-access") {
        "command_line --external-access (explicit shell context grant)".into()
    } else if settings.shell_network {
        format!(
            "{} shell_network=true (--network, RUNE_NETWORK or shell_network)",
            settings.source_of("shell_network")
        )
    } else {
        "shell context default external_access=false; permission mode does not grant network access"
            .into()
    };
    let temporary_source = if backend.name() == "namespaces" {
        "namespaces private tmpfs (host /tmp is replaced before writable binds)"
    } else {
        "seatbelt temporary_roots from TMPDIR, system temp directory and /tmp"
    };
    Ok(Report {
        command: prepared.reviewed.clone(),
        backend: Sourced {
            value: backend.name().into(),
            source: format!(
                "host detection and enforcement probe ({})",
                std::env::consts::OS
            ),
        },
        support: match support {
            Support::Full => "full",
            Support::Partial { .. } => "partial",
            Support::Unsupported { .. } => "unsupported",
        },
        enforcement,
        reason,
        allow_unsandboxed: Sourced {
            value: settings.allow_unsandboxed,
            source: format!(
                "{} allow_unsandboxed",
                settings.source_of("allow_unsandboxed")
            ),
        },
        writable_roots,
        temporary_roots: backend
            .temporary_writable_roots()
            .iter()
            .map(|path| PathRule {
                path: path.to_string(),
                access: "writable",
                source: temporary_source.into(),
            })
            .collect(),
        writable_device_paths: backend
            .writable_device_paths()
            .iter()
            .map(|path| PathRule {
                path: path.to_string(),
                access: "writable",
                source: format!("{} backend device write grants", backend.name()),
            })
            .collect(),
        network: Sourced {
            value: policy.network,
            source: network_source,
        },
        effective_network: match enforcement {
            "unsandboxed" => "unrestricted",
            "refused" => "not_run",
            _ if policy.network => "allowed",
            _ => "denied",
        },
        protected_paths,
        protections_applied: enforcement == "enforced",
        cache_roots: caches
            .iter()
            .map(|cache| CacheRule {
                path: cache.path.to_string(),
                source: cache.rule.clone(),
            })
            .collect(),
        cache_roots_refused: refused_caches
            .iter()
            .map(|(entry, reason)| RefusedCache {
                entry: entry.clone(),
                reason,
            })
            .collect(),
    })
}

pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "command: {}", report.command);
    let _ = writeln!(
        out,
        "backend: {} (source: {})",
        report.backend.value, report.backend.source
    );
    let _ = writeln!(
        out,
        "support: {}; enforcement: {}",
        report.support, report.enforcement
    );
    if let Some(reason) = &report.reason {
        let _ = writeln!(out, "reason: {reason}");
    }
    let _ = writeln!(
        out,
        "allow_unsandboxed: {} (source: {})",
        report.allow_unsandboxed.value, report.allow_unsandboxed.source
    );
    let _ = writeln!(
        out,
        "network policy: {}; effective: {} (source: {})",
        if report.network.value {
            "allowed"
        } else {
            "denied"
        },
        report.effective_network,
        report.network.source
    );
    for cache in &report.cache_roots {
        let _ = writeln!(out, "cache root: {} (source: {})", cache.path, cache.source);
    }
    for refused in &report.cache_roots_refused {
        let _ = writeln!(
            out,
            "cache root refused: {}: {}",
            refused.entry, refused.reason
        );
    }
    for (label, paths) in [
        ("writable roots", &report.writable_roots),
        ("temporary roots", &report.temporary_roots),
        ("writable device paths", &report.writable_device_paths),
        ("protected paths", &report.protected_paths),
    ] {
        let _ = writeln!(out, "{label}:");
        if paths.is_empty() {
            let _ = writeln!(out, "  none");
        }
        for path in paths {
            let _ = writeln!(
                out,
                "  {}: {} (source: {})",
                path.path, path.access, path.source
            );
        }
    }
    let _ = writeln!(out, "protections applied: {}", report.protections_applied);
    out
}

#[cfg(test)]
mod tests {
    use rune_exec::sandbox::{LinuxSandbox, MacSandbox, NullSandbox};

    use super::*;

    #[test]
    fn the_status_line_names_each_state() {
        let settings = Settings::default();
        let status = |state, reason: Option<&str>| Status {
            backend: "namespaces".to_owned(),
            state,
            reason: reason.map(str::to_owned),
        };
        assert_eq!(
            status_line(&settings, &status("enforced", None)),
            "namespaces, enforced"
        );
        assert_eq!(
            status_line(&settings, &status("off", None)),
            "off, commands run without a sandbox (source: default)"
        );
        assert_eq!(
            status_line(&settings, &status("refused", Some("bwrap is missing"))),
            "namespaces, refusing commands: bwrap is missing"
        );
    }

    #[test]
    fn a_report_distinguishes_enforcement_refusal_and_explicit_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Utf8Path::from_path(dir.path()).expect("utf8");
        let prepared = rune_exec::command::prepare_shell(
            "echo fixture",
            workspace,
            None,
            std::collections::BTreeMap::default(),
        )
        .expect("prepare");
        let policy = rune_exec::SandboxPolicy::new(workspace.to_owned(), vec![], false);
        let launch = crate::cli::parse(
            vec!["sandbox".into(), "explain".into(), "echo fixture".into()],
            false,
        )
        .expect("parse");
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            // Full support must use the host's backend and native path rules.
            #[cfg(target_os = "linux")]
            let backend = LinuxSandbox::with_helper("/fixture/bwrap".into());
            #[cfg(target_os = "macos")]
            let backend = MacSandbox::with_support(Support::Full);
            let report = report(
                &backend,
                &prepared,
                &policy,
                &Settings::default(),
                Layer::Default,
                &launch,
                (&[], &[]),
            )
            .expect("report");
            assert_eq!(report.enforcement, "enforced");
            assert!(report.protections_applied);
            assert_eq!(report.effective_network, "denied");
            assert!(!report.temporary_roots.is_empty());
            assert!(!report.writable_device_paths.is_empty());
        }
        let unavailable: Vec<Box<dyn Sandbox>> = vec![
            Box::new(NullSandbox),
            Box::new(LinuxSandbox::with_support(Support::Partial {
                reason: "fixture partial support".into(),
            })),
            Box::new(MacSandbox::with_support(Support::Unsupported {
                reason: "fixture missing helper".into(),
            })),
        ];
        for backend in unavailable {
            for allow_unsandboxed in [false, true] {
                let settings = Settings {
                    allow_unsandboxed,
                    ..Settings::default()
                };
                let report = report(
                    &*backend,
                    &prepared,
                    &policy,
                    &settings,
                    Layer::Default,
                    &launch,
                    (&[], &[]),
                )
                .expect("report");
                assert_eq!(
                    report.enforcement,
                    if allow_unsandboxed {
                        "unsandboxed"
                    } else {
                        "refused"
                    }
                );
                assert_eq!(
                    report.effective_network,
                    if allow_unsandboxed {
                        "unrestricted"
                    } else {
                        "not_run"
                    }
                );
                assert!(!report.protections_applied);
                assert!(report.reason.is_some());
                assert!(render(&report).contains("source:"));
            }
        }
    }
}
