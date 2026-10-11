//! Exact source messages removed by manual compaction.
//!
//! The session writer's lock also protects this archive. Atomic replacement
//! makes a failed write leave the old archive intact, and IDs never depend on
//! the live history positions, which compaction renumbers.

use camino::Utf8Path;
use rune_agent::history::Turn;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::{read_private, write_private_atomic};
use serde::{Deserialize, Serialize};

const FILE: &str = "compaction-archive.json";
const MAX_BYTES: u64 = rune_session::MAX_LOG_BYTES;

/// One original message, including every content part and provider replay state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Stable, session-local archive identifier, starting at one.
    pub id: u64,
    /// The message exactly as it appeared before compaction.
    pub message: Turn,
}

/// Reads the bounded archive, preserving original IDs and message content.
pub fn read(dir: &Utf8Path) -> Result<Vec<Record>> {
    let Some(contents) = read_private(&dir.join(FILE), MAX_BYTES)? else {
        return Ok(Vec::new());
    };
    let records: Vec<Record> = serde_json::from_str(&contents).map_err(|err| {
        RuneError::new(
            ErrorCode::CorruptRecord,
            format!("invalid compaction archive: {err}"),
        )
    })?;
    for (index, record) in records.iter().enumerate() {
        if record.id != (index as u64).saturating_add(1) {
            return Err(RuneError::new(
                ErrorCode::CorruptRecord,
                "compaction archive record IDs are not contiguous from one",
            ));
        }
    }
    Ok(records)
}

/// Durably saves the entire removed prefix before live history can change.
pub fn append(dir: &Utf8Path, messages: &[Turn]) -> Result<()> {
    append_limited(dir, messages, MAX_BYTES)
}

fn append_limited(dir: &Utf8Path, messages: &[Turn], max_bytes: u64) -> Result<()> {
    let mut records = read(dir)?;
    for message in messages {
        records.push(Record {
            id: (records.len() as u64).saturating_add(1),
            message: message.clone(),
        });
    }
    let contents = serde_json::to_string(&records)?;
    if contents.len() as u64 > max_bytes {
        return Err(RuneError::new(
            ErrorCode::TooLarge,
            format!("compaction archive exceeds its {max_bytes} byte limit"),
        ));
    }
    write_private_atomic(&dir.join(FILE), &contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_net::message::{ContentPart, ImageRef};

    #[test]
    fn original_parts_and_replay_survive_restart_and_later_renumbering() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = Utf8Path::from_path(temp.path()).expect("utf8");
        let mut message = Turn::assistant(
            7,
            vec![
                ContentPart::Text {
                    text: "intermediate reply\nCafé".into(),
                },
                ContentPart::Reasoning {
                    text: "original reasoning".into(),
                },
                ContentPart::ToolCall {
                    id: rune_core::id::ToolCallId::new("call_1").expect("id"),
                    name: "read_file".into(),
                    arguments: "{ \"path\": \"source.txt\" }".into(),
                },
                ContentPart::Image {
                    image: ImageRef {
                        id: 42,
                        media_type: "image/png".into(),
                        encoded_bytes: 100,
                    },
                },
            ],
        );
        message.replay = Some("opaque provider state".into());
        append(dir, std::slice::from_ref(&message)).expect("archive");
        let original = read(dir).expect("restart");
        assert_eq!(
            original,
            vec![Record {
                id: 1,
                message: message.clone()
            }]
        );
        // A later compaction can reuse a live position without reusing an ID.
        message.seq = 1;
        append(dir, std::slice::from_ref(&message)).expect("later archive");
        let records = read(dir).expect("second restart");
        assert_eq!(records[0], original[0]);
        assert_eq!(records[1], Record { id: 2, message });
    }

    #[test]
    fn archive_failure_preserves_prior_records_and_refuses_corrupt_ids() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = Utf8Path::from_path(temp.path()).expect("utf8");
        let message = Turn::user(1, "original");
        append(dir, std::slice::from_ref(&message)).expect("archive");
        let before = std::fs::read(dir.join(FILE)).expect("before");
        let error = append_limited(dir, std::slice::from_ref(&message), before.len() as u64)
            .expect_err("size cap");
        assert_eq!(error.code(), ErrorCode::TooLarge);
        assert_eq!(std::fs::read(dir.join(FILE)).expect("after"), before);
        write_private_atomic(
            dir.join(FILE).as_path(),
            &serde_json::to_string(&vec![Record {
                id: 2,
                message: message.clone(),
            }])
            .expect("json"),
        )
        .expect("corrupt ID");
        let before = std::fs::read(dir.join(FILE)).expect("corrupt archive");
        assert_eq!(
            append(dir, &[message]).expect_err("refused").code(),
            ErrorCode::CorruptRecord
        );
        assert_eq!(std::fs::read(dir.join(FILE)).expect("unchanged"), before);
    }
}
