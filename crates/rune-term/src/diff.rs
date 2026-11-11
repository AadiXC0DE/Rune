//! A short, coloured view of what an edit changed.
//!
//! Shown under an edit's tool row so the reader sees the change, not only its
//! line counts. The change is a line diff cut into numbered hunks, removed
//! lines are marked `-` and added lines `+`, and the view is cut to a few
//! lines so a large edit cannot flood the scrollback. Colours come from the
//! theme, so a theme without them draws the marks alone.

use std::ops::Range;

use crate::transcript::sanitize;
use crate::width::{expand_tabs, str_width, truncate_to_width};

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

/// How a line of a diff relates the two texts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// A line both texts share.
    Context,
    /// A line only the old text has.
    Removed,
    /// A line only the new text has.
    Added,
}

/// One line of a hunk with its numbers in the old and the new text.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiffLine<'a> {
    /// How the line relates the two texts.
    pub op: Op,
    /// Its number in the old text, absent for an added line.
    pub old: Option<usize>,
    /// Its number in the new text, absent for a removed line.
    pub new: Option<usize>,
    /// The line without its line break.
    pub text: &'a str,
}

/// A run of changed lines with the shared lines around them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hunk<'a> {
    /// The lines in order; removals come before additions in each change.
    pub lines: Vec<DiffLine<'a>>,
}

/// Shared lines kept on each side of a change.
pub const CONTEXT_LINES: usize = 3;

/// Above this many lines on either side the diff is not searched for.
const MAX_LINES: usize = 20_000;

/// Above this many removed and added lines the search stops.
const MAX_EDITS: usize = 4_000;

/// Returns the hunks that turn `old` into `new`, numbered from `first_line`.
///
/// The changes are a shortest edit script over lines, each kept with
/// [`CONTEXT_LINES`] shared lines on both sides; hunks whose context would
/// touch are merged. An input too large to search becomes one hunk of the
/// lines between the shared start and end.
#[must_use]
pub fn hunks<'a>(old: &'a str, new: &'a str, first_line: usize) -> Vec<Hunk<'a>> {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let script = if old.len() > MAX_LINES || new.len() > MAX_LINES {
        None
    } else {
        edit_script(&old, &new)
    };
    match script {
        Some(script) => group(&number(&old, &new, &script, first_line)),
        None => trimmed_hunk(&old, &new, first_line),
    }
}

/// The lines between the start and the end both texts share, as one hunk.
fn trimmed_hunk<'a>(old: &[&'a str], new: &[&'a str], first_line: usize) -> Vec<Hunk<'a>> {
    let start = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let end = old[start..]
        .iter()
        .rev()
        .zip(new[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let removed = &old[start..old.len().saturating_sub(end)];
    let added = &new[start..new.len().saturating_sub(end)];
    if removed.is_empty() && added.is_empty() {
        return Vec::new();
    }
    let at = |index: usize| Some(first_line.saturating_add(start).saturating_add(index));
    let lines = removed
        .iter()
        .enumerate()
        .map(|(index, text)| DiffLine {
            op: Op::Removed,
            old: at(index),
            new: None,
            text,
        })
        .chain(added.iter().enumerate().map(|(index, text)| DiffLine {
            op: Op::Added,
            old: None,
            new: at(index),
            text,
        }))
        .collect();
    vec![Hunk { lines }]
}

/// Gives each step of a script its text and line numbers.
fn number<'a>(
    old: &[&'a str],
    new: &[&'a str],
    script: &[Op],
    first_line: usize,
) -> Vec<DiffLine<'a>> {
    let (mut at_old, mut at_new) = (0_usize, 0_usize);
    let mut lines = Vec::with_capacity(script.len());
    for &op in script {
        let line = |index: usize| first_line.saturating_add(index);
        let (text, old_number, new_number) = match op {
            Op::Context => (old.get(at_old), Some(line(at_old)), Some(line(at_new))),
            Op::Removed => (old.get(at_old), Some(line(at_old)), None),
            Op::Added => (new.get(at_new), None, Some(line(at_new))),
        };
        if let Some(text) = text {
            lines.push(DiffLine {
                op,
                old: old_number,
                new: new_number,
                text,
            });
        }
        if op != Op::Added {
            at_old = at_old.saturating_add(1);
        }
        if op != Op::Removed {
            at_new = at_new.saturating_add(1);
        }
    }
    lines
}

/// Cuts numbered lines into hunks with context around each change.
fn group<'a>(lines: &[DiffLine<'a>]) -> Vec<Hunk<'a>> {
    let changed: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.op != Op::Context)
        .map(|(index, _)| index)
        .collect();
    let Some((&first, rest)) = changed.split_first() else {
        return Vec::new();
    };
    let last_line = lines.len().saturating_sub(1);
    let hunk = |from: usize, to: usize| Hunk {
        lines: lines[from..=to.saturating_add(CONTEXT_LINES).min(last_line)].to_vec(),
    };
    let mut hunks = Vec::new();
    let mut start = first.saturating_sub(CONTEXT_LINES);
    let mut end = first;
    for &index in rest {
        // More shared lines between two changes than both contexts hold.
        if index.saturating_sub(end) > CONTEXT_LINES.saturating_mul(2).saturating_add(1) {
            hunks.push(hunk(start, end));
            start = index.saturating_sub(CONTEXT_LINES);
        }
        end = index;
    }
    hunks.push(hunk(start, end));
    hunks
}

/// Byte ranges into one line.
type Ranges = Vec<Range<usize>>;

/// Returns the byte ranges of the words that differ between a removed and an
/// added line, or `None` when the lines are too unlike for that to help.
///
/// Lines are split into runs of letters, digits and `_`, runs of whitespace,
/// and single other characters. Lines are alike when the tokens they share,
/// whitespace aside, are at least two fifths of all their tokens.
#[must_use]
pub fn word_ranges(removed: &str, added: &str) -> Option<(Ranges, Ranges)> {
    let old = tokens(removed);
    let new = tokens(added);
    if old.len() > MAX_LINES || new.len() > MAX_LINES {
        return None;
    }
    let old_text: Vec<&str> = old.iter().map(|range| &removed[range.clone()]).collect();
    let new_text: Vec<&str> = new.iter().map(|range| &added[range.clone()]).collect();
    let script = edit_script(&old_text, &new_text)?;
    let words = |texts: &[&str]| texts.iter().filter(|text| !is_space(text)).count();
    let total = words(&old_text).saturating_add(words(&new_text));
    let (mut at_old, mut at_new, mut common) = (0_usize, 0_usize, 0_usize);
    let (mut old_ranges, mut new_ranges) = (Vec::new(), Vec::new());
    for op in script {
        match op {
            Op::Context => {
                if old_text.get(at_old).is_some_and(|text| !is_space(text)) {
                    common = common.saturating_add(1);
                }
                at_old = at_old.saturating_add(1);
                at_new = at_new.saturating_add(1);
            }
            Op::Removed => {
                push_merged(&mut old_ranges, old.get(at_old));
                at_old = at_old.saturating_add(1);
            }
            Op::Added => {
                push_merged(&mut new_ranges, new.get(at_new));
                at_new = at_new.saturating_add(1);
            }
        }
    }
    // 2 * common / total >= 0.4, in integers.
    (total > 0 && common.saturating_mul(5) >= total).then_some((old_ranges, new_ranges))
}

/// Returns the changed words of each line, empty for a shared line, a line
/// with no partner and a pair too unlike to compare.
///
/// In each run of changes the first removed line is paired with the first
/// added line, the second with the second, and so on.
#[must_use]
pub fn word_highlights(lines: &[DiffLine<'_>]) -> Vec<Vec<Range<usize>>> {
    let mut out = vec![Vec::new(); lines.len()];
    let mut start = 0;
    while start < lines.len() {
        let run = lines[start..]
            .iter()
            .take_while(|line| line.op != Op::Context)
            .count();
        if run == 0 {
            start = start.saturating_add(1);
            continue;
        }
        let removed = lines[start..start.saturating_add(run)]
            .iter()
            .take_while(|line| line.op == Op::Removed)
            .count();
        let added = run.saturating_sub(removed);
        for pair in 0..removed.min(added) {
            let old = start.saturating_add(pair);
            let new = start.saturating_add(removed).saturating_add(pair);
            if let Some((old_ranges, new_ranges)) = word_ranges(lines[old].text, lines[new].text) {
                out[old] = old_ranges;
                out[new] = new_ranges;
            }
        }
        start = start.saturating_add(run);
    }
    out
}

/// Splits a line into word, whitespace and single-character tokens.
fn tokens(line: &str) -> Vec<Range<usize>> {
    #[derive(PartialEq)]
    enum Kind {
        Word,
        Space,
        Other,
    }
    let kind = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            Kind::Word
        } else if c.is_whitespace() {
            Kind::Space
        } else {
            Kind::Other
        }
    };
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut last = None;
    for (at, c) in line.char_indices() {
        let this = kind(c);
        let end = at.saturating_add(c.len_utf8());
        match out.last_mut() {
            Some(range) if this != Kind::Other && last.as_ref() == Some(&this) => range.end = end,
            _ => out.push(at..end),
        }
        last = Some(this);
    }
    out
}

fn is_space(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}

/// Adds a token's range, joining it to the previous one when they touch.
fn push_merged(ranges: &mut Vec<Range<usize>>, range: Option<&Range<usize>>) {
    let Some(range) = range else {
        return;
    };
    match ranges.last_mut() {
        Some(last) if last.end == range.start => last.end = range.end,
        _ => ranges.push(range.clone()),
    }
}

/// Returns a shortest edit script from `old` to `new`, removals first in
/// each change, or `None` when it needs more than [`MAX_EDITS`] steps.
fn edit_script(old: &[&str], new: &[&str]) -> Option<Vec<Op>> {
    let mut steps = Vec::with_capacity(old.len().max(new.len()));
    let search = Search { old, new };
    if !search.walk(0, old.len(), 0, new.len(), Some(MAX_EDITS), &mut steps) {
        return None;
    }
    let mut script = Vec::with_capacity(steps.len());
    let (mut removed, mut added) = (0_usize, 0_usize);
    for op in steps.into_iter().chain(std::iter::once(Op::Context)) {
        match op {
            Op::Removed => removed = removed.saturating_add(1),
            Op::Added => added = added.saturating_add(1),
            Op::Context => {
                script.extend(std::iter::repeat_n(Op::Removed, removed));
                script.extend(std::iter::repeat_n(Op::Added, added));
                script.push(Op::Context);
                (removed, added) = (0, 0);
            }
        }
    }
    script.pop();
    Some(script)
}

/// Myers' O(ND) search over two line slices, in linear space.
struct Search<'s, 'a> {
    old: &'s [&'a str],
    new: &'s [&'a str],
}

impl Search<'_, '_> {
    /// Appends the steps from `old[a0..a1]` to `new[b0..b1]`; false when the
    /// first split needs more than `limit` edits.
    fn walk(
        &self,
        mut a0: usize,
        mut a1: usize,
        mut b0: usize,
        mut b1: usize,
        limit: Option<usize>,
        steps: &mut Vec<Op>,
    ) -> bool {
        let mut prefix = 0_usize;
        while a0 < a1 && b0 < b1 && self.old[a0] == self.new[b0] {
            (a0, b0, prefix) = (
                a0.saturating_add(1),
                b0.saturating_add(1),
                prefix.saturating_add(1),
            );
        }
        let mut suffix = 0_usize;
        while a0 < a1 && b0 < b1 && self.old[a1.saturating_sub(1)] == self.new[b1.saturating_sub(1)]
        {
            (a1, b1, suffix) = (
                a1.saturating_sub(1),
                b1.saturating_sub(1),
                suffix.saturating_add(1),
            );
        }
        steps.extend(std::iter::repeat_n(Op::Context, prefix));
        if a0 == a1 {
            steps.extend(std::iter::repeat_n(Op::Added, b1.saturating_sub(b0)));
        } else if b0 == b1 {
            steps.extend(std::iter::repeat_n(Op::Removed, a1.saturating_sub(a0)));
        } else {
            let Some((x, y)) = self.split(a0, a1, b0, b1, limit) else {
                return false;
            };
            if !self.walk(a0, x, b0, y, None, steps) || !self.walk(x, a1, y, b1, None, steps) {
                return false;
            }
        }
        steps.extend(std::iter::repeat_n(Op::Context, suffix));
        true
    }

    /// Returns a point on a shortest path through the middle of the edit
    /// graph of two slices that share no first and no last line, searching
    /// forward from the start and backward from the end at once.
    // Positions and diagonals are bounded by the line cap, far from overflow.
    #[allow(clippy::arithmetic_side_effects)]
    fn split(
        &self,
        a0: usize,
        a1: usize,
        b0: usize,
        b1: usize,
        limit: Option<usize>,
    ) -> Option<(usize, usize)> {
        let old_len = isize::try_from(a1 - a0).ok()?;
        let new_len = isize::try_from(b1 - b0).ok()?;
        let most = (old_len + new_len + 1) / 2;
        let rounds = limit.map_or(most, |limit| {
            most.min(isize::try_from(limit / 2).unwrap_or(most))
        });
        let offset = rounds + 1;
        let size = usize::try_from(2 * rounds + 3).ok()?;
        let mut forward = vec![-1_isize; size];
        let mut backward = vec![-1_isize; size];
        let at = |diagonal: isize| (offset + diagonal) as usize;
        forward[at(1)] = 0;
        backward[at(1)] = 0;
        let delta = old_len - new_len;
        let odd = delta % 2 != 0;
        let same = |x: isize, y: isize| self.old[a0 + x as usize] == self.new[b0 + y as usize];
        let same_back = |x: isize, y: isize| {
            self.old[a0 + (old_len - x - 1) as usize] == self.new[b0 + (new_len - y - 1) as usize]
        };
        let (mut f_start, mut f_end, mut b_start, mut b_end) = (0, 0, 0, 0);
        for round in 0..=rounds {
            let mut diagonal = -round + f_start;
            while diagonal <= round - f_end {
                let mut x = if diagonal == -round
                    || (diagonal != round && forward[at(diagonal - 1)] < forward[at(diagonal + 1)])
                {
                    forward[at(diagonal + 1)]
                } else {
                    forward[at(diagonal - 1)] + 1
                };
                let mut y = x - diagonal;
                while x < old_len && y < new_len && same(x, y) {
                    (x, y) = (x + 1, y + 1);
                }
                forward[at(diagonal)] = x;
                if x > old_len {
                    f_end += 2;
                } else if y > new_len {
                    f_start += 2;
                } else if odd {
                    let back = delta - diagonal;
                    if back.abs() <= rounds + 1
                        && backward[at(back)] != -1
                        && x >= old_len - backward[at(back)]
                    {
                        return Some((a0 + x as usize, b0 + y as usize));
                    }
                }
                diagonal += 2;
            }
            let mut diagonal = -round + b_start;
            while diagonal <= round - b_end {
                let mut x = if diagonal == -round
                    || (diagonal != round
                        && backward[at(diagonal - 1)] < backward[at(diagonal + 1)])
                {
                    backward[at(diagonal + 1)]
                } else {
                    backward[at(diagonal - 1)] + 1
                };
                let mut y = x - diagonal;
                while x < old_len && y < new_len && same_back(x, y) {
                    (x, y) = (x + 1, y + 1);
                }
                backward[at(diagonal)] = x;
                if x > old_len {
                    b_end += 2;
                } else if y > new_len {
                    b_start += 2;
                } else if !odd {
                    let front = delta - diagonal;
                    if front.abs() <= rounds + 1 && forward[at(front)] != -1 {
                        let fx = forward[at(front)];
                        let fy = fx - front;
                        if fx >= old_len - x {
                            return Some((a0 + fx as usize, b0 + fy as usize));
                        }
                    }
                }
                diagonal += 2;
            }
        }
        None
    }
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
            let text = sanitize(&expand_tabs(text));
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
    use Op::{Added, Context, Removed};

    fn line(op: Op, old: Option<usize>, new: Option<usize>, text: &str) -> DiffLine<'_> {
        DiffLine { op, old, new, text }
    }

    fn numbered(count: usize) -> String {
        (1..=count)
            .map(|n| format!("line {n}\n"))
            .collect::<Vec<_>>()
            .concat()
    }

    fn changed(text: &str, numbers: &[usize]) -> String {
        text.lines()
            .enumerate()
            .map(|(index, line)| {
                if numbers.contains(&index.saturating_add(1)) {
                    format!("{line} changed\n")
                } else {
                    format!("{line}\n")
                }
            })
            .collect()
    }

    fn ops(hunk: &Hunk<'_>) -> Vec<Op> {
        hunk.lines.iter().map(|line| line.op).collect()
    }

    #[test]
    fn one_changed_line_has_three_lines_of_context() {
        let old = numbered(20);
        let new = changed(&old, &[10]);
        let found = hunks(&old, &new, 1);
        assert_eq!(found.len(), 1);
        let texts: Vec<&str> = found[0].lines.iter().map(|line| line.text).collect();
        assert_eq!(
            texts,
            [
                "line 7",
                "line 8",
                "line 9",
                "line 10",
                "line 10 changed",
                "line 11",
                "line 12",
                "line 13"
            ]
        );
        assert_eq!(
            ops(&found[0]),
            [
                Context, Context, Context, Removed, Added, Context, Context, Context
            ]
        );
    }

    #[test]
    fn a_change_near_an_edge_keeps_the_context_there_is() {
        let old = numbered(5);
        let new = changed(&old, &[1]);
        let found = hunks(&old, &new, 1);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].lines.first().map(|line| line.text), Some("line 1"));
        assert_eq!(found[0].lines.last().map(|line| line.text), Some("line 4"));
    }

    #[test]
    fn two_distant_changes_make_two_hunks() {
        let old = numbered(30);
        let new = changed(&old, &[5, 20]);
        let found = hunks(&old, &new, 1);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].lines.first().map(|line| line.old), Some(Some(2)));
        assert_eq!(found[0].lines.last().map(|line| line.old), Some(Some(8)));
        assert_eq!(found[1].lines.first().map(|line| line.old), Some(Some(17)));
        assert_eq!(found[1].lines.last().map(|line| line.old), Some(Some(23)));
        // Seven shared lines between changes are more than both contexts hold.
        let new = changed(&old, &[5, 13]);
        let found = hunks(&old, &new, 1);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn close_changes_merge_into_one_hunk() {
        let old = numbered(30);
        // Six shared lines between the changes: the two contexts touch.
        let new = changed(&old, &[5, 12]);
        let found = hunks(&old, &new, 1);
        assert_eq!(found.len(), 1);
        let hunk = &found[0];
        assert_eq!(hunk.lines.first().map(|line| line.old), Some(Some(2)));
        assert_eq!(hunk.lines.last().map(|line| line.old), Some(Some(15)));
        let context = hunk.lines.iter().filter(|line| line.op == Context).count();
        assert_eq!(context, 3 + 6 + 3);
    }

    #[test]
    fn line_numbers_follow_both_sides() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nx\ny\nc\ne\nf\n";
        let found = hunks(old, new, 10);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].lines,
            [
                line(Context, Some(10), Some(10), "a"),
                line(Removed, Some(11), None, "b"),
                line(Added, None, Some(11), "x"),
                line(Added, None, Some(12), "y"),
                line(Context, Some(12), Some(13), "c"),
                line(Removed, Some(13), None, "d"),
                line(Context, Some(14), Some(14), "e"),
                line(Added, None, Some(15), "f"),
            ]
        );
    }

    #[test]
    fn identical_texts_have_no_hunks() {
        assert!(hunks("", "", 1).is_empty());
        assert!(hunks("same\nlines\n", "same\nlines\n", 1).is_empty());
        let old = numbered(25_000);
        assert!(hunks(&old, &old, 1).is_empty());
    }

    #[test]
    fn an_oversized_input_falls_back_to_one_trimmed_hunk() {
        // More lines than the search takes: everything between the first and
        // the last change is replaced, with no context.
        let old = numbered(20_001);
        let new = changed(&old, &[3, 20_000]);
        let found = hunks(&old, &new, 1);
        assert_eq!(found.len(), 1);
        let lines = &found[0].lines;
        assert_eq!(lines.len(), 2 * 19_998);
        assert_eq!(lines[0], line(Removed, Some(3), None, "line 3"));
        assert_eq!(lines[19_998], line(Added, None, Some(3), "line 3 changed"));
        assert!(lines[..19_998].iter().all(|line| line.op == Removed));
        assert!(lines[19_998..].iter().all(|line| line.op == Added));
        // More edits than the search takes: the shared line in the middle is
        // replaced too.
        let old: String = (0..2_100)
            .map(|n| format!("a{n}\n"))
            .chain(["keep\n".to_owned()])
            .chain((0..10).map(|n| format!("c{n}\n")))
            .collect();
        let new: String = (0..2_100)
            .map(|n| format!("b{n}\n"))
            .chain(["keep\n".to_owned()])
            .chain((0..10).map(|n| format!("d{n}\n")))
            .collect();
        let found = hunks(&old, &new, 1);
        assert_eq!(found.len(), 1);
        assert!(found[0].lines.iter().all(|line| line.op != Context));
        assert_eq!(found[0].lines.len(), 2 * 2_111);
        // Just under the edit bound the shared line is found.
        let old: String = (0..1_990)
            .map(|n| format!("a{n}\n"))
            .chain(["keep\n".to_owned()])
            .chain((0..10).map(|n| format!("c{n}\n")))
            .collect();
        let new: String = (0..1_990)
            .map(|n| format!("b{n}\n"))
            .chain(["keep\n".to_owned()])
            .chain((0..10).map(|n| format!("d{n}\n")))
            .collect();
        let found = hunks(&old, &new, 1);
        assert_eq!(found.len(), 1);
        let context: Vec<&str> = found[0]
            .lines
            .iter()
            .filter(|line| line.op == Context)
            .map(|line| line.text)
            .collect();
        assert_eq!(context, ["keep"]);
    }

    #[test]
    fn myers_finds_a_minimal_script() {
        // (old, new, removed, added), one character per line.
        let cases = [
            ("", "", 0, 0),
            ("", "a", 0, 1),
            ("a", "", 1, 0),
            ("a", "a", 0, 0),
            ("a", "b", 1, 1),
            ("abc", "abc", 0, 0),
            ("abc", "ab", 1, 0),
            ("abc", "bc", 1, 0),
            ("abc", "axc", 1, 1),
            ("abcabba", "cbabac", 3, 2),
            ("abcd", "acbd", 1, 1),
            ("xaxbxc", "abc", 3, 0),
            ("abc", "xaxbxc", 0, 3),
            ("abcdef", "fedcba", 5, 5),
            ("aaaa", "aa", 2, 0),
            ("abab", "baba", 1, 1),
            ("abcdefgh", "abxdefyh", 2, 2),
            ("kitten", "sitting", 2, 3),
        ];
        for (old, new, removed, added) in cases {
            let split = |text: &str| -> String { text.chars().flat_map(|c| [c, '\n']).collect() };
            let (old_text, new_text) = (split(old), split(new));
            let found = hunks(&old_text, &new_text, 1);
            let count = |op| {
                found
                    .iter()
                    .flat_map(|hunk| &hunk.lines)
                    .filter(|line| line.op == op)
                    .count()
            };
            assert_eq!(
                (count(Removed), count(Added)),
                (removed, added),
                "{old} -> {new}"
            );
            // The texts are short enough for one hunk to hold all of them, so
            // its two sides spell the two texts.
            let side = |skip| -> String {
                found
                    .iter()
                    .flat_map(|hunk| &hunk.lines)
                    .filter(|line| line.op != skip)
                    .map(|line| line.text)
                    .collect()
            };
            if old != new {
                assert_eq!(side(Added), old, "{old} -> {new}");
                assert_eq!(side(Removed), new, "{old} -> {new}");
            }
        }
    }

    #[test]
    fn removals_come_before_additions_in_a_change() {
        let found = hunks("a\nb\nc\n", "x\ny\nz\n", 1);
        assert_eq!(
            ops(&found[0]),
            [Removed, Removed, Removed, Added, Added, Added]
        );
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

    #[test]
    fn a_tab_in_a_changed_line_expands_to_the_next_stop() {
        let lines = [('+', "\tx"), ('-', "abc\ty")];
        let style = DiffStyle {
            added: "<g>".to_owned(),
            removed: "<r>".to_owned(),
            dim: "<d>".to_owned(),
            reset: "</>".to_owned(),
        };
        let out = render(&lines, &style, 2, 40, true);
        assert_eq!(out[0], "    <g>+         x</>");
        assert_eq!(out[1], "    <r>- abc     y</>");
    }

    #[test]
    fn a_renamed_variable_highlights_only_the_name() {
        let removed = "let count = items.len();";
        let added = "let total_items = items.len();";
        let (old, new) = word_ranges(removed, added).expect("alike lines");
        assert_eq!(old, [Range { start: 4, end: 9 }]);
        assert_eq!(new, [Range { start: 4, end: 15 }]);
        assert_eq!(&removed[old[0].clone()], "count");
        assert_eq!(&added[new[0].clone()], "total_items");
    }

    #[test]
    fn adjacent_changed_tokens_merge_into_one_range() {
        let (old, new) = word_ranges("call(a, b);", "call(x.y, b);").expect("alike lines");
        assert_eq!(old, [Range { start: 5, end: 6 }]);
        assert_eq!(new, [Range { start: 5, end: 8 }]);
    }

    #[test]
    fn unrelated_lines_get_no_word_highlight() {
        assert_eq!(word_ranges("fn main() {", "return value;"), None);
        assert_eq!(word_ranges("", ""), None);
        assert_eq!(word_ranges("   ", " "), None);
    }

    #[test]
    fn the_forty_percent_threshold_is_inclusive() {
        // Two shared words of ten: exactly 0.4.
        let (old, new) = word_ranges("a b c d e", "a b x y z").expect("at the threshold");
        // The shared spaces between the changed words stay plain.
        assert_eq!(old, [4..5, 6..7, 8..9]);
        assert_eq!(new, [4..5, 6..7, 8..9]);
        // Two shared words of eleven: under 0.4.
        assert_eq!(word_ranges("a b c d e", "a b x y z w"), None);
        // Shared whitespace does not count.
        assert_eq!(word_ranges("a    b", "c    d"), None);
    }

    #[test]
    fn unpaired_lines_are_left_plain() {
        let old = "keep\nlet a = 1;\nlet b = 2;\nkeep\nx = 1\n";
        let new = "keep\nlet a = 10;\nkeep\nx = 1\ny = 1\n";
        let found = hunks(old, new, 1);
        assert_eq!(found.len(), 1);
        let lines = &found[0].lines;
        let words = word_highlights(lines);
        let shown: Vec<(Op, &str, Vec<&str>)> = lines
            .iter()
            .zip(&words)
            .map(|(line, ranges)| {
                let texts = ranges
                    .iter()
                    .map(|range| &line.text[range.clone()])
                    .collect();
                (line.op, line.text, texts)
            })
            .collect();
        assert_eq!(
            shown,
            [
                (Context, "keep", vec![]),
                (Removed, "let a = 1;", vec!["1"]),
                (Removed, "let b = 2;", vec![]),
                (Added, "let a = 10;", vec!["10"]),
                (Context, "keep", vec![]),
                (Context, "x = 1", vec![]),
                (Added, "y = 1", vec![]),
            ]
        );
    }
}
