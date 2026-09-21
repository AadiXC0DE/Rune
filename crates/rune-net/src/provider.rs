//! Provider dialect contract.
//!
//! A dialect owns everything specific to one endpoint shape: how a request body
//! is written, how a stream frame is reduced, and how the conversation is
//! projected onto that wire format. It owns nothing else. Transport, retries,
//! credentials, and product state live elsewhere, so adding a dialect cannot
//! disturb the agent loop.

use rune_core::config::Effort;
use rune_core::error::Result;

use crate::message::{Message, ToolSpec};
use crate::stream::{Limit, StreamReducer};

/// What the model should do with tools on this request.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ToolChoice {
    /// The model decides.
    #[default]
    Auto,
    /// Do not call tools.
    None,
    /// The model must call a tool.
    Required,
}

impl ToolChoice {
    /// Returns the wire representation shared by the dialects that use it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Required => "required",
        }
    }
}

/// A fully specified request, ready to serialize.
#[derive(Clone, Debug)]
pub struct RequestPlan {
    /// Model identifier.
    pub model: String,
    /// System instructions, kept separate because some dialects place them
    /// outside the message list.
    pub instructions: String,
    /// Conversation messages, excluding the system lane.
    pub messages: Vec<Message>,
    /// Advertised tools.
    pub tools: Vec<ToolSpec>,
    /// Tool choice for this request.
    pub tool_choice: ToolChoice,
    /// Whether the model may call several tools at once.
    pub parallel_tool_calls: bool,
    /// Reasoning effort, when the model supports it.
    pub effort: Effort,
    /// Whether fast mode is requested.
    pub fast_mode: bool,
    /// Output token ceiling, when one applies.
    pub max_output_tokens: Option<u64>,
    /// Ordered upstream provider preference, applied where the endpoint supports it.
    pub provider_order: Vec<String>,
    /// Whether the request is restricted to the listed providers.
    pub provider_strict: bool,
}

impl RequestPlan {
    /// Builds a plan with the defaults a fresh request uses.
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            instructions: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: true,
            effort: Effort::Auto,
            fast_mode: false,
            max_output_tokens: None,
            provider_order: Vec::new(),
            provider_strict: false,
        }
    }

    /// Returns true when the request advertises tools.
    #[must_use]
    pub fn has_tools(&self) -> bool {
        !self.tools.is_empty()
    }
}

/// A dialect.
pub trait Provider: Send + Sync {
    /// Short name, used in diagnostics and in the model key.
    fn name(&self) -> &'static str;

    /// Serializes a request body.
    fn build_request(&self, plan: &RequestPlan) -> Result<serde_json::Value>;

    /// Returns the path appended to the endpoint base URL.
    fn request_path(&self) -> &'static str;

    /// Creates a reducer for one response.
    fn reducer(&self) -> Box<dyn StreamReducer>;

    /// Returns the request headers a dialect requires beyond the defaults.
    ///
    /// The credential is passed separately so a dialect never handles it.
    fn extra_headers(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }

    /// Returns an error when the plan cannot be expressed in this dialect.
    fn validate(&self, plan: &RequestPlan) -> Result<()> {
        if plan.model.trim().is_empty() {
            return Err(rune_core::error::RuneError::missing_field("model"));
        }
        crate::message::validate(&plan.messages)?;
        crate::message::validate_tools(&plan.tools)?;
        Ok(())
    }

    /// Stream limits for this dialect.
    fn limits(&self) -> Limit {
        Limit::default()
    }
}

/// Formats a plan for display in a trace, with the message text summarized.
///
/// Used by the transport when it records a failed request, so a diagnostic never
/// contains a full conversation.
#[must_use]
pub fn summarize_plan(plan: &RequestPlan) -> serde_json::Value {
    serde_json::json!({
        "model": plan.model,
        "messages": plan.messages.len(),
        "tools": plan.tools.len(),
        "tool_choice": plan.tool_choice.as_str(),
        "instructions_bytes": plan.instructions.len(),
        "max_output_tokens": plan.max_output_tokens,
        "effort": plan.effort.as_str(),
        "fast_mode": plan.fast_mode,
        "provider_order": plan.provider_order.len(),
        "provider_strict": plan.provider_strict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal dialect used to prove the trait is implementable outside this
    /// crate, which is the property that lets a third party add one.
    struct FakeDialect;

    struct FakeReducer {
        finished: bool,
    }

    impl StreamReducer for FakeReducer {
        fn apply(
            &mut self,
            payload: Option<&str>,
            out: &mut Vec<crate::stream::ProviderEvent>,
        ) -> Result<()> {
            match payload {
                Some("done") => {
                    self.finished = true;
                    out.push(crate::stream::ProviderEvent::Finish {
                        reason: crate::stream::FinishReason::Stop,
                    });
                    Ok(())
                }
                Some(text) => {
                    out.push(crate::stream::ProviderEvent::TextDelta {
                        delta: text.to_owned(),
                    });
                    Ok(())
                }
                None => Ok(()),
            }
        }

        fn is_finished(&self) -> bool {
            self.finished
        }

        fn usage(&self) -> crate::stream::Usage {
            crate::stream::Usage::default()
        }

        fn finish(&self) -> Result<crate::stream::FinishReason> {
            if self.finished {
                Ok(crate::stream::FinishReason::Stop)
            } else {
                Err(crate::stream::incomplete_stream())
            }
        }
    }

    impl Provider for FakeDialect {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn build_request(&self, plan: &RequestPlan) -> Result<serde_json::Value> {
            Ok(serde_json::json!({ "model": plan.model }))
        }

        fn request_path(&self) -> &'static str {
            "/v1/fake"
        }

        fn reducer(&self) -> Box<dyn StreamReducer> {
            Box::new(FakeReducer { finished: false })
        }
    }

    #[test]
    fn the_trait_is_object_safe() {
        let providers: Vec<Box<dyn Provider>> = vec![Box::new(FakeDialect)];
        assert_eq!(providers.len(), 1);
    }

    #[test]
    fn a_dialect_rejects_a_plan_without_a_model() {
        let dialect = FakeDialect;
        let plan = RequestPlan::new("");
        let err = dialect.validate(&plan).expect_err("rejected");
        assert_eq!(err.code(), rune_core::error::ErrorCode::MissingField);
    }

    #[test]
    fn a_dialect_validates_the_history_it_receives() {
        let dialect = FakeDialect;
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![
            Message::system("one"),
            Message::user("hi"),
            Message::system("late"),
        ];
        let err = dialect.validate(&plan).expect_err("rejected");
        assert_eq!(err.detail().invariant.as_deref(), Some("system_first"));
    }

    #[test]
    fn a_dialect_rejects_a_duplicate_tool_advertisement() {
        let dialect = FakeDialect;
        let mut plan = RequestPlan::new("m");
        let spec = ToolSpec {
            name: "read_file".to_owned(),
            description: "d".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        };
        plan.tools = vec![spec.clone(), spec];
        assert!(dialect.validate(&plan).is_err());
    }

    #[test]
    fn a_truncated_stream_is_an_error_not_a_success() {
        let mut reducer = FakeReducer { finished: false };
        let mut events = Vec::new();
        reducer.apply(Some("hello"), &mut events).expect("apply");
        assert_eq!(events.len(), 1);

        let err = reducer.finish().expect_err("truncated");
        assert_eq!(err.code(), rune_core::error::ErrorCode::IncompleteStream);
    }

    #[test]
    fn a_completed_stream_reports_its_reason() {
        let mut reducer = FakeReducer { finished: false };
        let mut events = Vec::new();
        reducer.apply(Some("done"), &mut events).expect("apply");
        assert!(reducer.is_finished());
        assert_eq!(
            reducer.finish().expect("finished"),
            crate::stream::FinishReason::Stop
        );
    }

    #[test]
    fn default_headers_and_limits_are_empty_and_bounded() {
        let dialect = FakeDialect;
        assert!(dialect.extra_headers().is_empty());
        assert_eq!(dialect.limits().max_events, Limit::default().max_events);
    }

    #[test]
    fn tool_choice_renders_the_shared_spelling() {
        assert_eq!(ToolChoice::Auto.as_str(), "auto");
        assert_eq!(ToolChoice::None.as_str(), "none");
        assert_eq!(ToolChoice::Required.as_str(), "required");
    }

    #[test]
    fn summarizing_a_plan_omits_the_conversation() {
        let mut plan = RequestPlan::new("m");
        plan.instructions = "long instructions".to_owned();
        plan.messages = vec![Message::user("secret text")];
        let summary = summarize_plan(&plan);
        let rendered = summary.to_string();
        assert!(!rendered.contains("secret text"));
        assert_eq!(summary["messages"], 1);
        assert_eq!(summary["instructions_bytes"], 17);
    }
}
