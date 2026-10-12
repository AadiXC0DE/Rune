//! Prompt validation and turn-scoped inline images.

use std::collections::BTreeMap;
use std::sync::Arc;

use rune_agent::history::History;
use rune_core::error::{Result, RuneError};
use rune_net::message::{ContentPart, ImageRef, Message};
use rune_net::provider::{Provider, RequestPlan};
use serde_json::Value;

use crate::jsonrpc::{MAX_FRAME_BYTES, RpcError};

const MAX_IMAGES: usize = 16;

/// Image bytes live only for this prompt, including retries and tool steps.
#[derive(Default)]
pub(crate) struct Prompt {
    pub text: String,
    parts: Vec<ContentPart>,
    data: BTreeMap<u64, Arc<str>>,
}

impl Prompt {
    pub fn parse(params: &Value, provider: &dyn Provider) -> std::result::Result<Self, RpcError> {
        let blocks = params
            .get("prompt")
            .ok_or_else(|| RpcError::from_rune(&RuneError::missing_field("prompt")))?;
        let blocks = blocks.as_array().ok_or_else(|| {
            RpcError::from_rune(&RuneError::invalid_field(
                "prompt",
                "must be an array of content blocks",
            ))
        })?;
        let mut prompt = Self::default();
        let mut encoded_bytes = 0_usize;
        for (index, block) in blocks.iter().enumerate() {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).ok_or_else(|| {
                        RpcError::invalid_params(format!("`prompt[{index}].text` must be a string"))
                    })?;
                    prompt.push_text(text);
                }
                Some("resource") => {
                    if let Some(text) = block
                        .get("resource")
                        .and_then(|resource| resource.get("text"))
                        .and_then(Value::as_str)
                    {
                        prompt.push_text(text);
                    }
                }
                Some("image") => {
                    // Use the same capability validation as actual provider requests.
                    // Refuse unsupported dialects before copying any image bytes.
                    let mut plan = RequestPlan::new("");
                    plan.messages
                        .push(Message::user_parts(vec![ContentPart::Image {
                            image: ImageRef {
                                id: 0,
                                media_type: String::new(),
                                encoded_bytes: 0,
                            },
                        }]));
                    provider
                        .capabilities()
                        .validate(provider.name(), &plan)
                        .map_err(|err| RpcError::from_rune(&err))?;
                    if prompt.data.len() >= MAX_IMAGES {
                        return Err(RpcError::from_rune(&RuneError::invalid_field(
                            "prompt",
                            "at most 16 image blocks are accepted",
                        )));
                    }
                    let field = |name: &str| -> std::result::Result<&str, RpcError> {
                        block.get(name).and_then(Value::as_str).ok_or_else(|| {
                            RpcError::from_rune(&RuneError::invalid_field(
                                format!("prompt[{index}].{name}"),
                                "must be a string",
                            ))
                        })
                    };
                    let mime = field("mimeType")?;
                    if !matches!(
                        mime,
                        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                    ) {
                        return Err(RpcError::from_rune(&RuneError::invalid_field(
                            format!("prompt[{index}].mimeType"),
                            "must be image/png, image/jpeg, image/gif, or image/webp",
                        )));
                    }
                    let data = field("data")?;
                    encoded_bytes = encoded_bytes.saturating_add(data.len());
                    if encoded_bytes > MAX_FRAME_BYTES {
                        return Err(RpcError::from_rune(&RuneError::too_large(
                            "prompt images",
                            encoded_bytes,
                            MAX_FRAME_BYTES,
                        )));
                    }
                    let size = base64_size(data).map_err(|err| RpcError::from_rune(&err))?;
                    let id = (prompt.data.len() as u64).saturating_add(1);
                    prompt.parts.push(ContentPart::Image {
                        image: ImageRef {
                            id,
                            media_type: mime.to_owned(),
                            encoded_bytes: size as u64,
                        },
                    });
                    prompt.data.insert(id, Arc::from(data));
                }
                Some(other) => {
                    return Err(RpcError::invalid_params(format!(
                        "this server does not accept `{other}` content in a prompt"
                    )));
                }
                None => {
                    return Err(RpcError::invalid_params(
                        "a prompt block must declare its type",
                    ));
                }
            }
        }
        if prompt.text.trim().is_empty() {
            if prompt.data.is_empty() {
                return Err(RpcError::from_rune(&RuneError::invalid_field(
                    "prompt",
                    "must contain some text",
                )));
            }
            "[Image attached]".clone_into(&mut prompt.text);
        }
        Ok(prompt)
    }

    fn push_text(&mut self, text: &str) {
        if !self.text.is_empty() {
            self.text.push_str("\n\n");
        }
        self.text.push_str(text);
        self.parts.push(ContentPart::Text {
            text: text.to_owned(),
        });
    }

    /// Project attachments onto the originating user turn without retaining
    /// image references or bytes in session history. Later prompts cannot reuse IDs.
    pub fn hydrate(&self, seq: u64, history: &History, plan: &mut RequestPlan) {
        if self.data.is_empty() {
            return;
        }
        // The pinned constraint message precedes every user turn, so pairing
        // from the end preserves user sequence positions when it is present.
        for (turn, message) in history
            .turns()
            .iter()
            .rev()
            .zip(plan.messages.iter_mut().rev())
        {
            if turn.seq == seq {
                message.parts.clone_from(&self.parts);
                plan.image_data.extend(self.data.clone());
                break;
            }
        }
    }
}

/// Validate canonical, padded standard base64 without a decoding allocation.
fn base64_size(data: &str) -> Result<usize> {
    let invalid = || {
        RuneError::invalid_field(
            "image.data",
            "must be nonempty standard base64 with valid padding",
        )
    };
    if data.is_empty() || !data.len().is_multiple_of(4) {
        return Err(invalid());
    }
    let padding = data.bytes().rev().take_while(|byte| *byte == b'=').count();
    if padding > 2 {
        return Err(invalid());
    }
    let mut last = 0;
    for byte in data.bytes().take(data.len().saturating_sub(padding)) {
        last = match byte {
            b'A'..=b'Z' => byte.saturating_sub(b'A'),
            b'a'..=b'z' => byte.saturating_sub(b'a').saturating_add(26),
            b'0'..=b'9' => byte.saturating_sub(b'0').saturating_add(52),
            b'+' => 62,
            b'/' => 63,
            _ => return Err(invalid()),
        };
    }
    if (padding == 1 && last & 3 != 0) || (padding == 2 && last & 15 != 0) {
        return Err(invalid());
    }
    Ok((data.len() / 4).saturating_mul(3).saturating_sub(padding))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_net::chat_completions::ChatCompletions;
    use rune_net::stream::StreamReducer;
    use serde_json::json;

    struct NoImages;

    impl Provider for NoImages {
        fn name(&self) -> &'static str {
            "no_images_fixture"
        }
        fn request_path(&self) -> &'static str {
            "/unsupported"
        }
        fn build_request(&self, _plan: &RequestPlan) -> Result<Value> {
            panic!("must reject before serialization")
        }
        fn reducer(&self) -> Box<dyn StreamReducer> {
            ChatCompletions.reducer()
        }
    }

    #[test]
    fn an_unsupported_dialect_returns_a_named_error_before_reading_payloads() {
        let provider = NoImages;
        assert!(!provider.capabilities().images);
        let error = Prompt::parse(
            &json!({"prompt": [{"type": "text", "text": "valid"}, {"type": "image"}]}),
            &provider,
        )
        .err()
        .expect("unsupported");
        assert_eq!(error.data.expect("named error")["code"], "unsupported");
        assert!(error.message.contains("no_images_fixture"));
        assert!(error.message.contains("images"));
    }

    #[test]
    fn base64_validation_checks_alphabet_padding_and_trailing_bits() {
        for (data, size) in [
            ("AA==", 1),
            ("AAA=", 2),
            ("AAAA", 3),
            ("/w==", 1),
            ("//8=", 2),
        ] {
            assert_eq!(base64_size(data).expect("valid base64"), size);
        }
        for data in [
            "", "A", "AA", "AAA", "====", "A===", "AA=A", "AA==AA==", "A!==", "AB==", "AAB=",
            "AA\n=", "__8=", "é==",
        ] {
            assert!(base64_size(data).is_err(), "{data}");
        }
    }

    #[test]
    fn count_and_aggregate_encoded_bytes_are_bounded() {
        let image = json!({"type": "image", "mimeType": "image/png", "data": "AA=="});
        assert!(
            Prompt::parse(
                &json!({"prompt": vec![image.clone(); MAX_IMAGES]}),
                &ChatCompletions
            )
            .is_ok()
        );
        let error = Prompt::parse(
            &json!({"prompt": vec![image; MAX_IMAGES + 1]}),
            &ChatCompletions,
        )
        .err()
        .expect("count cap");
        assert_eq!(error.data.expect("code")["code"], "invalid_field");
        let image = json!({"type": "image", "mimeType": "image/png", "data": "A".repeat(MAX_FRAME_BYTES / 2 + 4)});
        let error = Prompt::parse(&json!({"prompt": [image.clone(), image]}), &ChatCompletions)
            .err()
            .expect("aggregate cap");
        assert_eq!(error.data.expect("code")["code"], "too_large");
    }

    #[test]
    fn hydration_targets_the_originating_turn_with_constraints_and_later_steering() {
        let prompt = Prompt::parse(
            &json!({"prompt": [{"type": "image", "mimeType": "image/png", "data": "AA=="}]}),
            &ChatCompletions,
        )
        .expect("image prompt");
        let mut history = History::new();
        history.pin_constraint("keep this").expect("pin");
        history.push_user("old");
        let seq = history.push_user(&prompt.text);
        history.push_user("later steering");
        let mut plan = RequestPlan::new("selected");
        plan.messages = history.to_messages();
        prompt.hydrate(seq, &history, &mut plan);
        assert!(matches!(
            plan.messages[2].parts[0],
            ContentPart::Image { .. }
        ));
        assert_eq!(plan.messages[3], Message::user("later steering"));
        assert_eq!(
            plan.image_data.get(&1).expect("inline bytes").as_ref(),
            "AA=="
        );
        assert!(!history.to_messages().iter().any(|message| {
            message
                .parts
                .iter()
                .any(|part| matches!(part, ContentPart::Image { .. }))
        }));
    }
}
