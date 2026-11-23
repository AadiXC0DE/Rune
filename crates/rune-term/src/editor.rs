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
use std::sync::Arc;

use crate::width::{grapheme_width, graphemes, str_width};

/// How many editing steps are kept for undo.
pub const HISTORY_LIMIT: usize = 128;

/// How many killed texts are kept for a yank.
pub const KILL_RING_LIMIT: usize = 8;

/// A paste longer than this many characters becomes a chip.
pub const CHIP_CHARS: usize = 800;

/// A paste with more lines than this becomes a chip.
pub const CHIP_LINES: usize = 3;

/// A paste held out of the drawn draft, standing behind its marker.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Chip {
    /// Number shown in the marker, unique within the draft.
    pub id: u32,
    /// Text drawn in the draft in place of the paste.
    pub marker: String,
    /// The pasted text the marker stands for.
    pub text: String,
    /// What the chip holds.
    pub kind: ChipKind,
}

/// What a chip stands for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ChipKind {
    /// Pasted text.
    Paste,
    /// An image file attached to the message.
    Image {
        /// The file.
        path: std::path::PathBuf,
        /// Its size.
        bytes: u64,
    },
}

/// The size an image marker shows: `420 KB`, or `1.2 MB` from one MiB.
#[must_use]
pub fn image_size(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        let tenths = bytes.saturating_mul(10) / MIB;
        format!("{}.{} MB", tenths / 10, tenths % 10)
    } else {
        format!("{} KB", bytes / 1024)
    }
}

/// Returns the byte ranges of text shaped like a chip marker:
/// `[pasted #N: N lines]` or `[pasted #N: N characters]`.
#[must_use]
pub fn marker_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut spans = paste_marker_spans(text);
    spans.extend(image_marker_spans(text));
    spans.sort_by_key(|span| span.start);
    spans
}

/// The spans of `[image #N: name size]` markers.
fn image_marker_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    const OPEN: &str = "[image #";
    let mut spans = Vec::new();
    let mut from = 0usize;
    while let Some(found) = text.get(from..).and_then(|rest| rest.find(OPEN)) {
        let start = from.saturating_add(found);
        let rest = text
            .get(start.saturating_add(OPEN.len())..)
            .unwrap_or_default();
        let id = rest.bytes().take_while(u8::is_ascii_digit).count();
        let close = rest.find(']');
        match close {
            Some(close)
                if id > 0 && rest.get(id..).is_some_and(|after| after.starts_with(": ")) =>
            {
                let end = start
                    .saturating_add(OPEN.len())
                    .saturating_add(close)
                    .saturating_add(1);
                spans.push(start..end);
                from = end;
            }
            _ => from = start.saturating_add(1),
        }
    }
    spans
}

/// The spans of `[pasted #N: ...]` markers.
fn paste_marker_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    const OPEN: &str = "[pasted #";
    let digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
    let mut spans = Vec::new();
    let mut from = 0usize;
    while let Some(found) = text.get(from..).and_then(|rest| rest.find(OPEN)) {
        let start = from.saturating_add(found);
        let rest = text
            .get(start.saturating_add(OPEN.len())..)
            .unwrap_or_default();
        let id = digits(rest);
        let after = rest.get(id..).unwrap_or_default();
        let count = after.strip_prefix(": ").map_or(0, digits);
        let tail = after
            .get(2usize.saturating_add(count)..)
            .unwrap_or_default();
        let close = [" lines]", " characters]"]
            .iter()
            .find(|close| tail.starts_with(**close));
        match close {
            Some(close) if id > 0 && count > 0 => {
                let end = start
                    .saturating_add(OPEN.len())
                    .saturating_add(id)
                    .saturating_add(2)
                    .saturating_add(count)
                    .saturating_add(close.len());
                spans.push(start..end);
                from = end;
            }
            _ => from = start.saturating_add(1),
        }
    }
    spans
}

/// Returns the lines a paste holds; a final line break does not start one.
fn line_count(text: &str) -> usize {
    text.lines().count().max(1)
}

/// Returns whether a paste is large enough to become a chip.
#[must_use]
pub fn exceeds_chip_thresholds(text: &str) -> bool {
    text.chars().count() > CHIP_CHARS || line_count(text) > CHIP_LINES
}

/// Splits one logical line into the units a row break may fall between:
/// grapheme clusters, except that a chip marker stays whole when it fits
/// on a row of `room` columns.
fn units(part: &str, room: usize) -> Vec<&str> {
    let spans = marker_spans(part);
    let mut out = Vec::new();
    let mut byte = 0usize;
    let mut spans = spans.iter().peekable();
    for grapheme in graphemes(part) {
        let end = byte.saturating_add(grapheme.len());
        while spans.peek().is_some_and(|span| span.end <= byte) {
            spans.next();
        }
        let inside = spans
            .peek()
            .filter(|span| span.start <= byte && byte < span.end)
            .map(|span| (*span).clone());
        match inside {
            Some(span) if span.start == byte => {
                let marker = part.get(span.clone()).unwrap_or_default();
                if str_width(marker) <= room {
                    out.push(marker);
                } else {
                    out.push(grapheme);
                }
            }
            Some(span)
                if part
                    .get(span.clone())
                    .is_some_and(|marker| str_width(marker) <= room) => {}
            _ => out.push(grapheme),
        }
        byte = end;
    }
    out
}

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
        for grapheme in units(part, room_for(1).min(room_for(0))) {
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

/// Returns the columns a unit takes in a draft row; a tab takes one.
fn cell_width(grapheme: &str) -> usize {
    if grapheme == "\t" {
        1
    } else if grapheme.chars().nth(1).is_some() && grapheme.starts_with('[') {
        str_width(grapheme)
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
    chips: Arc<Vec<Chip>>,
    next_chip: u32,
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
///
/// A large paste is held as a [`Chip`]: the draft shows its marker, the caret
/// and row breaks never land inside the marker, and [`Composer::expanded`]
/// puts the pasted text back. Chips are part of every undo state, so undoing
/// the removal of a marker brings its text back with it.
#[derive(Clone, Debug)]
pub struct Composer {
    text: String,
    cursor: usize,
    undo: VecDeque<Snapshot>,
    redo: VecDeque<Snapshot>,
    kill_ring: VecDeque<String>,
    last_yank: Option<(usize, usize, usize)>,
    recall: Option<usize>,
    draft: String,
    chips: Arc<Vec<Chip>>,
    next_chip: u32,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            kill_ring: VecDeque::new(),
            last_yank: None,
            recall: None,
            draft: String::new(),
            chips: Arc::default(),
            next_chip: 1,
        }
    }
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
        self.prune();
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
        if !self.text.is_empty() {
            self.record();
            self.text.clear();
            self.cursor = 0;
            self.prune();
        }
        if self.chips.is_empty() {
            self.next_chip = 1;
        }
    }

    /// Removes the cluster before the cursor, or the whole chip marker it
    /// ends.
    pub fn delete_back(&mut self) -> bool {
        self.last_yank = None;
        if let Some((start, end)) = self.chip_at(self.cursor) {
            return self.remove_chars(start, end);
        }
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

    /// Removes the cluster at the cursor, or the whole chip marker it starts.
    pub fn delete_forward(&mut self) -> bool {
        self.last_yank = None;
        if let Some((start, end)) = self.chip_at(self.cursor.saturating_add(1)) {
            return self.remove_chars(start, end);
        }
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

    /// Moves the cursor one cluster left, or over a whole chip marker.
    pub fn move_left(&mut self) -> bool {
        self.last_yank = None;
        let at = self.snap(self.floor(self.cursor.saturating_sub(1)), false);
        let moved = at != self.cursor;
        self.cursor = at;
        moved
    }

    /// Moves the cursor one cluster right, or over a whole chip marker.
    pub fn move_right(&mut self) -> bool {
        self.last_yank = None;
        let at = self.snap(self.ceil(self.cursor), true);
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
        self.restore(previous);
        true
    }

    /// Reapplies the last reverted edit.
    pub fn redo(&mut self) -> bool {
        self.last_yank = None;
        let Some(next) = self.redo.pop_back() else {
            return false;
        };
        self.push_undo(self.snapshot());
        self.restore(next);
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

    /// Inserts pasted text at the cursor, or a chip standing for it when it
    /// holds more than [`CHIP_CHARS`] characters or [`CHIP_LINES`] lines.
    pub fn paste_text(&mut self, text: &str) -> Paste {
        let chars = text.chars().count();
        if !exceeds_chip_thresholds(text) {
            self.insert(text);
            return Paste::Inserted { chars };
        }
        let marker = self.next_marker(text);
        self.insert(&marker);
        self.add_chip(&marker, text);
        Paste::Placeholder { marker, chars }
    }

    /// Turns a character range of the draft into a chip, as one edit.
    /// Returns false, changing nothing, for an empty or invalid range.
    pub fn collapse_into_chip(&mut self, start: usize, end: usize) -> bool {
        let total = self.text.chars().count();
        if start >= end || end > total {
            return false;
        }
        let text = char_slice(&self.text, start, end).to_owned();
        let marker = self.next_marker(&text);
        self.record();
        let from = self.byte_of(start);
        let to = self.byte_of(end);
        self.text.replace_range(from..to, &marker);
        self.cursor = start.saturating_add(marker.chars().count());
        self.add_chip(&marker, &text);
        self.prune();
        true
    }

    /// Returns the chips whose markers are in the draft.
    #[must_use]
    pub fn chips(&self) -> &[Chip] {
        &self.chips
    }

    /// Puts an image chip at the cursor, `[image #{n}: {name} {size}]`, `n`
    /// counting this draft's images from 1, with a space before it when the
    /// cursor follows other text. Returns the marker.
    pub fn attach_image(&mut self, path: std::path::PathBuf, bytes: u64) -> String {
        let number = self
            .image_chips()
            .iter()
            .filter_map(|chip| {
                chip.marker
                    .strip_prefix("[image #")?
                    .split(':')
                    .next()?
                    .parse::<u32>()
                    .ok()
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let marker = format!("[image #{number}: {name} {}]", image_size(bytes));
        let before = char_slice(&self.text, 0, self.cursor).chars().last();
        let spaced = if before.is_some_and(|ch| !ch.is_whitespace()) {
            format!(" {marker}")
        } else {
            marker.clone()
        };
        self.insert(&spaced);
        Arc::make_mut(&mut self.chips).push(Chip {
            id: self.next_chip,
            marker: marker.clone(),
            text: format!("[image #{number}]"),
            kind: ChipKind::Image { path, bytes },
        });
        self.next_chip = self.next_chip.saturating_add(1);
        marker
    }

    /// The image chips in the draft, in the order they appear.
    fn image_chips(&self) -> Vec<&Chip> {
        let mut found: Vec<(usize, &Chip)> = self
            .chips
            .iter()
            .filter(|chip| matches!(chip.kind, ChipKind::Image { .. }))
            .filter_map(|chip| self.text.find(&chip.marker).map(|at| (at, chip)))
            .collect();
        found.sort_by_key(|(at, _)| *at);
        found.into_iter().map(|(_, chip)| chip).collect()
    }

    /// The draft's image files, in order, with their sizes and markers.
    #[must_use]
    pub fn images(&self) -> Vec<(std::path::PathBuf, u64, String)> {
        self.image_chips()
            .into_iter()
            .filter_map(|chip| match &chip.kind {
                ChipKind::Image { path, bytes } => {
                    Some((path.clone(), *bytes, chip.marker.clone()))
                }
                ChipKind::Paste => None,
            })
            .collect()
    }

    /// Removes the image chip with this marker from the draft, as one edit.
    /// Returns whether it was there.
    pub fn remove_marker(&mut self, marker: &str) -> bool {
        let Some(at) = self.text.find(marker) else {
            return false;
        };
        self.record();
        let mut end = at.saturating_add(marker.len());
        // A space put before the marker goes with it; else one after it.
        let begin = if at > 0 && self.text.get(at.saturating_sub(1)..at) == Some(" ") {
            at.saturating_sub(1)
        } else {
            if self.text.get(end..end.saturating_add(1)) == Some(" ") {
                end = end.saturating_add(1);
            }
            at
        };
        let first = self
            .text
            .get(..begin)
            .map_or(0, |head| head.chars().count());
        let removed = self
            .text
            .get(begin..end)
            .map_or(0, |gone| gone.chars().count());
        self.text.replace_range(begin..end, "");
        self.cursor = if self.cursor <= first {
            self.cursor
        } else if self.cursor >= first.saturating_add(removed) {
            self.cursor.saturating_sub(removed)
        } else {
            first
        };
        self.prune();
        true
    }

    /// Returns the character range of the chip marker that contains `at` or
    /// ends at it.
    #[must_use]
    pub fn chip_at(&self, at: usize) -> Option<(usize, usize)> {
        self.marker_ranges()
            .into_iter()
            .find(|(start, end, _)| *start < at && at <= *end)
            .map(|(start, end, _)| (start, end))
    }

    /// Returns whether the caret is inside a chip marker or at either edge.
    #[must_use]
    pub fn caret_on_chip(&self) -> bool {
        self.chip_touching().is_some()
    }

    /// Returns the draft with every chip marker replaced by its text.
    #[must_use]
    pub fn expanded(&self) -> String {
        if self.chips.is_empty() {
            return self.text.clone();
        }
        let mut ranges = self.marker_bytes();
        ranges.sort_by_key(|(range, _)| range.start);
        let mut out = String::with_capacity(self.text.len());
        let mut at = 0usize;
        for (range, chip) in ranges {
            if range.start < at {
                continue;
            }
            out.push_str(self.text.get(at..range.start).unwrap_or_default());
            match &chip.kind {
                ChipKind::Paste => out.push_str(&chip.text),
                // The image travels as its own part; the text names it.
                ChipKind::Image { .. } => out.push_str(&chip.text),
            }
            at = range.end;
        }
        out.push_str(self.text.get(at..).unwrap_or_default());
        out
    }

    /// Replaces the chip marker the caret touches with its text, as one
    /// edit, leaving the caret after the text. Returns whether there was one.
    pub fn expand_chip_at_caret(&mut self) -> bool {
        let Some((start, end, index)) = self.chip_touching() else {
            return false;
        };
        let Some(text) = self
            .chips
            .get(index)
            .filter(|chip| chip.kind == ChipKind::Paste)
            .map(|chip| chip.text.clone())
        else {
            return false;
        };
        self.last_yank = None;
        self.record();
        let from = self.byte_of(start);
        let to = self.byte_of(end);
        self.text.replace_range(from..to, &text);
        self.cursor = start.saturating_add(text.chars().count());
        self.prune();
        true
    }

    /// Replaces the line with `text`, placing the cursor after it.
    ///
    /// Recall state is dropped, because text put on the line by a caller is
    /// not part of any history walk and stepping from it would be meaningless.
    pub fn set(&mut self, text: &str) {
        self.show(text);
        self.recall = None;
        self.draft.clear();
        self.prune();
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
        self.prune();
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
        self.prune();
    }

    /// Moves the cursor to `at`, keeping it on a cluster boundary and off
    /// the inside of a chip marker.
    fn jump(&mut self, at: usize) -> bool {
        self.last_yank = None;
        let at = self.snap(self.floor(at), at > self.cursor);
        let moved = at != self.cursor;
        self.cursor = at;
        moved
    }

    /// Returns the state an edit is about to replace.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            chips: Arc::clone(&self.chips),
            next_chip: self.next_chip,
        }
    }

    /// Puts back a state from the undo or redo stack.
    fn restore(&mut self, state: Snapshot) {
        self.text = state.text;
        self.cursor = state.cursor;
        self.chips = state.chips;
        self.next_chip = state.next_chip;
    }

    /// Returns the marker the next chip made from `text` gets.
    fn next_marker(&self, text: &str) -> String {
        let id = self.next_chip;
        let lines = line_count(text);
        if lines > 1 {
            format!("[pasted #{id}: {lines} lines]")
        } else {
            format!("[pasted #{id}: {} characters]", text.chars().count())
        }
    }

    /// Records a chip for a marker just put in the draft.
    fn add_chip(&mut self, marker: &str, text: &str) {
        Arc::make_mut(&mut self.chips).push(Chip {
            id: self.next_chip,
            marker: marker.to_owned(),
            text: text.to_owned(),
            kind: ChipKind::Paste,
        });
        self.next_chip = self.next_chip.saturating_add(1);
    }

    /// Drops the chips whose markers are neither in the draft nor in the
    /// draft parked by a history walk.
    fn prune(&mut self) {
        let live =
            |chip: &Chip| self.text.contains(&chip.marker) || self.draft.contains(&chip.marker);
        if self.chips.iter().all(live) {
            return;
        }
        let kept: Vec<Chip> = self
            .chips
            .iter()
            .filter(|chip| live(chip))
            .cloned()
            .collect();
        self.chips = Arc::new(kept);
    }

    /// Returns the byte range of every chip marker in the draft, with its chip.
    fn marker_bytes(&self) -> Vec<(std::ops::Range<usize>, &Chip)> {
        self.chips
            .iter()
            .flat_map(|chip| {
                self.text
                    .match_indices(chip.marker.as_str())
                    .map(move |(byte, found)| (byte..byte.saturating_add(found.len()), chip))
            })
            .collect()
    }

    /// Returns the character range of every chip marker, with the chip's
    /// index.
    fn marker_ranges(&self) -> Vec<(usize, usize, usize)> {
        self.chips
            .iter()
            .enumerate()
            .flat_map(|(index, chip)| {
                self.text
                    .match_indices(chip.marker.as_str())
                    .map(move |(byte, found)| (byte, byte.saturating_add(found.len()), index))
            })
            .map(|(from, to, index)| {
                let start = self.text.get(..from).map_or(0, |head| head.chars().count());
                let len = self
                    .text
                    .get(from..to)
                    .map_or(0, |marker| marker.chars().count());
                (start, start.saturating_add(len), index)
            })
            .collect()
    }

    /// Returns the marker the caret is inside or at either edge of.
    fn chip_touching(&self) -> Option<(usize, usize, usize)> {
        let at = self.cursor;
        let ranges = self.marker_ranges();
        ranges
            .iter()
            .find(|(start, end, _)| *start < at && at <= *end)
            .or_else(|| ranges.iter().find(|(start, _, _)| *start == at))
            .copied()
    }

    /// Moves an index inside a chip marker to the marker's end when
    /// `forward`, else to its start.
    fn snap(&self, at: usize, forward: bool) -> usize {
        self.marker_ranges()
            .into_iter()
            .find(|(start, end, _)| *start < at && at < *end)
            .map_or(at, |(start, end, _)| if forward { end } else { start })
    }

    /// Removes a character range as one edit, dropping the chips it held.
    fn remove_chars(&mut self, start: usize, end: usize) -> bool {
        if start >= end {
            return false;
        }
        self.record();
        let from = self.byte_of(start);
        let to = self.byte_of(end);
        self.text.replace_range(from..to, "");
        self.cursor = start;
        self.prune();
        true
    }

    /// Returns a slice of the draft with chip markers replaced by their text.
    fn expanded_slice(&self, start: usize, end: usize) -> String {
        let mut out = String::new();
        let mut at = start;
        let mut ranges = self.marker_ranges();
        ranges.sort_by_key(|(start, _, _)| *start);
        for (from, to, index) in ranges {
            if from < at || to > end {
                continue;
            }
            out.push_str(char_slice(&self.text, at, from));
            if let Some(chip) = self.chips.get(index) {
                out.push_str(&chip.text);
            }
            at = to;
        }
        out.push_str(char_slice(&self.text, at, end));
        out
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
        let start = self.snap(start, false);
        let end = self.snap(end, true);
        if start >= end {
            self.last_yank = None;
            return false;
        }
        let killed = self.expanded_slice(start, end);
        self.record();
        let from = self.byte_of(start);
        let to = self.byte_of(end);
        self.text.replace_range(from..to, "");
        self.cursor = start;
        self.prune();
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
        self.cursor = self.snap(position_in(&self.text, &rows, target, column), false);
        true
    }

    /// Keeps the cursor inside the line and off the interior of a cluster.
    fn settle(&mut self) {
        let total = self.text.chars().count();
        self.cursor = self.snap(self.floor(self.cursor.min(total)), false);
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

    #[test]
    fn an_image_chip_expands_to_its_reference() {
        let mut composer = Composer::new();
        composer.insert("look");
        let marker = composer.attach_image(std::path::PathBuf::from("/tmp/shot.png"), 430_080);
        assert_eq!(marker, "[image #1: shot.png 420 KB]");
        assert_eq!(composer.text(), "look [image #1: shot.png 420 KB]");
        assert_eq!(composer.expanded(), "look [image #1]");
        let second = composer.attach_image(std::path::PathBuf::from("/tmp/b.gif"), 1_300_000);
        assert_eq!(second, "[image #2: b.gif 1.2 MB]");
        let listed: Vec<String> = composer
            .images()
            .into_iter()
            .map(|(path, _, _)| path.display().to_string())
            .collect();
        assert_eq!(listed, ["/tmp/shot.png", "/tmp/b.gif"]);
        assert!(composer.remove_marker(&marker));
        assert_eq!(composer.images().len(), 1);
        assert_eq!(composer.expanded(), "look [image #2]");
        // An image chip is not turned back into text.
        assert!(!composer.expand_chip_at_caret());
        let spans = marker_spans("a [image #3: x.png 1 KB] b");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans.first().cloned(), Some(2..24));
    }

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

    /// Returns `count` short lines, each ending in a line break but the last.
    fn lines(count: usize) -> String {
        (0..count)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_paste_over_the_thresholds_becomes_a_chip() {
        let mut composer = Composer::new();
        let exact = "a".repeat(CHIP_CHARS);
        assert_eq!(
            composer.paste_text(&exact),
            Paste::Inserted { chars: CHIP_CHARS }
        );
        assert_eq!(composer.text(), exact);
        composer.clear();

        let long = "b".repeat(CHIP_CHARS + 1);
        let outcome = composer.paste_text(&long);
        assert_eq!(outcome.marker(), Some("[pasted #1: 801 characters]"));
        assert_eq!(composer.text(), "[pasted #1: 801 characters]");
        assert_eq!(composer.expanded(), long);
        composer.clear();

        let three = lines(3);
        composer.paste_text(&three);
        assert_eq!(composer.text(), three);
        composer.clear();

        composer.insert("see ");
        composer.paste_text(&lines(4));
        assert_eq!(composer.text(), "see [pasted #1: 4 lines]");
        assert_eq!(composer.expanded(), format!("see {}", lines(4)));

        // A final line break does not count as a line of its own.
        let mut trailing = Composer::new();
        trailing.paste_text("a\nb\nc\n");
        assert_eq!(trailing.text(), "a\nb\nc\n");
        trailing.clear();
        trailing.paste_text("a\nb\nc\nd\n");
        assert_eq!(trailing.text(), "[pasted #1: 4 lines]");
        assert_eq!(trailing.expanded(), "a\nb\nc\nd\n");
    }

    #[test]
    fn a_chip_deletes_as_one_unit() {
        let mut composer = Composer::new();
        composer.insert("x ");
        composer.paste_text(&lines(5));
        composer.insert(" y");
        let marker = "[pasted #1: 5 lines]";
        assert_eq!(composer.text(), format!("x {marker} y"));

        // Backspace at the end of the marker removes it whole.
        composer.move_left();
        composer.move_left();
        assert!(composer.delete_back());
        assert_eq!(composer.text(), "x  y");
        assert!(composer.chips().is_empty());
        assert_eq!(composer.expanded(), "x  y");

        // One undo brings it back, chip included.
        assert!(composer.undo());
        assert_eq!(composer.text(), format!("x {marker} y"));
        assert_eq!(composer.expanded(), format!("x {} y", lines(5)));

        // Delete at the start of the marker removes it whole too.
        composer.move_home();
        composer.move_right();
        composer.move_right();
        assert_eq!(composer.cursor(), 2);
        assert!(composer.delete_forward());
        assert_eq!(composer.text(), "x  y");
        composer.undo();

        // A kill covering part of the marker takes all of it, and the ring
        // keeps the pasted text, so a yank brings it back as plain text.
        composer.move_end();
        composer.move_left();
        composer.move_left();
        assert!(composer.kill_word_back());
        assert_eq!(composer.text(), "x  y");
        assert!(composer.yank());
        assert_eq!(composer.text(), format!("x {} y", lines(5)));
        assert!(composer.chips().is_empty());
    }

    #[test]
    fn motion_skips_over_a_chip() {
        let mut composer = Composer::new();
        composer.insert("ab");
        composer.paste_text(&lines(4));
        composer.insert("cd");
        let marker_len = "[pasted #1: 4 lines]".chars().count();
        let end = 4 + marker_len;
        assert_eq!(composer.cursor(), end);

        composer.move_left();
        composer.move_left();
        assert_eq!(composer.cursor(), 2 + marker_len);
        assert!(composer.move_left());
        assert_eq!(composer.cursor(), 2);
        assert!(composer.move_right());
        assert_eq!(composer.cursor(), 2 + marker_len);

        // Word motion and vertical motion stop at the marker's edges too.
        composer.move_word_left();
        assert!(composer.cursor() == 2 || composer.cursor() == 0);
        composer.move_word_right();
        assert!(
            composer
                .chip_at(composer.cursor())
                .is_none_or(|(_, e)| e == composer.cursor())
        );

        // Rows break around the marker, never inside it, wherever it fits on
        // a row (20 columns of marker after a 2-column indent).
        let text = composer.text().to_owned();
        for width in 22..48 {
            for row in layout(&text, 2, 2, width) {
                assert!(
                    composer
                        .chip_at(row.start)
                        .is_none_or(|(_, end)| end == row.start),
                    "width {width} breaks inside the marker: {row:?}"
                );
            }
        }
        let mut wrapped = Composer::new();
        wrapped.insert("abcdefgh ");
        wrapped.paste_text(&lines(4));
        let width = 24;
        assert!(wrapped.move_up(2, 2, width));
        let at = wrapped.cursor();
        assert!(wrapped.chip_at(at).is_none_or(|(_, end)| end == at));
    }

    #[test]
    fn expanded_replaces_every_marker() {
        let mut composer = Composer::new();
        composer.insert("first ");
        composer.paste_text(&lines(4));
        composer.insert(" then ");
        composer.paste_text(&"z".repeat(900));
        composer.insert(" end");
        assert_eq!(
            composer.text(),
            "first [pasted #1: 4 lines] then [pasted #2: 900 characters] end"
        );
        assert_eq!(
            composer.expanded(),
            format!("first {} then {} end", lines(4), "z".repeat(900))
        );

        // A pasted text that looks like a marker is not expanded again.
        let mut nested = Composer::new();
        nested.paste_text(&format!("[pasted #2: 4 lines]\n{}", lines(4)));
        nested.paste_text(&lines(6));
        assert_eq!(
            nested.expanded(),
            format!("[pasted #2: 4 lines]\n{}{}", lines(4), lines(6))
        );
    }

    #[test]
    fn chip_ids_keep_counting() {
        let mut composer = Composer::new();
        composer.paste_text(&lines(4));
        composer.insert(" ");
        composer.paste_text(&lines(5));
        assert_eq!(composer.text(), "[pasted #1: 4 lines] [pasted #2: 5 lines]");

        composer.move_home();
        assert!(composer.delete_forward());
        assert_eq!(composer.text(), " [pasted #2: 5 lines]");
        composer.move_end();
        composer.paste_text(&lines(6));
        assert_eq!(composer.text(), " [pasted #2: 5 lines][pasted #3: 6 lines]");
        assert_eq!(composer.expanded(), format!(" {}{}", lines(5), lines(6)));

        // A cleared draft has no chips left, so numbering starts again.
        composer.clear();
        composer.paste_text(&lines(4));
        assert_eq!(composer.text(), "[pasted #1: 4 lines]");
    }

    #[test]
    fn a_chip_expands_in_place_as_one_edit() {
        let mut composer = Composer::new();
        composer.insert("a ");
        composer.paste_text(&lines(4));
        assert!(composer.caret_on_chip());
        assert!(composer.expand_chip_at_caret());
        assert_eq!(composer.text(), format!("a {}", lines(4)));
        assert!(!composer.caret_on_chip());
        assert!(!composer.expand_chip_at_caret());
        assert!(composer.undo());
        assert_eq!(composer.text(), "a [pasted #1: 4 lines]");
        composer.move_home();
        assert!(!composer.caret_on_chip());
        composer.move_right();
        composer.move_right();
        assert!(composer.caret_on_chip());
    }

    #[test]
    fn a_range_collapses_into_a_chip_as_one_edit() {
        let mut composer = Composer::new();
        composer.insert("keep ");
        composer.insert(&lines(4));
        assert!(composer.collapse_into_chip(5, composer.text().chars().count()));
        assert_eq!(composer.text(), "keep [pasted #1: 4 lines]");
        assert_eq!(composer.expanded(), format!("keep {}", lines(4)));
        assert!(composer.undo());
        assert_eq!(composer.text(), format!("keep {}", lines(4)));
    }

    #[test]
    fn a_history_walk_keeps_the_chips_of_the_parked_draft() {
        let mut composer = Composer::new();
        composer.paste_text(&lines(4));
        let entries = vec!["older".to_owned()];
        assert!(composer.history_previous(&entries));
        assert_eq!(composer.expanded(), "older");
        assert!(composer.history_next(&entries));
        assert_eq!(composer.text(), "[pasted #1: 4 lines]");
        assert_eq!(composer.expanded(), lines(4));
    }
}
