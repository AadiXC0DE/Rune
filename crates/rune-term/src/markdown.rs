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
    /// Selects the background of fenced code rows; empty draws none.
    pub code_bg: String,
    /// How links and file paths are written.
    pub links: LinkContext,
}

/// Where links lead and whether the terminal can follow them.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LinkContext {
    /// Whether links are written as OSC 8 hyperlinks. Off shows the target.
    pub hyperlinks: bool,
    /// The root that a path in an answer is resolved against.
    pub workspace: Option<camino::Utf8PathBuf>,
    /// The host named in `file://` links.
    pub host: String,
    /// Whether paths open in the editor through `vscode://file` links.
    pub vscode: bool,
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
            code_bg: String::new(),
            links: LinkContext::default(),
        }
    }
}

/// One styled source line, ready to wrap.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct StyledLine {
    /// Written before the first wrapped row, such as a list marker.
    pub first: String,
    /// Columns of the hanging indent that wrapped rows start with.
    pub indent: usize,
    /// Written before each wrapped row after the first: `indent` columns.
    pub continuation: String,
    /// The styled text, wrapped to the width left after the indent.
    pub body: String,
}

impl StyledLine {
    fn plain(body: String) -> Self {
        Self {
            body,
            ..Self::default()
        }
    }
}

/// Styles one complete line of prose. `width` bounds a horizontal rule.
#[must_use]
pub fn style_line(line: &str, style: &MarkdownStyle, ascii: bool, width: usize) -> StyledLine {
    style_line_with(line, style, ascii, width, true)
}

/// Styles a line that may still grow, which leaves file paths unresolved so
/// each path is looked up once, when its line is complete.
#[must_use]
pub(crate) fn style_open_line(
    line: &str,
    style: &MarkdownStyle,
    ascii: bool,
    width: usize,
) -> StyledLine {
    style_line_with(line, style, ascii, width, false)
}

fn style_line_with(
    line: &str,
    style: &MarkdownStyle,
    ascii: bool,
    width: usize,
    paths: bool,
) -> StyledLine {
    let ctx = Ctx {
        style,
        paths: paths && style.links.hyperlinks,
        autolink: true,
    };
    if !style.enabled {
        return StyledLine::plain(line.to_owned());
    }
    let lead = line
        .len()
        .saturating_sub(line.trim_start_matches([' ', '\t']).len());
    let expanded;
    let line = if line[..lead].contains('\t') {
        expanded = format!(
            "{}{}",
            crate::width::expand_tabs(&line[..lead]),
            &line[lead..]
        );
        expanded.as_str()
    } else {
        line
    };
    let indent_len = line
        .len()
        .saturating_sub(line.trim_start_matches(' ').len());
    let (indent, rest) = line.split_at(indent_len);

    if indent_len <= 3 && is_rule(rest) {
        let glyph = if ascii { "-" } else { "\u{2500}" };
        let length = width.clamp(3, 48);
        return StyledLine::plain(format!(
            "{}{}{}",
            style.dim,
            glyph.repeat(length),
            style.reset
        ));
    }
    if indent_len <= 3
        && let Some(text) = heading(rest)
    {
        return StyledLine::plain(format!(
            "{}{}{}",
            style.heading,
            inline(text, ctx),
            style.reset
        ));
    }
    if indent_len <= 3
        && let Some(text) = rest.strip_prefix('>')
    {
        let bar = if ascii { "|" } else { "\u{2502}" };
        let text = text.strip_prefix(' ').unwrap_or(text);
        let prefix = format!("{}{bar}{} ", style.dim, style.reset);
        return StyledLine {
            first: prefix.clone(),
            indent: 2,
            continuation: prefix,
            body: format!("{}{}{}", style.dim, inline(text, ctx), style.reset),
        };
    }
    let level = indent_len / 2;
    let pad = " ".repeat(level.saturating_mul(2));
    if let Some(text) = bullet(rest) {
        if let Some((done, text)) = task(text) {
            let first = if done {
                format!("{pad}{}[x]{} ", style.dim, style.reset)
            } else {
                format!("{pad}[ ] ")
            };
            let body = if done {
                format!("{}{}{}", style.dim, inline(text, ctx), style.reset)
            } else {
                inline(text, ctx)
            };
            return hanging(first, pad.len().saturating_add(4), body);
        }
        let marker = if ascii { "-" } else { "\u{2022}" };
        let first = format!("{pad}{}{marker}{} ", style.dim, style.reset);
        return hanging(first, pad.len().saturating_add(2), inline(text, ctx));
    }
    if let Some((marker, text)) = ordered(rest) {
        let first = format!("{pad}{marker} ");
        let indent = pad.len().saturating_add(marker.len()).saturating_add(1);
        return hanging(first, indent, inline(text, ctx));
    }
    StyledLine::plain(format!("{indent}{}", inline(rest, ctx)))
}

/// A list line whose wrapped rows start under its text.
fn hanging(first: String, indent: usize, body: String) -> StyledLine {
    StyledLine {
        first,
        indent,
        continuation: " ".repeat(indent),
        body,
    }
}

/// Returns whether a list item is a task, whether it is done, and its text.
fn task(text: &str) -> Option<(bool, &str)> {
    let done = match text.get(..3)? {
        "[ ]" => false,
        "[x]" | "[X]" => true,
        _ => return None,
    };
    let rest = &text[3..];
    if rest.is_empty() {
        return Some((done, rest));
    }
    rest.strip_prefix(' ').map(|rest| (done, rest))
}

/// Returns the marker and text of an ordered list item: up to nine digits,
/// `.` or `)`, then a space.
pub(crate) fn ordered(text: &str) -> Option<(&str, &str)> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=9).contains(&digits) || !matches!(text.as_bytes().get(digits), Some(b'.' | b')')) {
        return None;
    }
    let end = digits.saturating_add(1);
    text[end..]
        .strip_prefix(' ')
        .map(|rest| (&text[..end], rest))
}

/// Returns true for a thematic break: three or more of one of `-`, `*`, `_`.
pub(crate) fn is_rule(text: &str) -> bool {
    let compact: Vec<char> = text.chars().filter(|ch| *ch != ' ').collect();
    compact.len() >= 3
        && matches!(compact.first(), Some('-' | '*' | '_'))
        && compact.iter().all(|ch| Some(ch) == compact.first())
}

/// Returns the text of an ATX heading.
pub(crate) fn heading(text: &str) -> Option<&str> {
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
pub(crate) fn bullet(text: &str) -> Option<&str> {
    let mut chars = text.chars();
    let marker = chars.next()?;
    if !matches!(marker, '-' | '*' | '+') {
        return None;
    }
    text[1..].strip_prefix(' ')
}

/// What an inline pass may turn into links.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    style: &'a MarkdownStyle,
    /// Whether workspace paths are looked up and linked.
    paths: bool,
    /// Whether bare URLs and paths are linked: off inside a link's own label.
    autolink: bool,
}

/// Styles emphasis, code spans and links within a line.
fn inline(text: &str, ctx: Ctx<'_>) -> String {
    let style = ctx.style;
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

        if let Some((consumed, styled)) = image(rest, ctx).or_else(|| link(rest, ctx)) {
            out.push_str(&styled);
            at = at.saturating_add(consumed);
            continue;
        }

        let boundary = before.is_none_or(|byte| !is_path_byte(byte));
        if ctx.autolink && boundary {
            if let Some(url) = bare_url(rest) {
                if style.links.hyperlinks {
                    out.push_str(&linked(url, &format!("{}{url}{}", style.link, style.reset)));
                } else {
                    out.push_str(url);
                }
                at = at.saturating_add(url.len());
                continue;
            }
            if ctx.paths
                && let Some((consumed, styled)) = path_link(rest, &style.links, style)
            {
                out.push_str(&styled);
                at = at.saturating_add(consumed);
                continue;
            }
        }

        if let Some((consumed, inner)) =
            delimited(rest, "**", before).or_else(|| delimited(rest, "__", before))
        {
            out.push_str(&style.bold);
            out.push_str(&inline(inner, ctx));
            out.push_str(&style.reset);
            at = at.saturating_add(consumed);
            continue;
        }

        if let Some((consumed, inner)) =
            delimited(rest, "*", before).or_else(|| delimited(rest, "_", before))
        {
            out.push_str(&style.italic);
            out.push_str(&inline(inner, ctx));
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

/// Returns `text` as a hyperlink to `uri`. The identifier is derived from
/// the target, so drawing a line again writes the same bytes.
fn linked(uri: &str, text: &str) -> String {
    // FNV-1a over the target.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in uri.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    crate::width::hyperlink(uri, text, &format!("rune-{hash:016x}"))
}

/// Matches an `http://` or `https://` URL at the start of `text`: up to the
/// next whitespace, without trailing `.,;:!?` or an unbalanced `)`.
fn bare_url(text: &str) -> Option<&str> {
    let scheme = if text.starts_with("https://") {
        8
    } else if text.starts_with("http://") {
        7
    } else {
        return None;
    };
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    let mut url = &text[..end];
    loop {
        if let Some(stripped) = url.strip_suffix(['.', ',', ';', ':', '!', '?']) {
            url = stripped;
        } else if url.ends_with(')') && url.matches(')').count() > url.matches('(').count() {
            url = &url[..url.len().saturating_sub(1)];
        } else {
            break;
        }
    }
    (url.len() > scheme).then_some(url)
}

const fn is_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'/' | b'-')
}

/// Matches a path naming a file under the workspace, with an optional
/// `:line` or `:line:column`, and returns it linked.
fn path_link(text: &str, links: &LinkContext, style: &MarkdownStyle) -> Option<(usize, String)> {
    let workspace = links.workspace.as_ref()?;
    let token = text.bytes().take_while(|byte| is_path_byte(*byte)).count();
    let path = text[..token].trim_end_matches('.');
    if !path.contains(['/', '.']) || path.starts_with('/') {
        return None;
    }
    let mut end = path.len();
    let mut numbers = Vec::new();
    while numbers.len() < 2 {
        let rest = &text[end..];
        let Some(digits) = rest.strip_prefix(':') else {
            break;
        };
        let count = digits.bytes().take_while(u8::is_ascii_digit).count();
        if count == 0 {
            break;
        }
        numbers.push(&digits[..count]);
        end = end.saturating_add(1).saturating_add(count);
    }
    let relative = camino::Utf8Path::new(path);
    if relative
        .components()
        .any(|part| matches!(part, camino::Utf8Component::ParentDir))
    {
        return None;
    }
    let joined = workspace.join(relative);
    if !joined.is_file() {
        return None;
    }
    let root = workspace.canonicalize_utf8().ok()?;
    let absolute = joined.canonicalize_utf8().ok()?;
    if !absolute.starts_with(&root) {
        return None;
    }
    let absolute = absolute.as_str();
    let absolute = absolute
        .strip_prefix("\\\\?\\")
        .unwrap_or(absolute)
        .replace('\\', "/");
    let absolute = absolute.trim_start_matches('/');
    let uri = if links.vscode {
        let mut uri = format!("vscode://file/{absolute}");
        for number in &numbers {
            uri.push(':');
            uri.push_str(number);
        }
        uri
    } else {
        format!("file://{}/{absolute}", links.host)
    };
    let shown = &text[..end];
    Some((
        end,
        linked(&uri, &format!("{}{shown}{}", style.link, style.reset)),
    ))
}

/// Matches `![alt](target)` at the start of `text`.
fn image(text: &str, ctx: Ctx<'_>) -> Option<(usize, String)> {
    let rest = text.strip_prefix('!')?;
    let (consumed, alt, target) = parts(rest, true)?;
    let style = ctx.style;
    let label = format!("{}[image: {alt}]{}", style.link, style.reset);
    let styled = if style.links.hyperlinks {
        linked(target, &label)
    } else {
        format!("{label} ({target})")
    };
    Some((consumed.saturating_add(1), styled))
}

/// Splits `[label](target)` at the start of `text` into the bytes it spans,
/// the label and the target.
fn parts(text: &str, empty_label: bool) -> Option<(usize, &str, &str)> {
    let label_end = text.strip_prefix('[')?.find("](")?.saturating_add(1);
    let label = &text[1..label_end];
    let target_start = label_end.saturating_add(2);
    let after = &text[target_start..];
    let target_end = after.find(')')?;
    let target = &after[..target_end];
    if (label.is_empty() && !empty_label)
        || target.is_empty()
        || target.contains(char::is_whitespace)
    {
        return None;
    }
    let consumed = target_start.saturating_add(target_end).saturating_add(1);
    Some((consumed, label, target))
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
fn link(text: &str, ctx: Ctx<'_>) -> Option<(usize, String)> {
    let (consumed, label, target) = parts(text, false)?;
    let style = ctx.style;
    let inner = Ctx {
        autolink: false,
        ..ctx
    };
    let label = format!("{}{}{}", style.link, inline(label, inner), style.reset);
    let styled = if style.links.hyperlinks {
        linked(target, &label)
    } else {
        format!("{label} ({target})")
    };
    Some((consumed, styled))
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
        let delimiter = lines
            .get(index.saturating_add(1))
            .copied()
            .unwrap_or_default();
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
    let ctx = Ctx {
        style: &plain,
        paths: false,
        autolink: true,
    };
    crate::width::wrap_styled(&inline(cell, ctx), usize::MAX)
        .first()
        .map_or(0, |line| line.width)
}

pub(crate) fn is_row(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('|') && line.len() > 1
}

pub(crate) fn is_delimiter(line: &str) -> bool {
    let line = line.trim();
    is_row(line) && line.contains('-') && line.chars().all(|ch| matches!(ch, '|' | '-' | ':' | ' '))
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
            code_bg: String::new(),
            links: LinkContext::default(),
        }
    }

    /// The marked style with hyperlinks on and paths resolved under `workspace`.
    fn linking(workspace: Option<&camino::Utf8Path>, vscode: bool) -> MarkdownStyle {
        MarkdownStyle {
            links: LinkContext {
                hyperlinks: true,
                workspace: workspace.map(camino::Utf8Path::to_path_buf),
                host: "box".to_owned(),
                vscode,
            },
            ..marked()
        }
    }

    fn body(line: &str, style: &MarkdownStyle) -> String {
        style_line(line, style, false, 80).body
    }

    /// The targets of the hyperlinks a line opens, in order.
    fn targets(text: &str) -> Vec<String> {
        text.split("\u{1b}]8;")
            .skip(1)
            .filter_map(|rest| {
                let (params, rest) = rest.split_once(';')?;
                let (uri, _) = rest.split_once("\u{1b}\\")?;
                (!uri.is_empty() || !params.is_empty()).then(|| uri.to_owned())
            })
            .filter(|uri| !uri.is_empty())
            .collect()
    }

    fn workspace() -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().canonicalize().unwrap()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        (dir, root)
    }

    fn styled(line: &str) -> String {
        let line = style_line(line, &marked(), false, 10);
        format!("{}{}", line.first, line.body)
    }

    #[test]
    fn disabled_styling_leaves_the_line_as_written() {
        let line = "## **raw** `text`";
        assert_eq!(
            style_line(line, &MarkdownStyle::default(), false, 80).body,
            line
        );
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
        assert!(line.first.is_ascii(), "{line:?}");
        assert!(style_line("***", &marked(), true, 4).body.is_ascii());
    }

    #[test]
    fn a_link_keeps_its_label_and_carries_its_target() {
        let line = body(
            "see [the docs](https://example.com/a) now",
            &linking(None, false),
        );
        assert!(line.starts_with("see \u{1b}]8;id=rune-"), "{line}");
        assert!(
            line.contains(";https://example.com/a\u{1b}\\<a>the docs</>"),
            "{line}"
        );
        assert!(line.ends_with("\u{1b}]8;;\u{1b}\\ now"), "{line}");
        assert_eq!(
            line,
            body(
                "see [the docs](https://example.com/a) now",
                &linking(None, false)
            ),
            "a line drawn again writes the same bytes"
        );
        assert_eq!(styled("[not a link] (x)"), "[not a link] (x)");
    }

    #[test]
    fn links_fall_back_to_text_and_url_without_hyperlinks() {
        assert_eq!(
            styled("see [the docs](https://example.com/a) now"),
            "see <a>the docs</> (https://example.com/a) now"
        );
        assert_eq!(styled("[text](url with space)"), "[text](url with space)");
        let line = body("a [link](https://x.test/a) b", &linking(None, false));
        assert_eq!(targets(&line), ["https://x.test/a"]);
        assert!(!line.contains("(https://"), "{line}");
    }

    #[test]
    fn bare_urls_drop_trailing_punctuation() {
        let style = linking(None, false);
        for (line, url) in [
            ("see https://a.test/x.", "https://a.test/x"),
            ("(see https://a.test/x)", "https://a.test/x"),
            (
                "go to http://a.test/wiki/A_(b), then",
                "http://a.test/wiki/A_(b)",
            ),
            ("is it https://a.test/?q=1?!", "https://a.test/?q=1"),
            ("end https://a.test/a_b_c;", "https://a.test/a_b_c"),
        ] {
            let styled = body(line, &style);
            assert_eq!(targets(&styled), [url], "{line}");
            assert!(styled.contains(&format!("<a>{url}</>")), "{styled}");
        }
        // Without hyperlinks the text is left as written.
        assert_eq!(styled("see https://a.test/x."), "see https://a.test/x.");
        // A URL over 2,000 bytes is shown, not linked.
        let long = format!("https://a.test/{}", "a".repeat(2_100));
        let styled = body(&long, &style);
        assert!(targets(&styled).is_empty());
        assert!(styled.contains(&long));
    }

    #[test]
    fn existing_workspace_paths_are_linked_with_their_line() {
        let (_dir, root) = workspace();
        let absolute = root.as_str().trim_start_matches('/');
        let line = body(
            "see src/main.rs:12:4 and src/main.rs.",
            &linking(Some(&root), false),
        );
        assert_eq!(
            targets(&line),
            [
                format!("file://box/{absolute}/src/main.rs"),
                format!("file://box/{absolute}/src/main.rs"),
            ]
        );
        assert!(line.contains("<a>src/main.rs:12:4</>"), "{line}");
        assert!(
            line.ends_with("<a>src/main.rs</>\u{1b}]8;;\u{1b}\\."),
            "{line}"
        );
        let line = body("at src/main.rs:12", &linking(Some(&root), true));
        assert_eq!(
            targets(&line),
            [format!("vscode://file/{absolute}/src/main.rs:12")]
        );
        // A path that does not exist, and one in an unfinished line, stay plain.
        let style = linking(Some(&root), false);
        assert_eq!(body("src/missing.rs:3", &style), "src/missing.rs:3");
        assert_eq!(
            style_open_line("src/main.rs", &style, false, 80).body,
            "src/main.rs"
        );
        // Without hyperlinks nothing is looked up.
        let mut off = style.clone();
        off.links.hyperlinks = false;
        assert_eq!(body("src/main.rs:1", &off), "src/main.rs:1");
    }

    #[test]
    fn paths_outside_the_workspace_are_not_linked() {
        let (_dir, root) = workspace();
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(root.join("outside.rs"), "").unwrap();
        let style = linking(Some(&inner), false);
        for line in [
            "../outside.rs",
            "../outside.rs:3",
            "/etc/hosts",
            "./../outside.rs",
        ] {
            assert!(targets(&body(line, &style)).is_empty(), "{line}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("outside.rs"), inner.join("escape.rs")).unwrap();
            assert!(targets(&body("escape.rs", &style)).is_empty());
        }
    }

    #[test]
    fn images_render_as_labels() {
        assert_eq!(
            styled("![a cat](https://a.test/cat.png) here"),
            "<a>[image: a cat]</> (https://a.test/cat.png) here"
        );
        let line = body("![a cat](https://a.test/cat.png)", &linking(None, false));
        assert_eq!(targets(&line), ["https://a.test/cat.png"]);
        assert!(line.contains("<a>[image: a cat]</>"), "{line}");
        assert!(!line.contains("(https://"), "{line}");
        assert_eq!(styled("<b>html</b>"), "<b>html</b>");
    }

    #[test]
    fn a_pipe_table_is_aligned_and_its_delimiter_becomes_a_rule() {
        let text =
            "Results:\n| Task | Pass |\n|---|:-:|\n| pagination | yes |\n| csv | **no** |\nafter";
        let aligned = align_tables(text, true);
        let lines: Vec<&str> = aligned.lines().collect();
        assert_eq!(lines[0], "Results:");
        assert_eq!(lines[1], "Task       | Pass");
        assert!(
            lines[2].chars().all(|ch| ch == '-' || ch == '+'),
            "{}",
            lines[2]
        );
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
