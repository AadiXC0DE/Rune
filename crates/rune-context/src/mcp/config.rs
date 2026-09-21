//! MCP server configuration.
//!
//! A server is named, carries one transport, and carries its own timeouts and
//! restart budget, so a slow server cannot inherit the settings of another. The
//! compiled defaults are read from the limit table rather than written twice, so
//! `rune limits` reports what a server that omits a value will actually use.
//!
//! A literal `authorization` header is refused at parse time. A credential
//! written into a profile file is a credential in a file that gets copied,
//! shared, and committed; naming an environment variable instead keeps the value
//! out of every artifact Rune writes.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use rune_core::budget::{EMERGENCY_CEILING_BYTES, LimitName};
use rune_core::error::{Result, RuneError};

/// Longest accepted server name.
///
/// Bounded because the name prefixes every tool the server contributes and a
/// tool name is capped at 64 bytes, so a longer name could not stay distinct.
pub const MAX_SERVER_NAME_BYTES: usize = 32;

/// How a server is reached.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transport {
    /// A child process speaking JSON-RPC on its standard input and output.
    Stdio {
        /// Program and arguments, the program first.
        command: Vec<String>,
        /// Variables added to the environment the child inherits.
        #[serde(default)]
        environment: BTreeMap<String, String>,
    },
    /// A streamable HTTP endpoint.
    Http {
        /// Endpoint every request is posted to.
        url: String,
        /// Headers sent with every request.
        #[serde(default)]
        headers: BTreeMap<String, String>,
        /// Headers whose value is read from an environment variable. The key is
        /// the header name and the value is the variable name.
        #[serde(default)]
        header_env: BTreeMap<String, String>,
        /// Environment variable holding a bearer token.
        #[serde(default)]
        bearer_token_env: Option<String>,
    },
    /// The legacy pair of an event stream and a message endpoint.
    Sse {
        /// Endpoint opened as an event stream.
        url: String,
    },
}

impl Transport {
    /// Returns the transport name used in messages.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
            Self::Sse { .. } => "sse",
        }
    }

    /// Validates every field of the transport.
    fn validate(&self) -> Result<()> {
        match self {
            Self::Stdio {
                command,
                environment,
            } => {
                let Some(program) = command.first() else {
                    return Err(RuneError::invalid_field(
                        "command",
                        "a stdio server needs a program to run",
                    ));
                };
                if program.is_empty() {
                    return Err(RuneError::invalid_field(
                        "command",
                        "the program name cannot be empty",
                    ));
                }
                for (name, value) in environment {
                    check_variable("environment", name)?;
                    check_variable_value("environment", value)?;
                }
                Ok(())
            }
            Self::Http {
                url,
                headers,
                header_env,
                bearer_token_env,
            } => {
                check_url("url", url)?;
                for (name, value) in headers {
                    check_header_name("headers", name)?;
                    check_header_value("headers", value)?;
                    if name.eq_ignore_ascii_case("authorization") {
                        return Err(RuneError::invalid_field(
                            "headers",
                            "a literal authorization header is not accepted",
                        )
                        .with_hint(
                            "name an environment variable with header_env, or set bearer_token_env",
                        ));
                    }
                }
                for (name, variable) in header_env {
                    check_header_name("header_env", name)?;
                    check_variable("header_env", variable)?;
                }
                if let Some(variable) = bearer_token_env {
                    check_variable("bearer_token_env", variable)?;
                }
                Ok(())
            }
            Self::Sse { url } => check_url("url", url),
        }
    }
}

impl fmt::Debug for Transport {
    /// Renders the transport with every header and environment value replaced.
    ///
    /// Configuration is printed by status and diagnostic output, and a custom
    /// header or a child environment value routinely carries a credential.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio {
                command,
                environment,
            } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("environment", &Redacted(environment))
                .finish(),
            Self::Http {
                url,
                headers,
                header_env,
                bearer_token_env,
            } => f
                .debug_struct("Http")
                .field("url", url)
                .field("headers", &Redacted(headers))
                .field("header_env", header_env)
                .field("bearer_token_env", bearer_token_env)
                .finish(),
            Self::Sse { url } => f.debug_struct("Sse").field("url", url).finish(),
        }
    }
}

/// Renders a map with its values replaced, keeping the keys.
struct Redacted<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.keys().map(|name| (name, "<redacted>")))
            .finish()
    }
}

/// One configured server.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(try_from = "RawServerConfig")]
pub struct ServerConfig {
    /// Name used to namespace the server's tools.
    pub name: String,
    /// How the server is reached.
    pub transport: Transport,
    /// Whether the server is connected at all.
    pub enabled: bool,
    /// Whether a failed connection fails the whole set.
    pub required: bool,
    /// Longest wait for the server to start and complete initialization.
    pub startup_timeout_ms: u64,
    /// Longest wait for any other operation.
    pub operation_timeout_ms: u64,
    /// Automatic restarts permitted before the server is left failed.
    pub restart_limit: u32,
}

impl ServerConfig {
    /// Returns the startup timeout.
    #[must_use]
    pub fn startup_timeout(&self) -> Duration {
        Duration::from_millis(self.startup_timeout_ms)
    }

    /// Returns the per-operation timeout.
    #[must_use]
    pub fn operation_timeout(&self) -> Duration {
        Duration::from_millis(self.operation_timeout_ms)
    }

    /// Parses one server from a JSON document, validating every field.
    pub fn parse(value: &Value) -> Result<Self> {
        let raw: RawServerConfig = serde_json::from_value(value.clone())?;
        Self::try_from(raw)
    }

    /// Parses one server from JSON text.
    pub fn parse_str(text: &str) -> Result<Self> {
        let raw: RawServerConfig = serde_json::from_str(text)?;
        Self::try_from(raw)
    }

    /// Parses a list of servers, refusing a repeated name.
    ///
    /// Two servers sharing a name would contribute colliding tool prefixes, so
    /// the document is refused rather than resolved by order.
    pub fn parse_list(value: &Value) -> Result<Vec<Self>> {
        let items = value.as_array().ok_or_else(|| {
            RuneError::invalid_field("servers", "expected an array of server objects")
        })?;
        let mut parsed: Vec<Self> = Vec::with_capacity(items.len());
        for item in items {
            let config = Self::parse(item)?;
            if parsed.iter().any(|existing| existing.name == config.name) {
                return Err(RuneError::invalid_field(
                    "name",
                    format!("`{}` is declared more than once", config.name),
                ));
            }
            parsed.push(config);
        }
        Ok(parsed)
    }

    /// Checks every field that a server needs a well-formed value for.
    fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(RuneError::invalid_field(
                "name",
                "a server name cannot be empty",
            ));
        }
        if self.name.len() > MAX_SERVER_NAME_BYTES {
            return Err(RuneError::invalid_field(
                "name",
                format!(
                    "`{}` is {} bytes, the limit is {MAX_SERVER_NAME_BYTES}",
                    self.name,
                    self.name.len()
                ),
            ));
        }
        if !self
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(RuneError::invalid_field(
                "name",
                "a server name accepts letters, digits, `_`, and `-` only",
            ));
        }
        self.transport.validate()
    }
}

/// The wire form, deserialized before validation.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServerConfig {
    name: String,
    transport: Transport,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_required")]
    required: bool,
    #[serde(default = "default_startup_timeout_ms")]
    startup_timeout_ms: u64,
    #[serde(default = "default_operation_timeout_ms")]
    operation_timeout_ms: u64,
    #[serde(default = "default_restart_limit")]
    restart_limit: u32,
}

impl TryFrom<RawServerConfig> for ServerConfig {
    type Error = RuneError;

    fn try_from(raw: RawServerConfig) -> Result<Self> {
        let config = Self {
            name: raw.name,
            transport: raw.transport,
            enabled: raw.enabled,
            required: raw.required,
            startup_timeout_ms: raw.startup_timeout_ms,
            operation_timeout_ms: raw.operation_timeout_ms,
            restart_limit: raw.restart_limit,
        };
        config.validate()?;
        Ok(config)
    }
}

/// The compiled default of a numeric limit.
fn limit_default(name: LimitName) -> u64 {
    name.default_value().effective(EMERGENCY_CEILING_BYTES)
}

fn default_enabled() -> bool {
    true
}

fn default_required() -> bool {
    false
}

fn default_startup_timeout_ms() -> u64 {
    limit_default(LimitName::McpStartupTimeoutMs)
}

fn default_operation_timeout_ms() -> u64 {
    limit_default(LimitName::McpOperationTimeoutMs)
}

fn default_restart_limit() -> u32 {
    u32::try_from(limit_default(LimitName::McpRestartLimit)).unwrap_or(1)
}

/// Refuses a header name that cannot be sent.
fn check_header_name(field: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(RuneError::invalid_field(
            field,
            "a header name cannot be empty",
        ));
    }
    if name
        .bytes()
        .any(|byte| byte < 0x20 || byte == 0x7f || byte == b':')
    {
        return Err(RuneError::invalid_field(
            field,
            format!("`{name}` is not a valid header name"),
        ));
    }
    Ok(())
}

/// Refuses a header value that would break the request framing.
fn check_header_value(field: &str, value: &str) -> Result<()> {
    if value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(RuneError::invalid_field(
            field,
            "a header value cannot contain a control character",
        ));
    }
    Ok(())
}

/// Refuses an environment variable name that cannot be read or set.
fn check_variable(field: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(RuneError::invalid_field(
            field,
            "an environment variable name cannot be empty",
        ));
    }
    if name.contains('=') || name.contains('\0') {
        return Err(RuneError::invalid_field(
            field,
            format!("`{name}` is not a valid environment variable name"),
        ));
    }
    Ok(())
}

/// Refuses an environment value the process could not carry.
fn check_variable_value(field: &str, value: &str) -> Result<()> {
    if value.contains('\0') {
        return Err(RuneError::invalid_field(
            field,
            "an environment value cannot contain a null byte",
        ));
    }
    Ok(())
}

/// Refuses an endpoint that is not an HTTP URL.
fn check_url(field: &str, url: &str) -> Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(RuneError::invalid_field(
            field,
            "a remote server needs an http:// or https:// URL",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stdio_document() -> Value {
        json!({
            "name": "files",
            "transport": { "transport": "stdio", "command": ["mcp-server", "--root", "."] }
        })
    }

    #[test]
    fn omitted_fields_take_the_compiled_defaults() {
        let config = ServerConfig::parse(&stdio_document()).expect("parsed");
        assert!(config.enabled);
        assert!(!config.required);
        assert_eq!(config.startup_timeout_ms, 30_000);
        assert_eq!(config.operation_timeout_ms, 60_000);
        assert_eq!(config.restart_limit, 1);
        assert_eq!(config.name, "files");
        assert_eq!(config.transport.kind(), "stdio");
    }

    #[test]
    fn explicit_fields_win_over_the_defaults() {
        let mut document = stdio_document();
        document["enabled"] = json!(false);
        document["required"] = json!(true);
        document["startup_timeout_ms"] = json!(1_500);
        document["operation_timeout_ms"] = json!(2_500);
        document["restart_limit"] = json!(3);
        let config = ServerConfig::parse(&document).expect("parsed");
        assert!(!config.enabled);
        assert!(config.required);
        assert_eq!(config.startup_timeout(), Duration::from_millis(1_500));
        assert_eq!(config.operation_timeout(), Duration::from_millis(2_500));
        assert_eq!(config.restart_limit, 3);
    }

    #[test]
    fn a_literal_authorization_header_is_refused_whatever_its_case() {
        for name in ["authorization", "Authorization", "AUTHORIZATION"] {
            let document = json!({
                "name": "remote",
                "transport": {
                    "transport": "http",
                    "url": "https://example.test/mcp",
                    "headers": { name: "Bearer secret" }
                }
            });
            let error = ServerConfig::parse(&document).expect_err("refused");
            assert_eq!(error.code(), rune_core::error::ErrorCode::InvalidField);
            assert_eq!(error.field(), Some("headers"));
        }
    }

    #[test]
    fn an_authorization_header_from_the_environment_is_accepted() {
        let document = json!({
            "name": "remote",
            "transport": {
                "transport": "http",
                "url": "https://example.test/mcp",
                "header_env": { "Authorization": "RUNE_MCP_TOKEN" }
            }
        });
        let config = ServerConfig::parse(&document).expect("parsed");
        assert_eq!(config.transport.kind(), "http");
    }

    #[test]
    fn a_bearer_token_variable_is_accepted() {
        let document = json!({
            "name": "remote",
            "transport": {
                "transport": "http",
                "url": "https://example.test/mcp",
                "bearer_token_env": "RUNE_MCP_TOKEN"
            }
        });
        assert!(ServerConfig::parse(&document).is_ok());
    }

    #[test]
    fn an_invalid_server_name_is_refused() {
        for name in ["", "with space", "with.dot", "with/slash", "café"] {
            let mut document = stdio_document();
            document["name"] = json!(name);
            let error = ServerConfig::parse(&document).expect_err("refused");
            assert_eq!(error.field(), Some("name"), "name `{name}` was accepted");
        }
        let mut document = stdio_document();
        document["name"] = json!("a".repeat(MAX_SERVER_NAME_BYTES + 1));
        assert_eq!(
            ServerConfig::parse(&document).expect_err("refused").field(),
            Some("name")
        );
    }

    #[test]
    fn a_remote_server_needs_an_http_url() {
        let document = json!({
            "name": "remote",
            "transport": { "transport": "http", "url": "example.test/mcp" }
        });
        assert_eq!(
            ServerConfig::parse(&document).expect_err("refused").field(),
            Some("url")
        );
    }

    #[test]
    fn a_stdio_server_needs_a_program() {
        let document = json!({
            "name": "local",
            "transport": { "transport": "stdio", "command": [] }
        });
        assert_eq!(
            ServerConfig::parse(&document).expect_err("refused").field(),
            Some("command")
        );
    }

    #[test]
    fn a_header_value_with_a_newline_is_refused() {
        let document = json!({
            "name": "remote",
            "transport": {
                "transport": "http",
                "url": "https://example.test/mcp",
                "headers": { "x-note": "a\r\nx-injected: b" }
            }
        });
        assert_eq!(
            ServerConfig::parse(&document).expect_err("refused").field(),
            Some("headers")
        );
    }

    #[test]
    fn an_environment_entry_the_process_could_not_carry_is_refused() {
        let document = json!({
            "name": "local",
            "transport": {
                "transport": "stdio",
                "command": ["mcp-server"],
                "environment": { "A=B": "value" }
            }
        });
        assert_eq!(
            ServerConfig::parse(&document).expect_err("refused").field(),
            Some("environment")
        );
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let mut document = stdio_document();
        document["timeout"] = json!(10);
        assert!(ServerConfig::parse(&document).is_err());
    }

    #[test]
    fn a_configuration_round_trips_through_json() {
        let document = json!({
            "name": "remote",
            "transport": {
                "transport": "http",
                "url": "https://example.test/mcp",
                "headers": { "x-tenant": "acme" },
                "header_env": { "Authorization": "RUNE_MCP_TOKEN" },
                "bearer_token_env": "RUNE_MCP_TOKEN"
            },
            "enabled": true,
            "required": true,
            "startup_timeout_ms": 1000,
            "operation_timeout_ms": 2000,
            "restart_limit": 2
        });
        let config = ServerConfig::parse(&document).expect("parsed");
        let text = serde_json::to_string(&config).expect("serialized");
        assert_eq!(ServerConfig::parse_str(&text).expect("reparsed"), config);
    }

    #[test]
    fn a_list_refuses_a_repeated_name() {
        let document = json!([
            { "name": "files", "transport": { "transport": "stdio", "command": ["a"] } },
            { "name": "files", "transport": { "transport": "stdio", "command": ["b"] } }
        ]);
        assert_eq!(
            ServerConfig::parse_list(&document)
                .expect_err("refused")
                .field(),
            Some("name")
        );
    }

    #[test]
    fn a_list_keeps_the_declared_order() {
        let document = json!([
            { "name": "one", "transport": { "transport": "stdio", "command": ["a"] } },
            { "name": "two", "transport": { "transport": "stdio", "command": ["b"] } }
        ]);
        let servers = ServerConfig::parse_list(&document).expect("parsed");
        assert_eq!(
            servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[test]
    fn a_list_that_is_not_an_array_is_refused() {
        assert_eq!(
            ServerConfig::parse_list(&json!({}))
                .expect_err("refused")
                .field(),
            Some("servers")
        );
    }

    #[test]
    fn debug_output_redacts_header_and_environment_values() {
        let document = json!({
            "name": "remote",
            "transport": {
                "transport": "http",
                "url": "https://example.test/mcp",
                "headers": { "x-api-key": "sk-live-secret" }
            }
        });
        let config = ServerConfig::parse(&document).expect("parsed");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("sk-live-secret"), "{rendered}");
        assert!(rendered.contains("x-api-key"), "{rendered}");
    }
}
