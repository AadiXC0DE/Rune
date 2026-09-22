//! The local usage ledger.
//!
//! One JSON object per line in the state root, schema version 1. The ledger is
//! the only record of what a request cost, so three properties are load bearing:
//!
//! - An unreported count is absent and a reported zero stays zero. The
//!   distinction survives a write, a read, and a report. Providers differ on
//!   whether they report a cache read of zero or report nothing, and only one of
//!   those means the request was not cached.
//! - A read never blocks an append. A reader opens the ledger and reads it, and
//!   takes no lock at all, so producing a report never delays recording.
//! - Every bound is checked on each append from one streaming pass over the
//!   file, because a count cached in memory is stale as soon as a second process
//!   appends.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::budget::EMERGENCY_CEILING_BYTES;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::{self, Paths};
use serde::{Deserialize, Serialize};

/// Schema version written by this build.
pub const SCHEMA_VERSION: u32 = 1;

/// Largest accepted record, in encoded bytes, excluding the line terminator.
pub const MAX_RECORD_BYTES: usize = 16 * 1024;

/// Largest accepted number of records.
pub const MAX_RECORDS: u64 = 200_000;

/// Ledger size that triggers compaction.
pub const COMPACTION_BYTES: u64 = 8 * 1024 * 1024;

/// Age past which a record does not belong in the ledger.
pub const RETENTION_MS: i64 = 35 * 24 * 60 * 60 * 1_000;

/// Bytes read from the ledger per system call while scanning.
const SCAN_CHUNK_BYTES: usize = 64 * 1024;

/// Suffix of the lock file guarding the ledger.
const LOCK_SUFFIX: &str = ".lock";

/// Returns milliseconds since the Unix epoch.
///
/// A clock reading before the epoch is reported as zero: the ledger has no room
/// for a negative instant, and a wrong sign would move a record out of every
/// period that should have contained it.
#[must_use]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Which request produced a record.
///
/// Helper requests are counted separately so their cost is visible rather than
/// folded into the answer the user asked for.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperKind {
    /// A request made for the user.
    Main,
    /// A permission review request.
    PermissionReview,
    /// An image analysis request made when the model cannot read images itself.
    Vision,
}

impl HelperKind {
    /// Every kind, in a stable order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Main, Self::PermissionReview, Self::Vision]
    }

    /// Returns the wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::PermissionReview => "permission_review",
            Self::Vision => "vision",
        }
    }
}

impl fmt::Display for HelperKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One recorded request.
///
/// Every count is optional because an unreported count is a fact the ledger must
/// preserve. A provider that reported no cache read never said the request was
/// uncached, so the field stays absent, while a provider that reported zero did.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct UsageRecord {
    /// Milliseconds since the Unix epoch when the request completed.
    pub created_at_ms: i64,
    /// Model that served the request.
    pub model: String,
    /// Input tokens, absent when the provider reported none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens, absent when the provider reported none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Tokens read from a prompt cache, absent when the provider reported none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Tokens written to a prompt cache, absent when the provider reported none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Reasoning tokens, absent when the provider reported none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Cost exactly as the provider reported it, absent when it reported none.
    ///
    /// Kept as the reported string rather than a parsed number so the ledger
    /// never rounds a figure a provider was precise about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost: Option<String>,
    /// Requests this record accounts for.
    pub request_count: u64,
    /// Which request produced this record.
    pub helper: HelperKind,
}

/// One line of the ledger.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct WireRecord {
    /// Schema version, so a future change can be migrated rather than guessed.
    schema_version: u32,
    #[serde(flatten)]
    record: UsageRecord,
}

impl UsageRecord {
    /// Builds a record for one completed request, with nothing reported.
    #[must_use]
    pub fn new(created_at_ms: i64, model: impl Into<String>, helper: HelperKind) -> Self {
        Self {
            created_at_ms,
            model: model.into(),
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            total_cost: None,
            request_count: 1,
            helper,
        }
    }

    /// Rejects a record that would make the ledger unusable.
    pub fn check(&self) -> Result<()> {
        if self.model.trim().is_empty() {
            return Err(RuneError::invalid_field("model", "must not be empty"));
        }
        if self
            .total_cost
            .as_ref()
            .is_some_and(|cost| cost.trim().is_empty())
        {
            return Err(RuneError::invalid_field(
                "total_cost",
                "must not be an empty string when reported; leave it absent instead",
            ));
        }
        Ok(())
    }

    /// Encodes the record as one line, without its terminator.
    ///
    /// The result never contains a newline: serialization escapes control
    /// characters, so a model name holding one cannot split the record across
    /// two lines.
    pub fn encode(&self) -> Result<String> {
        self.encode_with_limit(MAX_RECORD_BYTES)
    }

    /// Encodes the record, rejecting one larger than `limit` bytes.
    ///
    /// The limit is a parameter so the bound can be exercised at its exact
    /// boundary without building a record of the production size.
    pub fn encode_with_limit(&self, limit: usize) -> Result<String> {
        self.check()?;
        let line = serde_json::to_string(&WireRecord {
            schema_version: SCHEMA_VERSION,
            record: self.clone(),
        })?;
        if line.len() > limit {
            return Err(RuneError::too_large("usage_record", line.len(), limit)
                .with_invariant("record_size"));
        }
        Ok(line)
    }

    /// Decodes one line.
    pub fn decode(line: &str) -> Result<Self> {
        let wire: WireRecord = serde_json::from_str(line).map_err(|cause| {
            RuneError::new(
                ErrorCode::CorruptRecord,
                format!("a usage record could not be read: {cause}"),
            )
            .with_invariant("usage_record")
        })?;
        if wire.schema_version != SCHEMA_VERSION {
            return Err(RuneError::new(
                ErrorCode::UnsupportedVersion,
                format!(
                    "a usage record uses schema version {}, this build reads {SCHEMA_VERSION}",
                    wire.schema_version
                ),
            )
            .with_invariant("schema_version")
            .with_observed(format!("schema_version {}", wire.schema_version))
            .with_hint("upgrade Rune to read this ledger, or move the file aside"));
        }
        wire.record.check()?;
        Ok(wire.record)
    }
}

/// Bounds applied to a ledger.
///
/// The defaults are the production values. Every field is overridable so a bound
/// can be exercised at its boundary without writing a ledger of the production
/// size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LedgerCaps {
    /// Largest encoded record.
    pub max_record_bytes: usize,
    /// Largest number of records.
    pub max_records: u64,
    /// Size that triggers compaction.
    pub compaction_bytes: u64,
    /// Age past which a record is dropped when compaction runs.
    pub retention_ms: i64,
}

impl Default for LedgerCaps {
    fn default() -> Self {
        Self {
            max_record_bytes: MAX_RECORD_BYTES,
            max_records: MAX_RECORDS,
            compaction_bytes: COMPACTION_BYTES,
            retention_ms: RETENTION_MS,
        }
    }
}

impl LedgerCaps {
    /// Returns the size compaction trims to.
    ///
    /// Half the trigger size, so compaction is amortized: an append right after
    /// one has room to grow before the next.
    #[must_use]
    pub const fn compaction_target_bytes(&self) -> u64 {
        self.compaction_bytes / 2
    }

    /// Returns true when a ledger in this state must be compacted.
    fn needs_compaction(&self, scan: &Scan, pending_bytes: u64, now_ms: i64) -> bool {
        let records = scan.records.saturating_add(1);
        let bytes = scan.bytes.saturating_add(pending_bytes);
        records > self.max_records
            || bytes > self.compaction_bytes
            || scan
                .first_record_at_ms
                .is_some_and(|at| now_ms.saturating_sub(at) > self.retention_ms)
    }
}

/// What one streaming pass over the ledger observed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Scan {
    /// Complete lines, which is the record count.
    records: u64,
    /// Bytes in the file, including any unterminated fragment.
    bytes: u64,
    /// Bytes up to and including the last terminator.
    ///
    /// Equal to `bytes` unless the file ends inside a line, which is where an
    /// append truncates before writing.
    complete_bytes: u64,
    /// Timestamp of the first record, when the file starts with a readable one.
    ///
    /// Records are appended in time order, so the first record is the oldest and
    /// is what the retention bound is checked against.
    first_record_at_ms: Option<i64>,
}

/// What one append did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AppendOutcome {
    /// Records compaction removed, oldest first.
    pub dropped_records: u64,
    /// Whether an unterminated final line was removed while compacting.
    pub dropped_tail: bool,
    /// Records in the ledger after the append.
    pub records: u64,
}

/// A ledger read.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LedgerRead {
    /// Records in file order, oldest first.
    pub records: Vec<UsageRecord>,
    /// Whether an unterminated final line was dropped.
    ///
    /// True is expected when a writer was appending while the read ran, and
    /// means nothing was lost: the line was not complete when it was read.
    pub tail_incomplete: bool,
}

/// The append-only usage ledger.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Ledger {
    path: Utf8PathBuf,
    caps: LedgerCaps,
}

impl Ledger {
    /// Opens the ledger at `path`.
    #[must_use]
    pub fn new(path: impl Into<Utf8PathBuf>) -> Self {
        Self {
            path: path.into(),
            caps: LedgerCaps::default(),
        }
    }

    /// Opens the ledger in the state root.
    #[must_use]
    pub fn from_paths(paths: &Paths) -> Self {
        Self::new(paths.usage_file())
    }

    /// Opens the ledger at `path` with explicit bounds.
    #[must_use]
    pub fn with_caps(path: impl Into<Utf8PathBuf>, caps: LedgerCaps) -> Self {
        Self {
            path: path.into(),
            caps,
        }
    }

    /// Returns the ledger path.
    #[must_use]
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }

    /// Returns the lock file guarding the ledger.
    ///
    /// A sidecar rather than the ledger itself: compaction replaces the ledger
    /// with a rename, and a lock held on the replaced file would not exclude the
    /// next appender.
    #[must_use]
    pub fn lock_path(&self) -> Utf8PathBuf {
        let name = self
            .path
            .file_name()
            .map_or_else(|| String::from("usage.jsonl"), str::to_owned);
        self.path.with_file_name(format!("{name}{LOCK_SUFFIX}"))
    }

    /// Returns the bounds in force.
    #[must_use]
    pub const fn caps(&self) -> &LedgerCaps {
        &self.caps
    }

    /// Appends one record, compacting first when a bound would be crossed.
    ///
    /// Compaction happens before the record is written, so a failure leaves the
    /// ledger as it was and the returned outcome always describes a ledger that
    /// contains this record.
    pub fn append(&self, record: &UsageRecord) -> Result<AppendOutcome> {
        record.check()?;
        let line = record.encode_with_limit(self.caps.max_record_bytes)?;
        let now = now_ms();

        let guard = LockGuard::acquire(&self.lock_path())?;
        let scan = self.scan()?;
        let pending_bytes = u64::try_from(line.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let outcome = if self.caps.needs_compaction(&scan, pending_bytes, now) {
            self.compact(&line, now)?
        } else {
            self.append_line(&line, &scan)?;
            AppendOutcome {
                dropped_records: 0,
                dropped_tail: scan.bytes > scan.complete_bytes,
                records: scan.records.saturating_add(1),
            }
        };
        drop(guard);
        Ok(outcome)
    }

    /// Reads every record, oldest first.
    ///
    /// A missing ledger is an empty ledger, which is not an error. No lock is
    /// taken, so a read cannot delay an append from this or any other process.
    pub fn read(&self) -> Result<LedgerRead> {
        let text = paths::read_private(&self.path, EMERGENCY_CEILING_BYTES)?.unwrap_or_default();
        Self::parse(&text)
    }

    /// Parses a ledger body.
    fn parse(text: &str) -> Result<LedgerRead> {
        // An absent ledger and a ledger created but not yet written are both
        // empty bodies, which is an empty ledger rather than a damaged line.
        if text.is_empty() {
            return Ok(LedgerRead::default());
        }
        let terminated = text.ends_with('\n');
        let mut lines: Vec<&str> = text.split('\n').collect();
        if terminated {
            lines.pop();
        }
        let total = lines.len();
        let mut read = LedgerRead {
            records: Vec::with_capacity(total),
            tail_incomplete: false,
        };

        for (index, line) in lines.iter().enumerate() {
            let last = index.saturating_add(1) == total;
            match UsageRecord::decode(line) {
                Ok(record) => read.records.push(record),
                // The final line can be a fragment of a write in flight. An
                // unterminated empty line is not a fragment.
                Err(_) if last && !terminated && !line.is_empty() => read.tail_incomplete = true,
                Err(cause) => return Err(damaged_line(index.saturating_add(1), &cause)),
            }
        }
        Ok(read)
    }

    /// Streams the ledger, counting records and reading the first timestamp.
    ///
    /// Counting from the file rather than from memory is what keeps the bound
    /// correct when another process appended since this one last looked.
    fn scan(&self) -> Result<Scan> {
        let mut scan = Scan::default();
        let mut first_line: Vec<u8> = Vec::new();
        let mut capturing = true;
        // Offset of the byte under the cursor, advanced one byte at a time: a
        // chunk boundary must not be mistaken for a line boundary.
        let mut offset: u64 = 0;
        let Some(mut reader) = self.open_reader()? else {
            return Ok(scan);
        };
        let mut buffer = vec![0_u8; SCAN_CHUNK_BYTES];

        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            scan.bytes = scan
                .bytes
                .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            for &byte in &buffer[..read] {
                offset = offset.saturating_add(1);
                if byte == b'\n' {
                    scan.records = scan.records.saturating_add(1);
                    scan.complete_bytes = offset;
                    if capturing {
                        capturing = false;
                        scan.first_record_at_ms = first_timestamp(&first_line);
                        first_line.clear();
                    }
                } else if capturing {
                    // A record is bounded, so a first line longer than one is
                    // damaged and carries no timestamp worth reading.
                    if first_line.len() < MAX_RECORD_BYTES {
                        first_line.push(byte);
                    } else {
                        capturing = false;
                        first_line.clear();
                    }
                }
            }
        }
        Ok(scan)
    }

    /// Opens the ledger for reading, verifying it first. Absent is not an error.
    fn open_reader(&self) -> Result<Option<File>> {
        match std::fs::symlink_metadata(&self.path) {
            Ok(meta) => {
                check_ledger_file(&self.path, &meta)?;
                Ok(Some(File::open(&self.path)?))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Appends one encoded line under an already held lock.
    ///
    /// A fragment left by an interrupted write is truncated first. Appending
    /// after it would make the fragment part of the next record, so the whole
    /// ledger would fail to parse.
    fn append_line(&self, line: &str, scan: &Scan) -> Result<()> {
        let mut writer = self.open_writer()?;
        if writer.metadata()?.len() != scan.complete_bytes {
            writer.set_len(scan.complete_bytes)?;
        }
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        writer.sync_all()?;
        Ok(())
    }

    /// Rewrites the ledger with the records that still belong in it.
    ///
    /// The rewrite goes to a temporary file that replaces the ledger with one
    /// rename, so a reader that opened the old file keeps reading a complete
    /// ledger rather than a half-rewritten one.
    fn compact(&self, pending: &str, now_ms: i64) -> Result<AppendOutcome> {
        let text = paths::read_private(&self.path, EMERGENCY_CEILING_BYTES)?.unwrap_or_default();
        let read = Self::parse(&text)?;
        let parsed = u64::try_from(read.records.len()).unwrap_or(u64::MAX);

        let mut kept: Vec<String> = Vec::with_capacity(read.records.len());
        for record in &read.records {
            if now_ms.saturating_sub(record.created_at_ms) > self.caps.retention_ms {
                continue;
            }
            // A record that cannot be re-encoded is dropped rather than left in
            // place: it was written under bounds this build no longer accepts.
            if let Ok(line) = record.encode_with_limit(self.caps.max_record_bytes) {
                kept.push(line);
            }
        }

        // The pending record is part of the result, so the record cap applies to
        // the records that precede it.
        let existing_cap = usize::try_from(self.caps.max_records.saturating_sub(1)).unwrap_or(0);
        if kept.len() > existing_cap {
            let excess = kept.len().saturating_sub(existing_cap);
            kept.drain(..excess);
        }

        let line_bytes = |line: &String| {
            u64::try_from(line.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1)
        };
        let pending_bytes = u64::try_from(pending.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let target = self.caps.compaction_target_bytes();
        let mut total = kept.iter().fold(pending_bytes, |sum, line| {
            sum.saturating_add(line_bytes(line))
        });
        let mut first = 0_usize;
        // One record always survives: an append that leaves nothing behind has
        // destroyed the record it was called to add.
        while first.saturating_add(1) < kept.len() && total > target {
            total = total.saturating_sub(line_bytes(&kept[first]));
            first = first.saturating_add(1);
        }
        let kept = &kept[first..];

        let temp = self.path.with_extension("jsonl.rewrite");
        let mut body = String::with_capacity(usize::try_from(total.saturating_add(1)).unwrap_or(0));
        for line in kept {
            body.push_str(line);
            body.push('\n');
        }
        body.push_str(pending);
        body.push('\n');
        write_rewrite(&temp, &body)?;
        std::fs::rename(&temp, &self.path)?;

        let written = u64::try_from(kept.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        Ok(AppendOutcome {
            dropped_records: parsed.saturating_add(1).saturating_sub(written),
            dropped_tail: read.tail_incomplete,
            records: written,
        })
    }

    /// Opens the ledger for appending, creating it with private permissions.
    fn open_writer(&self) -> Result<File> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::symlink_metadata(&self.path) {
            Ok(meta) => check_ledger_file(&self.path, &meta)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }

        let mut options = OpenOptions::new();
        options.create(true).append(true);
        set_creation_mode(&mut options);
        Ok(options.open(&self.path)?)
    }
}

/// Holds the exclusive append lock for one operation.
struct LockGuard {
    file: File,
}

impl LockGuard {
    /// Takes the lock, waiting for another appender to finish.
    ///
    /// Only appenders wait here, and only for the length of one write. A read
    /// takes no lock, so a report never delays recording.
    fn acquire(path: &Utf8Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|parent| !parent.as_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            check_ledger_file(path, &meta)?;
        }
        let mut options = OpenOptions::new();
        // The lock file is opened for writing rather than appending. Taking a
        // lock on Windows needs a handle that was opened with read or write
        // access, and a handle opened only to append does not carry it, so the
        // lock is refused there. Nothing is written through this handle.
        options.create(true).write(true);
        set_creation_mode(&mut options);
        let file = options.open(path)?;
        file.lock().map_err(|err| {
            RuneError::new(
                ErrorCode::Locked,
                format!("the usage ledger could not be locked: {err}"),
            )
            .with_hint("check that the state directory is writable")
        })?;
        Ok(Self { file })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Closing the handle releases the lock too; unlocking here shortens the
        // window for the next appender rather than waiting for the drop.
        let _ = self.file.unlock();
    }
}

/// Writes the rewritten ledger, fsynced, before it replaces the ledger.
fn write_rewrite(path: &Utf8Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    set_creation_mode(&mut options);
    let mut file = options.open(path)?;
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// Applies the private creation mode to an options builder.
#[cfg(unix)]
fn set_creation_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(paths::FILE_MODE);
}

/// Applies the private creation mode to an options builder.
#[cfg(not(unix))]
fn set_creation_mode(_options: &mut OpenOptions) {}

/// Verifies that a path is a private regular file with a single link.
fn check_ledger_file(path: &Utf8Path, meta: &std::fs::Metadata) -> Result<()> {
    if meta.file_type().is_symlink() {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` is a symbolic link"),
        )
        .with_hint("the usage ledger must be a real file"));
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
        if mode & !paths::FILE_MODE != 0 {
            return Err(RuneError::new(
                ErrorCode::UnsafePath,
                format!(
                    "`{path}` has mode {mode:o}, expected {:o} or narrower",
                    paths::FILE_MODE
                ),
            )
            .with_hint(format!("run `chmod {:o} {path}`", paths::FILE_MODE)));
        }
        if meta.nlink() != 1 {
            return Err(RuneError::new(
                ErrorCode::UnsafePath,
                format!("`{path}` has {} hard links", meta.nlink()),
            )
            .with_hint("the ledger must not be shared between files"));
        }
    }
    Ok(())
}

/// Reads the timestamp of the first record in a ledger.
fn first_timestamp(line: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(line).ok()?;
    UsageRecord::decode(text).ok().map(|r| r.created_at_ms)
}

/// Builds the error reported for a damaged line.
fn damaged_line(number: usize, cause: &RuneError) -> RuneError {
    RuneError::new(
        ErrorCode::CorruptRecord,
        format!(
            "usage ledger line {number} could not be read: {}",
            cause.message()
        ),
    )
    .with_invariant("usage_record")
    .with_observed(format!("line {number}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deadline for a test that waits on another thread.
    const THREAD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

    /// Deadline for a test that waits on a lock, kept short so a regression
    /// fails quickly rather than stalling the suite.
    const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

    fn path_in(dir: &tempfile::TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(dir.path().join("usage.jsonl")).expect("utf-8 path")
    }

    fn ledger(dir: &tempfile::TempDir) -> Ledger {
        Ledger::new(path_in(dir))
    }

    /// An instant inside the retention window, so fixtures do not age out.
    ///
    /// Fixed once, so every fixture in every test is measured against the same
    /// instant and an expectation cannot drift by a millisecond while a test
    /// runs.
    fn base() -> i64 {
        static BASE: std::sync::LazyLock<i64> =
            std::sync::LazyLock::new(|| now_ms().saturating_sub(1_000));
        *BASE
    }

    fn at(offset_ms: i64) -> UsageRecord {
        UsageRecord::new(
            base().saturating_add(offset_ms),
            "test-model",
            HelperKind::Main,
        )
    }

    fn timestamps(read: &LedgerRead) -> Vec<i64> {
        read.records.iter().map(|r| r.created_at_ms).collect()
    }

    #[test]
    fn an_unreported_count_is_omitted_and_reads_back_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        let mut original = at(0);
        original.output_tokens = Some(12);

        let line = original.encode().expect("encode");
        assert!(!line.contains("input_tokens"), "{line}");
        assert!(line.contains("\"output_tokens\":12"), "{line}");

        ledger.append(&original).expect("append");
        let read = ledger.read().expect("read");
        assert_eq!(read.records.len(), 1);
        assert_eq!(read.records[0].input_tokens, None);
        assert_eq!(read.records[0].output_tokens, Some(12));
        assert_eq!(read.records[0], original);
    }

    #[test]
    fn a_reported_zero_stays_present_as_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        let mut original = at(0);
        original.input_tokens = Some(0);
        original.cache_read_tokens = Some(0);

        let line = original.encode().expect("encode");
        assert!(line.contains("\"input_tokens\":0"), "{line}");

        ledger.append(&original).expect("append");
        let read = ledger.read().expect("read");
        assert_eq!(read.records[0].input_tokens, Some(0));
        assert_eq!(read.records[0].cache_read_tokens, Some(0));
        assert_eq!(read.records[0].output_tokens, None);
    }

    #[test]
    fn zero_and_absence_remain_distinct_side_by_side() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        let mut zero = at(1);
        zero.input_tokens = Some(0);

        ledger.append(&at(0)).expect("append absent");
        ledger.append(&zero).expect("append zero");
        let read = ledger.read().expect("read");
        assert_eq!(read.records[0].input_tokens, None);
        assert_eq!(read.records[1].input_tokens, Some(0));
    }

    #[test]
    fn a_record_round_trips_through_the_wire_format() {
        let mut original = UsageRecord::new(base(), "claude-sonnet-4", HelperKind::Vision);
        original.input_tokens = Some(1200);
        original.output_tokens = Some(0);
        original.cache_read_tokens = Some(800);
        original.total_cost = Some("0.0123".to_owned());
        original.request_count = 3;

        let line = original.encode().expect("encode");
        let decoded = UsageRecord::decode(&line).expect("decode");
        assert_eq!(decoded, original);
        assert!(line.contains("\"schema_version\":1"), "{line}");
        assert!(line.contains("\"helper\":\"vision\""), "{line}");
    }

    #[test]
    fn an_unknown_schema_version_is_refused() {
        let line = r#"{"schema_version":9,"created_at_ms":1,"model":"m","request_count":1,"helper":"main"}"#;
        let err = UsageRecord::decode(line).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsupportedVersion);
        assert_eq!(err.detail().invariant.as_deref(), Some("schema_version"));
    }

    #[test]
    fn an_oversized_record_is_rejected_before_it_is_appended() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        let record = at(0);
        let err = record.encode_with_limit(16).expect_err("too large");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.detail().invariant.as_deref(), Some("record_size"));

        let exact = record.encode().expect("encode").len();
        assert!(record.encode_with_limit(exact).is_ok());
        assert!(record.encode_with_limit(exact.saturating_sub(1)).is_err());
        assert!(ledger.read().expect("read").records.is_empty());
    }

    #[test]
    fn a_record_without_a_model_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        let mut blank = at(0);
        blank.model = "  ".to_owned();
        let err = ledger.append(&blank).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("model"));

        let mut free = at(0);
        free.total_cost = Some(" ".to_owned());
        let err = ledger.append(&free).expect_err("refused");
        assert_eq!(err.field(), Some("total_cost"));
    }

    #[test]
    fn a_missing_ledger_reads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let read = ledger(&dir).read().expect("read");
        assert!(read.records.is_empty());
        assert!(!read.tail_incomplete);
    }

    #[test]
    fn exceeding_the_record_cap_compacts_and_reports_the_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base();
        let caps = LedgerCaps {
            max_records: 3,
            ..LedgerCaps::default()
        };
        let ledger = Ledger::with_caps(path_in(&dir), caps);

        let mut dropped = Vec::new();
        for offset in 1..=6 {
            dropped.push(ledger.append(&at(offset)).expect("append").dropped_records);
        }
        assert_eq!(dropped, vec![0, 0, 0, 1, 1, 1]);

        let read = ledger.read().expect("read");
        assert_eq!(
            timestamps(&read),
            vec![
                base.saturating_add(4),
                base.saturating_add(5),
                base.saturating_add(6)
            ]
        );
    }

    #[test]
    fn exceeding_the_size_bound_compacts_and_keeps_the_newest_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base();
        let caps = LedgerCaps {
            compaction_bytes: 900,
            ..LedgerCaps::default()
        };
        let ledger = Ledger::with_caps(path_in(&dir), caps);

        let mut dropped = 0;
        for offset in 1..=12 {
            dropped += ledger.append(&at(offset)).expect("append").dropped_records;
        }
        assert!(dropped > 0, "the size bound never compacted");

        let read = ledger.read().expect("read");
        assert_eq!(
            u64::try_from(read.records.len()).expect("len") + dropped,
            12
        );
        assert_eq!(
            read.records.last().map(|r| r.created_at_ms),
            Some(base.saturating_add(12)),
            "compaction dropped the newest record"
        );
        let file_bytes = std::fs::metadata(ledger.path()).expect("metadata").len();
        assert!(file_bytes < 900, "the ledger stayed over its bound");
    }

    #[test]
    fn a_ledger_holding_expired_records_is_compacted_on_the_next_append() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base();
        let caps = LedgerCaps {
            retention_ms: 60_000,
            ..LedgerCaps::default()
        };
        let ledger = Ledger::with_caps(path_in(&dir), caps);
        let expired = base.saturating_sub(600_000);

        // An expired record is still recorded: the call records what happened,
        // and the next compaction is what removes it.
        let first = ledger
            .append(&UsageRecord::new(expired, "test-model", HelperKind::Main))
            .expect("append");
        assert_eq!(first.dropped_records, 0);
        assert_eq!(timestamps(&ledger.read().expect("read")), vec![expired]);

        let second = ledger.append(&at(2)).expect("append");
        assert_eq!(second.dropped_records, 1);
        assert_eq!(
            timestamps(&ledger.read().expect("read")),
            vec![base.saturating_add(2)]
        );
    }

    #[test]
    fn a_torn_final_line_is_dropped_and_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base();
        let ledger = ledger(&dir);
        ledger.append(&at(1)).expect("append");

        let mut file = OpenOptions::new()
            .append(true)
            .open(ledger.path())
            .expect("open");
        file.write_all(b"{\"schema_version\":1,\"created_at")
            .expect("write fragment");
        file.sync_all().expect("sync");

        let read = ledger.read().expect("read");
        assert_eq!(timestamps(&read), vec![base.saturating_add(1)]);
        assert!(read.tail_incomplete);

        ledger.append(&at(2)).expect("append");
        let read = ledger.read().expect("read");
        assert_eq!(
            timestamps(&read),
            vec![base.saturating_add(1), base.saturating_add(2)]
        );
        assert!(!read.tail_incomplete);
    }

    #[test]
    fn a_torn_final_line_is_removed_by_compaction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base();
        let caps = LedgerCaps {
            max_records: 2,
            ..LedgerCaps::default()
        };
        let ledger = Ledger::with_caps(path_in(&dir), caps);
        ledger.append(&at(1)).expect("append");
        ledger.append(&at(2)).expect("append");

        let mut file = OpenOptions::new()
            .append(true)
            .open(ledger.path())
            .expect("open");
        file.write_all(b"{\"schema_version\":1").expect("fragment");
        file.sync_all().expect("sync");

        // The ledger already holds the cap, so this append compacts and the
        // fragment is dropped rather than carried forward.
        let outcome = ledger.append(&at(3)).expect("append");
        assert!(outcome.dropped_tail);
        assert_eq!(
            timestamps(&ledger.read().expect("read")),
            vec![base.saturating_add(2), base.saturating_add(3)]
        );
    }

    #[test]
    fn a_damaged_line_before_the_end_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        ledger.append(&at(1)).expect("append");

        let mut file = OpenOptions::new()
            .append(true)
            .open(ledger.path())
            .expect("open");
        file.write_all(b"not a record\n").expect("write junk");
        file.write_all(at(2).encode().expect("encode").as_bytes())
            .expect("write record");
        file.write_all(b"\n").expect("newline");

        let err = ledger.read().expect_err("refused");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
        assert_eq!(err.field(), None);
        assert!(err.message().contains("line 2"), "{err}");
    }

    #[test]
    fn a_widened_ledger_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        ledger.append(&at(1)).expect("append");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(ledger.path(), std::fs::Permissions::from_mode(0o644))
                .expect("widen");

            let err = ledger.read().expect_err("read refused");
            assert_eq!(err.code(), ErrorCode::UnsafePath);
            let err = ledger.append(&at(2)).expect_err("append refused");
            assert_eq!(err.code(), ErrorCode::UnsafePath);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_ledger_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("elsewhere.jsonl");
        std::fs::write(&target, b"").expect("target");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("usage.jsonl")).expect("utf-8");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");

        let ledger = Ledger::new(path);
        let err = ledger.append(&at(1)).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
        assert_eq!(
            std::fs::metadata(&target).expect("metadata").len(),
            0,
            "the link target was written through"
        );
    }

    #[test]
    fn appending_creates_a_private_ledger_and_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        ledger.append(&at(1)).expect("append");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            for path in [ledger.path(), ledger.lock_path().as_path()] {
                let mode = std::fs::metadata(path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, paths::FILE_MODE, "{path}");
            }
        }
    }

    #[test]
    fn a_read_completes_while_the_append_lock_is_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        ledger.append(&at(1)).expect("append");

        // The lock is held for the whole read. A read that waited on it would
        // not produce a count before the deadline, which is the failure reported
        // below. The lock is released before the assertion either way, so a read
        // that waits cannot deadlock the suite.
        let holder = OpenOptions::new()
            .create(true)
            .append(true)
            .open(ledger.lock_path())
            .expect("open lock");
        holder.lock().expect("lock");

        let (tx, rx) = std::sync::mpsc::channel::<Result<usize, String>>();
        let outcome = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let count = ledger.read().map(|read| read.records.len());
                let _ = tx.send(count.map_err(|err| err.to_string()));
            });
            let result = rx.recv_timeout(LOCK_WAIT);
            holder.unlock().expect("unlock");
            reader.join().expect("reader thread");
            result
        });

        let read = outcome.expect("the read waited on the append lock");
        assert_eq!(read.expect("read"), 1);
    }

    #[test]
    fn a_concurrent_reader_sees_every_record_and_never_blocks_the_writer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        let total = 60_i64;

        let outcome = std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                for offset in 1..=total {
                    ledger.append(&at(offset)).expect("append");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });

            let mut highest = 0_usize;
            let mut reads = 0_u64;
            let deadline = std::time::Instant::now() + THREAD_DEADLINE;
            while !writer.is_finished() {
                if std::time::Instant::now() > deadline {
                    return Err(format!("the reader stalled after {reads} reads"));
                }
                let read = ledger.read().map_err(|err| err.to_string())?;
                // Only a torn tail can shorten a read, and that fragment was
                // never a record.
                assert!(
                    read.records.len() >= highest || read.tail_incomplete,
                    "a read lost records: {highest} then {}",
                    read.records.len()
                );
                highest = highest.max(read.records.len());
                reads = reads.saturating_add(1);
            }
            writer.join().expect("writer thread");

            let read = ledger.read().map_err(|err| err.to_string())?;
            Ok((reads, timestamps(&read)))
        });

        let (reads, seen) = outcome.expect("the concurrent read must not fail");
        assert!(reads > 0, "the reader never ran while the writer worked");
        let expected: Vec<i64> = (1..=total)
            .map(|offset| base().saturating_add(offset))
            .collect();
        assert_eq!(seen, expected, "a concurrent read and append lost a record");
    }

    #[test]
    fn two_writers_appending_concurrently_lose_no_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = ledger(&dir);
        let per_writer = 30_i64;

        std::thread::scope(|scope| {
            for slice in [0_i64, 100] {
                let ledger = &ledger;
                scope.spawn(move || {
                    for offset in 1..=per_writer {
                        ledger
                            .append(&at(slice.saturating_add(offset)))
                            .expect("append");
                    }
                });
            }
        });

        let read = ledger.read().expect("read");
        let expected = usize::try_from(per_writer.saturating_mul(2)).expect("total");
        assert_eq!(read.records.len(), expected);

        let mut seen = timestamps(&read);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), expected, "an append overwrote another");
        assert_eq!(
            read.records.iter().map(|r| r.request_count).sum::<u64>(),
            u64::try_from(expected).expect("total")
        );
    }

    #[test]
    fn the_first_timestamp_is_read_from_the_first_line_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = base();
        let ledger = ledger(&dir);
        ledger.append(&at(7)).expect("append");
        ledger.append(&at(9)).expect("append");

        let scan = ledger.scan().expect("scan");
        assert_eq!(scan.records, 2);
        assert_eq!(scan.first_record_at_ms, Some(base.saturating_add(7)));
        assert!(scan.bytes > 0);
    }
}
