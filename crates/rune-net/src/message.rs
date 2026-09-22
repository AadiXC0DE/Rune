//! The canonical conversation model.
//!
//! Every provider dialect projects from this representation, so a dialect can
//! never become the internal one. The invariants below are checked by
//! [`validate`] and are what the wiring in the design depends on:
//!
//! - system content is contiguous and comes first
//! - a run of tool results matches the preceding assistant tool calls exactly
//! - every tool call identifier is unique within a message

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::ToolCallId;
pub use rune_core::tool::{
    MAX_TOOL_NAME, MAX_TOOLS, ToolSpec, validate_tool_spec, validate_tool_specs,
};
use serde::{Deserialize, Serialize};

/// Who produced a message.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Operator instructions that outrank the conversation.
    System,
    /// The person using the product.
    User,
    /// The model.
    Assistant,
    /// The result of a tool the model asked for.
    Tool,
}

impl Role {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// An image carried in a message.
///
/// Held by reference to the session image store rather than as bytes, so a
/// long conversation does not retain every attachment in memory.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ImageRef {
    /// Identifier assigned when the image was attached.
    pub id: u64,
    /// Media type, for example `image/png`.
    pub media_type: String,
    /// Encoded size in bytes, before base64.
    pub encoded_bytes: u64,
}

/// One part of a message body.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// Model reasoning, kept so it can be replayed where a provider requires it.
    Reasoning {
        /// The reasoning text.
        text: String,
    },
    /// A tool the model wants to run.
    ToolCall {
        /// Identifier assigned by the model, projected for the wire if needed.
        id: ToolCallId,
        /// Tool name.
        name: String,
        /// Arguments as a JSON string, exactly as the model produced them.
        arguments: String,
    },
    /// The result of a tool.
    ToolResult {
        /// Identifier of the call this answers.
        id: ToolCallId,
        /// Tool name.
        name: String,
        /// Result text, already bounded and redacted.
        content: String,
        /// Whether the tool reported a failure.
        is_error: bool,
    },
    /// An attached image.
    Image {
        /// Reference to the stored image.
        image: ImageRef,
    },
}

impl ContentPart {
    /// Returns the text of a text part.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }

    /// Returns the identifier of a tool call or result part.
    #[must_use]
    pub fn tool_call_id(&self) -> Option<&ToolCallId> {
        match self {
            Self::ToolCall { id, .. } | Self::ToolResult { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// One turn in the conversation.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Message {
    /// Who produced the message.
    pub role: Role,
    /// Body parts, in order.
    pub parts: Vec<ContentPart>,
    /// Opaque provider state to replay with this message on the next request.
    ///
    /// Some providers require signed reasoning or an encrypted block to be sent
    /// back verbatim. It is never interpreted, only stored and replayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<String>,
}

impl Message {
    /// Builds a system message.
    #[must_use]
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            parts: vec![ContentPart::Text { text: text.into() }],
            replay: None,
        }
    }

    /// Builds a user message from text.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            parts: vec![ContentPart::Text { text: text.into() }],
            replay: None,
        }
    }

    /// Builds a user message from parts.
    #[must_use]
    pub fn user_parts(parts: Vec<ContentPart>) -> Self {
        Self {
            role: Role::User,
            parts,
            replay: None,
        }
    }

    /// Builds an assistant message from parts.
    #[must_use]
    pub fn assistant(parts: Vec<ContentPart>) -> Self {
        Self {
            role: Role::Assistant,
            parts,
            replay: None,
        }
    }

    /// Returns the concatenated text of the message.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            if let ContentPart::Text { text } = part {
                out.push_str(text);
            }
        }
        out
    }

    /// Returns the tool calls in this message.
    #[must_use]
    pub fn tool_calls(&self) -> Vec<(&ToolCallId, &str, &str)> {
        self.parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id, name.as_str(), arguments.as_str())),
                _ => None,
            })
            .collect()
    }

    /// Returns true when the message has no body parts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }
}

/// Checks every invariant a provider request depends on.
///
/// Returns an error naming the violated invariant and the offending index, so a
/// corrupt history is diagnosable rather than a mystery failure at the endpoint.
pub fn validate(messages: &[Message]) -> Result<()> {
    // System content must be contiguous at the start.
    let mut seen_non_system = false;
    for (index, message) in messages.iter().enumerate() {
        match message.role {
            Role::System if seen_non_system => {
                return Err(RuneError::invariant(
                    "system_first",
                    format!("message {index} is a system message after conversation content"),
                ));
            }
            Role::System => {}
            _ => seen_non_system = true,
        }
    }

    // A run of tool results must answer the assistant's calls, in order.
    let mut index = 0;
    while index < messages.len() {
        let Some(message) = messages.get(index) else {
            break;
        };

        if message.role == Role::Tool {
            return Err(RuneError::invariant(
                "tool_result_pairing",
                format!("message {index} is a tool result with no preceding tool call"),
            ));
        }

        if message.role != Role::Assistant {
            index = index.saturating_add(1);
            continue;
        }

        let calls = message.tool_calls();
        if calls.is_empty() {
            index = index.saturating_add(1);
            continue;
        }

        // Identifiers must be unique within one assistant message.
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (id, _, _) in &calls {
            if !seen.insert(id.as_str()) {
                return Err(RuneError::invariant(
                    "unique_tool_calls",
                    format!("message {index} repeats tool call identifier `{id}`"),
                ));
            }
        }

        // Results must follow, contiguous and complete.
        let mut answered: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut cursor = index.saturating_add(1);
        while let Some(next) = messages.get(cursor) {
            if next.role != Role::Tool {
                break;
            }
            for part in &next.parts {
                if let ContentPart::ToolResult { id, .. } = part {
                    if !seen.contains(id.as_str()) {
                        return Err(RuneError::invariant(
                            "tool_result_pairing",
                            format!("message {cursor} answers unknown tool call `{id}`"),
                        ));
                    }
                    if !answered.insert(id.as_str()) {
                        return Err(RuneError::invariant(
                            "tool_result_pairing",
                            format!("tool call `{id}` was answered twice"),
                        ));
                    }
                }
            }
            cursor = cursor.saturating_add(1);
        }

        if answered.len() != seen.len() {
            let missing: Vec<&str> = seen.difference(&answered).copied().collect();
            return Err(RuneError::invariant(
                "tool_result_pairing",
                format!(
                    "message {index} has {} unanswered tool call(s): {}",
                    missing.len(),
                    missing.join(", ")
                ),
            ));
        }

        index = cursor;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str) -> ContentPart {
        ContentPart::ToolCall {
            id: ToolCallId::new(id).expect("id"),
            name: name.to_owned(),
            arguments: "{}".to_owned(),
        }
    }

    fn result(id: &str, name: &str) -> ContentPart {
        ContentPart::ToolResult {
            id: ToolCallId::new(id).expect("id"),
            name: name.to_owned(),
            content: "ok".to_owned(),
            is_error: false,
        }
    }

    fn tool_message(parts: Vec<ContentPart>) -> Message {
        Message {
            role: Role::Tool,
            parts,
            replay: None,
        }
    }

    #[test]
    fn a_valid_conversation_passes() {
        let messages = vec![
            Message::system("be helpful"),
            Message::user("read a file"),
            Message::assistant(vec![
                ContentPart::Text {
                    text: "reading".to_owned(),
                },
                call("c1", "read_file"),
            ]),
            tool_message(vec![result("c1", "read_file")]),
            Message::assistant(vec![ContentPart::Text {
                text: "done".to_owned(),
            }]),
        ];
        validate(&messages).expect("valid");
    }

    #[test]
    fn an_empty_conversation_is_valid() {
        validate(&[]).expect("valid");
    }

    #[test]
    fn system_content_after_a_turn_is_rejected() {
        let messages = vec![
            Message::system("first"),
            Message::user("hello"),
            Message::system("late"),
        ];
        let err = validate(&messages).expect_err("rejected");
        assert_eq!(err.detail().invariant.as_deref(), Some("system_first"));
        assert!(err.message().contains("message 2"));
    }

    #[test]
    fn multiple_leading_system_messages_are_accepted() {
        let messages = vec![
            Message::system("one"),
            Message::system("two"),
            Message::user("hello"),
        ];
        validate(&messages).expect("valid");
    }

    #[test]
    fn an_unpaired_tool_result_is_rejected() {
        let messages = vec![
            Message::user("hi"),
            tool_message(vec![result("c1", "read_file")]),
        ];
        let err = validate(&messages).expect_err("rejected");
        assert_eq!(
            err.detail().invariant.as_deref(),
            Some("tool_result_pairing")
        );
        assert!(err.message().contains("no preceding tool call"));
    }

    #[test]
    fn a_missing_tool_result_is_rejected_and_names_the_call() {
        let messages = vec![
            Message::assistant(vec![call("c1", "read_file"), call("c2", "glob_files")]),
            tool_message(vec![result("c1", "read_file")]),
        ];
        let err = validate(&messages).expect_err("rejected");
        assert_eq!(
            err.detail().invariant.as_deref(),
            Some("tool_result_pairing")
        );
        assert!(err.message().contains("c2"), "{}", err.message());
    }

    #[test]
    fn a_result_for_an_unknown_call_is_rejected() {
        let messages = vec![
            Message::assistant(vec![call("c1", "read_file")]),
            tool_message(vec![result("other", "read_file")]),
        ];
        let err = validate(&messages).expect_err("rejected");
        assert!(err.message().contains("unknown tool call"));
    }

    #[test]
    fn a_duplicate_result_is_rejected() {
        let messages = vec![
            Message::assistant(vec![call("c1", "read_file")]),
            tool_message(vec![result("c1", "read_file"), result("c1", "read_file")]),
        ];
        let err = validate(&messages).expect_err("rejected");
        assert!(err.message().contains("answered twice"));
    }

    #[test]
    fn a_duplicate_call_identifier_is_rejected() {
        let messages = vec![Message::assistant(vec![
            call("c1", "read_file"),
            call("c1", "glob_files"),
        ])];
        let err = validate(&messages).expect_err("rejected");
        assert_eq!(err.detail().invariant.as_deref(), Some("unique_tool_calls"));
    }

    #[test]
    fn results_may_span_several_messages() {
        let messages = vec![
            Message::assistant(vec![call("c1", "a"), call("c2", "b")]),
            tool_message(vec![result("c1", "a")]),
            tool_message(vec![result("c2", "b")]),
        ];
        validate(&messages).expect("valid");
    }

    #[test]
    fn an_assistant_turn_without_calls_does_not_require_results() {
        let messages = vec![
            Message::assistant(vec![ContentPart::Text {
                text: "no tools".to_owned(),
            }]),
            Message::user("next"),
        ];
        validate(&messages).expect("valid");
    }

    #[test]
    fn message_text_concatenates_only_text_parts() {
        let message = Message::assistant(vec![
            ContentPart::Reasoning {
                text: "thinking".to_owned(),
            },
            ContentPart::Text {
                text: "hello ".to_owned(),
            },
            call("c1", "read_file"),
            ContentPart::Text {
                text: "world".to_owned(),
            },
        ]);
        assert_eq!(message.text(), "hello world");
    }

    #[test]
    fn tool_spec_requires_an_object_schema() {
        let bad = ToolSpec {
            name: "t".to_owned(),
            description: "d".to_owned(),
            input_schema: serde_json::json!({ "type": "array" }),
        };
        let err = validate_tool_spec(&bad).expect_err("rejected");
        assert_eq!(err.field(), Some("tool.input_schema"));
    }

    #[test]
    fn tool_spec_requires_a_declared_type() {
        let bad = ToolSpec {
            name: "t".to_owned(),
            description: "d".to_owned(),
            input_schema: serde_json::json!({ "properties": {} }),
        };
        let err = validate_tool_spec(&bad).expect_err("rejected");
        assert!(err.message().contains("does not declare a type"));
    }

    #[test]
    fn tool_spec_rejects_an_empty_description() {
        let bad = ToolSpec {
            name: "t".to_owned(),
            description: String::new(),
            input_schema: serde_json::json!({ "type": "object" }),
        };
        assert!(validate_tool_spec(&bad).is_err());
    }

    #[test]
    fn tool_spec_rejects_a_long_name() {
        let bad = ToolSpec {
            name: "x".repeat(MAX_TOOL_NAME + 1),
            description: "d".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        };
        let err = validate_tool_spec(&bad).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn tool_spec_rejects_a_name_with_spaces() {
        let bad = ToolSpec {
            name: "read file".to_owned(),
            description: "d".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        };
        assert!(validate_tool_spec(&bad).is_err());
    }

    #[test]
    fn duplicate_tool_advertisement_is_rejected() {
        let spec = ToolSpec {
            name: "read_file".to_owned(),
            description: "d".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        };
        let err = validate_tool_specs(&[spec.clone(), spec]).expect_err("rejected");
        assert!(err.message().contains("advertised twice"));
    }

    #[test]
    fn too_many_tools_is_rejected() {
        let tools: Vec<ToolSpec> = (0..=MAX_TOOLS)
            .map(|index| ToolSpec {
                name: format!("tool_{index}"),
                description: "d".to_owned(),
                input_schema: serde_json::json!({ "type": "object" }),
            })
            .collect();
        let err = validate_tool_specs(&tools).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn messages_round_trip_through_json() {
        let message = Message::assistant(vec![
            ContentPart::Text {
                text: "hello".to_owned(),
            },
            call("c1", "read_file"),
        ]);
        let json = serde_json::to_string(&message).expect("serialize");
        let parsed: Message = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(message, parsed);
    }

    #[test]
    fn replay_bytes_survive_a_round_trip() {
        let mut message = Message::assistant(vec![ContentPart::Text {
            text: "x".to_owned(),
        }]);
        message.replay = Some("{\"signed\":\"opaque\"}".to_owned());
        let json = serde_json::to_string(&message).expect("serialize");
        let parsed: Message = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.replay.as_deref(), Some("{\"signed\":\"opaque\"}"));
    }
}
