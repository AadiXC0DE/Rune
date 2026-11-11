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
    /// No route to the endpoint: its host name did not resolve or the network
    /// is down. Retried on a time budget of its own, since an outage says
    /// nothing about the request.
    Unreachable,
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
    /// Local policy forbids the request, such as when offline mode is enabled.
    PolicyDenied,
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
            Self::Unreachable => "unreachable",
            Self::Decode => "decode",
            Self::IncompleteStream => "incomplete_stream",
            Self::ProtocolViolation => "protocol_violation",
            Self::ProviderError => "provider_error",
            Self::ContentFiltered => "content_filtered",
            Self::PolicyDenied => "policy_denied",
            Self::Cancelled => "cancelled",
        }
    }

    /// Returns true when repeating the identical request could succeed.
    ///
    /// A cancellation, policy refusal, and malformed request are not retryable.
    /// A rate limit is retryable only after the endpoint's own delay, which the
    /// caller applies.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimited
                | Self::Unavailable
                | Self::Timeout
                | Self::Network
                | Self::Unreachable
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
            Self::Unavailable | Self::Network | Self::Unreachable | Self::Decode => {
                ErrorCode::TransportFailure
            }
            Self::Timeout => ErrorCode::Timeout,
            Self::IncompleteStream => ErrorCode::IncompleteStream,
            Self::ProtocolViolation => ErrorCode::ProtocolViolation,
            Self::ProviderError => ErrorCode::RequestRejected,
            Self::ContentFiltered => ErrorCode::RequestRejected,
            Self::PolicyDenied => ErrorCode::PermissionDenied,
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
    /// The endpoint's own verdict on whether a retry can succeed.
    should_retry: Option<bool>,
    /// Provider error code, when one was reported.
    provider_code: Option<String>,
    /// Repair hint.
    hint: Option<String>,
    /// The error a caller-supplied client reported, returned unchanged by
    /// [`NetError::to_rune_error`] so its code and detail survive the trip.
    source: Option<Box<RuneError>>,
    /// What a stream produced before it was cut, so the caller can keep it.
    partial: Option<Box<crate::transport::StreamOutcome>>,
}

impl NetError {
    /// Wraps an error from a caller-supplied client, keeping it whole.
    ///
    /// The kind is read from its code, so retries treat it like the same
    /// failure from the built-in client; any other code is not retried.
    #[must_use]
    pub fn from_rune_error(error: RuneError) -> Self {
        let kind = match error.code() {
            ErrorCode::RateLimited => FailureKind::RateLimited,
            ErrorCode::TransportFailure => FailureKind::Network,
            ErrorCode::Timeout => FailureKind::Timeout,
            ErrorCode::IncompleteStream => FailureKind::IncompleteStream,
            ErrorCode::RequestRejected => FailureKind::ProviderError,
            ErrorCode::Cancelled => FailureKind::Cancelled,
            _ => FailureKind::InvalidRequest,
        };
        Self {
            retry_after_ms: error.detail().retry_after_ms,
            ..Self::new(kind, error.message().to_owned())
        }
        .with_source(error)
    }

    /// Keeps the original error.
    fn with_source(mut self, error: RuneError) -> Self {
        self.source = Some(Box::new(error));
        self
    }
    /// Builds an error.
    #[must_use]
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            retry_after_ms: None,
            should_retry: None,
            provider_code: None,
            hint: None,
            source: None,
            partial: None,
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

    /// Attaches the endpoint's verdict on whether a retry can succeed.
    #[must_use]
    pub const fn with_should_retry(mut self, should_retry: bool) -> Self {
        self.should_retry = Some(should_retry);
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

    /// Attaches what a cut stream produced before it stopped.
    #[must_use]
    pub fn with_partial(mut self, partial: crate::transport::StreamOutcome) -> Self {
        self.partial = Some(Box::new(partial));
        self
    }

    /// Returns what a cut stream produced before it stopped, when kept.
    #[must_use]
    pub fn partial(&self) -> Option<&crate::transport::StreamOutcome> {
        self.partial.as_deref()
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

    /// Returns the endpoint's verdict on whether a retry can succeed, when it
    /// gave one.
    #[must_use]
    pub const fn should_retry(&self) -> Option<bool> {
        self.should_retry
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

    /// Converts to the workspace error type, preserving the code, hint and
    /// provider metadata. Provider codes are redacted before being exposed.
    #[must_use]
    pub fn to_rune_error(&self) -> RuneError {
        if let Some(source) = &self.source {
            return (**source).clone();
        }
        let mut error = RuneError::new(self.kind.code(), self.message.clone());
        if let Some(hint) = &self.hint {
            error = error.with_hint(hint.clone());
        }
        if let Some(status) = self.status {
            error = error.with_observed(format!("http {status}"));
        }
        if let Some(code) = &self.provider_code {
            error = error.with_provider_code(crate::redact::redact(code));
        }
        if let Some(millis) = self.retry_after_ms {
            error = error.with_retry_after(millis);
        }
        error
    }

    /// Returns the repair hint, when one was attached.
    #[must_use]
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// Maps an HTTP status and body to a failure kind.
    ///
    /// Endpoints do not agree on a machine-readable error shape, so the body's
    /// code and wording are read as well as the status, in a fixed order where
    /// the first match wins. What the endpoint said is kept in the message: it
    /// is the only party that knows which field it refused.
    #[must_use]
    pub fn classify_status(status: u16, body: &str) -> Self {
        let described = describe_body(body);
        let (code, explanation) = match &described {
            Some((code, explanation)) => (code.as_deref(), explanation.as_str()),
            None => (None, ""),
        };
        let text = format!("{explanation} {body}");
        let lower = text.to_lowercase();
        let has = |phrases: &[&str]| phrases.iter().any(|phrase| lower.contains(phrase));
        let client_error = (400..500).contains(&status);

        let (kind, hint) = if is_overflow(Some(status), code, &text) {
            (FailureKind::ContextTooLong, Some(OVERFLOW_HINT))
        } else if status == 413 {
            (FailureKind::InvalidRequest, Some(BODY_TOO_LARGE_HINT))
        } else if client_error && has(POLICY) {
            (FailureKind::ContentFiltered, None)
        } else if status == 402
            || (matches!(status, 400 | 429) && has(QUOTA))
            || code == Some("insufficient_quota")
        {
            (FailureKind::QuotaExceeded, Some(QUOTA_HINT))
        } else if matches!(status, 401 | 403) || (status == 400 && has(AUTH_400)) {
            (FailureKind::Unauthorized, Some(AUTH_HINT))
        } else if status == 429 {
            (FailureKind::RateLimited, None)
        } else if matches!(status, 408 | 504) {
            (FailureKind::Timeout, None)
        } else if status == 409 || status >= 500 {
            (FailureKind::Unavailable, None)
        } else if status == 404 {
            (FailureKind::ModelNotFound, None)
        } else if client_error {
            (FailureKind::InvalidRequest, None)
        } else {
            (FailureKind::ProviderError, None)
        };

        let mut error =
            Self::new(kind, format!("the endpoint returned HTTP {status}")).with_status(status);
        if !explanation.is_empty() {
            error.message = format!("the endpoint returned HTTP {status}: {explanation}");
        }
        if let Some(code) = code {
            error = error.with_provider_code(code);
        }
        if let Some(hint) = hint {
            error = error.with_hint(hint);
        }
        error
    }
}

const OVERFLOW_HINT: &str = "the conversation is longer than this model accepts";
const BODY_TOO_LARGE_HINT: &str = "the request body is larger than the endpoint accepts; remove large attachments or output from the conversation";
const QUOTA_HINT: &str = "the account behind this key is out of credit or quota; add credit on the provider's billing page, or run `rune connect` to use another key";
const AUTH_HINT: &str =
    "the provider refused the credential; run `rune connect` to store a new key";

/// Wordings vendors use for a request longer than the model's context window.
const OVERFLOW_PATTERNS: &[&str] = &[
    r"prompt is too long",
    r"input is too long for (?:the )?(?:requested|selected) model",
    r"too large for the model'?s configured input limit",
    r"exceeded this model context window limit",
    r"exceeds the context window",
    r"exceeds (?:the )?(?:model'?s )?maximum context length",
    r"input token count.*exceeds the maximum",
    r"number of input tokens exceeds maximum length",
    r"tokens in request more than max tokens allowed",
    r"maximum prompt length is \d+",
    r"reduce the length of the messages",
    r"requested input length \d+ exceeds maximum input length",
    r"maximum context length is \d+ tokens",
    r"exceeds (?:the )?maximum allowed input length of \d+ tokens",
    r"input \(\d+ tokens\) is longer than the model'?s context length",
    r"exceeds the limit of \d+",
    r"exceeds the available context size",
    r"greater than the context length",
    r"`inputs` tokens \+ `max_new_tokens` must be <= \d+",
    r"context window exceeds limit",
    r"context window exceeded",
    r"exceeded model token limit",
    r"context[_ ]length[_ ]exceeded",
    r"context length is only \d+ tokens",
    r"input length.*exceeds.*context length",
    r"prompt (?:too long|exceeds max length|超长)",
    r"too large for model with \d+ maximum context length",
    r"prompt has \d+ tokens, but the configured context size is \d+ tokens",
    r"model_context_window_exceeded",
    r"range of input length should be",
    r"too many tokens",
    r"token limit exceeded",
    r"request_too_large",
];

/// Wordings that resemble an overflow but name a rate limit or a bad setting.
const NOT_OVERFLOW_PATTERNS: &[&str] = &[
    r"^\s*(?:throttling error|service unavailable):",
    r"rate limit",
    r"too many requests",
    r"max[_ ]tokens must be less than",
    r"too many tokens(?: per day)?,? please wait",
];

/// Provider codes that name a context overflow.
const OVERFLOW_CODES: &[&str] = &[
    "context_length_exceeded",
    "model_context_window_exceeded",
    "request_too_large",
];

const QUOTA: &[&str] = &[
    "insufficient_quota",
    "insufficient quota",
    "insufficient balance",
    "insufficient_balance",
    "exceeded your current quota",
    "budget exceeded",
    "usage limit",
    "package has expired",
    "credit balance is too low",
];

const AUTH_400: &[&str] = &[
    "incorrect api key",
    "invalid api key",
    "invalid x-api-key",
    "api key not valid",
];

const POLICY: &[&str] = &[
    "content_policy",
    "content policy",
    "content management policy",
    "safety system",
    "content_filter",
    "flagged by",
];

fn case_insensitive_set(patterns: &[&str]) -> regex::RegexSet {
    regex::RegexSet::new(patterns.iter().map(|pattern| format!("(?i){pattern}")))
        .unwrap_or_else(|err| unreachable!("a fixed pattern failed to compile: {err}"))
}

static OVERFLOW: std::sync::LazyLock<regex::RegexSet> =
    std::sync::LazyLock::new(|| case_insensitive_set(OVERFLOW_PATTERNS));
static NOT_OVERFLOW: std::sync::LazyLock<regex::RegexSet> =
    std::sync::LazyLock::new(|| case_insensitive_set(NOT_OVERFLOW_PATTERNS));

/// Returns true when a failure says the request is longer than the model's
/// context window.
///
/// `text` is what the endpoint said; for an HTTP failure, the explanation and
/// the raw body joined with a space. A rate limit or server fault is never an
/// overflow, whatever its wording, because shortening the conversation would
/// not help it.
#[must_use]
pub fn is_overflow(status: Option<u16>, code: Option<&str>, text: &str) -> bool {
    if status.is_some_and(|status| status == 429 || (500..600).contains(&status)) {
        return false;
    }
    if code.is_some_and(|code| OVERFLOW_CODES.contains(&code)) {
        return true;
    }
    if OVERFLOW.is_match(text) && !NOT_OVERFLOW.is_match(text) {
        return true;
    }
    matches!(status, Some(400 | 413)) && text.trim().is_empty()
}

/// Extracts an error code and a readable explanation from a response body.
///
/// Endpoints disagree on the shape: OpenAI and its imitators nest the message
/// under `error`, some put a bare code in `error`, and a proxy in front of an
/// endpoint can answer with HTML instead of JSON. A body that is not one of the
/// recognized shapes still contributes its opening text, because an unexplained
/// status is the least useful thing to hand back and any clue beats none.
#[must_use]
fn describe_body(body: &str) -> Option<(Option<String>, String)> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    if trimmed.starts_with('<') && (lower.contains("<html") || lower.contains("<!doctype")) {
        return Some((
            None,
            "an HTML page came back instead of a model API response, so a gateway or proxy answered"
                .to_owned(),
        ));
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let error = value.get("error").unwrap_or(&value);
        let code = ["code", "type"]
            .iter()
            .find_map(|key| error.get(*key).and_then(serde_json::Value::as_str))
            .map(str::to_owned);

        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .or_else(|| error.as_str())
            .or_else(|| value.get("message").and_then(serde_json::Value::as_str));

        if let Some(message) = message {
            return Some((code, collapse(message)));
        }
    }

    // Not a shape that is understood. The opening of the body is still more
    // use than nothing, and the limit keeps an HTML error page from filling
    // the terminal.
    Some((None, collapse(&truncate_chars(trimmed, 300))))
}

/// Collapses whitespace so an explanation stays on one line.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncates to a character count, marking that it was cut.
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}...")
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
        // Each code maps back onto the kind whose `code` it is, so a failure a
        // reducer reports keeps the retry decision its kind carries.
        let kind = match err.code() {
            ErrorCode::IncompleteStream => FailureKind::IncompleteStream,
            ErrorCode::ProtocolViolation => FailureKind::ProtocolViolation,
            ErrorCode::Timeout => FailureKind::Timeout,
            ErrorCode::Cancelled => FailureKind::Cancelled,
            ErrorCode::PermissionDenied => FailureKind::PolicyDenied,
            ErrorCode::TooLarge
                if err.detail().provider_code.as_deref() == Some("context_overflow") =>
            {
                FailureKind::ContextTooLong
            }
            ErrorCode::TooLarge | ErrorCode::InvalidField => FailureKind::InvalidRequest,
            ErrorCode::RequestRejected => FailureKind::ProviderError,
            _ => FailureKind::Decode,
        };
        Self::new(kind, err.message().to_owned())
    }
}

/// Parses a `Retry-After` header value into milliseconds.
///
/// The header is a number of seconds, whole or decimal, or an HTTP date. A
/// date already past means the retry may go at once.
#[must_use]
pub fn parse_retry_after(raw: &str, now: jiff::Timestamp) -> Option<u64> {
    let trimmed = raw.trim();
    if let Some(millis) = parse_decimal(trimmed, 1000) {
        return Some(millis);
    }
    let date = jiff::fmt::rfc2822::DateTimeParser::new()
        .parse_timestamp(trimmed)
        .ok()?;
    let ahead = date.as_millisecond().saturating_sub(now.as_millisecond());
    Some(u64::try_from(ahead).unwrap_or(0))
}

/// Parses a `retry-after-ms` header value, a whole or decimal number of
/// milliseconds.
#[must_use]
pub fn parse_retry_after_ms(raw: &str) -> Option<u64> {
    parse_decimal(raw.trim(), 1)
}

/// Parses an `x-should-retry` header value; anything but `true` or `false`
/// says nothing.
#[must_use]
pub fn parse_should_retry(raw: &str) -> Option<bool> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("true") {
        Some(true)
    } else if trimmed.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

/// Reads a non-negative decimal number and scales it to milliseconds, rounding
/// a fraction of a millisecond up so a retry never goes early.
fn parse_decimal(text: &str, millis_per_unit: u64) -> Option<u64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) || text.ends_with('.') {
        return None;
    }
    let whole_millis = whole
        .parse::<u64>()
        .unwrap_or(u64::MAX)
        .saturating_mul(millis_per_unit);
    // Up to nine fractional digits are kept; finer ones only round up.
    let kept = &fraction[..fraction.len().min(9)];
    let scale = 10_u64.pow(u32::try_from(kept.len()).unwrap_or(0));
    let numerator = kept
        .parse::<u64>()
        .unwrap_or(0)
        .saturating_mul(millis_per_unit);
    let mut fraction_millis = numerator.checked_div(scale).unwrap_or(0);
    let remainder = numerator.checked_rem(scale).unwrap_or(0);
    if remainder != 0 || fraction[kept.len()..].bytes().any(|byte| byte != b'0') {
        fraction_millis = fraction_millis.saturating_add(1);
    }
    Some(whole_millis.saturating_add(fraction_millis))
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
            FailureKind::Unreachable,
            FailureKind::Decode,
            FailureKind::IncompleteStream,
            FailureKind::ProtocolViolation,
            FailureKind::ProviderError,
            FailureKind::ContentFiltered,
            FailureKind::PolicyDenied,
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
            FailureKind::Unreachable,
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
            FailureKind::PolicyDenied,
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
    fn a_policy_refusal_preserves_its_nonretryable_kind_and_hint() {
        let error = NetError::new(FailureKind::PolicyDenied, "outbound requests are disabled")
            .with_hint("remove the offline setting to reach a provider");
        assert!(!error.is_retryable());
        assert!(!error.kind().is_request_fault());
        assert_eq!(error.kind().as_str(), "policy_denied");
        let converted = error.to_rune_error();
        assert_eq!(converted.code(), ErrorCode::PermissionDenied);
        assert_eq!(converted.message(), error.message());
        assert_eq!(
            converted.detail().hint.as_deref(),
            Some("remove the offline setting to reach a provider")
        );
        let restored = NetError::from(converted);
        assert_eq!(restored.kind(), FailureKind::PolicyDenied);
        assert!(!restored.is_retryable());
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
        let error =
            NetError::classify_status(400, "This model's maximum context length is 8192 tokens");
        assert_eq!(error.kind(), FailureKind::ContextTooLong);
        assert!(error.to_rune_error().detail().hint.is_some());
    }

    #[test]
    fn every_known_failure_is_classified() {
        use FailureKind::{
            ContentFiltered, ContextTooLong, InvalidRequest, ModelNotFound, ProviderError,
            QuotaExceeded, RateLimited, Timeout, Unauthorized, Unavailable,
        };
        let cases: [(u16, &str, FailureKind); 45] = [
            (
                400,
                r#"{"error":{"message":"This model's maximum context length is 128000 tokens"}}"#,
                ContextTooLong,
            ),
            (
                400,
                r#"{"error":{"message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#,
                ContextTooLong,
            ),
            (
                400,
                r#"{"error":{"code":"context_length_exceeded","message":"x"}}"#,
                ContextTooLong,
            ),
            (
                400,
                r#"{"error":{"message":"Input is too long for requested model."}}"#,
                ContextTooLong,
            ),
            (
                400,
                r#"{"error":{"message":"input token count (1200000) exceeds the maximum number of tokens allowed"}}"#,
                ContextTooLong,
            ),
            (
                400,
                r#"{"message":"The input (140000 tokens) is longer than the model's context length (131072 tokens)."}"#,
                ContextTooLong,
            ),
            (
                400,
                r#"{"error":"prompt has 9000 tokens, but the configured context size is 8192 tokens"}"#,
                ContextTooLong,
            ),
            (
                400,
                r#"{"error":{"message":"Requested input length 40000 exceeds maximum input length 32768"}}"#,
                ContextTooLong,
            ),
            (
                400,
                r#"{"error":{"message":"the request exceeds the available context size, try increasing it"}}"#,
                ContextTooLong,
            ),
            (
                400,
                r#"{"error":{"message":"Range of input length should be [1, 98304]"}}"#,
                ContextTooLong,
            ),
            (
                400,
                r#"{"error":{"message":"Please reduce the length of the messages or completion."}}"#,
                ContextTooLong,
            ),
            (
                400,
                r#"{"error":{"message":"context window exceeded"}}"#,
                ContextTooLong,
            ),
            (
                400,
                r#"{"error":{"type":"request_too_large","message":"x"}}"#,
                ContextTooLong,
            ),
            (413, "", ContextTooLong),
            (400, "", ContextTooLong),
            (
                413,
                r#"{"error":{"message":"Request Entity Too Large"}}"#,
                InvalidRequest,
            ),
            (
                429,
                r#"{"error":{"message":"too many tokens per minute"}}"#,
                RateLimited,
            ),
            (
                429,
                r#"{"error":{"message":"prompt is too long"}}"#,
                RateLimited,
            ),
            (
                400,
                r#"{"error":{"message":"Too many tokens, please wait before trying again."}}"#,
                InvalidRequest,
            ),
            (
                400,
                r#"{"error":{"message":"max_tokens must be less than or equal to 4096"}}"#,
                InvalidRequest,
            ),
            (
                400,
                r#"{"message":"Throttling error: too many tokens"}"#,
                InvalidRequest,
            ),
            (
                400,
                r#"{"error":{"message":"Your request was rejected by the safety system"}}"#,
                ContentFiltered,
            ),
            (
                400,
                r#"{"error":{"code":"content_policy_violation","message":"x"}}"#,
                ContentFiltered,
            ),
            (
                402,
                r#"{"error":{"message":"Insufficient credits"}}"#,
                QuotaExceeded,
            ),
            (
                429,
                r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#,
                QuotaExceeded,
            ),
            (
                429,
                r#"{"error":{"message":"Insufficient Balance"}}"#,
                QuotaExceeded,
            ),
            (
                429,
                r#"{"error":{"message":"budget exceeded for this key"}}"#,
                QuotaExceeded,
            ),
            (
                429,
                r#"{"error":{"message":"usage limit reached for this plan"}}"#,
                QuotaExceeded,
            ),
            (
                400,
                r#"{"error":{"message":"Your credit balance is too low to access the API"}}"#,
                QuotaExceeded,
            ),
            (
                401,
                r#"{"error":{"message":"Invalid authentication"}}"#,
                Unauthorized,
            ),
            (403, r#"{"error":{"message":"forbidden"}}"#, Unauthorized),
            (
                400,
                r#"{"error":{"message":"Incorrect API key provided: sk-...."}}"#,
                Unauthorized,
            ),
            (
                400,
                r#"{"error":{"message":"API key not valid. Please pass a valid API key."}}"#,
                Unauthorized,
            ),
            (
                429,
                r#"{"error":{"type":"rate_limit_error","message":"Rate limit reached"}}"#,
                RateLimited,
            ),
            (408, "", Timeout),
            (504, "<html><body>Gateway Timeout</body></html>", Timeout),
            (409, r#"{"error":{"message":"conflict"}}"#, Unavailable),
            (
                500,
                r#"{"error":{"message":"internal error, prompt is too long"}}"#,
                Unavailable,
            ),
            (502, "<!DOCTYPE html><html>bad gateway</html>", Unavailable),
            (503, r#"{"error":{"message":"overloaded"}}"#, Unavailable),
            (
                529,
                r#"{"type":"error","error":{"type":"overloaded_error"}}"#,
                Unavailable,
            ),
            (
                404,
                r#"{"error":{"message":"model not found"}}"#,
                ModelNotFound,
            ),
            (422, r#"{"detail":"unprocessable"}"#, InvalidRequest),
            (
                400,
                r#"{"error":{"message":"unknown field `foo`"}}"#,
                InvalidRequest,
            ),
            (302, "", ProviderError),
        ];
        for (status, body, expected) in cases {
            let error = NetError::classify_status(status, body);
            assert_eq!(error.kind(), expected, "{status} {body}");
            assert_eq!(error.status(), Some(status));
            let hint = match expected {
                ContextTooLong => Some(OVERFLOW_HINT),
                InvalidRequest if status == 413 => Some(BODY_TOO_LARGE_HINT),
                QuotaExceeded => Some(QUOTA_HINT),
                Unauthorized => Some(AUTH_HINT),
                _ => None,
            };
            assert_eq!(error.hint(), hint, "{status} {body}");
        }
    }

    #[test]
    fn an_html_page_is_reported_as_a_proxy_answer() {
        let page = "\n  <!DOCTYPE html>\n<html><head><title>401</title></head><body>Unauthorized by the proxy</body></html>";
        let error = NetError::classify_status(401, page);
        assert_eq!(error.kind(), FailureKind::Unauthorized);
        assert_eq!(
            error.message(),
            "the endpoint returned HTTP 401: an HTML page came back instead of a model API response, so a gateway or proxy answered"
        );
        assert!(!error.message().contains("proxy</body>"));
        assert_eq!(error.hint(), Some(AUTH_HINT));
        assert_eq!(error.provider_code(), None);
    }

    #[test]
    fn an_unauthorized_error_hints_at_the_credential() {
        let error = NetError::classify_status(401, "");
        assert!(error.to_rune_error().detail().hint.is_some());
    }

    #[test]
    fn every_retry_after_form_is_read() {
        let now: jiff::Timestamp = "2026-10-21T07:27:40Z".parse().expect("time");
        // Seconds, whole and decimal.
        assert_eq!(parse_retry_after("5", now), Some(5000));
        assert_eq!(parse_retry_after(" 12 ", now), Some(12_000));
        assert_eq!(parse_retry_after("0", now), Some(0));
        assert_eq!(parse_retry_after("1.5", now), Some(1500));
        assert_eq!(parse_retry_after("0.0001", now), Some(1));
        // An HTTP date 20 s ahead, and one already past.
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT", now),
            Some(20_000)
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2026 07:00:00 GMT", now),
            Some(0)
        );
        // Milliseconds, whole and decimal.
        assert_eq!(parse_retry_after_ms("1500"), Some(1500));
        assert_eq!(parse_retry_after_ms(" 250.5 "), Some(251));
        assert_eq!(parse_retry_after_ms("0"), Some(0));
        // Nonsense says nothing.
        for raw in ["soon", "-1", "", "1.", ".5", "+3", "1e3", "inf", "1.2.3"] {
            assert_eq!(parse_retry_after(raw, now), None, "{raw}");
            assert_eq!(parse_retry_after_ms(raw), None, "{raw}");
        }
        // A huge value saturates instead of overflowing.
        assert_eq!(
            parse_retry_after(&u64::MAX.to_string(), now),
            Some(u64::MAX)
        );
        // The retry verdict is read without regard to case.
        assert_eq!(parse_should_retry("true"), Some(true));
        assert_eq!(parse_should_retry(" FALSE "), Some(false));
        assert_eq!(parse_should_retry("yes"), None);
    }

    #[test]
    fn converting_to_the_workspace_error_preserves_the_code_and_hint() {
        let error = NetError::classify_status(429, "").with_hint("slow down");
        let converted = error.to_rune_error();
        assert_eq!(converted.code(), ErrorCode::RateLimited);
        assert_eq!(converted.detail().hint.as_deref(), Some("slow down"));
    }

    #[test]
    fn converting_preserves_optional_provider_metadata_without_a_status() {
        let error = NetError::new(FailureKind::ProviderError, "failed")
            .with_provider_code("rate_limit_exceeded")
            .with_retry_after(0);
        let converted = RuneError::from(error);
        let detail = serde_json::to_value(converted.detail()).expect("detail");
        assert_eq!(
            detail,
            serde_json::json!({
                "provider_code": "rate_limit_exceeded",
                "retry_after_ms": 0
            })
        );
        assert!(!converted.detail().is_empty());

        let plain = NetError::new(FailureKind::Network, "failed").to_rune_error();
        assert!(plain.detail().is_empty());
        assert_eq!(
            serde_json::to_value(plain.detail()).expect("detail"),
            serde_json::json!({})
        );
    }

    #[test]
    fn converting_redacts_provider_codes_attached_by_a_caller() {
        let error = NetError::new(FailureKind::ProviderError, "failed")
            .with_provider_code("sk-fixture-secret-code");
        let converted = error.to_rune_error();
        assert_eq!(
            converted.detail().provider_code.as_deref(),
            Some("[redacted]")
        );
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

    #[test]
    fn a_workspace_error_maps_back_onto_the_kind_it_came_from() {
        for kind in [
            FailureKind::InvalidRequest,
            FailureKind::IncompleteStream,
            FailureKind::ProtocolViolation,
            FailureKind::ProviderError,
            FailureKind::Timeout,
            FailureKind::Cancelled,
            FailureKind::PolicyDenied,
        ] {
            let converted = NetError::from(RuneError::new(kind.code(), "failed"));
            assert_eq!(converted.kind(), kind);
        }
    }
}
