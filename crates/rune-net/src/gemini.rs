//! Native Gemini Generate Content requests and SSE responses.
//!
//! Wire reference: <https://ai.google.dev/api/generate-content>. Model content
//! is replayed verbatim so thought signatures stay attached to their parts.

use rune_core::config::Effort;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::ToolCallId;
use serde_json::{Value, json};

use crate::message::{ContentPart, Message, Role};
use crate::provider::{Provider, RequestPlan, ToolChoice};
use crate::stream::{
    FinishReason, Limit, ProviderEvent, StreamReducer, Usage, incomplete_stream,
    protocol_violation, stream_failure,
};

/// The native Gemini dialect, with a base URL ending in `/v1beta`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gemini;

impl Provider for Gemini {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        crate::provider::ProviderCapabilities {
            efforts: &[Effort::Auto],
            images: true,
            tool_streaming: crate::provider::ToolStreaming::CompleteCalls,
            fast_mode: false,
        }
    }

    fn request_path(&self) -> &'static str {
        "/models"
    }

    fn completion_path(&self, plan: &RequestPlan) -> Result<String> {
        let model = plan.model.strip_prefix("models/").unwrap_or(&plan.model);
        if model.is_empty()
            || !model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || matches!(model, "." | "..")
        {
            return Err(RuneError::invalid_field(
                "model",
                "invalid Gemini model identifier",
            ));
        }
        Ok(format!("/models/{model}:streamGenerateContent?alt=sse"))
    }

    fn models_path(&self) -> Option<&'static str> {
        Some("/models")
    }

    fn reducer(&self) -> Box<dyn StreamReducer> {
        Box::new(Reducer::default())
    }

    fn build_request(&self, plan: &RequestPlan) -> Result<Value> {
        self.validate(plan)?;
        self.completion_path(plan)?;
        let mut instructions = Vec::new();
        if !plan.instructions.is_empty() {
            instructions.push(json!({"text": plan.instructions}));
        }
        let mut contents: Vec<Value> = Vec::new();
        // Match tool results to native IDs, which are optional on the wire.
        let mut call_ids = std::collections::HashMap::new();
        for message in &plan.messages {
            if message.role == Role::System {
                instructions.push(json!({"text": message.text()}));
                continue;
            }
            let parts = encode_parts(message, plan, &mut call_ids)?;
            if parts.is_empty() {
                continue;
            }
            let role = if message.role == Role::Assistant {
                "model"
            } else {
                "user"
            };
            // Several tool result messages belong to one native user turn.
            if let Some(last) = contents.last_mut()
                && last["role"] == role
                && let Some(previous) = last["parts"].as_array_mut()
            {
                previous.extend(parts);
            } else {
                contents.push(json!({"role": role, "parts": parts}));
            }
        }
        let mut body = json!({"contents": contents});
        if !instructions.is_empty() {
            body["systemInstruction"] = json!({"parts": instructions});
        }
        if let Some(max) = plan.max_output_tokens {
            body["generationConfig"] = json!({"maxOutputTokens": max});
        }
        if plan.has_tools() {
            let declarations: Vec<_> = plan
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parametersJsonSchema": tool.input_schema,
                    })
                })
                .collect();
            body["tools"] = json!([{"functionDeclarations": declarations}]);
            let mode = match plan.tool_choice {
                ToolChoice::Auto => "AUTO",
                ToolChoice::None => "NONE",
                ToolChoice::Required => "ANY",
            };
            body["toolConfig"] = json!({"functionCallingConfig": {"mode": mode}});
        }
        Ok(body)
    }
}

fn encode_parts(
    message: &Message,
    plan: &RequestPlan,
    call_ids: &mut std::collections::HashMap<String, Option<String>>,
) -> Result<Vec<Value>> {
    if message.role == Role::Assistant
        && let Some(replay) = &message.replay
    {
        let replay: Value = serde_json::from_str(replay)?;
        if let Some(parts) = replay.get("gemini_parts").and_then(Value::as_array) {
            let native_calls: Vec<_> = parts
                .iter()
                .filter_map(|part| part.get("functionCall"))
                .collect();
            let calls = message.tool_calls();
            if native_calls.len() != calls.len() {
                return Err(protocol_violation(
                    "Gemini replay has mismatched tool calls",
                ));
            }
            for ((id, name, _), call) in calls.iter().zip(native_calls) {
                if call["name"] != *name {
                    return Err(protocol_violation(
                        "Gemini replay has a mismatched tool name",
                    ));
                }
                call_ids.insert(
                    id.as_str().to_owned(),
                    call.get("id").and_then(Value::as_str).map(str::to_owned),
                );
            }
            return Ok(parts.clone());
        }
    }
    let mut parts = Vec::new();
    for part in &message.parts {
        match part {
            ContentPart::Text { text } => parts.push(json!({"text": text})),
            ContentPart::Reasoning { .. } => {}
            ContentPart::ToolCall {
                id,
                name,
                arguments,
            } => {
                let args: Value = serde_json::from_str(arguments)?;
                if !args.is_object() {
                    return Err(RuneError::invalid_field(
                        "tool.arguments",
                        "Gemini function arguments must be an object",
                    ));
                }
                // Without native replay, assign a wire ID that the response
                // can match even when several calls have the same name.
                call_ids.insert(id.as_str().to_owned(), Some(id.as_str().to_owned()));
                parts
                    .push(json!({"functionCall": {"id": id.as_str(), "name": name, "args": args}}));
            }
            ContentPart::ToolResult {
                id,
                name,
                content,
                is_error,
            } => {
                let response = if *is_error {
                    json!({"error": content})
                } else {
                    json!({"output": content})
                };
                let mut call = json!({"name": name, "response": response});
                if let Some(Some(native_id)) = call_ids.get(id.as_str()) {
                    call["id"] = json!(native_id);
                }
                parts.push(json!({"functionResponse": call}));
            }
            ContentPart::Image { image } => {
                let data = plan.image_data.get(&image.id).ok_or_else(|| {
                    RuneError::new(
                        ErrorCode::Unsupported,
                        "Gemini image references require inline bytes from the image store",
                    )
                })?;
                parts.push(
                    json!({"inlineData": {"mimeType": image.media_type, "data": data.as_ref()}}),
                );
            }
        }
    }
    Ok(parts)
}

/// Reduces native `GenerateContentResponse` chunks, accepting complete calls.
#[derive(Debug, Default)]
pub struct Reducer {
    limits: Limit,
    parts: Vec<Value>,
    ids: std::collections::HashSet<String>,
    usage: Usage,
    reason: Option<FinishReason>,
    finished: bool,
    events: usize,
    total_bytes: usize,
    content_bytes: usize,
}

impl Reducer {
    /// Sets stream bounds for a host or fixture.
    #[must_use]
    pub fn with_limits(mut self, limits: Limit) -> Self {
        self.limits = limits;
        self
    }

    fn part(&mut self, part: &Value, out: &mut Vec<ProviderEvent>) -> Result<()> {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            self.content_bytes = self.content_bytes.saturating_add(text.len());
            if self.content_bytes > self.limits.max_content_bytes {
                return Err(RuneError::too_large(
                    "stream.content",
                    self.content_bytes,
                    self.limits.max_content_bytes,
                ));
            }
            if !text.is_empty() {
                out.push(if part["thought"] == true {
                    ProviderEvent::ReasoningDelta {
                        delta: text.to_owned(),
                    }
                } else {
                    ProviderEvent::TextDelta {
                        delta: text.to_owned(),
                    }
                });
            }
        }
        if let Some(call) = part.get("functionCall") {
            self.limits
                .check_tool_calls(self.ids.len().saturating_add(1))?;
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| protocol_violation("Gemini function call has no name"))?;
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            if !args.is_object() {
                return Err(protocol_violation(
                    "Gemini function arguments are not an object",
                ));
            }
            let arguments = serde_json::to_string(&args)?;
            if arguments.len() > self.limits.max_tool_arguments_bytes {
                return Err(RuneError::too_large(
                    "stream.tool_arguments",
                    arguments.len(),
                    self.limits.max_tool_arguments_bytes,
                ));
            }
            let raw_id = call.get("id").and_then(Value::as_str).map_or_else(
                || format!("gemini_call_{}", rune_core::id::SessionId::generate()),
                str::to_owned,
            );
            if !self.ids.insert(raw_id.clone()) {
                return Err(protocol_violation(
                    "Gemini repeated a function call identifier",
                ));
            }
            let id = ToolCallId::new(raw_id)?;
            out.push(ProviderEvent::ToolCallStart {
                id: id.clone(),
                name: name.to_owned(),
            });
            out.push(ProviderEvent::ToolCallDelta {
                id: id.clone(),
                delta: arguments.clone(),
            });
            out.push(ProviderEvent::ToolCallEnd { id, arguments });
        }
        self.parts.push(part.clone());
        Ok(())
    }
}

impl StreamReducer for Reducer {
    fn apply(&mut self, payload: Option<&str>, out: &mut Vec<ProviderEvent>) -> Result<()> {
        let Some(payload) = payload else {
            if !self.finished {
                let reason = self.finish()?;
                self.finished = true;
                out.push(ProviderEvent::Finish { reason });
            }
            return Ok(());
        };
        if self.finished {
            return Err(protocol_violation("Gemini frame arrived after completion"));
        }
        self.limits.check_event(payload.len())?;
        self.total_bytes = self.total_bytes.saturating_add(payload.len());
        self.limits.check_total(self.total_bytes)?;
        self.events = self.events.saturating_add(1);
        self.limits.check_events(self.events)?;
        let value: Value = serde_json::from_str(payload)
            .map_err(|error| protocol_violation(format!("invalid Gemini stream JSON: {error}")))?;
        if let Some(error) = value.get("error") {
            return Err(stream_failure(error, "Gemini reported an error"));
        }
        if let Some(usage) = value.get("usageMetadata") {
            let count = |name| usage.get(name).and_then(Value::as_u64);
            let usage = Usage {
                input_tokens: count("promptTokenCount"),
                output_tokens: count("candidatesTokenCount"),
                cache_read_tokens: count("cachedContentTokenCount"),
                cache_write_tokens: None,
                reasoning_tokens: count("thoughtsTokenCount"),
            };
            self.usage = self.usage.merge_max(usage);
            out.push(ProviderEvent::Usage(usage));
        }
        if value
            .get("promptFeedback")
            .and_then(|feedback| feedback.get("blockReason"))
            .and_then(Value::as_str)
            .is_some_and(|reason| reason != "BLOCK_REASON_UNSPECIFIED")
        {
            self.reason = Some(FinishReason::ContentFilter);
        }
        if let Some(candidates) = value.get("candidates").and_then(Value::as_array) {
            for candidate in candidates {
                if candidate.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                    continue;
                }
                if self.reason.is_some() {
                    return Err(protocol_violation(
                        "Gemini candidate arrived after its finish reason",
                    ));
                }
                if let Some(parts) = candidate
                    .get("content")
                    .and_then(|content| content.get("parts"))
                    .and_then(Value::as_array)
                {
                    for part in parts {
                        self.part(part, out)?;
                    }
                }
                if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                    self.reason = Some(match reason {
                        "STOP" if !self.ids.is_empty() => FinishReason::ToolCalls,
                        "MAX_TOKENS" => FinishReason::MaxTokens,
                        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII"
                        | "IMAGE_SAFETY" => FinishReason::ContentFilter,
                        "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" => {
                            FinishReason::ProviderError
                        }
                        "FINISH_REASON_UNSPECIFIED" => {
                            return Err(protocol_violation(
                                "Gemini reported an unspecified finish reason",
                            ));
                        }
                        _ => FinishReason::Stop,
                    });
                }
            }
        }
        Ok(())
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
    fn usage(&self) -> Usage {
        self.usage
    }
    fn replay(&self) -> Option<String> {
        if self.parts.is_empty() {
            None
        } else {
            Some(json!({"gemini_parts": self.parts}).to_string())
        }
    }
    fn finish(&self) -> Result<FinishReason> {
        self.reason.ok_or_else(incomplete_stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(reducer: &mut Reducer, frame: &Value, out: &mut Vec<ProviderEvent>) -> Result<()> {
        reducer.apply(Some(&frame.to_string()), out)
    }

    #[test]
    fn generated_call_ids_are_unique_across_responses() {
        let frame = json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "lookup", "args": {}}},
            {"functionCall": {"name": "lookup", "args": {}}}
        ]}, "finishReason": "STOP"}]});
        let mut ids = std::collections::HashSet::new();
        for _ in 0..3 {
            let mut reducer = Reducer::default();
            let mut events = Vec::new();
            feed(&mut reducer, &frame, &mut events).expect("response");
            for event in events {
                if let ProviderEvent::ToolCallStart { id, .. } = event {
                    assert!(ids.insert(id));
                }
            }
            let replay: Value =
                serde_json::from_str(&reducer.replay().expect("replay")).expect("JSON");
            assert!(
                replay["gemini_parts"][0]["functionCall"]
                    .get("id")
                    .is_none()
            );
        }
        assert_eq!(ids.len(), 6);
    }

    #[test]
    fn paths_accept_native_model_names_and_refuse_url_injection() {
        for model in ["gemini-fixture", "models/gemini-fixture"] {
            assert_eq!(
                Gemini
                    .completion_path(&RequestPlan::new(model))
                    .expect("path"),
                "/models/gemini-fixture:streamGenerateContent?alt=sse"
            );
        }
        for model in [
            "",
            "..",
            "../other",
            "model?key=secret",
            "model#fragment",
            "model/other",
            "model%2Fother",
            " model",
        ] {
            assert!(
                Gemini.build_request(&RequestPlan::new(model)).is_err(),
                "{model}"
            );
        }
    }

    #[test]
    fn native_tool_choices_and_system_lane_are_encoded() {
        let mut plan = RequestPlan::new("gemini-fixture");
        plan.instructions = "first".to_owned();
        plan.messages = vec![Message::system("second"), Message::user("hello")];
        plan.tools = vec![crate::message::ToolSpec {
            name: "lookup".to_owned(),
            description: "Look up a key".to_owned(),
            input_schema: json!({"type": "object"}),
        }];
        for (choice, mode) in [
            (ToolChoice::Auto, "AUTO"),
            (ToolChoice::None, "NONE"),
            (ToolChoice::Required, "ANY"),
        ] {
            plan.tool_choice = choice;
            let body = Gemini.build_request(&plan).expect("body");
            assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], mode);
            assert_eq!(
                body["systemInstruction"]["parts"],
                json!([{"text": "first"}, {"text": "second"}])
            );
            assert_eq!(body["contents"].as_array().expect("contents").len(), 1);
        }
        plan.tools.clear();
        let body = Gemini.build_request(&plan).expect("body without tools");
        assert!(body.get("tools").is_none());
        assert!(body.get("toolConfig").is_none());
        plan.provider_order = vec!["google".to_owned()];
        assert_eq!(
            Gemini
                .build_request(&plan)
                .expect_err("routing unsupported")
                .field(),
            Some("provider_order")
        );
    }

    #[test]
    fn unsupported_generation_controls_are_rejected() {
        let mut plan = RequestPlan::new("gemini-fixture");
        plan.effort = Effort::High;
        assert_eq!(
            Gemini
                .build_request(&plan)
                .expect_err("effort unsupported")
                .code(),
            ErrorCode::Unsupported
        );
        plan.effort = Effort::Auto;
        plan.fast_mode = true;
        assert_eq!(
            Gemini
                .build_request(&plan)
                .expect_err("speed unsupported")
                .code(),
            ErrorCode::Unsupported
        );
    }

    #[test]
    fn tool_failures_and_consecutive_results_use_native_user_content() {
        let id = ToolCallId::new("local-call").expect("id");
        let mut plan = RequestPlan::new("gemini-fixture");
        plan.messages = vec![
            Message::assistant(vec![ContentPart::ToolCall {
                id: id.clone(),
                name: "lookup".to_owned(),
                arguments: "{}".to_owned(),
            }]),
            Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult {
                    id,
                    name: "lookup".to_owned(),
                    content: "missing key".to_owned(),
                    is_error: true,
                }],
                replay: None,
            },
            Message::user("try another"),
        ];
        let body = Gemini.build_request(&plan).expect("request");
        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"]["id"],
            "local-call"
        );
        assert_eq!(body["contents"][1]["role"], "user");
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"],
            json!({"id": "local-call", "name": "lookup", "response": {"error": "missing key"}})
        );
        assert_eq!(body["contents"][1]["parts"][1]["text"], "try another");
    }

    #[test]
    fn missing_terminal_reason_is_incomplete_and_usage_absence_stays_absent() {
        let mut reducer = Reducer::default();
        let mut out = Vec::new();
        feed(
            &mut reducer,
            &json!({"candidates": [{"content": {"parts": [{"text": "partial"}]}}]}),
            &mut out,
        )
        .expect("partial");
        assert_eq!(
            reducer
                .apply(None, &mut out)
                .expect_err("incomplete")
                .code(),
            ErrorCode::IncompleteStream
        );
        assert!(reducer.usage().is_empty());
        assert!(
            !out.iter()
                .any(|event| matches!(event, ProviderEvent::Finish { .. }))
        );
    }

    #[test]
    fn reasoning_and_finish_reasons_are_normalized() {
        for (reason, expected) in [
            ("STOP", FinishReason::Stop),
            ("MAX_TOKENS", FinishReason::MaxTokens),
            ("SAFETY", FinishReason::ContentFilter),
            ("MALFORMED_FUNCTION_CALL", FinishReason::ProviderError),
        ] {
            let mut reducer = Reducer::default();
            let mut out = Vec::new();
            feed(&mut reducer, &json!({"candidates": [{"content": {"parts": [{"text": "thinking", "thought": true, "thoughtSignature": "signature"}]}, "finishReason": reason}]}), &mut out).expect("frame");
            reducer.apply(None, &mut out).expect("EOF");
            assert_eq!(reducer.finish().expect("reason"), expected);
            assert!(reducer.is_finished());
            assert!(
                matches!(out.first(), Some(ProviderEvent::ReasoningDelta { delta }) if delta == "thinking")
            );
            assert!(reducer.replay().expect("replay").contains("signature"));
            assert!(reducer.apply(Some("{}"), &mut out).is_err());
        }
        let mut reducer = Reducer::default();
        let mut out = Vec::new();
        feed(&mut reducer, &json!({"promptFeedback": {"blockReason": "SAFETY"}, "usageMetadata": {"promptTokenCount": 0}}), &mut out).expect("blocked");
        reducer.apply(None, &mut out).expect("EOF");
        assert_eq!(
            reducer.finish().expect("reason"),
            FinishReason::ContentFilter
        );
        assert_eq!(reducer.usage().input_tokens, Some(0));
        assert_eq!(reducer.usage().output_tokens, None);
    }

    #[test]
    fn malformed_frames_and_duplicate_ids_are_refused() {
        for frame in [
            "{",
            r#"{"error":{"status":"UNAVAILABLE","message":"overloaded"}}"#,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"args":{}}}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"lookup","args":[]}}]}}]}"#,
        ] {
            assert!(
                Reducer::default()
                    .apply(Some(frame), &mut Vec::new())
                    .is_err()
            );
        }
        let frame = json!({"candidates": [{"content": {"parts": [{"functionCall": {"id": "duplicate", "name": "lookup", "args": {}}}]}}]});
        let mut reducer = Reducer::default();
        feed(&mut reducer, &frame, &mut Vec::new()).expect("first call");
        assert_eq!(
            feed(&mut reducer, &frame, &mut Vec::new())
                .expect_err("duplicate")
                .code(),
            ErrorCode::ProtocolViolation
        );
    }

    #[test]
    fn all_native_stream_bounds_are_enforced() {
        let text = json!({"candidates": [{"content": {"parts": [{"text": "long text"}]}}]});
        let call = json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "lookup", "args": {"key": "alpha"}}}]}}]});
        let cases = [
            (
                Limit {
                    max_event_bytes: 1,
                    ..Limit::default()
                },
                text.clone(),
            ),
            (
                Limit {
                    max_total_bytes: 1,
                    ..Limit::default()
                },
                text.clone(),
            ),
            (
                Limit {
                    max_events: 0,
                    ..Limit::default()
                },
                text.clone(),
            ),
            (
                Limit {
                    max_content_bytes: 1,
                    ..Limit::default()
                },
                text,
            ),
            (
                Limit {
                    max_tool_calls: 0,
                    ..Limit::default()
                },
                call.clone(),
            ),
            (
                Limit {
                    max_tool_arguments_bytes: 1,
                    ..Limit::default()
                },
                call,
            ),
        ];
        for (limits, frame) in cases {
            let error = feed(
                &mut Reducer::default().with_limits(limits),
                &frame,
                &mut Vec::new(),
            )
            .expect_err("bounded");
            assert_eq!(error.code(), ErrorCode::TooLarge);
        }
    }
}
