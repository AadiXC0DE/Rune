//! Normalized stream events.
//!
//! Providers differ in framing, field names, and how they report termination.
//! They agree on nothing except that this enum is the only currency the agent
//! loop consumes, so a dialect quirk cannot reach the rest of the system.

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::ToolCallId;
use serde::{Deserialize, Serialize};

/// Why the model stopped.
///
/// Normalized across dialects. A dialect maps its own terminal label onto one of
/// these, and a label it does not recognize is a protocol violation rather than
/// a silent success.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The model finished naturally.
    Stop,
    /// The model wants tools run.
    ToolCalls,
    /// The response hit the output token limit.
    MaxTokens,
    /// A content filter stopped the response.
    ContentFilter,
    /// The turn reached its model-step limit.
    MaxModelTurns,
    /// The model declined.
    Refused,
    /// The turn was cancelled.
    Cancelled,
    /// The provider reported an error in the stream.
    ProviderError,
}

impl FinishReason {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::ToolCalls => "tool_calls",
            Self::MaxTokens => "max_tokens",
            Self::ContentFilter => "content_filter",
            Self::MaxModelTurns => "max_model_turns",
            Self::Refused => "refused",
            Self::Cancelled => "cancelled",
            Self::ProviderError => "provider_error",
        }
    }

    /// Returns true when the turn completed and can be answered.
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Stop | Self::ToolCalls)
    }
}

/// Token counts reported by a provider.
///
/// Every field is optional because the distinction between "not reported" and
/// "reported as zero" is part of the product contract. An unreported count is
/// absent; a reported zero stays zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Tokens read from a prompt cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Tokens written to a prompt cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Reasoning tokens, counted separately where the provider does so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// Returns true when no count was reported.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cache_read_tokens.is_none()
            && self.cache_write_tokens.is_none()
            && self.reasoning_tokens.is_none()
    }

    /// Combines two reports by taking the larger value per field.
    ///
    /// Streaming providers may restate running totals. Taking the larger value
    /// is correct for a monotonic counter and avoids double counting.
    #[must_use]
    pub fn merge_max(self, other: Self) -> Self {
        fn max(a: Option<u64>, b: Option<u64>) -> Option<u64> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            }
        }
        Self {
            input_tokens: max(self.input_tokens, other.input_tokens),
            output_tokens: max(self.output_tokens, other.output_tokens),
            cache_read_tokens: max(self.cache_read_tokens, other.cache_read_tokens),
            cache_write_tokens: max(self.cache_write_tokens, other.cache_write_tokens),
            reasoning_tokens: max(self.reasoning_tokens, other.reasoning_tokens),
        }
    }
}

/// One event from a model stream.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ProviderEvent {
    /// Text to append to the answer.
    TextDelta {
        /// The appended text.
        delta: String,
    },
    /// Reasoning text to append.
    ReasoningDelta {
        /// The appended text.
        delta: String,
    },
    /// A tool call has begun. Arguments arrive in subsequent deltas.
    ToolCallStart {
        /// Identifier assigned by the model.
        id: ToolCallId,
        /// Tool name.
        name: String,
    },
    /// A fragment of a tool call's arguments.
    ToolCallDelta {
        /// Identifier of the call being extended.
        id: ToolCallId,
        /// Appended fragment.
        delta: String,
    },
    /// A tool call's arguments are complete.
    ToolCallEnd {
        /// Identifier of the completed call.
        id: ToolCallId,
        /// Complete arguments as a JSON string.
        arguments: String,
    },
    /// Token counts reported mid-stream.
    Usage(Usage),
    /// Opaque provider state to store with the assistant message and replay.
    Replay {
        /// The state, exactly as the provider produced it.
        state: String,
    },
    /// The stream finished.
    Finish {
        /// Normalized stop reason.
        reason: FinishReason,
    },
}

/// Byte and count limits applied to one stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limit {
    /// Largest single event payload.
    pub max_event_bytes: usize,
    /// Largest total payload across the stream.
    pub max_total_bytes: usize,
    /// Largest number of events.
    pub max_events: usize,
    /// Largest number of tool calls.
    pub max_tool_calls: usize,
    /// Largest accumulated arguments for one tool call.
    pub max_tool_arguments_bytes: usize,
    /// Largest accumulated assistant content.
    pub max_content_bytes: usize,
}

impl Default for Limit {
    fn default() -> Self {
        Self {
            max_event_bytes: 1024 * 1024,
            max_total_bytes: 32 * 1024 * 1024,
            max_events: 100_000,
            max_tool_calls: 128,
            max_tool_arguments_bytes: 1024 * 1024,
            max_content_bytes: 8 * 1024 * 1024,
        }
    }
}

impl Limit {
    /// Rejects a frame that exceeds the per-event bound.
    pub fn check_event(&self, bytes: usize) -> Result<()> {
        if bytes > self.max_event_bytes {
            return Err(RuneError::too_large(
                "stream.event",
                bytes,
                self.max_event_bytes,
            ));
        }
        Ok(())
    }

    /// Rejects a stream that has grown past its total bound.
    pub fn check_total(&self, bytes: usize) -> Result<()> {
        if bytes > self.max_total_bytes {
            return Err(RuneError::too_large(
                "stream.total",
                bytes,
                self.max_total_bytes,
            ));
        }
        Ok(())
    }

    /// Rejects a stream with too many events.
    pub fn check_events(&self, count: usize) -> Result<()> {
        if count > self.max_events {
            return Err(RuneError::too_large(
                "stream.events",
                count,
                self.max_events,
            ));
        }
        Ok(())
    }

    /// Rejects a stream with too many tool calls.
    pub fn check_tool_calls(&self, count: usize) -> Result<()> {
        if count > self.max_tool_calls {
            return Err(RuneError::too_large(
                "stream.tool_calls",
                count,
                self.max_tool_calls,
            ));
        }
        Ok(())
    }
}

/// A dialect reducer: parsed frames in, normalized events out.
///
/// Implementations are stateful. A frame that violates the dialect returns an
/// error, and a reducer that has already produced a terminal event rejects
/// further input rather than emitting a second finish.
pub trait StreamReducer {
    /// Feeds one decoded frame payload.
    ///
    /// A payload of `None` means the stream ended, which is how a missing
    /// terminal event is detected.
    fn apply(&mut self, payload: Option<&str>, out: &mut Vec<ProviderEvent>) -> Result<()>;

    /// Returns true once a terminal event has been emitted.
    fn is_finished(&self) -> bool;

    /// Accumulated token usage.
    fn usage(&self) -> Usage;

    /// Opaque provider state to store with the assistant message.
    fn replay(&self) -> Option<String> {
        None
    }

    /// Checks that the stream ended cleanly.
    ///
    /// Called after the transport reports end of stream. A reducer that never
    /// saw a terminal event must fail here, because a truncated stream must
    /// never yield a success with partial arguments.
    fn finish(&self) -> Result<FinishReason>;
}

/// Builds the error used when a stream ends without a terminal event.
#[must_use]
pub fn incomplete_stream() -> RuneError {
    RuneError::new(
        ErrorCode::IncompleteStream,
        "the response stream ended without a completion event",
    )
    .with_hint("the connection may have dropped; retry the request")
}

/// Builds the error used when a frame violates the dialect.
#[must_use]
pub fn protocol_violation(detail: impl Into<String>) -> RuneError {
    RuneError::new(ErrorCode::ProtocolViolation, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reasons_have_distinct_names() {
        let all = [
            FinishReason::Stop,
            FinishReason::ToolCalls,
            FinishReason::MaxTokens,
            FinishReason::ContentFilter,
            FinishReason::MaxModelTurns,
            FinishReason::Refused,
            FinishReason::Cancelled,
            FinishReason::ProviderError,
        ];
        let mut seen = std::collections::HashSet::new();
        for reason in all {
            assert!(seen.insert(reason.as_str()), "duplicate {reason:?}");
        }
    }

    #[test]
    fn only_stop_and_tool_calls_are_successes() {
        assert!(FinishReason::Stop.is_success());
        assert!(FinishReason::ToolCalls.is_success());
        for reason in [
            FinishReason::MaxTokens,
            FinishReason::ContentFilter,
            FinishReason::Cancelled,
            FinishReason::ProviderError,
            FinishReason::Refused,
            FinishReason::MaxModelTurns,
        ] {
            assert!(!reason.is_success(), "{reason:?} should not be a success");
        }
    }

    #[test]
    fn unreported_counts_stay_absent() {
        let usage = Usage::default();
        assert!(usage.is_empty());
        let json = serde_json::to_value(usage).expect("serialize");
        assert_eq!(json, serde_json::json!({}));
    }

    #[test]
    fn a_reported_zero_is_not_absent() {
        let usage = Usage {
            input_tokens: Some(0),
            ..Usage::default()
        };
        assert!(!usage.is_empty());
        let json = serde_json::to_value(usage).expect("serialize");
        assert_eq!(json["input_tokens"], 0);
        assert!(json.get("output_tokens").is_none());
    }

    #[test]
    fn merging_usage_takes_the_larger_count() {
        let first = Usage {
            input_tokens: Some(100),
            output_tokens: Some(10),
            ..Usage::default()
        };
        let second = Usage {
            input_tokens: Some(150),
            output_tokens: Some(5),
            ..Usage::default()
        };
        let merged = first.merge_max(second);
        assert_eq!(merged.input_tokens, Some(150));
        assert_eq!(merged.output_tokens, Some(10));
    }

    #[test]
    fn merging_preserves_a_count_present_on_only_one_side() {
        let first = Usage {
            input_tokens: Some(1),
            ..Usage::default()
        };
        let second = Usage {
            reasoning_tokens: Some(7),
            ..Usage::default()
        };
        let merged = first.merge_max(second);
        assert_eq!(merged.input_tokens, Some(1));
        assert_eq!(merged.reasoning_tokens, Some(7));
    }

    #[test]
    fn limits_reject_oversized_input() {
        let limit = Limit {
            max_event_bytes: 10,
            max_total_bytes: 20,
            max_events: 3,
            max_tool_calls: 1,
            ..Limit::default()
        };
        assert!(limit.check_event(10).is_ok());
        assert!(limit.check_event(11).is_err());
        assert!(limit.check_total(20).is_ok());
        assert!(limit.check_total(21).is_err());
        assert!(limit.check_events(3).is_ok());
        assert!(limit.check_events(4).is_err());
        assert!(limit.check_tool_calls(1).is_ok());
        assert!(limit.check_tool_calls(2).is_err());
    }

    #[test]
    fn limit_errors_name_the_bound() {
        let limit = Limit {
            max_event_bytes: 5,
            ..Limit::default()
        };
        let err = limit.check_event(9).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("stream.event"));
    }

    #[test]
    fn incomplete_stream_names_the_remedy() {
        let err = incomplete_stream();
        assert_eq!(err.code(), ErrorCode::IncompleteStream);
        assert!(err.detail().hint.is_some());
    }
}
