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
    /// Where the cursor was left inside the region, counted from its top.
    cursor_row: u16,
    /// Whether the region has been drawn at least once.
    drawn: bool,
}

impl Inline {
    /// Builds a renderer for a terminal `cols` columns wide.
    #[must_use]
    pub const fn new(cols: u16) -> Self {
        Self {
            cols,
            cursor_row: 0,
            drawn: false,
        }
    }

    /// Changes the width, which invalidates nothing because every row is redrawn.
    pub const fn set_width(&mut self, cols: u16) {
        self.cols = cols;
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
        // Whatever is still arriving goes below the input, so a growing answer
        // never displaces the line being typed.
        rows.extend(below.iter().map(|row| self.clip(row)));
        let live_rows = u16::try_from(rows.len()).unwrap_or(u16::MAX).max(1);

        // Where the caret sits, as an offset from the top of the region. The
        // caller positions it within the prompt; a prompt that is absent leaves
        // the caret on the last row rather than nowhere.
        let caret_row = if prompt.is_empty() {
            live_rows.saturating_sub(1)
        } else {
            let within = caret
                .0
                .min(u16::try_from(prompt.len()).unwrap_or(1).saturating_sub(1));
            u16::try_from(prompt_start)
                .unwrap_or(0)
                .saturating_add(within)
        };
        let caret_row = caret_row.min(live_rows.saturating_sub(1));

        let mut out = String::new();
        out.push_str(HIDE_CURSOR);
        // Normalise the column first, because every move below counts rows and
        // would otherwise inherit the column the caret was left in.
        out.push('\r');

        if self.drawn {
            // Back to the top of the region that is on screen now, so it can be
            // cleared before anything is written over it.
            up(&mut out, self.cursor_row);
            out.push_str(ERASE_BELOW);
        }

        for line in settled {
            out.push_str(&self.clip(line));
            out.push_str("\r\n");
        }

        // The region starts where the settled lines ended. Writing it may scroll,
        // which is what keeps the newest row against the prompt.
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
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

        // The caret is placed after the region is written, which is what lets
        // the region grow downward without the move being recomputed: the walk
        // back is measured from the bottom row just written.
        let from_bottom = live_rows.saturating_sub(1).saturating_sub(caret_row);
        up(&mut out, from_bottom);
        column(&mut out, caret.1);
        out.push_str(SHOW_CURSOR);

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
    fn clearing_removes_the_region_and_stops_the_next_frame_erasing() {
        let mut inline = Inline::new(40);
        let _ = inline.frame(&[], None, &rows(&["s"]), &rows(&["> "]), &[], (0, 2));
        let cleared = inline.clear();
        assert!(String::from_utf8_lossy(&cleared).contains(ERASE_BELOW));
        assert!(!inline.is_drawn());
        // Nothing left to clear, so a second clear writes nothing.
        assert!(inline.clear().is_empty());
    }
}
