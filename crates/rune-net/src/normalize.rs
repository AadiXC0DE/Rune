//! Puts a conversation into a shape every dialect accepts before a request is
//! built.
//!
//! A history can hold things an endpoint rejects with a bad request: reasoning
//! written by a different model, an image for a model that cannot see, a call
//! whose run was interrupted before it had a result, or an assistant turn that
//! ended with nothing in it. Each is repaired here, on the request only; the
//! history keeps what was recorded.

use std::borrow::Cow;

use crate::compat::Images;
use crate::message::{ContentPart, Message, Role};
use crate::provider::RequestPlan;

/// Text sent in place of an image to a model that does not accept images.
pub const IMAGE_OMITTED: &str = "[image omitted: this model does not accept images]";

/// Result given to a call that has none because its run was interrupted.
pub const NOT_RUN: &str = "not run: the previous run was interrupted";

const ANTHROPIC: &str = "anthropic";

/// Returns the plan to send to `dialect`, borrowed when nothing changes.
#[must_use]
pub fn for_request<'a>(plan: &'a RequestPlan, dialect: &str) -> Cow<'a, RequestPlan> {
    let anthropic = dialect == ANTHROPIC;
    let mut messages: Cow<'_, [Message]> = Cow::Borrowed(&plan.messages);
    let steps: [Step; 5] = [
        drop_foreign_reasoning,
        omit_images,
        answer_orphaned_calls,
        drop_empty_claude_content,
        drop_empty_turns,
    ];
    for step in steps {
        if let Some(next) = step(&messages, plan, anthropic) {
            messages = Cow::Owned(next);
        }
    }
    match messages {
        Cow::Borrowed(_) => Cow::Borrowed(plan),
        Cow::Owned(messages) => {
            let mut owned = plan.clone();
            owned.messages = messages;
            Cow::Owned(owned)
        }
    }
}

/// One repair: `None` when the messages need no change.
type Step = fn(&[Message], &RequestPlan, bool) -> Option<Vec<Message>>;

/// Whether reasoning stored on `message` was written by another model.
fn is_foreign(message: &Message, plan: &RequestPlan, anthropic: bool) -> bool {
    let Some(origin) = message.origin.as_deref() else {
        return false;
    };
    if message.role != Role::Assistant || plan.origin.is_empty() || origin == plan.origin {
        return false;
    }
    // Thinking is kept across Claude models; the endpoint drops what the new
    // model cannot read.
    let claude = |o: &str| o.starts_with("anthropic:");
    !(anthropic && claude(origin) && claude(&plan.origin))
}

fn drop_foreign_reasoning(
    messages: &[Message],
    plan: &RequestPlan,
    anthropic: bool,
) -> Option<Vec<Message>> {
    let carries = |message: &Message| {
        message.replay.is_some()
            || message
                .parts
                .iter()
                .any(|part| matches!(part, ContentPart::Reasoning { .. }))
    };
    if !messages
        .iter()
        .any(|message| carries(message) && is_foreign(message, plan, anthropic))
    {
        return None;
    }
    Some(
        messages
            .iter()
            .map(|message| {
                let mut message = message.clone();
                if is_foreign(&message, plan, anthropic) {
                    message.replay = None;
                    message
                        .parts
                        .retain(|part| !matches!(part, ContentPart::Reasoning { .. }));
                }
                message
            })
            .collect(),
    )
}

fn omit_images(messages: &[Message], plan: &RequestPlan, _: bool) -> Option<Vec<Message>> {
    let image = |part: &ContentPart| matches!(part, ContentPart::Image { .. });
    if plan.traits.images != Images::No
        || !messages
            .iter()
            .any(|message| message.parts.iter().any(image))
    {
        return None;
    }
    Some(
        messages
            .iter()
            .map(|message| {
                let mut message = message.clone();
                for part in &mut message.parts {
                    if image(part) {
                        *part = ContentPart::Text {
                            text: IMAGE_OMITTED.to_owned(),
                        };
                    }
                }
                message
            })
            .collect(),
    )
}

/// The calls of the assistant message at `index` that the tool messages right
/// after it do not answer, in call order.
fn unanswered(messages: &[Message], index: usize) -> Vec<(&ContentPart, usize)> {
    let Some(message) = messages.get(index) else {
        return Vec::new();
    };
    if message.role != Role::Assistant {
        return Vec::new();
    }
    let answered: Vec<&str> = messages
        .iter()
        .skip(index.saturating_add(1))
        .take_while(|next| next.role == Role::Tool)
        .flat_map(|next| next.parts.iter())
        .filter_map(|part| match part {
            ContentPart::ToolResult { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    message
        .parts
        .iter()
        .filter(|part| matches!(part, ContentPart::ToolCall { .. }))
        .enumerate()
        .filter(|(_, part)| {
            part.tool_call_id()
                .is_some_and(|id| !answered.contains(&id.as_str()))
        })
        .map(|(order, part)| (part, order))
        .collect()
}

fn answer_orphaned_calls(messages: &[Message], _: &RequestPlan, _: bool) -> Option<Vec<Message>> {
    if (0..messages.len()).all(|index| unanswered(messages, index).is_empty()) {
        return None;
    }
    let mut out: Vec<Message> = Vec::with_capacity(messages.len().saturating_add(1));
    let mut index = 0;
    while let Some(message) = messages.get(index) {
        out.push(message.clone());
        let missing = unanswered(messages, index);
        index = index.saturating_add(1);
        if missing.is_empty() {
            continue;
        }
        let order_of = |id: &str| {
            message
                .tool_calls()
                .iter()
                .position(|(call, _, _)| call.as_str() == id)
        };
        // The answering tool messages are copied, and the last one receives
        // the missing results, each placed before the first later call's.
        let mut answer = Message {
            role: Role::Tool,
            parts: Vec::new(),
            replay: None,
            origin: None,
        };
        while let Some(next) = messages.get(index).filter(|next| next.role == Role::Tool) {
            out.push(next.clone());
            index = index.saturating_add(1);
        }
        let target = match out.last_mut() {
            Some(last) if last.role == Role::Tool => last,
            _ => &mut answer,
        };
        for (part, order) in missing {
            let ContentPart::ToolCall { id, name, .. } = part else {
                continue;
            };
            let at = target
                .parts
                .iter()
                .position(|held| match held {
                    ContentPart::ToolResult { id, .. } => {
                        order_of(id.as_str()).is_some_and(|other| other > order)
                    }
                    _ => false,
                })
                .unwrap_or(target.parts.len());
            target.parts.insert(
                at,
                ContentPart::ToolResult {
                    id: id.clone(),
                    name: name.clone(),
                    content: NOT_RUN.to_owned(),
                    is_error: true,
                },
            );
        }
        if !answer.parts.is_empty() {
            out.push(answer);
        }
    }
    Some(out)
}

/// Whether a stored Claude block is a thinking block with nothing in it.
fn is_empty_thinking(block: &serde_json::Value) -> bool {
    let blank = |key: &str| {
        block
            .get(key)
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
    };
    block.get("type").and_then(serde_json::Value::as_str) == Some("thinking")
        && blank("thinking")
        && blank("signature")
}

/// Whether a stored replay holds an empty thinking block.
fn has_empty_thinking(replay: &str) -> bool {
    // Stored replays are compact JSON, so the text test skips the parse for
    // every replay without an empty field.
    replay.contains(r#""thinking":"""#)
        && serde_json::from_str::<Vec<serde_json::Value>>(replay)
            .is_ok_and(|blocks| blocks.iter().any(is_empty_thinking))
}

/// The replay without its empty thinking blocks; `None` when nothing is left.
fn without_empty_thinking(replay: &str) -> Option<String> {
    let blocks: Vec<serde_json::Value> = serde_json::from_str(replay).ok()?;
    let kept: Vec<serde_json::Value> = blocks
        .into_iter()
        .filter(|block| !is_empty_thinking(block))
        .collect();
    if kept.is_empty() {
        return None;
    }
    serde_json::to_string(&kept).ok()
}

fn drop_empty_claude_content(
    messages: &[Message],
    _: &RequestPlan,
    anthropic: bool,
) -> Option<Vec<Message>> {
    let empty_text =
        |part: &ContentPart| matches!(part, ContentPart::Text { text } if text.is_empty());
    let needs = |message: &Message| {
        message.parts.iter().any(empty_text)
            || message.replay.as_deref().is_some_and(has_empty_thinking)
    };
    if !anthropic || !messages.iter().any(needs) {
        return None;
    }
    Some(
        messages
            .iter()
            .map(|message| {
                let mut message = message.clone();
                message.parts.retain(|part| !empty_text(part));
                if message.replay.as_deref().is_some_and(has_empty_thinking) {
                    message.replay = message.replay.as_deref().and_then(without_empty_thinking);
                }
                message
            })
            .collect(),
    )
}

fn drop_empty_turns(
    messages: &[Message],
    _: &RequestPlan,
    anthropic: bool,
) -> Option<Vec<Message>> {
    let empty = |message: &Message| {
        message.role == Role::Assistant
            && !(anthropic && message.replay.is_some())
            && !message.parts.iter().any(|part| match part {
                ContentPart::Text { text } => !text.is_empty(),
                ContentPart::ToolCall { .. } => true,
                _ => false,
            })
    };
    if !messages.iter().any(empty) {
        return None;
    }
    Some(
        messages
            .iter()
            .filter(|message| !empty(message))
            .cloned()
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::Anthropic;
    use crate::chat_completions::ChatCompletions;
    use crate::message::ImageRef;
    use crate::provider::Provider;
    use rune_core::id::ToolCallId;

    fn call(id: &str) -> ContentPart {
        ContentPart::ToolCall {
            id: ToolCallId::new(id).expect("id"),
            name: "read_file".to_owned(),
            arguments: "{}".to_owned(),
        }
    }

    fn result(id: &str) -> ContentPart {
        ContentPart::ToolResult {
            id: ToolCallId::new(id).expect("id"),
            name: "read_file".to_owned(),
            content: "ok".to_owned(),
            is_error: false,
        }
    }

    fn text(text: &str) -> ContentPart {
        ContentPart::Text {
            text: text.to_owned(),
        }
    }

    fn tool(parts: Vec<ContentPart>) -> Message {
        Message {
            role: Role::Tool,
            parts,
            replay: None,
            origin: None,
        }
    }

    fn reasoned(origin: &str, replay: &str) -> Message {
        let mut message = Message::assistant(vec![
            ContentPart::Reasoning {
                text: "thought".to_owned(),
            },
            text("answer"),
        ]);
        message.replay = Some(replay.to_owned());
        message.origin = Some(origin.to_owned());
        message
    }

    fn plan(origin: &str, messages: Vec<Message>) -> RequestPlan {
        let model = origin.split_once(':').map_or(origin, |(_, model)| model);
        let mut plan = RequestPlan::new(model);
        plan.origin = origin.to_owned();
        plan.messages = messages;
        plan
    }

    const THINKING: &str = r#"[{"type":"thinking","thinking":"t","signature":"s"}]"#;

    #[test]
    fn reasoning_from_another_model_is_not_replayed() {
        let mut plan = plan(
            "chat_completions:kimi-k3",
            vec![
                Message::user("hi"),
                reasoned(
                    "chat_completions:deepseek-flash",
                    r#"{"reasoning_content":"x"}"#,
                ),
                Message::user("next"),
            ],
        );
        plan.tools = vec![crate::message::ToolSpec {
            name: "read_file".to_owned(),
            description: "d".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        plan.compat.reasoning_always = true;
        plan.compat.reasoning_replay = crate::compat::ReasoningReplay::ReasoningContent;
        let sent = for_request(&plan, "chat_completions");
        let message = &sent.messages[1];
        assert!(message.replay.is_none());
        assert_eq!(message.parts, vec![text("answer")]);
        let body = ChatCompletions.build_request(&sent).expect("build");
        assert_eq!(body["messages"][1]["reasoning_content"], "");
    }

    #[test]
    fn claude_thinking_survives_a_claude_model_switch() {
        let plan = plan(
            "anthropic:claude-opus-5-5",
            vec![
                Message::user("hi"),
                reasoned("anthropic:claude-opus-5", THINKING),
                Message::user("next"),
            ],
        );
        let sent = for_request(&plan, "anthropic");
        assert!(matches!(sent, Cow::Borrowed(_)));
        let body = Anthropic.build_request(&sent).expect("build");
        assert_eq!(body["messages"][1]["content"][0]["type"], "thinking");
    }

    #[test]
    fn a_dialect_change_drops_thinking() {
        let plan = plan(
            "chat_completions:anthropic/claude-opus-5-5",
            vec![
                Message::user("hi"),
                reasoned("anthropic:claude-opus-5-5", THINKING),
                Message::user("next"),
            ],
        );
        let sent = for_request(&plan, "chat_completions");
        assert!(sent.messages[1].replay.is_none());
        assert_eq!(sent.messages[1].parts, vec![text("answer")]);
    }

    #[test]
    fn an_interrupted_call_gets_a_failed_result() {
        let messages = vec![
            Message::user("read two"),
            Message::assistant(vec![call("c1"), call("c2")]),
            tool(vec![result("c1")]),
            Message::user("go on"),
        ];
        for dialect in [Anthropic.name(), ChatCompletions.name()] {
            let plan = plan(&format!("{dialect}:m"), messages.clone());
            crate::message::validate(&plan.messages).expect_err("orphaned");
            let sent = for_request(&plan, dialect);
            crate::message::validate(&sent.messages).expect("answered");
            assert_eq!(sent.messages.len(), 4);
            let ContentPart::ToolResult {
                id,
                content,
                is_error,
                ..
            } = &sent.messages[2].parts[1]
            else {
                panic!("a result");
            };
            assert_eq!(
                (id.as_str(), content.as_str(), *is_error),
                ("c2", NOT_RUN, true)
            );
        }

        let plan = plan(
            "anthropic:m",
            vec![
                Message::user("read"),
                Message::assistant(vec![call("c1")]),
                Message::user("again"),
            ],
        );
        let sent = for_request(&plan, "anthropic");
        assert_eq!(sent.messages[2].role, Role::Tool);
        crate::message::validate(&sent.messages).expect("answered");
    }

    #[test]
    fn an_empty_assistant_turn_is_dropped() {
        let plan = plan(
            "anthropic:m",
            vec![
                Message::user("one"),
                Message::assistant(vec![text("")]),
                Message::user("two"),
            ],
        );
        let sent = for_request(&plan, "anthropic");
        assert_eq!(sent.messages.len(), 2);
        let body = Anthropic.build_request(&sent).expect("build");
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"].as_array().expect("blocks").len(), 2);

        let mut kept = Message::assistant(vec![text("")]);
        kept.replay = Some(r#"[{"type":"thinking","thinking":"","signature":""}]"#.to_owned());
        let plan = plan_with(vec![Message::user("one"), kept, Message::user("two")]);
        let sent = for_request(&plan, "anthropic");
        assert_eq!(
            sent.messages.len(),
            2,
            "an empty thinking block does not keep it"
        );
    }

    fn plan_with(messages: Vec<Message>) -> RequestPlan {
        plan("anthropic:m", messages)
    }

    #[test]
    fn claude_call_ids_are_made_safe_on_both_sides() {
        let plan = plan_with(vec![
            Message::user("read"),
            Message::assistant(vec![call("call:abc/1")]),
            tool(vec![result("call:abc/1")]),
        ]);
        let body = Anthropic
            .build_request(&for_request(&plan, "anthropic"))
            .expect("build");
        assert_eq!(body["messages"][1]["content"][0]["id"], "call_abc_1");
        assert_eq!(
            body["messages"][2]["content"][0]["tool_use_id"],
            "call_abc_1"
        );
        assert!(
            plan.messages[1]
                .tool_calls()
                .iter()
                .any(|(id, _, _)| id.as_str() == "call:abc/1")
        );
    }

    #[test]
    fn an_image_is_omitted_for_a_blind_model() {
        let image = ContentPart::Image {
            image: ImageRef {
                id: 1,
                media_type: "image/png".to_owned(),
                encoded_bytes: 4,
            },
        };
        let mut plan = plan(
            "chat_completions:deepseek-flash",
            vec![Message::user_parts(vec![text("look"), image.clone()])],
        );
        plan.traits.images = Images::No;
        let sent = for_request(&plan, "chat_completions");
        assert_eq!(sent.messages[0].parts[1], text(IMAGE_OMITTED));

        plan.traits.images = Images::Unknown;
        let sent = for_request(&plan, "chat_completions");
        assert!(matches!(sent, Cow::Borrowed(_)));
        assert_eq!(sent.messages[0].parts[1], image);
    }

    #[test]
    fn an_unchanged_plan_is_borrowed() {
        let plan = plan(
            "anthropic:claude-opus-5-5",
            vec![
                Message::user("read"),
                reasoned("anthropic:claude-opus-5-5", THINKING),
                Message::user("next"),
                Message::assistant(vec![call("toolu_1")]),
                tool(vec![result("toolu_1")]),
            ],
        );
        for dialect in ["anthropic", "chat_completions", "responses", "gemini"] {
            assert!(
                matches!(for_request(&plan, dialect), Cow::Borrowed(_)),
                "{dialect}"
            );
        }
        let mut old = plan.clone();
        old.messages[1].origin = None;
        assert!(matches!(
            for_request(&old, "chat_completions"),
            Cow::Borrowed(_)
        ));
    }
}
