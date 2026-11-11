//! The Anthropic Messages dialect.
//!
//! A first-class dialect rather than something reached through a shim or a
//! gateway account, so a plain API key is enough to use Anthropic models. The
//! message model differs from the OpenAI shapes: system content is a top-level
//! field, tool calls and results are content blocks, and a response may carry
//! thinking blocks that must be replayed with the assistant turn.

use rune_core::config::Effort;
use rune_core::error::{Result, RuneError};
use rune_core::id::ToolCallId;

use crate::compat::{CallIdShape, ClaudeThinking, shape_call_id};
use crate::message::{ContentPart, Message, Role};
use crate::provider::{MarkerPolicy, Provider, RequestPlan, ToolChoice};
use crate::stream::{
    FinishReason, Limit, StreamReducer, Usage, incomplete_stream, protocol_violation,
    stream_failure,
};

/// Dialect name.
pub const NAME: &str = "anthropic";

/// Path appended to the endpoint base URL.
pub const PATH: &str = "/v1/messages";

/// Default output ceiling without extended thinking.
///
/// The field is required, so a plan without one still produces a valid request.
/// With extended thinking, the fallback also includes the thinking budget so
/// this many tokens remain available for the reply.
pub const DEFAULT_MAX_TOKENS: u64 = 8192;

/// Output ceiling for an adaptive-thinking model when the plan names none.
///
/// Thinking shares the ceiling with the reply, so it needs more room than
/// `DEFAULT_MAX_TOKENS`.
pub const ADAPTIVE_MAX_TOKENS: u64 = 32_768;

/// The dialect.
#[derive(Clone, Copy, Debug, Default)]
pub struct Anthropic;

impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            efforts: crate::provider::ALL_EFFORTS,
            images: true,
            tool_streaming: crate::provider::ToolStreaming::Incremental,
            fast_mode: false,
            requires_output_ceiling: true,
        }
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

    fn replay_is_signed(&self, replay: &str) -> bool {
        let Ok(serde_json::Value::Array(blocks)) = serde_json::from_str(replay) else {
            return false;
        };
        blocks.iter().any(
            |block| match block.get("type").and_then(serde_json::Value::as_str) {
                Some("redacted_thinking") => true,
                Some("thinking") => block
                    .get("signature")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|signature| !signature.is_empty()),
                _ => false,
            },
        )
    }

    fn build_request(&self, plan: &RequestPlan) -> Result<serde_json::Value> {
        let mut messages: Vec<serde_json::Value> = Vec::new();
        // Blocks the last plan message adds, which a shared-prefix request leaves unmarked.
        let mut newest_blocks = 0;
        for message in &plan.messages {
            let encoded = encode_message(message, plan)?;
            newest_blocks = encoded
                .get("content")
                .and_then(serde_json::Value::as_array)
                .map_or(usize::MAX, Vec::len);
            merge_adjacent(&mut messages, encoded);
        }

        let adaptive = matches!(
            plan.traits.claude_thinking,
            ClaudeThinking::Adaptive
                | ClaudeThinking::AdaptiveAlwaysOn
                | ClaudeThinking::AdaptiveBetweenTools
        );
        let requested_budget = if adaptive {
            None
        } else {
            thinking_budget(plan.effort)
        };
        let max_tokens = plan.max_output_tokens.unwrap_or(if adaptive {
            ADAPTIVE_MAX_TOKENS
        } else {
            DEFAULT_MAX_TOKENS.saturating_add(requested_budget.unwrap_or_default())
        });
        let mut body = serde_json::Map::new();
        body.insert("model".to_owned(), serde_json::json!(plan.model));
        body.insert("max_tokens".to_owned(), serde_json::json!(max_tokens));
        body.insert("stream".to_owned(), serde_json::json!(true));
        body.insert("messages".to_owned(), serde_json::Value::Array(messages));

        if adaptive {
            let (thinking, effort) = adaptive_controls(plan);
            if let Some(thinking) = thinking {
                body.insert("thinking".to_owned(), thinking);
            }
            if let Some(effort) = effort {
                body.insert(
                    "output_config".to_owned(),
                    serde_json::json!({ "effort": effort }),
                );
            }
        } else if let Some(budget) = requested_budget {
            // Manual thinking needs at least 1024 tokens and a budget strictly
            // below the total output ceiling. Keep an explicit ceiling intact.
            if max_tokens <= 1024 {
                return Err(RuneError::invalid_field(
                    "max_output_tokens",
                    "extended thinking requires an output ceiling of at least 1025 tokens",
                ));
            }
            if plan.has_tools() && plan.tool_choice == ToolChoice::Required {
                return Err(RuneError::invalid_field(
                    "tool_choice",
                    "forced tool use is incompatible with extended thinking",
                ));
            }
            body.insert(
                "thinking".to_owned(),
                serde_json::json!({
                    "type": "enabled",
                    "budget_tokens": budget.min(max_tokens.saturating_sub(1)),
                }),
            );
        } else if plan.effort == Effort::None {
            body.insert(
                "thinking".to_owned(),
                serde_json::json!({ "type": "disabled" }),
            );
        }

        if !plan.instructions.is_empty() {
            body.insert(
                "system".to_owned(),
                serde_json::json!([{
                    "type": "text",
                    "text": plan.instructions,
                }]),
            );
            if plan.cache_markers != MarkerPolicy::None {
                body["system"][0]["cache_control"] = cache_control();
            }
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
            if plan.tool_choice != ToolChoice::Auto || !plan.parallel_tool_calls {
                // The endpoint spells a required tool call `any`, not the
                // `required` the OpenAI shapes use.
                let choice = match plan.tool_choice {
                    ToolChoice::Required if plan.traits.forced_tool_choice => "any",
                    // Models that refuse a forced choice get the closest one they accept.
                    ToolChoice::Required => "auto",
                    other => other.as_str(),
                };
                let mut choice = serde_json::json!({ "type": choice });
                if !plan.parallel_tool_calls && plan.tool_choice != ToolChoice::None {
                    choice["disable_parallel_tool_use"] = serde_json::json!(true);
                }
                body.insert("tool_choice".to_owned(), choice);
            }
        }

        mark_cache_breakpoints(&mut body, plan.cache_markers, newest_blocks);
        Ok(serde_json::Value::Object(body))
    }
}

/// Returns the `thinking` and `output_config.effort` values for a model with
/// adaptive thinking.
fn adaptive_controls(plan: &RequestPlan) -> (Option<serde_json::Value>, Option<&'static str>) {
    let effort = match plan.effort {
        Effort::Auto => return (None, None),
        Effort::None => {
            return match plan.traits.claude_thinking {
                // Thinking cannot be turned off, so ask for as little as possible.
                ClaudeThinking::AdaptiveAlwaysOn => (None, Some("low")),
                ClaudeThinking::AdaptiveBetweenTools => {
                    (Some(serde_json::json!({ "type": "between_tools" })), None)
                }
                _ => (Some(serde_json::json!({ "type": "disabled" })), None),
            };
        }
        Effort::Minimal | Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Xhigh if !plan.traits.efforts.contains(&Effort::Xhigh) => "high",
        Effort::Xhigh => "xhigh",
        Effort::Max => "max",
    };
    let mut thinking = serde_json::json!({ "type": "adaptive" });
    if plan.show_reasoning {
        thinking["display"] = serde_json::json!("summarized");
    }
    (Some(thinking), Some(effort))
}

/// Encodes one message into a role with content blocks.
fn encode_message(message: &Message, plan: &RequestPlan) -> Result<serde_json::Value> {
    match message.role {
        Role::System => Err(RuneError::invalid_field(
            "messages",
            "system content must be supplied as instructions, not as a message",
        )),
        Role::User => Ok(serde_json::json!({
            "role": "user",
            "content": encode_user_blocks(message, plan),
        })),
        Role::Assistant => Ok(serde_json::json!({
            "role": "assistant",
            "content": encode_assistant_blocks(message),
        })),
        // Tool results are user-role content blocks in this dialect.
        Role::Tool => Ok(serde_json::json!({
            "role": "user",
            "content": encode_tool_blocks(message, plan),
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
fn encode_user_blocks(message: &Message, plan: &RequestPlan) -> Vec<serde_json::Value> {
    let mut blocks = replay_blocks(message);
    for part in &message.parts {
        match part {
            ContentPart::Text { text } => blocks.push(serde_json::json!({
                "type": "text",
                "text": text,
            })),
            ContentPart::Image { image } => {
                let source = plan.image_data.get(&image.id).map_or_else(
                    || serde_json::json!({"type": "image_ref", "media_type": image.media_type, "id": image.id}),
                    |data| serde_json::json!({"type": "base64", "media_type": image.media_type, "data": data.as_ref()}),
                );
                blocks.push(serde_json::json!({"type": "image", "source": source}));
            }
            _ => {}
        }
    }
    blocks
}

/// Whether a block is a thinking or a redacted thinking block.
fn is_thinking_block(block: &serde_json::Value) -> bool {
    matches!(
        block.get("type").and_then(serde_json::Value::as_str),
        Some("thinking" | "redacted_thinking")
    )
}

/// Encodes assistant content blocks, including tool use and thinking.
fn encode_assistant_blocks(message: &Message) -> Vec<serde_json::Value> {
    let stored = replay_blocks(message);

    // A replay holding the whole turn is sent back as it was produced, so the
    // thinking blocks keep their place between the calls, as long as the
    // message still makes the same calls in the same order.
    if stored.iter().any(|block| !is_thinking_block(block)) {
        let stored_ids = stored
            .iter()
            .filter(|block| {
                block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use")
            })
            .map(|block| block.get("id").and_then(serde_json::Value::as_str));
        let message_ids = message.parts.iter().filter_map(|part| match part {
            ContentPart::ToolCall { id, .. } => Some(Some(id.as_str())),
            _ => None,
        });
        if stored_ids.eq(message_ids) {
            return stored;
        }
    }

    // Otherwise the stored thinking blocks must still be replayed with the
    // turn that produced them, so they come first in the block list.
    let mut blocks: Vec<serde_json::Value> = stored.into_iter().filter(is_thinking_block).collect();

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
                    "id": shape_call_id(id.as_str(), CallIdShape::ClaudeSafe),
                    "name": name,
                    "input": parsed,
                }));
            }
            _ => {}
        }
    }

    blocks
}

/// Encodes tool results as user content blocks.
///
/// An image that follows a result joins that result's content, which then
/// becomes a list of a text block and image blocks. An image whose data the
/// plan does not hold is left out.
fn encode_tool_blocks(message: &Message, plan: &RequestPlan) -> Vec<serde_json::Value> {
    let mut blocks: Vec<serde_json::Value> = Vec::new();
    for part in &message.parts {
        match part {
            ContentPart::ToolResult {
                id,
                content,
                is_error,
                ..
            } => blocks.push(serde_json::json!({
                "type": "tool_result",
                "tool_use_id": shape_call_id(id.as_str(), CallIdShape::ClaudeSafe),
                "content": content,
                "is_error": is_error,
            })),
            ContentPart::Image { image } => {
                let Some(data) = plan.image_data.get(&image.id) else {
                    continue;
                };
                let Some(content) = blocks.last_mut().and_then(|block| block.get_mut("content"))
                else {
                    continue;
                };
                if let serde_json::Value::String(text) = content {
                    *content = serde_json::json!([{ "type": "text", "text": text }]);
                }
                if let serde_json::Value::Array(items) = content {
                    items.push(serde_json::json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": image.media_type, "data": data.as_ref()},
                    }));
                }
            }
            _ => {}
        }
    }
    blocks
}

/// Merges a role's blocks into the previous message when the roles match.
///
/// The endpoint rejects two consecutive messages with the same role, which a
/// tool turn produces naturally because tool results are user content.
/// The marker that lets the endpoint cache the request up to the block holding it.
fn cache_control() -> serde_json::Value {
    serde_json::json!({ "type": "ephemeral" })
}

/// Marks the tool list and the newest message as cacheable prefixes.
///
/// This endpoint caches only up to an explicit marker, so without one every
/// request paid full price for the tools, the instructions, and the whole
/// conversation. The prefix is cached in the order tools, system, messages:
/// the last tool covers the tools, the system block covers the instructions,
/// and the newest message's last block covers the conversation so far, which
/// the next request reads back before adding its own. That is three of the
/// four markers a request may carry. A thinking block or an empty text block
/// cannot hold one, so the newest block that can is used.
///
/// Under `SharedPrefix` the conversation marker skips the `newest_blocks`
/// blocks the last plan message added, and under `None` nothing is marked.
fn mark_cache_breakpoints(
    body: &mut serde_json::Map<String, serde_json::Value>,
    policy: MarkerPolicy,
    newest_blocks: usize,
) {
    if policy == MarkerPolicy::None {
        return;
    }
    let skip = if policy == MarkerPolicy::SharedPrefix {
        newest_blocks
    } else {
        0
    };
    if let Some(last) = body
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
        .and_then(|tools| tools.last_mut())
        .and_then(serde_json::Value::as_object_mut)
    {
        last.insert("cache_control".to_owned(), cache_control());
    }
    let block = if skip == 0 {
        body.get_mut("messages")
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|messages| messages.last_mut())
            .and_then(|message| message.get_mut("content"))
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|blocks| blocks.iter_mut().rev().find(|block| can_hold_marker(block)))
    } else {
        body.get_mut("messages")
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|messages| {
                messages
                    .iter_mut()
                    .rev()
                    .filter_map(|message| message.get_mut("content"))
                    .filter_map(serde_json::Value::as_array_mut)
                    .flat_map(|blocks| blocks.iter_mut().rev())
                    .skip(skip)
                    .find(|block| can_hold_marker(block))
            })
    }
    .and_then(serde_json::Value::as_object_mut);
    if let Some(block) = block {
        block.insert("cache_control".to_owned(), cache_control());
    }
}

/// Returns true for a content block the endpoint accepts a cache marker on.
fn can_hold_marker(block: &serde_json::Value) -> bool {
    match block.get("type").and_then(serde_json::Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| !text.is_empty()),
        Some("image" | "tool_use" | "tool_result" | "document") => true,
        _ => false,
    }
}

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

/// Maps a reasoning effort onto the dialect's manual thinking budget.
///
/// Models supporting manual extended thinking take a token budget rather than
/// a label. Request serialization bounds it by any explicit output ceiling.
#[must_use]
pub fn thinking_budget(effort: Effort) -> Option<u64> {
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
    signature: String,
    raw: Option<serde_json::Value>,
    ended: bool,
}

/// Reduces an Anthropic stream into normalized events.
#[derive(Debug)]
pub struct Reducer {
    limits: Limit,
    blocks: std::collections::BTreeMap<u64, PartialBlock>,
    /// Finished blocks in the order they stopped, kept for replay.
    ordered: Vec<serde_json::Value>,
    content: String,
    /// Bytes of thinking text and signatures, bounded with the content.
    reasoning_bytes: usize,
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
            ordered: Vec::new(),
            content: String::new(),
            reasoning_bytes: 0,
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
                    self.stop_reason = Some(map_stop_reason(reason));
                }
                Ok(())
            }
            "message_stop" => {
                self.finished = true;
                Ok(())
            }
            "error" => Err(stream_failure(
                value.get("error").unwrap_or(&serde_json::Value::Null),
                "the provider reported an error",
            )),
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
                let field = |name: &str| {
                    block
                        .get(name)
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                partial.text = field("thinking");
                partial.signature = field("signature");
                self.add_reasoning_bytes(
                    partial.text.len().saturating_add(partial.signature.len()),
                )?;
                if !partial.text.is_empty() {
                    out.push(crate::stream::ProviderEvent::ReasoningDelta {
                        delta: partial.text.clone(),
                    });
                }
            }
            "redacted_thinking" => {
                // Passed back byte for byte.
                partial.raw = Some(block.clone());
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
                    let total = self.content.len().saturating_add(self.reasoning_bytes);
                    if total > self.limits.max_content_bytes {
                        return Err(RuneError::too_large(
                            "stream.content",
                            total,
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
                    block.text.push_str(text);
                    self.add_reasoning_bytes(text.len())?;
                    out.push(crate::stream::ProviderEvent::ReasoningDelta {
                        delta: text.to_owned(),
                    });
                }
            }
            Some("signature_delta") => {
                if let Some(signature) = delta.get("signature").and_then(serde_json::Value::as_str)
                {
                    block.signature.push_str(signature);
                    self.add_reasoning_bytes(signature.len())?;
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
        if block.ended {
            return Ok(());
        }
        block.ended = true;
        match block.kind.as_deref() {
            Some("thinking") => {
                // A block with neither text nor signature carries nothing to replay.
                if !block.text.is_empty() || !block.signature.is_empty() {
                    self.ordered.push(serde_json::json!({
                        "type": "thinking",
                        "thinking": block.text,
                        "signature": block.signature,
                    }));
                }
            }
            Some("redacted_thinking") => {
                if let Some(raw) = block.raw.take() {
                    self.ordered.push(raw);
                }
            }
            Some("text") => {
                if !block.text.is_empty() {
                    self.ordered
                        .push(serde_json::json!({ "type": "text", "text": block.text }));
                }
            }
            Some("tool_use") => {
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
                let input: serde_json::Value =
                    serde_json::from_str(&arguments).unwrap_or_else(|_| serde_json::json!({}));
                self.ordered.push(serde_json::json!({
                    "type": "tool_use",
                    "id": id.as_str(),
                    "name": block.name.as_deref().unwrap_or_default(),
                    "input": input,
                }));
                out.push(crate::stream::ProviderEvent::ToolCallEnd { id, arguments });
            }
            _ => {}
        }
        Ok(())
    }

    /// Counts thinking text or a signature against the content bound.
    fn add_reasoning_bytes(&mut self, bytes: usize) -> Result<()> {
        self.reasoning_bytes = self.reasoning_bytes.saturating_add(bytes);
        let total = self.content.len().saturating_add(self.reasoning_bytes);
        if total > self.limits.max_content_bytes {
            return Err(RuneError::too_large(
                "stream.content",
                total,
                self.limits.max_content_bytes,
            ));
        }
        Ok(())
    }

    /// Records usage, keeping the largest report of each count.
    ///
    /// The endpoint reports running totals, so the largest value is the latest
    /// and adding them would double count. A later frame may omit the cache
    /// counts its input total was built from, and the maximum keeps the total
    /// that included them.
    fn record_usage(
        &mut self,
        usage: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) {
        let parsed = parse_usage(usage);
        self.usage = self.usage.merge_max(parsed);
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
        if !self.ordered.iter().any(is_thinking_block) {
            return None;
        }
        serde_json::to_string(&self.ordered).ok()
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
///
/// This endpoint's `input_tokens` excludes tokens read from or written to the
/// cache, while every other dialect counts them in. They are added back here,
/// so the input count means the whole prompt everywhere and the cache counts
/// are parts of it, which is what the context meter and the ledger assume.
fn parse_usage(value: &serde_json::Value) -> Usage {
    let field = |name: &str| value.get(name).and_then(serde_json::Value::as_u64);
    let cache_read_tokens = field("cache_read_input_tokens");
    let cache_write_tokens = field("cache_creation_input_tokens");
    Usage {
        input_tokens: field("input_tokens").map(|uncached| {
            uncached
                .saturating_add(cache_read_tokens.unwrap_or_default())
                .saturating_add(cache_write_tokens.unwrap_or_default())
        }),
        output_tokens: field("output_tokens"),
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens: None,
    }
}

/// Maps a stop reason onto the normalized one.
///
/// The reason arrives after the answer has streamed in full, so a reason this
/// build does not know is taken as a natural stop rather than a failure that
/// would discard the answer.
fn map_stop_reason(raw: &str) -> FinishReason {
    match raw {
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" | "model_context_window_exceeded" => FinishReason::MaxTokens,
        "refusal" => FinishReason::Refused,
        _ => FinishReason::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolSpec;
    use crate::stream::ProviderEvent;
    use rune_core::config::Effort;
    use rune_core::error::ErrorCode;

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
        assert_eq!(body["system"][0]["text"], "be helpful");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn the_tools_the_instructions_and_the_newest_turn_are_marked_cacheable() {
        let mut plan = plan_with_user("first");
        plan.messages
            .push(Message::assistant(vec![ContentPart::Text {
                text: "answer".to_owned(),
            }]));
        plan.messages.push(Message::user("second"));
        plan.tools = vec![
            ToolSpec {
                name: "a".to_owned(),
                description: "first tool".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            ToolSpec {
                name: "b".to_owned(),
                description: "second tool".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        ];
        let body = Anthropic.build_request(&plan).expect("build");
        let ephemeral = serde_json::json!({"type": "ephemeral"});

        assert_eq!(body["tools"][1]["cache_control"], ephemeral);
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["system"][0]["cache_control"], ephemeral);
        // Only the newest message carries a marker; the endpoint reads the
        // earlier prefix back from the previous request's marker.
        assert_eq!(
            body["messages"][2]["content"][0]["cache_control"],
            ephemeral
        );
        for earlier in 0..2 {
            assert!(
                body["messages"][earlier]["content"][0]
                    .get("cache_control")
                    .is_none()
            );
        }
        let markers = body.to_string().matches("cache_control").count();
        assert!(markers <= 4, "the endpoint accepts at most four markers");
    }

    #[test]
    fn a_shared_prefix_request_marks_the_previous_tail() {
        let ephemeral = serde_json::json!({"type": "ephemeral"});
        let mut plan = plan_with_user("first");
        plan.messages
            .push(Message::assistant(vec![ContentPart::Text {
                text: "answer".to_owned(),
            }]));
        plan.messages.push(Message::user("one-off"));
        plan.tools = vec![ToolSpec {
            name: "a".to_owned(),
            description: "a tool".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        plan.cache_markers = MarkerPolicy::SharedPrefix;
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(body["tools"][0]["cache_control"], ephemeral);
        assert_eq!(body["system"][0]["cache_control"], ephemeral);
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"],
            ephemeral
        );
        assert!(
            body["messages"][2]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );

        // An instruction after a tool result merges into its user message;
        // the marker stays on the result.
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![
            Message::user("start"),
            Message::assistant(vec![ContentPart::ToolCall {
                id: ToolCallId::new("toolu_1").expect("id"),
                name: "read_file".to_owned(),
                arguments: "{}".to_owned(),
            }]),
            Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult {
                    id: ToolCallId::new("toolu_1").expect("id"),
                    name: "read_file".to_owned(),
                    content: "contents".to_owned(),
                    is_error: false,
                }],
                replay: None,
                origin: None,
            },
            Message::user("one-off"),
        ];
        plan.cache_markers = MarkerPolicy::SharedPrefix;
        let body = Anthropic.build_request(&plan).expect("build");
        let last = &body["messages"][2]["content"];
        assert_eq!(last[0]["type"], "tool_result");
        assert_eq!(last[0]["cache_control"], ephemeral);
        assert!(last[1].get("cache_control").is_none());
        assert_eq!(body.to_string().matches("cache_control").count(), 1);
    }

    #[test]
    fn a_request_without_markers_has_none() {
        let mut plan = plan_with_user("first");
        plan.tools = vec![ToolSpec {
            name: "a".to_owned(),
            description: "a tool".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        plan.cache_markers = MarkerPolicy::None;
        let body = Anthropic.build_request(&plan).expect("build");
        assert!(!body.to_string().contains("cache_control"));
    }

    #[test]
    fn a_marker_skips_blocks_that_cannot_hold_one() {
        let mut body = serde_json::Map::new();
        body.insert(
            "messages".to_owned(),
            serde_json::json!([{"role": "assistant", "content": [
                {"type": "text", "text": "kept"},
                {"type": "thinking", "thinking": "private", "signature": "s"},
                {"type": "text", "text": ""},
            ]}]),
        );
        mark_cache_breakpoints(&mut body, MarkerPolicy::Full, 0);
        let blocks = &body["messages"][0]["content"];
        assert!(blocks[0].get("cache_control").is_some());
        assert!(blocks[1].get("cache_control").is_none());
        assert!(blocks[2].get("cache_control").is_none());
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
    fn auto_and_high_requests_have_distinct_thinking_settings() {
        let mut plan = plan_with_user("hi");
        let auto = Anthropic.build_request(&plan).expect("auto request");
        plan.effort = Effort::High;
        let high = Anthropic.build_request(&plan).expect("high request");

        assert_ne!(auto, high);
        assert!(auto.get("thinking").is_none());
        assert_eq!(auto["max_tokens"], 8192);
        assert_eq!(
            high["thinking"],
            serde_json::json!({ "type": "enabled", "budget_tokens": 16_384 })
        );
        assert_eq!(high["max_tokens"], 24_576);
        assert_eq!(auto["messages"], high["messages"]);
        assert_eq!(auto["system"], high["system"]);
    }

    #[test]
    fn none_effort_explicitly_disables_thinking() {
        let mut plan = plan_with_user("hi");
        plan.effort = Effort::None;
        plan.max_output_tokens = Some(1024);
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(body["thinking"], serde_json::json!({ "type": "disabled" }));
        assert_eq!(body["max_tokens"], 1024);
    }

    #[test]
    fn positive_efforts_serialize_manual_thinking_budgets() {
        for (effort, budget) in [
            (Effort::Minimal, 1024),
            (Effort::Low, 2048),
            (Effort::Medium, 8192),
            (Effort::High, 16_384),
            (Effort::Xhigh, 32_768),
            (Effort::Max, 65_536),
        ] {
            let mut plan = plan_with_user("hi");
            plan.effort = effort;
            let body = Anthropic.build_request(&plan).expect("build");
            assert_eq!(
                body["thinking"],
                serde_json::json!({ "type": "enabled", "budget_tokens": budget }),
                "{effort}"
            );
            assert_eq!(body["max_tokens"], budget + 8192, "{effort}");
        }
    }

    #[test]
    fn thinking_respects_explicit_output_ceilings() {
        let mut plan = plan_with_user("hi");
        plan.effort = Effort::High;
        for (ceiling, budget) in [
            (1025, 1024),
            (4096, 4095),
            (16_384, 16_383),
            (32_768, 16_384),
            (u64::MAX, 16_384),
        ] {
            plan.max_output_tokens = Some(ceiling);
            let body = Anthropic.build_request(&plan).expect("build");
            assert_eq!(body["max_tokens"], ceiling);
            assert_eq!(body["thinking"]["budget_tokens"], budget);
        }

        for ceiling in [0, 1023, 1024] {
            plan.max_output_tokens = Some(ceiling);
            let err = Anthropic.build_request(&plan).expect_err("too little room");
            assert_eq!(err.code(), ErrorCode::InvalidField);
            assert_eq!(err.detail().field.as_deref(), Some("max_output_tokens"));
        }
    }

    #[test]
    fn manual_thinking_rejects_forced_tool_use() {
        let mut plan = plan_with_user("hi");
        plan.effort = Effort::High;
        plan.tools = vec![ToolSpec {
            name: "t".to_owned(),
            description: "d".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];

        // Both supported tool choices still serialize alongside thinking.
        for choice in [ToolChoice::Auto, ToolChoice::None] {
            plan.tool_choice = choice;
            let body = Anthropic.build_request(&plan).expect("build");
            assert_eq!(body["thinking"]["type"], "enabled");
            assert_eq!(body["tools"][0]["name"], "t");
        }

        plan.tool_choice = ToolChoice::Required;
        let err = Anthropic
            .build_request(&plan)
            .expect_err("incompatible choice");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.detail().field.as_deref(), Some("tool_choice"));

        // Forced tool use still works when extended thinking is not enabled.
        for effort in [Effort::Auto, Effort::None] {
            plan.effort = effort;
            let body = Anthropic.build_request(&plan).expect("build");
            assert_eq!(body["tool_choice"], serde_json::json!({ "type": "any" }));
        }
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
    fn one_call_mode_disables_parallel_tool_use() {
        let mut plan = plan_with_user("hi");
        plan.tools = vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];
        plan.parallel_tool_calls = false;
        for (choice, wire) in [(ToolChoice::Auto, "auto"), (ToolChoice::Required, "any")] {
            plan.tool_choice = choice;
            let body = Anthropic.build_request(&plan).expect("build");
            assert_eq!(
                body["tool_choice"],
                serde_json::json!({ "type": wire, "disable_parallel_tool_use": true })
            );
            assert!(body.get("parallel_tool_calls").is_none());
        }

        plan.tool_choice = ToolChoice::None;
        let body = Anthropic
            .build_request(&plan)
            .expect("build without tool use");
        assert_eq!(body["tool_choice"], serde_json::json!({ "type": "none" }));

        plan.tools.clear();
        let body = Anthropic.build_request(&plan).expect("build without tools");
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
        assert_eq!(body["tool_choice"], serde_json::json!({ "type": "any" }));

        plan.tool_choice = ToolChoice::None;
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(body["tool_choice"], serde_json::json!({ "type": "none" }));
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
            origin: None,
        }];
        let body = Anthropic.build_request(&plan).expect("build");
        let message = &body["messages"][0];
        assert_eq!(message["role"], "user");
        assert_eq!(message["content"][0]["type"], "tool_result");
        assert_eq!(message["content"][0]["tool_use_id"], "toolu_1");
    }

    fn tool_turn(parts: Vec<ContentPart>) -> RequestPlan {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message {
            role: Role::Tool,
            parts,
            replay: None,
            origin: None,
        }];
        plan
    }

    fn result(id: &str, content: &str) -> ContentPart {
        ContentPart::ToolResult {
            id: ToolCallId::new(id).expect("id"),
            name: "read_file".to_owned(),
            content: content.to_owned(),
            is_error: false,
        }
    }

    fn image(id: u64) -> ContentPart {
        ContentPart::Image {
            image: crate::message::ImageRef {
                id,
                media_type: "image/png".to_owned(),
                encoded_bytes: 4,
            },
        }
    }

    #[test]
    fn a_tool_image_is_sent_inside_its_tool_result() {
        let mut plan = tool_turn(vec![
            result("toolu_1", "image shot.png attached"),
            image(1),
            result("toolu_2", "contents"),
        ]);
        plan.image_data.insert(1, "QUJD".into());
        let body = Anthropic.build_request(&plan).expect("build");
        let blocks = &body["messages"][0]["content"];
        assert_eq!(
            blocks[0]["content"],
            serde_json::json!([
                {"type": "text", "text": "image shot.png attached"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}},
            ])
        );
        assert_eq!(blocks[1]["content"], "contents");
    }

    #[test]
    fn a_tool_turn_without_images_is_encoded_as_before() {
        let plan = tool_turn(vec![result("toolu_1", "contents")]);
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(
            body["messages"],
            serde_json::json!([{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": "contents",
                    "is_error": false,
                    "cache_control": {"type": "ephemeral"},
                }],
            }])
        );
    }

    #[test]
    fn a_missing_image_is_left_out() {
        let plan = tool_turn(vec![result("toolu_1", "image shot.png attached"), image(9)]);
        let body = Anthropic.build_request(&plan).expect("build");
        let blocks = body["messages"][0]["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["content"], "image shot.png attached");
        assert!(!body.to_string().contains("image_ref"));
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
                origin: None,
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
    fn an_assistant_message_with_no_blocks_is_not_sent_as_an_empty_text() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![
            Message::user("one"),
            Message::assistant(vec![]),
            Message::user("two"),
        ];
        let sent = crate::normalize::for_request(&plan, NAME);
        let body = Anthropic.build_request(&sent).expect("build");
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert!(
            messages[0]["content"]
                .as_array()
                .expect("blocks")
                .iter()
                .all(|block| block["text"] != "")
        );
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

    /// Returns a plan for a Claude model with the traits the resolver gives it.
    fn claude_plan(model: &str, effort: Effort) -> RequestPlan {
        let mut plan = plan_with_user("hi");
        plan.model = model.to_owned();
        plan.traits = crate::compat::resolve("anthropic", None, model).1;
        plan.effort = effort;
        plan
    }

    fn with_tool(mut plan: RequestPlan, choice: ToolChoice) -> RequestPlan {
        plan.tools = vec![ToolSpec {
            name: "read".to_owned(),
            description: "read a file".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        plan.tool_choice = choice;
        plan
    }

    #[test]
    fn every_claude_row_maps_each_effort() {
        let adaptive = serde_json::json!({"type": "adaptive"});
        let disabled = serde_json::json!({"type": "disabled"});
        let between = serde_json::json!({"type": "between_tools"});
        let absent = serde_json::Value::Null;
        let effort = |level: &str| serde_json::json!({ "effort": level });
        // (model, effort, thinking, output_config)
        let rows = [
            ("claude-opus-5-5", Effort::Auto, &absent, absent.clone()),
            ("claude-opus-5-5", Effort::None, &absent, effort("low")),
            ("claude-opus-5-5", Effort::Minimal, &adaptive, effort("low")),
            ("claude-opus-5-5", Effort::Low, &adaptive, effort("low")),
            (
                "claude-opus-5-5",
                Effort::Medium,
                &adaptive,
                effort("medium"),
            ),
            ("claude-opus-5-5", Effort::High, &adaptive, effort("high")),
            ("claude-opus-5-5", Effort::Xhigh, &adaptive, effort("xhigh")),
            ("claude-opus-5-5", Effort::Max, &adaptive, effort("max")),
            ("claude-fable-5-1", Effort::None, &absent, effort("low")),
            ("claude-sonnet-5-5", Effort::Auto, &absent, absent.clone()),
            ("claude-sonnet-5-5", Effort::None, &between, absent.clone()),
            (
                "claude-sonnet-5-5",
                Effort::Minimal,
                &adaptive,
                effort("low"),
            ),
            ("claude-sonnet-5-5", Effort::Low, &adaptive, effort("low")),
            (
                "claude-sonnet-5-5",
                Effort::Medium,
                &adaptive,
                effort("medium"),
            ),
            ("claude-sonnet-5-5", Effort::High, &adaptive, effort("high")),
            (
                "claude-sonnet-5-5",
                Effort::Xhigh,
                &adaptive,
                effort("xhigh"),
            ),
            ("claude-sonnet-5-5", Effort::Max, &adaptive, effort("max")),
            ("claude-opus-4-7", Effort::Auto, &absent, absent.clone()),
            ("claude-opus-4-7", Effort::None, &disabled, absent.clone()),
            ("claude-opus-4-7", Effort::Minimal, &adaptive, effort("low")),
            ("claude-opus-4-7", Effort::Low, &adaptive, effort("low")),
            (
                "claude-opus-4-7",
                Effort::Medium,
                &adaptive,
                effort("medium"),
            ),
            ("claude-opus-4-7", Effort::High, &adaptive, effort("high")),
            ("claude-opus-4-7", Effort::Xhigh, &adaptive, effort("xhigh")),
            ("claude-opus-4-7", Effort::Max, &adaptive, effort("max")),
            ("claude-haiku-5-5", Effort::None, &disabled, absent.clone()),
            ("claude-opus-4-6", Effort::Xhigh, &adaptive, effort("high")),
            (
                "claude-sonnet-4-6",
                Effort::Xhigh,
                &adaptive,
                effort("high"),
            ),
            ("claude-sonnet-4-6", Effort::Max, &adaptive, effort("max")),
        ];
        for (model, level, thinking, output_config) in rows {
            let body = Anthropic
                .build_request(&claude_plan(model, level))
                .expect("build");
            assert_eq!(body["thinking"], *thinking, "{model} {level:?}");
            assert_eq!(body["output_config"], output_config, "{model} {level:?}");
        }
    }

    #[test]
    fn an_older_claude_keeps_its_thinking_budget() {
        let body = Anthropic
            .build_request(&claude_plan("claude-haiku-4-5", Effort::High))
            .expect("build");
        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "enabled", "budget_tokens": 16_384})
        );
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS + 16_384);
        assert!(body.get("output_config").is_none());
        let opus = Anthropic
            .build_request(&claude_plan("claude-opus-4-5-20251101", Effort::Low))
            .expect("build");
        assert_eq!(opus["thinking"]["budget_tokens"], 2048);
    }

    #[test]
    fn forced_tool_choice_degrades_where_refused() {
        for model in ["claude-fable-5-1", "claude-opus-5-5", "claude-sonnet-5-5"] {
            let body = Anthropic
                .build_request(&with_tool(
                    claude_plan(model, Effort::High),
                    ToolChoice::Required,
                ))
                .expect("build");
            assert_eq!(
                body["tool_choice"],
                serde_json::json!({"type": "auto"}),
                "{model}"
            );
            let mut single = with_tool(claude_plan(model, Effort::High), ToolChoice::Required);
            single.parallel_tool_calls = false;
            let body = Anthropic.build_request(&single).expect("build");
            assert_eq!(
                body["tool_choice"],
                serde_json::json!({"type": "auto", "disable_parallel_tool_use": true}),
                "{model}"
            );
        }
        for model in ["claude-opus-4-7", "claude-fable-5"] {
            let body = Anthropic
                .build_request(&with_tool(
                    claude_plan(model, Effort::High),
                    ToolChoice::Required,
                ))
                .expect("build");
            assert_eq!(
                body["tool_choice"],
                serde_json::json!({"type": "any"}),
                "{model}"
            );
        }
    }

    #[test]
    fn tool_choice_none_is_sent_with_adaptive_thinking() {
        for model in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-opus-4-7",
            "claude-fable-5-1",
        ] {
            let body = Anthropic
                .build_request(&with_tool(
                    claude_plan(model, Effort::High),
                    ToolChoice::None,
                ))
                .expect("build");
            assert_eq!(
                body["tool_choice"],
                serde_json::json!({"type": "none"}),
                "{model}"
            );
            assert_eq!(body["thinking"]["type"], "adaptive", "{model}");
        }
    }

    #[test]
    fn a_session_asks_for_summarized_thinking() {
        let mut plan = claude_plan("claude-opus-5-5", Effort::High);
        plan.show_reasoning = true;
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "adaptive", "display": "summarized"})
        );
        let quiet = Anthropic
            .build_request(&claude_plan("claude-opus-5-5", Effort::High))
            .expect("build");
        assert_eq!(quiet["thinking"], serde_json::json!({"type": "adaptive"}));
        // `between_tools` accepts no other field.
        let mut off = claude_plan("claude-sonnet-5-5", Effort::None);
        off.show_reasoning = true;
        let body = Anthropic.build_request(&off).expect("build");
        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "between_tools"})
        );
    }

    #[test]
    fn no_sampling_parameter_reaches_claude() {
        for model in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-fable-5-1",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-haiku-4-5",
            "claude-3-5-sonnet-20241022",
            "claude",
        ] {
            for &level in crate::provider::ALL_EFFORTS {
                // A user temperature that reached the plan is still not sent.
                let mut plan = claude_plan(model, level);
                plan.sampling.temperature = Some(0.2);
                let body = Anthropic.build_request(&plan).expect("build");
                for field in ["temperature", "top_p", "top_k"] {
                    assert!(body.get(field).is_none(), "{model} {level:?} {field}");
                }
            }
        }
    }

    #[test]
    fn an_adaptive_model_without_a_known_ceiling_gets_room_to_think() {
        for model in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-opus-4-7"] {
            for &level in crate::provider::ALL_EFFORTS {
                let body = Anthropic
                    .build_request(&claude_plan(model, level))
                    .expect("build");
                assert_eq!(body["max_tokens"], ADAPTIVE_MAX_TOKENS, "{model} {level:?}");
            }
        }
        let mut known = claude_plan("claude-opus-5-5", Effort::Max);
        known.max_output_tokens = Some(4096);
        let body = Anthropic.build_request(&known).expect("build");
        assert_eq!(body["max_tokens"], 4096);
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

    fn replayed(reducer: &Reducer) -> Vec<serde_json::Value> {
        let replay = reducer.replay().expect("replay");
        serde_json::from_str(&replay).expect("replay is a block array")
    }

    #[test]
    fn a_signature_delivered_as_a_delta_is_replayed() {
        let (_, reducer) = reduce(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"first "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"second"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBsig"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let blocks = replayed(&reducer);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "first second");
        assert_eq!(blocks[0]["signature"], "EqQBsig");
    }

    #[test]
    fn thinking_text_from_deltas_is_replayed() {
        let (events, reducer) = reduce(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weigh "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"both"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s1"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            // Omitted display: an empty text delta, then only the signature.
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"s2"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            // Neither text nor signature: nothing to replay.
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"done"}}"#,
            r#"{"type":"content_block_stop","index":3}"#,
            // Cut before its stop: stored nowhere.
            r#"{"type":"content_block_start","index":4,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":4,"delta":{"type":"thinking_delta","thinking":"partial"}}"#,
        ]);
        let reasoning: String = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ReasoningDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, "weigh bothpartial");
        assert_eq!(
            replayed(&reducer),
            vec![
                serde_json::json!({"type": "thinking", "thinking": "weigh both", "signature": "s1"}),
                serde_json::json!({"type": "thinking", "thinking": "", "signature": "s2"}),
                serde_json::json!({"type": "text", "text": "done"}),
            ]
        );
    }

    #[test]
    fn a_redacted_block_is_kept_in_place() {
        let redacted = r#"{"type":"redacted_thinking","data":"EmwKAhgBEgy3va3pzix/LafPsn4a"}"#;
        let start =
            format!(r#"{{"type":"content_block_start","index":1,"content_block":{redacted}}}"#);
        let (_, reducer) = reduce(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"a","signature":"s1"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            &start,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"thinking","thinking":"b","signature":"s2"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let blocks = replayed(&reducer);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["thinking"], "a");
        assert_eq!(
            blocks[1],
            serde_json::from_str::<serde_json::Value>(redacted).expect("redacted block")
        );
        assert_eq!(blocks[2]["thinking"], "b");

        let mut message = Message::assistant(vec![]);
        message.replay = reducer.replay();
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![message];
        let body = Anthropic.build_request(&plan).expect("build");
        assert_eq!(
            body["messages"][0]["content"][1]["data"],
            "EmwKAhgBEgy3va3pzix/LafPsn4a"
        );
    }

    fn call(id: &str) -> ContentPart {
        ContentPart::ToolCall {
            id: ToolCallId::new(id).expect("call id"),
            name: "read_file".to_owned(),
            arguments: r#"{"path":"a.rs"}"#.to_owned(),
        }
    }

    #[test]
    fn interleaved_thinking_keeps_its_order() {
        let (_, reducer) = reduce(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s1"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.rs\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"Reading the next file."}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"signature_delta","signature":"s2"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_2","name":"read_file","input":{}}}"#,
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.rs\"}"}}"#,
            r#"{"type":"content_block_stop","index":3}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let stored = replayed(&reducer);
        let kinds: Vec<&str> = stored
            .iter()
            .filter_map(|block| block["type"].as_str())
            .collect();
        assert_eq!(kinds, ["thinking", "tool_use", "thinking", "tool_use"]);

        let mut message = Message::assistant(vec![call("toolu_1"), call("toolu_2")]);
        message.replay = reducer.replay();
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![message];
        let body = Anthropic.build_request(&plan).expect("build");
        let content = body["messages"][0]["content"]
            .as_array()
            .expect("content blocks");
        assert_eq!(content.len(), 4);
        for (sent, kept) in content.iter().zip(&stored) {
            let mut sent = sent.clone();
            // The newest turn may carry a cache marker on its last block.
            if let Some(block) = sent.as_object_mut() {
                block.remove("cache_control");
            }
            assert_eq!(&sent, kept);
        }
    }

    #[test]
    fn an_older_thinking_only_replay_still_goes_first() {
        let thinking =
            serde_json::json!({"type": "thinking", "thinking": "outline", "signature": "s"});
        let mut message = Message::assistant(vec![
            ContentPart::Text {
                text: "reading".to_owned(),
            },
            call("toolu_1"),
        ]);
        message.replay = Some(serde_json::to_string(&vec![thinking.clone()]).expect("encode"));
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![message];
        let body = Anthropic.build_request(&plan).expect("build");
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0], thinking);
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[2]["type"], "tool_use");
        assert_eq!(content[2]["id"], "toolu_1");
    }

    #[test]
    fn a_history_with_changed_calls_falls_back_to_thinking_first() {
        let thinking =
            serde_json::json!({"type": "thinking", "thinking": "outline", "signature": "s"});
        let stored = vec![
            thinking.clone(),
            serde_json::json!({"type": "text", "text": "reading"}),
            serde_json::json!({"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {}}),
        ];
        let mut message = Message::assistant(vec![
            ContentPart::Text {
                text: "reading".to_owned(),
            },
            call("toolu_9"),
        ]);
        message.replay = Some(serde_json::to_string(&stored).expect("encode"));
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![message];
        let body = Anthropic.build_request(&plan).expect("build");
        let content = body["messages"][0]["content"]
            .as_array()
            .expect("content blocks");
        assert_eq!(content.len(), 3);
        assert_eq!(content[0], thinking);
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[2]["id"], "toolu_9");
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
    fn replay_is_signed_for_signed_or_redacted_thinking_only() {
        let signed =
            r#"[{"type":"thinking","thinking":"a","signature":"s1"},{"type":"text","text":"b"}]"#;
        let redacted = r#"[{"type":"redacted_thinking","data":"opaque"}]"#;
        let unsigned =
            r#"[{"type":"thinking","thinking":"a","signature":""},{"type":"text","text":"b"}]"#;
        assert!(Anthropic.replay_is_signed(signed));
        assert!(Anthropic.replay_is_signed(redacted));
        assert!(!Anthropic.replay_is_signed(unsigned));
        assert!(!Anthropic.replay_is_signed("not json"));
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
        // The endpoint's 10 excludes the cache; the normalized count is the
        // whole prompt, as every other dialect reports it.
        assert_eq!(usage.input_tokens, Some(18));
    }

    #[test]
    fn a_later_frame_without_cache_counts_keeps_the_full_input() {
        let (_, reducer) = reduce(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":4,"cache_read_input_tokens":90,"cache_creation_input_tokens":0,"output_tokens":1}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":4,"output_tokens":7}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let usage = reducer.usage();
        assert_eq!(usage.input_tokens, Some(94));
        assert_eq!(usage.output_tokens, Some(7));
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
    fn an_unknown_stop_reason_keeps_the_answer() {
        let (events, reducer) = reduce(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"done"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"sideways"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::Stop);
        assert!(events.contains(&ProviderEvent::TextDelta {
            delta: "done".to_owned()
        }));
    }

    #[test]
    fn a_full_context_window_stops_at_the_token_limit() {
        let (_, reducer) = reduce(&[
            r#"{"type":"message_delta","delta":{"stop_reason":"model_context_window_exceeded"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::MaxTokens);
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
    fn an_overloaded_or_failing_endpoint_is_retried() {
        for kind in ["overloaded_error", "api_error", "rate_limit_error"] {
            let mut reducer = Reducer::new();
            let mut events = Vec::new();
            let frame = format!(r#"{{"type":"error","error":{{"type":"{kind}","message":"m"}}}}"#);
            let err = reducer
                .apply(Some(&frame), &mut events)
                .expect_err("rejected");
            let err = crate::error::NetError::from(err);
            assert!(err.is_retryable(), "{kind} was not retried");
        }
    }

    #[test]
    fn an_invalid_request_reported_in_the_stream_is_not_retried() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(
                    r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#,
                ),
                &mut events,
            )
            .expect_err("rejected");
        let err = crate::error::NetError::from(err);
        assert!(!err.is_retryable());
        assert_eq!(err.message(), "bad");
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
