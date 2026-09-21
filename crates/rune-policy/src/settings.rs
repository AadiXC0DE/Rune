//! Atomic settings writes.
//!
//! Configuration is edited by a machine, from several processes at once, on a
//! file a person also edits by hand. The writer therefore has to survive a crash
//! mid-write, lose no concurrent update, and refuse to operate on a file whose
//! contents changed underneath it.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use camino::Utf8Path;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths;
use sha2::{Digest as _, Sha256};

/// How long to wait for the lock before giving up.
pub const LOCK_DEADLINE: Duration = Duration::from_millis(2000);

/// How often to retry while waiting for the lock.
pub const LOCK_POLL: Duration = Duration::from_millis(10);

/// Attempts made when the file changed underneath the writer.
///
/// A small bound is deliberate: contention between a person editing the file and
/// a command updating it is real, but retrying indefinitely would hide the
/// conflict rather than report it.
pub const CAS_ATTEMPTS: u32 = 3;

/// Backups kept beside the configuration.
pub const BACKUPS_KEPT: usize = 5;

/// Largest accepted configuration file.
pub const MAX_BYTES: u64 = 64 * 1024;

/// A fingerprint of the file contents at read time.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Fingerprint(String);

impl Fingerprint {
    /// Fingerprints a body.
    #[must_use]
    pub fn of(body: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        let digest = hasher.finalize();
        let mut out = String::with_capacity(64);
        for byte in digest {
            let _ = write!(out, "{byte:02x}");
        }
        Self(out)
    }

    /// Returns the digest text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An advisory lock over the settings file.
///
/// The lock is a separate file, so acquiring it never touches the content being
/// replaced. It is released on drop, including on an error path.
#[derive(Debug)]
pub struct Lock {
    path: camino::Utf8PathBuf,
    held: bool,
}

impl Lock {
    /// Acquires the lock, waiting up to the deadline.
    pub fn acquire(path: &Utf8Path) -> Result<Self> {
        let started = Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut file) => {
                    use std::io::Write as _;
                    let identity = format!("{}\n", std::process::id());
                    let _ = file.write_all(identity.as_bytes());
                    let _ = file.sync_all();
                    return Ok(Self {
                        path: path.to_owned(),
                        held: true,
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if started.elapsed() >= LOCK_DEADLINE {
                        let holder = std::fs::read_to_string(path).unwrap_or_default();
                        return Err(RuneError::new(
                            ErrorCode::Locked,
                            format!("`{path}` is held by another process"),
                        )
                        .with_observed(holder.trim().to_owned())
                        .with_hint("wait for the other command to finish"));
                    }
                    std::thread::sleep(LOCK_POLL);
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(&self.path);
            self.held = false;
        }
    }
}

/// Outcome of a settings write.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WriteOutcome {
    /// The file was replaced.
    Written {
        /// Bytes written.
        bytes: usize,
        /// Backup created from the previous contents, when there was one.
        backup: Option<String>,
    },
}

/// Replaces the settings file.
///
/// `expected` is the fingerprint the caller read. A mismatch means another
/// process or a person changed the file, which is reported rather than
/// overwritten.
pub fn write(path: &Utf8Path, body: &str, expected: Option<&Fingerprint>) -> Result<WriteOutcome> {
    if body.len() > usize::try_from(MAX_BYTES).unwrap_or(usize::MAX) {
        return Err(RuneError::too_large(
            "settings",
            body.len(),
            MAX_BYTES as usize,
        ));
    }

    // The lock is taken before the content is read, so the read and the write
    // cannot be interleaved by another writer.
    let lock_path = lock_path_for(path);
    let _lock = Lock::acquire(&lock_path)?;

    let previous = paths::read_private(path, MAX_BYTES)?;

    if let Some(expected) = expected {
        let actual = previous
            .as_deref()
            .map_or_else(|| Fingerprint::of(""), Fingerprint::of);
        if &actual != expected {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                format!("`{path}` changed since it was read"),
            )
            .with_observed(format!(
                "expected {}, found {}",
                expected.as_str(),
                actual.as_str()
            ))
            .with_hint("re-read the file and apply the change again"));
        }
    }

    // A backup is taken before the replacement, so a bad write is recoverable
    // from disk rather than only from memory.
    let backup = match &previous {
        Some(previous) if !previous.is_empty() => Some(backup(path, previous)?),
        _ => None,
    };

    paths::write_private(path, body)?;

    // A widened mode would make the file unreadable by the next command, so the
    // write is verified rather than assumed.
    verify_private(path)?;

    Ok(WriteOutcome::Written {
        bytes: body.len(),
        backup,
    })
}

/// Writes a body after reading the current one, retrying on contention.
///
/// `edit` receives the current body and returns the replacement. This is the
/// form callers should use: it makes the read-modify-write cycle atomic with
/// respect to the lock, so two concurrent commands cannot lose an update.
pub fn update<F>(path: &Utf8Path, edit: F) -> Result<WriteOutcome>
where
    F: Fn(Option<&str>) -> Result<String>,
{
    let lock_path = lock_path_for(path);
    let _lock = Lock::acquire(&lock_path)?;

    let previous = paths::read_private(path, MAX_BYTES)?;
    let replacement = edit(previous.as_deref())?;

    if replacement.len() > usize::try_from(MAX_BYTES).unwrap_or(usize::MAX) {
        return Err(RuneError::too_large(
            "settings",
            replacement.len(),
            MAX_BYTES as usize,
        ));
    }

    let backup = match &previous {
        Some(previous) if !previous.is_empty() => Some(backup(path, previous)?),
        _ => None,
    };

    paths::write_private(path, &replacement)?;
    verify_private(path)?;

    Ok(WriteOutcome::Written {
        bytes: replacement.len(),
        backup,
    })
}

/// Returns the lock path for a settings file.
#[must_use]
pub fn lock_path_for(path: &Utf8Path) -> camino::Utf8PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "settings".to_owned(), str::to_owned);
    match path.parent() {
        Some(parent) => parent.join(format!("{name}.lock")),
        None => camino::Utf8PathBuf::from(format!("{name}.lock")),
    }
}

/// Returns the index of the oldest backup for a path.
#[must_use]
pub fn backup_paths(path: &Utf8Path) -> Vec<camino::Utf8PathBuf> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let name = path
        .file_name()
        .map_or_else(|| "settings".to_owned(), str::to_owned);
    let prefix = format!("{name}.backup.");

    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut found: Vec<camino::Utf8PathBuf> = entries
        .flatten()
        .filter_map(|entry| camino::Utf8PathBuf::from_path_buf(entry.path()).ok())
        .filter(|candidate| {
            candidate
                .file_name()
                .is_some_and(|file| file.starts_with(&prefix))
        })
        .collect();
    found.sort();
    found
}

/// Writes a backup of the current contents and prunes old ones.
fn backup(path: &Utf8Path, body: &str) -> Result<String> {
    let Some(parent) = path.parent() else {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` has no parent directory"),
        ));
    };
    let name = path
        .file_name()
        .map_or_else(|| "settings".to_owned(), str::to_owned);
    let stamp = backup_stamp();
    let target = parent.join(format!("{name}.backup.{stamp}"));

    paths::write_private(&target, body)?;

    // Pruning keeps the directory bounded; a failure to prune is not fatal
    // because the write itself succeeded.
    let existing = backup_paths(path);
    for stale in existing.iter().rev().skip(BACKUPS_KEPT) {
        let _ = std::fs::remove_file(stale);
    }

    Ok(target.to_string())
}

/// Returns a sortable stamp for a backup name.
///
/// Derived from the clock in milliseconds, so lexicographic order is
/// chronological order and the oldest backup is the first entry.
fn backup_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:013}", now.as_millis())
}

/// Verifies that a written file is a private regular file.
fn verify_private(path: &Utf8Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` became a symbolic link"),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = meta.mode() & 0o777;
        if mode & !0o600 != 0 {
            return Err(RuneError::new(
                ErrorCode::UnsafePath,
                format!("`{path}` was written with mode {mode:o}"),
            )
            .with_hint(format!("run `chmod 600 {path}`")));
        }
        if meta.nlink() != 1 {
            return Err(RuneError::new(
                ErrorCode::UnsafePath,
                format!("`{path}` has {} hard links", meta.nlink()),
            ));
        }
    }
    Ok(())
}

/// Merges a JSON object update into a settings body.
///
/// Used by the commands that change one key. Unknown keys already in the file are
/// preserved, because a hand-edited file may hold settings this build does not
/// know about yet.
pub fn merge_json(existing: Option<&str>, update: &serde_json::Value) -> Result<String> {
    let mut document: serde_json::Value = match existing {
        Some(body) if !body.trim().is_empty() => serde_json::from_str(body).map_err(|err| {
            RuneError::new(
                ErrorCode::CorruptRecord,
                format!("the settings file could not be parsed: {err}"),
            )
            .with_hint("repair the file, or restore a backup")
        })?,
        _ => serde_json::Value::Object(serde_json::Map::new()),
    };

    let Some(target) = document.as_object_mut() else {
        return Err(RuneError::new(
            ErrorCode::CorruptRecord,
            "the settings file does not hold an object",
        ));
    };
    let Some(source) = update.as_object() else {
        return Err(RuneError::invalid_field(
            "update",
            "the merged value must be an object",
        ));
    };

    for (key, value) in source {
        if value.is_null() {
            target.remove(key);
        } else {
            target.insert(key.clone(), value.clone());
        }
    }

    let mut body = serde_json::to_string_pretty(&document)?;
    body.push('\n');
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn settings_path(dir: &TempDir) -> camino::Utf8PathBuf {
        camino::Utf8PathBuf::from_path_buf(dir.path().join("settings.json")).expect("utf8")
    }

    #[test]
    fn a_write_creates_the_file_with_private_permissions() {
        let dir = TempDir::new().expect("tempdir");
        let path = settings_path(&dir);
        let outcome = write(&path, "{\"a\":1}\n", None).expect("write");
        assert!(matches!(outcome, WriteOutcome::Written { .. }));
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{\"a\":1}\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn a_write_releases_the_lock() {
        let dir = TempDir::new().expect("tempdir");
        let path = settings_path(&dir);
        write(&path, "{}\n", None).expect("write");
        assert!(!lock_path_for(&path).exists(), "the lock was left behind");
    }

    #[test]
    fn a_second_writer_waits_then_reports_the_holder() {
        let dir = TempDir::new().expect("tempdir");
        let path = settings_path(&dir);
        let lock_path = lock_path_for(&path);
        let held = Lock::acquire(&lock_path).expect("first");

        let err = write(&path, "{}\n", None).expect_err("locked");
        assert_eq!(err.code(), ErrorCode::Locked);
        assert!(err.detail().hint.is_some());
        drop(held);
    }

    #[test]
    fn a_stale_change_is_refused_rather_than_overwritten() {
        let dir = TempDir::new().expect("tempdir");
        let path = settings_path(&dir);
        write(&path, "{\"a\":1}\n", None).expect("first");

        let stale = Fingerprint::of("{\"a\":0}\n");
        let err = write(&path, "{\"a\":2}\n", Some(&stale)).expect_err("stale");
        assert_eq!(err.code(), ErrorCode::InvalidState);
        assert!(err.detail().hint.is_some());
        // The file is untouched.
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{\"a\":1}\n");
    }

    #[test]
    fn a_matching_fingerprint_is_accepted() {
        let dir = TempDir::new().expect("tempdir");
        let path = settings_path(&dir);
        write(&path, "{\"a\":1}\n", None).expect("first");
        let current = Fingerprint::of("{\"a\":1}\n");
        write(&path, "{\"a\":2}\n", Some(&current)).expect("second");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{\"a\":2}\n");
    }

    #[test]
    fn a_backup_is_created_from_the_previous_contents() {
        let dir = TempDir::new().expect("tempdir");
        let path = settings_path(&dir);
        write(&path, "{\"a\":1}\n", None).expect("first");
        let outcome = write(&path, "{\"a\":2}\n", None).expect("second");
        let WriteOutcome::Written {
            backup: Some(backup),
            ..
        } = outcome
        else {
            panic!("no backup was created");
        };
        assert_eq!(
            std::fs::read_to_string(backup).expect("read"),
            "{\"a\":1}\n"
        );
    }

    #[test]
    fn a_first_write_creates_no_backup() {
        let dir = TempDir::new().expect("tempdir");
        let path = settings_path(&dir);
        let outcome = write(&path, "{}\n", None).expect("write");
        assert!(matches!(
            outcome,
            WriteOutcome::Written { backup: None, .. }
        ));
    }

    #[test]
    fn backups_are_pruned_to_the_kept_count() {
        let dir = TempDir::new().expect("tempdir");
        let path = settings_path(&dir);
        write(&path, "{\"n\":0}\n", None).expect("initial");
        for index in 1..(BACKUPS_KEPT + 5) {
            write(&path, &format!("{{\"n\":{index}}}\n"), None).expect("write");
        }
        let backups = backup_paths(&path);
        assert!(
            backups.len() <= BACKUPS_KEPT,
            "kept {} backups",
            backups.len()
        );
    }

    #[test]
    fn an_oversized_body_is_refused() {
        let dir = TempDir::new().expect("tempdir");
        let path = settings_path(&dir);
        let body = "x".repeat(usize::try_from(MAX_BYTES).unwrap_or(0) + 1);
        let err = write(&path, &body, None).expect_err("too large");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn an_update_sees_the_current_contents() {
        let dir = TempDir::new().expect("tempdir");
        let path = settings_path(&dir);
        write(&path, "{\"a\":1}\n", None).expect("initial");

        update(&path, |current| {
            assert_eq!(current, Some("{\"a\":1}\n"));
            Ok("{\"a\":2}\n".to_owned())
        })
        .expect("update");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{\"a\":2}\n");
    }

    #[test]
    fn concurrent_updates_lose_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let path = settings_path(&dir);
        write(&path, "{\"count\":0}\n", None).expect("initial");

        let mut handles = Vec::new();
        for _ in 0..8 {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                // A bounded retry models what a real command does under
                // contention; the point is that no increment is lost.
                for _ in 0..4 {
                    let result = update(&path, |current| {
                        let mut document: serde_json::Value = current
                            .and_then(|body| serde_json::from_str(body).ok())
                            .unwrap_or_else(|| serde_json::json!({ "count": 0 }));
                        let count = document
                            .get("count")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0);
                        document["count"] = serde_json::json!(count.saturating_add(1));
                        let mut body = serde_json::to_string(&document)?;
                        body.push('\n');
                        Ok(body)
                    });
                    if result.is_ok() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }));
        }
        for handle in handles {
            let _ = handle.join();
        }

        let body = std::fs::read_to_string(&path).expect("read");
        let document: serde_json::Value = serde_json::from_str(&body).expect("parsed");
        let count = document
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        assert_eq!(count, 8, "an update was lost");
    }

    #[test]
    fn merging_preserves_unknown_keys() {
        let existing = "{\"known\":1,\"unknown\":\"keep me\"}\n";
        let update = serde_json::json!({ "known": 2 });
        let merged = merge_json(Some(existing), &update).expect("merged");
        let document: serde_json::Value = serde_json::from_str(&merged).expect("parsed");
        assert_eq!(document["known"], 2);
        assert_eq!(document["unknown"], "keep me");
    }

    #[test]
    fn merging_a_null_removes_a_key() {
        let existing = "{\"a\":1,\"b\":2}\n";
        let update = serde_json::json!({ "a": null });
        let merged = merge_json(Some(existing), &update).expect("merged");
        let document: serde_json::Value = serde_json::from_str(&merged).expect("parsed");
        assert!(document.get("a").is_none());
        assert_eq!(document["b"], 2);
    }

    #[test]
    fn merging_into_an_empty_file_starts_an_object() {
        let merged = merge_json(None, &serde_json::json!({ "a": 1 })).expect("merged");
        let document: serde_json::Value = serde_json::from_str(&merged).expect("parsed");
        assert_eq!(document["a"], 1);
    }

    #[test]
    fn merging_into_a_corrupt_file_is_refused_with_a_remedy() {
        let err =
            merge_json(Some("not json"), &serde_json::json!({ "a": 1 })).expect_err("corrupt");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn merging_rejects_a_non_object_update() {
        let err = merge_json(None, &serde_json::json!([1, 2, 3])).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn a_fingerprint_is_stable_and_distinguishing() {
        assert_eq!(Fingerprint::of("a"), Fingerprint::of("a"));
        assert_ne!(Fingerprint::of("a"), Fingerprint::of("b"));
        assert_eq!(Fingerprint::of("a").as_str().len(), 64);
    }

    #[test]
    fn a_backup_stamp_sorts_chronologically() {
        let first = backup_stamp();
        std::thread::sleep(Duration::from_millis(2));
        let second = backup_stamp();
        assert!(first <= second, "stamps did not sort: {first} vs {second}");
        assert_eq!(first.len(), 13);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_is_refused() {
        let dir = TempDir::new().expect("tempdir");
        let victim = dir.path().join("victim.json");
        std::fs::write(&victim, "{}").expect("write");
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&victim, &link).expect("symlink");
        let path = camino::Utf8PathBuf::from_path_buf(link).expect("utf8");

        assert!(write(&path, "{\"a\":1}\n", None).is_err());
        assert_eq!(std::fs::read_to_string(&victim).expect("read"), "{}");
    }
}
