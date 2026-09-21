//! Transport and provider error taxonomy.
//!
//! Distinct from the workspace error type because a caller needs to decide
//! whether to retry, and that decision depends on the failure kind rather than
//! on the message.

use rune_core::error::{ErrorCode, RuneError};
use serde::{Deserialize, Serialize};
use std::result::Result as StdResult;

/// Why a provider request failed.
///
/// The agent loop decides whether to retry from this value and nothing else, so
/// a new dialect maps its failures onto these kinds rather than inventing its
/// own vocabulary.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The request was malformed. Retrying cannot help.
    InvalidRequest,
    /// The credential is missing, rejected, or insufficient.
    Unauthorized,
    /// The account is out of credit or quota.
    QuotaExceeded,
    /// The endpoint rate limited the request.
    RateLimited,
    /// The model identifier is unknown to the endpoint.
    ModelNotFound,
    /// The context is longer than the model accepts.
    ContextTooLong,
    /// The endpoint is temporarily unavailable.
    Unavailable,
    /// The request timed out.
    Timeout,
    /// The connection failed.
    Network,
    /// The response could not be decoded.
    Decode,
    /// The stream ended before a terminal event.
    IncompleteStream,
    /// A frame violated the dialect.
    ProtocolViolation,
    /// The provider reported an error in the stream.
    ProviderError,
    /// The provider blocked the request on policy grounds.
    ContentFiltered,
    /// The request was cancelled.
    Cancelled,
}

impl FailureKind {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::Unauthorized => "unauthorized",
            Self::QuotaExceeded => "quota_exceeded",
            Self::RateLimited => "rate_limited",
            Self::ModelNotFound => "model_not_found",
            Self::ContextTooLong => "context_too_long",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::Decode => "decode",
            Self::IncompleteStream => "incomplete_stream",
            Self::ProtocolViolation => "protocol_violation",
            Self::ProviderError => "provider_error",
            Self::ContentFiltered => "content_filtered",
            Self::Cancelled => "cancelled",
        }
    }

    /// Returns true when repeating the identical request could succeed.
    ///
    /// A cancellation and a malformed request are not retryable. A rate limit is
    /// retryable only after the endpoint's own delay, which the caller applies.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimited
                | Self::Unavailable
                | Self::Timeout
                | Self::Network
                | Self::IncompleteStream
                | Self::ProviderError
        )
    }

    /// Returns true when the failure is attributable to the request, not the
    /// endpoint. These are surfaced to the model as a tool-visible error rather
    /// than retried.
    #[must_use]
    pub const fn is_request_fault(self) -> bool {
        matches!(
            self,
            Self::InvalidRequest
                | Self::ModelNotFound
                | Self::ContextTooLong
                | Self::ContentFiltered
        )
    }

    /// Maps onto the workspace error code used in machine-readable output.
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        match self {
            Self::InvalidRequest => ErrorCode::InvalidField,
            Self::Unauthorized => ErrorCode::AuthenticationRequired,
            Self::QuotaExceeded => ErrorCode::LimitExceeded,
            Self::RateLimited => ErrorCode::RateLimited,
            Self::ModelNotFound => ErrorCode::NotFound,
            Self::ContextTooLong => ErrorCode::TooLarge,
            Self::Unavailable | Self::Network | Self::Decode => ErrorCode::TransportFailure,
            Self::Timeout => ErrorCode::Timeout,
            Self::IncompleteStream => ErrorCode::IncompleteStream,
            Self::ProtocolViolation => ErrorCode::ProtocolViolation,
            Self::ProviderError => ErrorCode::RequestRejected,
            Self::ContentFiltered => ErrorCode::RequestRejected,
            Self::Cancelled => ErrorCode::Cancelled,
        }
    }
}

/// A provider failure.
#[derive(Clone, Debug)]
pub struct NetError {
    /// What went wrong.
    kind: FailureKind,
    /// Human-readable message, already redacted.
    message: String,
    /// HTTP status, when the failure came from a response.
    status: Option<u16>,
    /// Delay the endpoint asked for before a retry.
    retry_after_ms: Option<u64>,
    /// Provider error code, when one was reported.
    provider_code: Option<String>,
    /// Repair hint.
    hint: Option<String>,
}

impl NetError {
    /// Builds an error.
    #[must_use]
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            retry_after_ms: None,
            provider_code: None,
            hint: None,
        }
    }

    /// Attaches the HTTP status.
    #[must_use]
    pub const fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    /// Attaches the delay the endpoint requested.
    #[must_use]
    pub const fn with_retry_after(mut self, millis: u64) -> Self {
        self.retry_after_ms = Some(millis);
        self
    }

    /// Attaches the provider's own error code.
    #[must_use]
    pub fn with_provider_code(mut self, code: impl Into<String>) -> Self {
        self.provider_code = Some(code.into());
        self
    }

    /// Attaches a repair hint.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Returns what went wrong.
    #[must_use]
    pub const fn kind(&self) -> FailureKind {
        self.kind
    }

    /// Returns the message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the HTTP status, when there was one.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        self.status
    }

    /// Returns the requested retry delay.
    #[must_use]
    pub const fn retry_after_ms(&self) -> Option<u64> {
        self.retry_after_ms
    }

    /// Returns the provider's error code.
    #[must_use]
    pub fn provider_code(&self) -> Option<&str> {
        self.provider_code.as_deref()
    }

    /// Returns true when repeating the request could succeed.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }

    /// Converts to the workspace error type, preserving the code and hint.
    #[must_use]
    pub fn to_rune_error(&self) -> RuneError {
        let mut error = RuneError::new(self.kind.code(), self.message.clone());
        if let Some(hint) = &self.hint {
            error = error.with_hint(hint.clone());
        }
        if let Some(status) = self.status {
            error = error.with_observed(format!("http {status}"));
        }
        error
    }

    /// Maps an HTTP status and body to a failure kind.
    ///
    /// The body is inspected only for a provider code or a hint about length,
    /// because endpoints do not agree on a machine-readable error shape.
    #[must_use]
    pub fn classify_status(status: u16, body: &str) -> Self {
        let lower = body.to_ascii_lowercase();
        let kind = match status {
            400 => {
                if lower.contains("context length")
                    || lower.contains("too long")
                    || lower.contains("maximum context")
                {
                    FailureKind::ContextTooLong
                } else {
                    FailureKind::InvalidRequest
                }
            }
            401 | 403 => FailureKind::Unauthorized,
            402 => FailureKind::QuotaExceeded,
            404 => FailureKind::ModelNotFound,
            408 | 504 => FailureKind::Timeout,
            413 => FailureKind::ContextTooLong,
            422 => FailureKind::InvalidRequest,
            429 => FailureKind::RateLimited,
            500 | 502 | 503 | 529 => FailureKind::Unavailable,
            _ => FailureKind::ProviderError,
        };

        let mut error =
            Self::new(kind, format!("the endpoint returned HTTP {status}")).with_status(status);

        if kind == FailureKind::ContextTooLong {
            error = error.with_hint("reduce the conversation or start a new session");
        }
        if kind == FailureKind::Unauthorized {
            error = error.with_hint("check the credential for this provider");
        }
        error
    }
}

impl std::fmt::Display for FailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)?;
        if let Some(status) = self.status {
            write!(f, " (http {status})")?;
        }
        if let Some(hint) = &self.hint {
            write!(f, ": {hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for NetError {}

impl From<NetError> for RuneError {
    fn from(err: NetError) -> Self {
        err.to_rune_error()
    }
}

/// Result alias for the transport layer.
pub type NetResult<T> = StdResult<T, NetError>;

impl From<std::io::Error> for NetError {
    fn from(err: std::io::Error) -> Self {
        let kind = match err.kind() {
            std::io::ErrorKind::TimedOut => FailureKind::Timeout,
            std::io::ErrorKind::Interrupted => FailureKind::Cancelled,
            _ => FailureKind::Network,
        };
        Self::new(kind, err.to_string())
    }
}

impl From<serde_json::Error> for NetError {
    fn from(err: serde_json::Error) -> Self {
        Self::new(FailureKind::Decode, err.to_string())
    }
}

impl From<RuneError> for NetError {
    fn from(err: RuneError) -> Self {
        let kind = match err.code() {
            ErrorCode::IncompleteStream => FailureKind::IncompleteStream,
            ErrorCode::ProtocolViolation => FailureKind::ProtocolViolation,
            ErrorCode::Timeout => FailureKind::Timeout,
            ErrorCode::Cancelled => FailureKind::Cancelled,
            ErrorCode::TooLarge => FailureKind::InvalidRequest,
            _ => FailureKind::Decode,
        };
        Self::new(kind, err.message().to_owned())
    }
}

/// Parses a `Retry-After` header value into milliseconds.
///
/// The header is either a number of seconds or an HTTP date. A date is not
/// parsed here because the endpoints that use one are rare and a wrong value is
/// worse than none; only the numeric form is honored.
#[must_use]
pub fn parse_retry_after(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    let seconds: u64 = trimmed.parse().ok()?;
    Some(seconds.saturating_mul(1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The workspace error used for a truncated stream.
    fn incomplete_error() -> RuneError {
        RuneError::new(ErrorCode::IncompleteStream, "ended early")
    }

    #[test]
    fn failure_kinds_have_distinct_names() {
        let all = [
            FailureKind::InvalidRequest,
            FailureKind::Unauthorized,
            FailureKind::QuotaExceeded,
            FailureKind::RateLimited,
            FailureKind::ModelNotFound,
            FailureKind::ContextTooLong,
            FailureKind::Unavailable,
            FailureKind::Timeout,
            FailureKind::Network,
            FailureKind::Decode,
            FailureKind::IncompleteStream,
            FailureKind::ProtocolViolation,
            FailureKind::ProviderError,
            FailureKind::ContentFiltered,
            FailureKind::Cancelled,
        ];
        let mut seen = std::collections::HashSet::new();
        for kind in all {
            assert!(seen.insert(kind.as_str()), "duplicate {kind:?}");
        }
    }

    #[test]
    fn retryable_kinds_are_the_transient_ones() {
        for kind in [
            FailureKind::RateLimited,
            FailureKind::Unavailable,
            FailureKind::Timeout,
            FailureKind::Network,
            FailureKind::IncompleteStream,
            FailureKind::ProviderError,
        ] {
            assert!(kind.is_retryable(), "{kind:?} should be retryable");
        }
        for kind in [
            FailureKind::InvalidRequest,
            FailureKind::Unauthorized,
            FailureKind::Cancelled,
            FailureKind::ModelNotFound,
            FailureKind::ContentFiltered,
        ] {
            assert!(!kind.is_retryable(), "{kind:?} must not be retried");
        }
    }

    #[test]
    fn a_cancellation_is_never_retried() {
        // Retrying a cancellation would resurrect work the user stopped.
        assert!(!FailureKind::Cancelled.is_retryable());
    }

    #[test]
    fn request_faults_are_distinguished_from_endpoint_faults() {
        assert!(FailureKind::ContextTooLong.is_request_fault());
        assert!(FailureKind::InvalidRequest.is_request_fault());
        assert!(!FailureKind::Unavailable.is_request_fault());
    }

    #[test]
    fn status_classification_covers_the_documented_cases() {
        let cases = [
            (400, "bad input", FailureKind::InvalidRequest),
            (401, "", FailureKind::Unauthorized),
            (403, "", FailureKind::Unauthorized),
            (402, "", FailureKind::QuotaExceeded),
            (404, "", FailureKind::ModelNotFound),
            (408, "", FailureKind::Timeout),
            (413, "", FailureKind::ContextTooLong),
            (422, "", FailureKind::InvalidRequest),
            (429, "", FailureKind::RateLimited),
            (500, "", FailureKind::Unavailable),
            (502, "", FailureKind::Unavailable),
            (503, "", FailureKind::Unavailable),
            (504, "", FailureKind::Timeout),
        ];
        for (status, body, expected) in cases {
            let error = NetError::classify_status(status, body);
            assert_eq!(error.kind(), expected, "status {status}");
            assert_eq!(error.status(), Some(status));
        }
    }

    #[test]
    fn a_400_mentioning_context_length_is_classified_as_such() {
        let error = NetError::classify_status(400, "This model's maximum context length is 8192");
        assert_eq!(error.kind(), FailureKind::ContextTooLong);
        assert!(error.to_rune_error().detail().hint.is_some());
    }

    #[test]
    fn an_unauthorized_error_hints_at_the_credential() {
        let error = NetError::classify_status(401, "");
        assert!(error.to_rune_error().detail().hint.is_some());
    }

    #[test]
    fn retry_after_parses_seconds() {
        assert_eq!(parse_retry_after("5"), Some(5000));
        assert_eq!(parse_retry_after(" 12 "), Some(12_000));
        assert_eq!(parse_retry_after("0"), Some(0));
    }

    #[test]
    fn retry_after_rejects_a_date_and_nonsense() {
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(parse_retry_after("-1"), None);
    }

    #[test]
    fn retry_after_saturates_instead_of_overflowing() {
        assert_eq!(parse_retry_after(&u64::MAX.to_string()), Some(u64::MAX));
    }

    #[test]
    fn converting_to_the_workspace_error_preserves_the_code_and_hint() {
        let error = NetError::classify_status(429, "").with_hint("slow down");
        let converted = error.to_rune_error();
        assert_eq!(converted.code(), ErrorCode::RateLimited);
        assert_eq!(converted.detail().hint.as_deref(), Some("slow down"));
    }

    #[test]
    fn builder_methods_accumulate() {
        let error = NetError::new(FailureKind::RateLimited, "slow")
            .with_status(429)
            .with_retry_after(2000)
            .with_provider_code("rate_limit_exceeded")
            .with_hint("wait");
        assert_eq!(error.status(), Some(429));
        assert_eq!(error.retry_after_ms(), Some(2000));
        assert_eq!(error.provider_code(), Some("rate_limit_exceeded"));
        assert!(error.to_string().contains("wait"));
    }

    #[test]
    fn io_errors_map_to_transport_kinds() {
        let timed_out = std::io::Error::new(std::io::ErrorKind::TimedOut, "slow");
        assert_eq!(NetError::from(timed_out).kind(), FailureKind::Timeout);

        let interrupted = std::io::Error::new(std::io::ErrorKind::Interrupted, "stopped");
        assert_eq!(NetError::from(interrupted).kind(), FailureKind::Cancelled);

        let other = std::io::Error::other("?");
        assert_eq!(NetError::from(other).kind(), FailureKind::Network);
    }

    #[test]
    fn a_workspace_incomplete_stream_error_maps_to_that_kind() {
        assert_eq!(
            NetError::from(incomplete_error()).kind(),
            FailureKind::IncompleteStream
        );
    }
}
