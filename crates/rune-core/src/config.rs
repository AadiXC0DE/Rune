//! Layered configuration.
//!
//! Configuration resolves from five layers, highest wins:
//!
//! 1. a command-line flag for this process
//! 2. a `RUNE_*` environment variable
//! 3. the project file `.rune.toml`
//! 4. the user file `config.toml`
//! 5. the compiled default
//!
//! Project files are committed to version control, so they accept only keys that
//! are safe for a repository to set. A profile-owned key found in a project file
//! is ignored and reported, never applied. Every resolution records which layer
//! supplied the value so `rune config --explain` can show it.

use std::collections::BTreeMap;
use std::fmt;

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::budget::{Budget, BudgetSet, LimitName};
use crate::error::{ErrorCode, Result, RuneError};

/// Largest accepted configuration file.
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Source of a configuration value, ordered from lowest to highest precedence.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// Compiled default.
    Default,
    /// The user configuration file.
    User,
    /// The project configuration file.
    Project,
    /// A process environment variable.
    Environment,
    /// A command-line flag.
    CommandLine,
}

impl Layer {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::User => "user",
            Self::Project => "project",
            Self::Environment => "environment",
            Self::CommandLine => "command_line",
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Effective permission mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Prompt before unresolved sensitive calls.
    Ask,
    /// Apply rules, then automatically review unresolved calls.
    #[default]
    Auto,
    /// Disable permission checks.
    #[serde(rename = "full_access", alias = "full-access", alias = "yolo")]
    FullAccess,
}

impl PermissionMode {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::FullAccess => "full_access",
        }
    }

    /// Returns the label shown in the interface.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::FullAccess => "full access",
        }
    }

    /// Parses a mode written by a user.
    ///
    /// Accepts the canonical spelling, the hyphenated form, and the legacy
    /// name, because all three appear in existing configuration.
    #[must_use]
    pub fn from_name(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ask" => Some(Self::Ask),
            "auto" => Some(Self::Auto),
            "full-access" | "full_access" | "fullaccess" | "yolo" => Some(Self::FullAccess),
            _ => None,
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Reasoning effort requested from the model.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// Let the provider decide.
    #[default]
    Auto,
    /// No extended reasoning.
    None,
    /// Smallest reasoning budget.
    Minimal,
    /// Low reasoning budget.
    Low,
    /// Medium reasoning budget.
    Medium,
    /// High reasoning budget.
    High,
    /// Very high reasoning budget.
    Xhigh,
    /// Largest reasoning budget.
    Max,
}

impl Effort {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Provider identity.
///
/// A fresh install has no provider. Every command that needs one fails with
/// `authentication_required` naming how to connect one, rather than silently
/// selecting an endpoint and implying an account the product does not require.
/// Serialized as a bare name, and read back from any name.
///
/// The named variant holds a string, so the derived representation would be a
/// table that no hand-written configuration uses and that a provider connected
/// by name could not be read back from. This is a plain string in both
/// directions: a name that is not one of the built-ins is the named variant,
/// which is what connecting a provider by name writes.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Provider {
    /// No provider has been connected.
    #[default]
    Unconfigured,
    /// An OpenAI-compatible Chat Completions endpoint.
    ChatCompletions,
    /// An OpenAI Responses endpoint.
    Responses,
    /// An Anthropic Messages endpoint.
    Anthropic,
    /// A named endpoint declared in the user configuration.
    Named(String),
}

impl Serialize for Provider {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Provider {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        // Every name is accepted, because a provider the user named themselves
        // is a legitimate value. Refusing an unknown name here would mean a
        // connection could be written and never read back.
        let raw = String::deserialize(deserializer)?;
        Ok(parse_provider(&raw))
    }
}

impl Provider {
    /// Returns true when a provider has been connected.
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        !matches!(self, Self::Unconfigured)
    }

    /// Returns the wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Unconfigured => "unconfigured",
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::Anthropic => "anthropic",
            Self::Named(name) => name.as_str(),
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A configuration file as parsed, before merging.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Sandbox policy applied to command execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// Maximum model tool-loop steps; zero means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_steps: Option<u64>,
    /// Bytes retained from one tool result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_result_bytes: Option<u64>,
    /// Whether project instructions and workspace context are loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<bool>,
    /// Preferred upstream providers, in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_order: Option<Vec<String>>,
    /// Whether to restrict requests to the listed providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_strict: Option<bool>,
    /// Limit overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<BTreeMap<String, Budget>>,
}

/// Input capacity assumed when a model does not declare one.
///
/// Deliberately modest: an underestimated window triggers compaction early,
/// which costs a summary request, while an overestimated one produces a
/// rejected request that costs the whole turn. A model with a larger window
/// declares it in the `models` table rather than relying on this.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// One entry in the `models` table.
///
/// Accepts either a bare identifier, which is what most configurations need, or
/// a table naming the identifier and what the model accepts. The endpoint's own
/// listing carries no capacity for most compatible servers, so the window a
/// session budgets against has to be stated somewhere, and a configuration file
/// is the only place that knows it.
#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ModelEntry {
    /// Just the identifier.
    Id(String),
    /// The identifier with what the model accepts.
    Detailed(Box<ModelSettings>),
}

impl ModelEntry {
    /// Returns the model identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Id(id) => id.as_str(),
            Self::Detailed(settings) => settings.id.as_str(),
        }
    }

    /// Returns the declared input capacity, when one is stated.
    #[must_use]
    pub fn context_window(&self) -> Option<u64> {
        match self {
            Self::Id(_) => None,
            Self::Detailed(settings) => settings.context_window,
        }
    }
}

/// What a model accepts, as declared in a configuration file.
#[derive(Clone, PartialEq, Eq, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettings {
    /// Identifier sent to the endpoint.
    pub id: String,
    /// Input capacity in tokens.
    ///
    /// Declared rather than assumed: a wrong window either truncates a
    /// conversation that would have fit or lets one grow past what the model
    /// accepts, and both are worse than being told.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Largest answer the model will produce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

/// A user configuration file as parsed.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    /// Active provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,

    /// Model per provider, either a bare identifier or a table describing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<BTreeMap<String, ModelEntry>>,

    /// Endpoint for the active provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// Environment variable holding the credential, if not the platform default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,

    /// Effective permission mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,

    /// Reasoning effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,

    /// Whether fast mode is requested where supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_mode: Option<bool>,

    /// Theme name or a light or dark pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,

    /// Whether automatic update checks run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_upgrade: Option<bool>,

    /// Whether tool call groups are collapsed in the transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collapse_tool_calls: Option<bool>,

    /// Whether session titles are generated from the first prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_titles: Option<bool>,

    /// Whether project context is loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<bool>,

    /// Additional workspace roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<Utf8PathBuf>>,

    /// Limit overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<BTreeMap<String, Budget>>,

    /// Permission rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<toml::Value>,

    /// Model used for automatic permission review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_model: Option<String>,

    /// Preferred upstream providers, in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_order: Option<Vec<String>>,

    /// Whether to restrict requests to the listed providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_strict: Option<bool>,
}

/// A diagnostic produced while loading configuration.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Layer the diagnostic applies to.
    pub layer: Layer,
    /// Path of the file, when the diagnostic concerns a file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Utf8PathBuf>,
    /// Stable code.
    pub code: ErrorCode,
    /// Key at fault, when one applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Human-readable explanation.
    pub message: String,
    /// Repair hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Diagnostic {
    fn new(layer: Layer, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            layer,
            path: None,
            code,
            key: None,
            message: message.into(),
            hint: None,
        }
    }

    fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    fn with_path(mut self, path: &Utf8Path) -> Self {
        self.path = Some(path.to_owned());
        self
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Where each effective value came from.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MergeReport {
    /// Layer supplying each key, keyed by configuration key.
    pub sources: BTreeMap<String, Layer>,
}

impl MergeReport {
    /// Records that a key came from a layer.
    pub fn record(&mut self, key: impl Into<String>, layer: Layer) {
        self.sources.insert(key.into(), layer);
    }

    /// Returns the layer for a key, defaulting to the compiled default.
    #[must_use]
    pub fn source(&self, key: &str) -> Layer {
        self.sources.get(key).copied().unwrap_or(Layer::Default)
    }
}

/// Fully resolved configuration.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Active provider.
    pub provider: Provider,
    /// Effective model identifier for the active provider.
    pub model: String,
    /// Input capacity the configured model accepts, when one is declared.
    ///
    /// Held here rather than looked up at the point it is needed, because the
    /// only place that knows it is the configuration file, and the session that
    /// budgets against it has no access to the raw file.
    pub context_window: Option<u64>,
    /// Endpoint base URL.
    pub base_url: Option<String>,
    /// Environment variable holding the credential.
    pub api_key_env: Option<String>,
    /// Effective permission mode.
    pub permission_mode: PermissionMode,
    /// Whether a command may run without a sandbox.
    ///
    /// Off by default. A host with no usable backend refuses commands rather
    /// than running them unrestricted, so turning this off removes the only way
    /// to run anything on such a host.
    pub allow_unsandboxed: bool,
    /// Reasoning effort.
    pub effort: Effort,
    /// Whether fast mode is requested.
    pub fast_mode: bool,
    /// Theme selection.
    pub theme: Option<String>,
    /// Whether automatic update checks run.
    pub auto_upgrade: bool,
    /// Whether every outbound request is refused.
    pub offline: bool,
    /// Whether tool call groups are collapsed.
    pub collapse_tool_calls: bool,
    /// Whether session titles are generated.
    pub session_titles: bool,
    /// Whether project context is loaded.
    pub context: bool,
    /// Additional workspace roots.
    pub additional_directories: Vec<Utf8PathBuf>,
    /// Effective limits.
    pub limits: BudgetSet,
    /// Preferred upstream providers.
    pub provider_order: Vec<String>,
    /// Whether to restrict requests to the listed providers.
    pub provider_strict: bool,
    /// Model used for automatic permission review.
    pub review_model: Option<String>,
    /// Where each value came from.
    pub sources: MergeReport,
    /// Diagnostics produced while loading.
    pub diagnostics: Vec<Diagnostic>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            provider: Provider::default(),
            model: String::new(),
            context_window: None,
            base_url: None,
            api_key_env: None,
            permission_mode: PermissionMode::default(),
            allow_unsandboxed: false,
            effort: Effort::default(),
            fast_mode: false,
            theme: None,
            auto_upgrade: true,
            offline: false,
            collapse_tool_calls: false,
            session_titles: true,
            context: true,
            additional_directories: Vec::new(),
            limits: BudgetSet::new(),
            provider_order: Vec::new(),
            provider_strict: false,
            review_model: None,
            sources: MergeReport::default(),
            diagnostics: Vec::new(),
        }
    }
}

impl Settings {
    /// Returns true when any layer produced a diagnostic.
    #[must_use]
    pub fn has_diagnostics(&self) -> bool {
        !self.diagnostics.is_empty()
    }

    /// Returns the layer that supplied a key.
    #[must_use]
    pub fn source_of(&self, key: &str) -> Layer {
        self.sources.source(key)
    }

    /// Returns an error when a model request could not be built.
    ///
    /// Checked before any network call so the failure is immediate and names
    /// exactly what is missing, rather than surfacing as a transport error.
    pub fn require_model(&self) -> Result<()> {
        if !self.provider.is_configured() {
            return Err(unconfigured_provider_error());
        }
        if self.model.trim().is_empty() {
            return Err(RuneError::new(
                ErrorCode::InvalidConfiguration,
                format!("no model is selected for provider `{}`", self.provider),
            )
            .with_hint("set `model` in the user config, or pass `--model <id>`"));
        }
        Ok(())
    }

    /// Returns every key with its effective value and source, for `--explain`.
    #[must_use]
    pub fn explain(&self) -> Vec<Explained> {
        let rows = vec![
            ("provider", self.provider.to_string()),
            ("model", self.model.clone()),
            (
                "context_window",
                self.context_window
                    .unwrap_or(DEFAULT_CONTEXT_WINDOW)
                    .to_string(),
            ),
            (
                "base_url",
                self.base_url
                    .clone()
                    .unwrap_or_else(|| "default".to_owned()),
            ),
            (
                "api_key_env",
                self.api_key_env
                    .clone()
                    .unwrap_or_else(|| "default".to_owned()),
            ),
            ("permission_mode", self.permission_mode.to_string()),
            ("effort", self.effort.to_string()),
            ("fast_mode", self.fast_mode.to_string()),
            (
                "theme",
                self.theme.clone().unwrap_or_else(|| "auto".to_owned()),
            ),
            ("auto_upgrade", self.auto_upgrade.to_string()),
            ("collapse_tool_calls", self.collapse_tool_calls.to_string()),
            ("session_titles", self.session_titles.to_string()),
            ("context", self.context.to_string()),
            ("provider_strict", self.provider_strict.to_string()),
        ];

        let mut out: Vec<Explained> = rows
            .into_iter()
            .map(|(key, value)| Explained {
                key: key.to_owned(),
                value,
                source: self.sources.source(key),
            })
            .collect();

        for name in LimitName::all() {
            out.push(Explained {
                key: name.as_str().to_owned(),
                value: self.limits.get(*name).to_string(),
                source: self.limits.source(*name).unwrap_or(Layer::Default),
            });
        }

        out
    }
}

/// One row of `rune config --explain`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Explained {
    /// Configuration key.
    pub key: String,
    /// Effective value, rendered for display.
    pub value: String,
    /// Layer that supplied it.
    pub source: Layer,
}

/// Environment overrides read for this process.
#[derive(Clone, Debug, Default)]
pub struct EnvironmentOverrides {
    /// Provider selection.
    pub provider: Option<String>,
    /// Model selection.
    pub model: Option<String>,
    /// Endpoint base URL.
    pub base_url: Option<String>,
    /// Credential environment variable.
    pub api_key_env: Option<String>,
    /// Permission mode.
    pub permission_mode: Option<String>,
    /// Reasoning effort.
    pub effort: Option<String>,
    /// Fast mode.
    pub fast_mode: Option<bool>,
    /// Theme.
    pub theme: Option<String>,
    /// Automatic updates.
    pub auto_upgrade: Option<bool>,
    /// Additional directories.
    pub additional_directories: Vec<Utf8PathBuf>,
    /// Limit overrides in `name=value` form.
    pub limits: Vec<String>,
    /// Provider order.
    pub provider_order: Option<String>,
    /// Whether to restrict to the listed providers.
    pub provider_strict: Option<bool>,
    /// Review model.
    pub review_model: Option<String>,
    /// Offline mode.
    pub offline: Option<bool>,
}

impl EnvironmentOverrides {
    /// Reads the supported variables from the process environment.
    #[must_use]
    pub fn from_process() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Reads the supported variables from an arbitrary lookup.
    ///
    /// Exists so tests can exercise the mapping without touching the process
    /// environment, which is not thread safe to mutate.
    #[must_use]
    pub fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Self {
        let boolean = |lookup: &mut dyn FnMut(&str) -> Option<String>, key: &str| {
            lookup(key).and_then(|value| parse_bool(&value))
        };

        let mut out = Self {
            provider: lookup("RUNE_PROVIDER"),
            model: lookup("RUNE_MODEL"),
            base_url: lookup("RUNE_BASE_URL"),
            api_key_env: lookup("RUNE_API_KEY_ENV"),
            permission_mode: lookup("RUNE_PERMISSION_MODE"),
            effort: lookup("RUNE_EFFORT"),
            fast_mode: boolean(&mut lookup, "RUNE_FAST_MODE"),
            theme: lookup("RUNE_THEME"),
            auto_upgrade: boolean(&mut lookup, "RUNE_AUTO_UPGRADE"),
            additional_directories: Vec::new(),
            limits: Vec::new(),
            provider_order: lookup("RUNE_PROVIDER_ORDER"),
            provider_strict: boolean(&mut lookup, "RUNE_PROVIDER_STRICT"),
            review_model: lookup("RUNE_REVIEW_MODEL"),
            offline: boolean(&mut lookup, "RUNE_OFFLINE"),
        };

        if let Some(list) = lookup("RUNE_ADDITIONAL_DIRS") {
            out.additional_directories = list
                .split(':')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(Utf8PathBuf::from)
                .collect();
        }

        if let Some(list) = lookup("RUNE_LIMITS") {
            out.limits = list
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect();
        }

        out
    }
}

/// Interpretations accepted for a boolean environment variable.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// Keys accepted in a project configuration file.
///
/// Any other known key found in a project file is ignored with a diagnostic,
/// because a repository can be changed by anyone who can open a pull request.
const PROJECT_SAFE_KEYS: &[&str] = &[
    "sandbox",
    "max_agent_steps",
    "max_tool_result_bytes",
    "context",
    "provider_order",
    "provider_strict",
    "limits",
];

/// Profile-owned keys. Present in a project file, they are ignored and reported.
const PROFILE_ONLY_KEYS: &[&str] = &[
    "provider",
    "models",
    "model",
    "base_url",
    "api_key_env",
    "permission_mode",
    "permission",
    "effort",
    "fast_mode",
    "theme",
    "auto_upgrade",
    "collapse_tool_calls",
    "session_titles",
    "additional_directories",
    "review_model",
];

/// Loads and merges every configuration layer.
///
/// Missing files are not an error. A malformed file produces a diagnostic and
/// that layer is skipped, so a broken project file cannot prevent startup.
#[must_use = "the result carries diagnostics that must be surfaced"]
pub fn load(
    project_path: Option<&Utf8Path>,
    user_path: Option<&Utf8Path>,
    env: &EnvironmentOverrides,
) -> Settings {
    let mut settings = Settings::default();

    if let Some(path) = user_path {
        match read_user(path) {
            Ok(Some(user)) => apply_user(&mut settings, &user, Layer::User),
            Ok(None) => {}
            Err(diagnostic) => settings.diagnostics.push(diagnostic),
        }
    }

    if let Some(path) = project_path {
        match read_project(path) {
            Ok(Some((project, diagnostics))) => {
                settings.diagnostics.extend(diagnostics);
                apply_project(&mut settings, &project, Layer::Project);
            }
            Ok(None) => {}
            Err(diagnostic) => settings.diagnostics.push(diagnostic),
        }
    }

    apply_environment(&mut settings, env);

    settings
}

/// Reads the user configuration file.
fn read_user(path: &Utf8Path) -> std::result::Result<Option<UserConfig>, Diagnostic> {
    let Some(text) = read_bounded(path, Layer::User)? else {
        return Ok(None);
    };
    let config: UserConfig = toml::from_str(&text).map_err(|err| {
        Diagnostic::new(
            Layer::User,
            ErrorCode::InvalidConfiguration,
            format!("could not parse: {err}"),
        )
        .with_path(path)
        .with_hint("run `rune doctor` for the resolved configuration")
    })?;
    Ok(Some(config))
}

/// Reads the project configuration file.
///
/// Returns the parsed configuration together with a diagnostic for every
/// profile-owned key that was present and ignored, so the mistake is surfaced
/// without discarding the keys that are legitimately project-scoped.
fn read_project(
    path: &Utf8Path,
) -> std::result::Result<Option<(ProjectConfig, Vec<Diagnostic>)>, Diagnostic> {
    let Some(text) = read_bounded(path, Layer::Project)? else {
        return Ok(None);
    };
    let mut value: toml::Value = toml::from_str(&text).map_err(|err| {
        Diagnostic::new(
            Layer::Project,
            ErrorCode::InvalidConfiguration,
            format!("could not parse: {err}"),
        )
        .with_path(path)
    })?;

    let mut diagnostics = Vec::new();
    if let Some(table) = value.as_table_mut() {
        for key in table.keys().cloned().collect::<Vec<_>>() {
            if PROFILE_ONLY_KEYS.contains(&key.as_str()) {
                table.remove(&key);
                diagnostics.push(
                    Diagnostic::new(
                        Layer::Project,
                        ErrorCode::KeyNotAllowedInScope,
                        format!("key `{key}` is a user setting and was ignored"),
                    )
                    .with_key(key)
                    .with_path(path)
                    .with_hint("set this in the user config instead"),
                );
            } else if !PROJECT_SAFE_KEYS.contains(&key.as_str()) {
                return Err(Diagnostic::new(
                    Layer::Project,
                    ErrorCode::InvalidConfiguration,
                    format!("unknown key `{key}`"),
                )
                .with_key(key)
                .with_path(path)
                .with_hint(format!(
                    "accepted keys are {}",
                    PROJECT_SAFE_KEYS.join(", ")
                )));
            }
        }
    }

    let config: ProjectConfig = value.try_into().map_err(|err: toml::de::Error| {
        Diagnostic::new(
            Layer::Project,
            ErrorCode::InvalidConfiguration,
            format!("could not parse: {err}"),
        )
        .with_path(path)
    })?;
    Ok(Some((config, diagnostics)))
}

/// Reads a file with a size bound, returning `None` when it does not exist.
fn read_bounded(path: &Utf8Path, layer: Layer) -> std::result::Result<Option<String>, Diagnostic> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(Diagnostic::new(
                layer,
                ErrorCode::UnsafePath,
                format!("could not read: {err}"),
            )
            .with_path(path));
        }
    };

    if meta.len() > MAX_CONFIG_BYTES {
        return Err(Diagnostic::new(
            layer,
            ErrorCode::TooLarge,
            format!("file holds {} bytes", meta.len()),
        )
        .with_path(path)
        .with_hint(format!("the limit is {MAX_CONFIG_BYTES} bytes")));
    }

    let text = std::fs::read_to_string(path).map_err(|err| {
        Diagnostic::new(
            layer,
            ErrorCode::UnsafePath,
            format!("could not read: {err}"),
        )
        .with_path(path)
    })?;

    Ok(Some(text))
}

/// Applies a user configuration to the settings.
fn apply_user(settings: &mut Settings, user: &UserConfig, layer: Layer) {
    if let Some(provider) = &user.provider {
        settings.provider = provider.clone();
        settings.sources.record("provider", layer);
    }
    if let Some(base_url) = &user.base_url {
        settings.base_url = Some(base_url.clone());
        settings.sources.record("base_url", layer);
    }
    if let Some(api_key_env) = &user.api_key_env {
        settings.api_key_env = Some(api_key_env.clone());
        settings.sources.record("api_key_env", layer);
    }
    if let Some(models) = &user.models {
        let key = provider_key(&settings.provider);
        if let Some(model) = models.get(&key).or_else(|| models.get("default")) {
            model.id().clone_into(&mut settings.model);
            settings.sources.record("model", layer);
            // The declared capacity travels with the model it describes, so a
            // layer that names a model also names what it accepts.
            settings.context_window = model.context_window();
            if settings.context_window.is_some() {
                settings.sources.record("context_window", layer);
            }
        }
    }
    if let Some(mode) = user.permission_mode {
        settings.permission_mode = mode;
        settings.sources.record("permission_mode", layer);
    }
    if let Some(effort) = user.effort {
        settings.effort = effort;
        settings.sources.record("effort", layer);
    }
    if let Some(fast) = user.fast_mode {
        settings.fast_mode = fast;
        settings.sources.record("fast_mode", layer);
    }
    if let Some(theme) = &user.theme {
        settings.theme = Some(theme.clone());
        settings.sources.record("theme", layer);
    }
    if let Some(auto) = user.auto_upgrade {
        settings.auto_upgrade = auto;
        settings.sources.record("auto_upgrade", layer);
    }
    if let Some(collapse) = user.collapse_tool_calls {
        settings.collapse_tool_calls = collapse;
        settings.sources.record("collapse_tool_calls", layer);
    }
    if let Some(titles) = user.session_titles {
        settings.session_titles = titles;
        settings.sources.record("session_titles", layer);
    }
    if let Some(context) = user.context {
        settings.context = context;
        settings.sources.record("context", layer);
    }
    if let Some(dirs) = &user.additional_directories {
        settings.additional_directories.clone_from(dirs);
        settings.sources.record("additional_directories", layer);
    }
    if let Some(order) = &user.provider_order {
        settings.provider_order.clone_from(order);
        settings.sources.record("provider_order", layer);
    }
    if let Some(strict) = user.provider_strict {
        settings.provider_strict = strict;
        settings.sources.record("provider_strict", layer);
    }
    if let Some(review) = &user.review_model {
        settings.review_model = Some(review.clone());
        settings.sources.record("review_model", layer);
    }
    apply_limit_table(
        &mut settings.limits,
        user.limits.as_ref(),
        layer,
        &mut settings.diagnostics,
    );
}

/// Applies a project configuration, ignoring anything not repository safe.
fn apply_project(settings: &mut Settings, project: &ProjectConfig, layer: Layer) {
    // A repository may state the sandbox posture, and the only value it may
    // state is the restrictive one: a repository cannot grant itself the right
    // to run commands unrestricted on someone else's machine.
    if let Some(sandbox) = &project.sandbox {
        match sandbox.trim().to_ascii_lowercase().as_str() {
            "enforce" | "strict" => {}
            other => settings.diagnostics.push(Diagnostic::new(
                layer,
                ErrorCode::InvalidField,
                format!("project `sandbox`: `{other}` is not a value a repository may set"),
            )),
        }
    }

    // Project limits and step caps sit below user settings only when the user
    // has not already set them, which the layer ordering already handles.
    if let Some(steps) = project
        .max_agent_steps
        .filter(|_| settings.source_of("max_agent_steps") == Layer::Default)
    {
        let _ = settings
            .limits
            .set(LimitName::MaxAgentSteps, Budget::Bounded(steps), layer);
    }
    if let Some(bytes) = project
        .max_tool_result_bytes
        .filter(|_| settings.source_of("max_tool_result_bytes") == Layer::Default)
    {
        let _ = settings
            .limits
            .set(LimitName::MaxToolResultBytes, Budget::Bounded(bytes), layer);
    }
    if let Some(context) = project
        .context
        .filter(|_| settings.source_of("context") == Layer::Default)
    {
        settings.context = context;
        settings.sources.record("context", layer);
    }
    if let Some(order) = project
        .provider_order
        .as_ref()
        .filter(|_| settings.provider_order.is_empty())
    {
        settings.provider_order.clone_from(order);
        settings.sources.record("provider_order", layer);
    }
    if let Some(strict) = project
        .provider_strict
        .filter(|_| settings.source_of("provider_strict") == Layer::Default)
    {
        settings.provider_strict = strict;
        settings.sources.record("provider_strict", layer);
    }
    apply_limit_table(
        &mut settings.limits,
        project.limits.as_ref(),
        layer,
        &mut settings.diagnostics,
    );
}

/// Applies a `limits` table from any layer.
fn apply_limit_table(
    budgets: &mut BudgetSet,
    table: Option<&BTreeMap<String, Budget>>,
    layer: Layer,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(table) = table else {
        return;
    };
    for (key, value) in table {
        match key.parse::<LimitName>() {
            Ok(name) => {
                if let Err(err) = budgets.set(name, *value, layer) {
                    diagnostics.push(
                        Diagnostic::new(layer, err.code(), err.message().to_owned())
                            .with_key(key.clone()),
                    );
                }
            }
            Err(err) => {
                diagnostics.push(
                    Diagnostic::new(layer, err.code(), err.message().to_owned())
                        .with_key(key.clone())
                        .with_hint("accepted keys are listed by `rune limits`"),
                );
            }
        }
    }
}

/// Applies process environment overrides, which sit above every file layer.
fn apply_environment(settings: &mut Settings, env: &EnvironmentOverrides) {
    let layer = Layer::Environment;

    if let Some(provider) = &env.provider {
        settings.provider = parse_provider(provider);
        settings.sources.record("provider", layer);
    }
    if let Some(model) = &env.model {
        settings.model.clone_from(model);
        settings.sources.record("model", layer);
    }
    if let Some(url) = &env.base_url {
        settings.base_url = Some(url.clone());
        settings.sources.record("base_url", layer);
    }
    if let Some(var) = &env.api_key_env {
        settings.api_key_env = Some(var.clone());
        settings.sources.record("api_key_env", layer);
    }
    if let Some(raw) = &env.permission_mode {
        match PermissionMode::from_name(raw) {
            Some(mode) => {
                settings.permission_mode = mode;
                settings.sources.record("permission_mode", layer);
            }
            None => settings.diagnostics.push(
                Diagnostic::new(
                    layer,
                    ErrorCode::InvalidField,
                    format!("RUNE_PERMISSION_MODE is `{raw}`"),
                )
                .with_key("permission_mode")
                .with_hint("accepted values are ask, auto, and full-access"),
            ),
        }
    }
    if let Some(raw) = &env.effort {
        let parsed = match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Effort::Auto),
            "none" => Some(Effort::None),
            "minimal" => Some(Effort::Minimal),
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "xhigh" => Some(Effort::Xhigh),
            "max" => Some(Effort::Max),
            _ => None,
        };
        match parsed {
            Some(effort) => {
                settings.effort = effort;
                settings.sources.record("effort", layer);
            }
            None => settings.diagnostics.push(
                Diagnostic::new(
                    layer,
                    ErrorCode::InvalidField,
                    format!("RUNE_EFFORT is `{raw}`"),
                )
                .with_key("effort")
                .with_hint(
                    "accepted values are auto, none, minimal, low, medium, high, xhigh, and max",
                ),
            ),
        }
    }
    if let Some(fast) = env.fast_mode {
        settings.fast_mode = fast;
        settings.sources.record("fast_mode", layer);
    }
    if let Some(theme) = &env.theme {
        settings.theme = Some(theme.clone());
        settings.sources.record("theme", layer);
    }
    if let Some(auto) = env.auto_upgrade {
        settings.auto_upgrade = auto;
        settings.sources.record("auto_upgrade", layer);
    }
    if let Some(offline) = env.offline {
        settings.offline = offline;
        settings.sources.record("offline", layer);
    }
    if !env.additional_directories.is_empty() {
        settings
            .additional_directories
            .clone_from(&env.additional_directories);
        settings.sources.record("additional_directories", layer);
    }
    if let Some(order) = &env.provider_order {
        settings.provider_order = order
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect();
        settings.sources.record("provider_order", layer);
    }
    if let Some(strict) = env.provider_strict {
        settings.provider_strict = strict;
        settings.sources.record("provider_strict", layer);
    }
    if let Some(review) = &env.review_model {
        settings.review_model = Some(review.clone());
        settings.sources.record("review_model", layer);
    }

    for entry in &env.limits {
        let Some((key, value)) = entry.split_once('=') else {
            settings.diagnostics.push(
                Diagnostic::new(
                    layer,
                    ErrorCode::InvalidField,
                    format!("RUNE_LIMITS entry `{entry}` is not in name=value form"),
                )
                .with_hint("use `rune --limit <name>=<value>` instead"),
            );
            continue;
        };
        apply_limit_string(
            &mut settings.limits,
            key.trim(),
            value.trim(),
            layer,
            &mut settings.diagnostics,
        );
    }
}

/// Applies one limit override written as text.
pub fn apply_limit_string(
    budgets: &mut BudgetSet,
    key: &str,
    value: &str,
    layer: Layer,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Ok(name) = key.parse::<LimitName>() else {
        diagnostics.push(
            Diagnostic::new(
                layer,
                ErrorCode::InvalidField,
                format!("`{key}` is not a known limit"),
            )
            .with_key(key.to_owned())
            .with_hint("run `rune limits` to list every limit"),
        );
        return;
    };

    let Ok(budget) = value.parse::<Budget>() else {
        diagnostics.push(
            Diagnostic::new(
                layer,
                ErrorCode::InvalidField,
                format!("`{value}` is not a non-negative integer or `off`"),
            )
            .with_key(key.to_owned()),
        );
        return;
    };

    if let Err(err) = budgets.set(name, budget, layer) {
        diagnostics.push(
            Diagnostic::new(layer, err.code(), err.message().to_owned()).with_key(key.to_owned()),
        );
    }
}

/// Returns the settings key used for a provider's model.
#[must_use]
pub fn provider_key(provider: &Provider) -> String {
    match provider {
        Provider::Unconfigured => "default".to_owned(),
        Provider::ChatCompletions => "chat_completions".to_owned(),
        Provider::Responses => "responses".to_owned(),
        Provider::Anthropic => "anthropic".to_owned(),
        Provider::Named(name) => name.clone(),
    }
}

/// Parses a provider name as written by a user or an environment variable.
///
/// Unknown names become [`Provider::Named`] so a configured connection can be
/// selected by its own name. Validation against the declared connections happens
/// when the catalog is built, not here, because this crate does not read them.
#[must_use]
pub fn parse_provider(raw: &str) -> Provider {
    match raw.trim().to_ascii_lowercase().as_str() {
        "chat_completions" | "chat-completions" | "openai" => Provider::ChatCompletions,
        "responses" => Provider::Responses,
        "anthropic" => Provider::Anthropic,
        other => Provider::Named(other.to_owned()),
    }
}

/// Describes why a command cannot proceed without a connected provider.
///
/// Returned instead of a generic failure so the message names every way to
/// connect one. The text is product copy and is asserted by a test.
#[must_use]
pub fn unconfigured_provider_error() -> RuneError {
    RuneError::new(
        ErrorCode::AuthenticationRequired,
        "no model provider is connected",
    )
    .with_hint("run `rune connect` to choose an endpoint and store a credential")
}

/// Renders the project configuration keys, for documentation consistency tests.
#[must_use]
pub const fn project_safe_keys() -> &'static [&'static str] {
    PROJECT_SAFE_KEYS
}

/// Renders the profile-owned keys, for documentation consistency tests.
#[must_use]
pub const fn profile_only_keys() -> &'static [&'static str] {
    PROFILE_ONLY_KEYS
}

/// Serializes settings to the JSON shape used by `status --json`.
#[must_use]
pub fn to_status_json(settings: &Settings, workspace: &Utf8Path) -> serde_json::Value {
    serde_json::json!({
        "provider": settings.provider,
        "model": settings.model,
        "base_url": settings.base_url,
        "permission_mode": settings.permission_mode,
        "permission_label": settings.permission_mode.label(),
        "effort": settings.effort,
        "fast_mode": settings.fast_mode,
        "theme": settings.theme,
        "auto_upgrade": settings.auto_upgrade,
        "context": settings.context,
        "collapse_tool_calls": settings.collapse_tool_calls,
        "session_titles": settings.session_titles,
        "additional_directories": settings.additional_directories,
        "provider_order": settings.provider_order,
        "provider_strict": settings.provider_strict,
        "review_model": settings.review_model,
        "workspace": workspace.as_str(),
        "diagnostics": settings.diagnostics,
    })
}

/// Diagnostic codes emitted while loading configuration, for documentation.
#[must_use]
pub fn diagnostic_codes() -> Vec<ErrorCode> {
    vec![
        ErrorCode::InvalidConfiguration,
        ErrorCode::KeyNotAllowedInScope,
        ErrorCode::TooLarge,
        ErrorCode::UnsafePath,
        ErrorCode::InvalidField,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &TempDir, name: &str, body: &str) -> Utf8PathBuf {
        let path = Utf8PathBuf::from_path_buf(dir.path().join(name)).expect("utf8 path");
        std::fs::write(&path, body).expect("write");
        path
    }

    fn empty_env() -> EnvironmentOverrides {
        EnvironmentOverrides::default()
    }

    #[test]
    fn missing_files_resolve_to_defaults() {
        let settings = load(None, None, &empty_env());
        assert!(settings.diagnostics.is_empty());
        assert_eq!(settings.permission_mode, PermissionMode::Auto);
        assert_eq!(settings.source_of("permission_mode"), Layer::Default);
        assert!(settings.context);
    }

    #[test]
    fn user_layer_supplies_values() {
        let dir = TempDir::new().expect("tempdir");
        let user = write(
            &dir,
            "config.toml",
            r#"
permission_mode = "ask"
theme = "light"
effort = "high"
theme_unused = "x"
"#
            .replace("theme_unused = \"x\"\n", "")
            .as_str(),
        );
        let settings = load(None, Some(&user), &empty_env());
        assert!(
            settings.diagnostics.is_empty(),
            "{:?}",
            settings.diagnostics
        );
        assert_eq!(settings.permission_mode, PermissionMode::Ask);
        assert_eq!(settings.theme.as_deref(), Some("light"));
        assert_eq!(settings.effort, Effort::High);
        assert_eq!(settings.source_of("permission_mode"), Layer::User);
    }

    #[test]
    fn environment_beats_user_file() {
        let dir = TempDir::new().expect("tempdir");
        let user = write(&dir, "config.toml", "permission_mode = \"ask\"\n");
        let mut env = empty_env();
        env.permission_mode = Some("full-access".to_owned());
        let settings = load(None, Some(&user), &env);
        assert_eq!(settings.permission_mode, PermissionMode::FullAccess);
        assert_eq!(settings.source_of("permission_mode"), Layer::Environment);
    }

    #[test]
    fn command_line_beats_environment() {
        let mut settings = Settings::default();
        let mut env = empty_env();
        env.model = Some("from-env".to_owned());
        apply_environment(&mut settings, &env);
        settings.model = "from-flag".to_owned();
        settings.sources.record("model", Layer::CommandLine);
        assert_eq!(settings.model, "from-flag");
        assert_eq!(settings.source_of("model"), Layer::CommandLine);
    }

    #[test]
    fn profile_only_key_in_project_is_ignored_and_reported() {
        let dir = TempDir::new().expect("tempdir");
        let project = write(
            &dir,
            ".rune.toml",
            "model = \"sneaky\"\nmax_agent_steps = 5\n",
        );

        // The layer applies for its permitted keys, and the profile-only key is
        // reported alongside them rather than aborting the load.
        let (project_config, diagnostics) =
            read_project(&project).expect("parse").expect("present");
        assert_eq!(project_config.max_agent_steps, Some(5));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, ErrorCode::KeyNotAllowedInScope);
        assert_eq!(diagnostics[0].key.as_deref(), Some("model"));

        // And the ignored key never reaches the settings.
        let settings = load(Some(&project), None, &empty_env());
        assert!(settings.model.is_empty());
        assert!(settings.has_diagnostics());
    }

    #[test]
    fn unknown_project_key_is_rejected_with_the_accepted_set() {
        let dir = TempDir::new().expect("tempdir");
        let project = write(&dir, ".rune.toml", "nonsense = true\n");
        let diagnostic = read_project(&project).expect_err("rejected");
        assert_eq!(diagnostic.code, ErrorCode::InvalidConfiguration);
        assert_eq!(diagnostic.key.as_deref(), Some("nonsense"));
        assert!(diagnostic.hint.is_some());
    }

    #[test]
    fn malformed_user_file_does_not_abort_startup() {
        let dir = TempDir::new().expect("tempdir");
        let user = write(&dir, "config.toml", "this is not toml =");
        let settings = load(None, Some(&user), &empty_env());
        assert_eq!(settings.diagnostics.len(), 1);
        assert_eq!(settings.diagnostics[0].layer, Layer::User);
        assert_eq!(settings.permission_mode, PermissionMode::Auto);
    }

    #[test]
    fn oversized_config_is_reported_not_read() {
        let dir = TempDir::new().expect("tempdir");
        let big = "x".repeat((MAX_CONFIG_BYTES + 1) as usize);
        let user = write(&dir, "config.toml", &format!("theme = \"{big}\"\n"));
        let settings = load(None, Some(&user), &empty_env());
        assert_eq!(settings.diagnostics.len(), 1);
        assert_eq!(settings.diagnostics[0].code, ErrorCode::TooLarge);
        assert!(settings.diagnostics[0].hint.is_some());
    }

    #[test]
    fn project_cannot_widen_permission_mode() {
        let dir = TempDir::new().expect("tempdir");
        let project = write(&dir, ".rune.toml", "permission_mode = \"full-access\"\n");
        let settings = load(Some(&project), None, &empty_env());
        assert_eq!(settings.permission_mode, PermissionMode::Auto);
    }

    #[test]
    fn project_supplies_step_cap_when_user_does_not() {
        let dir = TempDir::new().expect("tempdir");
        let project = write(&dir, ".rune.toml", "max_agent_steps = 40\n");
        let settings = load(Some(&project), None, &empty_env());
        assert_eq!(
            settings.limits.get(LimitName::MaxAgentSteps).value(),
            Some(40)
        );
        assert_eq!(
            settings.limits.source(LimitName::MaxAgentSteps),
            Some(Layer::Project)
        );
    }

    #[test]
    fn invalid_limit_value_produces_a_diagnostic_and_keeps_the_default() {
        let dir = TempDir::new().expect("tempdir");
        let user = write(
            &dir,
            "config.toml",
            "[limits]\ncompaction_trigger_percent = 5\n",
        );
        let settings = load(None, Some(&user), &empty_env());
        assert_eq!(settings.diagnostics.len(), 1);
        assert_eq!(
            settings.diagnostics[0].key.as_deref(),
            Some("compaction_trigger_percent")
        );
        assert_eq!(
            settings.limits.get(LimitName::CompactionTriggerPercent),
            LimitName::CompactionTriggerPercent.default_value()
        );
    }

    #[test]
    fn unknown_limit_key_produces_a_diagnostic() {
        let dir = TempDir::new().expect("tempdir");
        let user = write(&dir, "config.toml", "[limits]\nnot_a_limit = 10\n");
        let settings = load(None, Some(&user), &empty_env());
        assert_eq!(settings.diagnostics.len(), 1);
        assert!(settings.diagnostics[0].hint.is_some());
    }

    #[test]
    fn invalid_permission_mode_env_is_reported() {
        let mut env = empty_env();
        env.permission_mode = Some("sideways".to_owned());
        let settings = load(None, None, &env);
        assert_eq!(settings.diagnostics.len(), 1);
        assert_eq!(settings.permission_mode, PermissionMode::Auto);
    }

    #[test]
    fn permission_mode_accepts_legacy_and_canonical_spellings() {
        for raw in ["full-access", "full_access", "yolo"] {
            let mut env = empty_env();
            env.permission_mode = Some(raw.to_owned());
            let settings = load(None, None, &env);
            assert!(settings.diagnostics.is_empty(), "rejected {raw}");
            assert_eq!(settings.permission_mode, PermissionMode::FullAccess);
        }
    }

    #[test]
    fn boolean_env_parsing_accepts_the_documented_spellings() {
        for raw in ["1", "true", "on", "yes", "TRUE", "On"] {
            assert_eq!(parse_bool(raw), Some(true), "{raw}");
        }
        for raw in ["0", "false", "off", "no"] {
            assert_eq!(parse_bool(raw), Some(false), "{raw}");
        }
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn environment_overrides_read_from_a_lookup() {
        let vars = std::collections::HashMap::from([
            ("RUNE_MODEL", "test/model"),
            ("RUNE_PERMISSION_MODE", "ask"),
            ("RUNE_FAST_MODE", "on"),
            ("RUNE_ADDITIONAL_DIRS", "/a:/b"),
            ("RUNE_LIMITS", "list_entries=5, read_file_lines=10"),
        ]);
        let env = EnvironmentOverrides::from_lookup(|key| vars.get(key).map(|v| (*v).to_owned()));
        assert_eq!(env.model.as_deref(), Some("test/model"));
        assert_eq!(env.fast_mode, Some(true));
        assert_eq!(env.additional_directories.len(), 2);
        assert_eq!(env.limits.len(), 2);
    }

    #[test]
    fn model_resolves_per_provider() {
        let dir = TempDir::new().expect("tempdir");
        let user = write(
            &dir,
            "config.toml",
            "[models]\nanthropic = \"claude-x\"\ndefault = \"fallback\"\n",
        );
        let mut settings = Settings {
            provider: Provider::Anthropic,
            ..Settings::default()
        };
        apply_user(
            &mut settings,
            &read_user(&user).expect("read").expect("present"),
            Layer::User,
        );
        assert_eq!(settings.model, "claude-x");
    }

    #[test]
    fn a_model_may_be_a_bare_identifier_or_a_table() {
        // Most configurations name a model and nothing else, so the bare form
        // has to keep working. The table form exists for what the endpoint does
        // not report, which for most compatible servers is the context window.
        let bare: UserConfig =
            toml::from_str("[models]\nanthropic = \"claude-x\"\n").expect("bare");
        let entry = bare
            .models
            .expect("models")
            .remove("anthropic")
            .expect("entry");
        assert_eq!(entry.id(), "claude-x");
        assert_eq!(entry.context_window(), None);

        let detailed: UserConfig =
            toml::from_str("[models.anthropic]\nid = \"claude-x\"\ncontext_window = 1000000\n")
                .expect("detailed");
        let entry = detailed
            .models
            .expect("models")
            .remove("anthropic")
            .expect("entry");
        assert_eq!(entry.id(), "claude-x");
        assert_eq!(entry.context_window(), Some(1_000_000));
    }

    #[test]
    fn a_declared_window_reaches_the_settings() {
        // The key is the provider's own name, and an unconfigured session reads
        // the `default` entry.
        let config: UserConfig =
            toml::from_str("[models.default]\nid = \"m\"\ncontext_window = 200000\n")
                .expect("config");
        let mut settings = Settings::default();
        apply_user(&mut settings, &config, Layer::User);
        assert_eq!(settings.model, "m");
        assert_eq!(settings.context_window, Some(200_000));
        assert_eq!(settings.source_of("context_window"), Layer::User);
    }

    #[test]
    fn an_undeclared_window_is_reported_as_the_default() {
        // The report has to say which is in force, because a window that is
        // assumed rather than declared is the thing a reader needs to know.
        let settings = Settings::default();
        let row = settings
            .explain()
            .into_iter()
            .find(|row| row.key == "context_window")
            .expect("context_window is reported");
        assert_eq!(row.value, DEFAULT_CONTEXT_WINDOW.to_string());
        assert_eq!(row.source, Layer::Default);
    }

    #[test]
    fn explain_covers_every_limit() {
        let settings = Settings::default();
        let explained = settings.explain();
        for name in LimitName::all() {
            assert!(
                explained.iter().any(|row| row.key == name.as_str()),
                "missing {name:?}"
            );
        }
    }

    #[test]
    fn explain_reports_default_layer_for_untouched_keys() {
        let settings = Settings::default();
        let row = settings
            .explain()
            .into_iter()
            .find(|row| row.key == "permission_mode")
            .expect("present");
        assert_eq!(row.source, Layer::Default);
        assert_eq!(row.value, "auto");
    }

    #[test]
    fn layer_ordering_is_explicit() {
        assert!(Layer::Default < Layer::User);
        assert!(Layer::User < Layer::Project);
        assert!(Layer::Project < Layer::Environment);
        assert!(Layer::Environment < Layer::CommandLine);
    }

    #[test]
    fn status_json_includes_the_workspace_and_label() {
        let settings = Settings::default();
        let json = to_status_json(&settings, Utf8Path::new("/tmp/w"));
        assert_eq!(json["permission_mode"], "auto");
        assert_eq!(json["permission_label"], "auto");
        assert_eq!(json["workspace"], "/tmp/w");
    }

    #[test]
    fn project_and_profile_key_sets_do_not_overlap() {
        for key in project_safe_keys() {
            assert!(
                !profile_only_keys().contains(key),
                "`{key}` is in both sets"
            );
        }
    }

    #[test]
    fn fresh_install_has_no_provider() {
        let settings = load(None, None, &empty_env());
        assert_eq!(settings.provider, Provider::Unconfigured);
        assert!(!settings.provider.is_configured());
        assert!(settings.model.is_empty());
    }

    #[test]
    fn model_request_without_provider_fails_with_a_named_remedy() {
        let settings = load(None, None, &empty_env());
        let err = settings.require_model().expect_err("unconfigured");
        assert_eq!(err.code(), ErrorCode::AuthenticationRequired);
        let hint = err.detail().hint.as_deref().expect("hint");
        assert!(hint.contains("rune connect"), "hint was `{hint}`");
        assert_ne!(hint, err.message(), "the hint repeats the message");
    }

    #[test]
    fn provider_without_model_is_a_configuration_error() {
        let dir = TempDir::new().expect("tempdir");
        let user = write(&dir, "config.toml", "provider = \"anthropic\"\n");
        let settings = load(None, Some(&user), &empty_env());
        let err = settings.require_model().expect_err("no model");
        assert_eq!(err.code(), ErrorCode::InvalidConfiguration);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn configured_provider_and_model_satisfy_the_check() {
        let dir = TempDir::new().expect("tempdir");
        let user = write(
            &dir,
            "config.toml",
            "provider = \"anthropic\"\n[models]\nanthropic = \"claude-x\"\n",
        );
        let settings = load(None, Some(&user), &empty_env());
        assert!(settings.require_model().is_ok());
        assert_eq!(settings.model, "claude-x");
        assert_eq!(settings.provider, Provider::Anthropic);
    }

    #[test]
    fn named_provider_is_preserved_for_connection_selection() {
        assert_eq!(
            parse_provider("my-local-llama"),
            Provider::Named("my-local-llama".to_owned())
        );
        assert_eq!(parse_provider("openai"), Provider::ChatCompletions);
        assert_eq!(parse_provider("ANTHROPIC"), Provider::Anthropic);
        assert_eq!(
            parse_provider("chat-completions"),
            Provider::ChatCompletions
        );
    }

    #[test]
    fn unconfigured_provider_renders_as_text() {
        assert_eq!(Provider::Unconfigured.to_string(), "unconfigured");
        assert_eq!(provider_key(&Provider::Unconfigured), "default");
    }
}
