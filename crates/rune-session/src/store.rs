//! Session directory layout, durable append, and the single-writer lock.
//!
//! A session directory holds one authority and one projection:
//!
//! - `events.jsonl` is the authority. Every other file can be deleted and
//!   rebuilt from it.
//! - `session.json` is the derived projection, written for tools that want the
//!   summary without replaying the log. Atomic replacement gives readers a
//!   complete old or new projection, even if the writer is killed.
//! - `session.lock` is the advisory writer lock.
//!
//! One writer and any number of readers. A reader takes no lock and always sees
//! the last durable boundary: appends are a single `write` of a whole line
//! followed by an fsync, so a reader either sees the line or does not.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::str::FromStr;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::{EventSeq, SessionId};
use rune_core::paths::{self, Paths};
use serde::{Deserialize, Serialize};

use crate::event::{self, EventFrame, SessionEvent};

/// Name of the authoritative event log.
pub const EVENTS_FILE: &str = "events.jsonl";
/// Name of the derived metadata projection.
pub const METADATA_FILE: &str = "session.json";
/// Name of the advisory writer lock.
pub const LOCK_FILE: &str = "session.lock";

/// File holding the record of the writer that currently holds the lock.
///
/// Beside the lock file rather than inside it. A lock is held on the file it
/// names, and on a platform where that lock is mandatory the holder's own record
/// would be unreadable exactly when a refused writer needs to name the holder.
pub const HOLDER_FILE: &str = "session.holder";

/// Schema version of the metadata projection.
pub const METADATA_VERSION: u32 = 1;

/// Deadline for acquiring the writer lock, in milliseconds.
///
/// No tunable limit covers lock acquisition, and the choice is deliberate: a
/// writer holds the lock for the duration of one turn, so a ceiling measured in
/// seconds is long enough to ride out a slow append and short enough that a
/// user sees a clear failure rather than a hang.
pub const LOCK_DEADLINE_MS: u64 = 2_000;

/// Interval between lock attempts, in milliseconds.
const LOCK_POLL_MS: u64 = 5;

/// Token balance accumulated over a session.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct UsageTotal {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
}

impl UsageTotal {
    /// Adds one report, saturating rather than wrapping.
    pub fn record(&mut self, input_tokens: u64, output_tokens: u64) {
        self.input_tokens = self.input_tokens.saturating_add(input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(output_tokens);
    }

    /// Returns the combined count.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// The derived projection written to `session.json`.
///
/// Every field is derived from the event log, which is what makes the file
/// disposable.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Metadata {
    /// Schema version of this projection.
    pub schema: u32,
    /// Session the projection describes.
    pub id: SessionId,
    /// Events recorded.
    pub events: u64,
    /// Last sequence number, or zero when the log is empty.
    pub last_seq: u64,
    /// Turns started.
    pub turns: u64,
    /// Accumulated usage.
    pub usage: UsageTotal,
    /// Session title, absent until one is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// A session as a reader sees it: the events plus what they add up to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionState {
    /// Session the state came from.
    pub id: SessionId,
    /// Events read, contiguous from sequence one.
    pub events: Vec<EventFrame>,
    /// Turns started.
    pub turns: u64,
    /// Accumulated usage.
    pub usage: UsageTotal,
    /// Session title, when one was set.
    pub title: Option<String>,
    /// Workspace the session ran in, absent when none was recorded.
    pub workspace: Option<String>,
    /// Parent session, when this session is a child of another.
    pub parent: Option<String>,
    /// Byte offset where a torn final frame was dropped, when one was.
    pub truncated_at: Option<u64>,
}

impl SessionState {
    /// Finds saved conversation records by a case-insensitive literal fragment.
    ///
    /// Searches the authoritative frames, including records removed from model
    /// context by compaction and saved streaming fragments. Matches retain their
    /// original sequence, timestamp, schema and event content in log order.
    /// Metadata and record identifiers are not searched.
    pub fn search_records(&self, query: &str) -> Result<Vec<&EventFrame>> {
        if query.trim().is_empty() {
            return Err(RuneError::invalid_field("search", "search text is empty")
                .with_hint("pass a message, tool argument, or tool result fragment"));
        }
        let query = query.to_lowercase();
        let contains = |text: &str| text.to_lowercase().contains(&query);
        Ok(self
            .events
            .iter()
            .filter(|frame| match &frame.event {
                SessionEvent::UserMessage { text }
                | SessionEvent::AssistantMessage { text, .. }
                | SessionEvent::AssistantDelta { text, .. } => contains(text),
                SessionEvent::ToolCall {
                    name, arguments, ..
                } => contains(name) || contains(arguments),
                SessionEvent::ToolResult { output, .. } => contains(output),
                SessionEvent::Compaction { summary, .. } => contains(summary),
                _ => false,
            })
            .collect())
    }

    /// Writes the saved frames as stable schema-versioned JSONL in log order.
    ///
    /// Only typed event fields are serialized, excluding metadata, credentials,
    /// and unknown transport fields. Recorded message and tool content is kept
    /// verbatim. No export-time values or synthesized replay events are added.
    /// A torn tail, if present, is excluded as it is when reading the session.
    pub fn export_jsonl(&self, mut writer: impl Write) -> Result<()> {
        for frame in &self.events {
            writeln!(writer, "{}", frame.encode()?)?;
        }
        writer.flush()?;
        Ok(())
    }

    /// Returns the last sequence number read.
    #[must_use]
    pub fn last_seq(&self) -> Option<EventSeq> {
        self.events.last().map(|frame| frame.seq)
    }

    /// Returns the number of events read.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Returns true when no event was read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Identity of the process holding the writer lock.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LockHolder {
    /// Process id of the holder.
    pub pid: u32,
    /// Host the holder runs on, when the platform reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Milliseconds since the Unix epoch when the lock was taken.
    pub since_ms: u64,
}

/// Running totals derived from the event log.
#[derive(Clone, Debug)]
struct Projection {
    frames: u64,
    bytes: u64,
    last_seq: EventSeq,
    last_ts: u64,
    turns: u64,
    usage: UsageTotal,
    title: Option<String>,
    workspace: Option<String>,
    parent: Option<String>,
}

/// An open session directory with the writer lock held.
///
/// Dropping the store clears the holder record before releasing the lock.
#[derive(Debug)]
pub struct SessionStore {
    id: SessionId,
    dir: Utf8PathBuf,
    log: File,
    /// The writer lock, held until drop clears the holder record and unlocks it.
    lock: File,
    projection: std::cell::RefCell<Projection>,
}

/// A successfully synced event, with a separate disposable projection outcome.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AppendReport {
    /// Sequence number of the durable event. Do not retry this event.
    pub seq: EventSeq,
    /// Repair warning when refreshing `session.json` failed.
    ///
    /// The event and in-memory totals are already committed. Rebuilding metadata
    /// or reopening the session after fixing the filesystem repairs the summary.
    pub metadata_warning: Option<RuneError>,
}

impl SessionStore {
    /// Creates a session directory and takes the writer lock.
    ///
    /// Fails when the session already has an event log, because resuming is
    /// [`SessionStore::open`] rather than creation.
    pub fn create(paths: &Paths, id: &SessionId) -> Result<Self> {
        paths.ensure_roots()?;
        paths::create_dir_private(&paths.sessions_dir())?;

        let dir = paths.session_dir(id);
        let events = dir.join(EVENTS_FILE);
        if events.exists() {
            return Err(RuneError::new(
                ErrorCode::AlreadyExists,
                format!("session `{id}` already has an event log"),
            )
            .with_hint("open the session to resume it"));
        }

        paths::create_dir_private(&dir)?;
        Self::open_with_deadline(&dir, LOCK_DEADLINE_MS)
    }

    /// Opens an existing session directory and takes the writer lock.
    pub fn open(dir: &Utf8Path) -> Result<Self> {
        Self::open_with_deadline(dir, LOCK_DEADLINE_MS)
    }

    /// Opens a session directory, waiting at most `deadline_ms` for the lock.
    ///
    /// A torn final frame is dropped here, while the writer lock is held, so the
    /// next append starts at the last durable boundary. Appending onto a
    /// fragment instead would merge it with the new frame and turn one damaged
    /// line into a log that can no longer be read at all.
    pub fn open_with_deadline(dir: &Utf8Path, deadline_ms: u64) -> Result<Self> {
        let id = id_from_dir(dir)?;
        let lock = acquire_lock(dir, &id, deadline_ms)?;

        let log_path = dir.join(EVENTS_FILE);
        if !log_path.exists() {
            paths::write_private(&log_path, "")?;
        }
        let state = load_read_only(dir)?;
        let bytes = match state.truncated_at {
            Some(offset) => {
                let file = OpenOptions::new().write(true).open(&log_path)?;
                file.set_len(offset)?;
                file.sync_all()?;
                offset
            }
            None => std::fs::metadata(&log_path)?.len(),
        };

        let log = OpenOptions::new().append(true).open(&log_path)?;
        let store = Self {
            id,
            dir: dir.to_path_buf(),
            log,
            lock,
            projection: std::cell::RefCell::new(Projection {
                frames: u64::try_from(state.events.len()).unwrap_or(u64::MAX),
                bytes,
                last_seq: state.last_seq().unwrap_or(EventSeq(0)),
                last_ts: state.events.last().map_or(0, |frame| frame.timestamp_ms),
                turns: state.turns,
                usage: state.usage,
                title: state.title.clone(),
                workspace: state.workspace.clone(),
                parent: state.parent.clone(),
            }),
        };
        store.rebuild_metadata()?;
        Ok(store)
    }

    /// Returns the session identifier.
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Returns the session directory.
    #[must_use]
    pub fn dir(&self) -> &Utf8Path {
        &self.dir
    }

    /// Returns the path of the authoritative event log.
    #[must_use]
    pub fn events_path(&self) -> Utf8PathBuf {
        self.dir.join(EVENTS_FILE)
    }

    /// Returns the path of the derived metadata projection.
    #[must_use]
    pub fn metadata_path(&self) -> Utf8PathBuf {
        self.dir.join(METADATA_FILE)
    }

    /// Returns the sequence number the next append will use.
    #[must_use]
    pub fn next_seq(&self) -> EventSeq {
        self.projection.borrow().last_seq.next()
    }

    /// Returns the accumulated usage.
    #[must_use]
    pub fn usage(&self) -> UsageTotal {
        self.projection.borrow().usage
    }

    /// Returns the session title.
    #[must_use]
    pub fn title(&self) -> Option<String> {
        self.projection.borrow().title.clone()
    }

    /// Returns the number of turns started.
    #[must_use]
    pub fn turns(&self) -> u64 {
        self.projection.borrow().turns
    }

    /// Returns the derived metadata for this session.
    #[must_use]
    pub fn metadata(&self) -> Metadata {
        let projection = self.projection.borrow();
        Metadata {
            schema: METADATA_VERSION,
            id: self.id,
            events: projection.frames,
            last_seq: projection.last_seq.0,
            turns: projection.turns,
            usage: projection.usage,
            title: projection.title.clone(),
        }
    }

    /// Appends one event and fsyncs it, returning its durable sequence number
    /// and any metadata repair warning separately.
    ///
    /// The line is written whole, so a reader sees either all of it or none of
    /// it, and a torn tail can only ever be the last frame of the file.
    ///
    /// Once the event sync succeeds, projection failure cannot fail the append.
    /// Callers should report `metadata_warning` without retrying the event.
    pub fn append(&self, event: SessionEvent) -> Result<AppendReport> {
        let mut projection = self.projection.borrow_mut();
        let seq = projection.last_seq.next();
        let timestamp_ms = event::now_millis().max(projection.last_ts);
        let frame = EventFrame::new(seq, timestamp_ms, event);
        let line = frame.encode()?;

        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        event::check_log_capacity(projection.bytes, bytes.len())?;

        let file = &self.log;
        (&*file).write_all(&bytes)?;
        file.sync_data()?;

        projection.bytes = projection.bytes.saturating_add(bytes.len() as u64);
        projection.frames = projection.frames.saturating_add(1);
        projection.last_seq = seq;
        projection.last_ts = timestamp_ms;
        match frame.event {
            SessionEvent::TurnStarted { .. } => {
                projection.turns = projection.turns.saturating_add(1);
            }
            SessionEvent::UsageRecorded {
                input_tokens,
                output_tokens,
            } => projection.usage.record(input_tokens, output_tokens),
            SessionEvent::TitleSet { title } => projection.title = Some(title),
            SessionEvent::WorkspaceSet { workspace } => {
                projection.workspace = Some(workspace.clone());
            }
            SessionEvent::ChildOf { parent } => projection.parent = Some(parent.clone()),
            _ => {}
        }

        // Refreshed after every durable append. Until replacement completes,
        // readers can still see the previous complete projection.
        let metadata_warning = self.write_metadata(&projection).err().map(|error| {
            RuneError::new(
                error.code(),
                format!(
                    "session `{}` event {seq} is durable, but metadata refresh failed: {error}; repair session.json after fixing the filesystem, or reopen the session; do not retry this event",
                    self.id
                ),
            )
            .with_hint("fix the metadata path, then rebuild metadata or reopen the session")
        });
        Ok(AppendReport {
            seq,
            metadata_warning,
        })
    }

    /// Rewrites `session.json` from the event log on disk.
    ///
    /// Deleting the projection is always recoverable: it holds nothing that is
    /// not in the log.
    pub fn rebuild_metadata(&self) -> Result<()> {
        let projection = self.projection.borrow();
        self.write_metadata(&projection)
    }

    /// Serializes and atomically replaces the projection.
    fn write_metadata(&self, projection: &Projection) -> Result<()> {
        let metadata = Metadata {
            schema: METADATA_VERSION,
            id: self.id,
            events: projection.frames,
            last_seq: projection.last_seq.0,
            turns: projection.turns,
            usage: projection.usage,
            title: projection.title.clone(),
        };
        let mut text = serde_json::to_string_pretty(&metadata)?;
        text.push('\n');
        paths::write_private_atomic(&self.metadata_path(), &text)
    }

    /// Reads this session without taking the lock.
    pub fn read(&self) -> Result<SessionState> {
        load_read_only(&self.dir)
    }
}

impl Drop for SessionStore {
    fn drop(&mut self) {
        // Clear diagnostics while we still own the lock. After unlock, another
        // writer can acquire it and record its identity, which must survive our
        // drop. Closing the handle also releases the lock if unlock fails.
        let _ = paths::write_private(&self.dir.join(HOLDER_FILE), "");
        let _ = self.lock.unlock();
        #[cfg(test)]
        if let Some(after_unlock) = tests::AFTER_UNLOCK.with_borrow_mut(Option::take) {
            after_unlock();
        }
    }
}

/// Reads a session directory without taking the writer lock.
///
/// Returns the last durable boundary, which is where a concurrent writer may or
/// may not have landed. A torn final frame is dropped and reported in
/// `truncated_at`; any other defect fails, because reading a damaged log as a
/// shorter conversation would be a silent loss.
pub fn load_read_only(dir: &Utf8Path) -> Result<SessionState> {
    let id = id_from_dir(dir)?;
    let read = event::read_log(&dir.join(EVENTS_FILE))?;
    let mut state = SessionState {
        id,
        events: read.events,
        turns: 0,
        usage: UsageTotal::default(),
        title: None,
        workspace: None,
        parent: None,
        truncated_at: read.truncated_at,
    };
    for frame in &state.events {
        match &frame.event {
            SessionEvent::TurnStarted { .. } => state.turns = state.turns.saturating_add(1),
            SessionEvent::UsageRecorded {
                input_tokens,
                output_tokens,
            } => state.usage.record(*input_tokens, *output_tokens),
            SessionEvent::TitleSet { title } => state.title = Some(title.clone()),
            SessionEvent::WorkspaceSet { workspace } => {
                state.workspace = Some(workspace.clone());
            }
            SessionEvent::ChildOf { parent } => state.parent = Some(parent.clone()),
            _ => {}
        }
    }
    Ok(state)
}

/// Writes a complete event log atomically, replacing any existing one.
///
/// The content is staged beside the log and renamed over it, so a crash during
/// a rewrite leaves either the old log or the new one, never a mixture. Used by
/// recovery, which must never leave a half-written log behind.
pub fn write_log_atomic(dir: &Utf8Path, frames: &[EventFrame]) -> Result<()> {
    let target = dir.join(EVENTS_FILE);
    let temp = dir.join(format!("{EVENTS_FILE}.rewrite"));

    let mut text = String::new();
    for frame in frames {
        text.push_str(&frame.encode()?);
        text.push('\n');
    }
    event::check_log_capacity(0, text.len())?;

    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(paths::FILE_MODE);
    }
    {
        let mut file = options.open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, &target)?;
    Ok(())
}

/// Creates the lock file if it is absent.
///
/// Separate from [`acquire_lock`] so a caller can prepare a session directory
/// before any writer exists.
pub fn ensure_lock_file(dir: &Utf8Path) -> Result<()> {
    let path = dir.join(LOCK_FILE);
    if path.exists() {
        return Ok(());
    }
    paths::write_private(&path, "")
}

/// Takes the advisory writer lock, waiting at most `deadline_ms`.
fn acquire_lock(dir: &Utf8Path, id: &SessionId, deadline_ms: u64) -> Result<File> {
    ensure_lock_file(dir)?;
    let path = dir.join(LOCK_FILE);

    let meta = std::fs::symlink_metadata(&path)?;
    if meta.file_type().is_symlink() {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` is a symbolic link"),
        )
        .with_hint("the writer lock must be a real file"));
    }

    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(paths::FILE_MODE);
    }
    let file = options.open(&path)?;

    let started = Instant::now();
    let deadline = Duration::from_millis(deadline_ms);
    loop {
        match file.try_lock() {
            Ok(()) => {
                record_holder(dir)?;
                return Ok(file);
            }
            Err(TryLockError::WouldBlock) => {
                let elapsed = started.elapsed();
                if elapsed >= deadline {
                    return Err(locked_error(dir, id));
                }
                let remaining = deadline.saturating_sub(elapsed);
                std::thread::sleep(remaining.min(Duration::from_millis(LOCK_POLL_MS)));
            }
            Err(TryLockError::Error(err)) => return Err(err.into()),
        }
    }
}

/// Writes the holder record beside the lock file.
///
/// Best effort, and deliberately so: the record is diagnostic text, while the
/// lock itself is held by the operating system. Only one writer exists at a
/// time. Both recording and clearing happen with the lock held, so an old
/// writer cannot erase its successor's record. Readers can briefly observe an
/// empty record during acquisition or release.
fn record_holder(dir: &Utf8Path) -> Result<()> {
    let holder = LockHolder {
        pid: std::process::id(),
        host: std::env::var("HOSTNAME")
            .ok()
            .or_else(|| std::env::var("HOST").ok())
            .filter(|value| !value.is_empty()),
        since_ms: event::now_millis(),
    };
    paths::write_private(&dir.join(HOLDER_FILE), &serde_json::to_string(&holder)?)
}

/// Builds the error reported when the writer lock cannot be taken.
fn locked_error(dir: &Utf8Path, id: &SessionId) -> RuneError {
    let holder = std::fs::read_to_string(dir.join(HOLDER_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<LockHolder>(&text).ok());

    let by = match &holder {
        Some(holder) => match &holder.host {
            Some(host) => format!("pid {} on {host}", holder.pid),
            None => format!("pid {}", holder.pid),
        },
        None => "another process".to_owned(),
    };

    RuneError::new(
        ErrorCode::Locked,
        format!("session `{id}` is locked by {by}, directory {dir}"),
    )
    .with_observed(holder.map_or_else(|| "unknown".to_owned(), |holder| holder.pid.to_string()))
    .with_hint("wait for the writer to finish, or read the session without the lock")
}

/// Returns the session identifier encoded in a directory name.
fn id_from_dir(dir: &Utf8Path) -> Result<SessionId> {
    let name = dir.file_name().ok_or_else(|| {
        RuneError::invalid_field("session_dir", format!("`{dir}` has no final component"))
    })?;
    SessionId::from_str(name).map_err(|_| {
        RuneError::invalid_field(
            "session_id",
            format!("`{name}` is not a session identifier"),
        )
        .with_observed(name.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{LogRead, read_log};

    std::thread_local! {
        /// Forces a handoff immediately after unlock, before drop continues.
        pub(super) static AFTER_UNLOCK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            std::cell::RefCell::new(None);
    }

    /// Builds paths rooted entirely inside the temporary directory.
    ///
    /// Every root is set explicitly: a relative default would resolve against
    /// the workspace and make two test binaries fight over the same directory.
    fn paths_for(root: &tempfile::TempDir) -> Paths {
        let base =
            |name: &str| Utf8PathBuf::from_path_buf(root.path().join(name)).expect("utf8 path");
        Paths {
            config_root: base("config"),
            state_root: base("state"),
            data_root: base("data"),
        }
    }

    fn store_in(root: &tempfile::TempDir) -> (Paths, SessionStore) {
        let paths = paths_for(root);
        let id = SessionId::from_str("sessionaaaaa").expect("id");
        let store = SessionStore::create(&paths, &id).expect("create");
        (paths, store)
    }

    fn user(text: &str) -> SessionEvent {
        SessionEvent::UserMessage {
            text: text.to_owned(),
        }
    }

    #[test]
    fn creating_a_session_makes_a_private_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let (paths, store) = store_in(&root);

        let dir = paths.session_dir(&store.id());
        assert!(dir.is_dir());
        assert_eq!(store.dir(), dir.as_path());
        assert!(store.events_path().is_file());
        assert!(store.metadata_path().is_file());
        assert!(dir.join(LOCK_FILE).is_file());
        assert_eq!(store.next_seq(), EventSeq::FIRST);
        assert_eq!(store.turns(), 0);
        assert_eq!(store.usage(), UsageTotal::default());
        assert_eq!(store.title(), None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
            for name in [EVENTS_FILE, METADATA_FILE, LOCK_FILE] {
                let mode = std::fs::metadata(dir.join(name))
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "{name} was {mode:o}");
            }
        }
    }

    #[test]
    fn creating_over_an_existing_log_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let (paths, store) = store_in(&root);
        store.append(user("hello")).expect("append");

        let err = SessionStore::create(&paths, &store.id()).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::AlreadyExists);
    }

    #[test]
    fn an_appended_log_re_reads_identically() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);

        let events = [
            SessionEvent::TurnStarted { turn: 1 },
            user("first"),
            SessionEvent::AssistantMessage {
                turn: 1,
                text: "answer".to_owned(),
            },
            SessionEvent::UsageRecorded {
                input_tokens: 12,
                output_tokens: 4,
            },
            SessionEvent::TitleSet {
                title: "a title".to_owned(),
            },
        ];
        for (index, event) in events.iter().enumerate() {
            let report = store.append(event.clone()).expect("append");
            assert!(report.metadata_warning.is_none());
            let seq = report.seq;
            assert_eq!(
                seq,
                EventSeq(u64::try_from(index).unwrap_or(0).saturating_add(1))
            );
        }

        let read = store.read().expect("read");
        assert_eq!(read.events.len(), events.len());
        for (frame, event) in read.events.iter().zip(events.iter()) {
            assert_eq!(&frame.event, event);
        }
        assert_eq!(read.turns, 1);
        assert_eq!(read.usage.input_tokens, 12);
        assert_eq!(read.usage.output_tokens, 4);
        assert_eq!(read.title.as_deref(), Some("a title"));
        assert_eq!(read.truncated_at, None);

        let from_disk = read_log(&store.events_path()).expect("read log");
        assert_eq!(
            from_disk,
            LogRead {
                events: read.events,
                truncated_at: None
            }
        );
    }

    #[test]
    fn a_reader_sees_an_appended_event_without_the_lock() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);

        let reader = store.read().expect("read while locked");
        assert!(reader.is_empty());
        store
            .append(user("written while the reader ran"))
            .expect("append");
        let after = load_read_only(store.dir()).expect("read while locked");
        assert_eq!(after.len(), 1);
        assert_eq!(after.turns, 0);
    }

    #[test]
    fn appending_never_rewrites_earlier_bytes() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);

        store.append(user("one")).expect("append");
        let first = std::fs::read(store.events_path()).expect("read");
        store.append(user("two")).expect("append");
        let both = std::fs::read(store.events_path()).expect("read");

        assert!(both.starts_with(&first));
        assert!(both.len() > first.len());
        let line = store.read().expect("read").events[0]
            .encode()
            .expect("encode");
        assert_eq!(&both[..first.len()], format!("{line}\n").as_bytes());
    }

    #[test]
    fn timestamps_do_not_go_backwards_when_the_clock_does() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);

        store.append(user("one")).expect("append");
        {
            let mut projection = store.projection.borrow_mut();
            projection.last_ts = u64::MAX;
        }
        store.append(user("two")).expect("append");

        let read = store.read().expect("read");
        assert_eq!(read.events[1].timestamp_ms, u64::MAX);
    }

    #[test]
    fn deleting_the_derived_metadata_rebuilds_byte_identically() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);
        store
            .append(SessionEvent::TurnStarted { turn: 1 })
            .expect("append");
        store.append(user("hello")).expect("append");
        store
            .append(SessionEvent::TitleSet {
                title: "t".to_owned(),
            })
            .expect("append");

        let before = std::fs::read(store.metadata_path()).expect("read");
        std::fs::remove_file(store.metadata_path()).expect("remove");
        assert!(!store.metadata_path().exists());

        store.rebuild_metadata().expect("rebuild");
        let after = std::fs::read(store.metadata_path()).expect("read");
        assert_eq!(before, after);
        assert_eq!(
            serde_json::from_slice::<Metadata>(&after).expect("parse"),
            store.metadata()
        );
    }

    #[test]
    fn metadata_failure_reports_durable_events_and_allows_repair_without_retry() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);
        std::fs::remove_file(store.metadata_path()).expect("remove metadata");
        std::fs::create_dir(store.metadata_path()).expect("block replacement");

        let events = [
            SessionEvent::TurnStarted { turn: 1 },
            SessionEvent::UsageRecorded {
                input_tokens: 7,
                output_tokens: 2,
            },
            SessionEvent::TitleSet {
                title: "durable".to_owned(),
            },
        ];
        for (index, event) in events.iter().enumerate() {
            let report = store.append(event.clone()).expect("durable append");
            let seq = EventSeq((index as u64).saturating_add(1));
            assert_eq!(report.seq, seq);
            let warning = report.metadata_warning.expect("repair warning");
            assert!(
                warning
                    .message()
                    .contains(&format!("event {seq} is durable"))
            );
            assert!(warning.message().contains("metadata refresh failed"));
            assert!(warning.message().contains("do not retry this event"));
            assert!(
                warning
                    .detail()
                    .hint
                    .as_deref()
                    .expect("hint")
                    .contains("rebuild")
            );
            let read = load_read_only(store.dir()).expect("replay despite metadata failure");
            assert_eq!(read.len(), index + 1);
            assert_eq!(read.events[index].event, *event);
            assert_eq!(read.last_seq(), Some(seq));
            assert_eq!(store.next_seq(), seq.next());
        }
        assert_eq!(store.turns(), 1);
        assert_eq!(store.usage().total(), 9);
        assert_eq!(store.title().as_deref(), Some("durable"));
        assert_eq!(store.metadata().events, 3);
        assert!(store.rebuild_metadata().is_err(), "repair remains fallible");

        let log = std::fs::read(store.events_path()).expect("durable log");
        std::fs::remove_dir(store.metadata_path()).expect("unblock replacement");
        store.rebuild_metadata().expect("repair metadata");
        assert_eq!(std::fs::read(store.events_path()).expect("log"), log);
        assert_eq!(
            serde_json::from_slice::<Metadata>(
                &std::fs::read(store.metadata_path()).expect("metadata")
            )
            .expect("parse metadata"),
            store.metadata()
        );
        let report = store
            .append(user("next event"))
            .expect("append after repair");
        assert_eq!(report.seq, EventSeq(4));
        assert!(report.metadata_warning.is_none());
        let dir = store.dir().to_owned();
        drop(store);
        let reopened = SessionStore::open(&dir).expect("reopen");
        assert_eq!(reopened.read().expect("read").len(), 4);
        assert_eq!(reopened.next_seq(), EventSeq(5));
    }

    #[test]
    fn event_write_failure_still_fails_without_advancing_the_projection() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, mut store) = store_in(&root);
        let before = std::fs::read(store.metadata_path()).expect("metadata");
        store.log = File::open(store.events_path()).expect("read-only log handle");

        store
            .append(user("cannot write"))
            .expect_err("append failed");
        assert_eq!(store.next_seq(), EventSeq::FIRST);
        assert_eq!(store.metadata().events, 0);
        assert!(store.read().expect("read").is_empty());
        assert_eq!(
            std::fs::read(store.metadata_path()).expect("metadata"),
            before
        );
    }

    #[test]
    fn metadata_survives_a_reopen() {
        let root = tempfile::tempdir().expect("tempdir");
        let id = SessionId::from_str("sessionaaaaa").expect("id");
        let paths = paths_for(&root);
        let dir = paths.session_dir(&id);
        {
            let store = SessionStore::create(&paths, &id).expect("create");
            store
                .append(SessionEvent::TurnStarted { turn: 1 })
                .expect("append");
            store
                .append(SessionEvent::UsageRecorded {
                    input_tokens: 7,
                    output_tokens: 2,
                })
                .expect("append");
            store.append(user("resumed")).expect("append");
            store
                .append(SessionEvent::TitleSet {
                    title: "kept".to_owned(),
                })
                .expect("append");
        }

        let reopened = SessionStore::open(&dir).expect("open");
        assert_eq!(reopened.next_seq(), EventSeq(5));
        assert_eq!(reopened.turns(), 1);
        assert_eq!(reopened.usage().input_tokens, 7);
        assert_eq!(reopened.title().as_deref(), Some("kept"));
        assert_eq!(
            reopened.append(user("after")).expect("append").seq,
            EventSeq(5)
        );
        assert_eq!(reopened.read().expect("read").len(), 5);
    }

    #[cfg(unix)]
    #[test]
    fn metadata_replacement_preserves_an_open_readers_complete_projection() {
        use std::io::Read;

        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);
        for title in ["a long title 🦀".repeat(1024), "short".to_owned()] {
            let before = std::fs::read(store.metadata_path()).expect("old metadata");
            let mut reader = File::open(store.metadata_path()).expect("open reader");

            store
                .append(SessionEvent::TitleSet { title })
                .expect("append");

            let mut retained = Vec::new();
            reader.read_to_end(&mut retained).expect("read old handle");
            assert_eq!(retained, before, "an existing reader retains the old file");
            let after = std::fs::read(store.metadata_path()).expect("new metadata");
            assert_ne!(after, before);
            assert_eq!(
                serde_json::from_slice::<Metadata>(&after).expect("complete new metadata"),
                store.metadata()
            );
            assert!(after.ends_with(b"\n"));
        }
    }

    #[test]
    fn a_second_writer_fails_naming_the_session_and_the_holder() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);

        let err = SessionStore::open_with_deadline(store.dir(), 0).expect_err("locked");
        assert_eq!(err.code(), ErrorCode::Locked);
        assert_eq!(err.field(), None);
        assert!(
            err.message().contains(store.id().as_str()),
            "{}",
            err.message()
        );
        assert!(
            err.message().contains(&std::process::id().to_string()),
            "{}",
            err.message()
        );
        assert!(err.detail().hint.is_some());

        // The holder still works, and a reader still reads.
        assert_eq!(
            store.append(user("mine")).expect("append").seq,
            EventSeq::FIRST
        );
        assert_eq!(load_read_only(store.dir()).expect("read").len(), 1);
    }

    #[test]
    fn a_released_lock_clears_the_holder_and_admits_the_next_writer() {
        let root = tempfile::tempdir().expect("tempdir");
        let (paths, store) = store_in(&root);
        let dir = paths.session_dir(&store.id());
        store.append(user("first writer")).expect("append");
        let holder_path = dir.join(HOLDER_FILE);
        assert!(
            !std::fs::read_to_string(&holder_path)
                .expect("read")
                .is_empty()
        );
        drop(store);

        assert!(
            std::fs::read_to_string(&holder_path)
                .expect("read")
                .is_empty()
        );
        let next = SessionStore::open_with_deadline(&dir, 0).expect("open");
        assert_eq!(
            next.append(user("second writer")).expect("append").seq,
            EventSeq(2)
        );
    }

    #[test]
    fn releasing_an_old_owner_preserves_the_new_owners_holder_record() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);
        store.append(user("old owner")).expect("append");
        let dir = store.dir().to_path_buf();
        let next_owner = std::rc::Rc::new(std::cell::RefCell::new(None));
        let acquired = std::rc::Rc::clone(&next_owner);
        let next_dir = dir.clone();

        // Pause the old owner's drop after unlock until another thread takes
        // the lock and writes its record. Cleanup after unlock would erase it.
        AFTER_UNLOCK.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move || {
                let next = std::thread::spawn(move || {
                    let next = SessionStore::open_with_deadline(&next_dir, 0)
                        .expect("new owner acquires the released lock");
                    let record = std::fs::read(next_dir.join(HOLDER_FILE)).expect("new record");
                    (next, record)
                })
                .join()
                .expect("new owner thread");
                *acquired.borrow_mut() = Some(next);
            }));
        });

        drop(store);

        let (next, record) = next_owner.borrow_mut().take().expect("new owner");
        let holder: LockHolder = serde_json::from_slice(&record).expect("valid holder record");
        assert_eq!(holder.pid, std::process::id());
        assert_eq!(
            std::fs::read(dir.join(HOLDER_FILE)).expect("preserved record"),
            record,
            "the old owner's drop must preserve the new owner's diagnostics"
        );
        let err = SessionStore::open_with_deadline(&dir, 0).expect_err("new owner holds lock");
        assert_eq!(err.code(), ErrorCode::Locked);
        assert!(err.message().contains(&holder.pid.to_string()));
        assert_eq!(
            next.append(user("new owner")).expect("append").seq,
            EventSeq(2)
        );
        assert_eq!(next.read().expect("read").len(), 2);
    }

    #[test]
    fn the_lock_deadline_is_bounded_and_reported() {
        assert_eq!(LOCK_DEADLINE_MS, 2_000);

        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);

        let started = Instant::now();
        let err = SessionStore::open_with_deadline(store.dir(), 60).expect_err("locked");
        let waited = started.elapsed();
        assert_eq!(err.code(), ErrorCode::Locked);
        assert!(waited >= Duration::from_millis(60), "waited {waited:?}");

        // A zero deadline attempts once and reports immediately.
        let started = Instant::now();
        SessionStore::open_with_deadline(store.dir(), 0).expect_err("locked");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_reader_never_blocks_while_a_writer_appends() {
        let root = tempfile::tempdir().expect("tempdir");
        let (paths, store) = store_in(&root);
        let dir = paths.session_dir(&store.id());
        let expected = usize::try_from(200_u64).expect("count");

        // The reader holds no lock, so it makes progress the whole time the
        // writer runs and never observes the log going backwards.
        let reader = std::thread::spawn(move || {
            let started = Instant::now();
            let mut previous = 0_usize;
            let mut reads = 0_u64;
            while started.elapsed() < Duration::from_secs(30) {
                let state = load_read_only(&dir).expect("reader must not fail");
                assert!(
                    state.len() >= previous,
                    "reader saw {} events after {previous}",
                    state.len()
                );
                previous = state.len();
                reads = reads.saturating_add(1);
                if previous >= expected {
                    break;
                }
            }
            assert!(reads > 0, "reader never ran");
            previous
        });

        for index in 0..expected {
            store
                .append(user(&format!("event {index}")))
                .expect("append");
        }
        let seen = reader.join().expect("join");
        assert_eq!(seen, expected);
        assert_eq!(store.read().expect("read").len(), expected);
    }

    #[test]
    fn readers_of_a_partially_written_frame_see_the_last_durable_boundary() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);
        store.append(user("complete")).expect("append");

        let path = store.events_path();
        let complete = std::fs::read(&path).expect("read");
        let mut torn = complete.clone();
        torn.extend_from_slice(b"{\"schema\":1,\"seq\":2,\"timestamp_m");
        std::fs::write(&path, &torn).expect("write");

        let state = load_read_only(store.dir()).expect("read");
        assert_eq!(state.len(), 1);
        assert!(state.truncated_at.is_some());
        assert_eq!(
            state.truncated_at,
            Some(u64::try_from(complete.len()).expect("offset"))
        );
    }

    #[test]
    fn a_gapped_log_is_refused_by_the_store() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);
        store
            .append(SessionEvent::TurnStarted { turn: 1 })
            .expect("append");

        let path = store.events_path();
        let existing = std::fs::read_to_string(&path).expect("read");
        let gap = EventFrame::new(
            EventSeq(9),
            event::now_millis(),
            SessionEvent::UserMessage {
                text: "out of order".to_owned(),
            },
        )
        .encode()
        .expect("encode");
        std::fs::write(&path, format!("{existing}{gap}\n")).expect("write");

        let err = load_read_only(store.dir()).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
        assert!(err.message().contains("missing event 2"));

        // Opening validates the log too, once the writer that holds the lock
        // has released it.
        let dir = store.dir().to_path_buf();
        drop(store);
        let err = SessionStore::open_with_deadline(&dir, 0).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
        assert!(err.message().contains("missing event 2"));
    }

    #[test]
    fn a_directory_name_that_is_not_a_session_id_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = Utf8PathBuf::from_path_buf(root.path().join("not a session")).expect("utf8");
        std::fs::create_dir_all(&dir).expect("create");

        let err = load_read_only(&dir).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("session_id"));

        let err = SessionStore::open(&dir).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn a_rewrite_replaces_the_log_atomically() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);
        store
            .append(SessionEvent::TurnStarted { turn: 1 })
            .expect("append");

        let frames = vec![
            EventFrame::new(EventSeq(1), 5, user("kept")),
            EventFrame::new(EventSeq(2), 6, user("added")),
        ];
        write_log_atomic(store.dir(), &frames).expect("rewrite");

        let read = load_read_only(store.dir()).expect("read");
        assert_eq!(read.events, frames);
        assert!(!store.dir().join(format!("{EVENTS_FILE}.rewrite")).exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.events_path())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn a_rewrite_past_the_log_cap_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);
        let payload = "x".repeat(
            usize::try_from(event::MAX_LOG_BYTES)
                .expect("cap")
                .saturating_add(1),
        );
        let frames = vec![EventFrame::new(EventSeq(1), 5, user(&payload))];

        let err = write_log_atomic(store.dir(), &frames).expect_err("refused");
        assert!(matches!(
            err.code(),
            ErrorCode::TooLarge | ErrorCode::CorruptRecord
        ));
        assert!(load_read_only(store.dir()).expect("read").is_empty());
    }

    #[test]
    fn a_symlinked_lock_file_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let (paths, store) = store_in(&root);
        let dir = paths.session_dir(&store.id());
        let lock_path = dir.join(LOCK_FILE);
        std::fs::remove_file(&lock_path).expect("remove");
        std::fs::write(dir.join("elsewhere"), "").expect("write");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("elsewhere"), &lock_path).expect("symlink");
            let err = SessionStore::open_with_deadline(&dir, 0).expect_err("refused");
            assert_eq!(err.code(), ErrorCode::UnsafePath);
        }
    }

    #[test]
    fn opening_a_torn_log_truncates_to_the_last_durable_boundary() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);
        store.append(user("durable")).expect("append");
        let path = store.events_path();
        let dir = store.dir().to_path_buf();
        let complete = std::fs::read(&path).expect("read");
        drop(store);

        // A crash mid-append leaves a fragment with no terminator.
        let mut torn = complete.clone();
        torn.extend_from_slice(b"{\"schema\":1,\"seq\":2,\"timestamp_m");
        std::fs::write(&path, &torn).expect("write");

        let reopened = SessionStore::open(&dir).expect("open");

        assert_eq!(reopened.next_seq(), EventSeq(2));
        assert_eq!(std::fs::read(&path).expect("read"), complete);
        assert_eq!(
            reopened.append(user("after the tear")).expect("append").seq,
            EventSeq(2)
        );
        let read = reopened.read().expect("read");
        assert_eq!(read.len(), 2);
        assert_eq!(read.truncated_at, None);
        assert_eq!(read.events[1].event, user("after the tear"));
    }

    #[cfg(unix)]
    #[test]
    fn a_widened_log_file_is_refused_on_open() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);
        store.append(user("x")).expect("append");
        let dir = store.dir().to_path_buf();
        let log_path = store.events_path();
        drop(store);

        std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o644)).expect("widen");
        let err = SessionStore::open(&dir).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
    }

    #[test]
    fn a_reader_of_a_torn_log_is_not_disturbed_by_another_reader() {
        let root = tempfile::tempdir().expect("tempdir");
        let (_paths, store) = store_in(&root);
        store.append(user("only")).expect("append");
        let dir = store.dir().to_path_buf();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let dir = dir.clone();
            handles.push(std::thread::spawn(move || {
                load_read_only(&dir).expect("read").len()
            }));
        }
        for handle in handles {
            assert_eq!(handle.join().expect("join"), 1);
        }
    }
    #[test]
    fn usage_totals_saturate() {
        let mut total = UsageTotal {
            input_tokens: u64::MAX,
            output_tokens: 1,
        };
        total.record(10, 10);
        assert_eq!(total.input_tokens, u64::MAX);
        assert_eq!(total.output_tokens, 11);
        assert_eq!(total.total(), u64::MAX);
    }
}
