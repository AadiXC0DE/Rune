//! Terminal styling for the Markdown a model writes.
//!
//! One source line at a time, so a streaming answer styles each line as it
//! completes and an unfinished line is styled again as it grows. The markers
//! are removed and replaced by escapes; wrapping happens afterwards, and the
//! wrapper measures only visible text and reopens a style at each break.
//!
//! Only the constructs that read badly as raw text are handled: headings,
//! quotes, list bullets, rules, emphasis, inline code and links. Anything that
//! does not parse cleanly is left as the model wrote it, because showing a
//! stray marker is better than hiding text.

/// The escapes each construct is drawn with.
///
/// Supplied by the caller so the theme chooses the colours. An empty string
/// draws the construct without an escape, which is how a colourless theme
/// still loses the raw markers.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct MarkdownStyle {
    /// Whether Markdown is styled at all. Off leaves text exactly as written.
    pub enabled: bool,
    /// Opens a heading.
    pub heading: String,
    /// Opens strong emphasis.
    pub bold: String,
    /// Opens emphasis.
    pub italic: String,
    /// Opens inline code.
    pub code: String,
    /// Opens link text.
    pub link: String,
    /// Opens quoted text, list markers and rules.
    pub dim: String,
    /// Closes any of the above.
    pub reset: String,
}

impl MarkdownStyle {
    /// Styling with terminal attributes only, for a theme with no colours.
    #[must_use]
    pub fn attributes() -> Self {
        Self {
            enabled: true,
            heading: "\u{1b}[1m".to_owned(),
            bold: "\u{1b}[1m".to_owned(),
            italic: "\u{1b}[3m".to_owned(),
            code: String::new(),
            link: "\u{1b}[4m".to_owned(),
            dim: "\u{1b}[2m".to_owned(),
            reset: "\u{1b}[0m".to_owned(),
        }
    }
}

/// Styles one line of prose. `width` bounds a horizontal rule.
#[must_use]
pub fn style_line(line: &str, style: &MarkdownStyle, ascii: bool, width: usize) -> String {
    if !style.enabled {
        return line.to_owned();
    }
    let indent_len = line
        .len()
        .saturating_sub(line.trim_start_matches(' ').len());
    let (indent, rest) = line.split_at(indent_len);

    if indent_len <= 3 && is_rule(rest) {
        let glyph = if ascii { "-" } else { "\u{2500}" };
        let length = width.clamp(3, 48);
        return format!("{}{}{}", style.dim, glyph.repeat(length), style.reset);
    }
    if indent_len <= 3
        && let Some(text) = heading(rest)
    {
        return format!("{}{}{}", style.heading, inline(text, style), style.reset);
    }
    if indent_len <= 3
        && let Some(text) = rest.strip_prefix('>')
    {
        let bar = if ascii { "|" } else { "\u{2502}" };
        let text = text.strip_prefix(' ').unwrap_or(text);
        return format!(
            "{}{bar}{} {}{}{}",
            style.dim,
            style.reset,
            style.dim,
            inline(text, style),
            style.reset
        );
    }
    if let Some(text) = bullet(rest) {
        let marker = if ascii { "-" } else { "\u{2022}" };
        return format!(
            "{indent}{}{marker}{} {}",
            style.dim,
            style.reset,
            inline(text, style)
        );
    }
    format!("{indent}{}", inline(rest, style))
}

/// Returns true for a thematic break: three or more of one of `-`, `*`, `_`.
fn is_rule(text: &str) -> bool {
    let compact: Vec<char> = text.chars().filter(|ch| *ch != ' ').collect();
    compact.len() >= 3
        && matches!(compact.first(), Some('-' | '*' | '_'))
        && compact.iter().all(|ch| Some(ch) == compact.first())
}

/// Returns the text of an ATX heading.
fn heading(text: &str) -> Option<&str> {
    let level = text.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &text[level..];
    if rest.is_empty() {
        return Some("");
    }
    let rest = rest.strip_prefix(' ')?;
    Some(rest.trim_end().trim_end_matches('#').trim_end())
}

/// Returns the item text of an unordered list line.
fn bullet(text: &str) -> Option<&str> {
    let mut chars = text.chars();
    let marker = chars.next()?;
    if !matches!(marker, '-' | '*' | '+') {
        return None;
    }
    text[1..].strip_prefix(' ')
}

/// Styles emphasis, code spans and links within a line.
fn inline(text: &str, style: &MarkdownStyle) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let before = at
            .checked_sub(1)
            .and_then(|index| bytes.get(index))
            .copied();

        if rest.starts_with('`') {
            let ticks = rest.bytes().take_while(|byte| *byte == b'`').count();
            let fence = &rest[..ticks];
            if let Some(end) = rest[ticks..].find(fence) {
                let close = ticks.saturating_add(end);
                let code = &rest[ticks..close];
                let code = if code.len() > 2 && code.starts_with(' ') && code.ends_with(' ') {
                    &code[1..code.len().saturating_sub(1)]
                } else {
                    code
                };
                out.push_str(&style.code);
                out.push_str(code);
                out.push_str(&style.reset);
                at = at.saturating_add(close).saturating_add(ticks);
                continue;
            }
        }

        if let Some((consumed, styled)) = link(rest, style) {
            out.push_str(&styled);
            at = at.saturating_add(consumed);
            continue;
        }

        if let Some((consumed, inner)) =
            delimited(rest, "**", before).or_else(|| delimited(rest, "__", before))
        {
            out.push_str(&style.bold);
            out.push_str(&inline(inner, style));
            out.push_str(&style.reset);
            at = at.saturating_add(consumed);
            continue;
        }

        if let Some((consumed, inner)) =
            delimited(rest, "*", before).or_else(|| delimited(rest, "_", before))
        {
            out.push_str(&style.italic);
            out.push_str(&inline(inner, style));
            out.push_str(&style.reset);
            at = at.saturating_add(consumed);
            continue;
        }

        let next = rest.chars().next().map_or(1, char::len_utf8);
        out.push_str(&rest[..next]);
        at = at.saturating_add(next);
    }
    out
}

/// Matches `marker inner marker` at the start of `text`.
///
/// The opening marker must be followed by a non-space and the closing one
/// preceded by a non-space, as Markdown requires. An underscore must also sit
/// at a word boundary on both sides, so `snake_case_names` stay as written.
fn delimited<'a>(text: &'a str, marker: &str, before: Option<u8>) -> Option<(usize, &'a str)> {
    let body = text.strip_prefix(marker)?;
    if body.is_empty() || body.starts_with(' ') || body.starts_with(marker) {
        return None;
    }
    let word = |byte: Option<u8>| byte.is_some_and(|byte| byte.is_ascii_alphanumeric());
    let underscore = marker.starts_with('_');
    if underscore && word(before) {
        return None;
    }
    let mut search = 0;
    while let Some(found) = body[search..].find(marker) {
        let end = search.saturating_add(found);
        let past = end.saturating_add(marker.len());
        let after = body.as_bytes().get(past).copied();
        let doubled = marker.len() == 1 && body[past..].starts_with(marker);
        let blocked_by_word = underscore && word(after);
        let closes = end > 0 && !body[..end].ends_with(' ') && !blocked_by_word && !doubled;
        if closes {
            return Some((past.saturating_add(marker.len()), &body[..end]));
        }
        search = past;
    }
    None
}

/// Matches `[text](target)` at the start of `text`.
fn link(text: &str, style: &MarkdownStyle) -> Option<(usize, String)> {
    let label_end = text.strip_prefix('[')?.find("](")?.saturating_add(1);
    let label = &text[1..label_end];
    let target_start = label_end.saturating_add(2);
    let after = &text[target_start..];
    let target_end = after.find(')')?;
    let target = &after[..target_end];
    if label.is_empty() || target.is_empty() || target.contains(char::is_whitespace) {
        return None;
    }
    let consumed = target_start.saturating_add(target_end).saturating_add(1);
    // An OSC 8 hyperlink makes the label clickable where the terminal
    // supports it; elsewhere the escape is ignored and the label remains.
    Some((
        consumed,
        format!(
            "\u{1b}]8;;{target}\u{1b}\\{}{}{}\u{1b}]8;;\u{1b}\\",
            style.link,
            inline(label, style),
            style.reset
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brackets around each construct instead of escapes, so a test reads as text.
    fn marked() -> MarkdownStyle {
        MarkdownStyle {
            enabled: true,
            heading: "<h>".to_owned(),
            bold: "<b>".to_owned(),
            italic: "<i>".to_owned(),
            code: "<c>".to_owned(),
            link: "<a>".to_owned(),
            dim: "<d>".to_owned(),
            reset: "</>".to_owned(),
        }
    }

    fn styled(line: &str) -> String {
        style_line(line, &marked(), false, 10)
    }

    #[test]
    fn disabled_styling_leaves_the_line_as_written() {
        let line = "## **raw** `text`";
        assert_eq!(style_line(line, &MarkdownStyle::default(), false, 80), line);
    }

    #[test]
    fn emphasis_and_code_lose_their_markers() {
        assert_eq!(styled("a **b** c"), "a <b>b</> c");
        assert_eq!(styled("a *b* c"), "a <i>b</> c");
        assert_eq!(styled("use `x * y` here"), "use <c>x * y</> here");
        assert_eq!(styled("**bold with `code`**"), "<b>bold with <c>code</></>");
    }

    #[test]
    fn markers_that_do_not_pair_stay_as_written() {
        assert_eq!(styled("2 * 3 * 4"), "2 * 3 * 4");
        assert_eq!(styled("a **b"), "a **b");
        assert_eq!(styled("snake_case_name and _x"), "snake_case_name and _x");
        assert_eq!(styled("an `unclosed span"), "an `unclosed span");
    }

    #[test]
    fn block_constructs_are_drawn_without_their_syntax() {
        assert_eq!(styled("## Plan ##"), "<h>Plan</>");
        assert_eq!(styled("- item **one**"), "<d>\u{2022}</> item <b>one</>");
        assert_eq!(styled("  * nested"), "  <d>\u{2022}</> nested");
        assert_eq!(styled("> quoted"), "<d>\u{2502}</> <d>quoted</>");
        assert_eq!(styled("---"), format!("<d>{}</>", "\u{2500}".repeat(10)));
        assert_eq!(styled("1. first"), "1. first");
        assert_eq!(styled("#hashtag"), "#hashtag");
    }

    #[test]
    fn ascii_output_uses_ascii_markers() {
        let line = style_line("- item", &marked(), true, 10);
        assert!(line.is_ascii(), "{line}");
        assert!(style_line("***", &marked(), true, 4).is_ascii());
    }

    #[test]
    fn a_link_keeps_its_label_and_carries_its_target() {
        let line = styled("see [the docs](https://example.com/a) now");
        assert!(line.starts_with("see \u{1b}]8;;https://example.com/a\u{1b}\\<a>the docs</>"));
        assert!(line.ends_with("\u{1b}]8;;\u{1b}\\ now"), "{line}");
        assert_eq!(styled("[not a link] (x)"), "[not a link] (x)");
    }

    #[test]
    fn multibyte_text_is_kept_whole() {
        assert_eq!(styled("café **naïve** 日本"), "café <b>naïve</> 日本");
    }
}
