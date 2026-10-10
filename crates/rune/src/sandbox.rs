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
}

/// Builds a diagnostic report without starting the supplied command.
pub fn explain(
    settings: &Settings,
    saved_directory_source: Layer,
    launch: &Launch,
    workspace: &Utf8Path,
) -> Result<Report> {
    if launch.args.first().map(String::as_str) != Some("explain") {
        return Err(RuneError::invalid_field(
            "action",
            "expected `sandbox explain`",
        ));
    }
    let command = launch
        .args
        .iter()
        .skip(1)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let mut context = ExecutionContext::new(workspace.to_owned())
        .with_offline(settings.offline)
        .with_allow_unsandboxed(settings.allow_unsandboxed)
        .with_external_access(launch.has_flag("--external-access"));
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
    )
}

fn report(
    backend: &dyn Sandbox,
    prepared: &rune_exec::PreparedCommand,
    policy: &rune_exec::SandboxPolicy,
    settings: &Settings,
    saved_directory_source: Layer,
    launch: &Launch,
) -> Result<Report> {
    let paths = policy.resolved_paths()?;
    let support = backend.support();
    let (enforcement, reason) = match backend.wrap(prepared, policy, settings.allow_unsandboxed) {
        Ok(_) if support.is_full() => ("enforced", None),
        Ok(_) => ("unsandboxed", support.reason().map(str::to_owned)),
        Err(error) => ("refused", Some(error.to_string())),
    };
    let protected_paths = paths.protected_paths.iter().map(|path| PathRule {
        path: path.to_string(),
        access: "read_only",
        source: "sandbox PROTECTED_REPOSITORY_PATHS (existing paths under writable roots)".into(),
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
        let full_backends: Vec<Box<dyn Sandbox>> = vec![
            Box::new(LinuxSandbox::with_helper("/fixture/bwrap".into())),
            Box::new(MacSandbox::with_support(Support::Full)),
        ];
        for backend in full_backends {
            let report = report(
                &*backend,
                &prepared,
                &policy,
                &Settings::default(),
                Layer::Default,
                &launch,
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
