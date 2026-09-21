//! Project server trust and project-only template expansion.
//!
//! A repository can commit a `.mcp.json` file, so a server defined there is
//! something an attacker can ask the user to run by asking them to open a
//! repository. Every entry therefore starts pending, and a pending entry is
//! never started, never contacted, and never has an environment value read for
//! it. Only an approved entry reaches [`ProjectServers::startable`], which is
//! the single source of servers a runtime may connect.
//!
//! `${VAR}` and `${VAR:-default}` expand in a project entry only, and only after
//! approval. A profile entry is literal, because the profile is written by the
//! user for their own machine and reinterpreting its strings would silently
//! change what it means.

use std::collections::BTreeSet;
use std::fmt;

use camino::Utf8Path;
use serde::{Deserialize, Serialize};

use rune_core::budget::LimitName;
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::limits::default_limit;

/// Project configuration file name.
pub const PROJECT_FILE: &str = ".mcp.json";

/// Largest accepted project configuration file.
pub const MAX_PROJECT_BYTES: u64 = 1024 * 1024;

/// Largest expansion of one value.
pub const MAX_VALUE_BYTES: usize = 1024 * 1024;

/// Largest expansion of every value of one server, added together.
pub const MAX_TOTAL_BYTES: usize = 1024 * 1024;

/// Where one project server stands.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Never approved or rejected. Unusable until the user decides.
    Pending,
    /// Approved for this workspace.
    Approved,
    /// Refused for this workspace.
    Rejected,
}

/// The project servers of one workspace and their decisions.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ProjectServers {
    /// Names that loaded from the project file without a decision. Unusable
    /// until the user decides.
    pub pending: Vec<String>,
    /// Names approved for this workspace. These may be started.
    pub approved: Vec<String>,
    /// Names refused for this workspace.
    pub rejected: Vec<String>,
}

impl ProjectServers {
    /// Derives the trust surface of a workspace from its project file.
    ///
    /// A missing file yields an empty surface. Nothing here starts a server or
    /// reads an environment value: the file is parsed into names and decisions
    /// only, and every declared name starts pending until the trust record says
    /// otherwise.
    pub fn from_workspace(workspace: &Utf8Path, trust: &TrustStore) -> Result<Self> {
        let declared: Vec<String> = read_project_file(workspace)?
            .into_iter()
            .map(|server| server.name)
            .collect();
        Ok(Self::derive(&declared, trust))
    }

    /// Derives the three lists from a project file and a trust record.
    ///
    /// A name in no trust list stays pending. A trust record naming a server
    /// that is no longer in the file is ignored, so a removed server leaves no
    /// authority behind. `enable_all` approves every entry in the file at once.
    #[must_use]
    pub fn derive(declared: &[String], trust: &TrustStore) -> Self {
        let mut out = Self::default();
        for name in declared {
            match trust.decision(name) {
                Decision::Approved => out.approved.push(name.clone()),
                Decision::Rejected => out.rejected.push(name.clone()),
                Decision::Pending => out.pending.push(name.clone()),
            }
        }
        out
    }

    /// Returns the decision recorded for one server.
    #[must_use]
    pub fn decision(&self, name: &str) -> Decision {
        if self.approved.iter().any(|entry| entry == name) {
            return Decision::Approved;
        }
        if self.rejected.iter().any(|entry| entry == name) {
            return Decision::Rejected;
        }
        Decision::Pending
    }

    /// Returns the servers a runtime may start, which are the approved ones.
    ///
    /// This is the only accessor a connection path may use. A pending entry is
    /// absent even when it appears in the file, so no code path can reach a
    /// server the user has not approved.
    #[must_use]
    pub fn startable(&self) -> &[String] {
        &self.approved
    }

    /// Returns true when a server may be started.
    #[must_use]
    pub fn is_startable(&self, name: &str) -> bool {
        self.approved.iter().any(|entry| entry == name)
    }

    /// Returns true when the file declared no server at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.approved.is_empty() && self.rejected.is_empty()
    }
}

/// The trust record for one workspace.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustStore {
    /// Names the user approved.
    pub approved: BTreeSet<String>,
    /// Names the user refused.
    pub rejected: BTreeSet<String>,
    /// Whether the whole file was approved at once.
    pub enable_all: bool,
}

impl TrustStore {
    /// Returns the decision recorded for one server.
    #[must_use]
    pub fn decision(&self, name: &str) -> Decision {
        if self.enable_all || self.approved.contains(name) {
            return Decision::Approved;
        }
        if self.rejected.contains(name) {
            return Decision::Rejected;
        }
        Decision::Pending
    }

    /// Records a decision, replacing any earlier one for the same name.
    ///
    /// One name cannot be both approved and rejected, so recording a decision
    /// clears the other side rather than leaving a record that would resolve by
    /// whichever list is checked first.
    pub fn decide(&mut self, name: &str, decision: Decision) {
        self.approved.remove(name);
        self.rejected.remove(name);
        match decision {
            Decision::Approved => {
                self.approved.insert(name.to_owned());
            }
            Decision::Rejected => {
                self.rejected.insert(name.to_owned());
            }
            Decision::Pending => {}
        }
    }
}

/// One declared project server, before any decision or expansion.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectServer {
    /// Name the file declares.
    pub name: String,
    /// Program and arguments for a stdio server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Environment values the server would receive.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub environment: std::collections::BTreeMap<String, String>,
    /// Headers a remote server would receive.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
}

/// Expands `${VAR}` and `${VAR:-default}` in a project configuration string.
///
/// `workspace` is the directory a `${WORKSPACE}` reference names, so a project
/// can point at itself without hard-coding a checkout path. `env` supplies the
/// values; a variable it does not resolve is either replaced by its default or
/// reported as missing.
///
/// Only project strings reach this function. A profile string is literal, so
/// calling it there would change what the profile means.
pub fn expand_template(
    value: &str,
    workspace: &Utf8Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String> {
    let mut out = String::with_capacity(value.len().min(MAX_VALUE_BYTES));
    let mut index = 0;
    while index < value.len() {
        let Some(offset) = value.get(index..).and_then(|rest| rest.find("${")) else {
            out.push_str(value.get(index..).unwrap_or_default());
            break;
        };
        let open = index.saturating_add(offset);
        out.push_str(value.get(index..open).unwrap_or_default());
        let body = open.saturating_add(2);
        let Some(close) = value.get(body..).and_then(|rest| rest.find('}')) else {
            // An unterminated `${` is literal text, not a template.
            out.push_str(value.get(open..).unwrap_or_default());
            break;
        };
        let end = body.saturating_add(close);
        let inner = value.get(body..end).unwrap_or_default();
        out.push_str(&expand_one(inner, workspace, env)?);
        // Checked per replacement, so a value that expands into something huge
        // is refused before it is built rather than after.
        if out.len() > MAX_VALUE_BYTES {
            return Err(
                RuneError::too_large("expansion", out.len(), MAX_VALUE_BYTES)
                    .with_invariant("one expanded value fits max_value_bytes"),
            );
        }
        index = end.saturating_add(1);
    }
    // A value with no reference at all arrives here whole, so the bound is
    // applied to the finished text as well as to each replacement.
    if out.len() > MAX_VALUE_BYTES {
        return Err(
            RuneError::too_large("expansion", out.len(), MAX_VALUE_BYTES)
                .with_invariant("one expanded value fits max_value_bytes"),
        );
    }
    Ok(out)
}

/// The variable a `${WORKSPACE}` reference resolves to.
const WORKSPACE_VARIABLE: &str = "WORKSPACE";

/// Expands one `${...}` body, which is a name and an optional default.
fn expand_one(
    inner: &str,
    workspace: &Utf8Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String> {
    let (name, default) = match inner.split_once(":-") {
        Some((name, default)) => (name.trim(), Some(default)),
        None => (inner.trim(), None),
    };
    if name.is_empty() || !is_variable_name(name) {
        return Err(RuneError::invalid_field(
            "template",
            format!("`${{{inner}}}` is not a variable reference"),
        ));
    }
    let supplied = if name == WORKSPACE_VARIABLE {
        Some(workspace.as_str().to_owned())
    } else {
        env(name)
    };
    match (supplied, default) {
        (Some(value), _) => Ok(value),
        (None, Some(default)) => Ok(default.to_owned()),
        (None, None) => Err(missing(name)),
    }
}

/// Returns true when a name can be an environment variable.
fn is_variable_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Builds the error for a variable that is required and absent.
///
/// The message names the variable and never its value: a value read here is
/// exactly the secret the template exists to keep out of the repository.
fn missing(name: &str) -> RuneError {
    RuneError::new(
        ErrorCode::MissingField,
        format!("`{name}` is not set and the project configuration gives no default"),
    )
    .with_hint(format!(
        "set `{name}` in the environment, or write ${{{name}:-default}}"
    ))
}

/// Expands every string of one approved project server.
///
/// A decision other than `Approved` is refused before anything is read, so no
/// variable is consulted for a server the user has not approved. The second cap
/// applies across the whole entry, because many values that each fit can still
/// add up to more than the budget allows.
pub fn expand_approved(
    server: &ProjectServer,
    decision: Decision,
    workspace: &Utf8Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ProjectServer> {
    if decision != Decision::Approved {
        return Err(RuneError::new(
            ErrorCode::PermissionDenied,
            format!(
                "`{}` is {} and its configuration is not read",
                server.name,
                match decision {
                    Decision::Pending => "pending",
                    Decision::Rejected => "rejected",
                    Decision::Approved => "approved",
                }
            ),
        ));
    }
    let mut spent = 0_usize;
    let mut expand = |value: &str| -> Result<String> {
        let expanded = expand_template(value, workspace, env)?;
        spent = spent.saturating_add(expanded.len());
        if spent > MAX_TOTAL_BYTES {
            return Err(RuneError::too_large("expansion", spent, MAX_TOTAL_BYTES)
                .with_invariant("one server's expanded values fit max_total_bytes"));
        }
        Ok(expanded)
    };

    let mut command = Vec::with_capacity(server.command.len());
    for part in &server.command {
        command.push(expand(part)?);
    }
    let mut environment = std::collections::BTreeMap::new();
    for (name, value) in &server.environment {
        environment.insert(name.clone(), expand(value)?);
    }
    let mut headers = std::collections::BTreeMap::new();
    for (name, value) in &server.headers {
        headers.insert(name.clone(), expand(value)?);
    }
    Ok(ProjectServer {
        name: server.name.clone(),
        command,
        environment,
        headers,
    })
}

/// Reads the project configuration file of a workspace.
///
/// A missing file is not an error: most workspaces have none. The file is read
/// as a bounded, no-follow regular file, so a symlink or an oversized file is
/// refused rather than followed.
pub fn read_project_file(workspace: &Utf8Path) -> Result<Vec<ProjectServer>> {
    let path = workspace.join(PROJECT_FILE);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(RuneError::from(err)),
    };
    if metadata.is_symlink() || !metadata.is_file() {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("{path} is not a regular file"),
        ));
    }
    if metadata.len() > MAX_PROJECT_BYTES {
        return Err(RuneError::too_large(
            PROJECT_FILE,
            usize::try_from(metadata.len()).unwrap_or(usize::MAX),
            usize::try_from(MAX_PROJECT_BYTES).unwrap_or(usize::MAX),
        ));
    }
    let text = std::fs::read_to_string(&path).map_err(|err| {
        RuneError::new(
            ErrorCode::InvalidConfiguration,
            format!("{path} could not be read as text: {err}"),
        )
    })?;
    parse_project_file(&text, &path)
}

/// Parses a project configuration document, reading only `mcpServers`.
///
/// The shape is the one other clients write, so a repository that already has a
/// project MCP file works here unchanged. Unknown keys inside one entry are
/// refused, because an entry this build cannot express is one it cannot safely
/// quote to the user for approval. A file declaring more entries than
/// `list_entries` is refused rather than truncated, so an entry is never dropped
/// from the list the user reviews.
pub fn parse_project_file(text: &str, path: &Utf8Path) -> Result<Vec<ProjectServer>> {
    let document: serde_json::Value = serde_json::from_str(text).map_err(|err| {
        RuneError::new(
            ErrorCode::InvalidConfiguration,
            format!("{path} is not valid JSON: {err}"),
        )
    })?;
    let Some(servers) = document.get("mcpServers") else {
        return Ok(Vec::new());
    };
    let servers = servers.as_object().ok_or_else(|| {
        RuneError::invalid_field(
            "mcpServers",
            format!("{path} declares `mcpServers` but it is not an object"),
        )
    })?;
    let cap = default_limit(LimitName::ListEntries);
    if servers.len() > cap {
        return Err(RuneError::new(
            ErrorCode::LimitExceeded,
            format!(
                "{path} declares {} servers, the limit is {cap}",
                servers.len()
            ),
        )
        .with_hint(format!(
            "raise `{}` to review a larger file, or split the file",
            LimitName::ListEntries.as_str()
        )));
    }
    let mut out = Vec::with_capacity(servers.len());
    for (name, entry) in servers {
        let object = entry.as_object().ok_or_else(|| {
            RuneError::invalid_field("mcpServers", format!("`{name}` in {path} is not an object"))
        })?;
        let mut command = Vec::new();
        let mut environment = std::collections::BTreeMap::new();
        let mut headers = std::collections::BTreeMap::new();
        let (mut has_command, mut has_url) = (false, false);
        for (key, value) in object {
            match key.as_str() {
                "command" => {
                    has_command = true;
                    command = string_list(value, path, name)?;
                }
                "args" => {
                    command.extend(string_list(value, path, name)?);
                }
                "env" | "environment" => {
                    environment = string_map(value, path, name)?;
                }
                "headers" => {
                    headers = string_map(value, path, name)?;
                }
                "url" | "type" | "enabled" | "required" => {
                    has_url = has_url || key == "url";
                }
                other => {
                    return Err(RuneError::invalid_field(
                        "mcpServers",
                        format!("`{name}` in {path} carries unsupported key `{other}`"),
                    ));
                }
            }
        }
        if !has_command && !has_url {
            return Err(RuneError::invalid_field(
                "mcpServers",
                format!("`{name}` in {path} declares neither `command` nor `url`"),
            ));
        }
        out.push(ProjectServer {
            name: name.clone(),
            command,
            environment,
            headers,
        });
    }
    out.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(out)
}

/// Reads a JSON value as a string or a list of strings.
fn string_list(value: &serde_json::Value, path: &Utf8Path, name: &str) -> Result<Vec<String>> {
    if let Some(text) = value.as_str() {
        return Ok(vec![text.to_owned()]);
    }
    let items = value.as_array().ok_or_else(|| {
        RuneError::invalid_field(
            "mcpServers",
            format!("`{name}` in {path} has a program field that is not a string or a list"),
        )
    })?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let text = item.as_str().ok_or_else(|| {
            RuneError::invalid_field(
                "mcpServers",
                format!("`{name}` in {path} has a non-string argument"),
            )
        })?;
        out.push(text.to_owned());
    }
    Ok(out)
}

/// Reads a JSON value as a map of strings.
fn string_map(
    value: &serde_json::Value,
    path: &Utf8Path,
    name: &str,
) -> Result<std::collections::BTreeMap<String, String>> {
    let object = value.as_object().ok_or_else(|| {
        RuneError::invalid_field(
            "mcpServers",
            format!("`{name}` in {path} has a map that is not an object"),
        )
    })?;
    let mut out = std::collections::BTreeMap::new();
    for (key, entry) in object {
        let text = entry.as_str().ok_or_else(|| {
            RuneError::invalid_field(
                "mcpServers",
                format!("`{name}` in {path} has a non-string value for `{key}`"),
            )
        })?;
        out.insert(key.clone(), text.to_owned());
    }
    Ok(out)
}

/// Renders the trust surface for a report, naming states and never values.
impl fmt::Display for ProjectServers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        if !self.approved.is_empty() {
            parts.push(format!("approved: {}", self.approved.join(", ")));
        }
        if !self.pending.is_empty() {
            parts.push(format!("pending: {}", self.pending.join(", ")));
        }
        if !self.rejected.is_empty() {
            parts.push(format!("rejected: {}", self.rejected.join(", ")));
        }
        if parts.is_empty() {
            return f.write_str("no project servers");
        }
        f.write_str(&parts.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use std::collections::BTreeMap;

    /// Returns an environment lookup over a fixed map.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    /// A lookup that records every variable it was asked about.
    struct Watched<'a> {
        seen: &'a std::cell::RefCell<Vec<String>>,
    }

    impl Watched<'_> {
        fn lookup(&self) -> impl Fn(&str) -> Option<String> + '_ {
            move |name: &str| {
                self.seen.borrow_mut().push(name.to_owned());
                None
            }
        }
    }

    #[test]
    fn a_variable_is_substituted() {
        let expanded = expand_template(
            "${HOME}/bin",
            Utf8Path::new("/w"),
            &env_of(&[("HOME", "/users/me")]),
        )
        .expect("expand");
        assert_eq!(expanded, "/users/me/bin");
    }

    #[test]
    fn a_default_is_used_when_the_variable_is_absent() {
        let expanded = expand_template("${PROJECT_ROOT:-.}", Utf8Path::new("/w"), &env_of(&[]))
            .expect("expand");
        assert_eq!(expanded, ".");

        let provided = expand_template(
            "${PROJECT_ROOT:-.}",
            Utf8Path::new("/w"),
            &env_of(&[("PROJECT_ROOT", "/checkout")]),
        )
        .expect("expand");
        assert_eq!(provided, "/checkout");
    }

    #[test]
    fn a_workspace_reference_resolves_to_the_workspace() {
        let expanded = expand_template(
            "${WORKSPACE}/tools",
            Utf8Path::new("/work/repo"),
            &env_of(&[]),
        )
        .expect("expand");
        assert_eq!(expanded, "/work/repo/tools");
    }

    #[test]
    fn a_text_without_a_reference_is_unchanged() {
        for value in ["plain", "with $VAR", "unterminated ${VAR", "a } b", ""] {
            assert_eq!(
                expand_template(value, Utf8Path::new("/w"), &env_of(&[])).expect("expand"),
                value
            );
        }
    }

    #[test]
    fn a_missing_required_variable_is_reported_without_its_value() {
        let err = expand_template(
            "${TOKEN}",
            Utf8Path::new("/w"),
            &env_of(&[("OTHER", "secret-value")]),
        )
        .expect_err("missing");

        assert_eq!(err.code(), ErrorCode::MissingField);
        assert!(err.message().contains("TOKEN"), "{}", err.message());
        assert!(
            !err.message().contains("secret-value"),
            "the error carries no value: {}",
            err.message()
        );
        assert!(err.detail().observed.is_none());
    }

    #[test]
    fn a_value_that_is_only_whitespace_is_not_a_default() {
        // `${TOKEN:- }` has a default of one space, so it resolves rather than
        // failing; `${TOKEN:-}` has an empty default.
        assert_eq!(
            expand_template("${TOKEN:-}", Utf8Path::new("/w"), &env_of(&[])).expect("expand"),
            ""
        );
        assert_eq!(
            expand_template("${TOKEN:- }", Utf8Path::new("/w"), &env_of(&[])).expect("expand"),
            " "
        );
    }

    #[test]
    fn a_malformed_reference_is_refused() {
        for value in ["${}", "${:-default}", "${1BAD}", "${A B}"] {
            let err =
                expand_template(value, Utf8Path::new("/w"), &env_of(&[])).expect_err("malformed");
            assert_eq!(err.code(), ErrorCode::InvalidField, "{value}");
        }
    }

    #[test]
    fn a_value_that_expands_past_its_budget_is_refused() {
        let huge = "y".repeat(MAX_VALUE_BYTES.saturating_add(1));
        let err = expand_template(
            "${HUGE}",
            Utf8Path::new("/w"),
            &env_of(&[("HUGE", huge.as_str())]),
        )
        .expect_err("too large");

        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("expansion"));
        assert!(
            !err.message().contains(&huge),
            "the error carries no expanded value"
        );
    }

    #[test]
    fn a_profile_string_is_not_expanded() {
        // The same text reaches both paths, and the difference is that no code
        // expands it for a profile entry. This goes through the profile parser
        // so the assertion is about the real read path, not about a string.
        let document = serde_json::json!({
            "name": "files",
            "transport": {
                "transport": "stdio",
                "command": ["server", "--token", "${TOKEN}", "--root", "${ROOT:-/var}"],
                "environment": { "TOKEN": "${TOKEN}" }
            }
        });
        let config =
            crate::mcp::config::ServerConfig::parse(&document).expect("profile entry parses");
        let crate::mcp::config::Transport::Stdio {
            command,
            environment,
        } = &config.transport
        else {
            panic!("expected a stdio entry");
        };

        assert!(
            command.iter().any(|part| part == "${TOKEN}"),
            "a profile command keeps its template text: {command:?}"
        );
        assert!(command.iter().any(|part| part == "${ROOT:-/var}"));
        assert_eq!(environment["TOKEN"], "${TOKEN}");

        let token = command
            .iter()
            .find(|part| part.contains("${TOKEN}"))
            .expect("the template argument survives");
        let root = command
            .iter()
            .find(|part| part.contains("ROOT"))
            .expect("the defaulted argument survives");

        // The project path expands, which is the behavior the profile must not
        // inherit.
        assert_eq!(
            expand_template(token, Utf8Path::new("/w"), &env_of(&[("TOKEN", "abc")]))
                .expect("expand"),
            "abc"
        );
        assert_eq!(
            expand_template(root, Utf8Path::new("/w"), &env_of(&[])).expect("expand"),
            "/var"
        );
    }

    #[test]
    fn a_pending_server_is_never_startable() {
        let declared = vec!["project-local".to_owned(), "project-remote".to_owned()];
        let mut trust = TrustStore::default();
        trust.decide("project-remote", Decision::Approved);
        let servers = ProjectServers::derive(&declared, &trust);

        assert_eq!(servers.startable(), ["project-remote".to_owned()]);
        assert!(!servers.is_startable("project-local"));
        assert_eq!(servers.decision("project-local"), Decision::Pending);
        assert!(servers.pending.contains(&"project-local".to_owned()));
        assert_eq!(
            servers.pending.len() + servers.approved.len() + servers.rejected.len(),
            2,
            "every declared server carries exactly one state"
        );
    }

    #[test]
    fn a_pending_server_is_never_expanded_and_reads_no_variable() {
        let server = ProjectServer {
            name: "project-local".to_owned(),
            command: vec!["server".to_owned(), "${PROJECT_TOKEN}".to_owned()],
            environment: BTreeMap::from([(
                "PROJECT_TOKEN".to_owned(),
                "${PROJECT_TOKEN}".to_owned(),
            )]),
            headers: BTreeMap::new(),
        };
        let seen = std::cell::RefCell::new(Vec::new());
        let watched = Watched { seen: &seen };
        let lookup = watched.lookup();

        let err = expand_approved(&server, Decision::Pending, Utf8Path::new("/w"), &lookup)
            .expect_err("pending");
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
        assert_eq!(
            seen.borrow().len(),
            0,
            "no variable is read for a pending entry"
        );
        assert!(err.message().contains("pending"));

        let err = expand_approved(&server, Decision::Rejected, Utf8Path::new("/w"), &lookup)
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
        assert_eq!(seen.borrow().len(), 0);
    }

    #[test]
    fn an_approved_server_expands_every_field() {
        let server = ProjectServer {
            name: "project-local".to_owned(),
            command: vec!["server".to_owned(), "${PROJECT_ROOT:-.}".to_owned()],
            environment: BTreeMap::from([("ROOT".to_owned(), "${WORKSPACE}".to_owned())]),
            headers: BTreeMap::from([("X-Root".to_owned(), "${PROJECT_ROOT:-.}".to_owned())]),
        };

        let expanded = expand_approved(
            &server,
            Decision::Approved,
            Utf8Path::new("/work/repo"),
            &env_of(&[("PROJECT_ROOT", "/checkout")]),
        )
        .expect("expand");

        assert_eq!(expanded.command, ["server", "/checkout"]);
        assert_eq!(expanded.environment["ROOT"], "/work/repo");
        assert_eq!(expanded.headers["X-Root"], "/checkout");
    }

    #[test]
    fn an_approved_server_reports_a_missing_variable_by_field() {
        let server = ProjectServer {
            name: "project-local".to_owned(),
            command: vec!["server".to_owned(), "${ABSENT}".to_owned()],
            environment: BTreeMap::new(),
            headers: BTreeMap::new(),
        };

        let err = expand_approved(
            &server,
            Decision::Approved,
            Utf8Path::new("/w"),
            &env_of(&[]),
        )
        .expect_err("missing");
        assert_eq!(err.code(), ErrorCode::MissingField);
        assert!(err.message().contains("ABSENT"));
    }

    #[test]
    fn an_approved_server_bounds_its_values_in_total() {
        let value = "x".repeat(MAX_VALUE_BYTES);
        let server = ProjectServer {
            name: "s".to_owned(),
            command: vec![value.clone(), value],
            environment: BTreeMap::new(),
            headers: BTreeMap::new(),
        };

        let err = expand_approved(
            &server,
            Decision::Approved,
            Utf8Path::new("/w"),
            &env_of(&[]),
        )
        .expect_err("over the aggregate budget");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("expansion"));
    }

    #[test]
    fn a_workspace_surface_keeps_every_declared_server_pending() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
        std::fs::write(
            workspace.join(PROJECT_FILE),
            r#"{"mcpServers": {
                "local": {"command": "server", "env": {"TOKEN": "${TOKEN}"}},
                "remote": {"type": "http", "url": "https://example.test/mcp"}
            }}"#,
        )
        .expect("write");

        let servers =
            ProjectServers::from_workspace(&workspace, &TrustStore::default()).expect("surface");
        assert_eq!(servers.pending, ["local", "remote"]);
        assert!(servers.startable().is_empty());

        // Approving one entry leaves the other pending and startable by nobody.
        let mut trust = TrustStore::default();
        trust.decide("remote", Decision::Approved);
        let servers = ProjectServers::from_workspace(&workspace, &trust).expect("surface");
        assert_eq!(servers.startable(), ["remote"]);
        assert_eq!(servers.pending, ["local"]);
    }

    #[test]
    fn a_workspace_without_a_file_has_an_empty_surface() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");

        let servers =
            ProjectServers::from_workspace(&workspace, &TrustStore::default()).expect("surface");
        assert!(servers.is_empty());
    }

    #[test]
    fn trust_decisions_replace_rather_than_accumulate() {
        let mut trust = TrustStore::default();
        trust.decide("s", Decision::Approved);
        trust.decide("s", Decision::Rejected);

        let servers = ProjectServers::derive(&["s".to_owned()], &trust);
        assert!(servers.approved.is_empty());
        assert_eq!(servers.rejected, ["s"]);
        assert!(!servers.is_startable("s"));

        trust.decide("s", Decision::Pending);
        let servers = ProjectServers::derive(&["s".to_owned()], &trust);
        assert_eq!(servers.pending, ["s"]);
    }

    #[test]
    fn approving_the_file_approves_every_entry() {
        let trust = TrustStore {
            enable_all: true,
            ..TrustStore::default()
        };
        let servers = ProjectServers::derive(&["a".to_owned(), "b".to_owned()], &trust);

        assert_eq!(servers.startable(), ["a".to_owned(), "b".to_owned()]);
        assert!(servers.pending.is_empty());
    }

    #[test]
    fn a_removed_server_leaves_no_authority_behind() {
        let mut trust = TrustStore::default();
        trust.decide("gone", Decision::Approved);

        let servers = ProjectServers::derive(&["kept".to_owned()], &trust);
        assert!(
            servers.approved.is_empty(),
            "a removed entry is not startable"
        );
        assert_eq!(servers.pending, ["kept"]);
        assert!(!servers.is_startable("gone"));
    }

    #[test]
    fn no_declared_server_is_an_empty_surface() {
        let servers = ProjectServers::derive(&[], &TrustStore::default());
        assert!(servers.is_empty());
        assert!(servers.startable().is_empty());
        assert_eq!(servers.to_string(), "no project servers");
    }

    #[test]
    fn the_trust_surface_names_states_and_never_values() {
        let mut trust = TrustStore::default();
        trust.decide("local", Decision::Approved);
        trust.decide("remote", Decision::Rejected);
        let servers = ProjectServers::derive(
            &["local".to_owned(), "remote".to_owned(), "other".to_owned()],
            &trust,
        );

        let rendered = servers.to_string();
        assert!(rendered.contains("approved: local"));
        assert!(rendered.contains("pending: other"));
        assert!(rendered.contains("rejected: remote"));
    }

    #[test]
    fn a_project_file_is_parsed_into_servers() {
        let text = r#"{
            "mcpServers": {
                "local": {
                    "command": "npx",
                    "args": ["-y", "@example/server", "${PROJECT_ROOT:-.}"],
                    "env": {"PROJECT_TOKEN": "${PROJECT_TOKEN}"}
                },
                "remote": {
                    "type": "http",
                    "url": "https://mcp.example.com/mcp",
                    "headers": {"X-Workspace": "${WORKSPACE_ID}"}
                }
            }
        }"#;
        let servers = parse_project_file(text, Utf8Path::new("/w/.mcp.json")).expect("parse");

        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "local");
        assert_eq!(
            servers[0].command,
            ["npx", "-y", "@example/server", "${PROJECT_ROOT:-.}"]
        );
        assert_eq!(servers[1].headers["X-Workspace"], "${WORKSPACE_ID}");
    }

    #[test]
    fn a_file_without_the_key_declares_nothing() {
        assert!(
            parse_project_file("{}", Utf8Path::new("/w/.mcp.json"))
                .expect("parse")
                .is_empty()
        );
    }

    #[test]
    fn a_malformed_project_file_is_refused() {
        let path = Utf8Path::new("/w/.mcp.json");
        assert_eq!(
            parse_project_file("not json", path)
                .expect_err("json")
                .code(),
            ErrorCode::InvalidConfiguration
        );
        assert_eq!(
            parse_project_file(r#"{"mcpServers": []}"#, path)
                .expect_err("shape")
                .code(),
            ErrorCode::InvalidField
        );
        assert_eq!(
            parse_project_file(r#"{"mcpServers": {"s": {"unknown": 1}}}"#, path)
                .expect_err("keys")
                .code(),
            ErrorCode::InvalidField
        );
    }

    #[test]
    fn a_missing_project_file_declares_nothing() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
        assert!(read_project_file(&workspace).expect("read").is_empty());
    }

    #[test]
    fn a_project_file_is_read_and_its_entries_stay_pending() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
        std::fs::write(
            workspace.join(PROJECT_FILE),
            r#"{"mcpServers": {"local": {"command": "server"}}}"#,
        )
        .expect("write");

        let declared: Vec<String> = read_project_file(&workspace)
            .expect("read")
            .into_iter()
            .map(|server| server.name)
            .collect();
        let servers = ProjectServers::derive(&declared, &TrustStore::default());
        assert_eq!(servers.pending, ["local"]);
        assert!(servers.startable().is_empty());
    }

    #[test]
    fn a_symlinked_project_file_is_refused() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
        let outside = workspace.join("outside.json");
        std::fs::write(&outside, r#"{"mcpServers": {}}"#).expect("write");
        std::os::unix::fs::symlink(&outside, workspace.join(PROJECT_FILE)).expect("symlink");

        let err = read_project_file(&workspace).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
    }

    #[test]
    fn an_oversized_project_file_is_refused_naming_the_size() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let workspace = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
        std::fs::write(
            workspace.join(PROJECT_FILE),
            "x".repeat(
                usize::try_from(MAX_PROJECT_BYTES)
                    .expect("fits")
                    .saturating_add(1),
            ),
        )
        .expect("write");

        let err = read_project_file(&workspace).expect_err("too large");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }
}
