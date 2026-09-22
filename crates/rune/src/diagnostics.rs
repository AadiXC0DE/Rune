//! Diagnostic commands.
//!
//! These answer "why is this not working" without starting a model turn and
//! without opening a network connection. Every check emits a stable code and a
//! marker indicating whether it passed, warned, or failed.

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
            "rune {} ({})\nworkspace {}\nconfig {}\nstate  {}\n\n",
            self.version, self.workspace, self.workspace, self.config_root, self.state_root
        );
        for check in &self.checks {
            let marker = match check.outcome {
                Outcome::Ok => "ok  ",
                Outcome::Warn => "warn",
                Outcome::Fail => "fail",
                Outcome::Unknown => "?   ",
            };
            let _ = writeln!(out, "{marker} {:<24} {}", check.name, check.detail);
            if let Some(hint) = &check.hint {
                let _ = writeln!(out, "     {:24} {hint}", "");
            }
        }
        out
    }
}

/// Runs every local check.
///
/// Performs no network access and starts no model turn, so it completes even
/// when the machine is offline.
#[must_use]
pub fn run(settings: &Settings, paths: &Paths, workspace: &Utf8Path) -> Report {
    let checks = vec![
        check_workspace(workspace),
        check_state_root(paths),
        check_config_layers(settings),
        check_provider(settings),
        check_limits(settings),
        check_legacy_layout(),
        check_network(settings),
        check_sandbox(),
    ];

    Report {
        version: crate::version::version_line(),
        workspace: workspace.to_string(),
        config_root: paths.config_root.to_string(),
        state_root: paths.state_root.to_string(),
        checks,
    }
}

/// Reports what this host can enforce for a command.
///
/// A host with no backend is reported rather than passed over: a command that
/// cannot be restricted is a fact the user needs before trusting one.
fn check_sandbox() -> Check {
    let backend = rune_exec::sandbox::detect();
    let name = backend.name();
    match backend.support() {
        rune_exec::sandbox::Support::Full => Check::ok(
            "sandbox",
            format!("{name} restricts every process the command starts"),
        ),
        rune_exec::sandbox::Support::Partial { reason } => Check::warn(
            "sandbox",
            format!("{name} cannot restrict every thread: {reason}"),
        )
        .with_hint("commands run without full enforcement; pass the override to accept that"),
        rune_exec::sandbox::Support::Unsupported { reason } => Check::warn(
            "sandbox",
            format!("{name} cannot restrict commands here: {reason}"),
        )
        .with_hint("commands run unrestricted; an explicit override is required to run one"),
    }
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

/// Reports the provider state without contacting it.
fn check_provider(settings: &Settings) -> Check {
    if !settings.provider.is_configured() {
        return Check {
            name: "provider".to_owned(),
            outcome: Outcome::Unknown,
            detail: "no provider connected".to_owned(),
            hint: Some("run `rune connect` to choose an endpoint".to_owned()),
            code: Some(ErrorCode::AuthenticationRequired),
        };
    }

    if settings.model.trim().is_empty() {
        return Check::warn(
            "provider",
            format!("{} connected but no model is selected", settings.provider),
        )
        .with_hint("set `model` in the user config, or pass `--model <id>`");
    }

    Check::ok(
        "provider",
        format!("{} with model {}", settings.provider, settings.model),
    )
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

/// Reports the network policy in effect.
fn check_network(settings: &Settings) -> Check {
    if !settings.provider.is_configured() {
        return Check {
            name: "network".to_owned(),
            outcome: Outcome::Unknown,
            detail: "not checked, no provider connected".to_owned(),
            hint: None,
            code: None,
        };
    }
    Check {
        name: "network".to_owned(),
        outcome: Outcome::Unknown,
        detail: "not checked, run a request to verify connectivity".to_owned(),
        hint: None,
        code: None,
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
    let mut out = String::from("key                               value                source\n");
    for row in settings.explain() {
        let _ = writeln!(out, "{:<33} {:<20} {}", row.key, row.value, row.source);
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
    fn report_passes_on_a_clean_setup() {
        let dir = TempDir::new().expect("tempdir");
        let workspace = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths_for(&dir);
        let settings = config::load(None, None, &EnvironmentOverrides::default());
        let report = run(&settings, &paths, workspace);
        assert_eq!(report.exit_code(), 0, "{}", report.render());
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
    fn unconfigured_provider_reports_unknown_not_failure() {
        let settings = config::load(None, None, &EnvironmentOverrides::default());
        let check = check_provider(&settings);
        assert_eq!(check.outcome, Outcome::Unknown);
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
        let check = check_provider(&settings);
        assert_eq!(check.outcome, Outcome::Warn);
        assert!(check.hint.is_some());
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
