//! Prompt history.
//!
//! A bounded record of what the user typed, so the composer can recall it. It is
//! stored as one entry per line, newest last, and compacted once it grows past a
//! threshold so an unbounded file cannot accumulate over a long-lived install.
//!
//! History is workspace-scoped and session-scoped separately. Recall in a
//! composer is per workspace, because a prompt written while working in one
//! repository rarely belongs in another, and the file itself records the session
//! so a caller can filter either way.

use std::collections::VecDeque;

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;

/// Most entries kept after compaction.
pub const MAX_ENTRIES: usize = 1_000;

/// Largest the file is allowed to grow, in bytes.
///
/// It is also what [`History::open`] reads a file at, so recording drops the
/// oldest entries to stay under it. Without that, a thousand long prompts
/// outgrow the read and recall stops working for good.
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Most a file past [`MAX_BYTES`] can hold.
///
/// Only the entry count bounds such a file, and each entry is bounded by
/// [`MAX_PROMPT_BYTES`], which JSON can expand six times over, plus its
/// location. It is read at this size so its prompts are kept, and the next
/// write brings it back under [`MAX_BYTES`].
const OVERSIZED_BYTES: u64 = (MAX_ENTRIES as u64).saturating_mul(
    (MAX_PROMPT_BYTES as u64)
        .saturating_mul(6)
        .saturating_add(4096),
);

/// Longest single prompt recorded.
///
/// A pasted file is not history; recording megabytes per keystroke would make
/// the file useless and the cap meaningless.
pub const MAX_PROMPT_BYTES: usize = 8 * 1024;

/// One recorded prompt.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// The text the user submitted.
    pub text: String,
    /// Workspace it was submitted in, absent when none was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Session it was submitted in, absent when none was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl Entry {
    /// Builds an entry with no location.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            workspace: None,
            session: None,
        }
    }

    /// Attaches the workspace and session.
    #[must_use]
    pub fn located(mut self, workspace: &Utf8Path, session: &str) -> Self {
        self.workspace = Some(workspace.to_string());
        self.session = Some(session.to_owned());
        self
    }

    /// Returns the line this entry is stored as.
    ///
    /// Encoded as JSON so a prompt containing a newline or a tab cannot split
    /// into two entries.
    pub fn encode(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// Reads one stored line.
    pub fn decode(line: &str) -> Result<Self> {
        serde_json::from_str(line).map_err(|err| {
            RuneError::new(
                ErrorCode::CorruptRecord,
                format!("a history entry could not be read: {err}"),
            )
            .with_hint("the damaged line is skipped; remove the file to start clean")
        })
    }
}

/// Prompt history for one installation.
#[derive(Debug)]
pub struct History {
    path: Utf8PathBuf,
    entries: VecDeque<Entry>,
}

impl History {
    /// Opens the history file.
    ///
    /// A missing file is an empty history. A damaged line is skipped rather than
    /// failing the whole file, because one bad line would otherwise make every
    /// earlier prompt unreachable.
    pub fn open(paths: &Paths) -> Result<Self> {
        let path = paths.history_file();
        let text = match rune_core::paths::read_private(&path, MAX_BYTES) {
            Err(err) if err.code() == ErrorCode::TooLarge => {
                rune_core::paths::read_private(&path, OVERSIZED_BYTES)?
            }
            other => other?,
        };
        let mut entries = VecDeque::new();
        if let Some(text) = text {
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(entry) = Entry::decode(line) {
                    entries.push_back(entry);
                }
            }
        }
        while entries.len() > MAX_ENTRIES {
            entries.pop_front();
        }
        Ok(Self { path, entries })
    }

    /// Returns the number of entries held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns the path the history is stored at.
    #[must_use]
    pub fn path(&self) -> &Utf8Path {
        &self.path
    }

    /// Records one prompt.
    ///
    /// A repeat of the most recent prompt is not recorded again, because holding
    /// the arrow key would otherwise fill the history with one line.
    pub fn record(&mut self, entry: Entry) -> Result<()> {
        let text = truncate(&entry.text, MAX_PROMPT_BYTES);
        if text.trim().is_empty() {
            return Ok(());
        }
        if self.entries.back().map(|last| last.text.as_str()) == Some(text.as_str()) {
            return Ok(());
        }

        let mut entry = entry;
        entry.text = text;
        self.entries.push_back(entry);
        if self.entries.len() > MAX_ENTRIES {
            let excess = self.entries.len().saturating_sub(MAX_ENTRIES);
            for _ in 0..excess {
                self.entries.pop_front();
            }
        }
        self.rewrite()
    }

    /// Returns the prompts recorded for one workspace, oldest first.
    #[must_use]
    pub fn for_workspace(&self, workspace: &Utf8Path) -> Vec<&Entry> {
        let wanted = workspace.as_str();
        self.entries
            .iter()
            .filter(|entry| entry.workspace.as_deref() == Some(wanted))
            .collect()
    }

    /// Returns the prompts recorded for one session, oldest first.
    #[must_use]
    pub fn for_session(&self, session: &str) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.session.as_deref() == Some(session))
            .collect()
    }

    /// Returns every entry, oldest first.
    #[must_use]
    pub fn entries(&self) -> Vec<&Entry> {
        self.entries.iter().collect()
    }

    /// Removes every entry.
    pub fn clear(&mut self) -> Result<()> {
        self.entries.clear();
        self.rewrite()
    }

    /// Writes the whole file, dropping the oldest entries past the byte cap.
    ///
    /// Written whole rather than appended: the file is bounded and small, and a
    /// rewrite is what makes compaction and removal atomic. The text goes to a
    /// file beside it that is then renamed over it, so a write cut short leaves
    /// the previous history in place rather than a truncated one.
    fn rewrite(&mut self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            rune_core::paths::create_dir_private(parent)?;
        }
        let mut lines: VecDeque<String> = VecDeque::with_capacity(self.entries.len());
        let mut total: usize = 0;
        for entry in &self.entries {
            let line = entry.encode()?;
            total = total.saturating_add(line.len()).saturating_add(1);
            lines.push_back(line);
        }
        let budget = usize::try_from(MAX_BYTES).unwrap_or(usize::MAX);
        while total > budget {
            let Some(oldest) = lines.pop_front() else {
                break;
            };
            total = total.saturating_sub(oldest.len().saturating_add(1));
            self.entries.pop_front();
        }

        let mut out = String::with_capacity(total);
        for line in &lines {
            out.push_str(line);
            out.push('\n');
        }
        let staged = Utf8PathBuf::from(format!("{}.{}.tmp", self.path, std::process::id()));
        if let Err(err) = rune_core::paths::write_private(&staged, &out) {
            let _ = std::fs::remove_file(&staged);
            return Err(err);
        }
        std::fs::rename(&staged, &self.path).map_err(|err| {
            let _ = std::fs::remove_file(&staged);
            RuneError::new(
                ErrorCode::Internal,
                format!("the history could not be replaced: {err}"),
            )
        })
    }
}

/// Cuts text to a byte budget without splitting a character.
fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut cut = limit;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut = cut.saturating_sub(1);
    }
    text[..cut].to_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn paths(root: &Utf8Path) -> Paths {
        let resolved = Paths::resolve(
            Some(root.as_str()),
            Some(root.as_str()),
            Some(root.as_str()),
            Some(root.as_str()),
            None,
        );
        resolved.ensure_roots().expect("roots");
        resolved
    }

    #[test]
    fn history_survives_a_reopen() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);

        let mut history = History::open(&paths).expect("open");
        history
            .record(Entry::new("first").located(root, "s1"))
            .expect("record");
        history
            .record(Entry::new("second").located(root, "s1"))
            .expect("record");
        drop(history);

        let reopened = History::open(&paths).expect("reopen");
        let texts: Vec<&str> = reopened.entries().iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["first", "second"]);
    }

    #[test]
    fn a_prompt_holding_a_newline_stays_one_entry() {
        // A line-oriented file would split this into two prompts.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut history = History::open(&paths).expect("open");
        history
            .record(Entry::new("line one\nline two"))
            .expect("record");
        drop(history);

        let reopened = History::open(&paths).expect("reopen");
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened.entries()[0].text, "line one\nline two");
    }

    #[test]
    fn a_repeat_of_the_last_prompt_is_not_recorded_twice() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut history = History::open(&paths).expect("open");
        history.record(Entry::new("same")).expect("record");
        history.record(Entry::new("same")).expect("record");
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn a_repeat_after_a_different_prompt_is_recorded() {
        // Only the immediately previous one is suppressed, so a deliberate
        // return to an earlier prompt is kept.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut history = History::open(&paths).expect("open");
        history.record(Entry::new("a")).expect("record");
        history.record(Entry::new("b")).expect("record");
        history.record(Entry::new("a")).expect("record");
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn an_empty_prompt_is_not_recorded() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut history = History::open(&paths).expect("open");
        history.record(Entry::new("   \n ")).expect("record");
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn the_file_is_compacted_to_the_entry_bound() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut history = History::open(&paths).expect("open");
        for index in 0..MAX_ENTRIES.saturating_add(50) {
            history
                .record(Entry::new(format!("prompt {index}")))
                .expect("record");
        }
        assert_eq!(history.len(), MAX_ENTRIES);

        let reopened = History::open(&paths).expect("reopen");
        assert_eq!(
            reopened.len(),
            MAX_ENTRIES,
            "the cap did not survive a reopen"
        );
        // The oldest were dropped, so the newest is still present.
        let last = MAX_ENTRIES.saturating_add(49);
        assert_eq!(
            reopened.entries().last().map(|e| e.text.as_str()),
            Some(format!("prompt {last}").as_str())
        );
    }

    /// Returns a line of history holding one prompt of the largest size kept.
    fn large_line(index: usize) -> String {
        let text = format!(
            "{index:04} {}",
            "x".repeat(MAX_PROMPT_BYTES.saturating_sub(5))
        );
        Entry::new(text).encode().expect("encoded")
    }

    #[test]
    fn recording_keeps_the_file_small_enough_to_open_again() {
        // A thousand entries of up to eight kilobytes each is more than the
        // read cap, and a file past it would have to be read at the larger
        // size on every start.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut history = History::open(&paths).expect("open");
        for index in 0..600 {
            let line = large_line(index);
            history
                .entries
                .push_back(Entry::decode(&line).expect("decoded"));
        }
        history.record(Entry::new("the newest")).expect("recorded");

        let size = std::fs::metadata(paths.history_file())
            .expect("metadata")
            .len();
        assert!(size <= MAX_BYTES, "the file grew to {size} bytes");
        let reopened = History::open(&paths).expect("the history opens again");
        assert_eq!(
            reopened.entries().last().map(|entry| entry.text.as_str()),
            Some("the newest")
        );
        assert!(reopened.len() < 601, "nothing old was dropped");
        assert_eq!(reopened.len(), history.len());
    }

    #[test]
    fn a_file_that_outgrew_the_cap_is_opened_and_brought_back_under_it() {
        // A file already past the cap is read in full and brought back under
        // it, rather than failing to open and taking recall with it.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut text = String::new();
        for index in 0..600 {
            text.push_str(&large_line(index));
            text.push('\n');
        }
        assert!(text.len() as u64 > MAX_BYTES);
        rune_core::paths::write_private(&paths.history_file(), &text).expect("write");

        let mut history = History::open(&paths).expect("an oversized history opens");
        assert_eq!(history.len(), 600);
        history.record(Entry::new("after")).expect("recorded");
        let size = std::fs::metadata(paths.history_file())
            .expect("metadata")
            .len();
        assert!(size <= MAX_BYTES, "the file is still {size} bytes");
    }

    #[test]
    fn a_rewrite_leaves_no_staging_file_behind() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut history = History::open(&paths).expect("open");
        history.record(Entry::new("one")).expect("recorded");
        history.record(Entry::new("two")).expect("recorded");

        let staged = format!("{}.{}.tmp", paths.history_file(), std::process::id());
        assert!(
            !Utf8Path::new(&staged).exists(),
            "the staging file was left at {staged}"
        );
        assert_eq!(History::open(&paths).expect("reopen").len(), 2);
    }

    #[test]
    fn an_oversized_prompt_is_truncated_on_a_character_boundary() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut history = History::open(&paths).expect("open");
        history
            .record(Entry::new("書".repeat(MAX_PROMPT_BYTES)))
            .expect("record");

        let text = &history.entries()[0].text;
        assert!(text.len() <= MAX_PROMPT_BYTES);
        // A byte cut through a wide character would leave invalid text.
        assert!(text.is_char_boundary(text.len()));
    }

    #[test]
    fn a_damaged_line_is_skipped_and_the_rest_survive() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        rune_core::paths::write_private(
            &paths.history_file(),
            "{\"text\":\"good\"}\nnot json\n{\"text\":\"also good\"}\n",
        )
        .expect("write");

        let history = History::open(&paths).expect("open");
        let texts: Vec<&str> = history.entries().iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["good", "also good"]);
    }

    #[test]
    fn recall_can_be_scoped_to_a_workspace_or_a_session() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let one = root.join("one");
        let two = root.join("two");
        let mut history = History::open(&paths).expect("open");
        history
            .record(Entry::new("in one").located(&one, "s1"))
            .expect("record");
        history
            .record(Entry::new("in two").located(&two, "s2"))
            .expect("record");
        history
            .record(Entry::new("again in one").located(&one, "s1"))
            .expect("record");

        let in_one: Vec<&str> = history
            .for_workspace(&one)
            .iter()
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(in_one, ["in one", "again in one"]);

        let in_s2: Vec<&str> = history
            .for_session("s2")
            .iter()
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(in_s2, ["in two"]);
    }

    #[test]
    fn clearing_removes_every_entry_from_disk() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut history = History::open(&paths).expect("open");
        history.record(Entry::new("gone")).expect("record");
        history.clear().expect("cleared");
        drop(history);

        assert_eq!(History::open(&paths).expect("reopen").len(), 0);
    }

    #[test]
    fn a_missing_file_is_an_empty_history() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        assert_eq!(History::open(&paths(root)).expect("open").len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_history_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let mut history = History::open(&paths).expect("open");
        history.record(Entry::new("secret-ish")).expect("record");

        let mode = std::fs::metadata(paths.history_file())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the history file is readable by others");
    }

    #[test]
    fn an_entry_round_trips_through_its_encoding() {
        let entry = Entry::new("a\tb").located(Utf8Path::new("/w"), "s");
        let back = Entry::decode(&entry.encode().expect("encoded")).expect("decoded");
        assert_eq!(back, entry);
    }
}
