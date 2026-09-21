//! Stable identifiers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::RuneError;

/// Number of random bytes behind a [`SessionId`].
///
/// Nine bytes encode to exactly twelve base64url characters with no padding,
/// which is what the on-disk layout expects.
const SESSION_ID_ENTROPY_BYTES: usize = 9;

/// Encoded length of a [`SessionId`].
const SESSION_ID_ENCODED_LEN: usize = 12;

/// Longest accepted identifier string, matching the on-disk validation bound.
const MAX_ID_LEN: usize = 255;

/// A random session identifier.
///
/// Encoded as twelve base64url characters generated from nine random bytes.
/// The alphabet is URL-safe and contains no character that needs escaping in a
/// path, so the identifier is safe as a directory name.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId([u8; SESSION_ID_ENCODED_LEN]);

impl SessionId {
    /// Generates a new identifier from the operating system random source.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0_u8; SESSION_ID_ENTROPY_BYTES];
        getrandom(&mut bytes);
        let mut encoded = [0_u8; SESSION_ID_ENCODED_LEN];
        base64url_encode(&bytes, &mut encoded);
        Self(encoded)
    }

    /// Returns the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The array only ever holds base64url bytes, so this cannot fail.
        std::str::from_utf8(&self.0).unwrap_or("")
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionId({})", self.as_str())
    }
}

impl FromStr for SessionId {
    type Err = RuneError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        validate_id_charset(raw, "session_id")?;
        if raw.len() != SESSION_ID_ENCODED_LEN {
            return Err(RuneError::invalid_field(
                "session_id",
                format!(
                    "expected {SESSION_ID_ENCODED_LEN} characters, found {}",
                    raw.len()
                ),
            ));
        }
        if raw == "latest" {
            return Err(RuneError::invalid_field(
                "session_id",
                "reserved name".to_owned(),
            ));
        }
        let mut out = [0_u8; SESSION_ID_ENCODED_LEN];
        out.copy_from_slice(raw.as_bytes());
        Ok(Self(out))
    }
}

impl TryFrom<String> for SessionId {
    type Error = RuneError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SessionId> for String {
    fn from(value: SessionId) -> Self {
        value.as_str().to_owned()
    }
}

/// Identifier of a single agent turn within a session.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct TurnId(pub u64);

impl TurnId {
    /// The identifier assigned before a turn is admitted.
    pub const ZERO: Self = Self(0);

    /// Returns true when no turn has been assigned yet.
    #[must_use]
    pub const fn is_unassigned(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "turn-{}", self.0)
    }
}

/// Identifier of one tool call, as reported by the model.
///
/// Providers emit identifiers that are not always safe to echo back, so the
/// value is kept verbatim here and projected for the wire where required.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ToolCallId(String);

impl ToolCallId {
    /// Longest accepted tool call identifier.
    pub const MAX_LEN: usize = 256;

    /// Wraps an existing identifier without validating the wire format.
    ///
    /// Used when replaying a stored provider identifier. Empty values are
    /// rejected because they cannot be matched to a result.
    pub fn new(raw: impl Into<String>) -> crate::error::Result<Self> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(RuneError::invalid_field(
                "tool_call_id",
                "must not be empty".to_owned(),
            ));
        }
        if raw.len() > Self::MAX_LEN {
            return Err(RuneError::invalid_field(
                "tool_call_id",
                format!("exceeds {} bytes", Self::MAX_LEN),
            ));
        }
        Ok(Self(raw))
    }

    /// Returns the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ToolCallId({})", self.0)
    }
}

/// Position of one event in the append-only session log.
///
/// Sequence numbers start at one and must be contiguous. A gap means the log
/// is damaged, which is reported by naming the missing index.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct EventSeq(pub u64);

impl EventSeq {
    /// The first sequence number written to a session.
    pub const FIRST: Self = Self(1);

    /// Returns the next sequence number, saturating at the maximum.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Returns the sequence number preceding this one, if any.
    #[must_use]
    pub const fn previous(self) -> Option<Self> {
        if self.0 <= 1 {
            None
        } else {
            Some(Self(self.0 - 1))
        }
    }
}

impl fmt::Display for EventSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Rejects identifiers containing anything outside the accepted alphabet.
///
/// The alphabet is restricted because these values become path components.
fn validate_id_charset(raw: &str, field: &'static str) -> crate::error::Result<()> {
    if raw.is_empty() {
        return Err(RuneError::invalid_field(
            field,
            "must not be empty".to_owned(),
        ));
    }
    if raw.len() > MAX_ID_LEN {
        return Err(RuneError::invalid_field(
            field,
            format!("exceeds {MAX_ID_LEN} bytes"),
        ));
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(RuneError::invalid_field(
            field,
            "contains characters outside the accepted set".to_owned(),
        ));
    }
    if raw == "." || raw == ".." {
        return Err(RuneError::invalid_field(
            field,
            "relative path components are not identifiers".to_owned(),
        ));
    }
    Ok(())
}

/// Fills the buffer from the operating system random source.
///
/// Returns zeros if the source is unavailable, which yields a predictable
/// identifier rather than an abort. Session creation validates uniqueness
/// against the store, so a collision is reported rather than silently reused.
fn getrandom(out: &mut [u8]) {
    use std::fs::File;
    use std::io::Read;

    if let Ok(mut file) = File::open("/dev/urandom") {
        if file.read_exact(out).is_ok() {
            return;
        }
    }
    out.fill(0);
}

/// Encodes bytes into the unpadded URL-safe base64 alphabet.
fn base64url_encode(input: &[u8], out: &mut [u8]) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    let mut out_index = 0;
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let mut block = [0_u8; 3];
        block.copy_from_slice(chunk);
        let n = (u32::from(block[0]) << 16) | (u32::from(block[1]) << 8) | u32::from(block[2]);
        for shift in [18_u32, 12, 6, 0] {
            let idx = ((n >> shift) & 0x3F) as usize;
            if out_index < out.len() {
                out[out_index] = ALPHABET[idx];
                out_index += 1;
            }
        }
    }

    match chunks.remainder() {
        [a] => {
            let n = u32::from(*a) << 16;
            for shift in [18_u32, 12] {
                let idx = ((n >> shift) & 0x3F) as usize;
                if out_index < out.len() {
                    out[out_index] = ALPHABET[idx];
                    out_index += 1;
                }
            }
        }
        [a, b] => {
            let n = (u32::from(*a) << 16) | (u32::from(*b) << 8);
            for shift in [18_u32, 12, 6] {
                let idx = ((n >> shift) & 0x3F) as usize;
                if out_index < out.len() {
                    out[out_index] = ALPHABET[idx];
                    out_index += 1;
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

    #[test]
    fn session_id_is_twelve_url_safe_characters() {
        let id = SessionId::generate();
        assert_eq!(id.as_str().len(), SESSION_ID_ENCODED_LEN);
        assert!(
            id.as_str()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn session_id_round_trips_through_text() {
        let id = SessionId::generate();
        let parsed: SessionId = id.to_string().parse().expect("round trip");
        assert_eq!(id, parsed);
    }

    #[test]
    fn session_id_round_trips_through_json() {
        let id = SessionId::generate();
        let json = serde_json::to_string(&id).expect("serialize");
        let parsed: SessionId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, parsed);
    }

    #[test]
    fn distinct_ids_do_not_collide_in_a_small_sample() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(SessionId::generate()), "duplicate generated id");
        }
    }

    #[test]
    fn session_id_rejects_wrong_length() {
        let err = "abc".parse::<SessionId>().expect_err("too short");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn session_id_rejects_reserved_name() {
        assert!("latest".parse::<SessionId>().is_err());
    }

    #[test]
    fn session_id_rejects_path_traversal() {
        assert!("..".parse::<SessionId>().is_err());
        assert!("a/../../etc".parse::<SessionId>().is_err());
    }

    #[test]
    fn tool_call_id_rejects_empty_and_oversized() {
        assert!(ToolCallId::new("").is_err());
        assert!(ToolCallId::new("x".repeat(ToolCallId::MAX_LEN + 1)).is_err());
        assert!(ToolCallId::new("call_1").is_ok());
    }

    #[test]
    fn event_seq_progresses_and_saturates() {
        assert_eq!(EventSeq::FIRST.previous(), None);
        assert_eq!(EventSeq::FIRST.next(), EventSeq(2));
        assert_eq!(EventSeq(u64::MAX).next(), EventSeq(u64::MAX));
    }

    #[test]
    fn base64url_encoding_matches_known_vectors() {
        let cases: &[(&[u8], &str)] = &[
            (b"", ""),
            (b"f", "Zg"),
            (b"fo", "Zm8"),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg"),
            (b"fooba", "Zm9vYmE"),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (input, expected) in cases {
            let mut out = [0_u8; 16];
            base64url_encode(input, &mut out);
            let produced = std::str::from_utf8(&out[..encoded_len(input.len())]).expect("utf8");
            assert_eq!(produced, *expected, "input {input:?}");
        }
    }

    fn encoded_len(input_len: usize) -> usize {
        (input_len / 3) * 4
            + match input_len % 3 {
                0 => 0,
                1 => 2,
                _ => 3,
            }
    }
}
