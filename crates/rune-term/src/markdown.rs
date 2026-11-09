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
    /// How fenced code is coloured.
    pub code_block: crate::highlight::CodeStyle,
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
            code_block: crate::highlight::CodeStyle::default(),
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

/// Lays out every pipe table in a finished text with aligned columns.
///
/// A table can only be measured once all of its rows exist, so this runs on a
/// finished answer rather than while it streams. Cells keep their Markdown, so
/// emphasis and code inside them are still styled afterwards. Text outside a
/// table, and inside a code fence, is left exactly as written.
#[must_use]
pub fn align_tables(text: &str, ascii: bool) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut fence = false;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = !fence;
        }
        let delimiter = lines.get(index.saturating_add(1)).copied().unwrap_or_default();
        if !fence && is_row(line) && is_delimiter(delimiter) {
            let mut rows = vec![cells(line)];
            let mut next = index.saturating_add(2);
            while let Some(row) = lines.get(next).filter(|row| is_row(row)) {
                rows.push(cells(row));
                next = next.saturating_add(1);
            }
            out.extend(render_table(&rows, ascii));
            index = next;
            continue;
        }
        out.push(line.to_owned());
        index = index.saturating_add(1);
    }
    out.join("\n")
}

/// Columns a cell occupies once its Markdown markers are drawn away.
fn visible_width(cell: &str) -> usize {
    let plain = MarkdownStyle {
        enabled: true,
        ..MarkdownStyle::default()
    };
    crate::width::wrap_styled(&inline(cell, &plain), usize::MAX)
        .first()
        .map_or(0, |line| line.width)
}

fn is_row(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('|') && line.len() > 1
}

fn is_delimiter(line: &str) -> bool {
    let line = line.trim();
    is_row(line)
        && line.contains('-')
        && line
            .chars()
            .all(|ch| matches!(ch, '|' | '-' | ':' | ' '))
}

/// Splits a row into trimmed cells, honouring escaped pipes.
fn cells(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|');
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            cell.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
            cell.push(ch);
        } else if ch == '|' {
            cells.push(cell.trim().to_owned());
            cell.clear();
        } else {
            cell.push(ch);
        }
    }
    cells.push(cell.trim().to_owned());
    cells
}

/// Renders rows, the first being the header, with every column padded to
/// its widest cell.
fn render_table(rows: &[Vec<String>], ascii: bool) -> Vec<String> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or_default();
    let mut widths = vec![0_usize; columns];
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(visible_width(cell));
        }
    }
    let (bar, rule, cross) = if ascii {
        ("|", "-", "+")
    } else {
        ("\u{2502}", "\u{2500}", "\u{253c}")
    };
    let line = |row: &Vec<String>| {
        let mut text = String::new();
        for (column, width) in widths.iter().enumerate() {
            if column > 0 {
                text.push(' ');
                text.push_str(bar);
                text.push(' ');
            }
            let cell = row.get(column).map_or("", String::as_str);
            text.push_str(cell);
            let pad = width.saturating_sub(visible_width(cell));
            text.extend(std::iter::repeat_n(' ', pad));
        }
        text.trim_end().to_owned()
    };
    let mut out = Vec::with_capacity(rows.len().saturating_add(1));
    if let Some(header) = rows.first() {
        out.push(line(header));
        // Each column's rule is its width, and the joint matches the ` | `
        // between cells, so the rule lines up under the header.
        out.push(
            widths
                .iter()
                .map(|width| rule.repeat((*width).max(1)))
                .collect::<Vec<_>>()
                .join(&format!("{rule}{cross}{rule}")),
        );
    }
    out.extend(rows.iter().skip(1).map(line));
    out
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
            code_block: crate::highlight::CodeStyle::default(),
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
    fn a_pipe_table_is_aligned_and_its_delimiter_becomes_a_rule() {
        let text = "Results:\n| Task | Pass |\n|---|:-:|\n| pagination | yes |\n| csv | **no** |\nafter";
        let aligned = align_tables(text, true);
        let lines: Vec<&str> = aligned.lines().collect();
        assert_eq!(lines[0], "Results:");
        assert_eq!(lines[1], "Task       | Pass");
        assert!(lines[2].chars().all(|ch| ch == '-' || ch == '+'), "{}", lines[2]);
        assert_eq!(lines[3], "pagination | yes");
        // Measured without its markers, so the styled cell lines up.
        assert_eq!(lines[4], "csv        | **no**");
        assert_eq!(lines[5], "after");
    }

    #[test]
    fn pipes_outside_a_table_and_inside_code_are_left_alone() {
        let text = "a | b\n```\n| x | y |\n|---|---|\n```";
        assert_eq!(align_tables(text, false), text);
    }

    #[test]
    fn multibyte_text_is_kept_whole() {
        assert_eq!(styled("café **naïve** 日本"), "café <b>naïve</> 日本");
    }
}
