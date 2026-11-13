//! Keeps the unsent draft on disk while it is typed, so a session that ends
//! without a clean exit gives it back next time in the same workspace.

use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::paths::Paths;
use sha2::{Digest, Sha256};

/// How long after one write the next may follow.
const WRITE_INTERVAL: Duration = Duration::from_secs(2);

/// The largest draft written.
const MAX_DRAFT_BYTES: usize = 256 * 1024;

/// What the session says when it brings a draft back.
pub const RESTORED: &str = "restored the unsent draft from a session that did not exit cleanly";

/// `<state root>/drafts/<first 16 hex digits of SHA-256 of the canonical
/// workspace>.txt`, so drafts of two workspaces never meet.
#[must_use]
pub fn path(paths: &Paths, workspace: &Utf8Path) -> Utf8PathBuf {
    let canonical = std::fs::canonicalize(workspace)
        .ok()
        .and_then(|path| Utf8PathBuf::from_path_buf(path).ok())
        .unwrap_or_else(|| workspace.to_path_buf());
    let digest = Sha256::digest(canonical.as_str().as_bytes());
    let name = digest.iter().take(8).fold(String::new(), |mut name, byte| {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
        name
    });
    paths.state_root.join("drafts").join(format!("{name}.txt"))
}

/// Takes the draft a session that did not exit cleanly left, removing the file.
#[must_use]
pub fn take(path: &Utf8Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path);
    (!text.is_empty()).then_some(text)
}

/// Writes the draft as it changes, at most every two seconds.
#[derive(Debug)]
pub struct Autosave {
    path: Utf8PathBuf,
    last_written: Option<Instant>,
    last_text: String,
}

impl Autosave {
    /// Saves to `path`.
    #[must_use]
    pub const fn new(path: Utf8PathBuf) -> Self {
        Self {
            path,
            last_written: None,
            last_text: String::new(),
        }
    }

    /// Writes `text` when it differs from what was written and two seconds
    /// have passed since the last write.
    pub fn observe(&mut self, text: &str) {
        self.observe_at(text, Instant::now());
    }

    /// [`Self::observe`] at a given time, for a test's clock.
    pub fn observe_at(&mut self, text: &str, now: Instant) {
        if text == self.last_text {
            return;
        }
        if self
            .last_written
            .is_some_and(|last| now.saturating_duration_since(last) < WRITE_INTERVAL)
        {
            return;
        }
        self.write(text, now);
    }

    /// How long until a change to `text` may be written, when one is waiting.
    #[must_use]
    pub fn due_in(&self, text: &str) -> Option<Duration> {
        if text == self.last_text {
            return None;
        }
        Some(self.last_written.map_or(Duration::ZERO, |last| {
            WRITE_INTERVAL.saturating_sub(last.elapsed())
        }))
    }

    /// Writes `text` now.
    pub fn flush(&mut self, text: &str) {
        self.write(text, Instant::now());
    }

    /// Removes the saved draft: it was sent, or the session ended cleanly.
    pub fn clear(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        self.last_text.clear();
    }

    fn write(&mut self, text: &str, now: Instant) {
        self.last_written = Some(now);
        text.clone_into(&mut self.last_text);
        if text.is_empty() {
            let _ = std::fs::remove_file(&self.path);
            return;
        }
        if text.len() > MAX_DRAFT_BYTES {
            return;
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = rune_core::paths::write_private_atomic(&self.path, text);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    fn fixture() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("drafts").join("x.txt")).expect("utf8");
        (dir, path)
    }

    #[test]
    fn observe_writes_at_most_every_two_seconds() {
        let (_dir, path) = fixture();
        let mut autosave = Autosave::new(path.clone());
        let start = Instant::now();
        autosave.observe_at("a", start);
        assert_eq!(std::fs::read_to_string(&path).expect("first"), "a");
        autosave.observe_at("ab", start + Duration::from_millis(500));
        assert_eq!(std::fs::read_to_string(&path).expect("held"), "a");
        autosave.observe_at("abc", start + Duration::from_secs(2));
        assert_eq!(std::fs::read_to_string(&path).expect("second"), "abc");
        assert_eq!(autosave.due_in("abc"), None);
        assert!(autosave.due_in("abcd").is_some());
        autosave.flush("abcd");
        assert_eq!(std::fs::read_to_string(&path).expect("flushed"), "abcd");
    }

    #[test]
    fn an_empty_draft_removes_the_file() {
        let (_dir, path) = fixture();
        let mut autosave = Autosave::new(path.clone());
        autosave.flush("draft");
        assert!(path.exists());
        autosave.flush("");
        assert!(!path.exists());
        autosave.flush("again");
        autosave.clear();
        assert!(!path.exists());
        assert_eq!(take(&path), None);
        std::fs::write(&path, "left").expect("left");
        assert_eq!(take(&path).as_deref(), Some("left"));
        assert!(!path.exists());
    }

    #[test]
    fn drafts_of_two_workspaces_do_not_meet() {
        let paths = Paths::resolve(None, None, None, None, Some("/tmp/rune-draft-test"));
        let one = path(&paths, Utf8Path::new("/work/one"));
        let two = path(&paths, Utf8Path::new("/work/two"));
        assert_ne!(one, two);
        assert!(one.starts_with(paths.state_root.join("drafts")));
        assert_eq!(one.file_name().map(str::len), Some(16 + 4));
    }
}
