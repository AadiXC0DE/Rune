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

use crate::width::{grapheme_width, graphemes, str_width};

/// How many editing steps are kept for undo.
pub const HISTORY_LIMIT: usize = 128;

/// How many killed texts are kept for a yank.
pub const KILL_RING_LIMIT: usize = 8;

/// One row of a draft as it is drawn, in character indexes.
///
/// `end` is exclusive and never includes a line break. `line` is the logical
/// line the row belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VisualRow {
    /// First character of the row.
    pub start: usize,
    /// Character after the last one on the row.
    pub end: usize,
    /// Index of the logical line, counting line breaks.
    pub line: usize,
}

/// Breaks a draft into the rows it is drawn on.
///
/// Row 0 has `width - first_indent` columns and every later row
/// `width - indent`. A logical line breaks after the last space that fits,
/// else before the cluster that does not fit, so a row never splits a
/// grapheme cluster. A line that fills its last row exactly gets one more,
/// empty row, so a caret at its end has a cell to stand on.
#[must_use]
pub fn layout(text: &str, first_indent: usize, indent: usize, width: usize) -> Vec<VisualRow> {
    let room_for = |row: usize| {
        width
            .saturating_sub(if row == 0 { first_indent } else { indent })
            .max(1)
    };
    let mut rows = Vec::new();
    let mut at = 0usize;
    for (line, part) in text.split('\n').enumerate() {
        let mut start = at;
        let mut pos = at;
        let mut col = 0usize;
        // The character after the last space on the row, and the columns after it.
        let mut space: Option<usize> = None;
        let mut after_space = 0usize;
        for grapheme in graphemes(part) {
            let cells = cell_width(grapheme);
            while col.saturating_add(cells) > room_for(rows.len()) && pos > start {
                if let Some(brk) = space.take().filter(|brk| *brk > start) {
                    rows.push(VisualRow {
                        start,
                        end: brk,
                        line,
                    });
                    start = brk;
                    col = after_space;
                } else {
                    rows.push(VisualRow {
                        start,
                        end: pos,
                        line,
                    });
                    start = pos;
                    col = 0;
                }
                after_space = col;
            }
            col = col.saturating_add(cells);
            pos = pos.saturating_add(grapheme.chars().count());
            if grapheme.chars().all(char::is_whitespace) {
                space = Some(pos);
                after_space = 0;
            } else {
                after_space = after_space.saturating_add(cells);
            }
        }
        let full = col > 0 && col >= room_for(rows.len());
        rows.push(VisualRow {
            start,
            end: pos,
            line,
        });
        if full {
            rows.push(VisualRow {
                start: pos,
                end: pos,
                line,
            });
        }
        at = pos.saturating_add(1);
    }
    rows
}

/// Returns the row holding a caret and the columns before it on that row.
///
/// A caret at the end of a wrapped row stands at the start of the next one.
#[must_use]
pub fn caret_in(text: &str, rows: &[VisualRow], caret: usize) -> (usize, usize) {
    let row = rows
        .iter()
        .rposition(|row| row.start <= caret && caret <= row.end)
        .unwrap_or(0);
    let start = rows.get(row).map_or(0, |row| row.start);
    (row, span_width(text, start, caret))
}

/// Returns the columns a cluster takes in a draft row; a tab takes one.
fn cell_width(grapheme: &str) -> usize {
    if grapheme == "\t" {
        1
    } else {
        grapheme_width(grapheme)
    }
}

/// Returns the text between two character indexes.
fn char_slice(text: &str, start: usize, end: usize) -> &str {
    let byte = |chars: usize| {
        text.char_indices()
            .nth(chars)
            .map_or(text.len(), |(byte, _)| byte)
    };
    text.get(byte(start)..byte(end.max(start)))
        .unwrap_or_default()
}

/// Returns the columns between two character indexes of a draft row.
fn span_width(text: &str, start: usize, end: usize) -> usize {
    graphemes(char_slice(text, start, end))
        .map(cell_width)
        .fold(0usize, usize::saturating_add)
}

/// Returns the caret position on `row` at or before `column`.
///
/// The end of a wrapped row is the start of the next row, so a caret placed
/// on a wrapped row stops before its last cluster.
fn position_in(text: &str, rows: &[VisualRow], row: usize, column: usize) -> usize {
    let Some(target) = rows.get(row) else {
        return 0;
    };
    let wrapped = rows
        .get(row.saturating_add(1))
        .is_some_and(|next| next.line == target.line);
    let clusters: Vec<&str> = graphemes(char_slice(text, target.start, target.end)).collect();
    let usable = if wrapped {
        clusters.len().saturating_sub(1)
    } else {
        clusters.len()
    };
    let mut at = target.start;
    let mut used = 0usize;
    for cluster in clusters.iter().take(usable) {
        let next = used.saturating_add(cell_width(cluster));
        if next > column {
            break;
        }
        used = next;
        at = at.saturating_add(cluster.chars().count());
    }
    at
}

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
    kill_ring: VecDeque<String>,
    last_yank: Option<(usize, usize, usize)>,
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
    /// This differs from the character count: a wide character advances two
    /// columns, and a line break or tab counts as one. Multiline renderers
    /// resolve this prefix width into a row and a column within that row.
    #[must_use]
    pub fn cursor_column(&self) -> usize {
        str_width(&displayed(self.head()))
    }

    /// Returns the visual row of the caret and its column inside that row.
    #[must_use]
    pub fn caret_row_col(
        &self,
        first_indent: usize,
        indent: usize,
        width: usize,
    ) -> (usize, usize) {
        let rows = layout(&self.text, first_indent, indent, width);
        caret_in(&self.text, &rows, self.cursor)
    }

    /// Moves the caret to the previous visual row, keeping its column.
    ///
    /// Returns false, without moving, when the caret is on the first row.
    pub fn move_up(&mut self, first_indent: usize, indent: usize, width: usize) -> bool {
        self.move_vertically(first_indent, indent, width, false)
    }

    /// Moves the caret to the next visual row, keeping its column.
    ///
    /// Returns false, without moving, when the caret is on the last row.
    pub fn move_down(&mut self, first_indent: usize, indent: usize, width: usize) -> bool {
        self.move_vertically(first_indent, indent, width, true)
    }

    /// Moves the caret to the start of its logical line.
    pub fn move_line_start(&mut self) -> bool {
        self.last_yank = None;
        self.jump(self.line_start())
    }

    /// Moves the caret to the end of its logical line.
    pub fn move_line_end(&mut self) -> bool {
        self.last_yank = None;
        self.jump(self.line_end())
    }

    /// Inserts text at the cursor.
    pub fn insert(&mut self, text: &str) {
        self.last_yank = None;
        if text.is_empty() {
            return;
        }
        self.record();
        let at = self.byte_of(self.cursor);
        self.text.insert_str(at, text);
        self.cursor = self.cursor.saturating_add(text.chars().count());
        self.settle();
    }

    /// Replaces a byte range as one edit and puts the caret after the replacement.
    /// Invalid ranges leave the draft unchanged.
    pub fn replace_range(&mut self, range: std::ops::Range<usize>, replacement: &str) {
        if range.start > range.end
            || !self.text.is_char_boundary(range.start)
            || !self.text.is_char_boundary(range.end)
        {
            return;
        }
        let caret = self.text[..range.start]
            .chars()
            .count()
            .saturating_add(replacement.chars().count());
        self.record();
        self.text.replace_range(range, replacement);
        self.cursor = caret;
        self.recall = None;
        self.draft.clear();
        self.settle();
    }

    /// Replaces a backslash just before the caret with a line break, as one
    /// edit. Returns whether there was one.
    pub fn newline_after_backslash(&mut self) -> bool {
        let end = self.byte_of(self.cursor);
        let Some(start) = end
            .checked_sub(1)
            .filter(|start| self.text.get(*start..end) == Some("\\"))
        else {
            return false;
        };
        self.last_yank = None;
        self.replace_range(start..end, "\n");
        true
    }

    /// Empties the line.
    ///
    /// Recall starts over as well. A line is cleared when it is submitted or
    /// abandoned, and a walk that carried on from the old position would skip
    /// the newest entry and bring back a draft that was already let go.
    pub fn clear(&mut self) {
        self.last_yank = None;
        self.recall = None;
        self.draft.clear();
        if self.text.is_empty() {
            return;
        }
        self.record();
        self.text.clear();
        self.cursor = 0;
    }

    /// Removes the cluster before the cursor.
    pub fn delete_back(&mut self) -> bool {
        self.last_yank = None;
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
        self.last_yank = None;
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
        self.last_yank = None;
        let at = self.floor(self.cursor.saturating_sub(1));
        let moved = at != self.cursor;
        self.cursor = at;
        moved
    }

    /// Moves the cursor one cluster right.
    pub fn move_right(&mut self) -> bool {
        self.last_yank = None;
        let at = self.ceil(self.cursor);
        let moved = at != self.cursor;
        self.cursor = at;
        moved
    }

    /// Kills the word before the cursor, and the space before that.
    ///
    /// Trailing whitespace goes first, so the word being removed is the one the
    /// user is looking at rather than the gap after it.
    pub fn kill_word_back(&mut self) -> bool {
        let start = self.word_start();
        self.kill_range(start, self.cursor)
    }

    /// Kills the word after the cursor and the space before it.
    pub fn kill_word_forward(&mut self) -> bool {
        let end = self.floor(self.cursor.saturating_add(self.word_after()));
        self.kill_range(self.cursor, end)
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
        self.jump(self.cursor.saturating_add(self.word_after()))
    }

    /// Moves the cursor to the start of the line.
    pub fn move_home(&mut self) -> bool {
        self.jump(0)
    }

    /// Moves the cursor to the end of the line.
    pub fn move_end(&mut self) -> bool {
        self.jump(self.text.chars().count())
    }

    /// Kills from the cursor to the end of its logical line.
    ///
    /// At the end of a line the line break goes instead, joining the next
    /// line to this one.
    pub fn kill_to_end(&mut self) -> bool {
        let end = self.line_end();
        let end = if end == self.cursor {
            let total = self.text.chars().count();
            end.saturating_add(1).min(total)
        } else {
            end
        };
        self.kill_range(self.cursor, end)
    }

    /// Kills from the start of the cursor's logical line to the cursor.
    ///
    /// At the start of a line the line break before it goes instead.
    pub fn kill_to_start(&mut self) -> bool {
        let start = self.line_start();
        let start = if start == self.cursor {
            start.saturating_sub(1)
        } else {
            start
        };
        self.kill_range(start, self.cursor)
    }

    /// Inserts the newest killed text at the cursor.
    pub fn yank(&mut self) -> bool {
        let Some(killed) = self.kill_ring.back().cloned() else {
            return false;
        };
        let start = self.cursor;
        self.insert(&killed);
        let index = self.kill_ring.len().saturating_sub(1);
        self.last_yank = Some((start, self.cursor, index));
        true
    }

    /// Replaces the text just yanked with the next older kill, as one edit.
    ///
    /// Works only straight after a yank or another yank pop; past the oldest
    /// kill it wraps to the newest.
    pub fn yank_pop(&mut self) -> bool {
        let Some((start, end, index)) = self.last_yank else {
            return false;
        };
        let older = index
            .checked_sub(1)
            .unwrap_or_else(|| self.kill_ring.len().saturating_sub(1));
        let Some(killed) = self.kill_ring.get(older).cloned() else {
            self.last_yank = None;
            return false;
        };
        self.record();
        let from = self.byte_of(start);
        let to = self.byte_of(end);
        self.text.replace_range(from..to, &killed);
        self.cursor = start.saturating_add(killed.chars().count());
        self.last_yank = Some((start, self.cursor, older));
        true
    }

    /// Reverts the last edit.
    pub fn undo(&mut self) -> bool {
        self.last_yank = None;
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
        self.last_yank = None;
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

    /// Replaces the line with `text`, placing the cursor after it.
    ///
    /// Recall state is dropped, because text put on the line by a caller is
    /// not part of any history walk and stepping from it would be meaningless.
    pub fn set(&mut self, text: &str) {
        self.show(text);
        self.recall = None;
        self.draft.clear();
    }

    /// Replaces an externally edited draft as one undoable edit.
    /// An unchanged draft retains its caret and editing history.
    pub fn replace_draft(&mut self, text: &str) {
        if text == self.text {
            return;
        }
        self.record();
        text.clone_into(&mut self.text);
        self.cursor = self.text.chars().count();
        self.recall = None;
        self.draft.clear();
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
        self.last_yank = None;
        text.clone_into(&mut self.text);
        self.undo.clear();
        self.redo.clear();
        self.cursor = self.text.chars().count();
    }

    /// Moves the cursor to `at`, keeping it on a cluster boundary.
    fn jump(&mut self, at: usize) -> bool {
        self.last_yank = None;
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
        self.last_yank = None;
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

    /// Returns the index where the word before the cursor begins.
    ///
    /// Whitespace immediately before the cursor is skipped, matching what a
    /// reader sees as the word to remove.
    fn word_start(&self) -> usize {
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
        self.floor(self.cursor.saturating_sub(count))
    }

    /// Removes a character range as one edit, keeping it in the kill ring.
    fn kill_range(&mut self, start: usize, end: usize) -> bool {
        if start >= end {
            self.last_yank = None;
            return false;
        }
        self.record();
        let from = self.byte_of(start);
        let to = self.byte_of(end);
        let killed = self.text.get(from..to).unwrap_or_default().to_owned();
        self.text.replace_range(from..to, "");
        self.cursor = start;
        self.kill_ring.push_back(killed);
        while self.kill_ring.len() > KILL_RING_LIMIT {
            self.kill_ring.pop_front();
        }
        true
    }

    /// Returns the characters from the cursor to the end of the next word.
    fn word_after(&self) -> usize {
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
        count
    }

    /// Returns the character index where the cursor's logical line starts.
    fn line_start(&self) -> usize {
        self.head()
            .chars()
            .rev()
            .position(|c| c == '\n')
            .map_or(0, |back| self.cursor.saturating_sub(back))
    }

    /// Returns the character index where the cursor's logical line ends.
    fn line_end(&self) -> usize {
        let tail = self.tail();
        let ahead = tail
            .chars()
            .position(|c| c == '\n')
            .unwrap_or_else(|| tail.chars().count());
        self.cursor.saturating_add(ahead)
    }

    /// Moves the caret one visual row, keeping its screen column.
    fn move_vertically(
        &mut self,
        first_indent: usize,
        indent: usize,
        width: usize,
        down: bool,
    ) -> bool {
        self.last_yank = None;
        let rows = layout(&self.text, first_indent, indent, width);
        let (row, column) = caret_in(&self.text, &rows, self.cursor);
        let target = if down {
            row.saturating_add(1)
        } else {
            let Some(above) = row.checked_sub(1) else {
                return false;
            };
            above
        };
        if target >= rows.len() {
            return false;
        }
        let indent_of = |row: usize| if row == 0 { first_indent } else { indent };
        let screen = column.saturating_add(indent_of(row));
        let column = screen.saturating_sub(indent_of(target));
        self.cursor = position_in(&self.text, &rows, target, column);
        true
    }

    /// Keeps the cursor inside the line and off the interior of a cluster.
    fn settle(&mut self) {
        let total = self.text.chars().count();
        self.cursor = self.floor(self.cursor.min(total));
    }
}

/// Drawn in place of a line break in a single-row prompt echo.
pub const LINE_BREAK: char = '\u{23ce}';

/// Returns the text for a single-row echo or prefix width measurement.
///
/// A pasted line break or tab is part of the text, but written to the terminal
/// as it is it would move the cursor to another row or column. Each is drawn as
/// one visible column instead, so the row stays one row and a caret column
/// measured from it is the column the terminal uses.
#[must_use]
pub fn displayed(text: &str) -> std::borrow::Cow<'_, str> {
    displayed_with_ascii(text, false)
}

/// Returns a single-row echo, using `/` for newlines in ASCII mode.
/// Source Unicode is preserved, including characters matching a decoration.
#[must_use]
pub fn displayed_with_ascii(text: &str, ascii: bool) -> std::borrow::Cow<'_, str> {
    if !text.contains(['\n', '\t']) {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(
        text.chars()
            .map(|c| match c {
                '\n' => {
                    if ascii {
                        '/'
                    } else {
                        LINE_BREAK
                    }
                }
                '\t' => ' ',
                c => c,
            })
            .collect(),
    )
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

    /// Returns the rows of a layout as `(start, end)` pairs.
    fn spans(text: &str, first_indent: usize, indent: usize, width: usize) -> Vec<(usize, usize)> {
        layout(text, first_indent, indent, width)
            .iter()
            .map(|row| (row.start, row.end))
            .collect()
    }

    /// Returns a composer holding `text` with the caret at character `at`.
    fn at(text: &str, at: usize) -> Composer {
        let mut composer = Composer::new();
        composer.insert(text);
        composer.move_home();
        for _ in 0..at {
            composer.move_right();
        }
        composer
    }

    #[test]
    fn layout_wraps_at_spaces_and_never_splits_graphemes() {
        assert_eq!(spans("hello world", 0, 0, 8), vec![(0, 6), (6, 11)]);
        // A wide character moves whole to the next row, and a letter keeps
        // its combining accent.
        assert_eq!(
            spans("ab\u{6f22}e\u{301}c", 0, 0, 3),
            vec![(0, 2), (2, 5), (5, 6)]
        );
        // Row 0 leaves room for the first indent; a full last row gets an
        // empty one for the caret.
        assert_eq!(spans("abcdef", 2, 0, 4), vec![(0, 2), (2, 6), (6, 6)]);
        let rows = layout("a\n\nb", 2, 2, 80);
        assert_eq!(
            rows,
            vec![
                VisualRow {
                    start: 0,
                    end: 1,
                    line: 0
                },
                VisualRow {
                    start: 2,
                    end: 2,
                    line: 1
                },
                VisualRow {
                    start: 3,
                    end: 4,
                    line: 2
                },
            ]
        );
        assert_eq!(
            spans(&"x".repeat(100), 2, 2, 40),
            vec![(0, 38), (38, 76), (76, 100)]
        );
    }

    #[test]
    fn up_and_down_keep_the_display_column_across_wide_characters() {
        let text = "ab\n\u{6f22}\u{5b57}x\nlonger line";
        let mut composer = at(text, 1);
        assert_eq!(composer.caret_row_col(2, 2, 80), (0, 1));
        assert!(composer.move_down(2, 2, 80));
        // Column 1 is inside the first wide character: the caret stays before it.
        assert_eq!(composer.cursor(), 3);
        assert_eq!(composer.caret_row_col(2, 2, 80), (1, 0));

        let mut composer = at(text, 2);
        assert!(composer.move_down(2, 2, 80));
        assert_eq!(composer.cursor(), 4);
        assert_eq!(composer.caret_row_col(2, 2, 80), (1, 2));
        assert!(composer.move_down(2, 2, 80));
        assert_eq!(composer.cursor(), 9);
        assert_eq!(composer.caret_row_col(2, 2, 80), (2, 2));

        let mut composer = at(text, 10);
        assert!(composer.move_up(2, 2, 80));
        assert_eq!(
            composer.cursor(),
            4,
            "column 3 falls inside the second wide character"
        );
        assert!(composer.move_up(2, 2, 80));
        assert_eq!(composer.cursor(), 2, "the short row clamps to its end");
        assert_eq!(composer.text(), text);
    }

    #[test]
    fn vertical_motion_reports_the_edges() {
        let text = "x".repeat(100);
        let mut composer = Composer::new();
        composer.insert(&text);
        assert_eq!(composer.caret_row_col(2, 2, 40), (2, 24));
        assert!(!composer.move_down(2, 2, 40));
        assert!(composer.move_up(2, 2, 40));
        assert_eq!(composer.caret_row_col(2, 2, 40), (1, 24));
        assert!(composer.move_up(2, 2, 40));
        assert_eq!(composer.caret_row_col(2, 2, 40), (0, 24));
        assert!(!composer.move_up(2, 2, 40));
        assert_eq!(composer.cursor(), 24);
        assert!(composer.move_down(2, 2, 40));
        assert_eq!(composer.cursor(), 62);
        assert_eq!(composer.text(), text);

        let mut single = at("abc", 1);
        assert!(!single.move_up(2, 2, 80));
        assert!(!single.move_down(2, 2, 80));
        assert_eq!(single.cursor(), 1);
    }

    #[test]
    fn home_then_home_goes_to_the_draft_start() {
        let mut composer = Composer::new();
        composer.insert("one\ntwo");
        assert!(composer.move_line_start());
        assert_eq!(composer.cursor(), 4);
        assert!(!composer.move_line_start());
        assert!(composer.move_home());
        assert_eq!(composer.cursor(), 0);

        assert!(composer.move_line_end());
        assert_eq!(composer.cursor(), 3);
        assert!(!composer.move_line_end());
        assert!(composer.move_end());
        assert_eq!(composer.cursor(), 7);
    }

    #[test]
    fn ctrl_k_at_a_line_end_joins_the_lines() {
        let mut composer = at("one\ntwo", 3);
        assert!(composer.kill_to_end());
        assert_eq!(composer.text(), "onetwo");
        assert!(composer.kill_to_end());
        assert_eq!(composer.text(), "one");
        assert!(
            !composer.kill_to_end(),
            "the end of the draft kills nothing"
        );
        assert!(composer.yank());
        assert_eq!(composer.text(), "onetwo");

        let mut composer = at("one\ntwo three", 8);
        assert!(composer.kill_to_start());
        assert_eq!(composer.text(), "one\nthree");
        assert_eq!(composer.cursor(), 4);
        assert!(composer.kill_to_start());
        assert_eq!(composer.text(), "onethree");
        assert_eq!(composer.cursor(), 3);
    }

    #[test]
    fn kill_ring_keeps_eight_and_cycles() {
        let mut composer = Composer::new();
        for index in 0..9 {
            composer.insert(&format!("k{index}"));
            assert!(composer.kill_to_start());
        }
        assert!(composer.yank());
        assert_eq!(composer.text(), "k8");
        for index in (1..8).rev() {
            assert!(composer.yank_pop());
            assert_eq!(composer.text(), format!("k{index}"));
        }
        assert!(composer.yank_pop());
        assert_eq!(
            composer.text(),
            "k8",
            "the oldest kept entry wraps to the newest"
        );
        assert_eq!(composer.cursor(), 2);

        // Undo walks back through the pops one at a time, then removes the yank.
        let mut composer = Composer::new();
        composer.insert("first");
        composer.kill_to_start();
        composer.insert("second");
        composer.kill_to_start();
        composer.insert("> ");
        assert!(composer.yank());
        assert!(composer.yank_pop());
        assert_eq!(composer.text(), "> first");
        assert!(composer.undo());
        assert_eq!(composer.text(), "> second");
        assert!(composer.undo());
        assert_eq!(composer.text(), "> ");
    }

    #[test]
    fn yank_pop_needs_a_preceding_yank() {
        let mut composer = Composer::new();
        assert!(!composer.yank_pop());
        composer.insert("one two");
        assert!(composer.kill_word_back());
        assert!(composer.kill_word_back());
        assert!(!composer.yank_pop());
        assert!(composer.yank());
        assert_eq!(composer.text(), "one ");
        composer.move_left();
        assert!(!composer.yank_pop(), "a motion ends the yank");
        composer.move_right();
        assert!(composer.yank());
        composer.insert("z");
        assert!(!composer.yank_pop(), "an edit ends the yank");
        assert_eq!(composer.text(), "one one z");
    }

    #[test]
    fn alt_d_kills_the_next_word() {
        let mut composer = at("alpha beta gamma", 5);
        assert!(composer.kill_word_forward());
        assert_eq!(composer.text(), "alpha gamma");
        assert_eq!(composer.cursor(), 5);
        assert!(composer.yank());
        assert_eq!(composer.text(), "alpha beta gamma");
        composer.move_end();
        assert!(!composer.kill_word_forward());
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
    fn newline_after_backslash_is_one_undo_step() {
        let mut composer = Composer::new();
        composer.insert("one \\");
        assert!(composer.newline_after_backslash());
        assert_eq!(composer.text(), "one \n");
        assert_eq!(composer.cursor(), 5);
        assert!(composer.undo());
        assert_eq!(composer.text(), "one \\");
        assert_eq!(composer.cursor(), 5);

        // Only the backslash right before the caret counts.
        let mut composer = at("a\\b", 3);
        assert!(!composer.newline_after_backslash());
        assert_eq!(composer.text(), "a\\b");
        let mut composer = at("a\\b", 2);
        assert!(composer.newline_after_backslash());
        assert_eq!(composer.text(), "a\nb");

        let mut composer = Composer::new();
        composer.insert("\\\\");
        assert!(composer.newline_after_backslash());
        assert_eq!(composer.text(), "\\\n");
        assert!(!Composer::new().newline_after_backslash());
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
    fn clearing_the_line_starts_recall_over() {
        // Sending a recalled entry clears the line, and recall starts over from
        // there. Carrying on from the old position skips an entry on the next
        // Up, and a Down past the newest brings back a draft that was let go.
        let entries: Vec<String> = ["one", "two", "three"].map(str::to_owned).to_vec();
        let mut composer = Composer::new();
        composer.insert("abandoned draft");
        assert!(composer.history_previous(&entries));
        assert!(composer.history_previous(&entries));
        assert_eq!(composer.text(), "two");

        composer.clear();
        assert!(composer.history_previous(&entries));
        assert_eq!(composer.text(), "three", "the newest entry was skipped");
        assert!(composer.history_next(&entries));
        assert_eq!(composer.text(), "", "an abandoned draft came back");
        assert!(!composer.history_next(&entries));
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
