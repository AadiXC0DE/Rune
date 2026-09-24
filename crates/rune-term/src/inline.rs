//! Inline rendering for an interactive session.
//!
//! The session writes into the terminal's normal flow rather than taking the
//! alternate screen. Finished lines are printed once and become part of the
//! terminal's own scrollback, which is what keeps a long conversation readable
//! with the terminal's search and copy. A small region at the bottom is
//! repainted in place for the prompt, the status line, and whatever the agent is
//! doing.
//!
//! Two rules follow from that, and both were broken before.
//!
//! Only one component may write to the terminal. A region painted at absolute
//! positions and a line printed in the flow disagree about where anything is,
//! because the printed line moves everything below it. Writing both means the
//! second overwrites rows the first placed by count, which eats characters and
//! leaves fragments of old frames on screen.
//!
//! The repainted region must be cleared before it is written. A row that gets
//! shorter would otherwise leave the tail of what it held before.
//!
//! Every move is relative to the cursor, which the renderer leaves inside its own
//! region. A relative move is unaffected by the screen scrolling, so a burst of
//! output that scrolls the terminal cannot desynchronise the renderer from it.

use std::fmt::Write as _;

use crate::width::{str_width, truncate_to_width};

/// Hides the cursor while a region is being rewritten.
pub const HIDE_CURSOR: &str = "\u{1b}[?25l";

/// Shows the cursor once the region is consistent.
pub const SHOW_CURSOR: &str = "\u{1b}[?25h";

/// Erases from the cursor to the end of the screen.
const ERASE_BELOW: &str = "\u{1b}[J";

/// Resets every attribute.
const RESET: &str = "\u{1b}[0m";

/// The region repainted in place at the bottom of the screen.
///
/// Rows are supplied in display order, top first: the activity line, then the
/// prompt, then the status lines. The renderer owns no content; it turns rows
/// into the bytes that put them on the terminal and leaves the cursor where the
/// caller says the caret belongs.
#[derive(Clone, Debug)]
pub struct Inline {
    cols: u16,
    /// Tallest the region may grow.
    ///
    /// A region taller than the screen cannot be repainted in place: writing it
    /// scrolls the terminal, which moves the rows the renderer believes it owns
    /// and leaves a copy of the status block behind. Capping the region means
    /// every frame it draws is one it can also erase.
    max_rows: u16,
    /// Where the cursor was left inside the region, counted from its top.
    cursor_row: u16,
    /// Whether the region has been drawn at least once.
    drawn: bool,
    /// Rows of the region as the terminal last received them.
    ///
    /// Kept so a frame can rewrite only the rows that changed. A streamed answer
    /// adds a few characters per delta and touches one row, while redrawing the
    /// whole region per delta costs every visible row each time, which is what
    /// made a long response write hundreds of times the size of the answer.
    shown: Vec<String>,
    /// Rows of the region that were settled into the flow when last drawn.
    shown_settled: usize,
}

impl Inline {
    /// Builds a renderer for a terminal `cols` columns wide.
    #[must_use]
    pub const fn new(cols: u16) -> Self {
        Self {
            cols,
            max_rows: u16::MAX,
            cursor_row: 0,
            drawn: false,
            shown: Vec::new(),
            shown_settled: 0,
        }
    }

    /// Changes the width, which invalidates nothing because every row is redrawn.
    pub const fn set_width(&mut self, cols: u16) {
        self.cols = cols;
    }

    /// Changes how many rows the region may occupy.
    ///
    /// A terminal that reports its height should be told to the renderer, so a
    /// region that would fill the screen is trimmed rather than scrolled. A
    /// limit of zero would leave nothing drawable, so it is raised to one.
    pub fn set_max_rows(&mut self, rows: u16) {
        let limit = rows.max(1);
        if limit == self.max_rows {
            return;
        }
        self.max_rows = limit;
        // The rows that were drawn are no longer the rows this renderer would
        // draw, so the next frame must repaint from the top rather than diff
        // against a region of a different size.
        self.shown.clear();
        self.shown_settled = 0;
        self.cursor_row = 0;
        self.drawn = false;
    }

    /// Returns the width.
    #[must_use]
    pub const fn width(&self) -> u16 {
        self.cols
    }

    /// Returns whether the region has been drawn.
    #[must_use]
    pub const fn is_drawn(&self) -> bool {
        self.drawn
    }

    /// Renders one frame.
    ///
    /// `settled` holds lines that are finished and are printed once above the
    /// region, in order. `activity` and `footer` are the rows that sit above
    /// the prompt; `prompt` is the rows of the line being typed, and `below` is
    /// whatever is still arriving, which grows downward from the input. `caret`
    /// is the column the cursor belongs at within the prompt row.
    ///
    /// The order matters and is deliberate: the status rows sit above the input
    /// so the place a user types stays put, and text that streams in goes below
    /// it, where it can lengthen without moving the line being typed.
    ///
    /// Returns the bytes to write. Nothing here reads the terminal, so the same
    /// inputs always produce the same bytes.
    pub fn frame(
        &mut self,
        settled: &[String],
        activity: Option<&str>,
        footer: &[String],
        prompt: &[String],
        below: &[String],
        caret: (u16, u16),
    ) -> Vec<u8> {
        // The input row is always present, even when nothing is being typed.
        // Without it the caret has no home of its own and falls back onto the
        // status row, which is what made the two overlap until streaming
        // happened to supply a prompt row.
        let prompt: &[String] = if prompt.is_empty() {
            &[String::new()]
        } else {
            prompt
        };

        let capacity = usize::from(activity.is_some())
            .saturating_add(footer.len())
            .saturating_add(prompt.len())
            .saturating_add(below.len());
        let mut rows: Vec<String> = Vec::with_capacity(capacity);
        if let Some(activity) = activity {
            rows.push(self.clip(activity));
        }
        // The status rows sit above the input, which is where a reader looks
        // for them and where they do not move as an answer arrives.
        rows.extend(footer.iter().map(|row| self.clip(row)));
        let prompt_start = rows.len();
        rows.extend(prompt.iter().map(|row| self.clip(row)));

        // A region taller than the screen cannot be repainted in place: writing
        // it scrolls the terminal, which moves the rows this renderer believes
        // it owns and leaves a copy of the status block behind. Only the rows
        // still arriving are trimmed, and the newest are the ones kept, because
        // the status and the input line are what must stay put.
        let reserved = rows.len();
        let room = usize::from(self.max_rows).saturating_sub(reserved).max(1);
        let arriving = if below.len() > room {
            below
                .get(below.len().saturating_sub(room)..)
                .unwrap_or(below)
        } else {
            below
        };
        rows.extend(arriving.iter().map(|row| self.clip(row)));
        let live_rows = u16::try_from(rows.len()).unwrap_or(u16::MAX).max(1);

        // Where the caret sits, as an offset from the top of the region. The
        // input row always exists by this point, so the caret is always placed
        // inside it and never on a row that belongs to something else.
        let within = caret
            .0
            .min(u16::try_from(prompt.len()).unwrap_or(1).saturating_sub(1));
        let caret_row = u16::try_from(prompt_start)
            .unwrap_or(0)
            .saturating_add(within);
        let caret_row = caret_row.min(live_rows.saturating_sub(1));

        // A frame that changes nothing still has to leave the cursor where the
        // caller wants it, but it does not have to rewrite the region. Only the
        // caret move is emitted in that case, which is what makes an idle
        // interface free to keep up to date.
        let unchanged = self.drawn
            && self.shown == rows
            && self.shown_settled == settled.len()
            && self.cursor_row == caret_row
            && settled.is_empty();
        if unchanged {
            let mut out = String::new();
            out.push_str(HIDE_CURSOR);
            out.push('\r');
            up(&mut out, self.cursor_row);
            column(&mut out, caret.1);
            out.push_str(SHOW_CURSOR);
            return out.into_bytes();
        }

        // The region only grows at the bottom while text streams in and the
        // input row is fixed, so the rows above the change keep their identity.
        // Rewriting only from the first row that differs is what keeps a delta's
        // cost proportional to what it changed rather than to the whole region.
        // A partial repaint is only sound while the region has not moved. A
        // frame that prints settled lines above the region scrolls it, so the
        // rows that look unchanged by index are no longer where they were.
        // Those frames repaint the region whole.
        let scrolled = !settled.is_empty();
        let first_changed = if self.drawn && self.shown_settled == settled.len() && !scrolled {
            self.shown
                .iter()
                .zip(rows.iter())
                .position(|(was, now)| was != now)
                .unwrap_or_else(|| self.shown.len().min(rows.len()))
        } else {
            0
        };

        let mut out = String::new();
        out.push_str(HIDE_CURSOR);
        out.push('\r');

        if self.drawn {
            // Back to the top of the region that is on screen, so a write can
            // begin from a known row. A frame that repaints everything clears
            // the region first, because it may be shorter than what is there.
            up(&mut out, self.cursor_row);
            if first_changed == 0 {
                out.push_str(ERASE_BELOW);
            }
        }

        for line in settled {
            out.push_str(&self.clip(line));
            out.push_str("\r\n");
        }

        // The region starts where the settled lines ended. Writing it may
        // scroll, which is what keeps the newest row against the input.
        //
        // Rows before the change were left in place, so the cursor steps down
        // to the first row that has to be written rather than rewriting them.
        // Without this step the write lands at the region's top and overwrites
        // the rows it was meant to leave alone.
        if first_changed > 0 {
            down(&mut out, u16::try_from(first_changed).unwrap_or(u16::MAX));
        }

        for (index, row) in rows.iter().enumerate().skip(first_changed) {
            if index > first_changed {
                out.push_str("\r\n");
            }
            out.push_str(row);
            // A row is padded rather than left short, so the caret column below
            // is reachable when the row itself is empty.
            if str_width(row) < usize::from(self.cols) {
                out.push_str("\u{1b}[K");
                out.push_str(RESET);
            } else {
                out.push_str(RESET);
            }
        }

        // Rows that were shown but are gone have to be removed, or a shrinking
        // region leaves its tail on the screen. The region is always the
        // bottom-most thing drawn, so everything below the last row written is
        // stale and is erased in one sequence. Walking down row by row would
        // write a newline at the bottom of the screen, which scrolls, pushing
        // the rows just written off the top.
        if self.drawn && self.shown.len() > rows.len().max(first_changed) {
            out.push_str(ERASE_BELOW);
        }

        // The caret is placed after the region is written. The walk back is
        // measured from the last row written, which may be above the region's
        // bottom when only an early row changed.
        let last_written = rows
            .len()
            .saturating_sub(1)
            .max(first_changed.min(rows.len().saturating_sub(1)));
        let from_bottom = last_written.saturating_sub(usize::from(caret_row));
        up(&mut out, u16::try_from(from_bottom).unwrap_or(u16::MAX));
        column(&mut out, caret.1);
        out.push_str(SHOW_CURSOR);

        self.shown = rows;
        self.shown_settled = settled.len();
        self.cursor_row = caret_row;
        self.drawn = true;
        out.into_bytes()
    }

    /// Clears the region, for a clean exit.
    ///
    /// Returns the bytes that remove what the session drew, leaving the cursor
    /// where the region began so the shell that started the session continues
    /// underneath it.
    pub fn clear(&mut self) -> Vec<u8> {
        if !self.drawn {
            return Vec::new();
        }
        let mut out = String::new();
        out.push_str(HIDE_CURSOR);
        out.push('\r');
        up(&mut out, self.cursor_row);
        out.push_str(ERASE_BELOW);
        out.push_str(SHOW_CURSOR);
        self.drawn = false;
        self.cursor_row = 0;
        self.shown.clear();
        self.shown_settled = 0;
        out.into_bytes()
    }

    /// Cuts a row to the terminal width.
    fn clip(&self, line: &str) -> String {
        let (prefix, _) = truncate_to_width(line, usize::from(self.cols));
        prefix.to_owned()
    }
}

/// Appends a move of `rows` rows towards the top of the screen.
///
/// A move of zero is left out, because the sequence for it is noise and the
/// terminal may treat it as one row rather than none.
fn up(out: &mut String, rows: u16) {
    if rows > 0 {
        let _ = write!(out, "\u{1b}[{rows}A");
    }
}

/// Appends a move of `rows` rows towards the bottom of the screen.
fn down(out: &mut String, rows: u16) {
    if rows > 0 {
        let _ = write!(out, "\u{1b}[{rows}B");
    }
}

/// Appends a move to a zero-based column of the current row.
fn column(out: &mut String, col: u16) {
    let _ = write!(out, "\u{1b}[{}G", col.saturating_add(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every control sequence in a byte string, in order.
    ///
    /// Lets a test assert on the moves a frame makes without matching the text.
    fn escapes(bytes: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let mut out = Vec::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\u{1b}' {
                continue;
            }
            let mut seq = String::from("\u{1b}");
            if chars.peek() == Some(&'[') {
                seq.push(chars.next().unwrap_or(' '));
                for ch in chars.by_ref() {
                    seq.push(ch);
                    if ch.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            out.push(seq);
        }
        out
    }

    fn rows(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn a_settled_line_is_printed_once_and_kept_in_the_flow() {
        let mut inline = Inline::new(40);
        let bytes = inline.frame(
            &rows(&["first line"]),
            None,
            &rows(&["status"]),
            &rows(&["> "]),
            &[],
            (0, 2),
        );
        let text = String::from_utf8(bytes).expect("utf8");
        // The line is written in the flow, on its own row, not positioned
        // absolutely, so the terminal scrolls it into its own scrollback.
        assert!(text.contains("first line\r\n"), "{text:?}");
        assert!(text.contains("> "), "{text:?}");
        assert!(text.contains("status"), "{text:?}");
    }

    #[test]
    fn the_cursor_is_hidden_while_a_frame_is_written_and_shown_after() {
        // A cursor visible mid-frame would be seen jumping between rows.
        let mut inline = Inline::new(40);
        let bytes = inline.frame(&[], None, &rows(&["s"]), &rows(&["> "]), &[], (0, 2));
        let text = String::from_utf8(bytes).expect("utf8");
        let hide = text.find(HIDE_CURSOR).expect("hidden");
        let show = text.find(SHOW_CURSOR).expect("shown");
        assert!(hide < show, "{text:?}");
        assert_eq!(text.matches(SHOW_CURSOR).count(), 1, "{text:?}");
    }

    #[test]
    fn the_second_frame_clears_the_region_before_writing_it() {
        // Without the clear, a row that shrinks leaves the tail of the row it
        // replaced, which is what makes an interface look like it is smearing.
        let mut inline = Inline::new(40);
        let _ = inline.frame(
            &[],
            None,
            &rows(&["a long status line"]),
            &rows(&["> "]),
            &[],
            (0, 2),
        );
        let bytes = inline.frame(&[], None, &rows(&["short"]), &rows(&["> "]), &[], (0, 2));
        let seq = escapes(&bytes);
        let erase = seq.iter().position(|s| s == ERASE_BELOW);
        assert!(erase.is_some(), "the region was not cleared: {seq:?}");
        // The clear happens before any text of the new frame.
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            text.find(ERASE_BELOW).unwrap() < text.find("short").unwrap(),
            "{text:?}"
        );
    }

    #[test]
    fn the_move_back_to_the_region_counts_the_rows_that_were_drawn() {
        // The region had three rows, so the next frame must walk back up three
        // from wherever the caret was left inside it.
        let mut inline = Inline::new(40);
        let _ = inline.frame(
            &[],
            Some("working"),
            &rows(&["s"]),
            &rows(&["> "]),
            &[],
            (0, 0),
        );
        let bytes = inline.frame(
            &[],
            Some("working"),
            &rows(&["s"]),
            &rows(&["> "]),
            &[],
            (0, 0),
        );
        let seq = escapes(&bytes);
        assert!(
            seq.iter()
                .any(|s| s == "\u{1b}[0A" || s == "\u{1b}[1A" || s == "\u{1b}[2A"),
            "no move back to the region: {seq:?}"
        );
    }

    #[test]
    fn the_caret_has_a_row_of_its_own_below_the_status_rows() {
        // With no prompt row the caret fell back to the last row of the region,
        // which is a status row, so the two overlapped until something supplied
        // a prompt. The input row is now always present, which pushes the caret
        // onto a row of its own below the status.
        let mut inline = Inline::new(40);
        let bytes = inline.frame(
            &[],
            None,
            &rows(&["status one", "status two"]),
            &[],
            &[],
            (0, 0),
        );
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(text.contains("status one"), "{text:?}");
        assert!(text.contains("status two"), "{text:?}");

        // Two status rows and the input row, so the write ends three rows below
        // where it started and the caret needs no move back: it is already on
        // the input row.
        let written = text.matches("\r\n").count();
        assert_eq!(written, 2, "the input row was not drawn: {text:?}");
        let seq = escapes(&bytes);
        assert!(
            !seq.iter().any(|s| s.ends_with('A')),
            "the caret was walked back onto a status row: {seq:?}"
        );
    }

    #[test]
    fn text_below_the_input_moves_the_caret_back_up_to_it() {
        // Streamed text extends below the input, so once it is drawn the caret
        // has to be walked back up to the line being typed. That walk is what
        // keeps the cursor on the input rather than on the status line.
        let mut inline = Inline::new(40);
        let bytes = inline.frame(
            &[],
            None,
            &rows(&["status"]),
            &rows(&["> hi"]),
            &rows(&["an answer"]),
            (0, 2),
        );
        let seq = escapes(&bytes);
        assert!(
            seq.iter().any(|s| s == "\u{1b}[1A"),
            "the caret did not reach the input row: {seq:?}"
        );
        // The row it lands on is the input, which is one above the answer.
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let input = text.find("> hi").expect("the input row");
        let answer = text.find("an answer").expect("the answer");
        assert!(input < answer, "the answer was drawn above the input");
    }

    #[test]
    fn the_caret_is_placed_where_the_caller_asked_within_the_prompt() {
        let mut inline = Inline::new(40);
        let bytes = inline.frame(&[], None, &rows(&["s"]), &rows(&["> hello"]), &[], (0, 7));
        let seq = escapes(&bytes);
        // A one-based column, so seven columns in is column eight.
        assert!(seq.iter().any(|s| s == "\u{1b}[8G"), "{seq:?}");
    }

    #[test]
    fn a_long_row_is_cut_to_the_terminal_width() {
        // A row that wrapped would push the region down and break the count the
        // next frame relies on.
        let mut inline = Inline::new(10);
        let long = "x".repeat(50);
        let bytes = inline.frame(
            &[],
            None,
            &rows(&[long.as_str()]),
            &rows(&["> "]),
            &[],
            (0, 2),
        );
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(!text.contains(&"x".repeat(11)), "{text:?}");
    }

    #[test]
    fn a_frame_with_no_settled_lines_still_draws_the_region() {
        let mut inline = Inline::new(30);
        let bytes = inline.frame(&[], None, &rows(&["status"]), &rows(&["> "]), &[], (0, 2));
        assert!(!bytes.is_empty());
        assert!(String::from_utf8_lossy(&bytes).contains("status"));
    }

    #[test]
    fn an_activity_row_sits_above_the_prompt() {
        let mut inline = Inline::new(40);
        let bytes = inline.frame(
            &[],
            Some("running a command"),
            &rows(&["s"]),
            &rows(&["> "]),
            &[],
            (0, 0),
        );
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let activity = text.find("running a command").expect("activity");
        let prompt = text.find("> ").expect("prompt");
        assert!(activity < prompt, "{text:?}");
    }

    #[test]
    fn the_first_frame_erases_nothing() {
        // The screen holds whatever the shell that started the session left.
        // Clearing from an unknown position would erase the user's own prompt.
        let mut inline = Inline::new(40);
        let bytes = inline.frame(&[], None, &rows(&["s"]), &rows(&["> "]), &[], (0, 2));
        let seq = escapes(&bytes);
        assert!(!seq.iter().any(|s| s == ERASE_BELOW), "{seq:?}");
    }

    #[test]
    fn a_frame_that_changes_one_row_writes_only_that_row() {
        // The whole point of tracking what was shown: a streamed answer adds a
        // few characters per delta, and rewriting the entire region per delta
        // cost hundreds of times the size of the answer.
        let mut inline = Inline::new(40);
        let footer = rows(&["status"]);
        let _ = inline.frame(
            &[],
            None,
            &footer,
            &rows(&["> "]),
            &rows(&["first"]),
            (0, 2),
        );
        let bytes = inline.frame(
            &[],
            None,
            &footer,
            &rows(&["> "]),
            &rows(&["first", "second"]),
            (0, 2),
        );
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            text.contains("second"),
            "the new row was not drawn: {text:?}"
        );
        assert!(
            !text.contains("status") && !text.contains("> "),
            "unchanged rows were rewritten: {text:?}"
        );
    }

    #[test]
    fn an_identical_frame_writes_only_a_caret_move() {
        let mut inline = Inline::new(40);
        let footer = rows(&["status"]);
        let _ = inline.frame(&[], None, &footer, &rows(&["> "]), &rows(&["x"]), (0, 2));
        let bytes = inline.frame(&[], None, &footer, &rows(&["> "]), &rows(&["x"]), (0, 2));
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            !text.contains("status"),
            "the region was rewritten: {text:?}"
        );
        assert!(
            text.contains("\u{1b}[3G"),
            "the caret did not move: {text:?}"
        );
    }

    #[test]
    fn a_frame_with_settled_lines_paints_the_region_whole() {
        // Printing a settled line above the region scrolls it, so the rows that
        // look unchanged by index have moved. Skipping them left stale rows on
        // the screen, which is what a partial repaint must never do across a
        // scroll.
        let mut inline = Inline::new(40);
        let footer = rows(&["status"]);
        let _ = inline.frame(&[], None, &footer, &rows(&["> "]), &rows(&["body"]), (0, 2));
        let bytes = inline.frame(
            &rows(&["a finished line"]),
            None,
            &footer,
            &rows(&["> "]),
            &rows(&["body"]),
            (0, 2),
        );
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(text.contains("a finished line"), "{text:?}");
        assert!(
            text.contains("status"),
            "the region was not repainted after scrolling: {text:?}"
        );
    }

    #[test]
    fn a_shrinking_region_clears_the_rows_it_lost() {
        // Asserted against the screen rather than the escapes: the point is that
        // the gone rows are not left behind, not which sequence erased them.
        let mut inline = Inline::new(40);
        let footer = rows(&["status"]);
        let first = inline.frame(
            &[],
            None,
            &footer,
            &rows(&["> "]),
            &rows(&["one", "two", "three"]),
            (0, 2),
        );
        let mut grid = crate::engine::Grid::new(40, 10).expect("grid");
        grid.feed(&first).expect("feed");
        assert!(grid.text().contains("three"), "{}", grid.text());

        let second = inline.frame(&[], None, &footer, &rows(&["> "]), &rows(&["one"]), (0, 2));
        grid.feed(&second).expect("feed");
        let screen = grid.text();
        assert!(screen.contains("one"), "{screen}");
        assert!(
            !screen.contains("three"),
            "a row that is gone was left on the screen:\n{screen}"
        );
    }

    #[test]
    fn a_partial_repaint_reproduces_the_same_screen_as_a_full_one() {
        // The rendered result must not depend on how it was reached. This is the
        // property the byte-saving depends on, so it is asserted directly.
        let footer = rows(&["status line"]);

        let mut diffed = Inline::new(40);
        let mut replay = crate::engine::Grid::new(40, 8).expect("grid");
        // The first frame is captured once and fed, because the renderer keeps
        // what it drew: asking it for the same frame twice returns only a caret
        // move on the second call.
        let first = diffed.frame(&[], None, &footer, &rows(&["> "]), &rows(&["a"]), (0, 2));
        replay.feed(&first).expect("feed");
        let step = diffed.frame(
            &[],
            None,
            &footer,
            &rows(&["> "]),
            &rows(&["a", "b", "c"]),
            (0, 2),
        );
        replay.feed(&step).expect("feed");

        let mut fresh = Inline::new(40);
        let whole = fresh.frame(
            &[],
            None,
            &footer,
            &rows(&["> "]),
            &rows(&["a", "b", "c"]),
            (0, 2),
        );
        let mut expected = crate::engine::Grid::new(40, 8).expect("grid");
        expected.feed(&whole).expect("feed");

        assert_eq!(
            replay.text(),
            expected.text(),
            "the incremental screen differs from the one-pass screen"
        );
    }

    #[test]
    fn clearing_removes_the_region_and_stops_the_next_frame_erasing() {
        let mut inline = Inline::new(40);
        let _ = inline.frame(&[], None, &rows(&["s"]), &rows(&["> "]), &[], (0, 2));
        let cleared = inline.clear();
        assert!(String::from_utf8_lossy(&cleared).contains(ERASE_BELOW));
        assert!(!inline.is_drawn());
        // Nothing left to clear, so a second clear writes nothing.
        assert!(inline.clear().is_empty());
    }

    /// Feeds frames into a grid the size of a real terminal and returns the
    /// screen, which is what a reader would see.
    fn screen_of(rows_high: u16, cols: u16, frames: &[Vec<u8>]) -> String {
        let mut grid = crate::engine::Grid::new(cols, rows_high).expect("grid");
        for frame in frames {
            grid.feed(frame).expect("feed");
        }
        grid.text()
    }

    #[test]
    fn a_region_taller_than_the_terminal_does_not_leave_a_stale_copy_behind() {
        // A live region that fills the screen scrolls the terminal as it is
        // written, which moves the rows the renderer believes it owns. Tracking
        // the cursor by row offset then lands on rows belonging to the previous
        // frame, so the status block is drawn twice and the output is written
        // over in the middle.
        const HEIGHT: u16 = 10;
        let footer = rows(&["status", "prompt"]);
        let answer = rows(&[
            "one", "two", "three", "four", "five", "six", "seven", "eight",
        ]);

        let mut inline = Inline::new(40);
        // The session tells the renderer how tall the terminal is, which is what
        // lets it keep the region inside the screen.
        inline.set_max_rows(HEIGHT.saturating_sub(1));
        let frames = vec![
            inline.frame(&[], None, &footer, &rows(&["> "]), &answer[..4], (0, 2)),
            inline.frame(&[], None, &footer, &rows(&["> "]), &answer[..6], (0, 2)),
            inline.frame(&[], None, &footer, &rows(&["> "]), &answer, (0, 2)),
        ];
        let screen = screen_of(HEIGHT, 40, &frames);

        assert_eq!(
            screen.matches("status").count(),
            1,
            "the status block was drawn more than once:\n{screen}"
        );
        // The newest rows are the ones kept, because that is where a reader is
        // looking while text arrives.
        for line in ["five", "six", "seven", "eight"] {
            assert!(
                screen.contains(line),
                "row {line:?} is missing from the screen:\n{screen}"
            );
        }
    }

    #[test]
    fn a_region_is_kept_inside_the_terminal_it_is_drawn_in() {
        // Whatever the caller passes, the region never exceeds the height the
        // renderer was told, because a taller one cannot be repainted in place.
        let mut inline = Inline::new(40);
        inline.set_max_rows(4);
        let many: Vec<String> = (0..40).map(|i| format!("row {i}")).collect();
        let bytes = inline.frame(&[], None, &rows(&["s"]), &rows(&["> "]), &many, (0, 2));
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let drawn = text.matches("\r\n").count().saturating_add(1);
        assert!(drawn <= 4, "the region drew {drawn} rows:\n{text}");
    }

    #[test]
    fn a_shrinking_region_never_scrolls_the_screen() {
        // Erasing rows that are gone must not write a newline past the region's
        // last row: at the bottom of the screen that scrolls, which pushes the
        // rows just written off the top and leaves the region untracked.
        let mut inline = Inline::new(40);
        // The same height the session would report for an eight-row terminal.
        inline.set_max_rows(7);
        let tall: Vec<String> = (0..10).map(|i| format!("row {i}")).collect();
        let first = inline.frame(&[], None, &rows(&["s"]), &rows(&["> "]), &tall, (0, 2));
        let mut grid = crate::engine::Grid::new(40, 8).expect("grid");
        grid.feed(&first).expect("feed");
        let before = grid.text();
        assert!(
            before.contains('>'),
            "the prompt is not on screen:\n{before}"
        );

        let second = inline.frame(&[], None, &rows(&["s"]), &rows(&["> "]), &tall[..2], (0, 2));
        grid.feed(&second).expect("feed");
        // The prompt is below the region's rows and must still be on screen.
        assert!(
            grid.text().contains('>'),
            "erasing the region's tail scrolled the prompt off:\nbefore:\n{before}\nafter:\n{}",
            grid.text()
        );
    }
}
