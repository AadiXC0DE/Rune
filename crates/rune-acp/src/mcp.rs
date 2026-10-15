//! Converts ACP stdio declarations to the shared, bounded MCP client.

use std::collections::BTreeMap;
use std::sync::Arc;

use camino::Utf8Path;
use rune_context::mcp::{Client, ServerConfig};
use rune_core::budget::{BudgetSet, LimitName};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::jsonrpc::RpcError;

#[derive(Deserialize)]
struct StdioServer {
    #[serde(rename = "type")]
    transport: Option<String>,
    name: String,
    command: String,
    args: Vec<String>,
    env: Vec<EnvironmentVariable>,
}

#[derive(Deserialize)]
struct EnvironmentVariable {
    name: String,
    value: String,
}

/// Validates the complete declaration before starting any child. Requested
/// servers are required: a startup failure must not look like a ready session.
pub(crate) fn connect(params: &Value, limits: &BudgetSet) -> Result<Option<Arc<Client>>, RpcError> {
    let Some(value) = params.get("mcpServers") else {
        return Ok(None);
    };
    let servers: Vec<StdioServer> = serde_json::from_value(value.clone())
        .map_err(|err| RpcError::invalid_params(format!("mcpServers: {err}")))?;
    if servers.is_empty() {
        return Ok(None);
    }
    let mut declarations = Vec::with_capacity(servers.len());
    for server in servers {
        if server
            .transport
            .as_deref()
            .is_some_and(|kind| kind != "stdio")
        {
            return Err(RpcError::invalid_params(
                "mcpServers: only stdio is supported",
            ));
        }
        if !Utf8Path::new(&server.command).is_absolute() {
            return Err(RpcError::invalid_params(
                "mcpServers.command must be an absolute path",
            ));
        }
        let mut command = vec![server.command];
        command.extend(server.args);
        let mut environment = BTreeMap::new();
        for variable in server.env {
            if environment.insert(variable.name, variable.value).is_some() {
                return Err(RpcError::invalid_params(
                    "mcpServers.env repeats a variable name",
                ));
            }
        }
        declarations.push(json!({
            "name": server.name,
            "transport": {"transport": "stdio", "command": command, "environment": environment},
            "required": true,
            "startup_timeout_ms": limits.get_bytes(LimitName::McpStartupTimeoutMs),
            "operation_timeout_ms": limits.get_bytes(LimitName::McpOperationTimeoutMs),
            "restart_limit": limits.get_bytes(LimitName::McpRestartLimit),
        }));
    }
    let configs = ServerConfig::parse_list(&Value::Array(declarations))
        .map_err(|err| RpcError::from_rune(&err))?;
    let client = Arc::new(Client::new(limits));
    client
        .connect_all(&configs)
        .map_err(|err| RpcError::from_rune(&err))?;
    // Sanitized server names can collide even when the declarations are
    // distinct. Refuse ambiguity before advertising any tools.
    let mut names = std::collections::BTreeSet::new();
    for tool in client.tools() {
        if !names.insert(tool.name) {
            return Err(RpcError::invalid_params(
                "mcpServers contribute duplicate tool names",
            ));
        }
    }
    Ok(Some(client))
}
