//! The OpenAI Chat Completions dialect.
//!
//! The widest interoperability surface, covering OpenAI-compatible endpoints
//! including local servers. Serialization and stream reduction live here; the
//! endpoint, credential, and HTTP client are supplied by the caller.

use rune_core::config::Effort;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::ToolCallId;

use crate::message::{ContentPart, Message, Role};
use crate::provider::{Provider, RequestPlan};
use crate::stream::{
    FinishReason, Limit, StreamReducer, Usage, incomplete_stream, protocol_violation,
};

/// Dialect name.
pub const NAME: &str = "chat_completions";

/// Path appended to the endpoint base URL.
pub const PATH: &str = "/chat/completions";

/// Largest error body retained for diagnostics.
pub const MAX_ERROR_BODY: usize = 64 * 1024;

/// The dialect.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChatCompletions;

impl ChatCompletions {
    /// Whether the request should ask for usage in the stream.
    ///
    /// Every compatible server that supports the field requires it to report
    /// token counts, and a server that does not understand it ignores it.
    const INCLUDE_USAGE: bool = true;
}

impl Provider for ChatCompletions {
    fn name(&self) -> &'static str {
        NAME
    }

    fn request_path(&self) -> &'static str {
        PATH
    }

    fn reducer(&self) -> Box<dyn StreamReducer> {
        Box::new(Reducer::new())
    }

    fn build_request(&self, plan: &RequestPlan) -> Result<serde_json::Value> {
        let mut messages: Vec<serde_json::Value> = Vec::new();

        // The system lane is emitted first and separately, because the
        // conversation model keeps it out of the message list.
        if !plan.instructions.is_empty() {
            messages.push(serde_json::json!({
                "role": "system",
                "content": plan.instructions,
            }));
        }

        for message in &plan.messages {
            messages.push(encode_message(message)?);
        }

        let mut body = serde_json::Map::new();
        body.insert("model".to_owned(), serde_json::json!(plan.model));
        body.insert("stream".to_owned(), serde_json::json!(true));
        if Self::INCLUDE_USAGE {
            body.insert(
                "stream_options".to_owned(),
                serde_json::json!({ "include_usage": true }),
            );
        }
        body.insert("messages".to_owned(), serde_json::Value::Array(messages));

        if plan.has_tools() {
            let tools: Result<Vec<serde_json::Value>> = plan
                .tools
                .iter()
                .map(|tool| {
                    Ok(serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.input_schema,
                        }
                    }))
                })
                .collect();
            body.insert("tools".to_owned(), serde_json::Value::Array(tools?));
            body.insert(
                "tool_choice".to_owned(),
                serde_json::json!(plan.tool_choice.as_str()),
            );
            if plan.parallel_tool_calls {
                body.insert("parallel_tool_calls".to_owned(), serde_json::json!(true));
            }
        }

        if let Some(max) = plan.max_output_tokens {
            // `max_tokens` rather than `max_completion_tokens`, because the
            // compatible-server ecosystem understands the former.
            body.insert("max_tokens".to_owned(), serde_json::json!(max));
        }

        Ok(serde_json::Value::Object(body))
    }
}

/// Encodes one message into the dialect's shape.
fn encode_message(message: &Message) -> Result<serde_json::Value> {
    match message.role {
        Role::System => Err(RuneError::invalid_field(
            "messages",
            "system content must be supplied as instructions, not as a message",
        )),
        Role::User => Ok(encode_user(message)),
        Role::Assistant => Ok(encode_assistant(message)),
        Role::Tool => Ok(encode_tool(message)),
    }
}

/// Encodes a user message, using content parts when images are present.
fn encode_user(message: &Message) -> serde_json::Value {
    let has_image = message
        .parts
        .iter()
        .any(|part| matches!(part, ContentPart::Image { .. }));

    if !has_image {
        return serde_json::json!({ "role": "user", "content": message.text() });
    }

    let parts: Vec<serde_json::Value> = message
        .parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(serde_json::json!({
                "type": "text",
                "text": text,
            })),
            ContentPart::Image { image } => Some(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!("image-ref:{}", image.id) },
            })),
            _ => None,
        })
        .collect();

    serde_json::json!({ "role": "user", "content": parts })
}

/// Encodes an assistant message, including any tool calls.
fn encode_assistant(message: &Message) -> serde_json::Value {
    let calls: Vec<serde_json::Value> = message
        .parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolCall {
                id,
                name,
                arguments,
            } => Some(serde_json::json!({
                "id": id.as_str(),
                "type": "function",
                "function": { "name": name, "arguments": arguments },
            })),
            _ => None,
        })
        .collect();

    let text = message.text();
    let mut value = serde_json::Map::new();
    value.insert("role".to_owned(), serde_json::json!("assistant"));

    if calls.is_empty() {
        value.insert("content".to_owned(), serde_json::json!(text));
    } else {
        // A message carrying tool calls sends a null content field, which is
        // what the reference implementation does.
        value.insert(
            "content".to_owned(),
            if text.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::json!(text)
            },
        );
        value.insert("tool_calls".to_owned(), serde_json::Value::Array(calls));
    }

    serde_json::Value::Object(value)
}

/// Encodes a tool result message.
fn encode_tool(message: &Message) -> serde_json::Value {
    let mut content = String::new();
    let mut id = String::new();
    for part in &message.parts {
        if let ContentPart::ToolResult {
            id: call_id,
            content: body,
            ..
        } = part
        {
            call_id.as_str().clone_into(&mut id);
            content.clone_from(body);
            break;
        }
    }

    serde_json::json!({
        "role": "tool",
        "content": content,
        "tool_call_id": id,
    })
}

/// Maps a reasoning effort onto the dialect's spelling.
#[must_use]
pub fn effort_wire(effort: Effort) -> Option<&'static str> {
    match effort {
        Effort::Auto => None,
        Effort::None => Some("none"),
        Effort::Minimal => Some("minimal"),
        Effort::Low => Some("low"),
        Effort::Medium => Some("medium"),
        Effort::High => Some("high"),
        Effort::Xhigh => Some("xhigh"),
        Effort::Max => Some("max"),
    }
}

/// Accumulates one tool call across streamed deltas.
#[derive(Debug, Default)]
struct PartialCall {
    id: Option<ToolCallId>,
    name: Option<String>,
    arguments: String,
    started: bool,
    ended: bool,
}

/// Reduces a Chat Completions stream into normalized events.
#[derive(Debug)]
pub struct Reducer {
    limits: Limit,
    /// Index into the tool-call table, because deltas address calls by index.
    calls: Vec<PartialCall>,
    content: String,
    usage: Usage,
    finish_reason: Option<FinishReason>,
    saw_terminal: bool,
    saw_done: bool,
    saw_role: bool,
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
            calls: Vec::new(),
            content: String::new(),
            usage: Usage::default(),
            finish_reason: None,
            saw_terminal: false,
            saw_done: false,
            saw_role: false,
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

        // An explicit error frame is a provider failure, not a decode problem.
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("the provider reported an error");
            return Err(RuneError::new(
                ErrorCode::RequestRejected,
                message.to_owned(),
            ));
        }

        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            let parsed = parse_usage(usage);
            self.usage = self.usage.merge_max(parsed);
            out.push(crate::stream::ProviderEvent::Usage(parsed));
        }

        let choices = value.get("choices");
        let Some(choices) = choices.and_then(serde_json::Value::as_array) else {
            // A usage-only frame has no choices, which is valid.
            return Ok(());
        };

        if choices.len() > 1 {
            return Err(protocol_violation(format!(
                "the stream returned {} choices, expected at most one",
                choices.len()
            )));
        }
        let Some(choice) = choices.first() else {
            return Ok(());
        };

        if let Some(index) = choice
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .filter(|index| *index != 0)
        {
            return Err(protocol_violation(format!(
                "the stream returned choice index {index}, expected 0"
            )));
        }

        if let Some(delta) = choice.get("delta") {
            self.apply_delta(delta, out)?;
        }

        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(serde_json::Value::as_str)
        {
            let mapped = map_finish_reason(reason)?;
            self.finish_reason = Some(mapped);
            self.saw_terminal = true;
        }

        Ok(())
    }

    /// Applies one delta object.
    fn apply_delta(
        &mut self,
        delta: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        if let Some(role) = delta.get("role").and_then(serde_json::Value::as_str) {
            if role != "assistant" {
                return Err(protocol_violation(format!(
                    "the stream reported role `{role}`, expected `assistant`"
                )));
            }
            self.saw_role = true;
        }

        // Functions and audio are deliberately unsupported; accepting them
        // silently would let an unimplemented feature appear to work.
        if delta.get("function_call").is_some() {
            return Err(protocol_violation(
                "the stream used the deprecated function_call field",
            ));
        }
        if delta.get("audio").is_some() {
            return Err(protocol_violation("the stream carried audio content"));
        }

        if let Some(refusal) = delta
            .get("refusal")
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
        {
            out.push(crate::stream::ProviderEvent::TextDelta {
                delta: refusal.to_owned(),
            });
        }

        for field in ["reasoning", "reasoning_content"] {
            if let Some(text) = delta
                .get(field)
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty())
            {
                out.push(crate::stream::ProviderEvent::ReasoningDelta {
                    delta: text.to_owned(),
                });
            }
        }

        if let Some(content) = delta
            .get("content")
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.content.push_str(content);
            if self.content.len() > self.limits.max_content_bytes {
                return Err(RuneError::too_large(
                    "stream.content",
                    self.content.len(),
                    self.limits.max_content_bytes,
                ));
            }
            out.push(crate::stream::ProviderEvent::TextDelta {
                delta: content.to_owned(),
            });
        }

        if let Some(calls) = delta
            .get("tool_calls")
            .and_then(serde_json::Value::as_array)
        {
            for call in calls {
                self.apply_tool_call(call, out)?;
            }
        }

        Ok(())
    }

    /// Applies one tool-call delta.
    fn apply_tool_call(
        &mut self,
        call: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let index = call
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| protocol_violation("a tool call delta carried no index"))?;

        if index >= self.limits.max_tool_calls {
            return Err(RuneError::too_large(
                "stream.tool_calls",
                index.saturating_add(1),
                self.limits.max_tool_calls,
            ));
        }

        while self.calls.len() <= index {
            self.calls.push(PartialCall::default());
        }

        let id = call.get("id").and_then(serde_json::Value::as_str);
        let name = call
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(serde_json::Value::as_str);
        let arguments = call
            .get("function")
            .and_then(|function| function.get("arguments"))
            .and_then(serde_json::Value::as_str);

        let Some(slot) = self.calls.get_mut(index) else {
            return Err(protocol_violation("tool call index out of range"));
        };

        if !slot.started {
            let id = id.ok_or_else(|| {
                protocol_violation("the first tool call delta carried no identifier")
            })?;
            let name = name
                .ok_or_else(|| protocol_violation("the first tool call delta carried no name"))?;
            slot.id = Some(ToolCallId::new(id)?);
            slot.name = Some(name.to_owned());
            slot.started = true;
            let id = slot
                .id
                .clone()
                .ok_or_else(|| protocol_violation("missing id"))?;
            out.push(crate::stream::ProviderEvent::ToolCallStart {
                id,
                name: name.to_owned(),
            });
            if let Some(arguments) = arguments {
                slot.arguments.push_str(arguments);
                let id = slot
                    .id
                    .clone()
                    .ok_or_else(|| protocol_violation("missing id"))?;
                out.push(crate::stream::ProviderEvent::ToolCallDelta {
                    id,
                    delta: arguments.to_owned(),
                });
            }
            return Ok(());
        }

        // A later delta may repeat the identifier, but it must not change it.
        if let (Some(id), Some(existing)) = (id, slot.id.as_ref())
            && id != existing.as_str()
        {
            return Err(protocol_violation(
                "a tool call delta changed the call identifier",
            ));
        }
        if let (Some(name), Some(existing)) = (name, slot.name.as_ref())
            && name != existing
        {
            return Err(protocol_violation(
                "a tool call delta changed the tool name",
            ));
        }

        if let Some(arguments) = arguments {
            slot.arguments.push_str(arguments);
            if slot.arguments.len() > self.limits.max_tool_arguments_bytes {
                return Err(RuneError::too_large(
                    "stream.tool_arguments",
                    slot.arguments.len(),
                    self.limits.max_tool_arguments_bytes,
                ));
            }
            let id = slot
                .id
                .clone()
                .ok_or_else(|| protocol_violation("missing id"))?;
            out.push(crate::stream::ProviderEvent::ToolCallDelta {
                id,
                delta: arguments.to_owned(),
            });
        }

        Ok(())
    }

    /// Emits completion events for every open tool call.
    fn close_tool_calls(&mut self, out: &mut Vec<crate::stream::ProviderEvent>) -> Result<()> {
        for slot in &mut self.calls {
            if !slot.started || slot.ended {
                continue;
            }
            let id = slot
                .id
                .clone()
                .ok_or_else(|| protocol_violation("a tool call has no identifier"))?;
            out.push(crate::stream::ProviderEvent::ToolCallEnd {
                id,
                arguments: slot.arguments.clone(),
            });
            slot.ended = true;
        }
        Ok(())
    }
}

impl StreamReducer for Reducer {
    fn apply(
        &mut self,
        payload: Option<&str>,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let Some(payload) = payload else {
            // End of transport. A stream that never reported a terminal reason
            // is incomplete, which the caller surfaces as a retryable failure.
            return Ok(());
        };

        if payload.trim() == "[DONE]" {
            self.saw_done = true;
            if self.finish_reason.is_none() {
                return Err(protocol_violation(
                    "the stream ended without a finish reason",
                ));
            }
            if self.finish_reason == Some(FinishReason::ToolCalls) {
                self.close_tool_calls(out)?;
            }
            return Ok(());
        }

        self.event_count = self.event_count.saturating_add(1);
        self.limits.check_events(self.event_count)?;
        self.apply_frame(payload, out)
    }

    fn is_finished(&self) -> bool {
        self.saw_done
    }

    fn usage(&self) -> Usage {
        self.usage
    }

    fn replay(&self) -> Option<String> {
        // The dialect has no signed reasoning to replay, but the identifiers
        // are recorded so a filtered history can drop a call and its result
        // together.
        let ids: Vec<&str> = self
            .calls
            .iter()
            .filter_map(|call| call.id.as_ref().map(ToolCallId::as_str))
            .collect();
        if ids.is_empty() {
            return None;
        }
        serde_json::to_string(&serde_json::json!({ "_tool_call_ids": ids })).ok()
    }

    fn finish(&self) -> Result<FinishReason> {
        if !self.saw_done {
            return Err(incomplete_stream());
        }
        if let Some(reason) = self.finish_reason {
            // Tool calls that never reported a name cannot be executed.
            if reason == FinishReason::ToolCalls
                && self.calls.iter().any(|call| call.name.is_none())
            {
                return Err(protocol_violation(
                    "the stream ended with an incomplete tool call",
                ));
            }
            if self.saw_role || !self.content.is_empty() || !self.calls.is_empty() {
                return Ok(reason);
            }
            // A finish with no content at all is still a valid empty answer.
            return Ok(reason);
        }
        Err(protocol_violation(
            "the stream ended without a finish reason",
        ))
    }
}

/// Reads a usage object.
fn parse_usage(value: &serde_json::Value) -> Usage {
    let field = |name: &str| value.get(name).and_then(serde_json::Value::as_u64);
    Usage {
        input_tokens: field("prompt_tokens"),
        output_tokens: field("completion_tokens"),
        cache_read_tokens: value
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(serde_json::Value::as_u64),
        cache_write_tokens: value
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cache_write_tokens"))
            .and_then(serde_json::Value::as_u64),
        reasoning_tokens: value
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(serde_json::Value::as_u64),
    }
}

/// Maps a dialect finish reason onto the normalized one.
fn map_finish_reason(raw: &str) -> Result<FinishReason> {
    match raw {
        "stop" => Ok(FinishReason::Stop),
        "tool_calls" | "function_call" => Ok(FinishReason::ToolCalls),
        "length" => Ok(FinishReason::MaxTokens),
        "content_filter" => Ok(FinishReason::ContentFilter),
        other => Err(protocol_violation(format!(
            "the stream reported an unknown finish reason `{other}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolSpec;
    use crate::provider::ToolChoice;
    use crate::stream::ProviderEvent;

    fn plan_with_user(text: &str) -> RequestPlan {
        let mut plan = RequestPlan::new("test/model");
        plan.instructions = "be helpful".to_owned();
        plan.messages = vec![Message::user(text)];
        plan
    }

    #[test]
    fn the_request_carries_the_model_stream_flag_and_system_lane() {
        let body = ChatCompletions
            .build_request(&plan_with_user("hi"))
            .expect("build");
        assert_eq!(body["model"], "test/model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be helpful");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "hi");
    }

    #[test]
    fn an_empty_instruction_lane_is_omitted() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message::user("hi")];
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert_eq!(body["messages"].as_array().expect("array").len(), 1);
    }

    #[test]
    fn tools_are_encoded_in_the_function_envelope() {
        let mut plan = plan_with_user("hi");
        plan.tools = vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        }];
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["required"][0],
            "path"
        );
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], true);
    }

    #[test]
    fn no_tool_fields_are_emitted_without_tools() {
        let body = ChatCompletions
            .build_request(&plan_with_user("hi"))
            .expect("build");
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn an_assistant_tool_call_encodes_with_null_content() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![
            Message::assistant(vec![ContentPart::ToolCall {
                id: ToolCallId::new("call_1").expect("id"),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"a\"}".to_owned(),
            }]),
            Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult {
                    id: ToolCallId::new("call_1").expect("id"),
                    name: "read_file".to_owned(),
                    content: "contents".to_owned(),
                    is_error: false,
                }],
                replay: None,
            },
        ];
        let body = ChatCompletions.build_request(&plan).expect("build");
        let assistant = &body["messages"][0];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant["content"].is_null());
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );

        let tool = &body["messages"][1];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["content"], "contents");
        assert_eq!(tool["tool_call_id"], "call_1");
    }

    #[test]
    fn an_image_becomes_a_content_part() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message::user_parts(vec![
            ContentPart::Text {
                text: "look".to_owned(),
            },
            ContentPart::Image {
                image: crate::message::ImageRef {
                    id: 7,
                    media_type: "image/png".to_owned(),
                    encoded_bytes: 100,
                },
            },
        ])];
        let body = ChatCompletions.build_request(&plan).expect("build");
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
    }

    #[test]
    fn a_plain_user_message_uses_a_string_body() {
        let body = ChatCompletions
            .build_request(&plan_with_user("plain"))
            .expect("build");
        assert!(body["messages"][1]["content"].is_string());
    }

    #[test]
    fn max_tokens_is_emitted_only_when_set() {
        let body = ChatCompletions
            .build_request(&plan_with_user("hi"))
            .expect("build");
        assert!(body.get("max_tokens").is_none());

        let mut plan = plan_with_user("hi");
        plan.max_output_tokens = Some(4096);
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn a_system_message_in_the_list_is_rejected() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message::system("inline")];
        let err = ChatCompletions.build_request(&plan).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn tool_choice_is_encoded_from_the_plan() {
        let mut plan = plan_with_user("hi");
        plan.tools = vec![ToolSpec {
            name: "t".to_owned(),
            description: "d".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];
        plan.tool_choice = ToolChoice::Required;
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert_eq!(body["tool_choice"], "required");
    }

    #[test]
    fn effort_maps_onto_wire_names() {
        assert_eq!(effort_wire(Effort::Auto), None);
        assert_eq!(effort_wire(Effort::Low), Some("low"));
        assert_eq!(effort_wire(Effort::Xhigh), Some("xhigh"));
        assert_eq!(effort_wire(Effort::Max), Some("max"));
    }

    /// Runs a reducer over a list of frames.
    fn reduce(frames: &[&str]) -> (Vec<ProviderEvent>, Reducer) {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        for frame in frames {
            reducer.apply(Some(frame), &mut events).expect("apply");
        }
        (events, reducer)
    }

    #[test]
    fn text_deltas_are_emitted_in_order() {
        let (events, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"Hello"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":" world"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
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
    fn reasoning_deltas_use_both_known_field_names() {
        let (events, _) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"reasoning":"step one"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":" step two"}}]}"#,
        ]);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ReasoningDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "step one step two");
    }

    #[test]
    fn a_tool_call_accumulates_across_deltas() {
        let (events, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::ToolCalls);

        let starts: Vec<&ProviderEvent> = events
            .iter()
            .filter(|event| matches!(event, ProviderEvent::ToolCallStart { .. }))
            .collect();
        assert_eq!(starts.len(), 1);

        let end = events
            .iter()
            .find_map(|event| match event {
                ProviderEvent::ToolCallEnd { arguments, .. } => Some(arguments.clone()),
                _ => None,
            })
            .expect("a completed call");
        assert_eq!(end, "{\"path\":\"a.rs\"}");
    }

    #[test]
    fn parallel_tool_calls_are_tracked_separately() {
        let (events, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"name":"a","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"c1","function":{"name":"b","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::ToolCalls);
        let ends = events
            .iter()
            .filter(|event| matches!(event, ProviderEvent::ToolCallEnd { .. }))
            .count();
        assert_eq!(ends, 2);
    }

    #[test]
    fn a_tool_call_that_changes_identifier_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"name":"a","arguments":"{}"}}]}}]}"#),
                &mut events,
            )
            .expect("first");
        let err = reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"arguments":"{}"}}]}}]}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
    }

    #[test]
    fn a_tool_call_that_changes_name_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"name":"a","arguments":"{}"}}]}}]}"#),
                &mut events,
            )
            .expect("first");
        let err = reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"b","arguments":"{}"}}]}}]}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
    }

    #[test]
    fn usage_is_parsed_and_merged() {
        let (events, reducer) = reduce(&[
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::Usage(_)))
        );
        let usage = reducer.usage();
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.cache_read_tokens, None);
    }

    #[test]
    fn a_usage_only_frame_without_choices_is_valid() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        reducer
            .apply(Some(r#"{"usage":{"prompt_tokens":1}}"#), &mut events)
            .expect("accepted");
        assert_eq!(reducer.usage().input_tokens, Some(1));
    }

    #[test]
    fn cache_and_reasoning_details_are_read() {
        let (_, reducer) = reduce(&[
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":20}}}"#,
        ]);
        let usage = reducer.usage();
        assert_eq!(usage.cache_read_tokens, Some(40));
        assert_eq!(usage.reasoning_tokens, Some(20));
    }

    #[test]
    fn a_truncated_stream_is_incomplete() {
        let (_, reducer) = reduce(&[r#"{"choices":[{"index":0,"delta":{"content":"half"}}]}"#]);
        let err = reducer.finish().expect_err("truncated");
        assert_eq!(err.code(), ErrorCode::IncompleteStream);
    }

    #[test]
    fn a_done_without_a_finish_reason_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(Some("[DONE]"), &mut events)
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
    }

    #[test]
    fn a_second_choice_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{}},{"index":1,"delta":{}}]}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
    }

    #[test]
    fn a_nonzero_choice_index_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(Some(r#"{"choices":[{"index":1,"delta":{}}]}"#), &mut events)
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
    }

    #[test]
    fn an_unexpected_role_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{"role":"user"}}]}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
    }

    #[test]
    fn the_deprecated_function_call_field_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{"function_call":{"name":"x"}}}]}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert!(err.message().contains("function_call"));
    }

    #[test]
    fn an_audio_delta_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{"audio":{"data":"x"}}}]}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert!(err.message().contains("audio"));
    }

    #[test]
    fn an_unknown_finish_reason_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"sideways"}]}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert!(err.message().contains("sideways"));
    }

    #[test]
    fn a_length_finish_reason_maps_to_the_token_limit() {
        let (_, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"length"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::MaxTokens);
    }

    #[test]
    fn a_content_filter_finish_reason_maps_through() {
        let (_, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(
            reducer.finish().expect("finished"),
            FinishReason::ContentFilter
        );
    }

    #[test]
    fn an_error_frame_becomes_a_provider_failure() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"error":{"message":"rate limited","type":"rate_limit"}}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::RequestRejected);
        assert!(err.message().contains("rate limited"));
    }

    #[test]
    fn malformed_json_is_a_protocol_violation() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(Some("{not json"), &mut events)
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
    }

    #[test]
    fn a_tool_call_with_no_name_is_rejected_immediately() {
        // Rejecting at the first delta is stronger than waiting for the finish
        // reason, because a call with no name can never be executed.
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"arguments":"{}"}}]}}]}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
        assert!(err.message().contains("no name"));
    }

    #[test]
    fn a_tool_call_with_no_identifier_is_rejected_immediately() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"read_file","arguments":"{}"}}]}}]}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
        assert!(err.message().contains("no identifier"));
    }

    #[test]
    fn exceeding_the_content_bound_is_rejected() {
        let mut reducer = Reducer::new().with_limits(Limit {
            max_content_bytes: 4,
            ..Limit::default()
        });
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{"content":"far too long"}}]}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn replay_records_the_call_identifiers() {
        let (_, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"name":"a","arguments":"{}"}}]}}]}"#,
        ]);
        let replay = reducer.replay().expect("replay");
        assert!(replay.contains("c0"));
    }

    #[test]
    fn replay_is_absent_without_tool_calls() {
        let (_, reducer) = reduce(&[r#"{"choices":[{"index":0,"delta":{"content":"hi"}}]}"#]);
        assert!(reducer.replay().is_none());
    }

    #[test]
    fn an_empty_answer_still_finishes() {
        let (_, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::Stop);
    }
}
