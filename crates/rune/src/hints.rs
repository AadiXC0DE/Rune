//! One-time hints that point at a feature until the user has used it.
//!
//! What has been used is kept in `<state root>/hints.json`, so a hint stops
//! once its feature is known, across sessions. A hint is offered at most once
//! per session even while unused.

use std::collections::BTreeSet;

use camino::{Utf8Path, Utf8PathBuf};

/// A feature a hint teaches.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Hint {
    /// Tab queues a message while a turn runs.
    Queue,
    /// `@` mentions a file.
    Mention,
    /// Ctrl-O opens the full transcript.
    Transcript,
    /// A double Escape rewinds.
    Rewind,
}

impl Hint {
    /// The hint's text.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Queue => "tab queues a message while a turn runs",
            Self::Mention => "@ mentions a file",
            Self::Transcript => "ctrl-o opens the full transcript",
            Self::Rewind => "double esc rewinds",
        }
    }
}

/// The file's shape.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Stored {
    #[serde(default)]
    used: BTreeSet<Hint>,
}

/// The hints used so far and those shown in this session.
#[derive(Debug)]
pub struct Hints {
    used: BTreeSet<Hint>,
    shown_this_session: BTreeSet<Hint>,
    path: Utf8PathBuf,
}

impl Hints {
    /// Reads the used hints under `state_root`; a missing or unreadable file
    /// means none was used.
    #[must_use]
    pub fn load(state_root: &Utf8Path) -> Self {
        let path = state_root.join("hints.json");
        let used = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Stored>(&text).ok())
            .map(|stored| stored.used)
            .unwrap_or_default();
        Self {
            used,
            shown_this_session: BTreeSet::new(),
            path,
        }
    }

    /// Records that the user used a feature, so its hint never shows again.
    /// A failed write is ignored: a hint shown once more is harmless.
    pub fn used(&mut self, hint: Hint) {
        if !self.used.insert(hint) {
            return;
        }
        let stored = Stored {
            used: self.used.clone(),
        };
        if let Ok(text) = serde_json::to_string(&stored) {
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = rune_core::paths::write_private(&self.path, &text);
        }
    }

    /// Returns the hint's text when it was never used and not yet shown in
    /// this session, and marks it shown.
    pub fn offer(&mut self, hint: Hint) -> Option<&'static str> {
        if self.used.contains(&hint) || !self.shown_this_session.insert(hint) {
            return None;
        }
        Some(hint.text())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    fn root() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("utf8");
        (dir, path)
    }

    #[test]
    fn a_used_hint_is_never_offered() {
        let (_dir, root) = root();
        let mut hints = Hints::load(&root);
        hints.used(Hint::Queue);
        assert_eq!(hints.offer(Hint::Queue), None);
        assert_eq!(hints.offer(Hint::Mention), Some("@ mentions a file"));
    }

    #[test]
    fn a_hint_is_offered_once_per_session() {
        let (_dir, root) = root();
        let mut hints = Hints::load(&root);
        assert_eq!(
            hints.offer(Hint::Transcript),
            Some("ctrl-o opens the full transcript")
        );
        assert_eq!(hints.offer(Hint::Transcript), None);
        // Shown is not used: a new session offers it again.
        let mut next = Hints::load(&root);
        assert!(next.offer(Hint::Transcript).is_some());
    }

    #[test]
    fn hints_survive_a_restart() {
        let (_dir, root) = root();
        let mut hints = Hints::load(&root);
        hints.used(Hint::Mention);
        hints.used(Hint::Rewind);
        let text = std::fs::read_to_string(root.join("hints.json")).expect("saved");
        assert_eq!(text, r#"{"used":["mention","rewind"]}"#);
        let mut restarted = Hints::load(&root);
        assert_eq!(restarted.offer(Hint::Mention), None);
        assert_eq!(restarted.offer(Hint::Rewind), None);
        assert!(restarted.offer(Hint::Queue).is_some());
    }
}
