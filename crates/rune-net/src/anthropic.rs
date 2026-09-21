//! The Anthropic Messages dialect.
//!
//! A first-class dialect rather than something reached through a shim or a
//! gateway account, so a plain API key is enough to use Anthropic models. The
//! message model differs from the OpenAI shapes: system content is a top-level
//! field, tool calls and results are content blocks, and a response may carry
//! thinking blocks that must be replayed with the assistant turn.

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::ToolCallId;

use crate::message::{ContentPart, Message, Role};
use crate::provider::{Provider, RequestPlan, ToolChoice};
use crate::stream::{
    FinishReason, Limit, StreamReducer, Usage, incomplete_stream, protocol_violation,
};

/// Dialect name.
pub const NAME: &str = "anthropic";

/// Path appended to the endpoint base URL.
pub const PATH: &str = "/v1/messages";

/// Default output ceiling.
///
/// The field is required, so a plan without one still produces a valid request.
pub const DEFAULT_MAX_TOKENS: u64 = 8192;

/// The dialect.
#[derive(Clone, Copy, Debug, Default)]
pub struct Anthropic;

impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        NAME
    }

    fn request_path(&self) -> &'static str {
        PATH
    }

    fn reducer(&self) -> Box<dyn StreamReducer> {
        Box::new(Reducer::new())
    }

    fn extra_headers(&self) -> Vec<(&'static str, String)> {
        vec![("anthropic-version", "2023-06-01".to_owned())]
    }

    fn build_request(&self, plan: &RequestPlan) -> Result<serde_json::Value> {
        let mut messages: Vec<serde_json::Value> = Vec::new();

        for message in &plan.messages {
            let encoded = encode_message(message)?;
            merge_adjacent(&mut messages, encoded);
        }

        let mut body = serde_json::Map::new();
        body.insert("model".to_owned(), serde_json::json!(plan.model));
        body.insert(
            "max_tokens".to_owned(),
            serde_json::json!(plan.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        );
        body.insert("stream".to_owned(), serde_json::json!(true));
        body.insert("messages".to_owned(), serde_json::Value::Array(messages));

        if !plan.instructions.is_empty() {
            body.insert("system".to_owned(), serde_json::json!(plan.instructions));
        }

        if plan.has_tools() {
            let tools: Vec<serde_json::Value> = plan
                .tools
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.input_schema,
                    })
                })
                .collect();
            body.insert("tools".to_owned(), serde_json::Value::Array(tools));
            if plan.tool_choice != ToolChoice::Auto {
                body.insert(
                    "tool_choice".to_owned(),
                    serde_json::json!({ "type": plan.tool_choice.as_str() }),
                );
            }
        }

        Ok(serde_json::Value::Object(body))
    }
}

/// Encodes one message into a role with content blocks.
fn encode_message(message: &Message) -> Result<serde_json::Value> {
    match message.role {
        Role::System => Err(RuneError::invalid_field(
            "messages",
            "system content must be supplied as instructions, not as a message",
        )),
        Role::User => Ok(serde_json::json!({
            "role": "user",
            "content": encode_user_blocks(message),
        })),
        Role::Assistant => Ok(serde_json::json!({
            "role": "assistant",
            "content": encode_assistant_blocks(message),
        })),
        // Tool results are user-role content blocks in this dialect.
        Role::Tool => Ok(serde_json::json!({
            "role": "user",
            "content": encode_tool_blocks(message),
        })),
    }
}

/// Returns the blocks stored in a message's replay field.
///
/// A malformed replay is ignored rather than fatal: the field is provider state
/// this build wrote, and a request without it is still valid.
fn replay_blocks(message: &Message) -> Vec<serde_json::Value> {
    let Some(replay) = &message.replay else {
        return Vec::new();
    };
    serde_json::from_str::<serde_json::Value>(replay)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

/// Encodes user content blocks.
fn encode_user_blocks(message: &Message) -> Vec<serde_json::Value> {
    let mut blocks = replay_blocks(message);
    for part in &message.parts {
        match part {
            ContentPart::Text { text } => blocks.push(serde_json::json!({
                "type": "text",
                "text": text,
            })),
            ContentPart::Image { image } => blocks.push(serde_json::json!({
                "type": "image",
                "source": {
                    "type": "image_ref",
                    "media_type": image.media_type,
                    "id": image.id,
                },
            })),
            _ => {}
        }
    }
    blocks
}

/// Encodes assistant content blocks, including tool use and thinking.
fn encode_assistant_blocks(message: &Message) -> Vec<serde_json::Value> {
    // A thinking block must be replayed with the turn that produced it, so it
    // comes first in the block list.
    let mut blocks = replay_blocks(message);

    for part in &message.parts {
        match part {
            ContentPart::Text { text } => {
                if !text.is_empty() {
                    blocks.push(serde_json::json!({ "type": "text", "text": text }));
                }
            }
            ContentPart::ToolCall {
                id,
                name,
                arguments,
            } => {
                // Arguments are an object here, not a JSON string. A malformed
                // fragment becomes an empty object so the request still forms,
                // and the tool layer reports the problem to the model.
                let parsed: serde_json::Value =
                    serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}));
                blocks.push(serde_json::json!({
                    "type": "tool_use",
                    "id": id.as_str(),
                    "name": name,
                    "input": parsed,
                }));
            }
            _ => {}
        }
    }

    if blocks.is_empty() {
        blocks.push(serde_json::json!({ "type": "text", "text": "" }));
    }
    blocks
}

/// Encodes tool results as user content blocks.
fn encode_tool_blocks(message: &Message) -> Vec<serde_json::Value> {
    let mut blocks = Vec::new();
    for part in &message.parts {
        if let ContentPart::ToolResult {
            id,
            content,
            is_error,
            ..
        } = part
        {
            blocks.push(serde_json::json!({
                "type": "tool_result",
                "tool_use_id": id.as_str(),
                "content": content,
                "is_error": is_error,
            }));
        }
    }
    blocks
}

/// Merges a role's blocks into the previous message when the roles match.
///
/// The endpoint rejects two consecutive messages with the same role, which a
/// tool turn produces naturally because tool results are user content.
fn merge_adjacent(messages: &mut Vec<serde_json::Value>, encoded: serde_json::Value) {
    let role = encoded.get("role").and_then(serde_json::Value::as_str);
    let blocks = encoded.get("content").cloned();

    let Some(extra) = blocks.and_then(|value| value.as_array().cloned()) else {
        messages.push(encoded);
        return;
    };

    if let Some(existing) = messages
        .last_mut()
        .filter(|last| last.get("role").and_then(serde_json::Value::as_str) == role)
        .and_then(|last| last.get_mut("content"))
        .and_then(serde_json::Value::as_array_mut)
    {
        existing.extend(extra);
        return;
    }

    messages.push(encoded);
}

/// Maps a reasoning effort onto the dialect's thinking budget.
///
/// The endpoint takes a token budget rather than a label, so the label is
/// translated into a budget.
#[must_use]
pub fn thinking_budget(effort: rune_core::config::Effort) -> Option<u64> {
    use rune_core::config::Effort;
    match effort {
        Effort::Auto | Effort::None => None,
        Effort::Minimal => Some(1024),
        Effort::Low => Some(2048),
        Effort::Medium => Some(8192),
        Effort::High => Some(16_384),
        Effort::Xhigh => Some(32_768),
        Effort::Max => Some(65_536),
    }
}

/// Accumulated state for one content block.
#[derive(Debug, Default)]
struct PartialBlock {
    kind: Option<String>,
    id: Option<ToolCallId>,
    name: Option<String>,
    arguments: String,
    text: String,
    ended: bool,
}

/// Reduces an Anthropic stream into normalized events.
#[derive(Debug)]
pub struct Reducer {
    limits: Limit,
    blocks: std::collections::BTreeMap<u64, PartialBlock>,
    thinking: Vec<serde_json::Value>,
    content: String,
    usage: Usage,
    stop_reason: Option<FinishReason>,
    finished: bool,
    event_count: usize,
}

impl Default for Reducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Reducer {
    /// Creates a reducer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limits: Limit::default(),
            blocks: std::collections::BTreeMap::new(),
            thinking: Vec::new(),
            content: String::new(),
            usage: Usage::default(),
            stop_reason: None,
            finished: false,
            event_count: 0,
        }
    }

    /// Sets the stream bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: Limit) -> Self {
        self.limits = limits;
        self
    }

    /// Applies one decoded frame.
    fn apply_frame(
        &mut self,
        payload: &str,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let value: serde_json::Value = serde_json::from_str(payload)
            .map_err(|err| protocol_violation(format!("stream frame is not valid JSON: {err}")))?;

        let kind = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| protocol_violation("a stream frame carried no event type"))?;

        match kind {
            "message_start" => {
                if let Some(usage) = value
                    .get("message")
                    .and_then(|message| message.get("usage"))
                {
                    self.record_usage(usage, out);
                }
                Ok(())
            }
            "content_block_start" => self.apply_block_start(&value, out),
            "content_block_delta" => self.apply_block_delta(&value, out),
            "content_block_stop" => self.apply_block_stop(&value, out),
            "message_delta" => {
                if let Some(usage) = value.get("usage") {
                    self.record_usage(usage, out);
                }
                if let Some(reason) = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(serde_json::Value::as_str)
                {
                    self.stop_reason = Some(map_stop_reason(reason)?);
                }
                Ok(())
            }
            "message_stop" => {
                self.finished = true;
                Ok(())
            }
            "error" => {
                let message = value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("the provider reported an error");
                Err(RuneError::new(
                    ErrorCode::RequestRejected,
                    message.to_owned(),
                ))
            }
            "ping" => Ok(()),
            // Unknown events are ignored so a new one does not break an old client.
            _ => Ok(()),
        }
    }

    /// Handles the start of a content block.
    fn apply_block_start(
        &mut self,
        value: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let Some(index) = value.get("index").and_then(serde_json::Value::as_u64) else {
            return Err(protocol_violation("a content block carried no index"));
        };
        let Some(block) = value.get("content_block") else {
            return Err(protocol_violation("a content block start carried no block"));
        };
        let Some(kind) = block.get("type").and_then(serde_json::Value::as_str) else {
            return Err(protocol_violation("a content block declared no type"));
        };

        let mut partial = PartialBlock {
            kind: Some(kind.to_owned()),
            ..PartialBlock::default()
        };

        match kind {
            "text" => {
                if let Some(text) = block
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    partial.text.push_str(text);
                    out.push(crate::stream::ProviderEvent::TextDelta {
                        delta: text.to_owned(),
                    });
                }
            }
            "thinking" => {
                // Retained verbatim for replay.
                self.thinking.push(block.clone());
                if let Some(text) = block
                    .get("thinking")
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    out.push(crate::stream::ProviderEvent::ReasoningDelta {
                        delta: text.to_owned(),
                    });
                }
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| protocol_violation("a tool use block carried no identifier"))?;
                let name = block
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| protocol_violation("a tool use block carried no name"))?;
                if self.blocks.len() >= self.limits.max_tool_calls {
                    return Err(RuneError::too_large(
                        "stream.tool_calls",
                        self.blocks.len().saturating_add(1),
                        self.limits.max_tool_calls,
                    ));
                }
                let call_id = ToolCallId::new(id)?;
                partial.id = Some(call_id.clone());
                partial.name = Some(name.to_owned());
                out.push(crate::stream::ProviderEvent::ToolCallStart {
                    id: call_id,
                    name: name.to_owned(),
                });
            }
            _ => {}
        }

        self.blocks.insert(index, partial);
        Ok(())
    }

    /// Handles a delta within a content block.
    fn apply_block_delta(
        &mut self,
        value: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let Some(index) = value.get("index").and_then(serde_json::Value::as_u64) else {
            return Err(protocol_violation("a content block delta carried no index"));
        };
        let Some(delta) = value.get("delta") else {
            return Ok(());
        };
        let kind = delta.get("type").and_then(serde_json::Value::as_str);

        let Some(block) = self.blocks.get_mut(&index) else {
            return Err(protocol_violation(format!(
                "a delta arrived for unknown content block {index}"
            )));
        };

        match kind {
            Some("text_delta") => {
                if let Some(text) = delta
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    block.text.push_str(text);
                    self.content.push_str(text);
                    if self.content.len() > self.limits.max_content_bytes {
                        return Err(RuneError::too_large(
                            "stream.content",
                            self.content.len(),
                            self.limits.max_content_bytes,
                        ));
                    }
                    out.push(crate::stream::ProviderEvent::TextDelta {
                        delta: text.to_owned(),
                    });
                }
            }
            Some("thinking_delta") => {
                if let Some(text) = delta
                    .get("thinking")
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    out.push(crate::stream::ProviderEvent::ReasoningDelta {
                        delta: text.to_owned(),
                    });
                }
            }
            Some("input_json_delta") => {
                if let Some(fragment) = delta
                    .get("partial_json")
                    .and_then(serde_json::Value::as_str)
                {
                    block.arguments.push_str(fragment);
                    if block.arguments.len() > self.limits.max_tool_arguments_bytes {
                        return Err(RuneError::too_large(
                            "stream.tool_arguments",
                            block.arguments.len(),
                            self.limits.max_tool_arguments_bytes,
                        ));
                    }
                    let id = block.id.clone().ok_or_else(|| {
                        protocol_violation("a tool use block lost its identifier")
                    })?;
                    out.push(crate::stream::ProviderEvent::ToolCallDelta {
                        id,
                        delta: fragment.to_owned(),
                    });
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Handles the end of a content block.
    fn apply_block_stop(
        &mut self,
        value: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let Some(index) = value.get("index").and_then(serde_json::Value::as_u64) else {
            return Err(protocol_violation("a content block stop carried no index"));
        };
        let Some(block) = self.blocks.get_mut(&index) else {
            return Ok(());
        };
        if block.ended || block.kind.as_deref() != Some("tool_use") {
            block.ended = true;
            return Ok(());
        }
        block.ended = true;
        let id = block
            .id
            .clone()
            .ok_or_else(|| protocol_violation("a tool use block lost its identifier"))?;
        // An empty argument string means no arguments were supplied.
        let arguments = if block.arguments.is_empty() {
            "{}".to_owned()
        } else {
            block.arguments.clone()
        };
        out.push(crate::stream::ProviderEvent::ToolCallEnd { id, arguments });
        Ok(())
    }

    /// Records usage, replacing rather than accumulating.
    ///
    /// The endpoint reports a running total, so the later value is the accurate
    /// one and adding them would double count.
    fn record_usage(
        &mut self,
        usage: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) {
        let parsed = parse_usage(usage);
        self.usage = Usage {
            input_tokens: parsed.input_tokens.or(self.usage.input_tokens),
            output_tokens: parsed.output_tokens.or(self.usage.output_tokens),
            cache_read_tokens: parsed.cache_read_tokens.or(self.usage.cache_read_tokens),
            cache_write_tokens: parsed.cache_write_tokens.or(self.usage.cache_write_tokens),
            reasoning_tokens: parsed.reasoning_tokens.or(self.usage.reasoning_tokens),
        };
        out.push(crate::stream::ProviderEvent::Usage(parsed));
    }
}

impl StreamReducer for Reducer {
    fn apply(
        &mut self,
        payload: Option<&str>,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let Some(payload) = payload else {
            return Ok(());
        };
        if payload.trim() == "[DONE]" {
            return Ok(());
        }

        self.event_count = self.event_count.saturating_add(1);
        self.limits.check_events(self.event_count)?;
        self.apply_frame(payload, out)
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn usage(&self) -> Usage {
        self.usage
    }

    fn replay(&self) -> Option<String> {
        if self.thinking.is_empty() {
            return None;
        }
        serde_json::to_string(&self.thinking).ok()
    }

    fn finish(&self) -> Result<FinishReason> {
        if !self.finished {
            return Err(incomplete_stream());
        }
        self.stop_reason
            .ok_or_else(|| protocol_violation("the stream ended without a stop reason"))
    }
}

/// Reads a usage object.
fn parse_usage(value: &serde_json::Value) -> Usage {
    let field = |name: &str| value.get(name).and_then(serde_json::Value::as_u64);
    Usage {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_read_tokens: field("cache_read_input_tokens"),
        cache_write_tokens: field("cache_creation_input_tokens"),
        reasoning_tokens: None,
    }
}

/// Maps a stop reason onto the normalized one.
fn map_stop_reason(raw: &str) -> Result<FinishReason> {
    match raw {
        "end_turn" | "stop_sequence" => Ok(FinishReason::Stop),
        "tool_use" => Ok(FinishReason::ToolCalls),
        "max_tokens" => Ok(FinishReason::MaxTokens),
        "refusal" => Ok(FinishReason::Refused),
        other => Err(protocol_violation(format!(
            "the stream reported an unknown stop reason `{other}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolSpec;
    use crate::stream::ProviderEvent;

    fn plan_with_user(text: &str) -> RequestPlan {
        let mut plan = RequestPlan::new("claude-test");
        plan.instructions = "be helpful".to_owned();
        plan.messages = vec![Message::user(text)];
        plan
    }

    #[test]
    fn the_request_uses_the_messages_shape() {
        let body = Anthropic
            .build_request(&plan_with_user("hi"))
            .expect("build");
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["stream"], true);
        assert_eq!(body["system"], "be helpful");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn system_content_is_never_a_message() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message::system("inline")];
        let err = Anthropic.build_request(&plan).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn the_api_version_header_is_declared() {
        let headers = Anthropic.extra_headers();
        assert!(headers.iter().any(|(name, _)| *name == "anthropic-version"));
    }

    #[test]
    fn a_requested_output_ceiling_is_used() {
        let mut plan = plan_with_user("hi");
        plan.max_output_tokens = Some(1024);
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(body["max_tokens"], 1024);
    }

    #[test]
    fn tools_use_the_flat_input_schema_shape() {
        let mut plan = plan_with_user("hi");
        plan.tools = vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(body["tools"][0].get("function").is_none());
    }

    #[test]
    fn auto_tool_choice_is_omitted() {
        let mut plan = plan_with_user("hi");
        plan.tools = vec![ToolSpec {
            name: "t".to_owned(),
            description: "d".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];
        let body = Anthropic.build_request(&plan).expect("build");
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn a_required_tool_choice_is_encoded_as_an_object() {
        let mut plan = plan_with_user("hi");
        plan.tools = vec![ToolSpec {
            name: "t".to_owned(),
            description: "d".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];
        plan.tool_choice = ToolChoice::Required;
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(body["tool_choice"]["type"], "required");
    }

    #[test]
    fn a_tool_call_becomes_a_tool_use_block_with_an_object_input() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message::assistant(vec![ContentPart::ToolCall {
            id: ToolCallId::new("toolu_1").expect("id"),
            name: "read_file".to_owned(),
            arguments: "{\"path\":\"a.rs\"}".to_owned(),
        }])];
        let body = Anthropic.build_request(&plan).expect("build");
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "toolu_1");
        assert_eq!(block["input"]["path"], "a.rs");
    }

    #[test]
    fn malformed_tool_arguments_become_an_empty_object() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message::assistant(vec![ContentPart::ToolCall {
            id: ToolCallId::new("t").expect("id"),
            name: "read_file".to_owned(),
            arguments: "{not json".to_owned(),
        }])];
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(
            body["messages"][0]["content"][0]["input"],
            serde_json::json!({})
        );
    }

    #[test]
    fn a_tool_result_becomes_a_user_role_tool_result_block() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult {
                id: ToolCallId::new("toolu_1").expect("id"),
                name: "read_file".to_owned(),
                content: "contents".to_owned(),
                is_error: false,
            }],
            replay: None,
        }];
        let body = Anthropic.build_request(&plan).expect("build");
        let message = &body["messages"][0];
        assert_eq!(message["role"], "user");
        assert_eq!(message["content"][0]["type"], "tool_result");
        assert_eq!(message["content"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn adjacent_same_role_messages_are_merged() {
        // A tool turn produces two user-role messages, which the endpoint
        // rejects unless they are merged into one.
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![
            Message::user("look at this"),
            Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult {
                    id: ToolCallId::new("t").expect("id"),
                    name: "read_file".to_owned(),
                    content: "x".to_owned(),
                    is_error: false,
                }],
                replay: None,
            },
        ];
        let body = Anthropic.build_request(&plan).expect("build");
        let messages = body["messages"].as_array().expect("array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"].as_array().expect("blocks").len(), 2);
    }

    #[test]
    fn distinct_roles_are_not_merged() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message::user("one"), Message::user("two")];
        let body = Anthropic.build_request(&plan).expect("build");
        let messages = body["messages"].as_array().expect("array");
        // Both are user role, so they merge.
        assert_eq!(messages.len(), 1);

        let mut plan = RequestPlan::new("m");
        plan.messages = vec![
            Message::user("one"),
            Message::assistant(vec![]),
            Message::user("two"),
        ];
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(body["messages"].as_array().expect("array").len(), 3);
    }

    #[test]
    fn an_image_becomes_an_image_block() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message::user_parts(vec![
            ContentPart::Text {
                text: "look".to_owned(),
            },
            ContentPart::Image {
                image: crate::message::ImageRef {
                    id: 1,
                    media_type: "image/png".to_owned(),
                    encoded_bytes: 5,
                },
            },
        ])];
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(body["messages"][0]["content"][1]["type"], "image");
    }

    #[test]
    fn replayed_thinking_blocks_are_sent_back_first() {
        let thinking = serde_json::json!({
            "type": "thinking",
            "thinking": "reasoning",
            "signature": "opaque",
        });
        let mut message = Message::assistant(vec![ContentPart::Text {
            text: "answer".to_owned(),
        }]);
        message.replay = Some(serde_json::to_string(&vec![thinking.clone()]).expect("encode"));

        let mut plan = RequestPlan::new("m");
        plan.messages = vec![message];
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(body["messages"][0]["content"][0], thinking);
        assert_eq!(body["messages"][0]["content"][1]["type"], "text");
    }

    #[test]
    fn an_assistant_message_with_no_blocks_still_encodes() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message::assistant(vec![])];
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn thinking_budgets_scale_with_effort() {
        use rune_core::config::Effort;
        assert_eq!(thinking_budget(Effort::Auto), None);
        assert_eq!(thinking_budget(Effort::None), None);
        assert_eq!(thinking_budget(Effort::Minimal), Some(1024));
        assert_eq!(thinking_budget(Effort::Max), Some(65_536));
        assert!(thinking_budget(Effort::High) > thinking_budget(Effort::Low));
    }

    /// Runs a reducer over frames.
    fn reduce(frames: &[&str]) -> (Vec<ProviderEvent>, Reducer) {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        for frame in frames {
            reducer.apply(Some(frame), &mut events).expect("apply");
        }
        (events, reducer)
    }

    #[test]
    fn text_blocks_accumulate_across_deltas() {
        let (events, reducer) = reduce(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::Stop);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::TextDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn a_tool_use_block_produces_complete_arguments() {
        let (events, reducer) = reduce(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::ToolCalls);
        let end = events
            .iter()
            .find_map(|event| match event {
                ProviderEvent::ToolCallEnd { arguments, .. } => Some(arguments.clone()),
                _ => None,
            })
            .expect("completed call");
        assert_eq!(end, "{\"path\":\"a.rs\"}");
    }

    #[test]
    fn a_tool_use_block_with_no_arguments_completes_with_empty_object() {
        let (events, reducer) = reduce(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"list","input":{}}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::ToolCalls);
        let end = events
            .iter()
            .find_map(|event| match event {
                ProviderEvent::ToolCallEnd { arguments, .. } => Some(arguments.clone()),
                _ => None,
            })
            .expect("completed call");
        assert_eq!(end, "{}");
    }

    #[test]
    fn thinking_is_retained_for_replay_and_streamed_as_reasoning() {
        let (events, reducer) = reduce(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":"sig"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ReasoningDelta { .. }))
        );
        let replay = reducer.replay().expect("replay");
        assert!(replay.contains("sig"));
    }

    #[test]
    fn replay_is_absent_without_thinking() {
        let (_, reducer) = reduce(&[
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert!(reducer.replay().is_none());
    }

    #[test]
    fn usage_replaces_rather_than_accumulates() {
        let (_, reducer) = reduce(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":1}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let usage = reducer.usage();
        // The later report is the authoritative total, not an increment.
        assert_eq!(usage.output_tokens, Some(42));
        assert_eq!(usage.input_tokens, Some(100));
    }

    #[test]
    fn cache_tokens_are_read_from_their_fields() {
        let (_, reducer) = reduce(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":5,"cache_creation_input_tokens":3}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let usage = reducer.usage();
        assert_eq!(usage.cache_read_tokens, Some(5));
        assert_eq!(usage.cache_write_tokens, Some(3));
    }

    #[test]
    fn a_truncated_stream_is_incomplete() {
        // A well formed prefix that simply stops before the terminal event.
        let (_, reducer) = reduce(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half"}}"#,
        ]);
        assert_eq!(
            reducer.finish().expect_err("truncated").code(),
            ErrorCode::IncompleteStream
        );
    }

    #[test]
    fn a_delta_before_its_block_start_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
        assert!(err.message().contains("unknown content block"));
    }

    #[test]
    fn a_stop_without_a_reason_is_rejected() {
        let (_, reducer) = reduce(&[r#"{"type":"message_stop"}"#]);
        assert_eq!(
            reducer.finish().expect_err("no reason").code(),
            ErrorCode::ProtocolViolation
        );
    }

    #[test]
    fn a_max_tokens_stop_maps_through() {
        let (_, reducer) = reduce(&[
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::MaxTokens);
    }

    #[test]
    fn a_refusal_stop_maps_through() {
        let (_, reducer) = reduce(&[
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::Refused);
    }

    #[test]
    fn an_unknown_stop_reason_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"type":"message_delta","delta":{"stop_reason":"sideways"}}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert!(err.message().contains("sideways"));
    }

    #[test]
    fn an_error_event_becomes_a_provider_failure() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::RequestRejected);
        assert!(err.message().contains("overloaded"));
    }

    #[test]
    fn a_tool_use_block_without_an_identifier_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"t","input":{}}}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert!(err.message().contains("no identifier"));
    }

    #[test]
    fn a_delta_for_an_unknown_block_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"type":"content_block_delta","index":9,"delta":{"type":"text_delta","text":"x"}}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
        assert!(err.message().contains('9'));
    }

    #[test]
    fn a_ping_is_ignored() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        reducer
            .apply(Some(r#"{"type":"ping"}"#), &mut events)
            .expect("accepted");
        assert!(events.is_empty());
    }

    #[test]
    fn an_unknown_event_is_ignored() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        reducer
            .apply(Some(r#"{"type":"something_new","x":1}"#), &mut events)
            .expect("accepted");
        assert!(events.is_empty());
    }

    #[test]
    fn exceeding_the_content_bound_is_rejected() {
        let mut reducer = Reducer::new().with_limits(Limit {
            max_content_bytes: 4,
            ..Limit::default()
        });
        let mut events = Vec::new();
        reducer
            .apply(
                Some(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
                &mut events,
            )
            .expect("start");
        let err = reducer
            .apply(
                Some(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"far too long"}}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }
}
