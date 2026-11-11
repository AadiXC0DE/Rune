//! Inspect and connect project MCP declarations without granting project trust.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::sync::Arc;

use camino::Utf8Path;
use rune_agent::steering::Cancellation;
use rune_context::mcp::{Client, ProjectedTool, ServerConfig};
use rune_context::mcp_trust::{self, Decision};
use rune_core::budget::LimitName;
use rune_core::config::{McpTools, Settings};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;
use rune_core::tool::ToolSpec;
use rune_policy::trust::{self, TrustStore, WorkspaceTrust};
use rune_tools::mcp_tools::{McpCall, McpDirectory, McpEntry};
use rune_tools::skill::{CapabilitySearch, Catalog};
use rune_tools::{ExecutionContext, Registry, ToolOutput};
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli::Launch;

/// Reads one external config and returns project JSON without writing or
/// connecting. The bounded read also catches a file growing after it is opened.
pub fn import(settings: &Settings, launch: &Launch) -> Result<String> {
    let [action, format, path] = launch.args.as_slice() else {
        return Err(RuneError::invalid_field(
            "import",
            "expected `rune mcp import claude-desktop <file>`",
        ));
    };
    if action != "import" || format != "claude-desktop" {
        return Err(RuneError::invalid_field(
            "import",
            "the supported import format is `claude-desktop`",
        ));
    }
    if !std::fs::metadata(path)?.is_file() {
        return Err(RuneError::invalid_field(
            "import",
            "expected a regular file",
        ));
    }
    let file = std::fs::File::open(path)?;
    let cap = mcp_trust::MAX_PROJECT_BYTES;
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(RuneError::too_large("import", bytes.len(), cap as usize));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| RuneError::invalid_field("import", "expected UTF-8 JSON"))?;
    let document = rune_context::mcp::config::import_claude_desktop(
        text,
        settings.limits.get_usize(LimitName::ListEntries),
    )?;
    let output = serde_json::to_string_pretty(&document)?;
    // Include println's newline. A compact source can become larger after
    // formatting; every emitted document must fit Rune's project file limit.
    let output_bytes = output.len().saturating_add(1);
    if output_bytes > cap as usize {
        return Err(RuneError::too_large("import", output_bytes, cap as usize));
    }
    Ok(output)
}

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

/// Combined schema bytes up to which `auto` advertises every MCP tool.
pub const EAGER_SCHEMA_BYTES: usize = 2_048;

/// Bytes the connected tools would add to every request, counted as the
/// compact JSON of each name, description and schema.
#[must_use]
pub fn combined_schema_bytes(tools: &[ProjectedTool]) -> usize {
    tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
            .to_string()
            .len()
        })
        .sum()
}

/// Whether the connected tools are advertised one by one rather than found
/// by search and called through `mcp_call`.
#[must_use]
pub fn is_eager(setting: McpTools, tools: &[ProjectedTool]) -> bool {
    match setting {
        McpTools::Eager => true,
        McpTools::Deferred => false,
        McpTools::Auto => combined_schema_bytes(tools) <= EAGER_SCHEMA_BYTES,
    }
}

/// Offers the connected tools to the model, deciding once how.
///
/// Returns the client when its tools are advertised one by one, for the host
/// to route them by name. Otherwise registers `mcp_call` and the MCP-aware
/// `capability_search` over it and returns `None`; the registry then owns the
/// client.
pub fn attach(
    client: Client,
    settings: &Settings,
    registry: &mut Registry,
    catalog: Arc<dyn Catalog>,
    cancellation: Cancellation,
) -> Result<Option<Client>> {
    if is_eager(settings.mcp_tools, &client.tools()) {
        return Ok(Some(client));
    }
    let directory: Arc<dyn McpDirectory> = Arc::new(ClientDirectory(client, cancellation));
    registry.insert(Box::new(McpCall::new(Arc::clone(&directory))))?;
    registry.replace(Box::new(CapabilitySearch::with_mcp(
        catalog,
        directory,
        settings.limits.get_usize(LimitName::McpSelectedSchemaBytes),
    )));
    Ok(None)
}

/// Appends every connected tool's schema to an advertisement.
pub fn append_tools(tools: &mut Vec<ToolSpec>, client: &Client) -> Result<()> {
    for tool in client.tools() {
        if tools.iter().any(|existing| existing.name == tool.name) {
            return Err(RuneError::new(
                ErrorCode::AlreadyExists,
                format!("two tools are advertised as `{}`", tool.name),
            ));
        }
        tools.push(ToolSpec {
            name: tool.name,
            description: tool.description,
            input_schema: tool.input_schema,
        });
    }
    Ok(())
}

/// Calls one tool by its advertised name, as a tool result.
///
/// Either flag stops the call: the run's, and the one of the context it runs in.
pub fn call_tool(
    client: &Client,
    name: &str,
    arguments: &Value,
    cancellation: &Cancellation,
    context: &ExecutionContext,
) -> Result<ToolOutput> {
    cancellation.check()?;
    context.check_cancelled()?;
    let result = client.call_cancellable(name, arguments, &|| {
        cancellation.is_cancelled() || context.is_cancelled()
    });
    cancellation.check()?;
    context.check_cancelled()?;
    match result {
        Ok(outcome) => Ok(ToolOutput {
            produced_bytes: outcome.text.len() as u64,
            text: outcome.text,
            is_error: outcome.is_error,
            plan_update: None,
            full_text: None,
            diff: None,
            image: None,
            check: None,
        }),
        Err(error) if error.code() == ErrorCode::Cancelled => Err(error),
        Err(error) => Ok(ToolOutput::from_error(&error)),
    }
}

/// The tools of a live client, for `mcp_call` and the search, with the run's
/// cancellation flag.
pub struct ClientDirectory(pub Client, pub Cancellation);

impl McpDirectory for ClientDirectory {
    fn entries(&self) -> Vec<McpEntry> {
        self.0
            .tools()
            .into_iter()
            .map(|tool| McpEntry {
                server: tool.server,
                tool: tool.server_tool,
                description: tool.description,
                input_schema: tool.input_schema,
            })
            .collect()
    }

    fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: &Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        let name = self
            .0
            .tools()
            .into_iter()
            .find(|projected| projected.server == server && projected.server_tool == tool)
            .map(|projected| projected.name)
            .ok_or_else(|| {
                RuneError::new(ErrorCode::NotFound, format!("no MCP tool {server}/{tool}"))
            })?;
        call_tool(&self.0, &name, arguments, &self.1, context)
    }
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
                "expected `rune mcp list`, `rune mcp check [server]`, or `rune mcp import claude-desktop <file>`",
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

/// Decides a call by the rules, holding a deferred `mcp_call` to any rule
/// written for the tool's own name as well, so deferring a server never
/// loosens what a user set. The stricter of the two answers wins.
pub fn decide_call(
    rules: &rune_policy::rules::RuleSet,
    mode: rune_core::config::PermissionMode,
    name: &str,
    target: Option<&str>,
    workspace: &Utf8Path,
) -> (rune_policy::decision::Outcome, String) {
    use rune_policy::decision::Outcome;
    let decided = rune_agent::turn::decide_call_in_workspace(rules, mode, name, target, workspace);
    let Some((server, tool)) = target
        .filter(|_| name == "mcp_call")
        .and_then(|target| target.strip_prefix("mcp:"))
        .and_then(|target| target.split_once('/'))
    else {
        return decided;
    };
    let direct_name = rune_context::mcp::schema::wire_name(server, tool);
    let fallback = rune_policy::decision::mode_default(mode);
    if !rules
        .evaluate_in_workspace(&direct_name, "", fallback, workspace)
        .from_rule
    {
        return decided;
    }
    let direct =
        rune_agent::turn::decide_call_in_workspace(rules, mode, &direct_name, None, workspace);
    let rank = |outcome: Outcome| match outcome {
        Outcome::Allow => 0,
        Outcome::Ask => 1,
        Outcome::Deny => 2,
    };
    if rank(direct.0) > rank(decided.0) {
        direct
    } else {
        decided
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rule_for_the_direct_name_holds_a_deferred_call() {
        use rune_core::config::PermissionMode;
        use rune_policy::decision::{Layer, Outcome};
        use rune_policy::rules::{Rule, RuleSet};
        let workspace = Utf8Path::new("/tmp/rune-mcp-rules");
        let mut rules = RuleSet::new();
        rules.push(Rule::allow("mcp_call", "*", Layer::User));
        rules.push(Rule::deny("mcp_github_delete_repo", "*", Layer::User));
        let decide = |target: &str| {
            decide_call(
                &rules,
                PermissionMode::Auto,
                "mcp_call",
                Some(target),
                workspace,
            )
            .0
        };
        assert_eq!(decide("mcp:github/delete_repo"), Outcome::Deny);
        // With no rule for the direct name, the rule for mcp_call stands.
        assert_eq!(decide("mcp:github/list_issues"), Outcome::Allow);
        // Other tools are decided as before.
        assert_eq!(
            decide_call(
                &rules,
                PermissionMode::Auto,
                "mcp_github_delete_repo",
                None,
                workspace
            )
            .0,
            Outcome::Deny
        );
    }

    fn tool(server: &str, name: &str, schema_bytes: usize) -> ProjectedTool {
        ProjectedTool {
            name: format!("mcp_{server}_{name}"),
            server: server.to_owned(),
            server_tool: name.to_owned(),
            description: String::new(),
            input_schema: json!({ "type": "object", "description": "d".repeat(schema_bytes) }),
        }
    }

    #[test]
    fn the_mode_follows_the_setting_and_the_combined_schema_size() {
        let small = [tool("one", "a", 850), tool("one", "b", 850)];
        let combined = combined_schema_bytes(&small);
        assert!((1_800..=2_048).contains(&combined), "{combined}");
        assert!(is_eager(McpTools::Auto, &small));
        assert!(!is_eager(McpTools::Deferred, &small));

        let many: Vec<_> = (0..40)
            .map(|index| tool("big", &format!("tool_{index}"), 40))
            .collect();
        assert!(combined_schema_bytes(&many) > EAGER_SCHEMA_BYTES);
        assert!(!is_eager(McpTools::Auto, &many));

        let huge = [tool("huge", "a", 200_000)];
        assert!(is_eager(McpTools::Eager, &huge));
        assert!(!is_eager(McpTools::Auto, &huge));
        assert!(is_eager(McpTools::Auto, &[]));
    }
}
