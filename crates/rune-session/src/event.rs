//! The append-only session event log.
//!
//! One JSON object per line, schema version 1. The log is the authority for a
//! session: every other file in a session directory is derived from it and can
//! be rebuilt. Three properties are what make the format survive a crash:
//!
//! - Sequence numbers run contiguously from one, so a missing event is reported
//!   as a gap naming the missing index rather than read as a shorter
//!   conversation.
//! - A frame is written whole and fsynced, so a tear can only affect the last
//!   line of the file.
//! - A torn last line is dropped and reported, so the rest of the log stays
//!   readable and the caller can decide whether to truncate or to recover.

use camino::Utf8Path;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::EventSeq;
use serde::{Deserialize, Serialize};

/// Schema version written by this build.
pub const SCHEMA_VERSION: u32 = 1;

/// Largest accepted frame, in encoded bytes, excluding the line terminator.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Largest accepted log file, in bytes, including every line terminator.
///
/// A session that reaches this size cannot be appended to; the caller is
/// expected to carry the transcript forward with a compaction event or to start
/// a new session.
pub const MAX_LOG_BYTES: u64 = 256 * 1024 * 1024;

/// Bytes of a corrupt frame kept in the error that reports it.
///
/// Bounded because the offending line can be as large as a frame, and an error
/// message is not a place to copy megabytes of input.
const EXCERPT_BYTES: usize = 64;

/// One recorded event.
///
/// Serialized with a `kind` tag, so a reader can dispatch on the variant
/// without knowing the position of any other field.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// A turn began.
    TurnStarted {
        /// Turn number, starting at one.
        turn: u64,
    },
    /// The user contributed a message.
    UserMessage {
        /// Message text.
        text: String,
    },
    /// The model contributed a message.
    AssistantMessage {
        /// Turn the message belongs to.
        turn: u64,
        /// Message text.
        text: String,
    },
    /// The model asked for a tool call.
    ToolCall {
        /// Identifier assigned by the model.
        call_id: String,
        /// Tool name.
        name: String,
        /// Arguments as the model produced them.
        arguments: String,
    },
    /// A tool call produced a result.
    ToolResult {
        /// Identifier of the call this answers.
        call_id: String,
        /// Whether the call succeeded.
        ok: bool,
        /// Result text, already bounded by the tool.
        output: String,
    },
    /// The transcript was compacted.
    Compaction {
        /// Last sequence number covered by the summary.
        through: u64,
        /// Summary that replaces the covered range.
        summary: String,
    },
    /// A provider reported token usage.
    UsageRecorded {
        /// Input tokens.
        input_tokens: u64,
        /// Output tokens.
        output_tokens: u64,
    },
    /// The session title was set.
    TitleSet {
        /// Display title.
        title: String,
    },
    /// The session was created as a child of another.
    ///
    /// Recorded so the session can be kept out of ordinary discovery: a child is
    /// an implementation detail of its parent's turn, not a conversation the
    /// user started, and resuming one directly would run it without the parent's
    /// authority.
    ChildOf {
        /// Session that created this one.
        parent: String,
    },
    /// The workspace the session ran in was recorded.
    ///
    /// Written once, when the session is created, so a later listing can scope
    /// itself to a workspace without reading every session's contents.
    WorkspaceSet {
        /// Canonical workspace path.
        workspace: String,
    },
}

impl SessionEvent {
    /// Returns the wire name of this event kind.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn_started",
            Self::UserMessage { .. } => "user_message",
            Self::AssistantMessage { .. } => "assistant_message",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::Compaction { .. } => "compaction",
            Self::UsageRecorded { .. } => "usage_recorded",
            Self::TitleSet { .. } => "title_set",
            Self::WorkspaceSet { .. } => "workspace_set",
            Self::ChildOf { .. } => "child_of",
        }
    }
}

/// One line of the log.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct EventFrame {
    /// Schema version of the frame.
    pub schema: u32,
    /// Position in the log, contiguous from one.
    pub seq: EventSeq,
    /// Milliseconds since the Unix epoch, increasing within one log.
    pub timestamp_ms: u64,
    /// The event body.
    pub event: SessionEvent,
}

impl EventFrame {
    /// Builds a frame at the current schema version.
    #[must_use]
    pub fn new(seq: EventSeq, timestamp_ms: u64, event: SessionEvent) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            seq,
            timestamp_ms,
            event,
        }
    }

    /// Encodes the frame as one line, without its terminator.
    ///
    /// The result never contains a newline: serialization escapes control
    /// characters, so a payload holding one cannot split the frame across two
    /// lines.
    pub fn encode(&self) -> Result<String> {
        self.encode_with_limit(MAX_FRAME_BYTES)
    }

    /// Encodes the frame, rejecting one larger than `limit` bytes.
    ///
    /// The limit is a parameter so the bound can be exercised at its exact
    /// boundary without building a frame of the production size.
    pub fn encode_with_limit(&self, limit: usize) -> Result<String> {
        self.check()?;
        let line = serde_json::to_string(self)?;
        if line.len() > limit {
            return Err(
                RuneError::too_large("event_frame", line.len(), limit).with_invariant("frame_size")
            );
        }
        Ok(line)
    }

    /// Decodes one line into a frame.
    ///
    /// The error names the sequence number when the line still carries one, and
    /// always names the violated invariant.
    pub fn decode(line: &str) -> Result<Self> {
        let frame: Self =
            serde_json::from_str(line).map_err(|cause| encoding_error(line, &cause))?;
        frame.check()?;
        Ok(frame)
    }

    /// Rejects a frame that must never reach the log.
    fn check(&self) -> Result<()> {
        if self.schema != SCHEMA_VERSION {
            return Err(RuneError::new(
                ErrorCode::UnsupportedVersion,
                format!(
                    "event {} uses schema version {}, this build reads {SCHEMA_VERSION}",
                    self.seq, self.schema
                ),
            )
            .with_invariant("schema_version")
            .with_observed(format!("schema {}", self.schema))
            .with_hint("upgrade Rune to read this session, or recover it into a new one"));
        }
        if self.seq.0 == 0 {
            return Err(RuneError::invariant(
                "event_sequence",
                "event sequence starts at 1, found 0",
            )
            .with_observed("seq 0"));
        }
        Ok(())
    }
}

/// Returns milliseconds since the Unix epoch.
///
/// A clock reading before the epoch is reported as zero rather than as a
/// negative value, which the frame schema has no room for.
pub(crate) fn now_millis() -> u64 {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// Why a log stopped being readable.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LogStop {
    /// The file ends inside a frame, so the fragment was dropped.
    TornTail,
    /// A frame was rejected; the error names the sequence and the invariant.
    Rejected(RuneError),
}

/// A log read that keeps whatever was still valid.
///
/// Used by recovery, which must report what it salvaged rather than refuse the
/// whole file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogSalvage {
    /// Frames read before the first defect, contiguous from sequence one.
    pub events: Vec<EventFrame>,
    /// What stopped the read, when anything did.
    pub stop: Option<LogStop>,
    /// Byte offset where salvaging stopped.
    pub stop_offset: Option<u64>,
    /// Frames left unread, counting the one that was rejected.
    pub dropped_frames: u64,
    /// Bytes left unread.
    pub dropped_bytes: u64,
}

impl LogSalvage {
    /// Returns true when the file ended inside a frame.
    #[must_use]
    pub fn is_torn(&self) -> bool {
        matches!(self.stop, Some(LogStop::TornTail))
    }

    /// Returns the error that stopped the read, when a frame was rejected.
    #[must_use]
    pub fn defect(&self) -> Option<&RuneError> {
        match &self.stop {
            Some(LogStop::Rejected(err)) => Some(err),
            _ => None,
        }
    }

    /// Returns the number of salvaged frames.
    #[must_use]
    pub fn salvaged(&self) -> u64 {
        frame_count(self.events.len())
    }

    /// Converts the salvage into a read, rejecting a defect that is not a tear.
    ///
    /// A torn tail is reported rather than failed, because it is the expected
    /// state after a crash. Any other defect means the log contradicts itself
    /// and must not be read as a shorter conversation.
    pub fn into_read(self) -> Result<LogRead> {
        match self.stop {
            Some(LogStop::Rejected(err)) => Err(err),
            _ => Ok(LogRead {
                events: self.events,
                truncated_at: self.stop_offset,
            }),
        }
    }
}

/// The readable prefix of a log, plus where reading stopped.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LogRead {
    /// Frames read, contiguous from sequence one.
    pub events: Vec<EventFrame>,
    /// Byte offset of the first byte dropped because the log ended mid-frame.
    pub truncated_at: Option<u64>,
}

impl LogRead {
    /// Returns true when a partial frame was dropped.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated_at.is_some()
    }

    /// Returns true when no event was read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Returns the number of events read.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Returns the last sequence number read.
    #[must_use]
    pub fn last_seq(&self) -> Option<EventSeq> {
        self.events.last().map(|frame| frame.seq)
    }
}

/// Reads a log, rejecting a defect that is not a torn tail.
///
/// A missing file reads as an empty log. A file with a gap, a rejected frame,
/// or an oversize frame fails with a typed error naming the sequence and the
/// invariant.
pub fn read_log(path: &Utf8Path) -> Result<LogRead> {
    read_log_limited(path, MAX_LOG_BYTES)
}

/// Reads a log with an explicit file size bound.
pub fn read_log_limited(path: &Utf8Path, limit: u64) -> Result<LogRead> {
    let bytes = read_log_bytes(path, limit)?.unwrap_or_default();
    salvage_bytes(&bytes).into_read()
}

/// Reads a log, keeping the longest valid prefix instead of failing.
///
/// Only input and output failures are returned. A damaged log is reported
/// through [`LogSalvage::stop`].
pub fn salvage_log(path: &Utf8Path) -> Result<LogSalvage> {
    salvage_log_limited(path, MAX_LOG_BYTES)
}

/// Reads a log with an explicit file size bound, keeping the valid prefix.
pub fn salvage_log_limited(path: &Utf8Path, limit: u64) -> Result<LogSalvage> {
    let bytes = read_log_bytes(path, limit)?.unwrap_or_default();
    Ok(salvage_bytes(&bytes))
}

/// Rejects an append that would push the log past [`MAX_LOG_BYTES`].
///
/// `current` is the size of the file and `additional` the encoded size of the
/// line about to be written, terminator included.
pub fn check_log_capacity(current: u64, additional: usize) -> Result<()> {
    let additional = u64::try_from(additional).unwrap_or(u64::MAX);
    let total = current.saturating_add(additional);
    if total > MAX_LOG_BYTES {
        return Err(size_error("event_log", total, MAX_LOG_BYTES).with_invariant("log_size"));
    }
    Ok(())
}

/// Reads a log file as bytes after verifying it is a private regular file.
///
/// The file is read as bytes rather than through `rune_core::paths::read_private`
/// because a tear can land inside a multi-byte character, and a lossy decode of
/// the torn tail keeps the frames before it reachable.
fn read_log_bytes(path: &Utf8Path, limit: u64) -> Result<Option<Vec<u8>>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    check_private(path, &meta)?;
    if meta.len() > limit {
        return Err(size_error("event_log", meta.len(), limit).with_invariant("log_size"));
    }
    Ok(Some(std::fs::read(path)?))
}

/// Verifies that a path is a regular private file with a single link.
///
/// Mirrors the write path checks in `rune_core::paths`, which cannot be reused
/// here because they only run alongside a read into a `String`.
fn check_private(path: &Utf8Path, meta: &std::fs::Metadata) -> Result<()> {
    if meta.file_type().is_symlink() {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` is a symbolic link"),
        )
        .with_hint("session files must be real files"));
    }
    if !meta.is_file() {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` exists and is not a regular file"),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let mode = meta.mode() & 0o777;
        if mode & !rune_core::paths::FILE_MODE != 0 {
            return Err(RuneError::new(
                ErrorCode::UnsafePath,
                format!(
                    "`{path}` has mode {mode:o}, expected {:o} or narrower",
                    rune_core::paths::FILE_MODE
                ),
            )
            .with_hint(format!(
                "run `chmod {:o} {path}`",
                rune_core::paths::FILE_MODE
            )));
        }
        if meta.nlink() != 1 {
            return Err(RuneError::new(
                ErrorCode::UnsafePath,
                format!("`{path}` has {} hard links", meta.nlink()),
            )
            .with_hint("Rune refuses session files with multiple links"));
        }
    }
    Ok(())
}

/// Parses a whole log body, keeping the longest valid prefix.
fn salvage_bytes(bytes: &[u8]) -> LogSalvage {
    let mut events: Vec<EventFrame> = Vec::new();
    let mut expected = EventSeq::FIRST;
    let mut cursor = 0_usize;
    let mut stop: Option<(LogStop, usize)> = None;

    while cursor < bytes.len() {
        let rest = &bytes[cursor..];
        let start = cursor;
        let (line, terminated) = match rest.iter().position(|byte| *byte == b'\n') {
            Some(index) => (rest.split_at(index).0, true),
            None => (rest, false),
        };
        cursor = start
            .saturating_add(line.len())
            .saturating_add(usize::from(terminated));

        if line.len() > MAX_FRAME_BYTES {
            let err = size_error(
                "event_frame",
                u64::try_from(line.len()).unwrap_or(u64::MAX),
                u64::try_from(MAX_FRAME_BYTES).unwrap_or(u64::MAX),
            )
            .with_invariant("frame_size");
            stop = Some((LogStop::Rejected(err), start));
            break;
        }

        // A frame without its terminator is never accepted, whatever it
        // decodes to. Accepting one would let the next append land on the same
        // line as this frame, turning two events into one unreadable line.
        if !terminated {
            stop = Some((LogStop::TornTail, start));
            break;
        }

        match decode_line(line) {
            Ok(frame) if frame.seq == expected => {
                expected = frame.seq.next();
                events.push(frame);
            }
            Ok(frame) => {
                stop = Some((LogStop::Rejected(sequence_gap(expected, frame.seq)), start));
                break;
            }
            Err(err) => {
                stop = Some((LogStop::Rejected(err), start));
                break;
            }
        }
    }

    let stop_offset = stop
        .as_ref()
        .map(|(_, offset)| u64::try_from(*offset).unwrap_or(u64::MAX));

    let (stop_kind, dropped_bytes, dropped_frames) = match stop {
        Some((kind, offset)) => {
            let dropped = &bytes[offset..];
            (
                Some(kind),
                u64::try_from(dropped.len()).unwrap_or(u64::MAX),
                count_frames(dropped),
            )
        }
        None => (None, 0, 0),
    };

    LogSalvage {
        events,
        stop_offset,
        stop: stop_kind,
        dropped_bytes,
        dropped_frames,
    }
}

/// Decodes one raw line, reporting invalid UTF-8 as corruption.
fn decode_line(line: &[u8]) -> Result<EventFrame> {
    let text = std::str::from_utf8(line).map_err(|err| {
        let at = err.valid_up_to();
        RuneError::invariant(
            "frame_encoding",
            format!("event frame at byte {at} is not valid UTF-8"),
        )
        .with_observed(format!("byte {at}"))
    })?;
    EventFrame::decode(text)
}

/// Builds the error reported for a line that is not a frame.
fn encoding_error(line: &str, cause: &serde_json::Error) -> RuneError {
    let observed = observed_seq(line);
    match observed {
        Some(seq) => RuneError::invariant(
            "frame_encoding",
            format!("event {seq} is not a valid event frame: {cause}"),
        )
        .with_observed(format!("seq {seq}")),
        None => RuneError::invariant(
            "frame_encoding",
            format!("an event frame is not valid JSON: {cause}"),
        )
        .with_observed(excerpt(line)),
    }
}

/// Builds the error reported for a sequence number out of order.
fn sequence_gap(expected: EventSeq, found: EventSeq) -> RuneError {
    RuneError::invariant(
        "event_sequence",
        format!("session log is missing event {expected}, found event {found}"),
    )
    .with_observed(format!("seq {found}"))
    .with_hint("recover the session to keep the events before the gap")
}

/// Builds the error reported for a size bound.
fn size_error(field: &str, observed: u64, limit: u64) -> RuneError {
    RuneError::too_large(
        field,
        usize::try_from(observed).unwrap_or(usize::MAX),
        usize::try_from(limit).unwrap_or(usize::MAX),
    )
}

/// Returns the sequence number still visible in a corrupt line.
///
/// Best effort: the value is only used to make the error easier to act on.
fn observed_seq(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("{\"schema\"")?;
    let rest = rest.split_once("\"seq\":")?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Returns a bounded, printable excerpt of a corrupt line.
fn excerpt(line: &str) -> String {
    let truncated = line.len() > EXCERPT_BYTES;
    let mut text: String = line
        .chars()
        .take(EXCERPT_BYTES)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    if truncated {
        text.push_str("...");
    }
    text
}

/// Counts the frames a byte range holds.
fn count_frames(region: &[u8]) -> u64 {
    let mut count = 0_usize;
    for byte in region {
        if *byte == b'\n' {
            count = count.saturating_add(1);
        }
    }
    let partial = usize::from(region.last().is_some_and(|byte| *byte != b'\n'));
    frame_count(count.saturating_add(partial))
}

/// Converts a length to the type used in reports.
fn frame_count(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use rune_core::error::ErrorCode;
    use rune_core::paths::write_private;
    use std::str::FromStr;

    fn frame(seq: u64, event: SessionEvent) -> EventFrame {
        EventFrame::new(EventSeq(seq), 1_000_u64.saturating_add(seq), event)
    }

    fn message(seq: u64) -> EventFrame {
        frame(
            seq,
            SessionEvent::UserMessage {
                text: format!("message {seq}"),
            },
        )
    }

    fn body(frames: &[EventFrame]) -> String {
        let mut text = String::new();
        for frame in frames {
            text.push_str(&frame.encode().expect("encode"));
            text.push('\n');
        }
        text
    }

    fn log_path(dir: &tempfile::TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(dir.path().join("events.jsonl")).expect("utf8 path")
    }

    fn write_log(path: &Utf8Path, contents: &str) {
        write_private(path, contents).expect("write log");
    }

    #[test]
    fn every_event_kind_round_trips() {
        let events = [
            SessionEvent::TurnStarted { turn: 1 },
            SessionEvent::UserMessage {
                text: "line one\nline two".to_owned(),
            },
            SessionEvent::AssistantMessage {
                turn: 1,
                text: "answer".to_owned(),
            },
            SessionEvent::ToolCall {
                call_id: "call_1".to_owned(),
                name: "read_file".to_owned(),
                arguments: "{\"path\":\"a.rs\"}".to_owned(),
            },
            SessionEvent::ToolResult {
                call_id: "call_1".to_owned(),
                ok: true,
                output: "contents".to_owned(),
            },
            SessionEvent::Compaction {
                through: 4,
                summary: "summary".to_owned(),
            },
            SessionEvent::UsageRecorded {
                input_tokens: 10,
                output_tokens: 3,
            },
            SessionEvent::TitleSet {
                title: "tidy the parser".to_owned(),
            },
        ];

        for (index, event) in events.iter().enumerate() {
            let seq = u64::try_from(index).expect("index") + 1;
            let original = frame(seq, event.clone());
            let line = original.encode().expect("encode");
            let decoded = EventFrame::decode(&line).expect("decode");
            assert_eq!(decoded, original);
        }
    }

    #[test]
    fn an_encoded_frame_is_one_line() {
        let original = frame(
            1,
            SessionEvent::UserMessage {
                text: "first\nsecond\r\nthird".to_owned(),
            },
        );
        let line = original.encode().expect("encode");
        assert!(!line.contains('\n'), "frame carried a newline: {line}");
        assert!(!line.contains('\r'));
    }

    #[test]
    fn an_unknown_schema_version_is_reported() {
        let line =
            r#"{"schema":9,"seq":1,"timestamp_ms":5,"event":{"kind":"turn_started","turn":1}}"#;
        let err = EventFrame::decode(line).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::UnsupportedVersion);
        assert_eq!(err.detail().invariant.as_deref(), Some("schema_version"));
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn a_zero_sequence_is_rejected_by_name() {
        let line =
            r#"{"schema":1,"seq":0,"timestamp_ms":5,"event":{"kind":"turn_started","turn":1}}"#;
        let err = EventFrame::decode(line).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
        assert_eq!(err.detail().invariant.as_deref(), Some("event_sequence"));
        assert!(err.message().contains("sequence"));
    }

    #[test]
    fn a_truncated_frame_names_its_sequence_number() {
        let line = r#"{"schema":1,"seq":7,"timestamp_ms":5,"event":{"kind":"user_"#;
        let err = EventFrame::decode(line).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
        assert_eq!(err.detail().invariant.as_deref(), Some("frame_encoding"));
        assert_eq!(err.detail().observed.as_deref(), Some("seq 7"));
        assert!(err.message().contains("event 7"));
    }

    #[test]
    fn a_corrupt_line_is_excerpted_not_echoed_whole() {
        let line = format!("not json at all {}", "x".repeat(4096));
        let err = EventFrame::decode(&line).expect_err("rejected");
        let observed = err.detail().observed.clone().expect("observed");
        assert!(observed.len() < 96, "excerpt was {} bytes", observed.len());
        assert!(observed.ends_with("..."));
    }

    #[test]
    fn a_frame_encodes_and_decodes_at_the_boundary() {
        let original = message(1);
        let limit = original.encode().expect("encode").len();

        let exact = message(1).encode_with_limit(limit).expect("at the limit");
        assert_eq!(exact.len(), limit);
        assert_eq!(EventFrame::decode(&exact).expect("decode"), original);

        let err = message(1)
            .encode_with_limit(limit.saturating_sub(1))
            .expect_err("over the limit");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.detail().invariant.as_deref(), Some("frame_size"));
    }

    #[test]
    fn a_frame_exactly_at_the_production_cap_is_accepted() {
        let base = frame(
            1,
            SessionEvent::UserMessage {
                text: String::new(),
            },
        );
        let empty = base.encode().expect("encode").len();
        let text = "a".repeat(MAX_FRAME_BYTES.saturating_sub(empty));
        let full = frame(1, SessionEvent::UserMessage { text });
        let line = full.encode().expect("at the cap");
        assert_eq!(line.len(), MAX_FRAME_BYTES);
        assert_eq!(EventFrame::decode(&line).expect("decode"), full);
    }

    #[test]
    fn a_frame_one_byte_past_the_production_cap_is_rejected() {
        let base = frame(
            1,
            SessionEvent::UserMessage {
                text: String::new(),
            },
        );
        let empty = base.encode().expect("encode").len();
        let text = "a".repeat(MAX_FRAME_BYTES.saturating_sub(empty).saturating_add(1));
        let err = frame(1, SessionEvent::UserMessage { text })
            .encode()
            .expect_err("over the cap");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.detail().observed.as_deref(), Some("67108865"));
    }

    #[test]
    fn a_contiguous_log_reads_completely() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        let frames = vec![message(1), message(2), message(3)];
        write_log(&path, &body(&frames));

        let read = read_log(&path).expect("read");
        assert_eq!(read.events, frames);
        assert_eq!(read.last_seq(), Some(EventSeq(3)));
        assert!(!read.is_truncated());
        assert_eq!(read.len(), 3);
    }

    #[test]
    fn a_gap_names_the_missing_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        write_log(&path, &body(&[message(1), message(2), message(4)]));

        let err = read_log(&path).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
        assert_eq!(err.detail().invariant.as_deref(), Some("event_sequence"));
        assert!(
            err.message().contains("missing event 3"),
            "message was `{}`",
            err.message()
        );
    }

    #[test]
    fn a_duplicate_sequence_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        write_log(&path, &body(&[message(1), message(2), message(2)]));

        let err = read_log(&path).expect_err("rejected");
        assert!(err.message().contains("missing event 3"));
    }

    #[test]
    fn a_log_that_does_not_start_at_one_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        write_log(&path, &body(&[message(2)]));

        let err = read_log(&path).expect_err("rejected");
        assert!(err.message().contains("missing event 1"));
    }

    #[test]
    fn a_torn_final_frame_reads_to_the_last_complete_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        let frames = vec![message(1), message(2), message(3)];
        let complete = body(&frames);
        let torn = format!("{complete}{}", message(4).encode().expect("encode"));
        let torn = &torn[..torn.len().saturating_sub(5)];
        write_log(&path, torn);

        let read = read_log(&path).expect("read");
        assert_eq!(read.events, frames);
        assert_eq!(
            read.truncated_at,
            Some(u64::try_from(complete.len()).expect("offset"))
        );
    }

    #[test]
    fn a_torn_frame_inside_a_multi_byte_character_stays_readable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        let good = frame(
            1,
            SessionEvent::UserMessage {
                text: "café 😀".to_owned(),
            },
        );
        let complete = body(std::slice::from_ref(&good));
        let tail = frame(
            2,
            SessionEvent::TitleSet {
                title: "😀".to_owned(),
            },
        )
        .encode()
        .expect("encode");
        let mut torn = complete.clone();
        torn.push_str(&tail);
        // Cutting the encoded body mid-character leaves invalid UTF-8 behind.
        let cut = torn.len().saturating_sub(3);
        let mut bytes = torn.into_bytes();
        bytes.truncate(cut);
        write_log(&path, &complete);
        std::fs::write(&path, &bytes).expect("overwrite");

        let read = read_log(&path).expect("read");
        assert_eq!(read.events, vec![good]);
        assert_eq!(
            read.truncated_at,
            Some(u64::try_from(complete.len()).expect("offset"))
        );
    }

    #[test]
    fn an_invalid_utf8_line_is_reported_as_corruption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        let frames = [message(1)];
        let mut bytes = body(&frames).into_bytes();
        bytes.splice(2..2, [0xFF_u8, 0xFE]);
        write_log(&path, "\n");
        std::fs::write(&path, &bytes).expect("overwrite");

        let err = read_log(&path).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
        assert_eq!(err.detail().invariant.as_deref(), Some("frame_encoding"));
    }

    #[test]
    fn a_blank_line_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        write_log(&path, "\n");

        let err = read_log(&path).expect_err("rejected");
        assert_eq!(err.detail().invariant.as_deref(), Some("frame_encoding"));
    }

    #[test]
    fn an_empty_or_missing_log_reads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        write_log(&path, "");
        let read = read_log(&path).expect("read");
        assert!(read.is_empty());
        assert!(!read.is_truncated());

        std::fs::remove_file(&path).expect("remove");
        let missing = read_log(&path).expect("read");
        assert!(missing.is_empty());
    }

    #[test]
    fn a_log_at_the_size_boundary_reads_and_one_byte_past_it_does_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        let limit = 160_u64;

        let base = message(1).encode().expect("encode");
        let padding = " ".repeat(
            usize::try_from(limit)
                .expect("limit")
                .saturating_sub(base.len())
                .saturating_sub(1),
        );
        let line = format!("{base}{padding}");
        assert_eq!(u64::try_from(line.len() + 1).expect("len"), limit);

        write_log(&path, &format!("{line}\n"));
        let read = read_log_limited(&path, limit).expect("at the limit");
        assert_eq!(read.len(), 1);

        write_log(&path, &format!("{line} \n"));
        let err = read_log_limited(&path, limit).expect_err("over the limit");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.detail().invariant.as_deref(), Some("log_size"));
    }

    #[test]
    fn an_oversize_frame_is_rejected_before_it_is_appended() {
        let current = MAX_LOG_BYTES.saturating_sub(10);
        check_log_capacity(current, 10).expect("at the cap");
        let err = check_log_capacity(current, 11).expect_err("over the cap");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.detail().observed.as_deref(), Some("268435457"));

        let err = check_log_capacity(u64::MAX, 1).expect_err("saturated");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn a_frame_past_the_production_frame_cap_is_rejected_on_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        let line = "x".repeat(MAX_FRAME_BYTES.saturating_add(1));
        write_log(&path, &format!("{line}\n"));

        let err = read_log(&path).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.detail().invariant.as_deref(), Some("frame_size"));
    }

    #[test]
    fn salvaging_a_gap_keeps_the_prefix_and_counts_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        write_log(
            &path,
            &body(&[message(1), message(2), message(4), message(5)]),
        );

        let salvage = salvage_log(&path).expect("salvage");
        assert_eq!(salvage.events, vec![message(1), message(2)]);
        assert_eq!(salvage.salvaged(), 2);
        assert_eq!(salvage.dropped_frames, 2);
        assert!(salvage.dropped_bytes > 0);
        assert!(!salvage.is_torn());
        let defect = salvage.defect().expect("defect");
        assert!(defect.message().contains("missing event 3"));
        assert!(salvage.clone().into_read().is_err());
    }

    #[test]
    fn salvaging_a_torn_tail_reports_it_as_torn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        let complete = body(&[message(1)]);
        let torn = format!("{complete}{}", message(2).encode().expect("encode"));
        let torn = &torn[..torn.len().saturating_sub(4)];
        write_log(&path, torn);

        let salvage = salvage_log(&path).expect("salvage");
        assert!(salvage.is_torn());
        assert!(salvage.defect().is_none());
        assert_eq!(salvage.dropped_frames, 1);
        assert_eq!(salvage.salvaged(), 1);
        let read = salvage.into_read().expect("read");
        assert_eq!(read.events, vec![message(1)]);
    }

    #[test]
    fn a_widened_log_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = log_path(&dir);
        write_log(&path, &body(&[message(1)]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("widen");
            let err = read_log(&path).expect_err("refused");
            assert_eq!(err.code(), ErrorCode::UnsafePath);
        }
    }

    #[test]
    fn a_sequence_number_parses_from_an_arbitrary_fragment() {
        assert_eq!(
            observed_seq("{\"schema\":1,\"seq\":412,\"x\":1}"),
            Some(412)
        );
        assert_eq!(observed_seq("{\"seq\":412}"), None);
        assert_eq!(observed_seq("junk"), None);
    }

    #[test]
    fn a_session_id_is_usable_as_a_directory_name() {
        let id = rune_core::id::SessionId::from_str("abcdefghijkl").expect("id");
        assert_eq!(id.as_str(), "abcdefghijkl");
    }
}
