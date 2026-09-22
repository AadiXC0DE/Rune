//! Line editing for the composer.
//!
//! The cursor is a character index, never a byte offset, so it cannot land
//! inside a multi-byte character. Movement and deletion additionally step by
//! grapheme cluster, which keeps the cursor out of the middle of a cluster that
//! spans several characters, such as a letter with a combining accent or an
//! emoji with a skin tone modifier.
//!
//! Every edit records the state it replaced, so the line can be walked back and
//! forward again. The history is bounded, because a long session would
//! otherwise grow it without limit. Killing text keeps it for a later yank,
//! which is what makes a mistaken kill recoverable.

use std::collections::VecDeque;

use crate::width::{graphemes, str_width};

/// How many editing steps are kept for undo.
pub const HISTORY_LIMIT: usize = 128;

/// One editor state.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Snapshot {
    text: String,
    cursor: usize,
}

/// The outcome of a paste.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Paste {
    /// The text fit under the threshold and was inserted at the cursor.
    Inserted {
        /// Characters inserted.
        chars: usize,
    },
    /// The text was longer than the threshold and was not inserted.
    ///
    /// Echoing a large paste into the line would push the rest of the session
    /// off the screen, so the caller is handed a marker instead. The caller
    /// still holds the text and can expand the marker with
    /// [`Composer::insert`] once it knows where to put it.
    Placeholder {
        /// Text that stands in for the withheld paste.
        marker: String,
        /// Characters the withheld text holds.
        chars: usize,
    },
}

impl Paste {
    /// Returns the placeholder marker, when the paste was withheld.
    #[must_use]
    pub fn marker(&self) -> Option<&str> {
        match self {
            Self::Inserted { .. } => None,
            Self::Placeholder { marker, .. } => Some(marker),
        }
    }
}

/// The candidates completing the word at the cursor.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Completions {
    /// Matching candidates, in the order they were offered.
    pub matches: Vec<String>,
    /// The longest prefix every match shares, ready to replace the typed word.
    pub common: String,
}

/// A single line editor.
#[derive(Clone, Default, Debug)]
pub struct Composer {
    text: String,
    cursor: usize,
    undo: VecDeque<Snapshot>,
    redo: VecDeque<Snapshot>,
    kill: String,
    recall: Option<usize>,
    draft: String,
}

impl Composer {
    /// Creates an empty composer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the text being edited.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the cursor as a count of characters, not bytes.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Returns whether the line is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Returns the columns the line occupies.
    #[must_use]
    pub fn width(&self) -> usize {
        str_width(&self.text)
    }

    /// Returns the columns before the cursor.
    ///
    /// This is where a renderer places the cursor, which is not the same as the
    /// character count: a wide character advances two columns.
    #[must_use]
    pub fn cursor_column(&self) -> usize {
        str_width(self.head())
    }

    /// Inserts text at the cursor.
    pub fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.record();
        let at = self.byte_of(self.cursor);
        self.text.insert_str(at, text);
        self.cursor = self.cursor.saturating_add(text.chars().count());
        self.settle();
    }

    /// Empties the line.
    pub fn clear(&mut self) {
        if self.text.is_empty() {
            return;
        }
        self.record();
        self.text.clear();
        self.cursor = 0;
    }

    /// Removes the cluster before the cursor.
    pub fn delete_back(&mut self) -> bool {
        let start = self.floor(self.cursor.saturating_sub(1));
        if start == self.cursor {
            return false;
        }
        self.record();
        let from = self.byte_of(start);
        let to = self.byte_of(self.cursor);
        self.text.replace_range(from..to, "");
        self.cursor = start;
        true
    }

    /// Removes the cluster at the cursor.
    pub fn delete_forward(&mut self) -> bool {
        let end = self.ceil(self.cursor);
        if end == self.cursor {
            return false;
        }
        self.record();
        let from = self.byte_of(self.cursor);
        let to = self.byte_of(end);
        self.text.replace_range(from..to, "");
        true
    }

    /// Moves the cursor one cluster left.
    pub fn move_left(&mut self) -> bool {
        let at = self.floor(self.cursor.saturating_sub(1));
        let moved = at != self.cursor;
        self.cursor = at;
        moved
    }

    /// Moves the cursor one cluster right.
    pub fn move_right(&mut self) -> bool {
        let at = self.ceil(self.cursor);
        let moved = at != self.cursor;
        self.cursor = at;
        moved
    }

    /// Moves the cursor to the start of the word before it.
    pub fn move_word_left(&mut self) -> bool {
        let mut count = 0usize;
        let mut word = false;
        for c in self.head().chars().rev() {
            if c.is_whitespace() {
                if word {
                    break;
                }
            } else {
                word = true;
            }
            count = count.saturating_add(1);
        }
        self.jump(self.cursor.saturating_sub(count))
    }

    /// Moves the cursor to the end of the word after it.
    pub fn move_word_right(&mut self) -> bool {
        let mut count = 0usize;
        let mut word = false;
        for c in self.tail().chars() {
            if c.is_whitespace() {
                if word {
                    break;
                }
            } else {
                word = true;
            }
            count = count.saturating_add(1);
        }
        self.jump(self.cursor.saturating_add(count))
    }

    /// Moves the cursor to the start of the line.
    pub fn move_home(&mut self) -> bool {
        self.jump(0)
    }

    /// Moves the cursor to the end of the line.
    pub fn move_end(&mut self) -> bool {
        self.jump(self.text.chars().count())
    }

    /// Removes everything from the cursor to the end of the line, keeping it
    /// for a yank.
    pub fn kill_to_end(&mut self) -> bool {
        let at = self.byte_of(self.cursor);
        if at >= self.text.len() {
            return false;
        }
        self.record();
        self.text
            .get(at..)
            .unwrap_or_default()
            .clone_into(&mut self.kill);
        self.text.truncate(at);
        true
    }

    /// Removes everything from the start of the line to the cursor, keeping it
    /// for a yank.
    pub fn kill_to_start(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.record();
        let at = self.byte_of(self.cursor);
        self.text
            .get(..at)
            .unwrap_or_default()
            .clone_into(&mut self.kill);
        self.text.replace_range(..at, "");
        self.cursor = 0;
        true
    }

    /// Inserts the last killed text at the cursor.
    pub fn yank(&mut self) -> bool {
        if self.kill.is_empty() {
            return false;
        }
        let killed = self.kill.clone();
        self.insert(&killed);
        true
    }

    /// Reverts the last edit.
    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop_back() else {
            return false;
        };
        self.push_redo(self.snapshot());
        self.text = previous.text;
        self.cursor = previous.cursor;
        true
    }

    /// Reapplies the last reverted edit.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop_back() else {
            return false;
        };
        self.push_undo(self.snapshot());
        self.text = next.text;
        self.cursor = next.cursor;
        true
    }

    /// Inserts pasted text, or withholds it when it is longer than `threshold`.
    ///
    /// The threshold is counted in characters. A paste at or under it is
    /// inserted like any other text; a longer one is left out of the line, and
    /// the returned marker stands in for it.
    pub fn paste(&mut self, text: &str, threshold: usize) -> Paste {
        let chars = text.chars().count();
        if chars <= threshold {
            self.insert(text);
            return Paste::Inserted { chars };
        }
        Paste::Placeholder {
            marker: format!("[pasted {chars} characters]"),
            chars,
        }
    }

    /// Recalls the previous entry of a history.
    ///
    /// The entry nearest the cursor is the most recent one. At the oldest entry
    /// navigation stops rather than wrapping to the newest. The line being
    /// edited when recall starts is kept until [`Composer::history_next`]
    /// walks back past the newest entry.
    pub fn history_previous(&mut self, entries: &[String]) -> bool {
        let Some(newest) = entries.len().checked_sub(1) else {
            return false;
        };
        let next = match self.recall {
            None => {
                self.text.clone_into(&mut self.draft);
                newest
            }
            Some(0) => return false,
            Some(at) => at.saturating_sub(1),
        };
        let entry = entries.get(next).map_or("", String::as_str);
        let entry = entry.to_owned();
        self.show(&entry);
        self.recall = Some(next);
        true
    }

    /// Recalls the next entry of a history.
    ///
    /// The entry after the newest one is the line that was being edited, which
    /// parks navigation back at the end.
    pub fn history_next(&mut self, entries: &[String]) -> bool {
        let Some(at) = self.recall else {
            return false;
        };
        let after = at.saturating_add(1);
        if let Some(entry) = entries.get(after) {
            let entry = entry.clone();
            self.show(&entry);
            self.recall = Some(after);
        } else {
            let draft = std::mem::take(&mut self.draft);
            self.show(&draft);
            self.recall = None;
        }
        true
    }

    /// Returns the candidates completing the slash command being typed.
    ///
    /// A candidate matches when its name, with or without a leading slash,
    /// starts with the word after the slash. A line carrying arguments, or one
    /// that does not start with a slash, completes nothing. Candidates are
    /// returned in the form a command is typed, with the leading slash.
    #[must_use]
    pub fn completions(&self, candidates: &[String]) -> Completions {
        let Some(word) = self.text.strip_prefix('/') else {
            return Completions::default();
        };
        if word.chars().any(char::is_whitespace) {
            return Completions::default();
        }
        let matches: Vec<String> = candidates
            .iter()
            .filter(|candidate| name(candidate).starts_with(word))
            .map(|candidate| slashed(candidate))
            .collect();
        let common = common_prefix(matches.iter().map(String::as_str));
        Completions { matches, common }
    }

    /// Replaces the line, parking the cursor at the end.
    fn show(&mut self, text: &str) {
        text.clone_into(&mut self.text);
        self.undo.clear();
        self.redo.clear();
        self.cursor = self.text.chars().count();
    }

    /// Moves the cursor to `at`, keeping it on a cluster boundary.
    fn jump(&mut self, at: usize) -> bool {
        let at = self.floor(at);
        let moved = at != self.cursor;
        self.cursor = at;
        moved
    }

    /// Returns the state an edit is about to replace.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        }
    }

    /// Records the state an edit is about to replace.
    ///
    /// Recording discards the redo stack: once the line moves on from a
    /// reverted state, replaying that state is no longer what the user asked
    /// for.
    fn record(&mut self) {
        self.redo.clear();
        self.push_undo(self.snapshot());
    }

    /// Pushes a state onto the undo stack, dropping the oldest past the bound.
    fn push_undo(&mut self, state: Snapshot) {
        self.undo.push_back(state);
        while self.undo.len() > HISTORY_LIMIT {
            self.undo.pop_front();
        }
    }

    /// Pushes a state onto the redo stack, dropping the oldest past the bound.
    fn push_redo(&mut self, state: Snapshot) {
        self.redo.push_back(state);
        while self.redo.len() > HISTORY_LIMIT {
            self.redo.pop_front();
        }
    }

    /// Returns the byte offset of a character index.
    fn byte_of(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(byte, _)| byte)
    }

    /// Returns the closest cluster boundary at or before a character index.
    fn floor(&self, at: usize) -> usize {
        let mut chars = 0usize;
        for grapheme in graphemes(&self.text) {
            if chars >= at {
                break;
            }
            let next = chars.saturating_add(grapheme.chars().count());
            if next > at {
                return chars;
            }
            chars = next;
        }
        chars
    }

    /// Returns the first cluster boundary after a character index.
    fn ceil(&self, at: usize) -> usize {
        let mut chars = 0usize;
        for grapheme in graphemes(&self.text) {
            chars = chars.saturating_add(grapheme.chars().count());
            if chars > at {
                break;
            }
        }
        chars
    }

    /// Returns the text before the cursor.
    fn head(&self) -> &str {
        self.text
            .get(..self.byte_of(self.cursor))
            .unwrap_or_default()
    }

    /// Returns the text at and after the cursor.
    fn tail(&self) -> &str {
        self.text
            .get(self.byte_of(self.cursor)..)
            .unwrap_or_default()
    }

    /// Keeps the cursor inside the line and off the interior of a cluster.
    fn settle(&mut self) {
        let total = self.text.chars().count();
        self.cursor = self.floor(self.cursor.min(total));
    }
}

/// Returns a candidate without its leading slash.
fn name(candidate: &str) -> &str {
    candidate.strip_prefix('/').unwrap_or(candidate)
}

/// Returns a candidate in the form a command is typed.
fn slashed(candidate: &str) -> String {
    let name = name(candidate);
    let mut out = String::with_capacity(name.len().saturating_add(1));
    out.push('/');
    out.push_str(name);
    out
}

/// Returns the longest prefix every string shares.
fn common_prefix<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let mut items = items;
    let Some(first) = items.next() else {
        return String::new();
    };
    let mut end = first.len();
    for other in items {
        let shared = first
            .chars()
            .zip(other.chars())
            .take_while(|(left, right)| left == right)
            .map(|(c, _)| c.len_utf8())
            .fold(0usize, usize::saturating_add);
        end = end.min(shared);
    }
    first.get(..end).unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the byte offsets that start a cluster in `text`.
    fn boundaries(text: &str) -> Vec<usize> {
        let mut at = 0usize;
        let mut edges = vec![0usize];
        for grapheme in graphemes(text) {
            at = at.saturating_add(grapheme.len());
            edges.push(at);
        }
        edges
    }

    #[test]
    fn insertion_happens_at_the_cursor() {
        let mut composer = Composer::new();
        composer.insert("world");
        composer.move_home();
        composer.insert("hello ");
        assert_eq!(composer.text(), "hello world");
        assert_eq!(composer.cursor(), 6);
    }

    #[test]
    fn the_cursor_never_lands_inside_a_character() {
        let text = "書e\u{301}👍🏽x";
        let mut composer = Composer::new();
        composer.insert(text);
        assert_eq!(composer.cursor(), 6);

        let edges = boundaries(text);
        let mut stops = vec![composer.cursor()];
        while composer.move_left() {
            stops.push(composer.cursor());
        }
        assert_eq!(stops, vec![6, 5, 3, 1, 0]);
        for stop in &stops {
            let byte = composer.byte_of(*stop);
            assert!(edges.contains(&byte), "cursor landed at byte {byte}");
        }

        while composer.move_right() {}
        assert_eq!(composer.cursor(), 6);
    }

    #[test]
    fn deletion_removes_whole_clusters() {
        let mut composer = Composer::new();
        composer.insert("e\u{301}👍🏽");
        assert!(composer.delete_back());
        assert_eq!(composer.text(), "e\u{301}");
        assert!(composer.delete_back());
        assert_eq!(composer.text(), "");
        assert!(!composer.delete_back());

        composer.insert("書x");
        composer.move_home();
        assert!(composer.delete_forward());
        assert_eq!(composer.text(), "x");
        assert_eq!(composer.cursor(), 0);
        assert!(composer.delete_forward());
        assert_eq!(composer.text(), "");
        assert!(!composer.delete_forward());
    }

    #[test]
    fn deletion_at_either_end_does_nothing() {
        let mut composer = Composer::new();
        assert!(!composer.delete_back());
        assert!(!composer.delete_forward());

        composer.insert("ab");
        composer.move_end();
        assert!(!composer.delete_forward());
        composer.move_home();
        assert!(!composer.delete_back());
        assert_eq!(composer.text(), "ab");
    }

    #[test]
    fn word_movement_stops_at_boundaries_and_ends() {
        let mut composer = Composer::new();
        composer.insert("alpha beta gamma");

        composer.move_home();
        assert!(composer.move_word_right());
        assert_eq!(composer.cursor(), 5);
        assert!(composer.move_word_right());
        assert_eq!(composer.cursor(), 10);
        assert!(composer.move_word_right());
        assert_eq!(composer.cursor(), 16);
        assert!(!composer.move_word_right());

        assert!(composer.move_word_left());
        assert_eq!(composer.cursor(), 11);
        assert!(composer.move_word_left());
        assert_eq!(composer.cursor(), 6);
        assert!(composer.move_word_left());
        assert_eq!(composer.cursor(), 0);
        assert!(!composer.move_word_left());
        assert_eq!(composer.text(), "alpha beta gamma");
    }

    #[test]
    fn undo_and_redo_round_trip() {
        let mut composer = Composer::new();
        composer.insert("hello");
        composer.insert(" world");
        assert_eq!(composer.text(), "hello world");

        assert!(composer.undo());
        assert_eq!(composer.text(), "hello");
        assert_eq!(composer.cursor(), 5);
        assert!(composer.undo());
        assert_eq!(composer.text(), "");
        assert_eq!(composer.cursor(), 0);
        assert!(!composer.undo());

        assert!(composer.redo());
        assert_eq!(composer.text(), "hello");
        assert!(composer.redo());
        assert_eq!(composer.text(), "hello world");
        assert_eq!(composer.cursor(), 11);
        assert!(!composer.redo());
    }

    #[test]
    fn an_edit_after_undo_discards_redo() {
        let mut composer = Composer::new();
        composer.insert("a");
        composer.insert("b");
        assert!(composer.undo());
        assert_eq!(composer.text(), "a");

        composer.insert("c");
        assert_eq!(composer.text(), "ac");
        assert!(!composer.redo());
        assert_eq!(composer.text(), "ac");
    }

    #[test]
    fn the_undo_stack_is_bounded() {
        let mut composer = Composer::new();
        let edits = HISTORY_LIMIT.saturating_add(10);
        for _ in 0..edits {
            composer.insert("x");
        }

        let mut undone = 0usize;
        while composer.undo() {
            undone = undone.saturating_add(1);
        }
        assert_eq!(undone, HISTORY_LIMIT);
        assert_eq!(composer.text().chars().count(), 10);
    }

    #[test]
    fn a_large_paste_is_withheld() {
        let pasted = "x".repeat(100_000);
        let mut composer = Composer::new();
        composer.insert("before ");
        let outcome = composer.paste(&pasted, 4096);
        let Paste::Placeholder { marker, chars } = outcome else {
            panic!("a large paste must not be inserted");
        };
        assert_eq!(chars, 100_000);
        assert!(marker.len() < 64);
        assert_eq!(composer.text(), "before ");
    }

    #[test]
    fn a_paste_at_the_threshold_is_inserted() {
        let pasted = "x".repeat(4096);
        let mut composer = Composer::new();
        assert_eq!(
            composer.paste(&pasted, 4096),
            Paste::Inserted { chars: 4096 }
        );
        assert_eq!(composer.text().chars().count(), 4096);
        assert_eq!(composer.cursor(), 4096);
    }

    #[test]
    fn history_navigation_clamps_at_both_ends() {
        let entries: Vec<String> = ["one", "two", "three"].map(str::to_owned).to_vec();
        let mut composer = Composer::new();
        composer.insert("draft");

        assert!(composer.history_previous(&entries));
        assert_eq!(composer.text(), "three");
        assert_eq!(composer.cursor(), 5);
        assert!(composer.history_previous(&entries));
        assert_eq!(composer.text(), "two");
        assert!(composer.history_previous(&entries));
        assert_eq!(composer.text(), "one");
        assert!(!composer.history_previous(&entries));
        assert_eq!(composer.text(), "one");

        assert!(composer.history_next(&entries));
        assert_eq!(composer.text(), "two");
        assert!(composer.history_next(&entries));
        assert_eq!(composer.text(), "three");
        assert!(composer.history_next(&entries));
        assert_eq!(composer.text(), "draft");
        assert!(!composer.history_next(&entries));
        assert_eq!(composer.text(), "draft");
    }

    #[test]
    fn an_empty_history_recalls_nothing() {
        let mut composer = Composer::new();
        composer.insert("draft");
        assert!(!composer.history_previous(&[]));
        assert!(!composer.history_next(&[]));
        assert_eq!(composer.text(), "draft");
    }

    #[test]
    fn a_kill_can_be_yanked_back() {
        let mut composer = Composer::new();
        assert!(!composer.yank());

        composer.insert("hello world");
        composer.move_home();
        assert!(composer.kill_to_end());
        assert_eq!(composer.text(), "");
        assert!(!composer.kill_to_end());
        assert!(composer.yank());
        assert_eq!(composer.text(), "hello world");
        assert_eq!(composer.cursor(), 11);

        composer.move_end();
        assert!(composer.kill_to_start());
        assert_eq!(composer.text(), "");
        assert!(!composer.kill_to_start());
        assert!(composer.yank());
        assert_eq!(composer.text(), "hello world");
        assert_eq!(composer.cursor(), 11);
    }

    #[test]
    fn a_kill_is_a_single_undo_step() {
        let mut composer = Composer::new();
        composer.insert("hello world");
        composer.move_home();
        assert!(composer.kill_to_end());
        assert!(composer.undo());
        assert_eq!(composer.text(), "hello world");
        assert_eq!(composer.cursor(), 0);
    }

    #[test]
    fn columns_count_wide_characters_as_two() {
        let mut composer = Composer::new();
        composer.insert("書x");
        assert_eq!(composer.width(), 3);
        assert_eq!(composer.cursor_column(), 3);
        composer.move_left();
        assert_eq!(composer.cursor_column(), 2);
    }

    #[test]
    fn completion_matches_a_leading_word() {
        let candidates: Vec<String> = ["/help", "/hello", "/history", "/quit"]
            .map(str::to_owned)
            .to_vec();
        let mut composer = Composer::new();
        composer.insert("/he");
        let completions = composer.completions(&candidates);
        assert_eq!(completions.matches, vec!["/help", "/hello"]);
        assert_eq!(completions.common, "/hel");

        composer.insert("llo");
        let completions = composer.completions(&candidates);
        assert_eq!(completions.matches, vec!["/hello"]);
        assert_eq!(completions.common, "/hello");

        composer.insert(" now");
        assert!(composer.completions(&candidates).matches.is_empty());
    }

    #[test]
    fn completion_ignores_plain_text() {
        let candidates: Vec<String> = ["/help", "/hello"].map(str::to_owned).to_vec();
        let mut composer = Composer::new();
        composer.insert("hello");
        assert!(composer.completions(&candidates).matches.is_empty());
        assert!(composer.completions(&[]).matches.is_empty());
    }

    #[test]
    fn completion_lists_every_candidate_for_a_bare_slash() {
        let candidates: Vec<String> = ["/help", "/hello"].map(str::to_owned).to_vec();
        let mut composer = Composer::new();
        composer.insert("/");
        let completions = composer.completions(&candidates);
        assert_eq!(completions.matches, vec!["/help", "/hello"]);
        assert_eq!(completions.common, "/hel");
    }

    #[test]
    fn completion_accepts_candidates_without_a_slash() {
        let candidates: Vec<String> = ["help", "hello"].map(str::to_owned).to_vec();
        let mut composer = Composer::new();
        composer.insert("/he");
        let completions = composer.completions(&candidates);
        assert_eq!(completions.matches, vec!["/help", "/hello"]);
        assert_eq!(completions.common, "/hel");
    }
}
