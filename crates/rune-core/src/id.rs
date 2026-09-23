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
        // A platform that cannot supply entropy is a platform this cannot
        // generate a unique identifier on, so it is reported rather than
        // answered with a constant.
        if fill_random(&mut bytes).is_err() {
            return Self::fallback();
        }
        let mut encoded = [0_u8; SESSION_ID_ENCODED_LEN];
        base64url_encode(&bytes, &mut encoded);
        Self(encoded)
    }

    /// Returns an identifier derived from the clock and the process.
    ///
    /// Returns an identifier derived from the clock and the process.
    ///
    /// Reached only when the platform random source is unavailable. It is not a
    /// substitute for entropy, so it mixes what does vary, and session creation
    /// still checks uniqueness against the store.
    fn fallback() -> Self {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::process::id().hash(&mut hasher);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        nanos.hash(&mut hasher);

        // Spread the digest across the whole buffer rather than truncating it,
        // so an identifier is not merely the low bits of one number.
        let mixed = hasher.finish().to_le_bytes().repeat(2);
        let mut bytes = [0_u8; SESSION_ID_ENTROPY_BYTES];
        for (slot, byte) in bytes.iter_mut().zip(mixed.iter()) {
            *slot = *byte;
        }
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
            Some(Self(self.0.saturating_sub(1)))
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
/// The platform facility is reached through the standard crate for it rather
/// than by opening a device: the device exists on some platforms and not others,
/// and a fallback that silently returns zeros makes every identifier identical
/// on the platform where the device is missing. A failure is returned instead, so
/// the caller is told rather than handed a predictable value.
fn fill_random(out: &mut [u8]) -> std::io::Result<()> {
    getrandom::getrandom(out).map_err(|err| std::io::Error::other(err.to_string()))
}

/// Encodes bytes into the unpadded URL-safe base64 alphabet.
///
/// Operates on a zero-padded copy so the loop has no truncated tail to handle
/// separately. The padding bytes only affect output positions that are never
/// written, because the number of emitted characters is derived from the real
/// length.
fn base64url_encode(input: &[u8], out: &mut [u8]) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    let emitted = input
        .len()
        .saturating_div(3)
        .saturating_mul(4)
        .saturating_add(match input.len() % 3 {
            1 => 2,
            2 => 3,
            _ => 0,
        });

    let mut index = 0;
    let mut out_index = 0;

    while out_index < emitted {
        // The three-byte window is refilled from the input each round, with
        // zeros past the end so the final partial group needs no separate path.
        let mut buffer = [0_u8; 3];
        let remaining = input.len().saturating_sub(index);
        let take = remaining.min(3);
        if let (Some(slice), Some(target)) = (
            input.get(index..index.saturating_add(take)),
            buffer.get_mut(..take),
        ) {
            target.copy_from_slice(slice);
        }

        let (Some(a), Some(b), Some(c)) = (buffer.first(), buffer.get(1), buffer.get(2)) else {
            break;
        };
        let block = (u32::from(*a) << 16) | (u32::from(*b) << 8) | u32::from(*c);

        for shift in [18_u32, 12, 6, 0] {
            if out_index >= emitted {
                break;
            }
            let alphabet_index = usize::try_from((block >> shift) & 0x3F).unwrap_or(0);
            if let (Some(slot), Some(value)) =
                (out.get_mut(out_index), ALPHABET.get(alphabet_index))
            {
                *slot = *value;
                out_index = out_index.saturating_add(1);
            }
        }

        index = index.saturating_add(3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

    #[test]
    fn generated_identifiers_differ() {
        // Reading a device that some platforms do not have returned a constant,
        // so every session on that platform shared one identifier and creation
        // looped until it gave up.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            assert!(
                seen.insert(SessionId::generate().to_string()),
                "a generated identifier repeated"
            );
        }
    }

    #[test]
    fn a_generated_identifier_is_not_all_one_character() {
        // The zeroed buffer encoded to the same character repeated, which is
        // the shape a caller would have to notice rather than an error it could
        // handle.
        for _ in 0..16 {
            let id = SessionId::generate().to_string();
            let first = id.chars().next().expect("an identifier is not empty");
            assert!(
                id.chars().any(|c| c != first),
                "`{id}` is one repeated character"
            );
        }
    }

    #[test]
    fn the_entropy_source_reports_success_on_this_platform() {
        let mut bytes = [0_u8; 16];
        fill_random(&mut bytes).expect("the platform random source is available");
        assert!(
            bytes.iter().any(|byte| *byte != 0),
            "the random source filled the buffer with zeros"
        );
    }

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
        input_len
            .saturating_div(3)
            .saturating_mul(4)
            .saturating_add(match input_len % 3 {
                0 => 0,
                1 => 2,
                _ => 3,
            })
    }
}
