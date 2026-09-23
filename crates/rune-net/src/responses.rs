//! The OpenAI Responses dialect.
//!
//! Used by subscription endpoints. Differs from Chat Completions in the message
//! model: instructions live in a dedicated field, content is a list of typed
//! items, and reasoning is carried as an opaque item that must be replayed
//! verbatim rather than reconstructed.

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::ToolCallId;

use crate::message::{ContentPart, Message, Role};
use crate::provider::{Provider, RequestPlan};
use crate::stream::{
    FinishReason, Limit, StreamReducer, Usage, incomplete_stream, protocol_violation,
};

/// Dialect name.
pub const NAME: &str = "responses";

/// Path appended to the endpoint base URL.
pub const PATH: &str = "/responses";

/// Path listing the served models, a sibling of the responses path.
pub const MODELS_PATH: &str = "/models";

/// Default instructions when a plan carries none.
///
/// The endpoint requires the field, so an empty plan still produces a valid
/// request rather than a rejection.
pub const DEFAULT_INSTRUCTIONS: &str = "You are a helpful assistant.";

/// The dialect.
#[derive(Clone, Copy, Debug, Default)]
pub struct Responses;

impl Provider for Responses {
    fn name(&self) -> &'static str {
        NAME
    }

    fn request_path(&self) -> &'static str {
        PATH
    }

    fn models_path(&self) -> Option<&'static str> {
        Some(MODELS_PATH)
    }

    fn reducer(&self) -> Box<dyn StreamReducer> {
        Box::new(Reducer::new())
    }

    fn build_request(&self, plan: &RequestPlan) -> Result<serde_json::Value> {
        let mut input: Vec<serde_json::Value> = Vec::new();

        for message in &plan.messages {
            encode_message(message, &mut input)?;
        }

        let instructions = if plan.instructions.is_empty() {
            DEFAULT_INSTRUCTIONS.to_owned()
        } else {
            plan.instructions.clone()
        };

        let mut body = serde_json::Map::new();
        body.insert("model".to_owned(), serde_json::json!(plan.model));
        body.insert("store".to_owned(), serde_json::json!(false));
        body.insert("stream".to_owned(), serde_json::json!(true));
        body.insert("instructions".to_owned(), serde_json::json!(instructions));
        body.insert("input".to_owned(), serde_json::Value::Array(input));
        body.insert(
            "include".to_owned(),
            serde_json::json!(["reasoning.encrypted_content"]),
        );

        if plan.has_tools() {
            let tools: Vec<serde_json::Value> = plan
                .tools
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                        "strict": false,
                    })
                })
                .collect();
            body.insert("tools".to_owned(), serde_json::Value::Array(tools));
            body.insert(
                "tool_choice".to_owned(),
                serde_json::json!(plan.tool_choice.as_str()),
            );
            body.insert(
                "parallel_tool_calls".to_owned(),
                serde_json::json!(plan.parallel_tool_calls),
            );
        }

        // The output token ceiling is deliberately not sent: the endpoint
        // chooses, and sending one has been rejected by subscription routes.

        if plan.fast_mode {
            body.insert("service_tier".to_owned(), serde_json::json!("priority"));
        }

        Ok(serde_json::Value::Object(body))
    }
}

/// Encodes one message into one or more input items.
fn encode_message(message: &Message, out: &mut Vec<serde_json::Value>) -> Result<()> {
    match message.role {
        Role::System => Err(RuneError::invalid_field(
            "messages",
            "system content must be supplied as instructions, not as a message",
        )),
        Role::User => {
            out.push(encode_user(message));
            Ok(())
        }
        Role::Assistant => {
            encode_assistant(message, out);
            Ok(())
        }
        Role::Tool => {
            encode_tool(message, out);
            Ok(())
        }
    }
}

/// Encodes a user message.
fn encode_user(message: &Message) -> serde_json::Value {
    let content: Vec<serde_json::Value> = message
        .parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(serde_json::json!({
                "type": "input_text",
                "text": text,
            })),
            ContentPart::Image { image } => Some(serde_json::json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": format!("image-ref:{}", image.id),
            })),
            _ => None,
        })
        .collect();

    serde_json::json!({ "role": "user", "content": content })
}

/// Returns the items stored in a message's replay field.
///
/// A malformed replay is ignored rather than fatal: the field holds provider
/// state this build wrote, and a request without it is still valid.
fn replay_items(message: &Message) -> Vec<serde_json::Value> {
    let Some(replay) = &message.replay else {
        return Vec::new();
    };
    serde_json::from_str::<serde_json::Value>(replay)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

/// Encodes an assistant message as a message item plus any function calls.
fn encode_assistant(message: &Message, out: &mut Vec<serde_json::Value>) {
    // An opaque reasoning item is replayed before the message it belongs to.
    out.extend(replay_items(message));

    let text = message.text();
    if !text.is_empty() {
        out.push(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
        }));
    }

    for part in &message.parts {
        if let ContentPart::ToolCall {
            id,
            name,
            arguments,
        } = part
        {
            out.push(serde_json::json!({
                "type": "function_call",
                "call_id": id.as_str(),
                "name": name,
                "arguments": arguments,
            }));
        }
    }
}

/// Encodes tool results as function call output items.
fn encode_tool(message: &Message, out: &mut Vec<serde_json::Value>) {
    for part in &message.parts {
        if let ContentPart::ToolResult {
            id,
            content,
            is_error,
            ..
        } = part
        {
            let output = if *is_error {
                format!("error: {content}")
            } else {
                content.clone()
            };
            out.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": id.as_str(),
                "output": output,
            }));
        }
    }
}

/// Maps a reasoning effort onto the dialect's spelling.
///
/// `minimal` is not accepted by this endpoint, so it becomes `low`.
#[must_use]
pub fn effort_wire(effort: rune_core::config::Effort) -> Option<&'static str> {
    use rune_core::config::Effort;
    match effort {
        Effort::Auto => None,
        Effort::None | Effort::Minimal => Some("low"),
        Effort::Low => Some("low"),
        Effort::Medium => Some("medium"),
        Effort::High => Some("high"),
        Effort::Xhigh | Effort::Max => Some("xhigh"),
    }
}

/// Accumulated state for one response item.
#[derive(Debug, Default)]
struct PartialCall {
    id: Option<ToolCallId>,
    arguments: String,
    ended: bool,
}

/// Reduces a Responses stream into normalized events.
#[derive(Debug)]
pub struct Reducer {
    limits: Limit,
    calls: std::collections::BTreeMap<String, PartialCall>,
    /// Opaque reasoning items, in arrival order.
    reasoning: Vec<serde_json::Value>,
    content: String,
    usage: Usage,
    terminal: Option<FinishReason>,
    completed: bool,
    event_count: usize,
    /// Set when a `response.failed` or `error` event was seen.
    failure: Option<String>,
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
            calls: std::collections::BTreeMap::new(),
            reasoning: Vec::new(),
            content: String::new(),
            usage: Usage::default(),
            terminal: None,
            completed: false,
            event_count: 0,
            failure: None,
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
            "response.created" | "response.in_progress" | "response.queued" => Ok(()),
            "response.output_text.delta" => self.apply_text_delta(&value, out),
            "response.reasoning_summary_text.delta" => Self::apply_reasoning_delta(&value, out),
            "response.output_item.added" => self.apply_item_added(&value, out),
            "response.function_call_arguments.delta" => self.apply_arguments_delta(&value, out),
            "response.output_item.done" => self.apply_item_done(&value, out),
            "response.completed" | "response.incomplete" => self.apply_terminal(&value, kind),
            "response.failed" => {
                let message = value
                    .get("response")
                    .and_then(|response| response.get("error"))
                    .and_then(|error| error.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("the provider reported a failure");
                self.failure = Some(message.to_owned());
                Err(RuneError::new(
                    ErrorCode::RequestRejected,
                    message.to_owned(),
                ))
            }
            "error" => {
                let message = value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("the provider reported an error");
                self.failure = Some(message.to_owned());
                Err(RuneError::new(
                    ErrorCode::RequestRejected,
                    message.to_owned(),
                ))
            }
            // Usage may arrive as its own event on some routes.
            "response.usage" => {
                if let Some(usage) = value.get("usage") {
                    self.record_usage(usage, out);
                }
                Ok(())
            }
            // Unknown event types are ignored, because the vocabulary grows and
            // an old client must not break on a new event.
            _ => Ok(()),
        }
    }

    /// Appends a text delta.
    fn apply_text_delta(
        &mut self,
        value: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let Some(delta) = value.get("delta").and_then(serde_json::Value::as_str) else {
            return Ok(());
        };
        if delta.is_empty() {
            return Ok(());
        }
        self.content.push_str(delta);
        if self.content.len() > self.limits.max_content_bytes {
            return Err(RuneError::too_large(
                "stream.content",
                self.content.len(),
                self.limits.max_content_bytes,
            ));
        }
        out.push(crate::stream::ProviderEvent::TextDelta {
            delta: delta.to_owned(),
        });
        Ok(())
    }

    /// Appends a reasoning summary delta.
    fn apply_reasoning_delta(
        value: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let Some(delta) = value
            .get("delta")
            .and_then(serde_json::Value::as_str)
            .filter(|delta| !delta.is_empty())
        else {
            return Ok(());
        };
        out.push(crate::stream::ProviderEvent::ReasoningDelta {
            delta: delta.to_owned(),
        });
        Ok(())
    }

    /// Handles a newly added output item.
    fn apply_item_added(
        &mut self,
        value: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let Some(item) = value.get("item") else {
            return Ok(());
        };
        let item_type = item.get("type").and_then(serde_json::Value::as_str);

        match item_type {
            Some("reasoning") => {
                // Retained verbatim so it can be replayed exactly.
                self.reasoning.push(item.clone());
                Ok(())
            }
            Some("function_call") => {
                let Some(call_id) = item.get("call_id").and_then(serde_json::Value::as_str) else {
                    return Err(protocol_violation(
                        "a function call item carried no call identifier",
                    ));
                };
                let Some(name) = item.get("name").and_then(serde_json::Value::as_str) else {
                    return Err(protocol_violation("a function call item carried no name"));
                };

                if self.calls.contains_key(call_id) {
                    return Err(protocol_violation(format!(
                        "function call `{call_id}` was declared twice"
                    )));
                }
                if self.calls.len() >= self.limits.max_tool_calls {
                    return Err(RuneError::too_large(
                        "stream.tool_calls",
                        self.calls.len().saturating_add(1),
                        self.limits.max_tool_calls,
                    ));
                }

                let id = ToolCallId::new(call_id)?;
                self.calls.insert(
                    call_id.to_owned(),
                    PartialCall {
                        id: Some(id.clone()),
                        arguments: String::new(),
                        ended: false,
                    },
                );
                out.push(crate::stream::ProviderEvent::ToolCallStart {
                    id,
                    name: name.to_owned(),
                });

                // An existing arguments string may arrive with the item.
                if let Some(arguments) = item
                    .get("arguments")
                    .and_then(serde_json::Value::as_str)
                    .filter(|arguments| !arguments.is_empty())
                {
                    self.append_arguments(call_id, arguments, out)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Appends a function call argument fragment.
    fn apply_arguments_delta(
        &mut self,
        value: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let Some(call_id) = value.get("item_id").and_then(serde_json::Value::as_str) else {
            return Ok(());
        };
        let Some(delta) = value.get("delta").and_then(serde_json::Value::as_str) else {
            return Ok(());
        };
        if delta.is_empty() {
            return Ok(());
        }
        self.append_arguments(call_id, delta, out)
    }

    /// Appends an argument fragment to a call, checking the bound.
    fn append_arguments(
        &mut self,
        call_id: &str,
        fragment: &str,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let Some(slot) = self.calls.get_mut(call_id) else {
            return Err(protocol_violation(format!(
                "arguments arrived for unknown function call `{call_id}`"
            )));
        };
        slot.arguments.push_str(fragment);
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
            .ok_or_else(|| protocol_violation("a function call has no identifier"))?;
        out.push(crate::stream::ProviderEvent::ToolCallDelta {
            id,
            delta: fragment.to_owned(),
        });
        Ok(())
    }

    /// Completes an output item.
    fn apply_item_done(
        &mut self,
        value: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let Some(item) = value.get("item") else {
            return Ok(());
        };
        let Some(call_id) = item.get("call_id").and_then(serde_json::Value::as_str) else {
            return Ok(());
        };
        let Some(slot) = self.calls.get_mut(call_id) else {
            return Ok(());
        };
        if slot.ended {
            return Ok(());
        }
        slot.ended = true;
        let id = slot
            .id
            .clone()
            .ok_or_else(|| protocol_violation("a function call has no identifier"))?;
        out.push(crate::stream::ProviderEvent::ToolCallEnd {
            id,
            arguments: slot.arguments.clone(),
        });
        Ok(())
    }

    /// Records usage from a value.
    fn record_usage(
        &mut self,
        usage: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) {
        let parsed = parse_usage(usage);
        self.usage = self.usage.merge_max(parsed);
        out.push(crate::stream::ProviderEvent::Usage(parsed));
    }

    /// Handles the terminal completion event.
    fn apply_terminal(&mut self, value: &serde_json::Value, kind: &str) -> Result<()> {
        let response = value.get("response");

        if let Some(usage) = response.and_then(|response| response.get("usage")) {
            let parsed = parse_usage(usage);
            self.usage = self.usage.merge_max(parsed);
        }

        // Preserve any reasoning item the completion reports, which is the
        // authoritative copy for replay.
        if let Some(items) = response
            .and_then(|response| response.get("output"))
            .and_then(serde_json::Value::as_array)
        {
            for item in items {
                if item
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| kind == "reasoning")
                {
                    self.reasoning.push(item.clone());
                }
            }
        }

        let reason = if kind == "response.incomplete" {
            FinishReason::MaxTokens
        } else {
            match response
                .and_then(|response| response.get("status"))
                .and_then(serde_json::Value::as_str)
            {
                Some("incomplete") => FinishReason::MaxTokens,
                Some("failed") => {
                    return Err(RuneError::new(
                        ErrorCode::RequestRejected,
                        "the provider reported a failed response",
                    ));
                }
                _ => {
                    // A completion whose output contains a function call is a
                    // tool turn, regardless of the reported status.
                    if self.calls.is_empty() {
                        FinishReason::Stop
                    } else {
                        FinishReason::ToolCalls
                    }
                }
            }
        };

        self.terminal = Some(reason);
        self.completed = true;
        Ok(())
    }

    /// Emits completion events for any call not already closed.
    fn close_remaining(&mut self, out: &mut Vec<crate::stream::ProviderEvent>) -> Result<()> {
        let ids: Vec<String> = self.calls.keys().cloned().collect();
        for key in ids {
            let Some(slot) = self.calls.get_mut(&key) else {
                continue;
            };
            if slot.ended {
                continue;
            }
            slot.ended = true;
            let id = slot
                .id
                .clone()
                .ok_or_else(|| protocol_violation("a function call has no identifier"))?;
            let arguments = slot.arguments.clone();
            out.push(crate::stream::ProviderEvent::ToolCallEnd { id, arguments });
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
            return Ok(());
        };
        if payload.trim() == "[DONE]" {
            return Ok(());
        }

        self.event_count = self.event_count.saturating_add(1);
        self.limits.check_events(self.event_count)?;

        self.apply_frame(payload, out)?;

        // Close calls as soon as the terminal event has been seen, so the
        // caller sees a complete set of events.
        if self.completed && self.terminal == Some(FinishReason::ToolCalls) {
            self.close_remaining(out)?;
        }
        Ok(())
    }

    fn is_finished(&self) -> bool {
        self.completed
    }

    fn usage(&self) -> Usage {
        self.usage
    }

    fn replay(&self) -> Option<String> {
        if self.reasoning.is_empty() {
            return None;
        }
        serde_json::to_string(&self.reasoning).ok()
    }

    fn finish(&self) -> Result<FinishReason> {
        if let Some(message) = &self.failure {
            return Err(RuneError::new(ErrorCode::RequestRejected, message.clone()));
        }
        if !self.completed {
            return Err(incomplete_stream());
        }
        self.terminal
            .ok_or_else(|| protocol_violation("the stream ended without a status"))
    }
}

/// Reads a usage object.
fn parse_usage(value: &serde_json::Value) -> Usage {
    let field = |name: &str| value.get(name).and_then(serde_json::Value::as_u64);
    Usage {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_read_tokens: value
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(serde_json::Value::as_u64),
        cache_write_tokens: value
            .get("input_tokens_details")
            .and_then(|details| details.get("cache_write_tokens"))
            .and_then(serde_json::Value::as_u64),
        reasoning_tokens: value
            .get("output_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(serde_json::Value::as_u64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolSpec;
    use crate::stream::ProviderEvent;

    fn plan_with_user(text: &str) -> RequestPlan {
        let mut plan = RequestPlan::new("test/model");
        plan.instructions = "be helpful".to_owned();
        plan.messages = vec![Message::user(text)];
        plan
    }

    #[test]
    fn the_request_uses_the_responses_shape() {
        let body = Responses
            .build_request(&plan_with_user("hi"))
            .expect("build");
        assert_eq!(body["model"], "test/model");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "be helpful");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn a_plan_without_instructions_gets_a_default() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message::user("hi")];
        let body = Responses.build_request(&plan).expect("build");
        assert_eq!(body["instructions"], DEFAULT_INSTRUCTIONS);
    }

    #[test]
    fn output_tokens_are_deliberately_not_sent() {
        let mut plan = plan_with_user("hi");
        plan.max_output_tokens = Some(4096);
        let body = Responses.build_request(&plan).expect("build");
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn fast_mode_sets_the_priority_tier() {
        let mut plan = plan_with_user("hi");
        plan.fast_mode = true;
        let body = Responses.build_request(&plan).expect("build");
        assert_eq!(body["service_tier"], "priority");
    }

    #[test]
    fn tools_use_the_flat_function_shape() {
        let mut plan = plan_with_user("hi");
        plan.tools = vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];
        let body = Responses.build_request(&plan).expect("build");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert!(body["tools"][0].get("function").is_none());
    }

    #[test]
    fn an_assistant_tool_call_becomes_a_function_call_item() {
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
        let body = Responses.build_request(&plan).expect("build");
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["call_id"], "call_1");
        assert_eq!(body["input"][0]["name"], "read_file");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["output"], "contents");
    }

    #[test]
    fn a_failed_tool_result_is_marked_in_the_output() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult {
                id: ToolCallId::new("c").expect("id"),
                name: "read_file".to_owned(),
                content: "not found".to_owned(),
                is_error: true,
            }],
            replay: None,
        }];
        let body = Responses.build_request(&plan).expect("build");
        assert_eq!(body["input"][0]["output"], "error: not found");
    }

    #[test]
    fn replayed_reasoning_items_are_sent_back_verbatim() {
        let item = serde_json::json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [],
            "encrypted_content": "opaque-blob",
        });
        let mut message = Message::assistant(vec![ContentPart::Text {
            text: "answer".to_owned(),
        }]);
        message.replay = Some(serde_json::to_string(&vec![item.clone()]).expect("encode"));

        let mut plan = RequestPlan::new("m");
        plan.messages = vec![message];
        let body = Responses.build_request(&plan).expect("build");
        assert_eq!(body["input"][0], item);
        assert_eq!(body["input"][1]["type"], "message");
    }

    #[test]
    fn an_image_becomes_an_input_image_part() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message::user_parts(vec![
            ContentPart::Text {
                text: "look".to_owned(),
            },
            ContentPart::Image {
                image: crate::message::ImageRef {
                    id: 3,
                    media_type: "image/png".to_owned(),
                    encoded_bytes: 10,
                },
            },
        ])];
        let body = Responses.build_request(&plan).expect("build");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
    }

    #[test]
    fn effort_maps_minimal_onto_low() {
        use rune_core::config::Effort;
        assert_eq!(effort_wire(Effort::Auto), None);
        assert_eq!(effort_wire(Effort::Minimal), Some("low"));
        assert_eq!(effort_wire(Effort::None), Some("low"));
        assert_eq!(effort_wire(Effort::High), Some("high"));
        assert_eq!(effort_wire(Effort::Max), Some("xhigh"));
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
    fn text_deltas_accumulate() {
        let (events, reducer) = reduce(&[
            r#"{"type":"response.output_text.delta","delta":"Hello"}"#,
            r#"{"type":"response.output_text.delta","delta":" world"}"#,
            r#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#,
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
    fn a_function_call_item_produces_start_delta_and_end() {
        let (events, reducer) = reduce(&[
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"read_file","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"call_1","delta":"{\"path\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"call_1","delta":"\"a.rs\"}"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"read_file"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#,
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
    fn a_completion_without_an_item_done_still_closes_its_calls() {
        let (events, reducer) = reduce(&[
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"c","name":"t","arguments":"{}"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#,
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::ToolCalls);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::ToolCallEnd { .. }))
        );
    }

    #[test]
    fn a_reasoning_item_is_retained_for_replay() {
        let (_, reducer) = reduce(&[
            r#"{"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#,
        ]);
        let replay = reducer.replay().expect("replay");
        assert!(replay.contains("opaque"));
    }

    #[test]
    fn a_completion_reporting_output_items_retains_reasoning_from_it() {
        let (_, reducer) = reduce(&[
            r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"reasoning","id":"rs_2","encrypted_content":"from-output"}]}}"#,
        ]);
        let replay = reducer.replay().expect("replay");
        assert!(replay.contains("from-output"));
    }

    #[test]
    fn replay_is_absent_without_reasoning() {
        let (_, reducer) = reduce(&[
            r#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#,
        ]);
        assert!(reducer.replay().is_none());
    }

    #[test]
    fn usage_is_read_from_the_completion() {
        let (_, reducer) = reduce(&[
            r#"{"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":100,"output_tokens":50,"input_tokens_details":{"cached_tokens":20},"output_tokens_details":{"reasoning_tokens":10}}}}"#,
        ]);
        let usage = reducer.usage();
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
        assert_eq!(usage.cache_read_tokens, Some(20));
        assert_eq!(usage.reasoning_tokens, Some(10));
    }

    #[test]
    fn an_incomplete_response_maps_to_the_token_limit() {
        let (_, reducer) = reduce(&[
            r#"{"type":"response.incomplete","response":{"status":"incomplete","output":[]}}"#,
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::MaxTokens);
    }

    #[test]
    fn a_truncated_stream_is_incomplete() {
        let (_, reducer) = reduce(&[r#"{"type":"response.output_text.delta","delta":"half"}"#]);
        assert_eq!(
            reducer.finish().expect_err("truncated").code(),
            ErrorCode::IncompleteStream
        );
    }

    #[test]
    fn a_failed_response_is_a_provider_failure() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"type":"response.failed","response":{"status":"failed","error":{"message":"bad"}}}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::RequestRejected);
        assert!(err.message().contains("bad"));
    }

    #[test]
    fn an_error_event_is_a_provider_failure() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"type":"error","message":"unavailable"}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::RequestRejected);
    }

    #[test]
    fn an_unknown_event_type_is_ignored() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        reducer
            .apply(
                Some(r#"{"type":"response.something_new","payload":1}"#),
                &mut events,
            )
            .expect("accepted");
        assert!(events.is_empty());
    }

    #[test]
    fn a_frame_without_a_type_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(Some(r#"{"delta":"x"}"#), &mut events)
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
    }

    #[test]
    fn arguments_for_an_unknown_call_are_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"type":"response.function_call_arguments.delta","item_id":"missing","delta":"{}"}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::ProtocolViolation);
        assert!(err.message().contains("missing"));
    }

    #[test]
    fn a_duplicate_call_declaration_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let frame = r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"c","name":"t","arguments":""}}"#;
        reducer.apply(Some(frame), &mut events).expect("first");
        let err = reducer
            .apply(Some(frame), &mut events)
            .expect_err("rejected");
        assert!(err.message().contains("declared twice"));
    }

    #[test]
    fn a_call_without_a_name_is_rejected() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"c"}}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert!(err.message().contains("no name"));
    }

    #[test]
    fn exceeding_the_argument_bound_is_rejected() {
        let mut reducer = Reducer::new().with_limits(Limit {
            max_tool_arguments_bytes: 4,
            ..Limit::default()
        });
        let mut events = Vec::new();
        reducer
            .apply(
                Some(r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"c","name":"t","arguments":""}}"#),
                &mut events,
            )
            .expect("start");
        let err = reducer
            .apply(
                Some(r#"{"type":"response.function_call_arguments.delta","item_id":"c","delta":"far too long"}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn done_is_accepted_and_ignored() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        reducer
            .apply(Some("[DONE]"), &mut events)
            .expect("accepted");
        assert!(events.is_empty());
    }
}
