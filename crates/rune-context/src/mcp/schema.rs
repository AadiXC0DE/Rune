//! Tool projection from a server's `tools/list` reply.
//!
//! A server's `inputSchema` is carried through unchanged. Not one property is
//! added, removed, or reordered: a schema the model sees must be the schema the
//! server validates against, because a constraint this client invents is a
//! constraint the server will reject a call for. Only the envelope is checked
//! here, and only to the extent that a schema describing something other than an
//! object cannot be called at all; everything semantic belongs to the server.
//!
//! Names are namespaced and reduced to the character set a tool name accepts.
//! A name that does not fit is cut and given a digest suffix, so two long names
//! that share a prefix stay distinct rather than colliding on one wire name.

use std::fmt::Write as _;

use serde_json::Value;
use sha2::{Digest, Sha256};

use rune_core::budget::LimitName;
use rune_core::error::{Result, RuneError};

use crate::resolve_limit;

/// Prefix every projected name carries, so a dynamic tool cannot shadow a
/// built-in one.
pub const PREFIX: &str = "mcp_";

/// Longest accepted tool name, in bytes.
pub const MAX_TOOL_NAME_BYTES: usize = 64;

/// Bytes of digest appended to a name that had to be shortened.
const DIGEST_BYTES: usize = 6;

/// Appended to a shortened description so the cut is visible.
const MARKER: &str = "...";

/// One tool, as this client will advertise it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProjectedTool {
    /// Name advertised to the model, namespaced and sanitized.
    pub name: String,
    /// Server the tool came from.
    pub server: String,
    /// Name the server knows the tool by.
    pub server_tool: String,
    /// Description, shortened to `mcp_description_bytes`.
    pub description: String,
    /// The server's `inputSchema`, byte for byte.
    pub input_schema: Value,
}

/// The tools one page carried, with the entries that were refused.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Projection {
    /// Tools that could be projected.
    pub tools: Vec<ProjectedTool>,
    /// One line per refused entry, naming what was wrong with it.
    pub warnings: Vec<String>,
}

/// Projects one tool.
///
/// The `inputSchema` is moved through without inspection beyond the envelope
/// check, so an optional property stays optional and a constraint keeps its
/// position.
pub fn project_tool(server: &str, tool: &Value) -> Result<ProjectedTool> {
    let object = tool
        .as_object()
        .ok_or_else(|| RuneError::invalid_field("tool", "a tool entry is not an object"))?;
    let server_tool = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| RuneError::missing_field("name"))?
        .to_owned();
    let input_schema = object
        .get("inputSchema")
        .ok_or_else(|| RuneError::missing_field("inputSchema"))?;
    check_object_schema(input_schema)?;

    let description = object
        .get("description")
        .and_then(Value::as_str)
        .map(|text| shorten(text, resolve_limit(LimitName::McpDescriptionBytes)))
        .unwrap_or_default();

    Ok(ProjectedTool {
        name: wire_name(server, &server_tool),
        server: server.to_owned(),
        server_tool,
        description,
        input_schema: input_schema.clone(),
    })
}

/// Projects every tool in a `tools/list` result.
///
/// A single unusable entry is reported rather than failing the page, so one
/// malformed tool does not hide the rest of the server.
pub fn project_page(server: &str, result: &Value) -> Result<Projection> {
    let entries = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            RuneError::invalid_field("tools", "a tools/list result carries no tools array")
        })?;

    let mut page = Projection::default();
    for entry in entries {
        match project_tool(server, entry) {
            Ok(tool) => {
                if page.tools.iter().any(|existing| existing.name == tool.name) {
                    page.warnings.push(format!(
                        "`{}` was skipped: the name is already in use",
                        tool.name
                    ));
                    continue;
                }
                page.tools.push(tool);
            }
            Err(error) => page
                .warnings
                .push(format!("one entry was skipped: {}", error.message())),
        }
    }
    Ok(page)
}

/// Returns the namespaced, sanitized wire name for a tool.
#[must_use]
pub fn wire_name(server: &str, tool: &str) -> String {
    let raw = format!("{PREFIX}{server}_{tool}");
    let mut sanitized = String::with_capacity(raw.len());
    for character in raw.chars() {
        sanitized.push(if accepted(character) { character } else { '_' });
    }
    if sanitized.len() <= MAX_TOOL_NAME_BYTES {
        return sanitized;
    }
    // The digest is taken over the whole name, so two names that share a long
    // prefix do not collapse into one entry after the cut.
    let digest = digest(&raw);
    let room = MAX_TOOL_NAME_BYTES
        .saturating_sub(digest.len())
        .saturating_sub(1);
    let mut out = String::with_capacity(MAX_TOOL_NAME_BYTES);
    out.push_str(&sanitized[..boundary(&sanitized, room)]);
    out.push('_');
    out.push_str(&digest);
    out
}

/// Returns true when a character survives sanitization.
fn accepted(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_' || character == '-'
}

/// Returns the first `DIGEST_BYTES` bytes of the name's SHA-256, in hexadecimal.
fn digest(name: &str) -> String {
    let hash = Sha256::digest(name.as_bytes());
    let mut out = String::with_capacity(DIGEST_BYTES);
    for byte in hash.iter().take(DIGEST_BYTES / 2) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Returns the largest prefix length of `text` that fits in `max` bytes.
fn boundary(text: &str, max: usize) -> usize {
    let mut end = max.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    end
}

/// Shortens a description to `limit` bytes, marking the cut.
fn shorten(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let room = limit.saturating_sub(MARKER.len());
    let mut out = String::with_capacity(limit);
    out.push_str(&text[..boundary(text, room)]);
    out.push_str(MARKER);
    out
}

/// Refuses a schema that cannot describe an arguments object.
///
/// A schema without a `type` is accepted: JSON Schema allows one, and real
/// servers emit `properties` without it. Only a schema that positively declares
/// another type is refused, because a call against it could never be well formed.
fn check_object_schema(schema: &Value) -> Result<()> {
    let Some(object) = schema.as_object() else {
        return Err(RuneError::invalid_field(
            "inputSchema",
            "an input schema must be a JSON object",
        ));
    };
    match object.get("type") {
        None => Ok(()),
        Some(Value::String(kind)) if kind == "object" => Ok(()),
        Some(Value::Array(kinds)) if kinds.iter().any(|kind| kind == "object") => Ok(()),
        Some(other) => Err(RuneError::invalid_field(
            "inputSchema",
            "an input schema must describe an object",
        )
        .with_observed(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::error::ErrorCode;
    use serde_json::json;

    /// A captured schema, in the order the server sent it.
    ///
    /// Two optional properties sit either side of a required one, so a
    /// projection that reorders, drops, or marks anything required shows up as a
    /// diff rather than passing unnoticed.
    const CAPTURED: &str = r#"{
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "type": "object",
      "title": "issue_create",
      "properties": {
        "zeta": { "type": "string", "description": "last in, first declared" },
        "title": { "type": "string", "minLength": 1 },
        "labels": {
          "type": "array",
          "items": { "type": "string" },
          "uniqueItems": true
        },
        "alpha": {
          "anyOf": [ { "type": "integer" }, { "type": "null" } ],
          "default": null
        }
      },
      "required": ["title"],
      "additionalProperties": false
    }"#;

    fn captured() -> Value {
        serde_json::from_str(CAPTURED).expect("captured schema")
    }

    fn tool_with(schema: &Value) -> Value {
        json!({
            "name": "create_issue",
            "description": "Creates an issue.",
            "inputSchema": schema,
        })
    }

    #[test]
    fn a_captured_schema_round_trips_byte_identically() {
        let tool = tool_with(&captured());
        let projected = project_tool("issues", &tool).expect("projected");
        assert_eq!(
            projected.input_schema.to_string(),
            tool["inputSchema"].to_string()
        );
        let reparsed: Value = serde_json::from_str(CAPTURED).expect("captured schema");
        assert_eq!(
            projected.input_schema.to_string(),
            reparsed.to_string(),
            "the schema subtree changed during projection"
        );
    }

    #[test]
    fn an_optional_property_stays_optional_and_in_position() {
        let projected = project_tool("issues", &tool_with(&captured())).expect("projected");
        let schema = projected.input_schema.as_object().expect("object");
        assert_eq!(schema["required"], json!(["title"]));
        let properties = schema["properties"].as_object().expect("properties");
        assert_eq!(
            properties.keys().map(String::as_str).collect::<Vec<_>>(),
            ["zeta", "title", "labels", "alpha"]
        );
    }

    #[test]
    fn a_property_the_server_never_mentioned_is_not_added() {
        let schema = json!({ "type": "object", "properties": { "a": { "type": "string" } } });
        let projected = project_tool("s", &tool_with(&schema)).expect("projected");
        assert_eq!(projected.input_schema, schema);
        assert!(
            projected.input_schema.get("required").is_none(),
            "required-ness was synthesized"
        );
    }

    #[test]
    fn a_name_is_namespaced_and_sanitized() {
        assert_eq!(wire_name("files", "read"), "mcp_files_read");
        assert_eq!(wire_name("files", "read file"), "mcp_files_read_file");
        assert_eq!(wire_name("my-server", "a/b"), "mcp_my-server_a_b");
        assert!(wire_name("files", "read").len() <= MAX_TOOL_NAME_BYTES);
    }

    #[test]
    fn a_long_name_is_shortened_to_the_cap_and_stays_distinct() {
        let long = "x".repeat(200);
        let first = wire_name("server", &format!("{long}a"));
        let second = wire_name("server", &format!("{long}b"));
        assert_eq!(first.len(), MAX_TOOL_NAME_BYTES);
        assert_eq!(second.len(), MAX_TOOL_NAME_BYTES);
        assert_ne!(first, second);
        assert!(first.starts_with("mcp_server_"), "{first}");
    }

    #[test]
    fn a_shortened_name_is_stable_across_calls() {
        let long = "y".repeat(200);
        assert_eq!(wire_name("server", &long), wire_name("server", &long));
    }

    #[test]
    fn a_schema_that_describes_something_else_is_refused() {
        for schema in [json!({ "type": "array" }), json!({ "type": "string" })] {
            let error = project_tool("s", &tool_with(&schema)).expect_err("refused");
            assert_eq!(error.code(), ErrorCode::InvalidField);
        }
        assert_eq!(
            project_tool("s", &tool_with(&json!({ "type": "object" })))
                .expect("accepted")
                .server_tool,
            "create_issue"
        );
        assert!(project_tool("s", &tool_with(&json!({ "type": ["object", "null"] }))).is_ok());
        assert!(project_tool("s", &tool_with(&json!({ "properties": {} }))).is_ok());
    }

    #[test]
    fn a_schema_that_is_not_an_object_is_refused() {
        let error = project_tool("s", &tool_with(&json!("not a schema"))).expect_err("refused");
        assert_eq!(error.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn a_tool_without_an_input_schema_is_refused_rather_than_given_one() {
        let tool = json!({ "name": "create_issue" });
        let error = project_tool("s", &tool).expect_err("refused");
        assert_eq!(error.code(), ErrorCode::MissingField);
        assert_eq!(error.field(), Some("inputSchema"));
    }

    #[test]
    fn a_tool_without_a_name_is_refused() {
        let tool = json!({ "inputSchema": { "type": "object" } });
        assert_eq!(
            project_tool("s", &tool).expect_err("refused").code(),
            ErrorCode::MissingField
        );
    }

    #[test]
    fn a_page_reports_a_bad_entry_without_losing_the_others() {
        let result = json!({
            "tools": [
                { "name": "good", "inputSchema": { "type": "object" } },
                { "name": "" },
                { "name": "also_good", "inputSchema": { "type": "object" } }
            ]
        });
        let page = project_page("s", &result).expect("projected");
        assert_eq!(
            page.tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["mcp_s_good", "mcp_s_also_good"]
        );
        assert_eq!(page.warnings.len(), 1);
    }

    #[test]
    fn a_page_carries_the_server_name_and_original_tool_name() {
        let result = json!({
            "tools": [{ "name": "create_issue", "inputSchema": { "type": "object" } }]
        });
        let page = project_page("issues", &result).expect("projected");
        assert_eq!(page.tools[0].server, "issues");
        assert_eq!(page.tools[0].server_tool, "create_issue");
        assert_eq!(page.tools[0].name, "mcp_issues_create_issue");
    }

    #[test]
    fn a_page_without_a_tools_array_is_refused() {
        assert_eq!(
            project_page("s", &json!({})).expect_err("refused").field(),
            Some("tools")
        );
    }

    #[test]
    fn a_long_description_is_shortened_at_the_limit() {
        let described = "d".repeat(resolve_limit(LimitName::McpDescriptionBytes) + 100);
        let tool = json!({
            "name": "create_issue",
            "description": described,
            "inputSchema": { "type": "object" }
        });
        let projected = project_tool("s", &tool).expect("projected");
        assert_eq!(
            projected.description.len(),
            resolve_limit(LimitName::McpDescriptionBytes)
        );
        assert!(projected.description.ends_with(MARKER));
    }

    #[test]
    fn a_tool_without_a_description_projects_an_empty_one() {
        let projected = project_tool("s", &tool_with(&captured())).expect("projected");
        assert_eq!(projected.description, "Creates an issue.");
    }
}
