//! Rendering a conversation for a terminal.
//!
//! The transcript decides what a turn looks like: which lines are the user's,
//! which are the model's, and which are the tools'. It is a pure projection, so
//! what a reader sees on a resumed session is decided by the log rather than by
//! whatever happened to be printed at the time.

use std::fmt::Write as _;

use crate::width::{grapheme_width, graphemes, str_width, truncate_to_width, wrap};

/// Who produced a line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Speaker {
    /// The user.
    User,
    /// The model's reasoning, shown apart from its answer.
    Reasoning,
    /// The model.
    Assistant,
    /// A tool call or its result.
    Tool,
    /// A note from the harness rather than from either party.
    Notice,
}

/// One entry in a transcript.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// Who produced it.
    pub speaker: Speaker,
    /// The text, already free of terminal control sequences.
    pub text: String,
    /// Whether the text may be expanded rather than summarized.
    pub expandable: bool,
}

impl Entry {
    /// Builds an entry from the user.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::User,
            text: text.into(),
            expandable: true,
        }
    }

    /// Builds an entry from the model.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Assistant,
            text: text.into(),
            expandable: true,
        }
    }

    /// Builds an entry for a tool call.
    #[must_use]
    pub fn tool(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Tool,
            text: text.into(),
            expandable: true,
        }
    }

    /// Builds an entry for the model's reasoning.
    #[must_use]
    pub fn reasoning(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Reasoning,
            text: text.into(),
            expandable: false,
        }
    }

    /// Builds an entry for a harness note.
    #[must_use]
    pub fn notice(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Notice,
            text: text.into(),
            expandable: false,
        }
    }
}

/// A run of consecutive tool calls, collapsed to one row.
///
/// A turn that calls five tools would otherwise push everything else off the
/// screen. The individual calls stay available for a reader who asks for them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ToolGroup {
    /// Names of the calls, in order.
    pub names: Vec<String>,
    /// How many of the calls reported a failure.
    pub failures: usize,
    /// Whether the group is shown expanded.
    pub expanded: bool,
}

impl ToolGroup {
    /// Builds a group from the names it covers.
    #[must_use]
    pub fn new(names: Vec<String>, failures: usize) -> Self {
        Self {
            names,
            failures,
            expanded: false,
        }
    }

    /// Returns the number of calls in the group.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Returns true when no calls are in the group.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Returns the collapsed summary row.
    #[must_use]
    pub fn summary(&self) -> String {
        let count = self.names.len();
        let names = self.names.join(", ");
        if self.failures == 0 {
            format!("{count} tool call(s): {names}")
        } else {
            format!("{count} tool call(s): {names} ({} failed)", self.failures)
        }
    }
}

/// Groups consecutive tool entries.
///
/// A run of one is left alone: collapsing a single call to a row that names it
/// would hide the result for no gain.
#[must_use]
pub fn group_tools(entries: &[Entry]) -> Vec<Result<Entry, ToolGroup>> {
    let mut out = Vec::new();
    let mut pending: Vec<&Entry> = Vec::new();

    let flush = |pending: &mut Vec<&Entry>, out: &mut Vec<Result<Entry, ToolGroup>>| {
        if pending.is_empty() {
            return;
        }
        if pending.len() == 1 {
            if let Some(entry) = pending.first() {
                out.push(Ok((*entry).clone()));
            }
        } else {
            let names: Vec<String> = pending
                .iter()
                .map(|entry| first_line(&entry.text))
                .collect();
            let failures = pending
                .iter()
                .filter(|entry| entry.text.contains("failed") || entry.text.contains("refused"))
                .count();
            out.push(Err(ToolGroup::new(names, failures)));
        }
        pending.clear();
    };

    for entry in entries {
        if entry.speaker == Speaker::Tool && entry.expandable {
            pending.push(entry);
            continue;
        }
        flush(&mut pending, &mut out);
        out.push(Ok(entry.clone()));
    }
    flush(&mut pending, &mut out);
    out
}

/// Returns the first non-empty line of a text.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// Renders a grouped transcript.
///
/// A group renders as one row unless it is expanded, in which case its entries
/// render in full. Expanding never loses content: the entries are held whole.
#[must_use]
pub fn render_grouped(entries: &[Entry], display: Display) -> String {
    let mut out = String::new();
    for item in group_tools(entries) {
        if !out.is_empty() {
            out.push('\n');
        }
        match item {
            Ok(entry) => render_entry(&entry, display, &Lanes::default(), &mut out),
            Err(group) => {
                let _ = writeln!(out, "  {}", group.summary());
            }
        }
    }
    out.trim_end().to_owned()
}

/// Whether content may be promoted into the terminal's own scrollback.
///
/// Content is promoted only once it is final. A tool result that is still
/// arriving would be written above the region the session repaints, and a later
/// frame could not correct it, so the watermark holds until the turn settles.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Watermark {
    /// Whether the current turn has finished.
    settled: bool,
    /// Entries already promoted, so nothing is promoted twice.
    promoted: usize,
}

impl Watermark {
    /// Builds a watermark for a turn in progress.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            settled: false,
            promoted: 0,
        }
    }

    /// Marks the turn as finished, which releases everything held.
    pub fn settle(&mut self) {
        self.settled = true;
    }

    /// Returns how many entries have been promoted.
    #[must_use]
    pub const fn promoted(&self) -> usize {
        self.promoted
    }

    /// Returns the entries safe to promote now.
    ///
    /// While the turn is running, nothing is final, so nothing is promoted. A
    /// caller that promotes anyway would be writing content the session can no
    /// longer correct.
    pub fn ready<'a>(&self, entries: &'a [Entry]) -> &'a [Entry] {
        if !self.settled {
            return &[];
        }
        entries.get(self.promoted..).unwrap_or(&[])
    }

    /// Records that the ready entries were promoted.
    pub fn advance(&mut self, entries: &[Entry]) {
        if self.settled {
            self.promoted = entries.len();
        }
    }
}

impl Default for Watermark {
    fn default() -> Self {
        Self::new()
    }
}

/// How much of a transcript to show.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Display {
    /// Width available, in terminal columns.
    pub width: usize,
    /// Lines a tool entry keeps before it is summarized.
    pub tool_lines: usize,
    /// Lines an assistant entry keeps before it is summarized.
    pub assistant_lines: usize,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            width: 80,
            tool_lines: 12,
            assistant_lines: 0,
        }
    }
}

/// Renders a transcript.
///
/// An assistant message is never summarized: it is the thing the reader asked
/// for. Tool output is, because it is usually long and rarely read in full.
#[must_use]
pub fn render(entries: &[Entry], display: Display) -> String {
    render_lanes(entries, display, &Lanes::default())
}

/// How a lane is drawn.
///
/// The caller supplies the escapes so the theme decides the colours rather than
/// the renderer naming them. Owned rather than borrowed because a theme resolves
/// its slots at run time, and the renderer outlives the value it was called
/// with. A default renders reasoning in the terminal's own dim attribute, which
/// every terminal understands.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Lanes {
    /// Opens a reasoning line.
    pub reasoning: String,
    /// Closes a reasoning line.
    pub reset: String,
}

impl Default for Lanes {
    fn default() -> Self {
        Self {
            reasoning: "\u{1b}[2m".to_owned(),
            reset: "\u{1b}[0m".to_owned(),
        }
    }
}

/// Renders a transcript with caller-supplied lane styling.
///
/// Separate from [`render`] so a caller with a theme can colour the reasoning
/// lane without every other caller having to say how it is drawn.
#[must_use]
pub fn render_lanes(entries: &[Entry], display: Display, lanes: &Lanes) -> String {
    let mut out = String::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        render_entry(entry, display, lanes, &mut out);
    }
    out.trim_end().to_owned()
}

/// Renders one entry into `out`.
fn render_entry(entry: &Entry, display: Display, lanes: &Lanes, out: &mut String) {
    // Reserve the lane's two marker columns without widening a narrow terminal:
    // the inline renderer clips any overflow, permanently losing those bytes.
    let width = display.width.saturating_sub(2).max(MIN_WIDTH);
    let text = sanitize(&entry.text);
    let body = if entry.speaker == Speaker::Assistant {
        AssistantRows::default().rows(&text, width)
    } else {
        wrap(&text, width)
    };

    match entry.speaker {
        Speaker::User => {
            let _ = writeln!(out, "> {}", body.first().map_or("", String::as_str));
            for line in body.iter().skip(1) {
                let _ = writeln!(out, "  {line}");
            }
        }
        Speaker::Assistant => {
            for line in &body {
                let _ = writeln!(out, "{line}");
            }
        }
        // Reasoning is drawn in a secondary colour and indented, so it is
        // visually a separate lane from the answer rather than mixed into it.
        Speaker::Reasoning => {
            for line in &body {
                let _ = writeln!(out, "{}  {line}{}", lanes.reasoning, lanes.reset);
            }
        }
        Speaker::Tool => {
            let keep = display.tool_lines.min(body.len());
            for line in body.iter().take(keep) {
                let _ = writeln!(out, "  {line}");
            }
            if keep < body.len() {
                let _ = writeln!(
                    out,
                    "  ... {} more line(s)",
                    body.len().saturating_sub(keep)
                );
            }
        }
        Speaker::Notice => {
            for line in &body {
                let _ = writeln!(out, "[{line}]");
            }
        }
    }
}

/// Minimum nonzero text width used by transcript wrapping.
pub const MIN_WIDTH: usize = 1;

/// Wrapped assistant rows, cached across appended, sanitized text deltas.
///
/// Complete source lines keep their rows and fence state. Only the unfinished
/// line is rewrapped, so completing a fence or appending code cannot move an
/// earlier line. A width change or a cleared source resets the cache.
#[derive(Clone, Debug, Default)]
pub struct AssistantRows {
    finished: Vec<String>,
    covered: usize,
    width: Option<usize>,
    fence: Option<Fence>,
}

impl AssistantRows {
    /// Returns all rows for text that grows by appending deltas.
    ///
    /// Prose uses ordinary word wrapping. Inside backtick or tilde fences,
    /// whitespace is preserved, tabs expand to eight-column stops, and soft
    /// breaks repeat the source indentation followed by `↪ `. On narrow
    /// terminals the repeated indentation shrinks to leave room for code.
    #[must_use]
    pub fn rows(&mut self, text: &str, width: usize) -> Vec<String> {
        let width = width.max(MIN_WIDTH);
        if text.len() < self.covered || self.width != Some(width) {
            *self = Self::default();
        }
        self.width = Some(width);
        let sealed = text.rfind('\n').map_or(0, |at| at.saturating_add(1));
        for line in text[self.covered..sealed].split_inclusive('\n') {
            self.finished.extend(assistant_line(
                line.strip_suffix('\n').unwrap_or(line),
                width,
                &mut self.fence,
            ));
        }
        self.covered = sealed;
        let mut rows = self.finished.clone();
        // A partial closing fence must not change the cached state until its
        // newline arrives: the next delta may still make it ordinary code.
        let mut fence = self.fence;
        rows.extend(assistant_line(&text[sealed..], width, &mut fence));
        rows
    }
}

#[derive(Clone, Copy, Debug)]
struct Fence {
    marker: u8,
    length: usize,
}

/// Recognizes a Markdown fence with at most three leading spaces.
fn fence_start(line: &str) -> Option<(Fence, &str)> {
    let rest = line.trim_start_matches(' ');
    if line.len().saturating_sub(rest.len()) > 3 {
        return None;
    }
    let marker = *rest.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let length = rest.bytes().take_while(|byte| *byte == marker).count();
    (length >= 3).then(|| (Fence { marker, length }, &rest[length..]))
}

fn assistant_line(line: &str, width: usize, fence: &mut Option<Fence>) -> Vec<String> {
    let candidate = fence_start(line);
    if let Some(open) = *fence {
        if candidate.is_some_and(|(close, rest)| {
            close.marker == open.marker && close.length >= open.length && rest.trim().is_empty()
        }) {
            *fence = None;
            return wrap(line, width);
        }
        return wrap_code(line, width);
    }
    if let Some((open, rest)) = candidate
        && (open.marker != b'`' || !rest.contains('`'))
    {
        *fence = Some(open);
    }
    wrap(line, width)
}

/// Hard wraps code by grapheme, retaining every space instead of word breaks.
fn wrap_code(source: &str, width: usize) -> Vec<String> {
    let mut expanded = String::new();
    let mut column = 0_usize;
    for cluster in graphemes(source) {
        if cluster == "\t" {
            let spaces = 8_usize.saturating_sub(column % 8);
            expanded.extend(std::iter::repeat_n(' ', spaces));
            column = column.saturating_add(spaces);
        } else {
            expanded.push_str(cluster);
            column = column.saturating_add(grapheme_width(cluster));
        }
    }
    let indent = expanded.bytes().take_while(|byte| *byte == b' ').count();
    let marker = if width >= 3 { "↪ " } else { "↪" };
    // Leave at least two cells for a wide grapheme whenever possible.
    let prefix = " ".repeat(indent.min(width.saturating_sub(4))) + marker;
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0_usize;
    let mut has_source = false;
    for cluster in graphemes(&expanded) {
        let columns = grapheme_width(cluster);
        if has_source && used.saturating_add(columns) > width {
            rows.push(std::mem::take(&mut row));
            // Below three columns the marker may need to shrink or disappear
            // to accommodate a wide glyph without clipping its code bytes.
            let (shown, columns_used) = truncate_to_width(&prefix, width.saturating_sub(columns));
            shown.clone_into(&mut row);
            used = columns_used;
        }
        row.push_str(cluster);
        used = used.saturating_add(columns);
        has_source = true;
    }
    rows.push(row);
    rows
}

/// Removes control sequences a terminal would act on.
///
/// Model output and tool output both reach a terminal unescaped otherwise, and a
/// carriage return or an erase sequence in that stream would rewrite what the
/// reader already saw. Newlines and tabs are kept; everything else in the
/// control range is dropped.
#[must_use]
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' | '\t' => out.push(ch),
            // An escape sequence runs until a byte that ends it. Dropping only
            // the escape byte would leave the parameters as text.
            '\u{1b}' => {
                consume_escape(&mut chars);
            }
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}

/// Consumes the rest of an escape sequence, including its introducer.
fn consume_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let Some(&next) = chars.peek() else {
        return;
    };
    match next {
        // A control sequence runs until a final byte in the range `@` to `~`.
        '[' => {
            chars.next();
            for ch in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&ch) {
                    return;
                }
            }
        }
        // An operating system command runs until a bell or a string terminator.
        ']' | 'P' | '^' | '_' => {
            chars.next();
            while let Some(ch) = chars.next() {
                if ch == '\u{7}' {
                    return;
                }
                if ch == '\u{1b}' && chars.peek() == Some(&'\\') {
                    chars.next();
                    return;
                }
            }
        }
        // An intermediate byte starts a sequence that runs to a final byte, as
        // in a character-set designation, so the introducer and both bytes go.
        '\u{20}'..='\u{2f}' => {
            chars.next();
            for ch in chars.by_ref() {
                if ('\u{30}'..='\u{7e}').contains(&ch) {
                    return;
                }
            }
        }
        // A two-character sequence: the designator is the whole sequence.
        _ => {
            chars.next();
        }
    }
}

/// Renders a single line of a prompt echo.
///
/// Used where the caller echoes input itself rather than letting the terminal
/// do it. A pasted line break is drawn as a visible mark, so the input stays on
/// one row.
#[must_use]
pub fn render_prompt(prompt: &str, input: &str, width: usize) -> String {
    let room = width.saturating_sub(str_width(prompt));
    let input = crate::editor::displayed(input);
    let (shown, _) = truncate_to_width(&input, room.max(1));
    format!("{prompt}{shown}")
}

/// Renders an editable prompt and its caret column in a horizontal viewport.
///
/// `column` is the display width before the caret, as returned by the line
/// editor. Long drafts scroll by whole grapheme clusters, leaving a visible
/// cell for the caret even when it is at the end of the input. The prompt is
/// kept at the left, shortened only when the terminal has no room beside it.
#[must_use]
pub fn render_prompt_at(prompt: &str, input: &str, column: usize, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }
    let (prompt, prompt_width) = truncate_to_width(prompt, width.saturating_sub(1));
    let room = width.saturating_sub(prompt_width);
    let input = crate::editor::displayed(input);
    let column = column.min(str_width(&input));
    let offset = column.saturating_sub(room.saturating_sub(1));
    let mut skipped_columns = 0_usize;
    let mut skipped_bytes = 0_usize;
    for cluster in graphemes(&input) {
        if skipped_columns >= offset {
            break;
        }
        skipped_columns = skipped_columns.saturating_add(grapheme_width(cluster));
        skipped_bytes = skipped_bytes.saturating_add(cluster.len());
    }
    let (shown, _) = truncate_to_width(&input[skipped_bytes..], room);
    let caret = prompt_width.saturating_add(column.saturating_sub(skipped_columns));
    (format!("{prompt}{shown}"), caret)
}

/// Renders draft line breaks as separate rows with an editable caret.
///
/// `column` counts the displayed prefix, including one cell for each newline
/// and tab, as returned by [`crate::editor::Composer::cursor_column`]. Each
/// logical line uses a horizontal viewport; the active line scrolls to its
/// caret. Continuations align with the text after the prompt marker.
#[must_use]
pub fn render_draft_at(
    prompt: &str,
    input: &str,
    mut column: usize,
    width: usize,
) -> (Vec<String>, (u16, u16)) {
    if !input.contains('\n') {
        let (row, caret) = render_prompt_at(prompt, input, column, width);
        return (vec![row], (0, u16::try_from(caret).unwrap_or(u16::MAX)));
    }
    column = column.min(str_width(&crate::editor::displayed(input)));
    let continuation = " ".repeat(str_width(prompt).min(width.saturating_sub(1)));
    let mut rows = Vec::new();
    let mut caret = (0, 0);
    let mut located = false;
    for (index, line) in input.split('\n').enumerate() {
        let line_width = str_width(&crate::editor::displayed(line));
        let active = !located && column <= line_width;
        let marker = if index == 0 { prompt } else { &continuation };
        let (row, col) = render_prompt_at(marker, line, if active { column } else { 0 }, width);
        rows.push(row);
        if active {
            caret = (
                u16::try_from(index).unwrap_or(u16::MAX),
                u16::try_from(col).unwrap_or(u16::MAX),
            );
            located = true;
        } else if !located {
            column = column.saturating_sub(line_width.saturating_add(1));
        }
    }
    (rows, caret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_draft_carets_follow_graphemes_and_empty_lines() {
        let mut composer = crate::editor::Composer::new();
        composer.insert("界\n\ne\u{301}\t👩‍💻\n");
        for expected in [
            (3, 2),
            (2, 6),
            (2, 4),
            (2, 3),
            (2, 2),
            (1, 2),
            (0, 4),
            (0, 2),
        ] {
            let (rows, caret) =
                render_draft_at("> ", composer.text(), composer.cursor_column(), 80);
            assert_eq!(rows, ["> 界", "  ", "  e\u{301} 👩‍💻", "  "]);
            assert_eq!(caret, expected);
            composer.move_left();
        }
    }

    #[test]
    fn multiline_drafts_scroll_only_the_edited_line_and_bound_narrow_carets() {
        let input = "first\nab書👋🏽e\u{301}Z\nlast";
        for width in 0..=12 {
            for column in 0..=str_width(&crate::editor::displayed(input)) {
                let (rows, caret) = render_draft_at("> ", input, column, width);
                assert_eq!(rows.len(), 3);
                assert!(rows.iter().all(|row| str_width(row) <= width));
                assert!(usize::from(caret.1) < width.max(1));
                assert!(caret.0 < 3);
            }
        }
        let (rows, caret) = render_draft_at("> ", input, 14, 8);
        assert_eq!(rows, ["> first", "  👋🏽e\u{301}Z", "  last"]);
        assert_eq!(caret, (1, 6));
    }

    #[test]
    fn an_assistant_message_renders_in_full() {
        let text = "one\n".repeat(40);
        let rendered = render(&[Entry::assistant(text)], Display::default());
        assert_eq!(
            rendered.lines().count(),
            40,
            "assistant text was summarized"
        );
    }

    #[test]
    fn fenced_code_preserves_spaces_and_indents_its_continuations() {
        let text = "before\n```rust\n    abcdefghijkl  mnop\n\n```\nafter";
        let rendered = render(
            &[Entry::assistant(text)],
            Display {
                width: 12,
                ..Display::default()
            },
        );
        assert_eq!(
            rendered,
            "before\n```rust\n    abcdef\n    ↪ ghij\n    ↪ kl  \n    ↪ mnop\n\n```\nafter"
        );
    }

    #[test]
    fn streamed_code_keeps_completed_rows_stable_at_every_character() {
        let mut cache = AssistantRows::default();
        let mut text = "```rust\n".to_owned();
        let mut previous = cache.rows(&text, 18);
        for ch in "    call(\"alpha  beta\",  gamma);\n```\nprose after".chars() {
            text.push(ch);
            let rows = cache.rows(&text, 18);
            let completed = previous.len().saturating_sub(1);
            assert_eq!(rows[..completed], previous[..completed], "{text:?}");
            assert_eq!(rows, AssistantRows::default().rows(&text, 18));
            previous = rows;
        }
        assert!(previous.iter().any(|row| row.starts_with("    ↪ ")));
    }

    #[test]
    fn fences_close_only_with_the_matching_marker_and_sufficient_length() {
        for opening in ["````rust", "~~~~rust", "   ````rust"] {
            let close = if opening.starts_with('~') {
                "~~~~"
            } else {
                "````"
            };
            let text = format!(
                "{opening}\n```\n~~~\n````suffix\n    abcdefghijkl\n{close}\nwords after the code"
            );
            let rows = AssistantRows::default().rows(&text, 12);
            assert!(rows.iter().any(|row| row == "    ↪ ijkl"), "{rows:?}");
            assert_eq!(
                &rows[rows.len().saturating_sub(2)..],
                &["words after", "the code"]
            );
        }
        // Inline backticks, four-space indentation and a backtick in the info
        // string are prose, so they cannot put subsequent lines into code mode.
        for invalid in ["inline ```rust", "    ```rust", "```ru`st"] {
            let text = format!("{invalid}\nwords after the code");
            let rows = AssistantRows::default().rows(&text, 12);
            assert_eq!(rows, wrap(&text, 12));
        }
    }

    #[test]
    fn code_tabs_and_graphemes_fit_narrow_rows_without_losing_indentation() {
        let rows = AssistantRows::default().rows("~~~\n\t書e\u{301}👩‍💻書\n~~~", 12);
        assert_eq!(
            rows,
            [
                "~~~",
                "        書e\u{301}",
                "        ↪ 👩‍💻",
                "        ↪ 書",
                "~~~"
            ]
        );
        assert!(rows.iter().all(|row| str_width(row) <= 12));
        for width in 1..=12 {
            let rows = AssistantRows::default().rows("```\n                abcdef\n```", width);
            assert!(rows.iter().all(|row| str_width(row) <= width), "{rows:?}");
            let recovered: String = rows
                .iter()
                .flat_map(|row| row.chars())
                .filter(char::is_ascii_alphabetic)
                .collect();
            assert_eq!(recovered, "abcdef");
        }
    }

    #[test]
    fn assistant_rows_reset_fence_state_on_clear_and_rewrap_on_resize() {
        let text = "```\n    abcdefghijkl\n```\nafter";
        let mut cache = AssistantRows::default();
        let _ = cache.rows(text, 18);
        assert_eq!(
            cache.rows(text, 10),
            AssistantRows::default().rows(text, 10)
        );
        assert_eq!(
            cache.rows(text, 30),
            AssistantRows::default().rows(text, 30)
        );
        let _ = cache.rows("```\ncode\n", 12);
        let _ = cache.rows("", 12);
        assert_eq!(
            cache.rows("words after the code", 12),
            wrap("words after the code", 12)
        );
    }

    #[test]
    fn fence_state_does_not_leak_between_assistant_entries_or_other_lanes() {
        let display = Display {
            width: 14,
            ..Display::default()
        };
        let prose = "words after the code";
        let rendered = render(
            &[Entry::assistant("```\ncode"), Entry::assistant(prose)],
            display,
        );
        assert!(
            rendered.ends_with(&wrap(prose, 12).join("\n")),
            "{rendered}"
        );
        let text = "```\n    abcdefghijkl\n```";
        let expected: Vec<String> = wrap(text, 12)
            .into_iter()
            .enumerate()
            .map(|(index, row)| format!("{}{row}", if index == 0 { "> " } else { "  " }))
            .collect();
        assert_eq!(render(&[Entry::user(text)], display), expected.join("\n"));
    }

    #[test]
    fn tool_output_is_summarized_and_says_how_much_was_hidden() {
        let display = Display {
            tool_lines: 3,
            ..Display::default()
        };
        let text = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render(&[Entry::tool(text)], display);
        assert!(rendered.contains("line 3"), "{rendered}");
        assert!(!rendered.contains("line 4"), "{rendered}");
        assert!(rendered.contains("7 more line(s)"), "{rendered}");
    }

    #[test]
    fn tool_output_within_the_bound_is_not_summarized() {
        let display = Display {
            tool_lines: 5,
            ..Display::default()
        };
        let rendered = render(&[Entry::tool("one\ntwo")], display);
        assert!(rendered.contains("two"));
        assert!(!rendered.contains("more line"), "{rendered}");
    }

    #[test]
    fn a_user_line_is_marked_so_it_is_distinguishable() {
        let rendered = render(&[Entry::user("hello")], Display::default());
        assert_eq!(rendered, "> hello");
    }

    #[test]
    fn a_user_line_wraps_under_its_marker() {
        let display = Display {
            width: 24,
            ..Display::default()
        };
        let rendered = render(&[Entry::user("a".repeat(60))], display);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.len() > 1, "{rendered}");
        assert!(lines[0].starts_with("> "));
        for line in lines.iter().skip(1) {
            assert!(
                line.starts_with("  "),
                "continuation lost its indent: {rendered}"
            );
        }
    }

    #[test]
    fn reasoning_is_drawn_in_its_own_lane_before_the_answer() {
        // Thinking and the answer are separate things, so they are drawn apart
        // rather than interleaved. The lane also has to survive the turn, which
        // is why the text is an entry rather than a transient.
        let entries = vec![
            Entry::reasoning("weighing the options"),
            Entry::assistant("the answer"),
        ];
        let rendered = render_lanes(
            &entries,
            Display::default(),
            &Lanes {
                reasoning: "<dim>".to_owned(),
                reset: "<reset>".to_owned(),
            },
        );
        let thinking = rendered.find("weighing the options").expect("reasoning");
        let answer = rendered.find("the answer").expect("the answer");
        assert!(thinking < answer, "reasoning was drawn after the answer");
        assert!(
            rendered.contains("<dim>"),
            "the lane was not styled: {rendered:?}"
        );
        assert!(rendered.contains("<reset>"), "{rendered:?}");
    }

    #[test]
    fn a_default_lane_still_marks_reasoning_apart() {
        // A caller with no theme still gets reasoning distinguished, rather
        // than mixed into the answer.
        let entries = vec![Entry::reasoning("thinking")];
        let rendered = render(&entries, Display::default());
        assert_ne!(rendered, "thinking", "reasoning was not set apart");
        assert!(rendered.contains("thinking"), "{rendered:?}");
    }

    #[test]
    fn a_notice_is_bracketed() {
        let rendered = render(&[Entry::notice("cancelled")], Display::default());
        assert_eq!(rendered, "[cancelled]");
    }

    #[test]
    fn entries_are_separated_by_one_blank_line() {
        let rendered = render(
            &[Entry::user("hi"), Entry::assistant("hello")],
            Display::default(),
        );
        assert_eq!(rendered, "> hi\n\nhello");
    }

    #[test]
    fn an_erase_sequence_is_removed_rather_than_printed() {
        // A terminal would act on these, rewriting what the reader already saw.
        let rendered = sanitize("keep\u{1b}[2Kdrop");
        assert_eq!(rendered, "keepdrop");
    }

    #[test]
    fn a_carriage_return_is_removed() {
        assert_eq!(sanitize("over\rwritten"), "overwritten");
    }

    #[test]
    fn a_hyperlink_sequence_is_removed_whole() {
        let rendered = sanitize("see \u{1b}]8;;https://example.com\u{7}here\u{1b}]8;;\u{7} now");
        assert_eq!(rendered, "see here now");
    }

    #[test]
    fn a_string_terminator_ends_a_hyperlink() {
        let rendered = sanitize("a\u{1b}]8;;http://x\u{1b}\\b");
        assert_eq!(rendered, "ab");
    }

    #[test]
    fn a_character_set_escape_is_removed_whole() {
        // An intermediate byte and its final byte both belong to the sequence,
        // so the following plain text is the first thing kept.
        assert_eq!(sanitize("a\u{1b}(Bplain"), "aplain");
    }

    #[test]
    fn a_two_character_escape_keeps_what_follows() {
        assert_eq!(sanitize("a\u{1b}=b"), "ab");
    }

    #[test]
    fn newlines_tabs_and_plain_text_survive() {
        assert_eq!(sanitize("a\nb\tc"), "a\nb\tc");
    }

    #[test]
    fn a_bell_alone_is_dropped() {
        assert_eq!(sanitize("a\u{7}b"), "ab");
    }

    #[test]
    fn an_unterminated_escape_does_not_loop_or_panic() {
        assert_eq!(sanitize("tail\u{1b}["), "tail");
        assert_eq!(sanitize("tail\u{1b}]"), "tail");
        assert_eq!(sanitize("\u{1b}"), "");
    }

    #[test]
    fn non_ascii_text_is_preserved() {
        assert_eq!(sanitize("héllo → wörld"), "héllo → wörld");
    }

    #[test]
    fn wide_characters_still_wrap_on_display_width() {
        let display = Display {
            width: 20,
            ..Display::default()
        };
        // Each glyph is two columns wide, so only a few fit per line.
        let rendered = render(&[Entry::assistant("書".repeat(30))], display);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.len() >= 2, "{rendered}");
        for line in lines {
            assert!(str_width(line) <= 20, "a line overflowed: {line:?}");
        }
    }

    #[test]
    fn a_narrow_terminal_still_renders() {
        let display = Display {
            width: 1,
            ..Display::default()
        };
        let rendered = render(&[Entry::assistant("hello world")], display);
        assert!(!rendered.is_empty());
        // Even below the space reserved for markers, the answer uses the
        // terminal's actual width rather than overflowing and being clipped.
        for line in rendered.lines() {
            assert!(str_width(line) <= display.width, "{line:?}");
        }
    }

    #[test]
    fn consecutive_tool_calls_collapse_to_one_summary_row() {
        let entries: Vec<Entry> = (1..=5)
            .map(|n| Entry::tool(format!("read_file {n}")))
            .collect();
        let rendered = render_grouped(&entries, Display::default());
        assert_eq!(rendered.lines().count(), 1, "{rendered}");
        assert!(rendered.contains("5 tool call(s)"), "{rendered}");
    }

    #[test]
    fn a_single_tool_call_is_not_collapsed() {
        // A row naming one call would hide its result for no gain.
        let rendered = render_grouped(&[Entry::tool("read_file src/main.rs")], Display::default());
        assert!(rendered.contains("read_file src/main.rs"), "{rendered}");
        assert!(!rendered.contains("tool call(s)"), "{rendered}");
    }

    #[test]
    fn a_non_tool_entry_ends_a_group() {
        let entries = vec![
            Entry::tool("first tool"),
            Entry::tool("second tool"),
            Entry::assistant("an answer"),
        ];
        let grouped = group_tools(&entries);
        assert_eq!(grouped.len(), 2, "{grouped:#?}");
        assert!(matches!(&grouped[0], Err(group) if group.len() == 2));
        assert!(matches!(&grouped[1], Ok(entry) if entry.speaker == Speaker::Assistant));
    }

    #[test]
    fn a_group_counts_the_calls_that_failed() {
        let entries = vec![
            Entry::tool("read_file ok"),
            Entry::tool("grep_files refused: no matches"),
        ];
        let grouped = group_tools(&entries);
        let Err(group) = &grouped[0] else {
            panic!("the calls were not grouped: {grouped:#?}");
        };
        assert_eq!(group.failures, 1);
        assert!(group.summary().contains("1 failed"), "{}", group.summary());
    }

    #[test]
    fn expanding_a_group_restores_every_row() {
        // Expanding must never lose content, so the entries are kept whole.
        let entries: Vec<Entry> = (1..=3)
            .map(|n| Entry::tool(format!("line {n}\nmore {n}")))
            .collect();
        let collapsed = render_grouped(&entries, Display::default());
        assert_eq!(collapsed.lines().count(), 1, "{collapsed}");

        let expanded = render(&entries, Display::default());
        for n in 1..=3 {
            assert!(expanded.contains(&format!("line {n}")), "{expanded}");
            assert!(expanded.contains(&format!("more {n}")), "{expanded}");
        }
    }

    #[test]
    fn nothing_is_promoted_while_a_turn_is_running() {
        // A result still arriving would be written above the region the session
        // repaints, where no later frame can correct it.
        let entries = vec![Entry::assistant("partial")];
        let watermark = Watermark::new();
        assert!(watermark.ready(&entries).is_empty());
        assert_eq!(watermark.promoted(), 0);
    }

    #[test]
    fn settling_releases_everything_held() {
        let entries = vec![Entry::assistant("one"), Entry::assistant("two")];
        let mut watermark = Watermark::new();
        watermark.settle();
        assert_eq!(watermark.ready(&entries).len(), 2);
        watermark.advance(&entries);
        assert_eq!(watermark.promoted(), 2);
        // Nothing is promoted twice.
        assert!(watermark.ready(&entries).is_empty());
    }

    #[test]
    fn an_unsettled_watermark_never_advances() {
        let entries = vec![Entry::assistant("one")];
        let mut watermark = Watermark::new();
        watermark.advance(&entries);
        assert_eq!(watermark.promoted(), 0);
    }

    #[test]
    fn an_empty_transcript_renders_nothing() {
        assert_eq!(render(&[], Display::default()), "");
    }

    #[test]
    fn an_empty_entry_renders_its_marker_only() {
        assert_eq!(render(&[Entry::user("")], Display::default()), ">");
    }

    #[test]
    fn a_control_sequence_in_model_output_cannot_rewrite_the_screen() {
        let entry = Entry::assistant("real\u{1b}[1AFAKE");
        let rendered = render(&[entry], Display::default());
        assert_eq!(rendered, "realFAKE");
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    }

    #[test]
    fn a_prompt_echo_truncates_rather_than_wrapping() {
        let rendered = render_prompt("> ", &"x".repeat(100), 20);
        assert_eq!(str_width(&rendered), 20);
        assert!(rendered.starts_with("> "));
    }

    #[test]
    fn an_editable_prompt_scrolls_to_the_caret_and_back() {
        let mut composer = crate::editor::Composer::new();
        let draft = "a".repeat(160) + "TAIL-END";
        composer.insert(&draft);
        let render = |composer: &crate::editor::Composer| {
            render_prompt_at("> ", composer.text(), composer.cursor_column(), 80)
        };
        let (row, caret) = render(&composer);
        assert!(row.ends_with("TAIL-END"), "{row}");
        assert_eq!(caret, 79);
        assert_eq!(str_width(&row), caret);

        composer.move_left();
        let (row, caret) = render(&composer);
        assert!(row.ends_with("TAIL-END"), "{row}");
        assert_eq!(row.chars().nth(caret), Some('D'));
        composer.delete_forward();
        composer.insert("Z");
        assert!(render(&composer).0.ends_with("TAIL-ENZ"));

        composer.move_home();
        assert_eq!(render(&composer), ("> ".to_owned() + &"a".repeat(78), 2));
        composer.move_end();
        assert!(render(&composer).0.ends_with("TAIL-ENZ"));
        assert_eq!(composer.text(), "a".repeat(160) + "TAIL-ENZ");
    }

    #[test]
    fn prompt_scrolling_preserves_wide_and_combining_clusters() {
        let input = "ab書👋🏽e\u{301}Z";
        let (row, caret) = render_prompt_at("> ", input, str_width(input), 8);
        assert_eq!(row, "> 👋🏽e\u{301}Z");
        assert_eq!(caret, 6);
        let (row, caret) = render_prompt_at("> ", input, 4, 8);
        assert_eq!(row, "> ab書👋🏽");
        assert_eq!(caret, 6);
    }

    #[test]
    fn short_drafts_keep_their_text_and_caret_position() {
        assert_eq!(
            render_prompt_at("> ", "abcd", 1, 80),
            ("> abcd".to_owned(), 3)
        );
        assert_eq!(render_prompt_at("> ", "", 0, 80), ("> ".to_owned(), 2));
        assert_eq!(
            render_prompt_at("> ", "a\n\t書", 5, 80),
            ("> a⏎ 書".to_owned(), 7)
        );
    }

    #[test]
    fn prompt_carets_stay_inside_even_the_smallest_widths() {
        let input = "書👋🏽e\u{301}Z";
        for width in 0..=12 {
            for column in [0, 2, 4, 5, 6, usize::MAX] {
                let (row, caret) = render_prompt_at("> ", input, column, width);
                assert!(str_width(&row) <= width, "{row:?} at width {width}");
                assert!(caret < width.max(1), "caret {caret} at width {width}");
            }
        }
    }
}
