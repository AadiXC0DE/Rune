//! The OpenAI Chat Completions dialect.
//!
//! The widest interoperability surface, covering OpenAI-compatible endpoints
//! including local servers. Serialization and stream reduction live here; the
//! endpoint, credential, and HTTP client are supplied by the caller.

use rune_core::config::Effort;
use rune_core::error::{Result, RuneError};
use rune_core::id::ToolCallId;

use crate::compat::{
    CacheMarkers, EmptyContent, Family, ReasoningReplay, SchemaRules, ThinkingFormat, shape_call_id,
};
use crate::message::{ContentPart, Message, Role};
use crate::provider::{MarkerPolicy, Provider, RequestPlan};
use crate::stream::{
    FinishReason, Limit, StreamReducer, Usage, incomplete_stream, protocol_violation,
    stream_failure,
};

/// Dialect name.
pub const NAME: &str = "chat_completions";

/// Path appended to the endpoint base URL.
pub const PATH: &str = "/chat/completions";

/// Path listing the served models, a sibling of the completion path.
pub const MODELS_PATH: &str = "/models";

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

    fn routing(&self) -> crate::provider::Routing {
        crate::provider::Routing::Supported
    }

    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            efforts: crate::provider::ALL_EFFORTS,
            images: true,
            tool_streaming: crate::provider::ToolStreaming::Incremental,
            fast_mode: false,
            requires_output_ceiling: false,
        }
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

    fn reducer_for(&self, plan: &RequestPlan) -> Box<dyn StreamReducer> {
        Box::new(Reducer::new().with_done_marker(plan.compat.needs_done_marker))
    }

    fn build_request(&self, plan: &RequestPlan) -> Result<serde_json::Value> {
        let compat = &plan.compat;
        let mut messages: Vec<serde_json::Value> = Vec::new();

        // The system lane is emitted first and separately, because the
        // conversation model keeps it out of the message list.
        if !plan.instructions.is_empty() {
            messages.push(serde_json::json!({
                "role": compat.system_role.as_str(),
                "content": plan.instructions,
            }));
        }

        // Wire messages before the last plan message's, for a shared-prefix request.
        let mut shared = 0;
        for (index, message) in plan.messages.iter().enumerate() {
            shared = messages.len();
            if message.role == Role::Tool {
                // One tool message can carry a whole batch of results, because
                // the history groups them, so it expands to one wire message per
                // result rather than being collapsed to its first.
                messages.extend(encode_tool_messages(message, plan));
                if compat.assistant_after_tool_result
                    && plan
                        .messages
                        .get(index.saturating_add(1))
                        .is_some_and(|next| next.role == Role::User)
                {
                    messages.push(serde_json::json!({ "role": "assistant", "content": "Done." }));
                }
                continue;
            }
            messages.push(encode_message(message, plan)?);
        }

        let mut body = serde_json::Map::new();
        body.insert("model".to_owned(), serde_json::json!(plan.model));
        body.insert("stream".to_owned(), serde_json::json!(true));
        if Self::INCLUDE_USAGE && compat.stream_usage {
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
                    let mut function = serde_json::json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": crate::schema::sanitize(
                            &tool.input_schema,
                            compat.schema_rules,
                            compat.strict_schema,
                        ),
                    });
                    if compat.strict_schema
                        && let Some(function) = function.as_object_mut()
                    {
                        function.insert("strict".to_owned(), serde_json::json!(true));
                    }
                    Ok(serde_json::json!({
                        "type": "function",
                        "function": function,
                    }))
                })
                .collect();
            body.insert("tools".to_owned(), serde_json::Value::Array(tools?));
            body.insert(
                "tool_choice".to_owned(),
                serde_json::json!(plan.tool_choice.as_str()),
            );
            if compat.parallel_tool_calls_field {
                body.insert(
                    "parallel_tool_calls".to_owned(),
                    serde_json::json!(plan.parallel_tool_calls),
                );
            }
        } else if compat.tools_with_tool_history
            && plan.messages.iter().any(|message| {
                message
                    .parts
                    .iter()
                    .any(|part| matches!(part, ContentPart::ToolCall { .. }))
            })
        {
            body.insert("tools".to_owned(), serde_json::json!([]));
        }

        insert_thinking(&mut body, plan);
        let sampling = plan.sampling;
        if let Some(temperature) = sampling.temperature {
            body.insert("temperature".to_owned(), serde_json::json!(temperature));
        }
        if let Some(top_p) = sampling.top_p {
            body.insert("top_p".to_owned(), serde_json::json!(top_p));
        }
        // Only the families that serve `top_k` on compatible routes get it.
        if let Some(top_k) = sampling.top_k
            && matches!(plan.traits.family, Family::Gemini | Family::Qwen)
        {
            body.insert("top_k".to_owned(), serde_json::json!(top_k));
        }

        if let Some(max) = plan.max_output_tokens {
            body.insert(
                compat.max_tokens_field.as_str().to_owned(),
                serde_json::json!(max),
            );
        }

        if let Some(options) = crate::provider::provider_options(plan) {
            body.insert("provider".to_owned(), options);
        }

        if compat.prompt_cache_key {
            body.insert(
                "prompt_cache_key".to_owned(),
                serde_json::json!(plan.cache_key.clone().unwrap_or_else(|| {
                    crate::provider::prefix_cache_key(&plan.instructions, &plan.tools)
                })),
            );
        }

        if compat.cache_control == CacheMarkers::Anthropic
            && plan.cache_markers != MarkerPolicy::None
            && let Some(messages) = body
                .get_mut("messages")
                .and_then(serde_json::Value::as_array_mut)
        {
            let marked = if plan.cache_markers == MarkerPolicy::SharedPrefix {
                &mut messages[..shared]
            } else {
                &mut messages[..]
            };
            mark_cache_breakpoints(marked, !plan.instructions.is_empty());
            if let [stable, rest] = plan.system_blocks()[..] {
                let marker = serde_json::json!({ "type": "ephemeral" });
                messages[0]["content"] = serde_json::json!([
                    { "type": "text", "text": stable, "cache_control": marker },
                    { "type": "text", "text": rest, "cache_control": marker },
                ]);
            }
        }

        Ok(serde_json::Value::Object(body))
    }
}

/// Marks the system message and the newest user or assistant message that has
/// text as prompt cache breakpoints. Tool messages are never marked.
fn mark_cache_breakpoints(messages: &mut [serde_json::Value], has_system: bool) {
    let is_text = |part: &serde_json::Value| {
        part.get("type").and_then(serde_json::Value::as_str) == Some("text")
    };
    let has_text = |message: &serde_json::Value| match message.get("content") {
        Some(serde_json::Value::String(text)) => !text.is_empty(),
        Some(serde_json::Value::Array(parts)) => parts.iter().any(|part| {
            is_text(part)
                && part
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|text| !text.is_empty())
        }),
        _ => false,
    };
    let newest = messages
        .iter()
        .enumerate()
        .skip(usize::from(has_system))
        .rev()
        .find(|(_, message)| {
            matches!(
                message.get("role").and_then(serde_json::Value::as_str),
                Some("user" | "assistant")
            ) && has_text(message)
        })
        .map(|(index, _)| index);
    for index in has_system.then_some(0).into_iter().chain(newest) {
        let Some(content) = messages
            .get_mut(index)
            .and_then(|message| message.get_mut("content"))
        else {
            continue;
        };
        let marker = serde_json::json!({ "type": "ephemeral" });
        match content {
            serde_json::Value::String(text) => {
                *content = serde_json::json!([{
                    "type": "text",
                    "text": std::mem::take(text),
                    "cache_control": marker,
                }]);
            }
            serde_json::Value::Array(parts) => {
                if let Some(part) = parts
                    .iter_mut()
                    .rev()
                    .find(|part| is_text(part))
                    .and_then(serde_json::Value::as_object_mut)
                {
                    part.insert("cache_control".to_owned(), marker);
                }
            }
            _ => {}
        }
    }
}

/// Encodes one message into the dialect's shape.
fn encode_message(message: &Message, plan: &RequestPlan) -> Result<serde_json::Value> {
    match message.role {
        Role::System => Err(RuneError::invalid_field(
            "messages",
            "system content must be supplied as instructions, not as a message",
        )),
        Role::User => Ok(encode_user(message, plan)),
        Role::Assistant => Ok(encode_assistant(message, plan)),
        // Tool results are expanded before this point, because one message can
        // carry several of them and this returns a single message.
        Role::Tool => Err(RuneError::invariant(
            "tool_result_encoding",
            "a tool result message reached the single-message encoder",
        )),
    }
}

/// Encodes a user message, using content parts when images are present.
fn encode_user(message: &Message, plan: &RequestPlan) -> serde_json::Value {
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
                "image_url": { "url": plan.image_data.get(&image.id).map_or_else(
                    || format!("image-ref:{}", image.id),
                    |data| format!("data:{};base64,{data}", image.media_type),
                ) },
            })),
            _ => None,
        })
        .collect();

    serde_json::json!({ "role": "user", "content": parts })
}

/// Encodes an assistant message, including any tool calls and the reasoning
/// the endpoint wants sent back.
fn encode_assistant(message: &Message, plan: &RequestPlan) -> serde_json::Value {
    let compat = &plan.compat;
    let calls: Vec<serde_json::Value> = message
        .parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolCall {
                id,
                name,
                arguments,
            } => Some(serde_json::json!({
                "id": shape_call_id(id.as_str(), compat.tool_call_id),
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
        // A message carrying only tool calls sends a null content field, which
        // is what the reference implementation does, unless the endpoint
        // refuses one.
        value.insert(
            "content".to_owned(),
            match (text.is_empty(), compat.assistant_content_when_only_calls) {
                (true, EmptyContent::Null) => serde_json::Value::Null,
                (true, EmptyContent::EmptyString) => serde_json::json!(""),
                (false, _) => serde_json::json!(text),
            },
        );
        value.insert("tool_calls".to_owned(), serde_json::Value::Array(calls));
    }

    encode_reasoning(&mut value, message, plan);
    serde_json::Value::Object(value)
}

/// Reasoning stored with an assistant message.
#[derive(Debug, Default, serde::Deserialize)]
struct StoredReasoning {
    /// The field it arrived in, when known.
    #[serde(default)]
    field: Option<String>,
    /// The reasoning text, exactly as streamed.
    #[serde(default)]
    text: String,
    /// The `reasoning_details` entries, verbatim.
    #[serde(default)]
    details: Vec<serde_json::Value>,
}

/// Returns the reasoning stored with a message.
///
/// A message whose replay state carries none, including one recorded before
/// that state held reasoning, falls back to its reasoning parts. A malformed
/// state counts as absent.
fn reasoning_of(message: &Message) -> Option<StoredReasoning> {
    let stored = message
        .replay
        .as_deref()
        .and_then(|blob| serde_json::from_str::<serde_json::Value>(blob).ok())
        .and_then(|mut blob| blob.get_mut("reasoning").map(serde_json::Value::take))
        .and_then(|reasoning| serde_json::from_value::<StoredReasoning>(reasoning).ok());
    if stored.is_some() {
        return stored;
    }
    let text: String = message
        .parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Reasoning { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then_some(StoredReasoning {
        field: None,
        text,
        details: Vec::new(),
    })
}

/// Adds earlier reasoning to an assistant message in the field the endpoint
/// reads it from.
fn encode_reasoning(
    value: &mut serde_json::Map<String, serde_json::Value>,
    message: &Message,
    plan: &RequestPlan,
) {
    let compat = &plan.compat;
    if compat.reasoning_replay == ReasoningReplay::None {
        return;
    }
    let stored = reasoning_of(message).unwrap_or_default();
    let observed = stored.field.as_deref();
    let merged = plan.traits.family == Family::Kimi
        && compat.reasoning_replay != ReasoningReplay::ReasoningDetails
        && !stored.details.is_empty();
    let details = !merged
        && match compat.reasoning_replay {
            ReasoningReplay::ReasoningDetails => true,
            ReasoningReplay::AsObserved => observed == Some("reasoning_details"),
            _ => false,
        };
    let text_field = match compat.reasoning_replay {
        ReasoningReplay::ReasoningContent => "reasoning_content",
        ReasoningReplay::AsObserved if !details => REASONING_FIELDS
            .into_iter()
            .find(|field| Some(*field) == observed)
            .unwrap_or("reasoning_content"),
        _ => "reasoning",
    };
    if merged {
        value.insert(
            "reasoning_details".to_owned(),
            serde_json::Value::Array(merge_details(&stored.details)),
        );
    }
    if details && !stored.details.is_empty() {
        value.insert(
            "reasoning_details".to_owned(),
            serde_json::Value::Array(stored.details),
        );
    } else if !stored.text.is_empty() {
        value.insert(text_field.to_owned(), serde_json::json!(stored.text));
        return;
    }
    // Some endpoints refuse a tool conversation in which any assistant
    // message lacks the field, even an empty one.
    if compat.reasoning_always && plan.has_tools() && !value.contains_key(text_field) {
        value.insert(text_field.to_owned(), serde_json::json!(stored.text));
    }
}

/// Joins streamed `reasoning_details` fragments into whole entries.
///
/// Entries sharing an `index` form one entry at the place of the first: its
/// `summary`, `text` and `data` strings are joined in arrival order and its
/// other fields come from the first fragment. An entry with no `index` stands
/// alone. The `index` key is not kept.
fn merge_details(details: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut merged: Vec<serde_json::Value> = Vec::new();
    let mut groups: Vec<(serde_json::Value, usize)> = Vec::new();
    for entry in details {
        let Some(fields) = entry.as_object() else {
            merged.push(entry.clone());
            continue;
        };
        let mut fields = fields.clone();
        let index = fields.remove("index").filter(|index| !index.is_null());
        let group = index.as_ref().and_then(|index| {
            groups
                .iter()
                .find(|(known, _)| known == index)
                .map(|(_, position)| *position)
        });
        let target = group.and_then(|position| merged.get_mut(position));
        let Some(target) = target.and_then(serde_json::Value::as_object_mut) else {
            if let Some(index) = index {
                groups.push((index, merged.len()));
            }
            merged.push(serde_json::Value::Object(fields));
            continue;
        };
        for key in ["summary", "text", "data"] {
            let Some(addition) = fields.get(key).and_then(serde_json::Value::as_str) else {
                continue;
            };
            let joined = match target.get(key).and_then(serde_json::Value::as_str) {
                Some(existing) => format!("{existing}{addition}"),
                None => addition.to_owned(),
            };
            target.insert(key.to_owned(), serde_json::json!(joined));
        }
    }
    merged
}

/// Encodes one tool result message.
///
/// Every result in the message becomes its own wire message, in the order the
/// calls were made. A message holding several results is the shape a batch
/// produces, and emitting only the first leaves the other calls unanswered,
/// which an endpoint rejects as a malformed conversation.
///
/// A tool message carries text only in this dialect, so images attached to
/// the results follow them in one user message.
fn encode_tool_messages(message: &Message, plan: &RequestPlan) -> Vec<serde_json::Value> {
    let mut messages: Vec<serde_json::Value> = message
        .parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolResult {
                id,
                name,
                content,
                is_error,
            } => {
                let mut result = serde_json::json!({
                    "role": "tool",
                    "content": content,
                    "tool_call_id": shape_call_id(id.as_str(), plan.compat.tool_call_id),
                    // The compatible-server ecosystem reads a failure from this
                    // field rather than from the text, and the text is still
                    // sent so a server that ignores the field loses nothing.
                    "is_error": is_error,
                });
                if plan.compat.tool_result_name
                    && let Some(result) = result.as_object_mut()
                {
                    result.insert("name".to_owned(), serde_json::json!(name));
                }
                Some(result)
            }
            _ => None,
        })
        .collect();
    if let Some((text, images)) = crate::provider::tool_turn_images(message, plan) {
        let mut content = vec![serde_json::json!({ "type": "text", "text": text })];
        content.extend(images.into_iter().map(|(media_type, data)| {
            serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{media_type};base64,{data}") },
            })
        }));
        messages.push(serde_json::json!({ "role": "user", "content": content }));
    }
    messages
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

/// Writes the reasoning controls of the endpoint's thinking format.
fn insert_thinking(body: &mut serde_json::Map<String, serde_json::Value>, plan: &RequestPlan) {
    let compat = &plan.compat;
    let effort = plan.effort;
    let id = plan.model.to_ascii_lowercase();
    match compat.thinking_format {
        ThinkingFormat::None => {}
        ThinkingFormat::DeepSeek => insert_deepseek_thinking(body, effort),
        ThinkingFormat::ReasoningEffort
            if plan.traits.family == Family::Kimi
                && compat.schema_rules == SchemaRules::Moonshot =>
        {
            insert_kimi_thinking(body, effort, &id);
        }
        ThinkingFormat::ReasoningEffort => {
            if let Some(level) = effort_wire(effort) {
                body.insert("reasoning_effort".to_owned(), serde_json::json!(level));
            }
        }
        ThinkingFormat::OpenRouter => {
            if let Some(level) = effort_wire(effort) {
                body.insert(
                    "reasoning".to_owned(),
                    serde_json::json!({ "effort": level }),
                );
            }
        }
        ThinkingFormat::Zai => {
            // GLM-5.3 cannot turn thinking off, so it stays enabled there.
            let thinking = if effort == Effort::None && !id.contains("glm-5.3") {
                serde_json::json!({ "type": "disabled" })
            } else {
                serde_json::json!({ "type": "enabled", "clear_thinking": false })
            };
            body.insert("thinking".to_owned(), thinking);
        }
        ThinkingFormat::MiniMax => {
            body.insert("reasoning_split".to_owned(), serde_json::json!(true));
            let level = match effort {
                Effort::Auto | Effort::None => None,
                Effort::Minimal => Some("low"),
                other => effort_wire(other),
            };
            if let Some(level) = level.filter(|_| id.contains("m3.1-flash")) {
                body.insert("reasoning_effort".to_owned(), serde_json::json!(level));
            }
        }
        ThinkingFormat::QwenEnable => {
            if effort != Effort::Auto {
                body.insert(
                    "enable_thinking".to_owned(),
                    serde_json::json!(effort != Effort::None),
                );
            }
        }
        ThinkingFormat::ChatTemplate => {
            if effort != Effort::Auto {
                body.insert(
                    "chat_template_kwargs".to_owned(),
                    serde_json::json!({ "enable_thinking": effort != Effort::None }),
                );
            }
        }
    }
}

/// Writes Kimi's controls: `reasoning_effort` in three levels, except the
/// K2 models, of which only K2.6 can switch thinking off.
fn insert_kimi_thinking(
    body: &mut serde_json::Map<String, serde_json::Value>,
    effort: Effort,
    id: &str,
) {
    let level = match effort {
        Effort::Auto => return,
        Effort::None => {
            if id.contains("k2.6") {
                body.insert(
                    "thinking".to_owned(),
                    serde_json::json!({ "type": "disabled" }),
                );
            }
            return;
        }
        Effort::Minimal | Effort::Low => "low",
        Effort::Medium | Effort::High | Effort::Xhigh => "high",
        Effort::Max => "max",
    };
    if !id.contains("k2.6") && !id.contains("k2.7") {
        body.insert("reasoning_effort".to_owned(), serde_json::json!(level));
    }
}

/// Writes DeepSeek's thinking switch and its three effort levels. `Auto`
/// sends nothing, which leaves the server default of thinking at high effort.
fn insert_deepseek_thinking(body: &mut serde_json::Map<String, serde_json::Value>, effort: Effort) {
    let level = match effort {
        Effort::Auto => return,
        Effort::None => {
            body.insert(
                "thinking".to_owned(),
                serde_json::json!({ "type": "disabled" }),
            );
            return;
        }
        Effort::Minimal | Effort::Low => "low",
        Effort::Medium | Effort::High | Effort::Xhigh => "high",
        Effort::Max => "max",
    };
    body.insert(
        "thinking".to_owned(),
        serde_json::json!({ "type": "enabled" }),
    );
    body.insert("reasoning_effort".to_owned(), serde_json::json!(level));
}

/// The text fields reasoning arrives in, in the order they are read.
const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

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
    /// Whether the endpoint must end the stream with `[DONE]`.
    needs_done_marker: bool,
    /// Whether the stream ended without `[DONE]` in a state taken as complete.
    ended_without_marker: bool,
    saw_role: bool,
    event_count: usize,
    /// Every reasoning delta emitted, in order.
    reasoning: String,
    /// Every `reasoning_details` entry, verbatim, because some endpoints need
    /// them sent back exactly.
    reasoning_details: Vec<serde_json::Value>,
    /// The field reasoning first arrived in.
    observed_field: Option<&'static str>,
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
            needs_done_marker: false,
            ended_without_marker: false,
            saw_role: false,
            event_count: 0,
            reasoning: String::new(),
            reasoning_details: Vec::new(),
            observed_field: None,
        }
    }

    /// Sets the stream bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: Limit) -> Self {
        self.limits = limits;
        self
    }

    /// Sets whether a stream that ends without `[DONE]` is incomplete.
    #[must_use]
    pub fn with_done_marker(mut self, needs_done_marker: bool) -> Self {
        self.needs_done_marker = needs_done_marker;
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
            return Err(stream_failure(error, "the provider reported an error"));
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
            self.finish_reason = Some(map_finish_reason(reason));
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
        if delta
            .get("function_call")
            .is_some_and(|call| !is_empty_function_call(call))
        {
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

        // Servers name the reasoning field differently and some send it under
        // two names at once, so the first that carries text is the only one read.
        let text_field = REASONING_FIELDS.into_iter().find_map(|field| {
            delta
                .get(field)
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| (field, text))
        });
        if let Some((field, text)) = text_field {
            self.observed_field.get_or_insert(field);
            self.push_reasoning(text, out)?;
        }
        if let Some(details) = delta
            .get("reasoning_details")
            .and_then(serde_json::Value::as_array)
            .filter(|details| !details.is_empty())
        {
            self.observed_field.get_or_insert("reasoning_details");
            for entry in details {
                let key = match entry.get("type").and_then(serde_json::Value::as_str) {
                    Some("reasoning.text") => Some("text"),
                    Some("reasoning.summary") => Some("summary"),
                    _ => None,
                };
                let text = key
                    .and_then(|key| entry.get(key))
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty());
                if text_field.is_none()
                    && let Some(text) = text
                {
                    self.push_reasoning(text, out)?;
                }
                self.reasoning_details.push(entry.clone());
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

    /// Records and emits one reasoning delta.
    fn push_reasoning(
        &mut self,
        text: &str,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        self.reasoning.push_str(text);
        if self.reasoning.len() > self.limits.max_content_bytes {
            return Err(RuneError::too_large(
                "stream.reasoning",
                self.reasoning.len(),
                self.limits.max_content_bytes,
            ));
        }
        out.push(crate::stream::ProviderEvent::ReasoningDelta {
            delta: text.to_owned(),
        });
        Ok(())
    }

    /// Applies one tool-call delta.
    fn apply_tool_call(
        &mut self,
        call: &serde_json::Value,
        out: &mut Vec<crate::stream::ProviderEvent>,
    ) -> Result<()> {
        let index = match call.get("index").filter(|index| !index.is_null()) {
            Some(index) => index
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| protocol_violation("a tool call delta carried no index"))?,
            // Some servers omit the index; a repeated identifier then names the
            // call, and anything else starts a new one.
            None => call
                .get("id")
                .and_then(serde_json::Value::as_str)
                .and_then(|id| {
                    self.calls.iter().position(|slot| {
                        slot.id
                            .as_ref()
                            .is_some_and(|existing| existing.as_str() == id)
                    })
                })
                .unwrap_or(self.calls.len()),
        };

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

    /// Closes every open call at the end of a turn that has a finish reason.
    ///
    /// Several compatible servers end a tool turn with `stop`, so every call
    /// that started is closed whatever the reason, and a turn that made calls
    /// is reported as one.
    fn close_turn(&mut self, out: &mut Vec<crate::stream::ProviderEvent>) -> Result<()> {
        self.close_tool_calls(out)?;
        if self.finish_reason == Some(FinishReason::Stop) && self.has_started_calls() {
            self.finish_reason = Some(FinishReason::ToolCalls);
        }
        Ok(())
    }

    fn has_started_calls(&self) -> bool {
        self.calls.iter().any(|call| call.started)
    }

    /// Decides whether a stream that ended without `[DONE]` is complete.
    ///
    /// It is when a finish reason arrived, or when it made calls whose
    /// arguments are all whole JSON values. Anything else may have been cut.
    fn end_without_marker(&mut self, out: &mut Vec<crate::stream::ProviderEvent>) -> Result<()> {
        if self.finish_reason.is_none() {
            let calls_complete =
                self.has_started_calls()
                    && self.calls.iter().filter(|call| call.started).all(|call| {
                        serde_json::from_str::<serde_json::Value>(&call.arguments).is_ok()
                    });
            if !calls_complete {
                return Ok(());
            }
            self.finish_reason = Some(FinishReason::ToolCalls);
        }
        self.close_turn(out)?;
        self.ended_without_marker = true;
        Ok(())
    }
}

/// Whether a legacy `function_call` value carries nothing.
///
/// Some servers send an empty one beside `tool_calls`; one that names a
/// function or carries arguments is still refused.
fn is_empty_function_call(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Object(map) => ["name", "arguments"].into_iter().all(|key| {
            map.get(key)
                .is_none_or(|field| field.is_null() || field.as_str() == Some(""))
        }),
        _ => false,
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
            if !self.saw_done && !self.needs_done_marker {
                self.end_without_marker(out)?;
            }
            return Ok(());
        };

        if payload.trim() == "[DONE]" {
            self.saw_done = true;
            if self.finish_reason.is_none() {
                if self.has_started_calls() {
                    self.finish_reason = Some(FinishReason::ToolCalls);
                } else if !self.content.is_empty() {
                    self.finish_reason = Some(FinishReason::Stop);
                } else {
                    return Err(protocol_violation(
                        "the stream ended without a finish reason",
                    ));
                }
            }
            return self.close_turn(out);
        }

        self.event_count = self.event_count.saturating_add(1);
        self.limits.check_events(self.event_count)?;
        self.apply_frame(payload, out)
    }

    fn is_finished(&self) -> bool {
        self.saw_done || self.ended_without_marker
    }

    fn usage(&self) -> Usage {
        self.usage
    }

    fn replay(&self) -> Option<String> {
        // The identifiers are recorded so a filtered history can drop a call
        // and its result together, and the reasoning so it can be sent back in
        // the field the endpoint reads.
        let ids: Vec<&str> = self
            .calls
            .iter()
            .filter_map(|call| call.id.as_ref().map(ToolCallId::as_str))
            .collect();
        if ids.is_empty() && self.observed_field.is_none() {
            return None;
        }
        let mut replay = serde_json::Map::new();
        replay.insert("_tool_call_ids".to_owned(), serde_json::json!(ids));
        if let Some(field) = self.observed_field {
            let mut reasoning = serde_json::Map::new();
            reasoning.insert("field".to_owned(), serde_json::json!(field));
            reasoning.insert("text".to_owned(), serde_json::json!(self.reasoning));
            if !self.reasoning_details.is_empty() {
                reasoning.insert(
                    "details".to_owned(),
                    serde_json::Value::Array(self.reasoning_details.clone()),
                );
            }
            replay.insert("reasoning".to_owned(), serde_json::Value::Object(reasoning));
        }
        serde_json::to_string(&replay).ok()
    }

    fn salvage(&mut self, out: &mut Vec<crate::stream::ProviderEvent>) {
        for slot in &mut self.calls {
            if !slot.started || slot.ended {
                continue;
            }
            let Some(id) = slot.id.clone() else {
                continue;
            };
            let whole = serde_json::from_str::<serde_json::Value>(&slot.arguments)
                .is_ok_and(|value| value.is_object());
            if !whole {
                continue;
            }
            out.push(crate::stream::ProviderEvent::ToolCallEnd {
                id,
                arguments: slot.arguments.clone(),
            });
            slot.ended = true;
        }
    }

    fn finish(&self) -> Result<FinishReason> {
        if !self.saw_done && !self.ended_without_marker {
            return Err(incomplete_stream());
        }
        if let Some(reason) = self.finish_reason {
            // Tool calls that never reported a name cannot be executed. A slot
            // that never started is a gap in the indices the server chose, not
            // a call.
            if reason == FinishReason::ToolCalls
                && self
                    .calls
                    .iter()
                    .any(|call| call.started && call.name.is_none())
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
        // DeepSeek reports hits at the top level; its `prompt_tokens` already
        // counts them.
        cache_read_tokens: value
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(serde_json::Value::as_u64)
            .or_else(|| field("prompt_cache_hit_tokens")),
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
///
/// Compatible servers extend the vocabulary, and the reason arrives after the
/// answer has streamed in full, so a reason this build does not know is taken
/// as a natural stop rather than a failure that would discard the answer.
fn map_finish_reason(raw: &str) -> FinishReason {
    match raw {
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "length" | "model_length" | "max_tokens" | "model_context_window_exceeded" => {
            FinishReason::MaxTokens
        }
        "content_filter" => FinishReason::ContentFilter,
        // A gateway that loses its upstream mid-answer says so here, and the
        // answer it ended is not a complete one.
        "error" => FinishReason::ProviderError,
        _ => FinishReason::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolSpec;
    use crate::provider::ToolChoice;
    use crate::stream::ProviderEvent;
    use rune_core::error::ErrorCode;

    fn plan_with_user(text: &str) -> RequestPlan {
        let mut plan = RequestPlan::new("test/model");
        plan.instructions = "be helpful".to_owned();
        plan.messages = vec![Message::user(text)];
        plan
    }

    /// A plan for `model` on a generic endpoint, sampled as the loop samples it.
    fn sampled_plan(model: &str) -> RequestPlan {
        let mut plan = plan_with_user("hi");
        plan.model = model.to_owned();
        (plan.compat, plan.traits) = crate::compat::resolve("chat_completions", None, model);
        plan.sampling = plan.traits.sampling;
        plan
    }

    #[test]
    fn family_sampling_defaults_are_sent() {
        let sampled = |model: &str| {
            let body = ChatCompletions
                .build_request(&sampled_plan(model))
                .expect("build");
            ["temperature", "top_p", "top_k"].map(|field| body.get(field).cloned())
        };
        let json = |value: serde_json::Value| Some(value);
        assert_eq!(
            sampled("qwen3-coder"),
            [json(serde_json::json!(0.55)), None, None]
        );
        assert_eq!(
            sampled("gemini-3.8-flash"),
            [
                json(serde_json::json!(1.0)),
                json(serde_json::json!(0.95)),
                json(serde_json::json!(64))
            ]
        );
        assert_eq!(
            sampled("glm-4.7"),
            [json(serde_json::json!(1.0)), None, None]
        );
        assert_eq!(
            sampled("minimax-m3"),
            [
                json(serde_json::json!(1.0)),
                json(serde_json::json!(0.95)),
                None
            ]
        );
        assert_eq!(sampled("test/model"), [None, None, None]);
    }

    #[test]
    fn a_user_temperature_overrides_the_default() {
        let mut plan = sampled_plan("gemini-3.8-flash");
        plan.sampling.temperature = Some(0.2);
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["top_p"], 0.95);
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
    fn one_call_mode_is_explicitly_serialized() {
        let mut plan = plan_with_user("hi");
        plan.tools = vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];
        plan.parallel_tool_calls = false;
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["tool_choice"], "auto");

        plan.tools.clear();
        let body = ChatCompletions
            .build_request(&plan)
            .expect("build without tools");
        assert!(body.get("parallel_tool_calls").is_none());
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
                origin: None,
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
    fn every_tool_result_in_a_batch_reaches_the_wire() {
        // The history groups a batch of results into one message, and a request
        // that answers only the first call of several is rejected by the
        // endpoint as a malformed conversation. The failure is invisible in the
        // body, so it surfaces as a bare status with nothing to act on.
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![
            Message::assistant(vec![
                ContentPart::ToolCall {
                    id: ToolCallId::new("call_1").expect("id"),
                    name: "shell".to_owned(),
                    arguments: "{}".to_owned(),
                },
                ContentPart::ToolCall {
                    id: ToolCallId::new("call_2").expect("id"),
                    name: "glob_files".to_owned(),
                    arguments: "{}".to_owned(),
                },
            ]),
            Message {
                role: Role::Tool,
                parts: vec![
                    ContentPart::ToolResult {
                        id: ToolCallId::new("call_1").expect("id"),
                        name: "shell".to_owned(),
                        content: "first".to_owned(),
                        is_error: false,
                    },
                    ContentPart::ToolResult {
                        id: ToolCallId::new("call_2").expect("id"),
                        name: "glob_files".to_owned(),
                        content: "second".to_owned(),
                        is_error: false,
                    },
                ],
                replay: None,
                origin: None,
            },
        ];
        let body = ChatCompletions.build_request(&plan).expect("build");
        let messages = body["messages"].as_array().expect("array");

        let answered: Vec<&str> = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .filter_map(|message| message["tool_call_id"].as_str())
            .collect();
        assert_eq!(
            answered,
            vec!["call_1", "call_2"],
            "a tool result was dropped, leaving its call unanswered"
        );
    }

    #[test]
    fn a_single_tool_result_still_encodes_alone() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult {
                id: ToolCallId::new("call_1").expect("id"),
                name: "read_file".to_owned(),
                content: "contents".to_owned(),
                is_error: false,
            }],
            replay: None,
            origin: None,
        }];
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert_eq!(body["messages"].as_array().expect("array").len(), 1);
        assert_eq!(body["messages"][0]["content"], "contents");
        assert_eq!(body["messages"][0]["tool_call_id"], "call_1");
    }

    #[test]
    fn a_tool_result_message_with_no_results_encodes_to_nothing() {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message {
            role: Role::Tool,
            parts: Vec::new(),
            replay: None,
            origin: None,
        }];
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert!(body["messages"].as_array().expect("array").is_empty());
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
    fn a_tool_image_follows_the_tool_messages_as_a_user_message() {
        let mut plan = tool_turn(vec![
            result("call_1", "image a.png attached"),
            image(1),
            result("call_2", "image b.png attached"),
            image(2),
        ]);
        plan.image_data.insert(1, "QUJD".into());
        plan.image_data.insert(2, "REVG".into());
        let body = ChatCompletions.build_request(&plan).expect("build");
        let messages = body["messages"].as_array().expect("array");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["tool_call_id"], "call_1");
        assert_eq!(messages[0]["content"], "image a.png attached");
        assert_eq!(messages[1]["tool_call_id"], "call_2");
        assert_eq!(
            messages[2],
            serde_json::json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "Images from the tool results above."},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,REVG"}},
                ],
            })
        );
    }

    #[test]
    fn a_tool_turn_without_images_is_encoded_as_before() {
        let plan = tool_turn(vec![result("call_1", "contents")]);
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert_eq!(
            body["messages"],
            serde_json::json!([{
                "role": "tool",
                "content": "contents",
                "tool_call_id": "call_1",
                "is_error": false,
            }])
        );
    }

    #[test]
    fn a_missing_image_is_left_out() {
        let mut plan = tool_turn(vec![
            result("call_1", "image a.png attached"),
            image(1),
            result("call_2", "image b.png attached"),
            image(2),
        ]);
        plan.image_data.insert(2, "REVG".into());
        let body = ChatCompletions.build_request(&plan).expect("build");
        let content = body["messages"][2]["content"].as_array().expect("parts");
        assert_eq!(content.len(), 2);
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,REVG");

        let plan = tool_turn(vec![result("call_1", "image a.png attached"), image(1)]);
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert_eq!(
            body["messages"][1]["content"],
            serde_json::json!([{
                "type": "text",
                "text": "Images from the tool results above are no longer available.",
            }])
        );
        assert!(!body.to_string().contains("image-ref"));
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
    fn auto_effort_preserves_provider_defaults() {
        let body = ChatCompletions
            .build_request(&plan_with_user("hi"))
            .expect("build");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn explicit_efforts_are_serialized_in_the_request() {
        for (effort, wire) in [
            (Effort::None, "none"),
            (Effort::Minimal, "minimal"),
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::Xhigh, "xhigh"),
            (Effort::Max, "max"),
        ] {
            let mut plan = plan_with_user("hi");
            plan.effort = effort;
            let body = ChatCompletions.build_request(&plan).expect("build");
            assert_eq!(body["reasoning_effort"], wire, "{effort}");
        }
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
    fn salvage_closes_only_calls_with_complete_arguments() {
        let (mut events, mut reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"read_file","arguments":"[1]"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"id":"c3","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}"#,
        ]);
        reducer.salvage(&mut events);
        let ended: Vec<(String, String)> = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ToolCallEnd { id, arguments } => {
                    Some((id.to_string(), arguments.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(ended, [("c1".to_owned(), r#"{"path":"a"}"#.to_owned())]);

        // A closed call is not closed twice.
        let mut again = Vec::new();
        reducer.salvage(&mut again);
        assert!(again.is_empty(), "{again:?}");
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

    fn reasoning_text(events: &[ProviderEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ReasoningDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Reduces a recorded stream file, one `data:` line per frame.
    fn reduce_recorded(recorded: &str) -> (Vec<ProviderEvent>, Reducer) {
        let frames: Vec<&str> = recorded
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .collect();
        reduce(&frames)
    }

    #[test]
    fn reasoning_is_read_under_every_field_name() {
        for (recorded, text, field) in [
            (
                include_str!("../tests/fixtures/chat_completions/reasoning_content/response.txt"),
                "Check the file first. Then answer.",
                "reasoning_content",
            ),
            (
                include_str!("../tests/fixtures/chat_completions/reasoning/response.txt"),
                "Check the file first. Then answer.",
                "reasoning",
            ),
            (
                include_str!("../tests/fixtures/chat_completions/reasoning_text/response.txt"),
                "Check the file first. Then answer.",
                "reasoning_text",
            ),
            (
                include_str!("../tests/fixtures/chat_completions/reasoning_details/response.txt"),
                "Check the file first. Then answer.",
                "reasoning",
            ),
        ] {
            let (events, reducer) = reduce_recorded(recorded);
            reducer.finish().expect("finished");
            assert_eq!(reasoning_text(&events), text, "{field}");
            assert_eq!(reducer.reasoning, text, "{field}");
            assert_eq!(reducer.observed_field, Some(field));
        }

        let (_, reducer) = reduce_recorded(include_str!(
            "../tests/fixtures/chat_completions/reasoning_details/response.txt"
        ));
        assert_eq!(reducer.reasoning_details.len(), 3);
        assert_eq!(
            reducer.reasoning_details[2]["type"], "reasoning.encrypted",
            "{:?}",
            reducer.reasoning_details
        );
    }

    #[test]
    fn a_reasoning_field_and_its_alias_are_not_doubled() {
        let (events, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"reasoning":"step one","reasoning_content":"step one"}}]}"#,
        ]);
        assert_eq!(reasoning_text(&events), "step one");
        assert_eq!(reducer.observed_field, Some("reasoning_content"));

        let (events, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"reasoning":"","reasoning_content":"x"}}]}"#,
        ]);
        assert_eq!(reasoning_text(&events), "x");
        assert_eq!(reducer.observed_field, Some("reasoning_content"));

        let (events, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.encrypted","data":"opaque","id":"r1","format":"openai-responses-v1","index":0}]}}]}"#,
        ]);
        assert!(reasoning_text(&events).is_empty(), "{events:?}");
        assert_eq!(reducer.reasoning_details.len(), 1);
        assert_eq!(reducer.observed_field, Some("reasoning_details"));

        let (events, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"reasoning_details":[{"type":"reasoning.summary","summary":"plan"},{"type":"reasoning.text","text":" and act"}]}}]}"#,
        ]);
        assert_eq!(reasoning_text(&events), "plan and act");
        assert_eq!(reducer.observed_field, Some("reasoning_details"));
    }

    #[test]
    fn reasoning_past_the_content_bound_is_rejected() {
        let mut reducer = Reducer::new().with_limits(Limit {
            max_content_bytes: 4,
            ..Limit::default()
        });
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{"reasoning_content":"far too long"}}]}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
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
    fn tool_calls_finished_with_stop_are_still_completed() {
        // Gemini's compatible endpoint and several local servers end a tool
        // turn with `stop` rather than `tool_calls`.
        let (events, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::ToolCalls);
        let ends: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ToolCallEnd { arguments, .. } => Some(arguments.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ends, vec!["{\"path\":\"a.rs\"}"]);
    }

    #[test]
    fn a_tool_call_index_that_skips_a_slot_still_finishes() {
        let (events, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_1","function":{"name":"read_file","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::ToolCalls);
        let ends = events
            .iter()
            .filter(|event| matches!(event, ProviderEvent::ToolCallEnd { .. }))
            .count();
        assert_eq!(ends, 1);
        assert_eq!(
            reducer.replay().expect("replay"),
            r#"{"_tool_call_ids":["call_1"]}"#
        );
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
    fn a_done_without_a_finish_reason_or_content_is_rejected() {
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
    fn an_unknown_finish_reason_keeps_the_answer() {
        let (events, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"content":"done"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"sideways"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::Stop);
        assert!(events.contains(&ProviderEvent::TextDelta {
            delta: "done".to_owned()
        }));
    }

    #[test]
    fn a_model_length_finish_reason_maps_to_the_token_limit() {
        let (_, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"model_length"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::MaxTokens);
    }

    #[test]
    fn an_error_finish_reason_is_not_a_success() {
        let (_, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"content":"par"},"finish_reason":"error"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(
            reducer.finish().expect("finished"),
            FinishReason::ProviderError
        );
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
    fn a_mid_stream_upstream_failure_is_retried() {
        // A gateway reports an upstream that dropped mid-answer with a status
        // number in place of a type, alongside an error finish.
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        reducer
            .apply(
                Some(r#"{"choices":[{"index":0,"delta":{"content":"par"}}]}"#),
                &mut events,
            )
            .expect("content");
        let err = reducer
            .apply(
                Some(r#"{"error":{"code":502,"message":"Provider disconnected"},"choices":[{"index":0,"delta":{"content":""},"finish_reason":"error"}]}"#),
                &mut events,
            )
            .expect_err("rejected");
        let err = crate::error::NetError::from(err);
        assert!(err.is_retryable(), "{err}");
        assert!(err.message().contains("Provider disconnected"));
    }

    #[test]
    fn an_invalid_request_error_frame_is_not_retried() {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let err = reducer
            .apply(
                Some(r#"{"error":{"message":"unknown parameter","type":"invalid_request_error"}}"#),
                &mut events,
            )
            .expect_err("rejected");
        assert!(!crate::error::NetError::from(err).is_retryable());
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

    fn assistant_after(reasoning: Option<&str>) -> RequestPlan {
        let mut plan = default_body_plan();
        plan.compat.reasoning_replay = ReasoningReplay::ReasoningContent;
        plan.compat.reasoning_always = true;
        if let Some(text) = reasoning {
            plan.messages[1].parts.insert(
                0,
                ContentPart::Reasoning {
                    text: text.to_owned(),
                },
            );
        }
        plan
    }

    #[test]
    fn reasoning_is_always_sent_with_tools_when_required() {
        let body = ChatCompletions
            .build_request(&assistant_after(None))
            .expect("build");
        assert_eq!(body["messages"][2]["role"], "assistant");
        assert_eq!(body["messages"][2]["reasoning_content"], "");

        let mut plan = assistant_after(None);
        plan.compat.reasoning_always = false;
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert!(body["messages"][2].get("reasoning_content").is_none());

        let body = ChatCompletions
            .build_request(&assistant_after(Some("think")))
            .expect("build");
        assert_eq!(body["messages"][2]["reasoning_content"], "think");
    }

    #[test]
    fn reasoning_is_omitted_without_tools() {
        let mut plan = assistant_after(None);
        plan.tools.clear();
        let body = ChatCompletions.build_request(&plan).expect("build");
        let assistant = body["messages"][2].as_object().expect("assistant");
        assert!(
            !assistant.contains_key("reasoning_content"),
            "{assistant:?}"
        );
        assert!(!assistant.contains_key("reasoning"), "{assistant:?}");
    }

    #[test]
    fn an_old_history_replays_reasoning_from_its_parts() {
        for blob in [
            Some(r#"{"_tool_call_ids":["call_1"]}"#),
            Some("not json"),
            None,
        ] {
            let mut plan = assistant_after(Some("from the parts"));
            plan.compat.reasoning_always = false;
            plan.messages[1].replay = blob.map(str::to_owned);
            let body = ChatCompletions.build_request(&plan).expect("build");
            assert_eq!(
                body["messages"][2]["reasoning_content"], "from the parts",
                "{blob:?}"
            );
        }

        let mut plan = assistant_after(Some("from the parts"));
        plan.messages[1].replay = Some(
            r#"{"_tool_call_ids":["call_1"],"reasoning":{"field":"reasoning","text":"stored"}}"#
                .to_owned(),
        );
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert_eq!(body["messages"][2]["reasoning_content"], "stored");

        plan.compat.reasoning_replay = ReasoningReplay::None;
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert!(body["messages"][2].get("reasoning_content").is_none());
    }

    #[test]
    fn replay_stores_the_reasoning_and_its_field() {
        let (_, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"a"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"b"}}]}"#,
        ]);
        let replay: serde_json::Value =
            serde_json::from_str(&reducer.replay().expect("replay")).expect("json");
        assert_eq!(
            replay,
            serde_json::json!({
                "_tool_call_ids": [],
                "reasoning": { "field": "reasoning_content", "text": "ab" },
            })
        );
    }

    #[test]
    fn an_empty_answer_still_finishes() {
        let (_, reducer) = reduce(&[
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(reducer.finish().expect("finished"), FinishReason::Stop);
    }

    fn default_body_plan() -> RequestPlan {
        let mut plan = plan_with_user("start");
        plan.tools = vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        }];
        plan.tool_choice = ToolChoice::Auto;
        plan.effort = Effort::High;
        plan.max_output_tokens = Some(1024);
        plan.messages
            .push(Message::assistant(vec![ContentPart::ToolCall {
                id: ToolCallId::new("call_1").expect("id"),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"a\"}".to_owned(),
            }]));
        plan.messages.push(Message {
            role: Role::Tool,
            parts: vec![result("call_1", "contents")],
            replay: None,
            origin: None,
        });
        plan.messages.push(Message::user("thanks"));
        plan
    }

    /// The body a default plan produced before the endpoint table shaped it.
    const TODAYS_BODY: &str = r#"{"model":"test/model","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"system","content":"be helpful"},{"role":"user","content":"start"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}]},{"role":"tool","content":"contents","tool_call_id":"call_1","is_error":false},{"role":"user","content":"thanks"}],"tools":[{"type":"function","function":{"name":"read_file","description":"Read a file","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}],"tool_choice":"auto","parallel_tool_calls":true,"reasoning_effort":"high","max_tokens":1024}"#;

    fn shaped(change: impl FnOnce(&mut crate::compat::EndpointCompat)) -> serde_json::Value {
        let mut plan = default_body_plan();
        change(&mut plan.compat);
        ChatCompletions.build_request(&plan).expect("build")
    }

    fn without_tools(change: impl FnOnce(&mut crate::compat::EndpointCompat)) -> serde_json::Value {
        let mut plan = default_body_plan();
        plan.tools.clear();
        change(&mut plan.compat);
        ChatCompletions.build_request(&plan).expect("build")
    }

    #[test]
    fn a_default_compat_sends_todays_body() {
        let body = ChatCompletions
            .build_request(&default_body_plan())
            .expect("build");
        assert_eq!(serde_json::to_string(&body).expect("json"), TODAYS_BODY);
    }

    #[test]
    fn the_developer_role_carries_the_instructions() {
        let body = shaped(|compat| compat.system_role = crate::compat::SystemRole::Developer);
        assert_eq!(body["messages"][0]["role"], "developer");
        assert_eq!(body["messages"][0]["content"], "be helpful");
    }

    #[test]
    fn usage_is_not_requested_when_the_endpoint_refuses_it() {
        let body = shaped(|compat| compat.stream_usage = false);
        assert!(body.get("stream_options").is_none(), "{body}");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn the_ceiling_uses_the_configured_field_name() {
        let body = shaped(|compat| {
            compat.max_tokens_field = crate::compat::MaxTokensField::MaxCompletionTokens;
        });
        assert_eq!(body["max_completion_tokens"], 1024);
        assert!(body.get("max_tokens").is_none(), "{body}");
    }

    #[test]
    fn a_call_only_message_can_send_an_empty_string() {
        let body = shaped(|compat| {
            compat.assistant_content_when_only_calls = EmptyContent::EmptyString;
        });
        assert_eq!(body["messages"][2]["content"], "");
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn a_tool_result_can_carry_its_tool_name() {
        let body = shaped(|compat| compat.tool_result_name = true);
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][3]["name"], "read_file");
        let default = shaped(|_| {});
        assert!(default["messages"][3].get("name").is_none());
    }

    #[test]
    fn history_with_calls_and_no_tools_sends_an_empty_tool_list() {
        let body = without_tools(|compat| compat.tools_with_tool_history = true);
        assert_eq!(body["tools"], serde_json::json!([]));
        assert!(body.get("tool_choice").is_none(), "{body}");
        assert!(body.get("parallel_tool_calls").is_none(), "{body}");

        let plain = without_tools(|_| {});
        assert!(plain.get("tools").is_none(), "{plain}");

        let mut no_calls = plan_with_user("hi");
        no_calls.compat.tools_with_tool_history = true;
        let body = ChatCompletions.build_request(&no_calls).expect("build");
        assert!(body.get("tools").is_none(), "{body}");

        let with_tools = shaped(|compat| compat.tools_with_tool_history = true);
        assert_eq!(with_tools, shaped(|_| {}));
    }

    #[test]
    fn an_assistant_turn_separates_a_tool_result_from_a_user_message() {
        let body = shaped(|compat| compat.assistant_after_tool_result = true);
        let messages = body["messages"].as_array().expect("array");
        assert_eq!(messages.len(), 6);
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(
            messages[4],
            serde_json::json!({ "role": "assistant", "content": "Done." })
        );
        assert_eq!(messages[5]["content"], "thanks");

        // Followed by an assistant message, or ending the list: nothing added.
        let mut plan = default_body_plan();
        plan.compat.assistant_after_tool_result = true;
        plan.messages.pop();
        let ends = ChatCompletions.build_request(&plan).expect("build");
        assert_eq!(ends["messages"].as_array().expect("array").len(), 4);
        plan.messages
            .push(Message::assistant(vec![ContentPart::Text {
                text: "read it".to_owned(),
            }]));
        let answered = ChatCompletions.build_request(&plan).expect("build");
        let messages = answered["messages"].as_array().expect("array");
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[4]["content"], "read it");
    }

    #[test]
    fn the_parallel_flag_can_be_left_out() {
        let body = shaped(|compat| compat.parallel_tool_calls_field = false);
        assert!(body.get("parallel_tool_calls").is_none(), "{body}");
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn call_ids_are_shaped_the_same_on_call_and_result() {
        let mut plan = default_body_plan();
        let long = "call_with.a:long/identifier-that-goes-past-forty-bytes";
        plan.messages[1] = Message::assistant(vec![ContentPart::ToolCall {
            id: ToolCallId::new(long).expect("id"),
            name: "read_file".to_owned(),
            arguments: "{}".to_owned(),
        }]);
        plan.messages[2] = Message {
            role: Role::Tool,
            parts: vec![result(long, "contents")],
            replay: None,
            origin: None,
        };
        for shape in [
            crate::compat::CallIdShape::ClaudeSafe,
            crate::compat::CallIdShape::Max40,
            crate::compat::CallIdShape::Alnum9,
        ] {
            plan.compat.tool_call_id = shape;
            let body = ChatCompletions.build_request(&plan).expect("build");
            let call = body["messages"][2]["tool_calls"][0]["id"]
                .as_str()
                .expect("call id");
            assert_eq!(call, shape_call_id(long, shape));
            assert_ne!(call, long, "{shape:?}");
            assert_eq!(body["messages"][3]["tool_call_id"], call, "{shape:?}");
        }
    }

    #[test]
    fn anthropic_markers_split_the_system_message() {
        let mut plan = default_body_plan();
        plan.instructions = "stable rules\n\nworkspace rules".to_owned();
        plan.stable_instructions_len = "stable rules".len();
        plan.compat.cache_control = CacheMarkers::Anthropic;
        let body = ChatCompletions.build_request(&plan).expect("build");
        let marker = serde_json::json!({ "type": "ephemeral" });
        assert_eq!(
            body["messages"][0]["content"],
            serde_json::json!([
                { "type": "text", "text": "stable rules", "cache_control": marker },
                { "type": "text", "text": "workspace rules", "cache_control": marker },
            ])
        );
    }

    #[test]
    fn a_plain_endpoint_gets_one_system_string() {
        let mut plan = default_body_plan();
        plan.instructions = "stable rules\n\nworkspace rules".to_owned();
        plan.stable_instructions_len = "stable rules".len();
        let body = ChatCompletions.build_request(&plan).expect("build");
        assert_eq!(
            body["messages"][0]["content"],
            "stable rules\n\nworkspace rules"
        );
    }

    #[test]
    fn cache_markers_go_on_the_system_and_newest_message() {
        let body = shaped(|compat| compat.cache_control = CacheMarkers::Anthropic);
        let marker = serde_json::json!({ "type": "ephemeral" });
        let messages = body["messages"].as_array().expect("array");
        assert_eq!(
            messages[0]["content"],
            serde_json::json!([{ "type": "text", "text": "be helpful", "cache_control": marker }])
        );
        assert_eq!(messages[1]["content"], "start");
        assert!(messages[2]["content"].is_null());
        assert_eq!(messages[3]["content"], "contents");
        assert_eq!(
            messages[4]["content"],
            serde_json::json!([{ "type": "text", "text": "thanks", "cache_control": marker }])
        );

        // An array body is marked on its last text part.
        let mut plan = default_body_plan();
        plan.compat.cache_control = CacheMarkers::Anthropic;
        plan.messages.push(Message::user_parts(vec![
            ContentPart::Text {
                text: "first".to_owned(),
            },
            image(7),
            ContentPart::Text {
                text: "second".to_owned(),
            },
        ]));
        let body = ChatCompletions.build_request(&plan).expect("build");
        let parts = body["messages"][5]["content"].as_array().expect("parts");
        assert!(parts[0].get("cache_control").is_none());
        assert_eq!(parts[2]["text"], "second");
        assert_eq!(parts[2]["cache_control"], marker);
        assert_eq!(body["messages"][4]["content"], "thanks");
    }

    #[test]
    fn a_cache_key_is_sent_when_enabled() {
        let body = shaped(|compat| compat.prompt_cache_key = true);
        let plan = default_body_plan();
        assert_eq!(
            body["prompt_cache_key"],
            crate::provider::prefix_cache_key(&plan.instructions, &plan.tools)
        );
        let mut keyed = default_body_plan();
        keyed.compat.prompt_cache_key = true;
        keyed.cache_key = Some("session-key".to_owned());
        let body = ChatCompletions.build_request(&keyed).expect("build");
        assert_eq!(body["prompt_cache_key"], "session-key");
        assert!(shaped(|_| {}).get("prompt_cache_key").is_none());
    }

    #[test]
    fn strict_schemas_and_no_thinking_format_shape_the_body() {
        let strict = shaped(|compat| compat.strict_schema = true);
        assert_eq!(strict["tools"][0]["function"]["strict"], true);
        let silent = shaped(|compat| compat.thinking_format = ThinkingFormat::None);
        assert!(silent.get("reasoning_effort").is_none(), "{silent}");
        let other = shaped(|compat| compat.thinking_format = ThinkingFormat::DeepSeek);
        assert_eq!(other["reasoning_effort"], "high");
    }

    #[test]
    fn deepseek_effort_maps_onto_thinking_and_effort() {
        let enabled = Some(serde_json::json!({ "type": "enabled" }));
        for (effort, thinking, level) in [
            (Effort::Auto, None, None),
            (
                Effort::None,
                Some(serde_json::json!({ "type": "disabled" })),
                None,
            ),
            (Effort::Minimal, enabled.clone(), Some("low")),
            (Effort::Low, enabled.clone(), Some("low")),
            (Effort::Medium, enabled.clone(), Some("high")),
            (Effort::High, enabled.clone(), Some("high")),
            (Effort::Xhigh, enabled.clone(), Some("high")),
            (Effort::Max, enabled.clone(), Some("max")),
        ] {
            let mut plan = default_body_plan();
            plan.compat.thinking_format = ThinkingFormat::DeepSeek;
            plan.effort = effort;
            let body = ChatCompletions.build_request(&plan).expect("build");
            assert_eq!(body.get("thinking").cloned(), thinking, "{effort}");
            assert_eq!(
                body.get("reasoning_effort")
                    .and_then(serde_json::Value::as_str),
                level,
                "{effort}"
            );
        }
    }

    /// The request body for a model on a resolved provider at an effort.
    fn thinking_body(
        provider: &str,
        base: Option<&str>,
        model: &str,
        effort: Effort,
    ) -> serde_json::Value {
        let mut plan = default_body_plan();
        (plan.compat, plan.traits) = crate::compat::resolve(provider, base, model);
        plan.model = model.to_owned();
        plan.effort = effort;
        ChatCompletions.build_request(&plan).expect("build")
    }

    const ABOVE_NONE: [Effort; 6] = [
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::Xhigh,
        Effort::Max,
    ];

    #[test]
    fn glm_keeps_its_thinking_across_turns() {
        let kept = serde_json::json!({ "type": "enabled", "clear_thinking": false });
        for effort in [Effort::Auto].into_iter().chain(ABOVE_NONE) {
            let body = thinking_body("zai", None, "glm-5", effort);
            assert_eq!(body["thinking"], kept, "{effort}");
            assert!(body.get("reasoning_effort").is_none(), "{effort}");
        }
        let off = thinking_body("zai", None, "glm-5", Effort::None);
        assert_eq!(off["thinking"], serde_json::json!({ "type": "disabled" }));
    }

    #[test]
    fn glm_5_3_cannot_turn_thinking_off() {
        let kept = serde_json::json!({ "type": "enabled", "clear_thinking": false });
        for model in ["glm-5.3", "GLM-5.3-Flash"] {
            let body = thinking_body("zai", None, model, Effort::None);
            assert_eq!(body["thinking"], kept, "{model}");
        }
    }

    #[test]
    fn openrouter_effort_uses_the_reasoning_object() {
        let model = "deepseek/deepseek-flash";
        for (effort, level) in [
            (Effort::None, "none"),
            (Effort::Minimal, "minimal"),
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::Xhigh, "xhigh"),
            (Effort::Max, "max"),
        ] {
            let body = thinking_body("openrouter", None, model, effort);
            assert_eq!(
                body["reasoning"],
                serde_json::json!({ "effort": level }),
                "{effort}"
            );
            assert!(body.get("reasoning_effort").is_none(), "{effort}");
            assert!(body.get("usage").is_none(), "{effort}");
        }
        let auto = thinking_body("openrouter", None, model, Effort::Auto);
        assert!(auto.get("reasoning").is_none(), "{auto}");
    }

    #[test]
    fn minimax_asks_for_split_reasoning() {
        for (model, effort, level) in [
            ("MiniMax-M3", Effort::Auto, None),
            ("MiniMax-M3", Effort::None, None),
            ("MiniMax-M3", Effort::High, None),
            ("MiniMax-M3.1-Flash", Effort::Auto, None),
            ("MiniMax-M3.1-Flash", Effort::None, None),
            ("MiniMax-M3.1-Flash", Effort::Minimal, Some("low")),
            ("MiniMax-M3.1-Flash", Effort::Low, Some("low")),
            ("MiniMax-M3.1-Flash", Effort::Medium, Some("medium")),
            ("MiniMax-M3.1-Flash", Effort::High, Some("high")),
            ("MiniMax-M3.1-Flash", Effort::Xhigh, Some("xhigh")),
            ("MiniMax-M3.1-Flash", Effort::Max, Some("max")),
        ] {
            let body = thinking_body("minimax", None, model, effort);
            assert_eq!(body["reasoning_split"], true, "{model} {effort}");
            assert_eq!(
                body.get("reasoning_effort")
                    .and_then(serde_json::Value::as_str),
                level,
                "{model} {effort}"
            );
            assert!(body.get("thinking").is_none(), "{model} {effort}");
        }
    }

    #[test]
    fn qwen_on_dashscope_enables_thinking() {
        let base = Some("https://dashscope.aliyuncs.com/compatible-mode/v1");
        let body = |effort| thinking_body("chat_completions", base, "qwen3-coder", effort);
        assert!(body(Effort::Auto).get("enable_thinking").is_none());
        assert_eq!(body(Effort::None)["enable_thinking"], false);
        for effort in ABOVE_NONE {
            assert_eq!(body(effort)["enable_thinking"], true, "{effort}");
            assert!(body(effort).get("reasoning_effort").is_none(), "{effort}");
        }
    }

    #[test]
    fn a_chat_template_server_enables_thinking() {
        let body = |effort| thinking_body("llamacpp", None, "qwen3-coder", effort);
        assert!(body(Effort::Auto).get("chat_template_kwargs").is_none());
        assert_eq!(
            body(Effort::None)["chat_template_kwargs"],
            serde_json::json!({ "enable_thinking": false })
        );
        for effort in ABOVE_NONE {
            assert_eq!(
                body(effort)["chat_template_kwargs"],
                serde_json::json!({ "enable_thinking": true }),
                "{effort}"
            );
            assert!(body(effort).get("reasoning_effort").is_none(), "{effort}");
        }
    }

    #[test]
    fn kimi_effort_uses_three_levels() {
        let disabled = serde_json::json!({ "type": "disabled" });
        for (model, effort, level, thinking) in [
            ("kimi-k3", Effort::Auto, None, None),
            ("kimi-k3", Effort::None, None, None),
            ("kimi-k3", Effort::Minimal, Some("low"), None),
            ("kimi-k3", Effort::Low, Some("low"), None),
            ("kimi-k3", Effort::Medium, Some("high"), None),
            ("kimi-k3", Effort::High, Some("high"), None),
            ("kimi-k3", Effort::Xhigh, Some("high"), None),
            ("kimi-k3.1-pro", Effort::Max, Some("max"), None),
            ("kimi-k2.6", Effort::None, None, Some(disabled.clone())),
            ("kimi-k2.6", Effort::High, None, None),
            ("kimi-k2.7-code", Effort::None, None, None),
            ("kimi-k2.7-code", Effort::Max, None, None),
        ] {
            let body = thinking_body("moonshot", None, model, effort);
            assert_eq!(
                body.get("reasoning_effort")
                    .and_then(serde_json::Value::as_str),
                level,
                "{model} {effort}"
            );
            assert_eq!(body.get("thinking").cloned(), thinking, "{model} {effort}");
        }
    }

    #[test]
    fn a_kimi_model_on_an_unknown_endpoint_keeps_the_passthrough() {
        let base = Some("https://gateway.example/v1");
        let body = thinking_body("chat_completions", base, "kimi-k3", Effort::Medium);
        assert_eq!(body["reasoning_effort"], "medium", "{body}");
    }

    #[test]
    fn kimi_details_merge_by_index() {
        let merged = merge_details(&[
            serde_json::json!({ "type": "reasoning.summary", "summary": "Read ", "id": "a", "index": 0 }),
            serde_json::json!({ "type": "reasoning.encrypted", "data": "x" }),
            serde_json::json!({ "type": "reasoning.summary", "summary": "first", "id": "b", "index": 0 }),
            serde_json::json!({ "type": "reasoning.text", "text": "one", "index": 1 }),
            serde_json::json!({ "type": "reasoning.encrypted", "data": "y" }),
            serde_json::json!({ "type": "reasoning.text", "text": " two", "index": 1 }),
        ]);
        assert_eq!(
            merged,
            vec![
                serde_json::json!({ "type": "reasoning.summary", "summary": "Read first", "id": "a" }),
                serde_json::json!({ "type": "reasoning.encrypted", "data": "x" }),
                serde_json::json!({ "type": "reasoning.text", "text": "one two" }),
                serde_json::json!({ "type": "reasoning.encrypted", "data": "y" }),
            ]
        );
    }

    #[test]
    fn deepseek_cache_hits_count_as_cached_input() {
        let usage = parse_usage(&serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "prompt_cache_hit_tokens": 70,
            "prompt_cache_miss_tokens": 30,
        }));
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.cache_read_tokens, Some(70));
        let both = parse_usage(&serde_json::json!({
            "prompt_tokens": 100,
            "prompt_tokens_details": { "cached_tokens": 60 },
            "prompt_cache_hit_tokens": 70,
        }));
        assert_eq!(both.cache_read_tokens, Some(60));
    }
}
