//! MCP server tools reached through search and one dispatcher.
//!
//! A server with a large catalog is not advertised tool by tool. The model
//! finds a tool with `capability_search`, reads its schema in the result, and
//! calls it through `mcp_call`, so the tool list stays the same whatever the
//! servers hold.

use std::sync::Arc;

use rune_context::mcp::schema::check_arguments;
use rune_core::error::{Result, RuneError};

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput, validate_closed_arguments};

/// One tool a connected MCP server offers.
#[derive(Clone, PartialEq, Debug)]
pub struct McpEntry {
    /// Server the tool belongs to.
    pub server: String,
    /// Name the server knows the tool by.
    pub tool: String,
    /// The tool's description.
    pub description: String,
    /// The server's input schema for the tool.
    pub input_schema: serde_json::Value,
}

/// The connected MCP tools, as the dispatcher and the search see them.
pub trait McpDirectory: Send + Sync {
    /// Returns every tool the connected servers offer now.
    fn entries(&self) -> Vec<McpEntry>;

    /// Calls one tool with arguments already checked against its schema.
    ///
    /// # Errors
    ///
    /// Returns `cancelled` when the call was cancelled, and the transport's
    /// error otherwise.
    fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput>;
}

/// Calls a tool of a connected MCP server by server and tool name.
pub struct McpCall {
    directory: Arc<dyn McpDirectory>,
}

impl std::fmt::Debug for McpCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpCall").finish_non_exhaustive()
    }
}

impl McpCall {
    /// Builds the tool over a directory of connected tools.
    #[must_use]
    pub fn new(directory: Arc<dyn McpDirectory>) -> Self {
        Self { directory }
    }
}

/// Reads a string argument, when it is present.
fn string<'a>(arguments: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    arguments.get(field).and_then(serde_json::Value::as_str)
}

impl Tool for McpCall {
    fn name(&self) -> &'static str {
        "mcp_call"
    }

    fn description(&self) -> &'static str {
        "Call an MCP server tool found with capability_search, passing arguments that match its schema."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": { "type": "string" },
                "tool": { "type": "string" },
                "arguments": { "type": "object" }
            },
            "required": ["server", "tool", "arguments"],
            "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Execute
    }

    fn permission_target(&self, arguments: &serde_json::Value) -> Option<String> {
        let server = string(arguments, "server")?;
        let tool = string(arguments, "tool")?;
        Some(format!("mcp:{server}/{tool}"))
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        validate_closed_arguments(arguments, &self.input_schema())?;
        let server =
            string(arguments, "server").ok_or_else(|| RuneError::missing_field("server"))?;
        let tool = string(arguments, "tool").ok_or_else(|| RuneError::missing_field("tool"))?;
        let given = arguments
            .get("arguments")
            .ok_or_else(|| RuneError::missing_field("arguments"))?;
        let Some(entry) = self
            .directory
            .entries()
            .into_iter()
            .find(|entry| entry.server == server && entry.tool == tool)
        else {
            return Ok(ToolOutput::failure(format!(
                "no MCP tool {server}/{tool}; search for it with capability_search"
            )));
        };
        if let Err(reason) = check_arguments(&entry.input_schema, given) {
            return Ok(ToolOutput::failure(format!(
                "arguments for {server}/{tool} do not match its schema: {reason}\nschema: {}",
                entry.input_schema
            )));
        }
        self.directory.call(server, tool, given, context)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::contract::model_spec;
    use camino::Utf8PathBuf;

    /// A directory with fixed entries that records every call it receives.
    #[derive(Default)]
    pub(crate) struct FixedDirectory {
        pub(crate) entries: Vec<McpEntry>,
        pub(crate) calls: Mutex<Vec<(String, String, serde_json::Value)>>,
    }

    impl McpDirectory for FixedDirectory {
        fn entries(&self) -> Vec<McpEntry> {
            self.entries.clone()
        }

        fn call(
            &self,
            server: &str,
            tool: &str,
            arguments: &serde_json::Value,
            _context: &ExecutionContext,
        ) -> Result<ToolOutput> {
            self.calls.lock().expect("calls").push((
                server.to_owned(),
                tool.to_owned(),
                arguments.clone(),
            ));
            Ok(ToolOutput::success(format!("called {server}/{tool}")))
        }
    }

    pub(crate) fn entry(server: &str, tool: &str, description: &str) -> McpEntry {
        McpEntry {
            server: server.to_owned(),
            tool: tool.to_owned(),
            description: description.to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "title": { "type": "string" } },
                "required": ["title"],
                "additionalProperties": false
            }),
        }
    }

    fn context() -> ExecutionContext {
        ExecutionContext::new(Utf8PathBuf::from("/tmp"))
    }

    #[test]
    fn mcp_call_schema_is_at_most_350_bytes() {
        let directory: Arc<dyn McpDirectory> = Arc::new(FixedDirectory::default());
        let spec = model_spec(&McpCall::new(directory));
        let advertised = serde_json::json!({
            "name": spec.name,
            "description": spec.description,
            "input_schema": spec.input_schema,
        })
        .to_string();
        assert_eq!(
            advertised,
            r#"{"name":"mcp_call","description":"Call an MCP server tool found with capability_search, passing arguments that match its schema.","input_schema":{"type":"object","properties":{"server":{"type":"string"},"tool":{"type":"string"},"arguments":{"type":"object"}},"required":["server","tool","arguments"],"additionalProperties":false}}"#
        );
        assert!(advertised.len() <= 350, "{} bytes", advertised.len());
    }

    #[test]
    fn mcp_call_validates_against_the_server_schema() {
        let directory = Arc::new(FixedDirectory {
            entries: vec![entry("issues", "create_issue", "Creates an issue.")],
            ..FixedDirectory::default()
        });
        let shared: Arc<dyn McpDirectory> = Arc::<FixedDirectory>::clone(&directory);
        let tool = McpCall::new(shared);

        let unknown = tool
            .call(
                &serde_json::json!({ "server": "issues", "tool": "delete", "arguments": {} }),
                &context(),
            )
            .expect("answered");
        assert!(unknown.is_error);
        assert_eq!(
            unknown.text,
            "no MCP tool issues/delete; search for it with capability_search"
        );

        let missing = tool
            .call(
                &serde_json::json!({ "server": "issues", "tool": "create_issue", "arguments": {} }),
                &context(),
            )
            .expect("answered");
        assert!(missing.is_error);
        assert_eq!(
            missing.text,
            "arguments for issues/create_issue do not match its schema: missing required \
             property `title`\nschema: {\"type\":\"object\",\"properties\":{\"title\":\
             {\"type\":\"string\"}},\"required\":[\"title\"],\"additionalProperties\":false}"
        );

        let mistyped = tool
            .call(
                &serde_json::json!({
                    "server": "issues", "tool": "create_issue", "arguments": { "title": 7 }
                }),
                &context(),
            )
            .expect("answered");
        assert!(mistyped.is_error);
        assert!(
            mistyped
                .text
                .contains("property `title` must be of type string"),
            "{}",
            mistyped.text
        );
        assert!(directory.calls.lock().expect("calls").is_empty());

        let arguments = serde_json::json!({
            "server": "issues", "tool": "create_issue", "arguments": { "title": "Broken" }
        });
        let called = tool.call(&arguments, &context()).expect("called");
        assert!(!called.is_error, "{}", called.text);
        assert_eq!(called.text, "called issues/create_issue");
        assert_eq!(
            *directory.calls.lock().expect("calls"),
            vec![(
                "issues".to_owned(),
                "create_issue".to_owned(),
                serde_json::json!({ "title": "Broken" })
            )]
        );
        assert_eq!(
            tool.permission_target(&arguments).as_deref(),
            Some("mcp:issues/create_issue")
        );
        assert_eq!(tool.activity(), Activity::Execute);
    }
}
