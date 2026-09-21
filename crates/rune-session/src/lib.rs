//! Session persistence.
//!
//! One session is one directory holding an append-only JSONL event log. The log
//! is the authority: the metadata projection beside it can be deleted and
//! rebuilt, and a damaged session can be copied into a new one without the
//! original being touched.
//!
//! Single writer, many readers. A writer takes the advisory lock in
//! `session.lock`; a reader takes nothing and reads the last durable boundary.

#![forbid(unsafe_code)]
// Tests assert by panicking. The guards that forbid panicking apply to the
// shipped build, where a panic on corrupt input is a defect.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod event;
pub mod recovery;
pub mod store;

pub use event::{
    EventFrame, LogRead, LogSalvage, LogStop, MAX_FRAME_BYTES, MAX_LOG_BYTES, SCHEMA_VERSION,
    SessionEvent, check_log_capacity, read_log, read_log_limited, salvage_log, salvage_log_limited,
};
pub use recovery::{RecoveryDefect, RecoveryReport, recover};
pub use store::{
    EVENTS_FILE, LOCK_DEADLINE_MS, LOCK_FILE, LockHolder, Metadata, SessionState, SessionStore,
    UsageTotal, load_read_only, write_log_atomic,
};
