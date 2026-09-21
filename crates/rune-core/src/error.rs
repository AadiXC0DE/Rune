//! Error taxonomy.
//!
//! Every error carries a stable machine-readable code, a human message, and,
//! where a specific field or invariant is at fault, a structured detail naming
//! it. Codes are part of the public surface: they appear in `--json` output and
//! in documentation, so they never change once released.

use std::fmt;

/// Stable machine-readable error code.
///
/// Serializes to `snake_case` because the codes appear in JSON output consumed
/// by scripts.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// A field carries a value that is malformed, out of range, or empty.
    InvalidField,
    /// A required field is absent.
    MissingField,
    /// An input is larger than the configured or hard bound.
    TooLarge,
    /// A configuration layer could not be parsed.
    InvalidConfiguration,
    /// A configuration key is not accepted in the scope where it appeared.
    KeyNotAllowedInScope,
    /// A path is outside every permitted root.
    PathOutsideWorkspace,
    /// A path or file failed a safety check such as symlink or permission mode.
    UnsafePath,
    /// A file changed between being read and being written.
    StalePreimage,
    /// A requested match is ambiguous.
    AmbiguousMatch,
    /// A requested resource does not exist.
    NotFound,
    /// A resource already exists.
    AlreadyExists,
    /// Another process or turn holds a lock.
    Locked,
    /// A credential is absent or unusable.
    AuthenticationRequired,
    /// The remote endpoint rejected the request.
    RequestRejected,
    /// The remote endpoint rate limited the request.
    RateLimited,
    /// A network request failed or timed out.
    TransportFailure,
    /// A response stream ended without a terminal event.
    IncompleteStream,
    /// A stream carried a frame that violates the protocol.
    ProtocolViolation,
    /// An operation exceeded its deadline.
    Timeout,
    /// The operation was cancelled by the user or by the caller.
    Cancelled,
    /// A configured limit was reached.
    LimitExceeded,
    /// A permission policy denied the action.
    PermissionDenied,
    /// The action requires approval that could not be collected.
    InputRequired,
    /// A stored record violates an invariant.
    CorruptRecord,
    /// A stored record uses a schema version this build cannot read.
    UnsupportedVersion,
    /// The requested capability is unavailable in this runtime or platform.
    Unsupported,
    /// An operation is not valid in the current state.
    InvalidState,
    /// An internal invariant failed. Indicates a defect, not user input.
    Internal,
}

impl ErrorCode {
    /// Returns the stable wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidField => "invalid_field",
            Self::MissingField => "missing_field",
            Self::TooLarge => "too_large",
            Self::InvalidConfiguration => "invalid_configuration",
            Self::KeyNotAllowedInScope => "key_not_allowed_in_scope",
            Self::PathOutsideWorkspace => "path_outside_workspace",
            Self::UnsafePath => "unsafe_path",
            Self::StalePreimage => "stale_preimage",
            Self::AmbiguousMatch => "ambiguous_match",
            Self::NotFound => "not_found",
            Self::AlreadyExists => "already_exists",
            Self::Locked => "locked",
            Self::AuthenticationRequired => "authentication_required",
            Self::RequestRejected => "request_rejected",
            Self::RateLimited => "rate_limited",
            Self::TransportFailure => "transport_failure",
            Self::IncompleteStream => "incomplete_stream",
            Self::ProtocolViolation => "protocol_violation",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::LimitExceeded => "limit_exceeded",
            Self::PermissionDenied => "permission_denied",
            Self::InputRequired => "input_required",
            Self::CorruptRecord => "corrupt_record",
            Self::UnsupportedVersion => "unsupported_version",
            Self::Unsupported => "unsupported",
            Self::InvalidState => "invalid_state",
            Self::Internal => "internal",
        }
    }

    /// Returns every code, for documentation consistency checks.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::InvalidField,
            Self::MissingField,
            Self::TooLarge,
            Self::InvalidConfiguration,
            Self::KeyNotAllowedInScope,
            Self::PathOutsideWorkspace,
            Self::UnsafePath,
            Self::StalePreimage,
            Self::AmbiguousMatch,
            Self::NotFound,
            Self::AlreadyExists,
            Self::Locked,
            Self::AuthenticationRequired,
            Self::RequestRejected,
            Self::RateLimited,
            Self::TransportFailure,
            Self::IncompleteStream,
            Self::ProtocolViolation,
            Self::Timeout,
            Self::Cancelled,
            Self::LimitExceeded,
            Self::PermissionDenied,
            Self::InputRequired,
            Self::CorruptRecord,
            Self::UnsupportedVersion,
            Self::Unsupported,
            Self::InvalidState,
            Self::Internal,
        ]
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Structured context describing exactly what failed.
///
/// The point of this type is that a caller never has to guess which field or
/// which invariant caused a failure.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct ErrorDetail {
    /// Field or symbol at fault, when the failure is attributable to one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Invariant that was violated, when one applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invariant: Option<String>,
    /// Repair hint shown to a user or returned to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Observed value, already redacted of secrets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<String>,
}

impl ErrorDetail {
    /// Returns an empty detail.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            field: None,
            invariant: None,
            hint: None,
            observed: None,
        }
    }

    /// Returns true when no context is attached.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.field.is_none()
            && self.invariant.is_none()
            && self.hint.is_none()
            && self.observed.is_none()
    }
}

/// The error type used across every crate in the workspace.
///
/// Deliberately not `#[non_exhaustive]`: callers inside the workspace match on
/// the code, and adding a code is a deliberate act that updates
/// `error_codes.md` and its consistency test.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RuneError {
    code: ErrorCode,
    message: String,
    /// Boxed so the error stays small on the success path. The detail is
    /// present on most failures but rarely observed, so paying for it inline in
    /// every `Result` would be the wrong trade.
    detail: Box<ErrorDetail>,
}

impl RuneError {
    /// Builds an error from a code and message.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: Box::new(ErrorDetail::none()),
        }
    }

    /// Builds an error attributable to a specific field.
    #[must_use]
    pub fn invalid_field(field: impl Into<String>, message: impl Into<String>) -> Self {
        let field = field.into();
        Self {
            code: ErrorCode::InvalidField,
            message: message.into(),
            detail: Box::new(ErrorDetail {
                field: Some(field),
                ..ErrorDetail::none()
            }),
        }
    }

    /// Builds an error for a missing required field.
    #[must_use]
    pub fn missing_field(field: impl Into<String>) -> Self {
        let field = field.into();
        Self {
            code: ErrorCode::MissingField,
            message: format!("required field `{field}` is missing"),
            detail: Box::new(ErrorDetail {
                field: Some(field),
                ..ErrorDetail::none()
            }),
        }
    }

    /// Builds an error for a violated invariant.
    #[must_use]
    pub fn invariant(invariant: impl Into<String>, message: impl Into<String>) -> Self {
        let invariant = invariant.into();
        Self {
            code: ErrorCode::CorruptRecord,
            message: message.into(),
            detail: Box::new(ErrorDetail {
                invariant: Some(invariant),
                ..ErrorDetail::none()
            }),
        }
    }

    /// Builds an error for a value that is too large.
    #[must_use]
    pub fn too_large(field: impl Into<String>, observed: usize, limit: usize) -> Self {
        let field = field.into();
        Self {
            code: ErrorCode::TooLarge,
            message: format!("`{field}` holds {observed} bytes, limit is {limit}"),
            detail: Box::new(ErrorDetail {
                field: Some(field),
                observed: Some(observed.to_string()),
                hint: Some(format!("reduce to at most {limit} bytes")),
                ..ErrorDetail::none()
            }),
        }
    }

    /// Attaches a repair hint.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.detail.hint = Some(hint.into());
        self
    }

    /// Attaches the observed value.
    #[must_use]
    pub fn with_observed(mut self, observed: impl Into<String>) -> Self {
        self.detail.observed = Some(observed.into());
        self
    }

    /// Attaches the violated invariant.
    #[must_use]
    pub fn with_invariant(mut self, invariant: impl Into<String>) -> Self {
        self.detail.invariant = Some(invariant.into());
        self
    }

    /// Returns the stable code.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.code
    }

    /// Returns the human-readable message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the structured detail.
    #[must_use]
    pub const fn detail(&self) -> &ErrorDetail {
        &self.detail
    }

    /// Returns true when the failure is attributable to a specific field.
    #[must_use]
    pub fn field(&self) -> Option<&str> {
        self.detail.field.as_deref()
    }
}

impl fmt::Display for RuneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(field) = &self.detail.field {
            write!(f, " (field `{field}`)")?;
        }
        if let Some(invariant) = &self.detail.invariant {
            write!(f, " (invariant `{invariant}`)")?;
        }
        if let Some(hint) = &self.detail.hint {
            write!(f, ": {hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for RuneError {}

/// Result alias used across the workspace.
pub type Result<T, E = RuneError> = std::result::Result<T, E>;

impl From<std::io::Error> for RuneError {
    fn from(err: std::io::Error) -> Self {
        let code = match err.kind() {
            std::io::ErrorKind::NotFound => ErrorCode::NotFound,
            std::io::ErrorKind::AlreadyExists => ErrorCode::AlreadyExists,
            std::io::ErrorKind::TimedOut => ErrorCode::Timeout,
            std::io::ErrorKind::Interrupted => ErrorCode::Cancelled,
            std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
            _ => ErrorCode::TransportFailure,
        };
        Self::new(code, err.to_string())
    }
}

impl From<serde_json::Error> for RuneError {
    fn from(err: serde_json::Error) -> Self {
        Self::new(ErrorCode::InvalidField, err.to_string())
    }
}

impl From<toml::de::Error> for RuneError {
    fn from(err: toml::de::Error) -> Self {
        Self::new(ErrorCode::InvalidConfiguration, err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_a_distinct_wire_name() {
        let mut seen = std::collections::HashSet::new();
        for code in ErrorCode::all() {
            assert!(seen.insert(code.as_str()), "duplicate wire name: {code:?}");
        }
        assert_eq!(seen.len(), ErrorCode::all().len());
    }

    #[test]
    fn code_wire_names_are_snake_case() {
        for code in ErrorCode::all() {
            let name = code.as_str();
            assert!(!name.is_empty());
            assert!(
                name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "not snake_case: {name}"
            );
            assert!(!name.starts_with('_') && !name.ends_with('_'));
        }
    }

    #[test]
    fn code_serde_matches_wire_name() {
        for code in ErrorCode::all() {
            let json = serde_json::to_string(code).expect("serialize");
            assert_eq!(json, format!("\"{}\"", code.as_str()));
        }
    }

    #[test]
    fn invalid_field_carries_the_field_name() {
        let err = RuneError::invalid_field("path", "must be absolute");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("path"));
        assert!(err.to_string().contains("path"));
    }

    #[test]
    fn too_large_reports_observed_and_limit() {
        let err = RuneError::too_large("content", 5000, 4096);
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.detail().observed.as_deref(), Some("5000"));
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn invariant_errors_name_the_invariant() {
        let err = RuneError::invariant("tool_call_pairing", "unpaired call");
        assert_eq!(err.detail().invariant.as_deref(), Some("tool_call_pairing"));
    }

    #[test]
    fn detail_serializes_only_present_fields() {
        let err = RuneError::invalid_field("path", "bad");
        let json = serde_json::to_value(err.detail()).expect("serialize");
        let object = json.as_object().expect("object");
        assert!(object.contains_key("field"));
        assert!(!object.contains_key("invariant"));
    }

    #[test]
    fn io_errors_map_to_specific_codes() {
        let not_found = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        assert_eq!(RuneError::from(not_found).code(), ErrorCode::NotFound);

        let denied = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no");
        assert_eq!(RuneError::from(denied).code(), ErrorCode::PermissionDenied);

        let other = std::io::Error::other("?");
        assert_eq!(RuneError::from(other).code(), ErrorCode::TransportFailure);
    }

    #[test]
    fn error_stays_small_on_the_success_path() {
        // Every fallible call carries this type, so its size is a real cost.
        let size = size_of::<RuneError>();
        assert!(size <= 48, "RuneError grew to {size} bytes");
    }

    #[test]
    fn error_is_a_std_error() {
        fn assert_error<E: std::error::Error>(_: &E) {}
        let err = RuneError::new(ErrorCode::Internal, "boom");
        assert_error(&err);
    }
}
