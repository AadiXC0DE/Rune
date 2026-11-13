//! Diagnostic commands.
//!
//! These answer "why is this not working" without starting a model turn.
//! Every check emits a stable name and a marker saying whether it passed,
//! warned, failed or could not run, with one line on how to fix it.

use std::fmt::Write as _;

use camino::Utf8Path;
use rune_core::budget::LimitName;
use rune_core::config::{self, Layer, Settings};
use rune_core::error::ErrorCode;
use rune_core::paths::Paths;
use serde::Serialize;

/// Outcome of one diagnostic check.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The check passed.
    Ok,
    /// The check found something worth reporting that is not fatal.
    Warn,
    /// The check found a problem that prevents the command from working.
    Fail,
    /// The check could not run, and says why.
    Unknown,
}

/// One diagnostic check result.
#[derive(Clone, Debug, Serialize)]
pub struct Check {
    /// Stable name of the check.
    pub name: String,
    /// Outcome.
    pub outcome: Outcome,
    /// What was observed.
    pub detail: String,
    /// What to do about it, when something is wrong.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Stable error code, when the check maps to one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
}

impl Check {
    /// Builds a passing check.
    fn ok(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            outcome: Outcome::Ok,
            detail: detail.into(),
            hint: None,
            code: None,
        }
    }

    /// Builds a warning.
    fn warn(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            outcome: Outcome::Warn,
            detail: detail.into(),
            hint: None,
            code: None,
        }
    }

    /// Builds a failure.
    fn fail(name: &str, code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            outcome: Outcome::Fail,
            detail: detail.into(),
            hint: None,
            code: Some(code),
        }
    }

    /// Attaches a repair hint.
    #[must_use]
    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Full diagnostic report.
#[derive(Clone, Debug, Serialize)]
pub struct Report {
    /// Version of the running binary.
    pub version: String,
    /// Workspace the checks ran against.
    pub workspace: String,
    /// Resolved configuration roots.
    pub config_root: String,
    /// Resolved state root.
    pub state_root: String,
    /// Every check, in a stable order.
    pub checks: Vec<Check>,
}

impl Report {
    /// Returns the exit code implied by the report.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        i32::from(
            self.checks
                .iter()
                .any(|check| check.outcome == Outcome::Fail),
        )
    }

    /// Renders the report as text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "{}\nworkspace {}\nconfig    {}\nstate     {}\n\n",
            self.version, self.workspace, self.config_root, self.state_root
        );
        for check in &self.checks {
            let _ = writeln!(
                out,
                "{} {:<14} {}",
                marker(check.outcome),
                check.name,
                check.detail
            );
            if let Some(hint) = &check.hint {
                let _ = writeln!(out, "        fix: {hint}");
            }
        }
        out
    }
}

/// The text marker of an outcome.
const fn marker(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Ok => "ok     ",
        Outcome::Warn => "warning",
        Outcome::Fail => "problem",
        Outcome::Unknown => "skipped",
    }
}

impl Check {
    /// Builds a check that could not run, saying why.
    fn skipped(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            outcome: Outcome::Unknown,
            detail: detail.into(),
            hint: None,
            code: None,
        }
    }
}

/// How long the provider's model list may take.
const PROVIDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Runs every check, in a stable order.
///
/// Only `update`, `clock`, `provider`, `reviewer` and `mcp` contact anything,
/// each with its own timeout; with `--offline` they are skipped and no
/// connection is opened.
#[must_use]
pub fn run(settings: &Settings, paths: &Paths, workspace: &Utf8Path) -> Report {
    let mut checks = vec![
        Check::ok("version", crate::version::version_line()),
        check_workspace(workspace),
        check_state_root(paths),
        check_legacy_layout(),
        // The release lookup these read lands with the upgrade command.
        Check::skipped("update", "not checked: the release lookup is not built yet"),
        Check::skipped("clock", "not checked: the release lookup is not built yet"),
        check_terminal(),
        check_config_layers(settings),
    ];
    let listed = if !settings.provider.is_configured() || settings.offline {
        None
    } else {
        Some(crate::provider_setup::fetch_catalog(
            settings,
            paths,
            PROVIDER_TIMEOUT,
        ))
    };
    let ids: Option<Vec<String>> = listed.as_ref().and_then(|listed| {
        listed.as_ref().ok().map(|catalog| {
            catalog
                .models
                .iter()
                .map(|model| model.id.clone())
                .collect()
        })
    });
    checks.push(provider_check(settings, listed.as_ref()));
    checks.push(check_limits(settings));
    checks.extend(reviewer_check(settings, ids.as_deref()));
    checks.push(check_sandbox(settings));
    checks.push(network_check(settings));
    checks.push(git_check());
    checks.push(checkpoints_check(crate::checkpoint::store_size(
        paths, workspace,
    )));
    checks.extend(mcp_checks(settings, paths, workspace));
    checks.push(Check::ok(
        "shell",
        format!(
            "commands run through {}",
            rune_exec::command::default_shell()
        ),
    ));
    checks.push(disk_check(
        rune_exec::disk::free_bytes(&paths.state_root).or_else(|| {
            paths
                .state_root
                .parent()
                .and_then(rune_exec::disk::free_bytes)
        }),
        directory_size(&paths.sessions_dir()),
        paths,
    ));
    checks.push(proxy_check(&|name| std::env::var(name).ok()));
    checks.push(check_certificates(
        &rune_net::roots::Roots::from_environment(),
    ));

    Report {
        version: crate::version::version_line(),
        workspace: workspace.to_string(),
        config_root: paths.config_root.to_string(),
        state_root: paths.state_root.to_string(),
        checks,
    }
}

/// What every check looks at and how to fix its common failure: the source
/// of the troubleshooting page.
#[must_use]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "read by the generated troubleshooting page")
)]
pub fn catalogue() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("version", "the running binary", "nothing to fix"),
        (
            "workspace",
            "the directory rune runs in",
            "run rune from a directory",
        ),
        (
            "state root",
            "where sessions and settings are kept",
            "make the directory writable",
        ),
        (
            "legacy state",
            "an older state layout",
            "move what you need into the state root",
        ),
        (
            "update",
            "whether a newer release exists",
            "run rune upgrade",
        ),
        (
            "clock",
            "the system clock against the network",
            "correct the system clock",
        ),
        (
            "terminal",
            "colours, keys and clipboard of this terminal",
            "use a terminal with the missing feature",
        ),
        (
            "configuration",
            "every configuration layer",
            "fix or remove the named key",
        ),
        (
            "provider",
            "the endpoint, the key and the model",
            "run rune connect",
        ),
        (
            "limits",
            "limits changed from their defaults",
            "run rune limits to see each source",
        ),
        (
            "reviewer",
            "the model auto mode reviews actions with",
            "set review_model to a served model",
        ),
        (
            "sandbox",
            "what can restrict commands on this host",
            "install the named sandbox helper",
        ),
        (
            "network",
            "whether commands may use the network",
            "set shell_network or use /network",
        ),
        (
            "git",
            "git, which undo of command changes needs",
            "install git",
        ),
        (
            "checkpoints",
            "the checkpoint store for this workspace",
            "run rune sessions prune",
        ),
        (
            "mcp",
            "every enabled MCP server",
            "fix the server or run rune mcp login",
        ),
        ("shell", "the shell commands run through", "set SHELL"),
        ("disk", "free space where sessions are kept", "free space"),
        (
            "proxy",
            "proxy variables in the environment",
            "unset a proxy that is wrong",
        ),
        (
            "certificates",
            "the certificate roots requests trust",
            "set SSL_CERT_FILE",
        ),
    ]
}

/// The terminal as its environment describes it; probing it would write to
/// a screen the report is being printed on.
fn check_terminal() -> Check {
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return Check::skipped("terminal", "standard output is not a terminal");
    }
    let env = rune_term::probe::TerminalEnv::from_lookup(|name| std::env::var(name).ok());
    Check::ok(
        "terminal",
        rune_term::probe::TerminalCaps::from_env(env).summary(),
    )
}

/// The provider: configured, reachable, accepting the key and listing the
/// model. `listed` is the model list request, when one was made.
fn provider_check(
    settings: &Settings,
    listed: Option<&rune_core::error::Result<rune_net::catalog::Catalog>>,
) -> Check {
    if !settings.provider.is_configured() {
        return Check::fail(
            "provider",
            ErrorCode::AuthenticationRequired,
            "no provider connected",
        )
        .with_hint("run rune connect");
    }
    if settings.model.trim().is_empty() {
        return Check::warn(
            "provider",
            format!("{} connected but no model is selected", settings.provider),
        )
        .with_hint("choose one from rune models");
    }
    let base_url = crate::provider_setup::configured_base_url(settings).unwrap_or_default();
    let Some(listed) = listed else {
        return Check::skipped("provider", "not checked: offline mode");
    };
    match listed {
        Err(err)
            if matches!(
                err.code(),
                ErrorCode::AuthenticationRequired | ErrorCode::PermissionDenied
            ) =>
        {
            let source = settings.api_key_env.as_deref().map_or_else(
                || "the stored credential".to_owned(),
                |name| format!("${name}"),
            );
            Check::fail(
                "provider",
                err.code(),
                format!("the endpoint refused the key from {source}"),
            )
            .with_hint("run rune connect")
        }
        Err(err) => Check::fail(
            "provider",
            err.code(),
            format!("{base_url} is not reachable: {}", err.message()),
        )
        .with_hint("check base_url, the network and proxy variables"),
        Ok(catalog)
            if !catalog
                .models
                .iter()
                .any(|model| model.id == settings.model) =>
        {
            Check::warn(
                "provider",
                format!("model {} is not listed by the endpoint", settings.model),
            )
            .with_hint("choose one from rune models")
        }
        Ok(_) => Check::ok(
            "provider",
            format!(
                "{} at {base_url}, key accepted, model {} listed",
                settings.provider, settings.model
            ),
        ),
    }
}

/// The reviewing model, in auto mode only, checked against the listed models.
fn reviewer_check(settings: &Settings, listed: Option<&[String]>) -> Option<Check> {
    if settings.permission_mode != config::PermissionMode::Auto {
        return None;
    }
    let Some(model) = crate::auto_review::reviewer_model(settings) else {
        return Some(Check::skipped("reviewer", "no model to review with"));
    };
    if listed.is_some_and(|ids| !ids.contains(&model)) {
        return Some(
            Check::warn(
                "reviewer",
                format!("auto mode reviews actions with {model}, which the endpoint does not list"),
            )
            .with_hint("set review_model to a model this endpoint serves"),
        );
    }
    Some(Check::ok(
        "reviewer",
        format!("auto mode reviews actions with {model}"),
    ))
}

/// Whether commands may use the network.
fn network_check(settings: &Settings) -> Check {
    let mut detail = if settings.shell_network {
        format!(
            "commands have network access ({})",
            settings.source_of("shell_network")
        )
    } else {
        "commands have no network access (default)".to_owned()
    };
    if settings.offline {
        detail.push_str("; offline mode");
    }
    Check::ok("network", detail)
}

/// Git, which undoing command changes relies on.
fn git_check() -> Check {
    match std::process::Command::new("git")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            Check::ok(
                "git",
                text.trim().trim_start_matches("git version ").to_owned(),
            )
        }
        _ => Check::warn(
            "git",
            "git is not installed; changes made by commands cannot be undone",
        )
        .with_hint("install git"),
    }
}

/// The checkpoint store for this workspace.
fn checkpoints_check(size: Option<u64>) -> Check {
    match size {
        None => Check::ok("checkpoints", "checkpoint store for this workspace: none yet"),
        Some(bytes) if bytes > 1024 * 1024 * 1024 => Check::warn(
            "checkpoints",
            format!("checkpoint store for this workspace: {}", readable_size(bytes)),
        )
        .with_hint(
            "old sessions' checkpoints are removed automatically above 1 GiB; run rune sessions prune to remove sessions",
        ),
        Some(bytes) => Check::ok(
            "checkpoints",
            format!("checkpoint store for this workspace: {}", readable_size(bytes)),
        ),
    }
}

/// Every enabled MCP server, connected once: one line for the servers that
/// connected and one problem per server that did not.
fn mcp_checks(settings: &Settings, paths: &Paths, workspace: &Utf8Path) -> Vec<Check> {
    let servers = match crate::mcp::doctor_servers(settings, paths, workspace) {
        Ok(servers) => servers,
        Err(err) => {
            return vec![
                Check::fail("mcp", err.code(), err.message().to_owned())
                    .with_hint("fix the [mcp_servers] configuration"),
            ];
        }
    };
    if servers.is_empty() {
        return vec![Check::ok("mcp", "no MCP servers")];
    }
    let connected: Vec<usize> = servers
        .iter()
        .filter_map(|server| server.outcome.as_ref().ok().copied())
        .collect();
    let mut checks = vec![Check::ok(
        "mcp",
        format!(
            "{} servers connected, {} tools",
            connected.len(),
            connected.iter().sum::<usize>()
        ),
    )];
    for server in &servers {
        if let Err((code, message)) = &server.outcome {
            let hint = if *code == ErrorCode::AuthenticationRequired {
                format!("run rune mcp login {}", server.name)
            } else {
                format!("run rune mcp check {} for details", server.name)
            };
            checks.push(
                Check::fail(&format!("mcp {}", server.name), *code, message.clone())
                    .with_hint(hint),
            );
        }
    }
    checks
}

/// Free space where sessions are kept, and what the sessions take.
fn disk_check(free: Option<u64>, sessions: u64, paths: &Paths) -> Check {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    let Some(free) = free else {
        return Check::skipped("disk", "the free space could not be read");
    };
    let detail = format!(
        "{} free; sessions use {}",
        readable_size(free),
        readable_size(sessions)
    );
    let fix = format!("free space; sessions are under {}", paths.state_root);
    if free < 100 * MIB {
        Check::fail("disk", ErrorCode::IoFailure, detail).with_hint(fix)
    } else if free < GIB {
        Check::warn("disk", detail).with_hint(fix)
    } else {
        Check::ok("disk", detail)
    }
}

/// The proxy variables set, with any credentials in them hidden.
fn proxy_check(env: &dyn Fn(&str) -> Option<String>) -> Check {
    let mut shown = Vec::new();
    for name in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY"] {
        for spelling in [name.to_owned(), name.to_ascii_lowercase()] {
            if let Some(value) = env(&spelling).filter(|value| !value.is_empty()) {
                shown.push(format!("{spelling}={}", hide_credentials(&value)));
            }
        }
    }
    if shown.is_empty() {
        Check::ok("proxy", "no proxy variables")
    } else {
        Check::ok("proxy", shown.join(", "))
    }
}

/// `http://user:secret@proxy:8080` becomes `http://***@proxy:8080`.
fn hide_credentials(url: &str) -> String {
    let (scheme, rest) = url
        .split_once("://")
        .map_or(("", url), |(scheme, rest)| (scheme, rest));
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    let Some((_, host)) = authority.rsplit_once('@') else {
        return url.to_owned();
    };
    let prefix = if scheme.is_empty() {
        String::new()
    } else {
        format!("{scheme}://")
    };
    format!("{prefix}***@{host}{path}")
}

/// The bytes the files under `dir` take.
fn directory_size(dir: &Utf8Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => camino::Utf8PathBuf::from_path_buf(entry.path())
                .map_or(0, |path| directory_size(&path)),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

/// A byte count in the largest whole unit: `512 B`, `3 KiB`, `12 MiB`.
fn readable_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes;
    let mut unit = 0_usize;
    while value >= 1024 && unit < 3 {
        value /= 1024;
        unit = unit.saturating_add(1);
    }
    format!("{value} {}", UNITS.get(unit).copied().unwrap_or("GiB"))
}

/// Reports which certificate roots provider and MCP requests trust.
fn check_certificates(roots: &rune_net::roots::Roots) -> Check {
    if roots.warning().is_some() {
        Check::warn("certificates", roots.describe())
    } else {
        Check::ok("certificates", roots.describe())
    }
}

/// Reports what this host can enforce for a command.
///
/// A host with no backend is reported rather than passed over: a command that
/// cannot be restricted is a fact the user needs before trusting one.
fn check_sandbox(settings: &Settings) -> Check {
    let backend = rune_exec::sandbox::detect();
    let support = backend.support();
    let guidance = rune_exec::sandbox::guidance(&support);
    sandbox_check(backend.name(), support, settings, guidance)
}

/// Describes enforcement from the backend probe: a full backend passes, a
/// missing one is a warning while commands run without it and a problem while
/// they are refused, with how to get a working backend as the hint.
fn sandbox_check(
    name: &str,
    support: rune_exec::sandbox::Support,
    settings: &Settings,
    guidance: Option<String>,
) -> Check {
    let detail = match support {
        rune_exec::sandbox::Support::Full => {
            return Check::ok(
                "sandbox",
                format!("{name} restricts every process the command starts"),
            );
        }
        rune_exec::sandbox::Support::Partial { reason } => {
            format!("{name} cannot restrict every thread: {reason}")
        }
        rune_exec::sandbox::Support::Unsupported { reason } => {
            format!("{name} cannot restrict commands here: {reason}")
        }
    };
    let check = if settings.allow_unsandboxed {
        Check::warn(
            "sandbox",
            format!(
                "commands run without a sandbox (source: {})",
                settings.source_of("allow_unsandboxed")
            ),
        )
    } else {
        Check::fail("sandbox", ErrorCode::Unsupported, detail)
    };
    check.with_hint(guidance.unwrap_or_else(|| {
        "shell commands are refused on this host; pass `--allow-unsandboxed` to run without sandbox enforcement".to_owned()
    }))
}

/// Verifies the workspace is reachable and is a directory.
fn check_workspace(workspace: &Utf8Path) -> Check {
    match std::fs::metadata(workspace) {
        Ok(meta) if meta.is_dir() => Check::ok("workspace", workspace.as_str()),
        Ok(_) => Check::fail(
            "workspace",
            ErrorCode::UnsafePath,
            format!("{workspace} is not a directory"),
        )
        .with_hint("run rune from a directory"),
        Err(err) => Check::fail(
            "workspace",
            ErrorCode::NotFound,
            format!("{workspace} could not be read: {err}"),
        ),
    }
}

/// Verifies the state root can be created with the expected permissions.
fn check_state_root(paths: &Paths) -> Check {
    match paths.ensure_roots() {
        Ok(()) => Check::ok("state root", paths.state_root.as_str()),
        Err(err) => Check::fail("state root", err.code(), err.message().to_owned())
            .with_hint(err.detail().hint.clone().unwrap_or_default()),
    }
}

/// Reports any configuration layer that produced a diagnostic.
fn check_config_layers(settings: &Settings) -> Check {
    if settings.diagnostics.is_empty() {
        return Check::ok("configuration", "every layer parsed");
    }

    let summary = settings
        .diagnostics
        .iter()
        .map(|diagnostic| {
            let key = diagnostic
                .key
                .as_deref()
                .map(|key| format!(" `{key}`"))
                .unwrap_or_default();
            format!("{}{}: {}", diagnostic.layer, key, diagnostic.message)
        })
        .collect::<Vec<_>>()
        .join("; ");

    let first = settings.diagnostics.first().map(|d| d.code);
    let hint = settings
        .diagnostics
        .iter()
        .find_map(|d| d.hint.clone())
        .unwrap_or_else(|| "run `rune config --explain` for the resolved values".to_owned());

    Check {
        name: "configuration".to_owned(),
        outcome: Outcome::Warn,
        detail: summary,
        hint: Some(hint),
        code: first,
    }
}

/// Reports limits that were overridden away from their defaults.
fn check_limits(settings: &Settings) -> Check {
    let overridden: Vec<String> = LimitName::all()
        .iter()
        .filter_map(|name| {
            settings
                .limits
                .source(*name)
                .map(|layer| format!("{}={} ({layer})", name.as_str(), settings.limits.get(*name)))
        })
        .collect();

    if overridden.is_empty() {
        Check::ok("limits", "every limit is at its default")
    } else {
        Check::ok("limits", format!("{} overridden", overridden.len()))
    }
}

/// Reports whether a legacy state directory exists.
fn check_legacy_layout() -> Check {
    let home = std::env::var("HOME").ok();
    let legacy = Paths::legacy_dir(home.as_deref());
    if legacy.as_std_path().exists() {
        Check::warn("legacy state", format!("{legacy} exists and is not read"))
            .with_hint("move any state you need into the current state root")
    } else {
        Check::ok("legacy state", "no earlier layout found")
    }
}

/// Renders `status --json`.
#[must_use]
pub fn status_json(settings: &Settings, paths: &Paths, workspace: &Utf8Path) -> serde_json::Value {
    let mut value = config::to_status_json(settings, workspace);
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "version".to_owned(),
            serde_json::Value::String(crate::version::VERSION.to_owned()),
        );
        object.insert(
            "channel".to_owned(),
            serde_json::Value::String(crate::version::CHANNEL.to_owned()),
        );
        object.insert(
            "config_root".to_owned(),
            serde_json::Value::String(paths.config_root.to_string()),
        );
        object.insert(
            "state_root".to_owned(),
            serde_json::Value::String(paths.state_root.to_string()),
        );
        object.insert(
            "data_root".to_owned(),
            serde_json::Value::String(paths.data_root.to_string()),
        );
        object.insert(
            "provider_connected".to_owned(),
            serde_json::Value::Bool(settings.provider.is_configured()),
        );
        let sandbox = crate::sandbox::status(settings);
        object.insert(
            "sandbox".to_owned(),
            serde_json::json!({"backend": sandbox.backend, "state": sandbox.state}),
        );
    }
    value
}

/// Renders the limits table as JSON.
#[must_use]
pub fn limits_json(settings: &Settings) -> serde_json::Value {
    serde_json::json!({ "limits": settings.limits.describe() })
}

/// Renders the limits table as text.
#[must_use]
pub fn limits_text(settings: &Settings) -> String {
    let mut out = String::from("limit                             value        unit     source\n");
    for row in settings.limits.describe() {
        let _ = writeln!(
            out,
            "{:<33} {:<12} {:<8} {}",
            row.name.as_str(),
            row.value,
            row.unit.suffix(),
            row.source.map_or("default", |layer| layer.as_str()),
        );
    }
    out
}

/// Renders the resolved configuration as text.
#[must_use]
pub fn config_text(settings: &Settings) -> String {
    let mut out =
        String::from("key                               value                source       help\n");
    for row in settings.explain() {
        let _ = writeln!(
            out,
            "{:<33} {:<20} {:<12} {}",
            row.key, row.value, row.source, row.help
        );
    }
    out
}

/// Renders the configuration as JSON.
#[must_use]
pub fn config_json(settings: &Settings) -> serde_json::Value {
    serde_json::json!({
        "values": settings.explain(),
        "diagnostics": settings.diagnostics,
        "layers": {
            "default": Layer::Default.as_str(),
            "user": Layer::User.as_str(),
            "project": Layer::Project.as_str(),
            "environment": Layer::Environment.as_str(),
            "command_line": Layer::CommandLine.as_str(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::config::EnvironmentOverrides;
    use tempfile::TempDir;

    fn paths_for(dir: &TempDir) -> Paths {
        Paths::resolve(
            Some(dir.path().to_str().unwrap_or("/tmp")),
            None,
            None,
            None,
            None,
        )
    }

    #[test]
    fn a_clean_setup_has_no_unexpected_problems() {
        let dir = TempDir::new().expect("tempdir");
        let workspace = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths_for(&dir);
        let settings = config::load(None, None, &EnvironmentOverrides::default());
        let report = run(&settings, &paths, workspace);
        // Only the missing provider, and a missing sandbox helper on a host
        // without one, are problems on a clean setup.
        for check in &report.checks {
            if check.outcome == Outcome::Fail {
                assert!(
                    matches!(check.name.as_str(), "provider" | "sandbox"),
                    "{}",
                    report.render()
                );
            }
        }
    }

    #[test]
    fn the_header_names_the_program_once() {
        let report = Report {
            version: crate::version::version_line(),
            workspace: "/work".to_owned(),
            config_root: "/config".to_owned(),
            state_root: "/state".to_owned(),
            checks: Vec::new(),
        };
        let expected = format!(
            "{}\nworkspace /work\nconfig    /config\nstate     /state\n\n",
            crate::version::version_line()
        );
        assert_eq!(report.render(), expected);
        assert!(!report.render().starts_with("rune rune"));
    }

    #[test]
    fn report_names_a_missing_workspace() {
        let dir = TempDir::new().expect("tempdir");
        let missing = Utf8Path::from_path(dir.path())
            .expect("utf8")
            .join("does-not-exist");
        let paths = paths_for(&dir);
        let settings = config::load(None, None, &EnvironmentOverrides::default());
        let report = run(&settings, &paths, &missing);
        assert_eq!(report.exit_code(), 1);
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "workspace")
            .expect("workspace check");
        assert_eq!(check.outcome, Outcome::Fail);
        assert!(check.code.is_some());
    }

    #[test]
    fn partial_and_unsupported_sandboxes_name_the_refusal_and_override() {
        let refusing = Settings {
            allow_unsandboxed: false,
            ..Settings::default()
        };
        for support in [
            rune_exec::sandbox::Support::Partial {
                reason: "cannot restrict every thread".to_owned(),
            },
            rune_exec::sandbox::Support::Unsupported {
                reason: "bwrap is not on PATH".to_owned(),
            },
        ] {
            let check = sandbox_check("namespaces", support, &refusing, None);
            assert_eq!(check.outcome, Outcome::Fail);
            let hint = check.hint.expect("refusal and override");
            assert!(
                hint.contains("shell commands are refused on this host"),
                "{hint}"
            );
            assert!(hint.contains("--allow-unsandboxed"), "{hint}");
            assert!(!hint.contains("commands run unrestricted"), "{hint}");
        }
    }

    #[test]
    fn the_sandbox_check_names_the_override_source_and_the_guidance() {
        let missing = || rune_exec::sandbox::Support::Unsupported {
            reason: "the bubblewrap helper (bwrap) is not installed".to_owned(),
        };
        let guidance = || Some("install bubblewrap: sudo apt-get install bubblewrap".to_owned());
        let full = sandbox_check(
            "namespaces",
            rune_exec::sandbox::Support::Full,
            &Settings::default(),
            None,
        );
        assert_eq!(full.outcome, Outcome::Ok);
        assert!(full.detail.contains("namespaces"), "{}", full.detail);
        let mut allowed = Settings {
            allow_unsandboxed: true,
            ..Settings::default()
        };
        allowed.sources.record("allow_unsandboxed", Layer::User);
        let warned = sandbox_check("namespaces", missing(), &allowed, guidance());
        assert_eq!(warned.outcome, Outcome::Warn);
        assert_eq!(
            warned.detail,
            "commands run without a sandbox (source: user)"
        );
        assert_eq!(warned.hint, guidance());
        let refused = Settings {
            allow_unsandboxed: false,
            ..Settings::default()
        };
        let failed = sandbox_check("namespaces", missing(), &refused, guidance());
        assert_eq!(failed.outcome, Outcome::Fail);
        assert!(
            failed.detail.contains("bwrap) is not installed"),
            "{}",
            failed.detail
        );
        assert_eq!(failed.hint, guidance());
    }

    #[test]
    fn the_certificate_line_names_the_roots_in_use() {
        use rune_net::roots::Roots;

        let built_in = check_certificates(&Roots::from_bundles(None, None));
        assert_eq!(built_in.outcome, Outcome::Ok);
        assert_eq!(built_in.detail, "built-in roots");
        let unusable = check_certificates(&Roots::from_bundles(Some(Ok(b"x".to_vec())), None));
        assert_eq!(unusable.outcome, Outcome::Warn);
        assert_eq!(
            unusable.detail,
            "SSL_CERT_FILE holds no certificates; using the built-in certificate roots"
        );
    }

    #[test]
    fn an_unconfigured_provider_is_a_problem() {
        let settings = config::load(None, None, &EnvironmentOverrides::default());
        let check = provider_check(&settings, None);
        assert_eq!(check.outcome, Outcome::Fail);
        assert_eq!(check.code, Some(ErrorCode::AuthenticationRequired));
        assert!(
            check
                .hint
                .as_deref()
                .expect("hint")
                .contains("rune connect")
        );
    }

    #[test]
    fn configured_provider_without_model_warns() {
        let settings = Settings {
            provider: config::Provider::Anthropic,
            ..Settings::default()
        };
        let check = provider_check(&settings, None);
        assert_eq!(check.outcome, Outcome::Warn);
        assert!(check.hint.is_some());
    }

    #[test]
    fn markers_read_ok_warning_problem_skipped() {
        let report = Report {
            version: "v".to_owned(),
            workspace: "/w".to_owned(),
            config_root: "/c".to_owned(),
            state_root: "/s".to_owned(),
            checks: vec![
                Check::ok("git", "2.47"),
                Check::warn("disk", "low").with_hint("free space"),
                Check::fail("provider", ErrorCode::AuthenticationRequired, "refused"),
                Check::skipped("update", "not checked"),
            ],
        };
        let text = report.render();
        assert!(text.contains("ok      git            2.47\n"), "{text}");
        assert!(
            text.contains("warning disk           low\n        fix: free space\n"),
            "{text}"
        );
        assert!(text.contains("problem provider       refused\n"), "{text}");
        assert!(
            text.contains("skipped update         not checked\n"),
            "{text}"
        );
        assert_eq!(report.exit_code(), 1);
        let json = serde_json::to_value(&report).expect("json");
        let outcomes: Vec<&str> = json["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .map(|check| check["outcome"].as_str().expect("outcome"))
            .collect();
        assert_eq!(outcomes, ["ok", "warn", "fail", "unknown"]);
    }

    #[test]
    fn proxy_credentials_are_hidden() {
        let env = |name: &str| match name {
            "HTTPS_PROXY" => Some("http://user:secret@proxy:8080".to_owned()),
            "no_proxy" => Some("localhost".to_owned()),
            _ => None,
        };
        let check = proxy_check(&env);
        assert_eq!(
            check.detail,
            "HTTPS_PROXY=http://***@proxy:8080, no_proxy=localhost"
        );
        assert!(!check.detail.contains("secret"));
        assert_eq!(proxy_check(&|_| None).detail, "no proxy variables");
        assert_eq!(hide_credentials("proxy:3128"), "proxy:3128");
    }

    #[test]
    fn disk_thresholds() {
        const MIB: u64 = 1024 * 1024;
        let paths = Paths::resolve(None, None, None, None, Some("/state"));
        assert_eq!(disk_check(Some(2048 * MIB), 0, &paths).outcome, Outcome::Ok);
        assert_eq!(
            disk_check(Some(500 * MIB), 0, &paths).outcome,
            Outcome::Warn
        );
        let full = disk_check(Some(50 * MIB), 3 * MIB, &paths);
        assert_eq!(full.outcome, Outcome::Fail);
        assert_eq!(full.detail, "50 MiB free; sessions use 3 MiB");
        assert!(
            full.hint
                .as_deref()
                .is_some_and(|hint| hint.contains("sessions are under"))
        );
        assert_eq!(disk_check(None, 0, &paths).outcome, Outcome::Unknown);
        assert_eq!(
            checkpoints_check(None).detail,
            "checkpoint store for this workspace: none yet"
        );
        assert_eq!(
            checkpoints_check(Some(2 * 1024 * MIB)).outcome,
            Outcome::Warn
        );
    }

    #[test]
    fn catalogue_covers_every_check_name() {
        let dir = TempDir::new().expect("tempdir");
        let workspace = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths_for(&dir);
        let settings = config::load(None, None, &EnvironmentOverrides::default());
        let report = run(&settings, &paths, workspace);
        let names: Vec<&str> = catalogue().iter().map(|(name, _, _)| *name).collect();
        for check in &report.checks {
            assert!(
                names.contains(&check.name.as_str()),
                "{} is not catalogued",
                check.name
            );
        }
    }

    #[test]
    fn configuration_diagnostics_become_a_warning_with_a_hint() {
        let dir = TempDir::new().expect("tempdir");
        let bad = Utf8Path::from_path(dir.path())
            .expect("utf8")
            .join("config.toml");
        std::fs::write(&bad, "not valid =").expect("write");
        let settings = config::load(None, Some(&bad), &EnvironmentOverrides::default());
        let check = check_config_layers(&settings);
        assert_eq!(check.outcome, Outcome::Warn);
        assert!(check.hint.is_some());
        assert!(check.code.is_some());
    }

    #[test]
    fn limits_json_includes_every_limit() {
        let settings = Settings::default();
        let json = limits_json(&settings);
        let list = json["limits"].as_array().expect("array");
        assert_eq!(list.len(), LimitName::all().len());
        for row in list {
            assert!(row["name"].is_string());
            assert!(row["description"].is_string());
            assert!(row["min"].is_number());
        }
    }

    #[test]
    fn limits_text_lists_every_limit_with_its_source() {
        let settings = Settings::default();
        let text = limits_text(&settings);
        for name in LimitName::all() {
            assert!(text.contains(name.as_str()), "missing {name:?}");
        }
        assert!(text.contains("default"));
    }

    #[test]
    fn config_json_reports_the_layer_order() {
        let settings = Settings::default();
        let json = config_json(&settings);
        assert_eq!(json["layers"]["command_line"], "command_line");
        assert!(json["values"].is_array());
    }

    #[test]
    fn config_text_lists_every_limit_key() {
        let settings = Settings::default();
        let text = config_text(&settings);
        for name in LimitName::all() {
            assert!(text.contains(name.as_str()), "missing {name:?}");
        }
    }

    #[test]
    fn status_json_marks_the_provider_as_not_connected() {
        let dir = TempDir::new().expect("tempdir");
        let workspace = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths_for(&dir);
        let settings = Settings::default();
        let json = status_json(&settings, &paths, workspace);
        assert_eq!(json["provider_connected"], false);
        assert_eq!(json["provider"], "unconfigured");
        assert!(json["version"].is_string());
        assert!(json["state_root"].is_string());
    }
}
