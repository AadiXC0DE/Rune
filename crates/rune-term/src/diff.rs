//! A short, coloured view of what an edit changed.
//!
//! Shown under an edit's tool row so the reader sees the change, not only its
//! line counts. Lines both sides share at the start and end are left out, the
//! rest is marked `-` and `+`, and the view is cut to a few lines so a large
//! edit cannot flood the scrollback. Colours come from the theme, so a theme
//! without them draws the marks alone.

use crate::transcript::sanitize;
use crate::width::{str_width, truncate_to_width};

/// Escape sequences for each kind of line; empty strings draw no colour.
#[derive(Clone, Debug, Default)]
pub struct DiffStyle {
    /// Added lines.
    pub added: String,
    /// Removed lines.
    pub removed: String,
    /// The note that lines were left out.
    pub dim: String,
    /// Ends a styled span.
    pub reset: String,
}

/// Returns the lines one replacement removes and adds, marked `-` and `+`.
///
/// Lines shared at the start and the end are trimmed, so a one-line change
/// inside a large replacement shows as that one line.
#[must_use]
pub fn changed_lines<'a>(old: &'a str, new: &'a str) -> Vec<(char, &'a str)> {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let start = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let end = old[start..]
        .iter()
        .rev()
        .zip(new[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let removed = &old[start..old.len().saturating_sub(end)];
    let added = &new[start..new.len().saturating_sub(end)];
    removed
        .iter()
        .map(|line| ('-', *line))
        .chain(added.iter().map(|line| ('+', *line)))
        .collect()
}

/// Draws marked lines indented under a tool row, at most `max_lines` of them,
/// each cut to `width` columns.
#[must_use]
pub fn render(
    lines: &[(char, &str)],
    style: &DiffStyle,
    max_lines: usize,
    width: usize,
    ascii: bool,
) -> Vec<String> {
    let indent = "    ";
    let room = width.saturating_sub(indent.len().saturating_add(2)).max(8);
    let ellipsis = if ascii { "..." } else { "\u{2026}" };
    let mut out: Vec<String> = lines
        .iter()
        .take(max_lines)
        .map(|(mark, text)| {
            let text = sanitize(&text.replace('\t', "    "));
            let shown = if str_width(&text) > room {
                let (cut, _) = truncate_to_width(&text, room.saturating_sub(str_width(ellipsis)));
                format!("{cut}{ellipsis}")
            } else {
                text
            };
            let colour = if *mark == '+' {
                &style.added
            } else {
                &style.removed
            };
            format!("{indent}{colour}{mark} {shown}{}", style.reset)
        })
        .collect();
    let more = lines.len().saturating_sub(max_lines);
    if more > 0 {
        let noun = if more == 1 { "line" } else { "lines" };
        out.push(format!(
            "{indent}{}{ellipsis} {more} more changed {noun}{}",
            style.dim, style.reset
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_lines_are_trimmed_from_both_ends() {
        let old = "a\nb\nc\nd";
        let new = "a\nB\nc\nd";
        assert_eq!(changed_lines(old, new), [('-', "b"), ('+', "B")]);
        assert_eq!(changed_lines("x", "x\ny"), [('+', "y")]);
        assert!(changed_lines("same", "same").is_empty());
    }

    #[test]
    fn the_view_is_cut_in_lines_and_columns_and_says_so() {
        let lines = [
            ('-', "one"),
            ('+', "two two two two two two two"),
            ('+', "three"),
        ];
        let style = DiffStyle {
            added: "<g>".to_owned(),
            removed: "<r>".to_owned(),
            dim: "<d>".to_owned(),
            reset: "</>".to_owned(),
        };
        let out = render(&lines, &style, 2, 20, true);
        assert_eq!(out[0], "    <r>- one</>");
        assert_eq!(out[1], "    <g>+ two two two...</>");
        assert_eq!(out[2], "    <d>... 1 more changed line</>");
    }
}
