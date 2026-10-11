//! Inspect and connect project MCP declarations without granting project trust.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;

use camino::Utf8Path;
use rune_context::mcp::{Client, ServerConfig};
use rune_context::mcp_trust::{self, Decision};
use rune_core::budget::LimitName;
use rune_core::config::Settings;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;
use rune_policy::trust::{self, TrustStore, WorkspaceTrust};
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli::Launch;

/// Owned by the interactive host. Dropping the client stops its stdio children.
pub struct Connection {
    pub client: Client,
    pub warnings: Vec<String>,
}

/// Connects approved, enabled stdio and HTTP servers for the interactive host.
/// Offline mode excludes HTTP before resolving configuration or credentials.
pub fn connect_interactive(
    settings: &Settings,
    paths: &Paths,
    workspace: &Utf8Path,
) -> Result<Option<Connection>> {
    let workspace = trust::canonical_workspace(workspace);
    let entries = project_entries(&workspace, settings)?;
    if entries.is_empty() {
        return Ok(None);
    }
    let trust = TrustStore::load(&paths.trust_file())?;
    let secrets = RefCell::new(Vec::new());
    let mut configs = Vec::new();
    let mut warnings = Vec::new();
    for (name, entry) in entries {
        let transport = transport_kind(&entry);
        // Filter before expansion or environment reads. Neither full-access
        // tool policy nor offline mode grants project startup trust.
        if decision(&trust, &workspace, &name) != Decision::Approved
            || !matches!(transport, "stdio" | "http")
            || (settings.offline && transport != "stdio")
            || entry.get("enabled").and_then(Value::as_bool) == Some(false)
        {
            continue;
        }
        match resolve(&name, &entry, &workspace, settings, &secrets) {
            Ok(config) => configs.push(config),
            Err(error) => {
                let message = redact(error.message(), &mut secrets.borrow_mut());
                if entry.get("required").and_then(Value::as_bool) == Some(true) {
                    return Err(RuneError::new(error.code(), message));
                }
                warnings.push(format!("MCP server `{name}`: {}: {message}", error.code()));
            }
        }
    }
    if configs.is_empty() && warnings.is_empty() {
        return Ok(None);
    }
    let client = Client::new(&settings.limits);
    let report = client.connect_all(&configs).map_err(|error| {
        RuneError::new(
            error.code(),
            redact(error.message(), &mut secrets.borrow_mut()),
        )
    })?;
    for failure in report.failed {
        warnings.push(format!(
            "MCP server `{}`: {}: {}",
            failure.server,
            failure.error.code(),
            redact(failure.error.message(), &mut secrets.borrow_mut())
        ));
    }
    Ok(Some(Connection { client, warnings }))
}

fn decision(trust: &TrustStore, workspace: &Utf8Path, name: &str) -> Decision {
    if trust.is_trusted(workspace)
        || trust
            .approved_entries(workspace)
            .contains(&format!("server:{name}"))
    {
        Decision::Approved
    } else if trust.decision(workspace) == Some(WorkspaceTrust::Rejected) {
        Decision::Rejected
    } else {
        Decision::Pending
    }
}

fn transport_kind(entry: &Value) -> &str {
    entry
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            if entry.get("url").is_some() {
                "http"
            } else {
                "stdio"
            }
        })
}

fn project_entries(workspace: &Utf8Path, settings: &Settings) -> Result<BTreeMap<String, Value>> {
    let Some(text) = mcp_trust::read_project_document(workspace)? else {
        return Ok(BTreeMap::new());
    };
    let document: Value = serde_json::from_str(&text).map_err(|_| {
        RuneError::new(
            ErrorCode::InvalidConfiguration,
            ".mcp.json is not valid JSON",
        )
    })?;
    let Some(entries) = document.get("mcpServers") else {
        return Ok(BTreeMap::new());
    };
    let entries = entries.as_object().ok_or_else(|| {
        RuneError::invalid_field("mcpServers", "expected an object of server declarations")
    })?;
    let cap = settings.limits.get_usize(LimitName::ListEntries);
    if entries.len() > cap {
        return Err(RuneError::too_large("mcpServers", entries.len(), cap));
    }
    Ok(entries
        .iter()
        .map(|(name, entry)| (name.clone(), entry.clone()))
        .collect())
}

/// Only diagnostic fields are serialized, never transport configuration.
#[derive(Serialize)]
pub struct Report {
    workspace: String,
    servers: Vec<Server>,
}

#[derive(Serialize)]
struct Server {
    name: String,
    source: &'static str,
    transport: &'static str,
    trust: Decision,
    enabled: bool,
    required: bool,
    status: &'static str,
    startup_error: Option<StartupError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    check: Option<Connectivity>,
}

#[derive(Serialize)]
struct Connectivity {
    protocol_version: Option<String>,
    tools: Vec<Tool>,
    warnings: Vec<String>,
    startup_timeout_ms: u64,
    operation_timeout_ms: u64,
    timed_out: bool,
}

#[derive(Serialize)]
struct Tool {
    name: String,
    server_tool: String,
}

#[derive(Serialize)]
struct StartupError {
    code: ErrorCode,
    message: String,
}

impl Report {
    pub fn check_failed(&self) -> bool {
        self.servers
            .iter()
            .any(|server| server.check.is_some() && server.status != "connected")
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.servers.is_empty() {
            out.push_str("no MCP servers configured\n");
        }
        for server in &self.servers {
            let trust = match server.trust {
                Decision::Approved => "approved",
                Decision::Pending => "pending",
                Decision::Rejected => "rejected",
            };
            let _ = writeln!(
                out,
                "{}  {}  {}  {}",
                server.name, server.transport, trust, server.status
            );
            if let Some(error) = &server.startup_error {
                let _ = writeln!(out, "  {}: {}", error.code, error.message);
            }
            if let Some(check) = &server.check {
                let _ = writeln!(
                    out,
                    "  protocol: {}; tools: {}",
                    check.protocol_version.as_deref().unwrap_or("unavailable"),
                    check.tools.len()
                );
                for tool in &check.tools {
                    let _ = writeln!(out, "  {} ({})", tool.server_tool, tool.name);
                }
                for warning in &check.warnings {
                    let _ = writeln!(out, "  warning: {warning}");
                }
                let _ = writeln!(
                    out,
                    "  timeouts: startup={} ms; operation={} ms; timed_out={}",
                    check.startup_timeout_ms, check.operation_timeout_ms, check.timed_out
                );
            }
        }
        out
    }
}

/// Attempts each eligible server separately so a required failure cannot hide
/// later failures. All child processes and connections close before returning.
pub fn run(
    settings: &Settings,
    paths: &Paths,
    launch: &Launch,
    workspace: &Utf8Path,
) -> Result<Report> {
    let (checking, selected) = match launch.args.as_slice() {
        [action] if action == "list" => (false, None),
        [action] if action == "check" => (true, None),
        [action, name] if action == "check" => (true, Some(name.as_str())),
        _ => {
            return Err(RuneError::invalid_field(
                "action",
                "expected `rune mcp list` or `rune mcp check [server]`",
            ));
        }
    };
    let workspace = trust::canonical_workspace(workspace);
    let trust = TrustStore::load(&paths.trust_file())?;
    let mut report = Report {
        workspace: workspace.to_string(),
        servers: Vec::new(),
    };
    let entries = project_entries(&workspace, settings)?;
    if entries.is_empty() {
        return empty_report(report, selected);
    }
    if let Some(name) = selected
        && !entries.contains_key(name)
    {
        return Err(unknown_server(name));
    }
    for (name, entry) in entries {
        if selected.is_some_and(|selected| selected != name) {
            continue;
        }
        report.servers.push(inspect(
            &name, &entry, &workspace, &trust, settings, checking,
        ));
    }
    Ok(report)
}

fn unknown_server(name: &str) -> RuneError {
    RuneError::new(ErrorCode::NotFound, format!("no MCP server named `{name}`"))
}

fn empty_report(report: Report, selected: Option<&str>) -> Result<Report> {
    match selected {
        Some(name) => Err(unknown_server(name)),
        None => Ok(report),
    }
}

fn inspect(
    name: &str,
    entry: &Value,
    workspace: &Utf8Path,
    trust: &TrustStore,
    settings: &Settings,
    checking: bool,
) -> Server {
    let decision = decision(trust, workspace, name);
    let kind = transport_kind(entry);
    let transport = match kind {
        "stdio" => "stdio",
        "http" => "http",
        "sse" => "sse",
        _ => "unknown",
    };
    let mut server = Server {
        name: name.to_owned(),
        source: mcp_trust::PROJECT_FILE,
        transport,
        trust: decision,
        enabled: entry
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        required: entry
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        status: "blocked",
        startup_error: None,
        check: checking.then(|| Connectivity {
            protocol_version: None,
            tools: Vec::new(),
            warnings: Vec::new(),
            startup_timeout_ms: settings.limits.get_bytes(LimitName::McpStartupTimeoutMs),
            operation_timeout_ms: settings.limits.get_bytes(LimitName::McpOperationTimeoutMs),
            timed_out: false,
        }),
    };
    if decision != Decision::Approved {
        return server;
    }
    if !server.enabled {
        server.status = "disabled";
        return server;
    }
    if settings.offline && transport != "stdio" {
        server.status = "offline";
        return server;
    }

    let secrets = RefCell::new(Vec::new());
    let result = resolve(name, entry, workspace, settings, &secrets).and_then(|config| {
        let client = Client::new(&settings.limits);
        // A separate client preserves required semantics while observing every
        // server, including those after a required server that fails.
        let result = client.connect_all(&[config]);
        if let Some(check) = &mut server.check {
            if let Some(status) = client.server(name) {
                check.protocol_version = status.protocol_version;
                check.warnings = status.warnings;
            }
            check.tools = client
                .tools()
                .into_iter()
                .map(|tool| Tool {
                    name: tool.name,
                    server_tool: tool.server_tool,
                })
                .collect();
        }
        client.shutdown();
        let report = result?;
        match report.failed.into_iter().next() {
            Some(failure) => Err(failure.error),
            None => Ok(()),
        }
    });
    let mut secrets = secrets.into_inner();
    if let Some(check) = &mut server.check {
        for tool in &mut check.tools {
            tool.name = redact(&tool.name, &mut secrets);
            tool.server_tool = redact(&tool.server_tool, &mut secrets);
        }
        for warning in &mut check.warnings {
            *warning = redact(warning, &mut secrets);
        }
    }
    match result {
        Ok(()) => server.status = "connected",
        Err(error) => {
            server.status = "failed";
            if let Some(check) = &mut server.check {
                check.timed_out = error.code() == ErrorCode::Timeout;
            }
            server.startup_error = Some(StartupError {
                code: error.code(),
                message: redact(error.message(), &mut secrets),
            });
        }
    }
    server
}

fn resolve(
    name: &str,
    entry: &Value,
    workspace: &Utf8Path,
    settings: &Settings,
    secrets: &RefCell<Vec<String>>,
) -> Result<ServerConfig> {
    // Deserialization errors can quote the offending secret value. Report a
    // fixed message for malformed configuration rather than copying that text.
    let invalid = || {
        RuneError::new(
            ErrorCode::InvalidConfiguration,
            "invalid MCP server declaration",
        )
    };
    if entry.get("type").is_some_and(|value| !value.is_string()) {
        return Err(invalid());
    }
    for field in ["enabled", "required"] {
        if entry.get(field).is_some_and(|value| !value.is_boolean()) {
            return Err(invalid());
        }
    }
    let single = json!({"mcpServers": {name: entry}});
    let declared = mcp_trust::parse_project_file(&single.to_string(), &workspace.join(".mcp.json"))
        .map_err(|_| invalid())?;
    let raw = declared.first().ok_or_else(invalid)?;
    secrets.borrow_mut().extend(raw.headers.values().cloned());
    secrets
        .borrow_mut()
        .extend(raw.environment.values().cloned());
    let env = |variable: &str| {
        let value = std::env::var(variable).ok();
        if let Some(value) = &value {
            secrets.borrow_mut().push(value.clone());
        }
        value
    };
    let expanded = mcp_trust::expand_approved(raw, Decision::Approved, workspace, &env)?;
    secrets
        .borrow_mut()
        .extend(expanded.headers.values().cloned());
    secrets
        .borrow_mut()
        .extend(expanded.environment.values().cloned());
    let kind = transport_kind(entry);
    let transport = match kind {
        "stdio" if entry.get("url").is_none() => json!({
            "transport": "stdio", "command": expanded.command, "environment": expanded.environment
        }),
        "http" | "sse" if entry.get("command").is_none() => {
            let url = entry
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?;
            let url = mcp_trust::expand_template(url, workspace, &env)?;
            secrets.borrow_mut().push(url.clone());
            if kind == "sse" {
                // The legacy transport cannot carry configured headers.
                if !expanded.headers.is_empty() {
                    return Err(RuneError::new(
                        ErrorCode::Unsupported,
                        "SSE headers are not supported",
                    ));
                }
                json!({"transport": "sse", "url": url})
            } else {
                json!({"transport": "http", "url": url, "headers": expanded.headers})
            }
        }
        _ => return Err(invalid()),
    };
    ServerConfig::parse(&json!({
        "name": name,
        "transport": transport,
        "required": entry.get("required").and_then(Value::as_bool).unwrap_or(false),
        "startup_timeout_ms": settings.limits.get_bytes(LimitName::McpStartupTimeoutMs),
        "operation_timeout_ms": settings.limits.get_bytes(LimitName::McpOperationTimeoutMs),
        "restart_limit": settings.limits.get_bytes(LimitName::McpRestartLimit),
    }))
    .map_err(|_| invalid())
}

fn redact(message: &str, secrets: &mut [String]) -> String {
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    let mut message = message.to_owned();
    for secret in secrets.iter().filter(|value| !value.is_empty()) {
        message = message.replace(secret, "<redacted>");
    }
    // Keep untrusted diagnostics on one terminal line in the text view.
    message.chars().filter(|ch| !ch.is_control()).collect()
}
