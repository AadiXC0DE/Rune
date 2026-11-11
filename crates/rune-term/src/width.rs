//! Display width, grapheme segmentation, and wrapping for terminal text.
//!
//! Every width decision is made here rather than against a live terminal, so
//! placement is reproducible and testable without a TTY. The scanner is
//! escape-aware: an escape sequence is copied whole, never split, and never
//! counted toward a column.

use std::sync::atomic::{AtomicBool, Ordering};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::engine::Style;

/// Whether East Asian ambiguous characters count as two columns.
static AMBIGUOUS_WIDE: AtomicBool = AtomicBool::new(false);

/// Columns between tab stops.
const TAB_STOP: usize = 8;

/// Sets how many columns an East Asian ambiguous character occupies.
///
/// `2` makes them wide, matching a terminal set to draw them that way; any
/// other value makes them one column. Set once, before anything is drawn.
pub fn set_ambiguous_width(columns: u8) {
    AMBIGUOUS_WIDE.store(columns == 2, Ordering::Relaxed);
}

/// Returns true when ambiguous characters are measured as two columns.
fn ambiguous_wide() -> bool {
    // Tests share one process, so only the thread that asked for the setting
    // sees it and the others keep measuring the default.
    #[cfg(test)]
    if !tests::SEES_AMBIGUOUS_WIDTH.with(std::cell::Cell::get) {
        return false;
    }
    AMBIGUOUS_WIDE.load(Ordering::Relaxed)
}

/// Returns the number of columns a character occupies.
///
/// Control characters and combining marks return zero. A terminal never
/// advances the cursor for either, so reporting a real width for them would
/// desynchronize every column after them.
#[must_use]
pub fn char_width(c: char) -> u8 {
    if c.is_control() {
        return 0;
    }
    let width = if ambiguous_wide() {
        UnicodeWidthChar::width_cjk(c)
    } else {
        UnicodeWidthChar::width(c)
    };
    match width {
        Some(width) => u8::try_from(width).unwrap_or(u8::MAX),
        None => 0,
    }
}

/// Returns the number of columns a string occupies.
#[must_use]
pub fn str_width(s: &str) -> usize {
    pieces(s).fold(0usize, |total, piece| match piece {
        Piece::Text(text) => total.saturating_add(grapheme_width(text)),
        Piece::Escape(_) => total,
    })
}

/// Returns the columns occupied by one grapheme cluster.
///
/// A cluster that measures zero columns but holds a printable base character
/// counts as one. Terminals render such a cluster as a single cell, so
/// reporting zero would place the following text one column too early.
#[must_use]
pub fn grapheme_width(grapheme: &str) -> usize {
    let width = if ambiguous_wide() {
        UnicodeWidthStr::width_cjk(grapheme)
    } else {
        UnicodeWidthStr::width(grapheme)
    };
    if width > 0 {
        return width;
    }
    usize::from(grapheme.chars().any(is_base))
}

/// Returns true when a character should claim a column of its own.
///
/// A zero width space, a variation selector, and a zero width joiner are none
/// of them a base, so a cluster holding only those stays invisible.
fn is_base(c: char) -> bool {
    !c.is_control()
        && UnicodeWidthChar::width(c) != Some(0)
        && !matches!(c, '\u{200b}' | '\u{200d}' | '\u{fe0e}' | '\u{fe0f}')
}

/// Returns the grapheme clusters of a string.
pub fn graphemes(s: &str) -> impl Iterator<Item = &str> {
    s.graphemes(true)
}

/// One indivisible piece of a string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Piece<'a> {
    /// A complete escape sequence, which occupies no columns.
    Escape(&'a str),
    /// One grapheme cluster, or one control character.
    Text(&'a str),
}

/// Walks a string as escapes and graphemes.
fn pieces(s: &str) -> Pieces<'_> {
    Pieces { rest: s }
}

/// Iterator over the pieces of a string.
struct Pieces<'a> {
    rest: &'a str,
}

impl<'a> Iterator for Pieces<'a> {
    type Item = Piece<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.rest.as_bytes();
        let first = *bytes.first()?;
        if first == 0x1b {
            // An escape that never terminates inside the string is dropped
            // rather than passed on half formed.
            let Some((end, _)) = escape_at(bytes, 0) else {
                self.rest = "";
                return Some(Piece::Escape(""));
            };
            let piece = Piece::Escape(self.rest.get(..end).unwrap_or(self.rest));
            self.rest = self.rest.get(end..).unwrap_or("");
            return Some(piece);
        }
        let grapheme = self.rest.graphemes(true).next().unwrap_or(self.rest);
        let len = grapheme.len();
        let piece = Piece::Text(grapheme);
        self.rest = self.rest.get(len..).unwrap_or("");
        Some(piece)
    }
}

/// Truncates a string to at most `width` columns.
///
/// Returns the prefix and the columns it occupies. A cluster that would
/// straddle the edge is dropped whole rather than split, and an escape sequence
/// is never cut: a sequence that starts inside the retained prefix is retained
/// entire, and one that starts after it is not retained at all.
#[must_use]
pub fn truncate_to_width(s: &str, width: usize) -> (&str, usize) {
    let mut used = 0usize;
    let mut cut = 0usize;
    for piece in pieces(s) {
        match piece {
            Piece::Escape(escape) => cut = cut.saturating_add(escape.len()),
            Piece::Text(text) => {
                let cluster = grapheme_width(text);
                if used.saturating_add(cluster) > width {
                    break;
                }
                used = used.saturating_add(cluster);
                cut = cut.saturating_add(text.len());
            }
        }
    }
    (s.get(..cut).unwrap_or(s), used)
}

/// Wraps a string to `width` columns, reasserting style at every break.
///
/// A styled run that spans a break is closed at the end of the line it started
/// on and reopened at the start of the next, so each line is self-contained and
/// a reader can render one line without the others. A newline is a hard break.
/// Zero width input still yields one, empty line.
#[must_use]
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    wrap_styled(s, width)
        .into_iter()
        .map(|line| line.text)
        .collect()
}

/// One wrapped line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WrappedLine {
    /// The line, continuation escapes and any closing reset included.
    pub text: String,
    /// Columns occupied by the line's visible text.
    pub width: usize,
    /// The style open at the end of the line, before the closing reset.
    pub style: Style,
    /// The hyperlink target open at the end of the line, when there is one.
    ///
    /// Reported as the target itself rather than as an identifier, because a
    /// line rewritten on its own has no pool to resolve an identifier through.
    pub hyperlink: Option<String>,
}

/// Wraps a string and reports the style open at each break.
#[must_use]
pub fn wrap_styled(s: &str, width: usize) -> Vec<WrappedLine> {
    let mut out: Vec<WrappedLine> = Vec::new();
    let mut line = String::new();
    let mut line_width = 0usize;
    let mut open = Open::default();

    for piece in pieces(s) {
        match piece {
            Piece::Escape(escape) => {
                line.push_str(escape);
                open.apply(escape);
            }
            Piece::Text(text) => {
                // A carriage return and a newline are one grapheme cluster, so
                // a break is detected by character and not by whole cluster.
                if text.contains(['\r', '\n']) {
                    out.push(open.close(&mut line, line_width));
                    line_width = 0;
                    open.reopen(&mut line);
                    let carried: String = text
                        .chars()
                        .filter(|ch| !matches!(ch, '\r' | '\n'))
                        .collect();
                    if !carried.is_empty() {
                        line_width = line_width.saturating_add(grapheme_width(&carried));
                        line.push_str(&carried);
                    }
                    continue;
                }
                // A tab becomes the spaces up to the next stop, so it is
                // measured and broken on like any other space and never
                // reaches the terminal, which would move the cursor itself.
                let (text, repeat) = if text == "\t" {
                    (" ", tab_columns(line_width))
                } else {
                    (text, 1)
                };
                for _ in 0..repeat {
                    let cluster = grapheme_width(text);
                    if line_width > 0 && line_width.saturating_add(cluster) > width {
                        // Break on the last space that fits, so a word is moved
                        // to the next line rather than cut in half. A run
                        // longer than the width has no space to break on and is
                        // split, which is the only option that keeps it inside
                        // the terminal.
                        if let Some(at) = last_break(&line, width) {
                            let carried = line.split_off(at);
                            let carried = carried.trim_start_matches(' ');
                            let carried_width = str_width(carried);
                            out.push(open.close(&mut line, line_width));
                            line_width = 0;
                            open.reopen(&mut line);
                            line.push_str(carried);
                            line_width = line_width.saturating_add(carried_width);
                        } else {
                            out.push(open.close(&mut line, line_width));
                            line_width = 0;
                            open.reopen(&mut line);
                        }
                    }
                    line.push_str(text);
                    line_width = line_width.saturating_add(cluster);
                }
            }
        }
    }
    out.push(open.close(&mut line, line_width));
    out
}

/// Returns the spaces a tab at `column` expands to: up to the next stop, and
/// at least one.
const fn tab_columns(column: usize) -> usize {
    TAB_STOP.saturating_sub(column % TAB_STOP)
}

/// Replaces every tab with the spaces up to the next eight column stop.
///
/// Escape sequences occupy no columns, and a line break returns the column to
/// zero, so the result lines up the way a terminal would draw the tabs.
#[must_use]
pub fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut column = 0usize;
    for piece in pieces(text) {
        match piece {
            Piece::Escape(escape) => out.push_str(escape),
            Piece::Text("\t") => {
                let spaces = tab_columns(column);
                out.extend(std::iter::repeat_n(' ', spaces));
                column = column.saturating_add(spaces);
            }
            Piece::Text(text) if text.contains(['\r', '\n']) => {
                out.push_str(text);
                column = 0;
            }
            Piece::Text(text) => {
                out.push_str(text);
                column = column.saturating_add(grapheme_width(text));
            }
        }
    }
    out
}

/// Returns the byte offset to break a line at, keeping the word whole.
///
/// The last space that fits is the break, and the space itself is dropped: it
/// sat at the end of the line, so keeping it would leave a trailing blank the
/// reader cannot see. Returns `None` when no space fits, which is the case for
/// a single run longer than the line.
fn last_break(line: &str, width: usize) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut consumed = 0usize;
    let mut offset = 0usize;
    for piece in pieces(line) {
        if let Piece::Text(text) = piece {
            let next = consumed.saturating_add(grapheme_width(text));
            if next > width {
                break;
            }
            if text == " " && offset > 0 {
                best = Some(offset);
            }
            consumed = next;
        }
        let len = match piece {
            Piece::Escape(escape) | Piece::Text(escape) => escape.len(),
        };
        offset = offset.saturating_add(len);
    }
    best
}

/// The style state a wrapped line is currently in.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct Open {
    style: Style,
    /// The hyperlink target, owned so a break can reopen it without a rescan.
    link: Option<String>,
}

impl Open {
    /// Applies one escape sequence.
    fn apply(&mut self, escape: &str) {
        if let Some(params) = escape_sgr(escape) {
            self.style.apply_sgr(params);
            return;
        }
        match escape_link(escape) {
            // Text inside one link repeats no open sequence, so the target is
            // copied only when it actually changes.
            Some(Link::Open(target)) => {
                if self.link.as_deref() != Some(target) {
                    self.link = target.to_owned().into();
                }
            }
            Some(Link::Close) => self.link = None,
            None => {}
        }
    }

    /// Closes the open style and takes the finished line.
    ///
    /// The link is closed before any reset, so a reader that stops at the first
    /// reset still ends the line outside the link.
    fn close(&self, line: &mut String, width: usize) -> WrappedLine {
        if self.link.is_some() {
            line.push_str(Style::HYPERLINK_CLOSE);
        }
        if self.style.has_sgr() {
            line.push_str(Style::RESET);
        }
        WrappedLine {
            text: std::mem::take(line),
            width,
            style: self.style,
            hyperlink: self.link.clone(),
        }
    }

    /// Reopens the style a wrapped or broken line continues in.
    fn reopen(&self, line: &mut String) {
        if let Some(target) = &self.link {
            line.push_str("\u{1b}]8;;");
            line.push_str(target);
            line.push_str("\u{1b}\\");
        }
        if self.style.has_sgr() {
            line.push_str(&self.style.sgr());
        }
    }
}

/// An OSC 8 hyperlink operation.
enum Link<'a> {
    /// Opens a link to a target.
    Open(&'a str),
    /// Closes the open link.
    Close,
}

/// Returns the SGR parameters of an escape sequence, when it is an SGR one.
fn escape_sgr(escape: &str) -> Option<&str> {
    let body = escape.strip_prefix("\u{1b}[")?;
    let (params, final_byte) = body.split_at(body.len().checked_sub(1)?);
    if final_byte != "m" || params.starts_with(['?', '<', '=', '>']) {
        return None;
    }
    Some(params)
}

/// Returns the hyperlink operation of an escape sequence, when it is one.
fn escape_link(escape: &str) -> Option<Link<'_>> {
    let data = osc8_payload(escape)?;
    let target = data.split_once(';').map_or("", |(_, target)| target);
    if target.is_empty() {
        return Some(Link::Close);
    }
    Some(Link::Open(target))
}

/// Returns the payload of an OSC 8 escape sequence.
fn osc8_payload(escape: &str) -> Option<&str> {
    let body = escape.strip_prefix("\u{1b}]8;")?;
    let body = body
        .strip_suffix('\u{7}')
        .or_else(|| body.strip_suffix("\u{1b}\\"))?;
    Some(body)
}

/// Returns the end offset and kind of the escape sequence at `at`.
fn escape_at(bytes: &[u8], at: usize) -> Option<(usize, u8)> {
    let mut index = at.checked_add(1)?;
    let introducer = *bytes.get(index)?;
    index = index.checked_add(1)?;
    match introducer {
        b'[' => {
            while let Some(byte) = bytes.get(index) {
                if (0x40..=0x7e).contains(byte) {
                    return Some((index.checked_add(1)?, b'['));
                }
                index = index.checked_add(1)?;
            }
            None
        }
        b']' => {
            while let Some(byte) = bytes.get(index) {
                if *byte == 0x07 {
                    return Some((index.checked_add(1)?, b']'));
                }
                if *byte == 0x1b && bytes.get(index.checked_add(1)?) == Some(&b'\\') {
                    return Some((index.checked_add(2)?, b']'));
                }
                index = index.checked_add(1)?;
            }
            None
        }
        _ => Some((index, introducer)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_have_no_width() {
        assert_eq!(char_width('\u{7}'), 0);
        assert_eq!(char_width('\t'), 0);
        assert_eq!(char_width('\n'), 0);
        assert_eq!(char_width('\u{301}'), 0);
    }

    #[test]
    fn cjk_occupies_two_columns() {
        assert_eq!(char_width('中'), 2);
        assert_eq!(str_width("中文"), 4);
    }

    #[test]
    fn combining_mark_attaches_to_its_base() {
        assert_eq!(str_width("e\u{301}"), 1);
        let clusters: Vec<&str> = graphemes("e\u{301}x").collect();
        assert_eq!(clusters, vec!["e\u{301}", "x"]);
        assert_eq!(grapheme_width(clusters[0]), 1);
        assert_eq!(grapheme_width("x"), 1);
    }

    #[test]
    fn a_lone_combining_mark_is_invisible() {
        assert_eq!(grapheme_width("\u{301}"), 0);
        assert_eq!(str_width("\u{301}"), 0);
    }

    #[test]
    fn joined_emoji_measure_two_columns() {
        assert_eq!(str_width("👨‍👩‍👧"), 2);
        assert_eq!(str_width("👋🏽"), 2);
        assert_eq!(str_width("🇺🇸"), 2);
        assert_eq!(graphemes("a👨‍👩‍👧b").count(), 3);
    }

    #[test]
    fn zero_width_space_costs_nothing() {
        assert_eq!(str_width("a\u{200b}b"), 2);
    }

    #[test]
    fn escapes_cost_no_columns() {
        assert_eq!(str_width("\u{1b}[31mred\u{1b}[0m"), 3);
    }

    #[test]
    fn truncate_never_splits_a_grapheme() {
        let (text, width) = truncate_to_width("中文x", 3);
        assert_eq!(text, "中");
        assert_eq!(width, 2);
        let (text, width) = truncate_to_width("e\u{301}x", 1);
        assert_eq!(text, "e\u{301}");
        assert_eq!(width, 1);
    }

    #[test]
    fn truncate_never_splits_an_escape() {
        let (text, width) = truncate_to_width("\u{1b}[31mab\u{1b}[0m", 1);
        assert_eq!(text, "\u{1b}[31ma");
        assert_eq!(width, 1);
        let (text, width) = truncate_to_width("\u{1b}[38;5;200mab", 2);
        assert_eq!(text, "\u{1b}[38;5;200mab");
        assert_eq!(width, 2);
    }

    #[test]
    fn truncate_past_the_end_returns_everything() {
        let (text, width) = truncate_to_width("abc", 8);
        assert_eq!(text, "abc");
        assert_eq!(width, 3);
    }

    #[test]
    fn wrapping_splits_on_columns() {
        assert_eq!(wrap("abcdef", 3), vec!["abc", "def"]);
    }

    #[test]
    fn wrapping_breaks_between_words_rather_than_inside_one() {
        // A break at the column leaves half a word on each line, which is what
        // makes model output look mangled: `you` becomes `yo` and `u`.
        let lines = wrap("hello world again", 12);
        assert_eq!(lines, vec!["hello world", "again"]);
        for line in &lines {
            assert!(str_width(line) <= 12, "{line:?} overflowed");
        }
        // No line starts or ends in the middle of a word.
        assert!(!lines[1].starts_with('u'), "{lines:?}");
    }

    #[test]
    fn a_word_longer_than_the_line_is_still_split() {
        // There is no space to break on, and leaving it whole would overflow the
        // terminal. Splitting is the only option that keeps the row inside.
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn a_break_drops_the_space_it_lands_on() {
        // Keeping it would leave an invisible trailing blank on the line.
        let lines = wrap("ab cd", 3);
        assert_eq!(lines, vec!["ab", "cd"]);
    }

    #[test]
    fn wrapping_a_single_word_shorter_than_the_line_leaves_it_alone() {
        assert_eq!(wrap("hello", 10), vec!["hello"]);
    }

    #[test]
    fn wrapping_cjk_counts_columns_not_codepoints() {
        assert_eq!(wrap("中文中文", 5), vec!["中文", "中文"]);
        assert_eq!(wrap("中文中文", 4), vec!["中文", "中文"]);
    }

    #[test]
    fn wrapping_an_unstyled_line_adds_nothing() {
        assert_eq!(wrap("abc", 10), vec!["abc"]);
        assert_eq!(wrap("", 10), vec![String::new()]);
    }

    #[test]
    fn wrapping_reasserts_style_on_both_lines() {
        let lines = wrap("\u{1b}[1;31mabcdef\u{1b}[0m", 3);
        assert_eq!(lines.len(), 2);
        // Each line is self-contained: the first closes the style it opened,
        // the second reopens it in canonical form.
        assert_eq!(lines[0], "\u{1b}[1;31mabc\u{1b}[0m");
        assert_eq!(lines[1], "\u{1b}[1;31mdef\u{1b}[0m");
        // Replaying both lines in order ends with the style cleared, which is
        // what a reader sees when it prints one after the other.
        let mut replay = Open::default();
        for line in &lines {
            for piece in pieces(line) {
                if let Piece::Escape(escape) = piece {
                    replay.apply(escape);
                }
            }
        }
        assert!(!replay.style.has_sgr());
        assert!(replay.link.is_none());
    }

    #[test]
    fn wrapping_keeps_a_plain_segment_plain() {
        let lines = wrap("ab\u{1b}[31mcd", 2);
        // The colour opens before the break, so the first line closes it and
        // the second reopens it rather than leaving either half styled.
        assert_eq!(
            lines,
            vec!["ab\u{1b}[31m\u{1b}[0m", "\u{1b}[31mcd\u{1b}[0m"]
        );
    }

    #[test]
    fn wrapping_carries_a_hyperlink_across_the_break() {
        let lines = wrap("\u{1b}]8;;https://a.test\u{1b}\\abcdef", 3);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "\u{1b}]8;;https://a.test\u{1b}\\abc\u{1b}]8;;\u{1b}\\"
        );
        assert_eq!(
            lines[1],
            "\u{1b}]8;;https://a.test\u{1b}\\def\u{1b}]8;;\u{1b}\\"
        );
    }

    #[test]
    fn a_carriage_return_newline_is_one_break() {
        assert_eq!(wrap("ab\r\ncd", 8), vec!["ab", "cd"]);
        assert_eq!(wrap("ab\rcd", 8), vec!["ab", "cd"]);
        assert_eq!(wrap("a\nb\r\nc", 8), vec!["a", "b", "c"]);
    }

    #[test]
    fn a_wrapped_line_reports_its_open_link() {
        let lines = wrap_styled("\u{1b}]8;;https://a.test\u{1b}\\abc", 2);
        assert_eq!(lines[0].hyperlink.as_deref(), Some("https://a.test"));
        assert_eq!(lines[1].hyperlink.as_deref(), Some("https://a.test"));
        let plain = wrap_styled("abc", 2);
        assert!(plain.iter().all(|line| line.hyperlink.is_none()));
    }

    #[test]
    fn a_newline_is_a_hard_break() {
        assert_eq!(wrap("ab\ncd", 8), vec!["ab", "cd"]);
        assert_eq!(wrap("ab\n\ncd", 8), vec!["ab", "", "cd"]);
    }

    #[test]
    fn an_unterminated_escape_is_dropped() {
        assert_eq!(str_width("a\u{1b}[3"), 1);
        assert_eq!(wrap("a\u{1b}[3", 8), vec!["a"]);
    }

    #[test]
    fn a_wide_glyph_wider_than_the_line_still_wraps_whole() {
        let lines = wrap("中文", 1);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "中");
        assert_eq!(lines[1], "文");
    }

    /// Removes every escape sequence, leaving only the visible text.
    fn visible(line: &str) -> String {
        pieces(line)
            .filter_map(|piece| match piece {
                Piece::Text(text) => Some(text),
                Piece::Escape(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_styled_line_breaks_where_the_plain_line_does() {
        let plain = wrap("hello world foo", 11);
        let styled = wrap("\u{1b}[1mhello\u{1b}[0m world foo", 11);
        let shown: Vec<String> = styled.iter().map(|line| visible(line)).collect();
        assert_eq!(shown, plain);
        assert_eq!(shown[0], "hello");

        // The break offset is the space itself, even with an escape right
        // before it.
        let line = "\u{1b}[1mhello\u{1b}[0m world";
        assert_eq!(last_break(line, 11), line.find(' '));
        let line = "\u{1b}]8;;https://a.test\u{1b}\\hello\u{1b}]8;;\u{1b}\\ world";
        assert_eq!(last_break(line, 11), line.find(' '));

        // A colour opened just before the breaking space is reopened, and
        // only it, on the next row.
        let lines = wrap("hello\u{1b}[31m world foo", 11);
        assert_eq!(lines[0], "hello\u{1b}[31m\u{1b}[0m");
        assert_eq!(lines[1], "\u{1b}[31mworld foo\u{1b}[0m");

        // A style that spans the break reopens unchanged.
        let lines = wrap("\u{1b}[1mhello world foo\u{1b}[0m", 11);
        assert_eq!(lines[0], "\u{1b}[1mhello\u{1b}[0m");
        assert_eq!(lines[1], "\u{1b}[1mworld foo\u{1b}[0m");

        // A link that spans the break reopens on the next row.
        let lines = wrap("\u{1b}]8;;https://a.test\u{1b}\\hello world foo", 11);
        let shown: Vec<String> = lines.iter().map(|line| visible(line)).collect();
        assert_eq!(shown, plain);
        assert!(lines[1].starts_with("\u{1b}]8;;https://a.test\u{1b}\\world"));

        // Long SGR parameters no longer push the break earlier.
        let plain = wrap("a bb ccc dddd", 9);
        let styled = wrap("\u{1b}[38;2;10;20;30ma bb\u{1b}[0m ccc dddd", 9);
        let shown: Vec<String> = styled.iter().map(|line| visible(line)).collect();
        assert_eq!(shown, plain);
        assert_eq!(plain, vec!["a bb ccc", "dddd"]);
    }

    #[test]
    fn a_line_of_escapes_and_one_word_has_no_break() {
        let line = "\u{1b}[1m\u{1b}]8;;https://a.test\u{1b}\\abcdefgh";
        assert_eq!(last_break(line, 4), None);
        let lines = wrap("\u{1b}[1mabcdefgh\u{1b}[0m", 4);
        let shown: Vec<String> = lines.iter().map(|line| visible(line)).collect();
        assert_eq!(shown, vec!["abcd", "efgh"]);
        // A space at the very start is never a break.
        assert_eq!(last_break(" abc", 4), None);
        assert_eq!(last_break("\u{1b}[1m abc", 4), Some(4));
    }

    #[test]
    fn grapheme_widths_match_the_table() {
        let table: &[(&str, &str, usize)] = &[
            ("family", "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}", 2),
            ("flag", "\u{1F1EF}\u{1F1F5}", 2),
            ("skin tone", "\u{1F44D}\u{1F3FD}", 2),
            ("emoji presentation", "\u{2764}\u{FE0F}", 2),
            ("text presentation", "\u{231A}\u{FE0E}", 1),
            ("combining", "e\u{301}", 1),
            ("cjk", "漢字", 4),
            ("fullwidth", "\u{FF21}", 2),
            ("ambiguous", "\u{25CB}", 1),
            ("box drawing", "\u{2500}", 1),
        ];
        for (name, text, columns) in table {
            assert_eq!(str_width(text), *columns, "{name}");
            let sum: usize = graphemes(text).map(grapheme_width).sum();
            assert_eq!(sum, *columns, "{name} by cluster");
            // Escapes around a sequence add nothing to it.
            let styled = format!("\u{1b}[1m{text}\u{1b}]8;;https://a.test\u{1b}\\\u{1b}[0m");
            assert_eq!(str_width(&styled), *columns, "{name} styled");
        }
        for text in table
            .iter()
            .map(|(_, text, _)| text)
            .filter(|text| **text != "漢字")
        {
            assert_eq!(graphemes(text).count(), 1, "{text} is one cluster");
        }
        // A lone regional indicator is a cluster of its own, and three make a
        // flag and a leftover.
        assert_eq!(graphemes("\u{1F1EF}\u{1F1F5}\u{1F1EF}").count(), 2);
        // Wrapping never splits a sequence.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let lines = wrap(&format!("a{family}{family}"), 3);
        assert_eq!(lines, vec![format!("a{family}"), family.to_owned()]);
    }

    thread_local! {
        /// Whether this test thread measures with the shared setting.
        pub(super) static SEES_AMBIGUOUS_WIDTH: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }

    /// Restores the default ambiguous width when dropped.
    struct Reset;

    impl Drop for Reset {
        fn drop(&mut self) {
            set_ambiguous_width(1);
            SEES_AMBIGUOUS_WIDTH.with(|sees| sees.set(false));
        }
    }

    #[test]
    fn ambiguous_characters_widen_when_asked() {
        SEES_AMBIGUOUS_WIDTH.with(|sees| sees.set(true));
        let _reset = Reset;
        set_ambiguous_width(1);
        assert_eq!(str_width("\u{25CB}"), 1);
        assert_eq!(char_width('\u{25CB}'), 1);

        set_ambiguous_width(2);
        assert_eq!(str_width("\u{25CB}"), 2);
        assert_eq!(char_width('\u{25CB}'), 2);
        assert_eq!(str_width("\u{2500}"), 2);
        for glyph in ["\u{258C}", "\u{2500}", "\u{2026}", "\u{2022}"] {
            assert_eq!(str_width(glyph), 2, "{glyph}");
        }
        // Clipping and wrapping still keep wide chrome inside the line.
        let (cut, used) = truncate_to_width("\u{2500}\u{2500}\u{2500}", 5);
        assert_eq!((cut, used), ("\u{2500}\u{2500}", 4));
        let lines = wrap_styled("\u{2022} \u{2026}\u{2026}\u{2026}", 5);
        assert!(lines.iter().all(|line| line.width <= 5), "{lines:?}");
        // Wide and narrow characters are not ambiguous and do not change.
        assert_eq!(str_width("漢"), 2);
        assert_eq!(str_width("a"), 1);

        // Any value other than two is the default.
        set_ambiguous_width(3);
        assert_eq!(str_width("\u{25CB}"), 1);
        set_ambiguous_width(0);
        assert_eq!(str_width("\u{25CB}"), 1);
    }

    #[test]
    fn tabs_expand_to_eight_column_stops_when_wrapping() {
        for column in 0..=17 {
            let text = format!("{}\t|", "a".repeat(column));
            let lines = wrap(&text, 40);
            assert_eq!(lines.len(), 1, "{column}");
            assert!(!lines[0].contains('\t'), "{column}");
            let stop = (column / 8 + 1) * 8;
            assert_eq!(lines[0].find('|'), Some(stop), "{column}");
            assert_eq!(str_width(&lines[0]), stop + 1, "{column}");
        }
        assert_eq!(wrap("abcdefg\tx", 20), vec!["abcdefg x"]);
        assert_eq!(wrap("abcdefgh\tx", 20), vec!["abcdefgh        x"]);
        // Escapes before a tab take no columns.
        assert_eq!(
            visible(&wrap("\u{1b}[1mab\u{1b}[0m\tc", 20)[0]),
            "ab      c"
        );
        // Wide characters count both columns.
        assert_eq!(wrap("漢\tx", 20), vec!["漢      x"]);
        // The spaces are columns and a place to break.
        let lines = wrap_styled("abc\tdefghij", 12);
        assert_eq!(lines[1].text, "defghij");
        assert!(
            lines
                .iter()
                .all(|line| line.width <= 12 && !line.text.contains('\t'))
        );
        // A tab after a newline counts from the start of the new row.
        assert_eq!(wrap("abc\n\tx", 20), vec!["abc", "        x"]);
        // A tab past the edge never overflows the row.
        let lines = wrap_styled("abcdef\tx", 7);
        assert!(
            lines
                .iter()
                .all(|line| line.width <= 7 && !line.text.contains('\t'))
        );
    }

    #[test]
    fn expand_tabs_resets_after_a_newline() {
        assert_eq!(expand_tabs("ab\tc\n\td"), "ab      c\n        d");
        assert_eq!(expand_tabs("ab\tc\r\n\td"), "ab      c\r\n        d");
        assert_eq!(expand_tabs("abcdefg\t."), "abcdefg .");
        assert_eq!(expand_tabs("abcdefgh\t."), "abcdefgh        .");
        assert_eq!(expand_tabs("\t\t."), format!("{}.", " ".repeat(16)));
        assert_eq!(
            expand_tabs("\u{1b}[31mab\u{1b}[0m\tc"),
            "\u{1b}[31mab\u{1b}[0m      c"
        );
        assert_eq!(expand_tabs("e\u{301}\t."), "e\u{301}       .");
        assert_eq!(expand_tabs("no tabs"), "no tabs");
    }

    #[test]
    fn style_state_round_trips_through_the_wrap() {
        let lines = wrap_styled("\u{1b}[4mabc", 2);
        assert_eq!(lines[0].width, 2);
        assert_eq!(lines[1].width, 1);
        assert!(lines[0].style.has_flag(crate::engine::flag::UNDERLINE));
        assert!(lines[1].style.has_flag(crate::engine::flag::UNDERLINE));
    }
}
