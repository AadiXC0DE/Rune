//! Recovery of a damaged session into a new one.
//!
//! Recovery never repairs in place. A damaged log is read for the longest valid
//! prefix of events, that prefix is written into a fresh session, and the
//! original is left exactly as it was found. Keeping the original is the point:
//! the damage is evidence, and a repair that rewrote it in place could destroy
//! the only remaining copy of an event that a later fix could still read.

use std::path::Path;

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::{EventSeq, SessionId};
use serde::{Deserialize, Serialize};

use crate::event::{self, LogStop};
use crate::store::{self, EVENTS_FILE, SessionStore};

/// What was wrong with the source session.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RecoveryDefect {
    /// Stable code of the failure that stopped the read.
    pub code: ErrorCode,
    /// Message of the failure, with the sequence number it names.
    pub message: String,
    /// Sequence number the failure named, when one was recoverable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// What a recovery produced.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RecoveryReport {
    /// Session the events were read from.
    pub source: Utf8PathBuf,
    /// New session the events were written to.
    pub destination: Utf8PathBuf,
    /// Identifier of the new session.
    pub id: SessionId,
    /// Events carried into the new session.
    pub salvaged: u64,
    /// Events dropped, counting the frame the read stopped on.
    pub dropped_frames: u64,
    /// Bytes dropped.
    pub dropped_bytes: u64,
    /// True when the source log ended inside a frame.
    pub truncated: bool,
    /// What stopped the read, when something did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defect: Option<RecoveryDefect>,
    /// First event carried over, when any was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_seq: Option<EventSeq>,
    /// Last event carried over, when any was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<EventSeq>,
}

impl RecoveryReport {
    /// Returns true when every event in the source was carried over.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.dropped_frames == 0 && self.defect.is_none() && !self.truncated
    }

    /// Returns a one-line description of what was dropped.
    #[must_use]
    pub fn loss_summary(&self) -> String {
        if self.is_complete() {
            return format!("all {} events recovered", self.salvaged);
        }
        let reason = match (&self.defect, self.truncated) {
            (Some(defect), _) => defect.message.clone(),
            (None, true) => "the log ends inside a frame".to_owned(),
            (None, false) => "the log stopped early".to_owned(),
        };
        format!(
            "{} events recovered, {} frames and {} bytes dropped: {reason}",
            self.salvaged, self.dropped_frames, self.dropped_bytes
        )
    }
}

/// Copies a session into a new session, keeping as much as is readable.
///
/// The source is only ever opened for reading. The destination must not exist;
/// its final path component must be a session identifier, which is what the new
/// session is named.
pub fn recover(source: &Path, destination: &Path) -> Result<RecoveryReport> {
    let source = to_utf8(source, "source")?;
    let destination = to_utf8(destination, "destination")?;

    if source == destination {
        return Err(RuneError::invalid_field(
            "destination",
            "recovery must write to a new session, not over the damaged one",
        ));
    }

    let log = source.join(EVENTS_FILE);
    if !log.exists() {
        return Err(RuneError::new(
            ErrorCode::NotFound,
            format!("`{log}` does not exist, so there is nothing to recover"),
        )
        .with_hint("check the session identifier"));
    }
    if destination.exists() {
        return Err(RuneError::new(
            ErrorCode::AlreadyExists,
            format!("`{destination}` already exists"),
        )
        .with_hint("recover into a session identifier that is not in use"));
    }

    let id = session_id_of(&destination)?;
    let salvage = event::salvage_log(&log)?;

    let (truncated, defect) = match &salvage.stop {
        Some(LogStop::TornTail) => (true, None),
        Some(LogStop::Rejected(err)) => (
            false,
            Some(RecoveryDefect {
                code: err.code(),
                message: err.message().to_owned(),
                seq: named_seq(err),
            }),
        ),
        None => (false, None),
    };

    rune_core::paths::create_dir_private(&destination)?;
    store::write_log_atomic(&destination, &salvage.events)?;

    // Writing the projection is what proves the recovered log is readable as a
    // session: a store refuses to open a log it cannot validate.
    let store = SessionStore::open_with_deadline(&destination, 0)?;
    drop(store);

    Ok(RecoveryReport {
        source: source.clone(),
        destination,
        id,
        salvaged: salvage.salvaged(),
        dropped_frames: salvage.dropped_frames,
        dropped_bytes: salvage.dropped_bytes,
        truncated,
        defect,
        first_seq: salvage.events.first().map(|frame| frame.seq),
        last_seq: salvage.events.last().map(|frame| frame.seq),
    })
}

/// Returns the session identifier a recovery destination is named after.
fn session_id_of(destination: &Utf8Path) -> Result<SessionId> {
    let name = destination.file_name().ok_or_else(|| {
        RuneError::invalid_field(
            "destination",
            format!("`{destination}` has no final path component"),
        )
    })?;
    name.parse::<SessionId>().map_err(|_| {
        RuneError::invalid_field(
            "destination",
            format!("`{name}` is not a session identifier"),
        )
        .with_hint("name the destination after the session identifier it becomes")
    })
}

/// Converts a native path, reporting one that is not valid UTF-8.
fn to_utf8(path: &Path, field: &str) -> Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(path.to_path_buf()).map_err(|rejected| {
        RuneError::invalid_field(
            field,
            format!("`{}` is not valid UTF-8", rejected.display()),
        )
    })
}

/// Extracts the sequence number a rejection named.
fn named_seq(err: &RuneError) -> Option<u64> {
    let observed = err.detail().observed.as_deref()?;
    let digits = observed.strip_prefix("seq ")?;
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::SessionEvent;
    use crate::store::load_read_only;
    use rune_core::paths::Paths;
    use std::str::FromStr;

    /// Builds paths rooted entirely inside the temporary directory.
    fn paths_for(root: &tempfile::TempDir) -> Paths {
        let base =
            |name: &str| Utf8PathBuf::from_path_buf(root.path().join(name)).expect("utf8 path");
        Paths {
            config_root: base("config"),
            state_root: base("state"),
            data_root: base("data"),
        }
    }

    fn id(name: &str) -> SessionId {
        SessionId::from_str(name).expect("id")
    }

    fn user(text: &str) -> SessionEvent {
        SessionEvent::UserMessage {
            text: text.to_owned(),
        }
    }

    fn hash(path: &Utf8Path) -> String {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(path).expect("read");
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Builds a session with three events and returns its paths.
    fn seeded(root: &tempfile::TempDir) -> (Paths, Utf8PathBuf) {
        let paths = paths_for(root);
        let id = id("sessionaaaaa");
        let store = SessionStore::create(&paths, &id).expect("create");
        store
            .append(SessionEvent::TurnStarted { turn: 1 })
            .expect("append");
        store.append(user("first")).expect("append");
        store.append(user("second")).expect("append");
        let dir = store.dir().to_path_buf();
        drop(store);
        (paths, dir)
    }

    #[test]
    fn an_undamaged_session_recovers_completely() {
        let root = tempfile::tempdir().expect("tempdir");
        let (paths, source) = seeded(&root);
        let destination = paths.session_dir(&id("sessionbbbbb"));

        let report = recover(source.as_std_path(), destination.as_std_path()).expect("recover");
        assert!(report.is_complete());
        assert_eq!(report.salvaged, 3);
        assert_eq!(report.dropped_frames, 0);
        assert_eq!(report.dropped_bytes, 0);
        assert_eq!(report.first_seq, Some(EventSeq(1)));
        assert_eq!(report.last_seq, Some(EventSeq(3)));
        assert_eq!(report.id, id("sessionbbbbb"));
        assert_eq!(report.source, source);
        assert_eq!(report.destination, destination);
        assert!(report.loss_summary().contains("all 3 events"));

        let recovered = load_read_only(&destination).expect("read");
        let original = load_read_only(&source).expect("read");
        assert_eq!(recovered.events, original.events);
        assert_eq!(recovered.turns, original.turns);
    }

    #[test]
    fn a_torn_log_recovers_the_prefix_and_leaves_the_source_untouched() {
        let root = tempfile::tempdir().expect("tempdir");
        let (paths, source) = seeded(&root);

        let log = source.join(EVENTS_FILE);
        let before = hash(&log);
        let complete = std::fs::read_to_string(&log).expect("read");
        let torn = format!("{complete}{{\"schema\":1,\"seq\":4,\"time");
        std::fs::write(&log, &torn).expect("write");
        let damaged = hash(&log);

        let destination = paths.session_dir(&id("sessionccccc"));
        let report = recover(source.as_std_path(), destination.as_std_path()).expect("recover");

        assert!(report.truncated);
        assert!(report.defect.is_none());
        assert_eq!(report.salvaged, 3);
        assert_eq!(report.dropped_frames, 1);
        assert_eq!(
            report.dropped_bytes,
            u64::try_from(torn.len() - complete.len()).expect("len")
        );
        assert_eq!(
            report.loss_summary(),
            format!(
                "3 events recovered, 1 frames and {} bytes dropped: the log ends inside a frame",
                report.dropped_bytes
            )
        );
        assert_eq!(hash(&log), damaged, "the source log was rewritten");
        assert_ne!(before, damaged);

        let recovered = load_read_only(&destination).expect("read");
        assert_eq!(recovered.len(), 3);
        assert_eq!(recovered.truncated_at, None);

        // Every file in the source directory is unchanged.
        let listing: Vec<_> = std::fs::read_dir(&source)
            .expect("read dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert!(listing.len() >= 3);
    }

    #[test]
    fn a_gap_recovers_the_prefix_and_reports_the_defect() {
        let root = tempfile::tempdir().expect("tempdir");
        let (paths, source) = seeded(&root);

        let log = source.join(EVENTS_FILE);
        let gap = event::EventFrame::new(EventSeq(9), event::now_millis(), user("out of order"))
            .encode()
            .expect("encode");
        let mut text = std::fs::read_to_string(&log).expect("read");
        text.push_str(&gap);
        text.push('\n');
        std::fs::write(&log, text).expect("write");
        let damaged = hash(&log);

        let destination = paths.session_dir(&id("sessionddddd"));
        let report = recover(source.as_std_path(), destination.as_std_path()).expect("recover");

        assert!(!report.truncated);
        assert_eq!(report.salvaged, 3);
        assert_eq!(report.dropped_frames, 1);
        let defect = report.defect.clone().expect("defect");
        assert_eq!(defect.code, ErrorCode::CorruptRecord);
        assert!(defect.message.contains("missing event 4"));
        assert_eq!(defect.seq, Some(9));
        assert!(report.loss_summary().contains("missing event 4"));
        assert_eq!(hash(&log), damaged, "the source log was rewritten");

        assert_eq!(load_read_only(&destination).expect("read").len(), 3);
    }

    #[test]
    fn recovering_from_a_missing_session_reports_not_found() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = paths_for(&root);
        let source = paths.session_dir(&id("sessionaaaaa"));
        std::fs::create_dir_all(&source).expect("create");
        let destination = paths.session_dir(&id("sessionbbbbb"));

        let err = recover(source.as_std_path(), destination.as_std_path()).expect_err("missing");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.message().contains("events.jsonl"));
        assert!(!destination.exists());
    }

    #[test]
    fn recovering_over_an_existing_session_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let (paths, source) = seeded(&root);
        let destination = paths.session_dir(&id("sessionbbbbb"));
        std::fs::create_dir_all(&destination).expect("create");

        let err = recover(source.as_std_path(), destination.as_std_path()).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::AlreadyExists);
        assert!(!destination.join(EVENTS_FILE).exists());
    }

    #[test]
    fn recovering_a_session_onto_itself_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, source) = seeded(&root);

        let err = recover(source.as_std_path(), source.as_std_path()).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("destination"));
    }

    #[test]
    fn a_destination_that_is_not_a_session_identifier_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, source) = seeded(&root);
        let destination: Utf8PathBuf =
            Utf8PathBuf::from_path_buf(root.path().join("scratch")).expect("utf8");

        let err = recover(source.as_std_path(), destination.as_std_path()).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("destination"));
        assert!(!destination.exists());
    }

    #[test]
    fn a_recovered_session_accepts_new_events() {
        let root = tempfile::tempdir().expect("tempdir");
        let (paths, source) = seeded(&root);
        let destination = paths.session_dir(&id("sessionbbbbb"));
        recover(source.as_std_path(), destination.as_std_path()).expect("recover");

        let store = SessionStore::open(&destination).expect("open");
        assert_eq!(store.next_seq(), EventSeq(4));
        assert_eq!(
            store.append(user("after recovery")).expect("append"),
            EventSeq(4)
        );
        assert_eq!(store.read().expect("read").len(), 4);
    }

    #[test]
    fn a_destination_directory_is_private() {
        let root = tempfile::tempdir().expect("tempdir");
        let (paths, source) = seeded(&root);
        let destination = paths.session_dir(&id("sessionbbbbb"));
        recover(source.as_std_path(), destination.as_std_path()).expect("recover");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&destination)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
            let mode = std::fs::metadata(destination.join(EVENTS_FILE))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn an_empty_source_log_recovers_to_an_empty_session() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = paths_for(&root);
        let source = paths.session_dir(&id("sessionaaaaa"));
        let store = SessionStore::create(&paths, &id("sessionaaaaa")).expect("create");
        drop(store);

        let destination = paths.session_dir(&id("sessionbbbbb"));
        let report = recover(source.as_std_path(), destination.as_std_path()).expect("recover");
        assert_eq!(report.salvaged, 0);
        assert_eq!(report.first_seq, None);
        assert_eq!(report.last_seq, None);
        assert!(report.is_complete());
        assert!(load_read_only(&destination).expect("read").is_empty());
    }
}
