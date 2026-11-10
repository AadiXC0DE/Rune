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
    let _ = tiers;
    let exact = spans_of(source, needle);
    if !exact.is_empty() {
        return Some(found(Tier::Exact, exact));
    }
    let source_lines = View::line_endings(source);
    let needle_lines = View::line_endings(needle);
    // A view that changes neither text would only repeat the exact search.
    let lines_changed = source_lines.text != source || needle_lines.text != needle;
    if lines_changed {
        let spans = view_spans(&source_lines, &needle_lines.text);
        if !spans.is_empty() {
            return Some(found(Tier::LineEndings, spans));
        }
    }
    let source_folded = View::folded(source);
    let needle_folded = View::folded(needle);
    if source_folded.text != source_lines.text || needle_folded.text != needle_lines.text {
        let spans = view_spans(&source_folded, &needle_folded.text);
        if !spans.is_empty() {
            return Some(found(Tier::Punctuation, spans));
        }
    }
    None
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
