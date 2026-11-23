//! Lossless matching of an edit's search text against a file.
//!
//! An exact byte match is tried first. When there is none, the text is
//! compared through normalised views (line endings, then typographic
//! punctuation and spaces), and a match found there is mapped back to the
//! original bytes. Only the matched ranges are replaced, so every byte outside
//! them is written as it was. No tier is fuzzy: a needle either matches a
//! normalised view exactly or it does not match.

use std::ops::Range;

/// A rule that decides whether a search string matches text in a file.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Tier {
    /// The bytes are the same (T0).
    Exact,
    /// The same once `\r\n` reads as `\n` (T1).
    LineEndings,
    /// The same once typographic quotes, dashes and spaces read as ASCII (T2).
    Punctuation,
    /// The same once trailing whitespace is ignored (T3).
    TrailingWhitespace,
    /// The same once indentation is ignored (T4).
    Indentation,
}

impl Tier {
    /// The phrase used in a result: "matched after normalising {clause}".
    #[must_use]
    pub const fn clause(self) -> &'static str {
        match self {
            Self::Exact => "",
            Self::LineEndings => "line endings",
            Self::Punctuation => "punctuation",
            Self::TrailingWhitespace => "trailing whitespace",
            Self::Indentation => "indentation",
        }
    }
}

/// Which tiers a search may use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tiers {
    /// T0 to T4, for a hunk that must match once.
    Unique,
    /// T0 to T2, for `occurrence` and `replace_all`.
    Selectable,
    /// T0 only, for an edit that changes nothing but carriage returns.
    Exact,
}

/// New-string indentation to swap after an indentation-tier match.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Reindent {
    /// The indentation the search text used.
    pub from: String,
    /// The indentation the file uses at the match.
    pub to: String,
}

/// The matches of the first tier that found any.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Matches {
    /// The tier that decided.
    pub tier: Tier,
    /// Non-overlapping byte ranges in the source, in order.
    pub spans: Vec<Range<usize>>,
    /// Set only by the indentation tier.
    pub reindent: Option<Reindent>,
}

/// Runs the tiers in order and stops at the first that finds at least one match.
#[must_use]
pub fn search(source: &str, needle: &str, tiers: Tiers) -> Option<Matches> {
    let exact = spans_of(source, needle);
    if !exact.is_empty() {
        return Some(found(Tier::Exact, exact));
    }
    if tiers == Tiers::Exact {
        return None;
    }
    let source_lines = View::file(source, false);
    let needle_lines = View::line_endings(needle);
    // A view that changes neither text would only repeat the exact search.
    let lines_changed = source_lines.text != source || needle_lines.text != needle;
    if lines_changed {
        let spans = view_spans(&source_lines, &needle_lines.text);
        if !spans.is_empty() {
            return Some(found(Tier::LineEndings, spans));
        }
    }
    let source_folded = View::file(source, true);
    let needle_folded = View::folded(needle);
    if source_folded.text != source_lines.text || needle_folded.text != needle_lines.text {
        let spans = view_spans(&source_folded, &needle_folded.text);
        if !spans.is_empty() {
            return Some(found(Tier::Punctuation, spans));
        }
    }
    // The line tiers can widen a match, so only a hunk that must match once
    // may use them.
    if tiers == Tiers::Selectable {
        return None;
    }
    let spans = line_windows(&source_folded, &needle_folded.text, |file, wanted| {
        file.trim_end_matches([' ', '\t']) == wanted.trim_end_matches([' ', '\t'])
    });
    if !spans.is_empty() {
        return Some(found(Tier::TrailingWhitespace, spans));
    }
    let spans = line_windows(&source_folded, &needle_folded.text, |file, wanted| {
        file.trim_matches([' ', '\t']) == wanted.trim_matches([' ', '\t'])
    });
    match spans.as_slice() {
        [] => None,
        // More than one window: the caller reports the ambiguity.
        [_, _, ..] => Some(found(Tier::Indentation, spans)),
        [span] => {
            let reindent = reindent_for(source, span.start, &needle_lines.text);
            Some(Matches {
                tier: Tier::Indentation,
                spans,
                reindent,
            })
        }
    }
}

/// The indentation swap for an indentation-tier match starting at `start`:
/// the search's first non-blank line's indentation, and the file's on the
/// line that matched it.
fn reindent_for(source: &str, start: usize, needle: &str) -> Option<Reindent> {
    let (skipped, first) = needle
        .split('\n')
        .enumerate()
        .find(|(_, line)| !line.trim_matches([' ', '\t']).is_empty())?;
    let file_line = source[start..].split('\n').nth(skipped)?;
    Some(Reindent {
        from: leading_indent(first).to_owned(),
        to: leading_indent(file_line).to_owned(),
    })
}

/// The spaces and tabs a line starts with.
fn leading_indent(line: &str) -> &str {
    let content = line.trim_start_matches([' ', '\t']);
    &line[..line.len().saturating_sub(content.len())]
}

/// `replacement` with `reindent.from` swapped for `reindent.to` at the start
/// of every non-blank line, or `None` when a non-blank line does not start
/// with `from` (it is indented less than the search, so the intent is unclear).
#[must_use]
pub fn reindented(replacement: &str, reindent: &Reindent) -> Option<String> {
    let mut out = String::with_capacity(replacement.len());
    for line in replacement.split_inclusive('\n') {
        if line.trim().is_empty() {
            out.push_str(line);
            continue;
        }
        let rest = line.strip_prefix(reindent.from.as_str())?;
        out.push_str(&reindent.to);
        out.push_str(rest);
    }
    Some(out)
}

/// Whole-line windows of `view` whose lines match the needle's, line by line.
///
/// The needle is compared with whole file lines, never a fragment of one, so
/// `foo(` inside `let x = foo(1);` cannot match. A needle ending in a newline
/// takes the last line's terminator with it; one without leaves the file's
/// terminator in place. A needle of only blank lines matches nothing.
fn line_windows(view: &View, needle: &str, same: impl Fn(&str, &str) -> bool) -> Vec<Range<usize>> {
    let (body, ends_with_newline) = match needle.strip_suffix('\n') {
        Some(body) => (body, true),
        None => (needle, false),
    };
    let wanted: Vec<&str> = body.split('\n').collect();
    if wanted
        .iter()
        .all(|line| line.trim_matches([' ', '\t']).is_empty())
    {
        return Vec::new();
    }
    // Each file line as (start, end of content, start of the next line).
    let text = &view.text;
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (at, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            lines.push((start, at, at.saturating_add(1)));
            start = at.saturating_add(1);
        }
    }
    if start < text.len() {
        lines.push((start, text.len(), text.len()));
    }
    let count = wanted.len();
    let mut spans = Vec::new();
    let mut index = 0usize;
    while index.saturating_add(count) <= lines.len() {
        let matched = wanted.iter().enumerate().all(|(offset, line)| {
            let (from, to, _) = lines[index.saturating_add(offset)];
            same(&text[from..to], line)
        });
        if matched {
            let last = lines[index.saturating_add(count).saturating_sub(1)];
            let end = if ends_with_newline { last.2 } else { last.1 };
            spans.push(view.to_source(lines[index].0)..view.to_source(end));
            index = index.saturating_add(count);
        } else {
            index = index.saturating_add(1);
        }
    }
    spans
}

fn found(tier: Tier, spans: Vec<Range<usize>>) -> Matches {
    Matches {
        tier,
        spans,
        reindent: None,
    }
}

/// Non-overlapping occurrences of `needle` in `text`, in order.
fn spans_of(text: &str, needle: &str) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    text.match_indices(needle)
        .map(|(start, matched)| start..start.saturating_add(matched.len()))
        .collect()
}

/// Matches in a view, mapped back to the source's byte ranges.
fn view_spans(view: &View, needle: &str) -> Vec<Range<usize>> {
    spans_of(&view.text, needle)
        .into_iter()
        .map(|span| view.to_source(span.start)..view.to_source(span.end))
        .collect()
}

/// A normalised copy of a text and a map from its byte offsets back to the original's.
#[derive(Clone, Debug)]
pub struct View {
    /// The normalised text.
    pub text: String,
    /// Source bytes before the view's first byte, such as a byte order mark.
    base: usize,
    /// `(view offset, extra source bytes up to and including this shift)`,
    /// sorted by view offset.
    shifts: Vec<(usize, usize)>,
}

impl View {
    /// Line-ending view: every `\r` directly followed by `\n` is removed.
    #[must_use]
    pub fn line_endings(source: &str) -> Self {
        Self::build(source, false)
    }

    /// Line-ending view plus the punctuation fold.
    #[must_use]
    pub fn folded(source: &str) -> Self {
        Self::build(source, true)
    }

    /// A view of a file: like [`View::line_endings`] or [`View::folded`], but
    /// a leading byte order mark is left out, so line tiers see the first
    /// line as written and every offset lands after the mark.
    fn file(source: &str, fold_punctuation: bool) -> Self {
        match source.strip_prefix('\u{FEFF}') {
            Some(rest) => {
                let mut view = Self::build(rest, fold_punctuation);
                view.base = source.len().saturating_sub(rest.len());
                view
            }
            None => Self::build(source, fold_punctuation),
        }
    }

    fn build(source: &str, fold_punctuation: bool) -> Self {
        let mut text = String::with_capacity(source.len());
        let mut shifts = Vec::new();
        let mut extra = 0usize;
        let mut characters = source.chars().peekable();
        while let Some(character) = characters.next() {
            if character == '\r' && characters.peek() == Some(&'\n') {
                // Recorded at the `\n`, so a span starting there starts at the
                // `\r` and the pair is replaced whole.
                extra = extra.saturating_add(1);
                shifts.push((text.len(), extra));
                continue;
            }
            let shown = if fold_punctuation {
                fold(character)
            } else {
                character
            };
            let lost = character.len_utf8().saturating_sub(shown.len_utf8());
            if lost > 0 {
                extra = extra.saturating_add(lost);
                shifts.push((text.len(), extra));
            }
            text.push(shown);
        }
        Self {
            text,
            base: 0,
            shifts,
        }
    }

    /// Maps a byte offset in `text` to the byte offset in the source.
    ///
    /// Adds the extra bytes of every shift recorded at a view offset strictly
    /// before `offset`.
    #[must_use]
    pub fn to_source(&self, offset: usize) -> usize {
        let before = self.shifts.partition_point(|&(at, _)| at < offset);
        let extra = before
            .checked_sub(1)
            .and_then(|index| self.shifts.get(index))
            .map_or(0, |&(_, extra)| extra);
        self.base.saturating_add(offset).saturating_add(extra)
    }
}

/// Lines `first..=last` (1-based) where `new` already occurs, when it has at least
/// 20 non-whitespace characters. Compared on the folded views.
#[must_use]
pub fn already_applied(source: &str, new: &str) -> Option<(usize, usize)> {
    if new.chars().filter(|c| !c.is_whitespace()).count() < 20 {
        return None;
    }
    let source_view = View::file(source, true);
    let needle = View::folded(new);
    let start = source_view.text.find(needle.text.as_str())?;
    let end = start.saturating_add(needle.text.len());
    let from = source_view.to_source(start);
    let to = source_view.to_source(end);
    let first = line_of(source, from);
    // A match that ends with a newline ends on the line before the next one.
    let last = line_of(source, to.saturating_sub(1).max(from));
    Some((first, last))
}

/// The 1-based line holding byte `offset`.
fn line_of(source: &str, offset: usize) -> usize {
    let offset = offset.min(source.len());
    source[..offset].matches('\n').count().saturating_add(1)
}

/// The file lines closest to a missed search, numbered as `read_file` shows them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Closest {
    /// First line shown, 1-based.
    pub first: usize,
    /// Last line shown, 1-based, at most `first + 11`.
    pub last: usize,
    /// The lines in `read_file` format, one per file line.
    pub numbered: String,
    /// A note when the only difference looks like tabs against spaces.
    pub indent_note: Option<&'static str>,
}

/// Lines in a file too long to search for the closest text.
const MAX_CLOSEST_SOURCE_LINES: usize = 100_000;

/// Most lines quoted, and most characters of one line.
const MAX_CLOSEST_LINES: usize = 12;
const MAX_CLOSEST_LINE_CHARS: usize = 200;
const MAX_CLOSEST_BYTES: usize = 4096;

/// A line's comparison key: folded, whitespace runs collapsed, trimmed.
fn line_key(line: &str) -> String {
    let folded: String = line.chars().map(fold).collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The file lines most of the needle's distinctive lines agree on.
///
/// Each needle line with a letter or digit votes for the start position its
/// equal file lines imply; the start with the most votes wins when at least
/// half of those lines agree on it.
#[must_use]
pub fn closest_lines(source: &str, needle: &str) -> Option<Closest> {
    let file: Vec<&str> = source
        .strip_prefix('\u{FEFF}')
        .unwrap_or(source)
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if file.len() > MAX_CLOSEST_SOURCE_LINES {
        return None;
    }
    let wanted: Vec<(usize, &str)> = needle
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .enumerate()
        .filter(|(_, line)| line.chars().any(char::is_alphanumeric))
        .collect();
    if wanted.is_empty() {
        return None;
    }
    let mut by_key: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (index, line) in file.iter().enumerate() {
        by_key.entry(line_key(line)).or_default().push(index);
    }
    // start -> (votes, file lines that voted for it)
    let mut tally: std::collections::BTreeMap<usize, (usize, Vec<(usize, usize)>)> =
        std::collections::BTreeMap::new();
    for &(offset, line) in &wanted {
        let Some(lines) = by_key.get(&line_key(line)) else {
            continue;
        };
        let mut voted = std::collections::BTreeSet::new();
        for &index in lines {
            let Some(start) = index.checked_sub(offset) else {
                continue;
            };
            if voted.insert(start) {
                let entry = tally.entry(start).or_default();
                entry.0 = entry.0.saturating_add(1);
                entry.1.push((offset, index));
            }
        }
    }
    let (&start, (votes, voters)) =
        tally
            .iter()
            .max_by(|(left_start, left), (right_start, right)| {
                left.0.cmp(&right.0).then(right_start.cmp(left_start))
            })?;
    if votes.saturating_mul(2) < wanted.len() {
        return None;
    }
    // The whole window the winning start implies, so the lines that differ
    // are shown beside the ones that agree.
    let body = needle.strip_suffix('\n').unwrap_or(needle);
    let window = body.split('\n').count().max(1);
    let first = start.saturating_add(1);
    let last = start
        .saturating_add(window)
        .min(file.len())
        .min(first.saturating_add(MAX_CLOSEST_LINES.saturating_sub(1)))
        .max(first);
    let mut numbered = String::new();
    for number in first..=last {
        let text = file[number.saturating_sub(1)];
        let shown = if text.chars().count() > MAX_CLOSEST_LINE_CHARS {
            let cut: String = text.chars().take(MAX_CLOSEST_LINE_CHARS).collect();
            format!("{cut}...")
        } else {
            text.to_owned()
        };
        let row = format!("{number:>6}\t{shown}");
        let separator = usize::from(!numbered.is_empty());
        if numbered
            .len()
            .saturating_add(separator)
            .saturating_add(row.len())
            > MAX_CLOSEST_BYTES
        {
            break;
        }
        if !numbered.is_empty() {
            numbered.push('\n');
        }
        numbered.push_str(&row);
    }
    let needle_lines: Vec<&str> = needle
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let pairs: Vec<(&str, &str)> = voters
        .iter()
        .filter(|&&(_, index)| index.saturating_add(1) >= first && index < last)
        .map(|&(offset, index)| (file[index], needle_lines[offset]))
        .filter(|(file_line, needle_line)| file_line != needle_line)
        .collect();
    Some(Closest {
        first,
        last,
        numbered,
        indent_note: indent_note(&pairs),
    })
}

/// The note for a closest match whose lines differ only in tabs against spaces.
fn indent_note(pairs: &[(&str, &str)]) -> Option<&'static str> {
    if pairs.is_empty()
        || !pairs.iter().all(|(file, needle)| {
            file.trim_start_matches([' ', '\t']) == needle.trim_start_matches([' ', '\t'])
        })
    {
        return None;
    }
    let has_tab = |line: &str| leading_indent(line).contains('\t');
    if pairs
        .iter()
        .all(|(file, needle)| has_tab(file) && !has_tab(needle))
    {
        return Some("The file indents with tabs; old_string uses spaces.");
    }
    if pairs
        .iter()
        .all(|(file, needle)| has_tab(needle) && !has_tab(file))
    {
        return Some("The file indents with spaces; old_string uses tabs.");
    }
    None
}

/// The punctuation fold for one character.
///
/// Typographic quotes, dashes and the minus sign read as their ASCII form,
/// and the no-break and fixed-width spaces as a plain space.
#[must_use]
pub const fn fold(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    #[test]
    fn a_view_maps_offsets_back_across_removed_carriage_returns() {
        let view = View::line_endings("a\r\nb\r\nc");
        assert_eq!(view.text, "a\nb\nc");
        let mapped: Vec<usize> = (0..=5).map(|offset| view.to_source(offset)).collect();
        assert_eq!(mapped, [0, 1, 3, 4, 6, 7]);
    }

    #[test]
    fn a_folded_view_maps_offsets_back_across_wide_characters() {
        // U+2019 is three bytes and folds to one.
        let source = "it\u{2019}s \u{201C}x\u{201D}";
        let view = View::folded(source);
        assert_eq!(view.text, "it's \"x\"");
        assert_eq!(
            view.to_source(2),
            2,
            "the folded character starts where it did"
        );
        assert_eq!(view.to_source(3), 5, "and ends after its three bytes");
        assert_eq!(view.to_source(view.text.len()), source.len());
    }

    #[test]
    fn the_fold_covers_every_listed_character() {
        let quotes = ['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'];
        let doubles = ['\u{201C}', '\u{201D}', '\u{201E}', '\u{201F}'];
        let dashes = [
            '\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}',
        ];
        let spaces = [
            '\u{00A0}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}',
            '\u{2008}', '\u{2009}', '\u{200A}', '\u{202F}', '\u{205F}', '\u{3000}',
        ];
        for c in quotes {
            assert_eq!(fold(c), '\'', "{c:?}");
        }
        for c in doubles {
            assert_eq!(fold(c), '"', "{c:?}");
        }
        for c in dashes {
            assert_eq!(fold(c), '-', "{c:?}");
        }
        for c in spaces {
            assert_eq!(fold(c), ' ', "{c:?}");
        }
        for c in ['a', '\'', '-', ' ', '\u{2016}', '\u{200B}', 'é'] {
            assert_eq!(fold(c), c, "{c:?}");
        }
    }

    #[test]
    fn an_exact_match_stops_the_search_before_the_folded_tiers() {
        let source = "it's and it\u{2019}s";
        let matches = search(source, "it's", Tiers::Unique).expect("a match");
        assert_eq!(matches.tier, Tier::Exact);
        assert_eq!(matches.spans.len(), 1);
        assert_eq!(matches.spans[0], 0..4);
    }

    #[test]
    fn a_folded_tier_reports_every_match_it_found() {
        let source = "it\u{2019}s, it\u{2019}s";
        let matches = search(source, "it's", Tiers::Unique).expect("a match");
        assert_eq!(matches.tier, Tier::Punctuation);
        assert_eq!(matches.spans.len(), 2);
        for span in &matches.spans {
            assert_eq!(&source[span.clone()], "it\u{2019}s");
        }
    }

    #[test]
    fn trailing_spaces_in_the_file_match_at_the_trailing_whitespace_tier() {
        let source = "fn main() {\n    let x = 1;   \n}\n";
        let matches = search(source, "    let x = 1;\n", Tiers::Unique).expect("a match");
        assert_eq!(matches.tier, Tier::TrailingWhitespace);
        assert_eq!(&source[matches.spans[0].clone()], "    let x = 1;   \n");
        assert!(search(source, "    let x = 1;\n", Tiers::Selectable).is_none());
    }

    #[test]
    fn trailing_tabs_in_the_search_match_at_the_trailing_whitespace_tier() {
        let source = "a\nb\nc\n";
        let matches = search(source, "a\t\nb\t\t", Tiers::Unique).expect("a match");
        assert_eq!(matches.tier, Tier::TrailingWhitespace);
        assert_eq!(&source[matches.spans[0].clone()], "a\nb");
    }

    #[test]
    fn a_window_keeps_the_file_terminator_when_the_search_has_none() {
        let source = "one  \ntwo\n";
        let without = search(source, "one", Tiers::Unique).expect("a match");
        assert_eq!(
            without.tier,
            Tier::Exact,
            "a plain fragment matches exactly first"
        );
        let matches = search(source, "one \t", Tiers::Unique).expect("a match");
        assert_eq!(matches.tier, Tier::TrailingWhitespace);
        assert_eq!(&source[matches.spans[0].clone()], "one  ");
        // A final line with no terminator: a search ending in a newline still
        // reaches the end of the text.
        let last = "x\nend  ";
        let matches = search(last, "end\n", Tiers::Unique).expect("a match");
        assert_eq!(&last[matches.spans[0].clone()], "end  ");
    }

    #[test]
    fn a_fragment_of_a_line_never_matches_a_line_tier() {
        let source = "let x = foo(1);  \n";
        assert!(search(source, "foo(  ", Tiers::Unique).is_none());
        assert!(
            search(" \n\t\n", "  \n", Tiers::Unique).is_none(),
            "blank needles never match"
        );
    }

    #[test]
    fn two_trailing_whitespace_windows_are_ambiguous() {
        let source = "x = 1 \ny\nx = 1\t\n";
        let matches = search(source, "x = 1  ", Tiers::Unique).expect("matches");
        assert_eq!(matches.tier, Tier::TrailingWhitespace);
        assert_eq!(matches.spans.len(), 2);
    }

    #[test]
    fn a_crlf_window_ends_before_the_carriage_return() {
        let source = "one  \r\ntwo\r\n";
        let matches = search(source, "one", Tiers::Unique).expect("a match");
        assert_eq!(matches.tier, Tier::Exact);
        let matches = search(source, "one\t", Tiers::Unique).expect("a match");
        assert_eq!(matches.tier, Tier::TrailingWhitespace);
        assert_eq!(&source[matches.spans[0].clone()], "one  ");
    }

    #[test]
    fn a_search_indented_differently_matches_at_the_indentation_tier() {
        let source = "fn f() {\n        let x = 1;\n        let y = 2;\n}\n";
        let matches =
            search(source, "    let x = 1;\n    let y = 2;\n", Tiers::Unique).expect("a match");
        assert_eq!(matches.tier, Tier::Indentation);
        assert_eq!(
            &source[matches.spans[0].clone()],
            "        let x = 1;\n        let y = 2;\n"
        );
        assert_eq!(
            matches.reindent,
            Some(Reindent {
                from: "    ".to_owned(),
                to: "        ".to_owned()
            })
        );
        assert!(
            search(
                source,
                "    let x = 1;\n    let y = 2;\n",
                Tiers::Selectable
            )
            .is_none()
        );
    }

    #[test]
    fn the_replacement_is_reindented_to_the_file() {
        let reindent = Reindent {
            from: "    ".to_owned(),
            to: "        ".to_owned(),
        };
        assert_eq!(
            reindented("    a\n        b\n", &reindent).as_deref(),
            Some("        a\n            b\n")
        );
        let from_none = Reindent {
            from: String::new(),
            to: "    ".to_owned(),
        };
        assert_eq!(
            reindented("a\nb", &from_none).as_deref(),
            Some("    a\n    b")
        );
    }

    #[test]
    fn blank_replacement_lines_stay_blank() {
        let reindent = Reindent {
            from: "  ".to_owned(),
            to: "\t".to_owned(),
        };
        assert_eq!(
            reindented("  a\n\n  b\n", &reindent).as_deref(),
            Some("\ta\n\n\tb\n")
        );
    }

    #[test]
    fn a_replacement_outdented_past_the_search_gives_up() {
        let reindent = Reindent {
            from: "    ".to_owned(),
            to: "        ".to_owned(),
        };
        assert_eq!(reindented("    a\n}\n", &reindent), None);
    }

    #[test]
    fn two_indentation_windows_are_ambiguous() {
        let source = "  x = 1\n    x = 1\n";
        let matches = search(source, "x = 1\n", Tiers::Unique).expect("matches");
        assert_eq!(matches.tier, Tier::Exact);
        let matches = search(source, "\tx = 1\n", Tiers::Unique).expect("matches");
        assert_eq!(matches.tier, Tier::Indentation);
        assert_eq!(matches.spans.len(), 2);
        assert_eq!(matches.reindent, None);
    }

    #[test]
    fn already_applied_needs_twenty_visible_characters() {
        let source = "one\nlet total = compute(a, b);\nthree\n";
        assert_eq!(
            already_applied(source, "let total = compute(a, b);\n"),
            Some((2, 2))
        );
        assert_eq!(
            already_applied(source, "one\nlet total = compute(a, b);"),
            Some((1, 2))
        );
        // Nineteen non-whitespace characters are not enough.
        assert_eq!(
            already_applied("abcdefghij klmnopqrs", "abcdefghij klmnopqrs"),
            None
        );
        assert_eq!(
            already_applied(source, "nothing like this appears here at all"),
            None
        );
    }

    #[test]
    fn closest_lines_need_half_of_the_distinctive_lines() {
        let source = "fn a() {\n    first();\n    second();\n}\n";
        let closest = closest_lines(source, "    first();\n    changed();\n").expect("half agree");
        // The window the agreeing line implies, including the line that differs.
        assert_eq!((closest.first, closest.last), (2, 3));
        let none = closest_lines(source, "    first();\n    x();\n    y();\n    z();\n");
        assert_eq!(none, None, "one of four distinctive lines is not enough");
        assert_eq!(
            closest_lines(source, "  }\n  ;\n"),
            None,
            "no distinctive lines"
        );
    }

    #[test]
    fn closest_lines_are_numbered_like_read_file() {
        let source = "a\nfn b() {\n    call();\n}\n";
        let closest = closest_lines(source, "fn b() {\n    call(1);\n}\n").expect("closest");
        assert_eq!((closest.first, closest.last), (2, 4));
        assert_eq!(
            closest.numbered,
            "     2\tfn b() {\n     3\t    call();\n     4\t}"
        );
    }

    #[test]
    fn closest_lines_are_capped_at_twelve_lines_and_two_hundred_characters() {
        let mut source = String::new();
        let long = "x".repeat(5000);
        for index in 0..20 {
            let _ = writeln!(source, "line {index} {long}");
        }
        let needle = format!("line 0 {long}\nline 19 {long}\n");
        let closest = closest_lines(
            &source,
            &format!("line 0 {long}\n{}line 19 {long}\n", "\n".repeat(18)),
        )
        .expect("closest");
        let _ = needle;
        assert_eq!((closest.first, closest.last), (1, 12));
        let first = closest.numbered.lines().next().expect("a line");
        assert!(first.ends_with("..."), "{first}");
        assert_eq!(
            first.split('\t').nth(1).map(|text| text.chars().count()),
            Some(203)
        );
        assert!(closest.numbered.len() <= 4096);
    }

    #[test]
    fn closest_lines_skip_files_over_one_hundred_thousand_lines() {
        let source = "x\n".repeat(100_001);
        assert_eq!(closest_lines(&source, "x\n"), None);
    }

    #[test]
    fn a_tab_indented_file_gets_the_tab_note() {
        let source = "fn f() {\n\tlet x = 1;\n\tlet y = 2;\n}\n";
        let closest = closest_lines(source, "    let x = 1;\n    let y = 2;\n").expect("closest");
        assert_eq!(
            closest.indent_note,
            Some("The file indents with tabs; old_string uses spaces.")
        );
        let spaces = "fn f() {\n    let x = 1;\n}\n";
        let closest = closest_lines(spaces, "\tlet x = 1;\n").expect("closest");
        assert_eq!(
            closest.indent_note,
            Some("The file indents with spaces; old_string uses tabs.")
        );
    }

    #[test]
    fn a_crlf_search_matches_an_lf_file_and_the_reverse() {
        let crlf = "one\r\ntwo\r\nthree";
        let matches = search(crlf, "one\ntwo", Tiers::Unique).expect("a match");
        assert_eq!(matches.tier, Tier::LineEndings);
        assert_eq!(&crlf[matches.spans[0].clone()], "one\r\ntwo");
        let lf = "one\ntwo\nthree";
        let matches = search(lf, "one\r\ntwo", Tiers::Unique).expect("a match");
        assert_eq!(matches.tier, Tier::LineEndings);
        assert_eq!(&lf[matches.spans[0].clone()], "one\ntwo");
        // A lone carriage return is never removed.
        assert!(search("a\rb", "ab", Tiers::Unique).is_none());
    }
}
