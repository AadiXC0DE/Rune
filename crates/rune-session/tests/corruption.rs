//! Corruption behavior of the event log.
//!
//! The log is the authority for a session, so every way it can be damaged has
//! to end in either the readable prefix or a typed error naming the sequence
//! number and the invariant. A panic here would mean a session that cannot be
//! diagnosed, and a silent short read would mean a session that looks intact
//! while missing events.

// Integration tests assert by panicking.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use camino::Utf8PathBuf;
use proptest::prelude::*;
use rune_core::error::{ErrorCode, Result};
use rune_core::id::EventSeq;
use rune_core::paths::write_private;
use rune_session::event::{EventFrame, LogRead, SessionEvent, read_log};
use rune_session::store::write_log_atomic;

/// Builds a log that exercises escaped newlines, non-ASCII text, and every
/// event kind, so a tear can land inside a multi-byte character or an escape.
fn rich_frames() -> Vec<EventFrame> {
    let events = vec![
        SessionEvent::TurnStarted { turn: 1 },
        SessionEvent::UserMessage {
            text: "first\nsecond\r\nthird\ttab".to_owned(),
        },
        SessionEvent::AssistantMessage {
            turn: 1,
            text: "café \u{1F600} \u{4F60}\u{597D}".to_owned(),
        },
        SessionEvent::ToolCall {
            call_id: "call_1".to_owned(),
            name: "read_file".to_owned(),
            arguments: "{\"path\":\"src/lib.rs\"}".to_owned(),
        },
        SessionEvent::ToolResult {
            call_id: "call_1".to_owned(),
            ok: false,
            output: "not found".to_owned(),
        },
        SessionEvent::UsageRecorded {
            input_tokens: u64::MAX,
            output_tokens: 0,
        },
        SessionEvent::TitleSet {
            title: "\"quoted\" \\ backslash".to_owned(),
        },
        SessionEvent::Compaction {
            through: 7,
            summary: "summary".to_owned(),
        },
    ];

    events
        .into_iter()
        .enumerate()
        .map(|(index, event)| {
            let seq = u64::try_from(index).expect("index").saturating_add(1);
            EventFrame::new(EventSeq(seq), 1_000_u64.saturating_add(seq), event)
        })
        .collect()
}

/// Encodes frames as the on-disk byte sequence, terminators included.
fn encoded(frames: &[EventFrame]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for frame in frames {
        bytes.extend_from_slice(frame.encode().expect("encode").as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

/// Asserts that a read of these bytes is a clean read or a typed error.
fn assert_readable(bytes: &[u8], dir: &tempfile::TempDir) -> Result<Option<LogRead>> {
    let path: Utf8PathBuf = dir.path().join("events.jsonl").try_into().expect("utf8");
    write_private(&path, "").expect("prepare");
    std::fs::write(&path, bytes).expect("write");

    match read_log(&path) {
        Ok(read) => Ok(Some(read)),
        Err(err) => Err(err),
    }
}

#[test]
fn every_truncation_of_a_valid_log_is_a_clean_read_or_a_typed_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let frames = rich_frames();
    let bytes = encoded(&frames);
    assert!(bytes.len() > 400, "fixture is too small to be interesting");

    for cut in 0..=bytes.len() {
        let truncated = &bytes[..cut];
        match assert_readable(truncated, &dir) {
            Ok(Some(read)) => {
                // Whatever was read must be a contiguous prefix from sequence
                // one, and must be exactly the frames whose lines survived.
                for (index, frame) in read.events.iter().enumerate() {
                    let expected = u64::try_from(index).expect("index").saturating_add(1);
                    assert_eq!(frame.seq, EventSeq(expected), "cut at byte {cut}");
                    assert_eq!(frame, &frames[index], "cut at byte {cut}");
                }
                // A read is torn exactly when the bytes do not end on a frame
                // boundary, and a tear always costs at least one frame.
                let on_boundary = truncated.is_empty() || truncated.ends_with(b"\n");
                assert_eq!(
                    read.truncated_at.is_some(),
                    !on_boundary,
                    "cut at byte {cut} of {} reported the wrong truncation",
                    bytes.len()
                );
                if read.truncated_at.is_some() {
                    assert!(read.events.len() < frames.len(), "cut at byte {cut}");
                }
            }
            Ok(None) => panic!("a read never returns nothing"),
            Err(err) => {
                // A typed error must name what failed.
                assert!(
                    !err.message().is_empty() || err.detail().invariant.is_some(),
                    "cut at byte {cut} produced a context-free error"
                );
            }
        }
    }
}

#[test]
fn truncation_never_loses_a_complete_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let frames = rich_frames();
    let bytes = encoded(&frames);
    let mut complete_prefix = 0_usize;

    for (index, frame) in frames.iter().enumerate() {
        let line = frame.encode().expect("encode");
        complete_prefix = complete_prefix.saturating_add(line.len()).saturating_add(1);
        let read = assert_readable(&bytes[..complete_prefix], &dir)
            .expect("a complete prefix reads")
            .expect("read");
        assert_eq!(read.events.len(), index.saturating_add(1));
        assert!(!read.is_truncated());
    }

    // One byte short of the terminator, the last frame is dropped but the
    // events before it survive.
    let read = assert_readable(&bytes[..complete_prefix.saturating_sub(1)], &dir)
        .expect("a torn final frame still reads")
        .expect("read");
    assert_eq!(read.events.len(), frames.len().saturating_sub(1));
    assert!(read.is_truncated());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_truncation_never_panics(seed in any::<u64>(), cut in any::<prop::sample::Index>()) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut frames = rich_frames();
        frames.push(EventFrame::new(
            EventSeq(9),
            seed,
            SessionEvent::UserMessage {
                text: format!("seed {seed}"),
            },
        ));
        let bytes = encoded(&frames);
        let cut = cut.index(bytes.len().saturating_add(1));
        let result = assert_readable(&bytes[..cut], &dir);

        match result {
            Ok(Some(read)) => {
                for (index, frame) in read.events.iter().enumerate() {
                    let expected = u64::try_from(index).expect("index").saturating_add(1);
                    prop_assert_eq!(frame.seq, EventSeq(expected));
                    prop_assert_eq!(frame, &frames[index]);
                }
            }
            Ok(None) => prop_assert!(false, "a read never returns nothing"),
            Err(err) => {
                prop_assert!(err.detail().invariant.is_some(), "{err}");
                prop_assert!(!err.message().is_empty());
            }
        }
    }

    #[test]
    fn arbitrary_bytes_in_place_of_a_frame_never_panic(
        mut bytes in prop::collection::vec(any::<u8>(), 0..512)
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path: Utf8PathBuf = dir.path().join("events.jsonl").try_into().expect("utf8");
        write_private(&path, "").expect("prepare");
        if let Some(last) = bytes.last_mut() {
            *last = b'\n';
        }
        std::fs::write(&path, &bytes).expect("write");

        let result = read_log(&path);
        match result {
            Ok(read) => prop_assert!(read.events.len() <= bytes.len().saturating_add(1)),
            Err(err) => {
                // Random bytes can only ever be rejected as corrupt, oversize,
                // or written at a schema version this build cannot read.
                prop_assert!(
                    matches!(
                        err.code(),
                        ErrorCode::CorruptRecord
                            | ErrorCode::TooLarge
                            | ErrorCode::UnsupportedVersion
                    ),
                    "unexpected code {:?}",
                    err.code()
                );
                prop_assert!(!err.message().is_empty());
            }
        }
    }
}

#[test]
fn a_rewrite_leaves_a_log_that_truncates_cleanly_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    let frames = rich_frames();
    write_log_atomic(dir.path().try_into().expect("utf8"), &frames).expect("rewrite");
    let bytes = std::fs::read(dir.path().join("events.jsonl")).expect("read");

    for cut in 0..=bytes.len() {
        assert_readable(&bytes[..cut], &dir).ok();
    }
}
