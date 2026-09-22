//! Frame composition and minimal repaint.
//!
//! A frame is composed into a [`Grid`] first and then committed against the
//! previous frame. The commit carries only the cells that changed, so a spinner
//! tick or a status line update costs a few dozen bytes rather than a repaint.
//!
//! Every commit is verified by replaying its own bytes into the previous screen
//! and comparing the result against the target. A mismatch is reported rather
//! than written, which turns a rendering defect into an error instead of a
//! screen that drifts further out of step with each frame.

use std::fmt::Write as _;

use rune_core::error::{Result, RuneError};

use crate::engine::{Cell, Color, DiffSpan, Grid, Style};
use crate::width::truncate_to_width;

/// The regions a frame is composed from.
///
/// The footer is pinned to the bottom of the screen and the transcript fills
/// what is left, keeping the newest rows, because new output appears at the
/// bottom and a reader watches the bottom of a live interface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Regions<'a> {
    /// Transcript rows, oldest first.
    pub transcript: &'a [String],
    /// What the agent is doing, drawn directly above the prompt.
    pub activity: Option<&'a str>,
    /// Prompt rows, top first.
    pub prompt: &'a [String],
    /// Footer rows, top first.
    pub footer: &'a [String],
    /// Prompt row carrying the cursor.
    pub cursor_row: usize,
    /// Column carrying the cursor.
    pub cursor_col: usize,
}

impl<'a> Regions<'a> {
    /// Returns a frame of a transcript over a prompt and a footer, with the
    /// cursor at the start of the first prompt row.
    #[must_use]
    pub const fn new(transcript: &'a [String], footer: &'a [String]) -> Self {
        Self {
            transcript,
            activity: None,
            prompt: &[],
            footer,
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    /// Places the activity line above the prompt.
    #[must_use]
    pub const fn with_activity(mut self, activity: Option<&'a str>) -> Self {
        self.activity = activity;
        self
    }

    /// Places the prompt rows above the footer.
    #[must_use]
    pub const fn with_prompt(mut self, prompt: &'a [String]) -> Self {
        self.prompt = prompt;
        self
    }

    /// Places the cursor within the prompt.
    #[must_use]
    pub const fn with_cursor(mut self, row: usize, col: usize) -> Self {
        self.cursor_row = row;
        self.cursor_col = col;
        self
    }

    /// Returns the rows the frame draws, top first.
    ///
    /// Rows are clipped to the width here, before anything is fed to a grid, so
    /// a long line can never wrap and displace the rows below it. A transcript
    /// shorter than its region is padded above rather than below, which keeps
    /// the newest line against the prompt and the footer against the bottom of
    /// the screen.
    #[must_use]
    pub fn rows(&self, cols: u16, rows: u16) -> Vec<String> {
        let total = usize::from(rows);
        let width = usize::from(cols);
        let footer = self.footer.len().min(total);
        let prompt = self.prompt.len().min(total.saturating_sub(footer));
        let activity =
            usize::from(self.activity.is_some() && footer.saturating_add(prompt) < total);
        let area = total
            .saturating_sub(footer)
            .saturating_sub(prompt)
            .saturating_sub(activity);
        let start = self
            .transcript
            .len()
            .saturating_sub(self.transcript.len().min(area));
        let kept: Vec<String> = self
            .transcript
            .get(start..)
            .unwrap_or_default()
            .iter()
            .map(|line| clip(line, width))
            .collect();
        let mut lines: Vec<String> = vec![String::new(); area.saturating_sub(kept.len())];
        lines.extend(kept);
        if activity == 1 {
            lines.push(clip(self.activity.unwrap_or_default(), width));
        }
        lines.extend(
            self.prompt
                .iter()
                .take(prompt)
                .map(|line| clip(line, width)),
        );
        lines.extend(
            self.footer
                .iter()
                .take(footer)
                .map(|line| clip(line, width)),
        );
        lines
    }

    /// Returns where the cursor rests, in a zero based coordinate.
    #[must_use]
    pub fn cursor(&self, cols: u16, rows: u16) -> (u16, u16) {
        let total = usize::from(rows);
        let footer = self.footer.len().min(total);
        let prompt = self.prompt.len().min(total.saturating_sub(footer));
        let start = total.saturating_sub(footer).saturating_sub(prompt);
        let row = if prompt == 0 {
            total.saturating_sub(1)
        } else {
            start.saturating_add(self.cursor_row.min(prompt.saturating_sub(1)))
        };
        let row = row.min(total.saturating_sub(1));
        let col = self.cursor_col.min(usize::from(cols).saturating_sub(1));
        (
            u16::try_from(row).unwrap_or(0),
            u16::try_from(col).unwrap_or(0),
        )
    }
}

/// Composes a frame into a grid.
///
/// The grid is built by feeding bytes rather than by writing cells, so the
/// composed screen is reachable by exactly the same route the terminal takes.
pub fn compose(regions: &Regions<'_>, cols: u16, rows: u16) -> Result<Grid> {
    let mut grid = Grid::new(cols, rows)?;
    let mut out = String::new();
    for (index, line) in regions.rows(cols, rows).iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        let Ok(row) = u16::try_from(index) else {
            break;
        };
        out.push_str(&cup(row, 0));
        out.push_str(line);
        if line.contains('\u{1b}') {
            out.push_str(Style::RESET);
        }
    }
    let (row, col) = regions.cursor(cols, rows);
    out.push_str(&cup(row, col));
    grid.feed(out.as_bytes())?;
    Ok(grid)
}

/// The committed screen and what the commit cost.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct FrameCommit {
    /// Bytes to write to the terminal.
    pub bytes: Vec<u8>,
    /// Columns written across every span.
    pub changed_cells: usize,
    /// Number of contiguous spans written.
    pub spans: usize,
    /// Rows repainted because an earlier write was interrupted.
    pub invalidated_rows: usize,
}

impl FrameCommit {
    /// Returns true when the commit carries nothing to write.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// The screen a frame is committed against.
///
/// Holds the frame that was last committed and a shadow of what the terminal
/// is showing. The two are only ever equal after a verified commit, so the diff
/// between them is exactly what the terminal is missing.
#[derive(Clone, Debug)]
pub struct FrameSurface {
    target: Grid,
    shadow: Grid,
    /// The screen image before the last commit, so an interrupted write can be
    /// rolled back to it instead of starting from a screen that was never shown.
    before: Grid,
    last: Vec<u8>,
    last_spans: Vec<DiffSpan>,
    pending: Vec<DiffSpan>,
    invalidations: u64,
}

impl FrameSurface {
    /// Returns a surface whose shadow is blank, matching a cleared screen.
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        Ok(Self {
            target: Grid::new(cols, rows)?,
            shadow: Grid::new(cols, rows)?,
            before: Grid::new(cols, rows)?,
            last: Vec::new(),
            last_spans: Vec::new(),
            pending: Vec::new(),
            invalidations: 0,
        })
    }

    /// Returns a surface adopting an existing screen.
    ///
    /// Used when the terminal holds content this process did not draw, and
    /// after a screen the process cannot account for.
    pub fn with_shadow(shadow: Grid) -> Self {
        Self {
            target: shadow.clone(),
            before: shadow.clone(),
            shadow,
            last: Vec::new(),
            last_spans: Vec::new(),
            pending: Vec::new(),
            invalidations: 0,
        }
    }

    /// Returns the frame the surface is committed against.
    #[must_use]
    pub const fn target(&self) -> &Grid {
        &self.target
    }

    /// Returns what the terminal is believed to be showing.
    #[must_use]
    pub const fn shadow(&self) -> &Grid {
        &self.shadow
    }

    /// Returns the bytes of the last commit.
    #[must_use]
    pub fn last_bytes(&self) -> &[u8] {
        &self.last
    }

    /// Returns how many interrupted writes have been recovered.
    #[must_use]
    pub const fn invalidations(&self) -> u64 {
        self.invalidations
    }

    /// Returns the bytes that turn the current screen into a target.
    ///
    /// An unchanged frame yields no bytes at all, including no cursor move, so
    /// an idle interface writes nothing.
    pub fn commit(&mut self, target: &Grid) -> Result<FrameCommit> {
        let bounds = target.bounds();
        if bounds != self.target.bounds() {
            // A resize leaves the real screen in a state nothing here can know,
            // so both sides are rebuilt and the whole frame is repainted.
            self.target = Grid::new(bounds.cols, bounds.rows)?;
            self.shadow = Grid::new(bounds.cols, bounds.rows)?;
            self.before = self.shadow.clone();
            self.pending.clear();
        }

        let mut spans = self.shadow.diff(target);
        let invalidated_rows = rows_of(&self.pending);
        for span in &self.pending {
            spans.push(*span);
        }
        coalesce(&mut spans);
        self.pending.clear();

        let mut state = self.shadow.style();
        if spans.is_empty() && self.shadow.cursor() == target.cursor() && state == target.style() {
            self.target.clone_from(target);
            self.before.clone_from(&self.shadow);
            self.last.clear();
            self.last_spans.clear();
            return Ok(FrameCommit::default());
        }

        let mut out = String::new();
        for span in &spans {
            write_span(&mut out, target, *span, &mut state);
        }
        out.push_str(&cursor_sequence(target, &mut state));
        // The cursor is left where the target has it and in the target's own
        // attributes, so the next commit starts from a state this one recorded.
        out.push_str(&transition(state, target.style()));

        let bytes = out.into_bytes();
        let mut replayed = self.shadow.clone();
        replayed.feed(&bytes)?;
        if replayed != *target {
            return Err(RuneError::invariant(
                "frame_replay",
                "replaying a commit did not reproduce its target screen",
            )
            .with_hint("the target holds a cell the terminal cannot produce"));
        }

        let changed_cells = spans.iter().fold(0usize, |total, span| {
            total.saturating_add(usize::from(span.len()))
        });
        let commit = FrameCommit {
            bytes: bytes.clone(),
            changed_cells,
            spans: spans.len(),
            invalidated_rows,
        };
        // The old image moves into `before`, so an interrupted write can be
        // rolled back to it without a second copy of the screen.
        self.before = std::mem::replace(&mut self.shadow, replayed);
        self.target.clone_from(target);
        self.last = bytes;
        self.last_spans = spans;
        Ok(commit)
    }

    /// Records that a commit was interrupted after the terminal accepted a
    /// prefix of its bytes.
    ///
    /// The shadow is advanced by exactly that prefix, so it still describes the
    /// screen, and every row the interrupted commit touched is scheduled for
    /// one repaint, because the terminal cannot be asked what it received.
    pub fn recover(&mut self, accepted: usize, target: &Grid) -> Result<()> {
        if accepted > self.last.len() {
            return Err(RuneError::invalid_field(
                "accepted",
                format!(
                    "a write of {} bytes cannot have delivered {accepted}",
                    self.last.len()
                ),
            )
            .with_observed(accepted.to_string()));
        }
        let prefix = self.last.get(..accepted).unwrap_or_default();
        self.shadow.clone_from(&self.before);
        self.shadow.feed(prefix)?;
        self.target.clone_from(target);
        self.pending.clone_from(&self.last_spans);
        self.invalidations = self.invalidations.saturating_add(1);
        Ok(())
    }
}

/// Appends text destined for the terminal's own scrollback.
///
/// A newline on its own moves down a row and keeps the column, so a document
/// that relies on it lands at the wrong column whenever the previous line was
/// shorter than the one before it. Only a carriage return followed by a
/// newline is accepted as a line break.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DocumentAppend {
    bytes: Vec<u8>,
}

impl DocumentAppend {
    /// Returns an append after checking every line break in it.
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        let mut previous = 0u8;
        for (offset, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' && previous != b'\r' {
                return Err(RuneError::invalid_field(
                    "document",
                    format!("a newline at offset {offset} is not preceded by a carriage return"),
                )
                .with_observed(offset.to_string())
                .with_hint("end a line with a carriage return, then a newline"));
            }
            previous = *byte;
        }
        Ok(Self { bytes })
    }

    /// Returns an append holding lines separated by carriage return and
    /// newline, with the document ending on a complete line.
    #[must_use]
    pub fn for_lines(lines: &[&str]) -> Self {
        let mut bytes = Vec::new();
        for line in lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }
        Self { bytes }
    }

    /// Returns the bytes to write.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the number of bytes to write.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns true when there is nothing to write.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns the number of complete lines in the document.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.bytes.windows(2).filter(|pair| pair == b"\r\n").count()
    }
}

/// Returns the sequence that moves the cursor to a zero based position.
fn cup(row: u16, col: u16) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "\u{1b}[{};{}H",
        row.saturating_add(1),
        col.saturating_add(1)
    );
    out
}

/// Returns the sequence that leaves the cursor where a target grid has it.
fn cursor_sequence(target: &Grid, state: &mut Style) -> String {
    let cursor = target.cursor();
    let mut out = cup(cursor.row, cursor.col);
    if cursor.pending_wrap
        && let Some(cell) = target.cell(cursor.row, cursor.col)
    {
        // A cursor parked at the right edge is only set by printing there, so
        // the glyph is written again with its own attributes.
        out.push_str(&transition(*state, cell.style));
        state.clone_from(&cell.style);
        out.push_str(&cell_text(cell));
    }
    out
}

/// Returns the bytes that take the terminal from one style to another.
///
/// An SGR sequence sets only the attributes it names, so a style that omits an
/// attribute the previous one set would inherit it. A style that cannot be
/// reached by adding to the current one starts from a reset.
fn transition(previous: Style, next: Style) -> String {
    if previous == next {
        return String::new();
    }
    let mut out = String::new();
    if previous.hyperlink != next.hyperlink && previous.hyperlink.is_some() {
        out.push_str(Style::HYPERLINK_CLOSE);
    }
    if !next.has_sgr() {
        if previous.has_sgr() {
            out.push_str(Style::RESET);
        }
        return out;
    }
    let stale = previous.flags & !next.flags != 0
        || (previous.fg != Color::Default && next.fg == Color::Default)
        || (previous.bg != Color::Default && next.bg == Color::Default);
    if stale {
        out.push_str(Style::RESET);
    }
    out.push_str(&next.sgr());
    out
}

/// Appends one span, moving the cursor first and setting the style before the
/// cells that need it.
fn write_span(out: &mut String, grid: &Grid, span: DiffSpan, state: &mut Style) {
    out.push_str(&cup(span.row, span.start));
    let mut col = span.start;
    while col < span.end {
        let Some(cell) = grid.cell(span.row, col) else {
            break;
        };
        if !cell.is_continuation() {
            out.push_str(&transition(*state, cell.style));
            state.clone_from(&cell.style);
            out.push_str(&cell_text(cell));
        }
        col = col.saturating_add(1);
    }
}

/// Returns the printable text of a cell, combining mark included.
fn cell_text(cell: &Cell) -> String {
    let mut out = String::new();
    if cell.is_continuation() {
        return out;
    }
    out.push(cell.codepoint);
    if let Some(mark) = cell.mark {
        out.push(mark);
    }
    out
}

/// Cuts a row to a width, closing a style the cut left open.
fn clip(line: &str, width: usize) -> String {
    let (prefix, _) = truncate_to_width(line, width);
    let mut out = prefix.to_owned();
    if out.len() != line.len() && line.contains('\u{1b}') {
        out.push_str(Style::RESET);
    }
    out
}

/// Returns the distinct rows a set of spans touches.
fn rows_of(spans: &[DiffSpan]) -> usize {
    let mut rows: Vec<u16> = spans.iter().map(|span| span.row).collect();
    rows.sort_unstable();
    rows.dedup();
    rows.len()
}

/// Sorts spans and merges the ones that touch or overlap.
///
/// Two spans of the same row that are adjacent are one write, and a span that
/// repeats another is dropped, so a repaint forced by an interrupted write does
/// not double the bytes.
fn coalesce(spans: &mut Vec<DiffSpan>) {
    spans.sort_unstable_by_key(|span| (span.row, span.start, span.end));
    let mut merged: Vec<DiffSpan> = Vec::with_capacity(spans.len());
    for span in spans.drain(..) {
        match merged.last_mut() {
            Some(last) if last.row == span.row && last.end >= span.start => {
                last.end = last.end.max(span.end);
            }
            _ => merged.push(span),
        }
    }
    *spans = merged;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Grid, flag};
    use crate::theme::Theme;
    use crate::width::str_width;
    use rune_core::error::ErrorCode;

    fn grid(cols: u16, rows: u16) -> Grid {
        Grid::new(cols, rows).expect("grid")
    }

    fn typed(cols: u16, rows: u16, text: &[&str]) -> Grid {
        let mut out = grid(cols, rows);
        for (index, line) in text.iter().enumerate() {
            let Ok(row) = u16::try_from(index) else {
                break;
            };
            let bytes = format!("{}{}", cup(row, 0), line);
            out.feed(bytes.as_bytes()).expect("feed");
        }
        out.feed(cup(0, 0).as_bytes()).expect("feed");
        out
    }

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn an_unchanged_frame_emits_no_bytes() {
        let target = typed(20, 3, &["hello", "world"]);
        let mut surface = FrameSurface::with_shadow(target.clone());
        let commit = surface.commit(&target).expect("commit");
        assert!(commit.is_empty());
        assert_eq!(commit.changed_cells, 0);
        assert_eq!(commit.spans, 0);
        assert!(surface.last_bytes().is_empty());
    }

    #[test]
    fn a_first_commit_paints_the_whole_frame() {
        let target = typed(20, 3, &["hello", "world"]);
        let mut surface = FrameSurface::new(20, 3).expect("surface");
        let commit = surface.commit(&target).expect("commit");
        assert!(!commit.is_empty());
        assert_eq!(commit.changed_cells, 10);
        assert_eq!(commit.spans, 2);
        assert_eq!(surface.shadow(), &target);
    }

    #[test]
    fn a_changed_row_repaints_only_that_row() {
        let before = typed(20, 3, &["hello", "world"]);
        let after = typed(20, 3, &["hello", "there"]);
        let mut surface = FrameSurface::with_shadow(before);
        let commit = surface.commit(&after).expect("commit");
        assert_eq!(commit.spans, 1);
        assert_eq!(commit.changed_cells, 5);
        let text = String::from_utf8_lossy(&commit.bytes).into_owned();
        assert!(text.contains("there"), "{text:?}");
        assert!(!text.contains("hello"), "{text:?}");
        assert_eq!(surface.shadow(), &after);
    }

    #[test]
    fn a_commit_that_replays_to_something_else_is_refused() {
        // A wide glyph whose base sits in the last column cannot be produced by
        // the terminal: printing it wraps to the next row. A target holding one
        // is therefore refused rather than written.
        let target =
            Grid::from_checkpoint(&patched(&grid(10, 3), 9, '\u{4e2d}', 2)).expect("checkpoint");
        let mut surface = FrameSurface::with_shadow(grid(10, 3));
        let err = surface.commit(&target).expect_err("unreachable target");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
        assert_eq!(err.detail().invariant.as_deref(), Some("frame_replay"));
        // Nothing was written, so the shadow still describes the old screen.
        assert_ne!(surface.shadow(), &target);
        assert!(surface.last_bytes().is_empty());
    }

    #[test]
    fn a_style_only_change_repaints_the_cells_that_carry_it() {
        let before = typed(20, 2, &["hello"]);
        let mut after = typed(20, 2, &["hello"]);
        let styled = format!("{}\u{1b}[31m{}hello", cup(1, 0), cup(0, 0));
        after.feed(styled.as_bytes()).expect("feed");
        let mut surface = FrameSurface::with_shadow(before);
        let commit = surface.commit(&after).expect("commit");
        assert_eq!(commit.changed_cells, 5);
        assert_eq!(surface.shadow(), &after);
    }

    #[test]
    fn a_style_that_drops_an_attribute_does_not_inherit_it() {
        // An SGR sequence sets only what it names, so repainting a row from a
        // dim style into a bold one has to clear the dim bit first or every
        // cell keeps an attribute the target does not have.
        let dim = format!("{}\u{1b}[1;1Hdimmed", Style::RESET);
        let mut before = grid(20, 2);
        before.feed(dim.as_bytes()).expect("feed");
        let mut target = grid(20, 2);
        target.feed(dim.as_bytes()).expect("feed");
        let bold = format!("{}\u{1b}[1mbold\u{1b}[0m", cup(0, 0));
        target.feed(bold.as_bytes()).expect("feed");

        let mut surface = FrameSurface::with_shadow(before);
        surface.commit(&target).expect("commit");
        assert_eq!(surface.shadow(), &target);
        for col in 0..4u16 {
            let cell = surface.shadow().cell(0, col).expect("cell");
            assert!(cell.style.has_flag(flag::BOLD), "column {col} lost bold");
            assert!(!cell.style.has_flag(flag::DIM), "column {col} kept dim");
        }
    }

    #[test]
    fn a_cursor_only_change_costs_a_move_and_nothing_else() {
        let mut target = typed(20, 3, &["hello"]);
        target.feed(cup(2, 7).as_bytes()).expect("feed");
        let mut surface = FrameSurface::with_shadow(typed(20, 3, &["hello"]));
        let commit = surface.commit(&target).expect("commit");
        assert_eq!(commit.changed_cells, 0);
        assert_eq!(commit.spans, 0);
        assert!(!commit.is_empty());
        assert_eq!(surface.shadow(), &target);
    }

    #[test]
    fn a_wider_terminal_repaints_whole_rows() {
        let mut surface = FrameSurface::with_shadow(typed(10, 2, &["hello"]));
        let target = typed(20, 2, &["hello"]);
        let commit = surface.commit(&target).expect("commit");
        assert!(!commit.is_empty());
        assert_eq!(surface.shadow(), &target);
        assert_eq!(surface.shadow().bounds().cols, 20);
    }

    #[test]
    fn a_commit_after_a_resize_is_a_no_op_the_second_time() {
        let mut surface = FrameSurface::with_shadow(typed(10, 2, &["hello"]));
        let target = typed(12, 4, &["hello", "again"]);
        surface.commit(&target).expect("commit");
        let again = surface.commit(&target).expect("commit");
        assert!(again.is_empty());
    }

    #[test]
    fn a_recovered_write_repaints_its_rows_exactly_once() {
        let target = typed(20, 3, &["hello", "world"]);
        let mut surface = FrameSurface::with_shadow(target.clone());
        let first = surface.commit(&target).expect("commit");
        assert!(first.is_empty(), "nothing changed yet");
        // A change is interrupted after the terminal took every byte of it. The
        // screen is right, but the process cannot prove it, so the rows are
        // painted once more and then trusted.
        let changed = typed(20, 3, &["hello", "there"]);
        let commit = surface.commit(&changed).expect("commit");
        surface
            .recover(commit.bytes.len(), &changed)
            .expect("recover");
        assert_eq!(surface.invalidations(), 1);
        let forced = surface.commit(&changed).expect("commit");
        assert_eq!(forced.invalidated_rows, 1);
        assert_eq!(forced.changed_cells, 5);
        let settled = surface.commit(&changed).expect("commit");
        assert!(settled.is_empty());
        assert_eq!(settled.invalidated_rows, 0);
    }

    #[test]
    fn a_recovered_prefix_advances_the_shadow() {
        let target = typed(20, 3, &["hello", "world"]);
        let changed = typed(20, 3, &["hello", "there"]);
        let mut surface = FrameSurface::with_shadow(target);
        let commit = surface.commit(&changed).expect("commit");
        surface.recover(1, &changed).expect("recover");
        // Only the cursor move arrived, so the screen still holds the old row.
        assert_eq!(surface.shadow().row_text(1), "world");
        assert_eq!(surface.invalidations(), 1);
        surface
            .recover(commit.bytes.len(), &changed)
            .expect("recover");
        assert_eq!(surface.shadow().row_text(1), "there");
        assert_eq!(surface.invalidations(), 2);
    }

    #[test]
    fn a_recovery_after_an_unchanged_frame_rolls_back_to_the_shown_screen() {
        let target = typed(20, 2, &["hello"]);
        let mut surface = FrameSurface::with_shadow(grid(20, 2));
        surface.commit(&target).expect("commit");
        // A frame with nothing to write must not move the rollback image back
        // to the screen from before the previous commit.
        let idle = surface.commit(&target).expect("commit");
        assert!(idle.is_empty());
        surface.recover(0, &target).expect("recover");
        assert_eq!(surface.shadow(), &target);
    }

    #[test]
    fn a_recovery_cannot_deliver_more_than_was_committed() {
        let target = typed(20, 2, &["hello"]);
        let mut surface = FrameSurface::with_shadow(grid(20, 2));
        let commit = surface.commit(&target).expect("commit");
        let err = surface
            .recover(commit.bytes.len().saturating_add(1), &target)
            .expect_err("over long recovery");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("accepted"));
    }

    #[test]
    fn a_frame_composes_every_region_bottom_up() {
        let transcript = lines(&["one", "two", "three"]);
        let prompt = lines(&["> fix it"]);
        let footer = lines(&["hint", "status"]);
        let regions = Regions::new(&transcript, &footer).with_prompt(&prompt);
        let composed = compose(&regions, 20, 6).expect("compose");
        assert_eq!(composed.row_text(0), "one");
        assert_eq!(composed.row_text(1), "two");
        assert_eq!(composed.row_text(2), "three");
        assert_eq!(composed.row_text(3), "> fix it");
        assert_eq!(composed.row_text(4), "hint");
        assert_eq!(composed.row_text(5), "status");
    }

    #[test]
    fn a_transcript_longer_than_the_screen_keeps_its_newest_rows() {
        let transcript = lines(&["one", "two", "three", "four", "five"]);
        let prompt = lines(&["> "]);
        let footer = lines(&["status"]);
        let regions = Regions::new(&transcript, &footer).with_prompt(&prompt);
        let composed = compose(&regions, 20, 5).expect("compose");
        assert_eq!(composed.row_text(0), "three");
        assert_eq!(composed.row_text(1), "four");
        assert_eq!(composed.row_text(2), "five");
        assert_eq!(composed.row_text(3), ">");
        assert_eq!(composed.row_text(4), "status");
    }

    #[test]
    fn a_short_transcript_is_padded_above_the_prompt() {
        let prompt = lines(&["> hello"]);
        let footer = lines(&["status"]);
        let transcript = lines(&["only"]);
        let regions = Regions::new(&transcript, &footer).with_prompt(&prompt);
        let composed = compose(&regions, 20, 6).expect("compose");
        for row in 0..3 {
            assert_eq!(composed.row_text(row), "", "row {row} should be blank");
        }
        assert_eq!(composed.row_text(3), "only");
        assert_eq!(composed.row_text(4), "> hello");
        assert_eq!(composed.row_text(5), "status");
    }

    #[test]
    fn the_activity_line_sits_directly_above_the_prompt() {
        for rows in 5..9u16 {
            let transcript = lines(&["one", "two", "three", "four", "five"]);
            let prompt = lines(&["> "]);
            let footer = lines(&["status"]);
            let regions = Regions::new(&transcript, &footer)
                .with_prompt(&prompt)
                .with_activity(Some("compiling"));
            let composed = compose(&regions, 20, rows).expect("compose");
            let last = rows.saturating_sub(1);
            assert_eq!(composed.row_text(last), "status");
            assert_eq!(composed.row_text(last.saturating_sub(1)), ">");
            assert_eq!(composed.row_text(last.saturating_sub(2)), "compiling");
        }
    }

    #[test]
    fn a_multi_row_prompt_keeps_its_rows_together() {
        let prompt = lines(&["first", "second", "third"]);
        let footer = lines(&["status"]);
        let transcript = lines(&["out"]);
        let regions = Regions::new(&transcript, &footer).with_prompt(&prompt);
        let composed = compose(&regions, 20, 5).expect("compose");
        assert_eq!(composed.row_text(1), "first");
        assert_eq!(composed.row_text(2), "second");
        assert_eq!(composed.row_text(3), "third");
        assert_eq!(composed.row_text(4), "status");
    }

    #[test]
    fn the_cursor_lands_in_the_prompt() {
        let prompt = lines(&["> ", "  second"]);
        let footer = lines(&["status"]);
        let transcript = lines(&["one"]);
        let regions = Regions::new(&transcript, &footer)
            .with_prompt(&prompt)
            .with_cursor(1, 3);
        let composed = compose(&regions, 20, 6).expect("compose");
        assert_eq!(composed.cursor().row, 4);
        assert_eq!(composed.cursor().col, 3);
    }

    #[test]
    fn a_row_never_exceeds_the_width() {
        let wide = lines(&["\u{4e2d}\u{6587} text that is far too long for the screen"]);
        let prompt = lines(&["> \u{4e2d}\u{6587}\u{4e2d}\u{6587}\u{4e2d}\u{6587}"]);
        let footer = lines(&["status \u{4e2d}\u{6587}\u{4e2d}\u{6587}"]);
        let regions = Regions::new(&wide, &footer).with_prompt(&prompt);
        let composed = compose(&regions, 12, 4).expect("compose");
        for row in 0..4 {
            let text = composed.row_text(row);
            assert!(str_width(&text) <= 12, "row {row} is too wide: {text:?}");
        }
    }

    #[test]
    fn composing_the_same_regions_twice_leaves_nothing_to_commit() {
        let transcript = lines(&["one", "two"]);
        let prompt = lines(&["> "]);
        let footer = lines(&["hint", "status"]);
        let regions = Regions::new(&transcript, &footer).with_prompt(&prompt);
        let first = compose(&regions, 20, 6).expect("compose");
        let mut surface = FrameSurface::new(20, 6).expect("surface");
        assert!(!surface.commit(&first).expect("commit").is_empty());
        let second = compose(&regions, 20, 6).expect("compose");
        assert!(surface.commit(&second).expect("commit").is_empty());
    }

    #[test]
    fn a_frame_drawn_from_the_footer_layout_covers_the_terminal() {
        let state = crate::footer::FooterState {
            model: "claude-sonnet-4".to_owned(),
            permission_mode: rune_core::config::PermissionMode::Auto,
            workspace: "/Users/dev/rune".to_owned(),
            context_used: 24_500,
            context_limit: 200_000,
            session_id: "9f2c1a7b4e".to_owned(),
        };
        let layout = crate::footer::solve((80, 24), 1, false, crate::footer::DEFAULT_MINIMUM_ROWS);
        assert!(!layout.too_small);
        let footer = crate::footer::render(&state, &layout, &Theme::fx_dark(), 80, true);
        let prompt = lines(&["> "]);
        let transcript: Vec<String> = (0..40).map(|index| format!("line {index}")).collect();
        let regions = Regions::new(&transcript, &footer).with_prompt(&prompt);
        let composed = compose(&regions, 80, 24).expect("compose");
        assert_eq!(
            composed.row_text(23),
            "claude-sonnet-4 | auto | ctx 12% (175.5k left) | /Users/dev/rune 9f2c1a7b"
        );
        assert_eq!(composed.row_text(22), crate::footer::HINTS);
        assert_eq!(composed.row_text(21), ">");
        assert_eq!(composed.row_text(20), "line 39");
        assert_eq!(composed.row_text(0), "line 19");
    }

    /// Returns a checkpoint of `grid` with one cell's glyph replaced.
    ///
    /// A checkpoint is the only route to a screen the terminal cannot produce,
    /// which is what makes the replay guard testable.
    fn patched(grid: &Grid, index: usize, codepoint: char, width: u8) -> Vec<u8> {
        const HEADER: usize = 13;
        let bounds = grid.bounds();
        let cells = usize::from(bounds.cols).saturating_mul(usize::from(bounds.rows));
        let mut bytes = grid.checkpoint();
        let block = bytes.len().saturating_sub(cells.saturating_mul(11));
        let record = block.saturating_add(index.saturating_mul(11));
        bytes[record.saturating_add(2)..record.saturating_add(6)]
            .copy_from_slice(&u32::from(codepoint).to_le_bytes());
        bytes[record.saturating_add(6)] = width;
        let hash = fnv1a(bytes.get(HEADER..).unwrap_or_default());
        bytes[5..13].copy_from_slice(&hash.to_le_bytes());
        bytes
    }

    /// Returns the FNV-1a digest of a byte slice.
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
}
