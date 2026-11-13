//! Rendering a conversation for a terminal.
//!
//! The transcript decides what a turn looks like: which lines are the user's,
//! which are the model's, and which are the tools'. It is a pure projection, so
//! what a reader sees on a resumed session is decided by the log rather than by
//! whatever happened to be printed at the time.

use std::fmt::Write as _;

use crate::width::{expand_tabs, grapheme_width, graphemes, str_width, truncate_to_width, wrap};

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

/// How serious a harness note is.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Severity {
    /// Something the reader may want to know.
    #[default]
    Info,
    /// Something that went wrong but did not stop the work.
    Warning,
    /// Something that stopped the work.
    Error,
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
    /// How serious a notice is.
    pub severity: Severity,
    /// What the reader can do about an error.
    pub hint: Option<String>,
}

impl Entry {
    /// Builds an entry from the user.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::User,
            text: text.into(),
            expandable: true,
            severity: Severity::Info,
            hint: None,
        }
    }

    /// Builds an entry from the model.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Assistant,
            text: text.into(),
            expandable: true,
            severity: Severity::Info,
            hint: None,
        }
    }

    /// Builds an entry for a tool call.
    #[must_use]
    pub fn tool(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Tool,
            text: text.into(),
            expandable: true,
            severity: Severity::Info,
            hint: None,
        }
    }

    /// Builds an entry for the model's reasoning.
    #[must_use]
    pub fn reasoning(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Reasoning,
            text: text.into(),
            expandable: false,
            severity: Severity::Info,
            hint: None,
        }
    }

    /// Builds an entry for a harness note.
    #[must_use]
    pub fn notice(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Notice,
            text: text.into(),
            expandable: false,
            severity: Severity::Info,
            hint: None,
        }
    }

    /// Builds a harness note about something that went wrong but did not stop
    /// the work.
    #[must_use]
    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::notice(text)
        }
    }

    /// Builds a harness note about something that stopped the work, with what
    /// the reader can do about it.
    #[must_use]
    pub fn error(text: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            severity: Severity::Error,
            hint,
            ..Self::notice(text)
        }
    }
}

/// Plain logical lines with an explicit speaker on every line. No wrapping,
/// Markdown styling, tool collapsing, or terminal control sequences are used.
#[must_use]
pub fn accessible_lines(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .flat_map(|entry| {
            let label = match entry.speaker {
                Speaker::User => "User",
                Speaker::Reasoning => "Reasoning",
                Speaker::Assistant => "Assistant",
                Speaker::Tool => "Tool",
                Speaker::Notice => "Notice",
            };
            let warning = if entry.severity == Severity::Warning {
                "warning: "
            } else {
                ""
            };
            let hint = entry.hint.as_deref().map(sanitize).unwrap_or_default();
            sanitize(&entry.text)
                .lines()
                .map(|line| format!("{label}: {warning}{line}"))
                .chain(hint.lines().map(|line| format!("{label}: {line}")))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Appends plain lines using newlines, without cursor positioning or erasure.
#[must_use]
pub fn append_lines(lines: &[String]) -> Vec<u8> {
    let mut out = String::new();
    for line in lines {
        out.push_str(&sanitize(line));
        out.push('\n');
    }
    out.into_bytes()
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
    /// Use ASCII for decorations while preserving source text.
    pub ascii: bool,
    /// Lines a tool entry keeps before it is summarized.
    pub tool_lines: usize,
    /// Lines an assistant entry keeps before it is summarized.
    pub assistant_lines: usize,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            width: 80,
            ascii: false,
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

/// What an expanded entry shows in place of its recorded text.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Expansion {
    /// Nothing more than the recorded text.
    Recorded,
    /// Text drawn as the entry's body, in full.
    Text(String),
    /// Rows drawn as they are, such as a rendered diff.
    Rows(Vec<String>),
}

/// The viewer's rows with where each entry and the selection start.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DetailRows {
    /// Every row, in order.
    pub rows: Vec<String>,
    /// The selected entry's header row.
    pub selected: Option<usize>,
    /// The first row of each entry.
    pub starts: Vec<usize>,
}

/// Whether an entry can be expanded and collapsed in the viewer.
fn collapsible(entry: &Entry) -> bool {
    match entry.speaker {
        Speaker::Tool => entry.expandable,
        Speaker::Reasoning => true,
        _ => false,
    }
}

/// Selection and disclosure state for the transcript viewer.
///
/// A tool entry shows its recorded text until it is expanded, when it shows
/// what the caller supplied for it, uncapped. A reasoning entry shows one
/// `thought:` row until it is expanded. Source entries are kept intact.
#[derive(Debug, Default)]
pub struct ToolDetails {
    entries: Vec<Entry>,
    expanded: Vec<bool>,
    expansions: Vec<Option<Expansion>>,
    selected: Option<usize>,
    cache: Option<SharedBodies>,
}

/// The rows of collapsed viewer entries, kept between openings of the viewer
/// so that only entries added since the last opening are rendered.
///
/// Each body is stored with the entry it was rendered from and is used again
/// only while the entry at that position is equal, so a transcript that was
/// replaced (a new session, a compaction) renders afresh by itself.
#[derive(Debug, Default)]
pub struct BodyCache {
    display: Option<Display>,
    bodies: Vec<(Entry, Vec<String>)>,
    renders: usize,
}

impl BodyCache {
    /// How many entry bodies were rendered rather than reused.
    #[must_use]
    pub const fn renders(&self) -> usize {
        self.renders
    }

    /// The rows of `entry` at `index`, rendered by `render` when not kept.
    fn body(
        &mut self,
        index: usize,
        entry: &Entry,
        display: Display,
        render: impl FnOnce() -> Vec<String>,
    ) -> Vec<String> {
        if self.display != Some(display) {
            self.display = Some(display);
            self.bodies.clear();
        }
        if let Some((kept, rows)) = self.bodies.get(index)
            && kept == entry
        {
            return rows.clone();
        }
        let rows = render();
        self.renders = self.renders.saturating_add(1);
        self.bodies.truncate(index);
        if self.bodies.len() == index {
            self.bodies.push((entry.clone(), rows.clone()));
        }
        rows
    }
}

/// A body cache shared by the viewer's openings.
pub type SharedBodies = std::sync::Arc<std::sync::Mutex<BodyCache>>;

impl ToolDetails {
    /// Starts with every entry collapsed and nothing selected.
    #[must_use]
    pub fn new(entries: Vec<Entry>) -> Self {
        let mut details = Self::default();
        details.replace(entries);
        details
    }

    /// Renders collapsed entries through `cache`, reusing the rows it holds.
    #[must_use]
    pub fn with_cache(mut self, cache: SharedBodies) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Replaces the entries, keeping each unchanged entry's state by index.
    pub fn replace(&mut self, entries: Vec<Entry>) {
        let kept = self
            .entries
            .iter()
            .zip(&entries)
            .take_while(|(old, new)| old == new)
            .count();
        self.expanded.truncate(kept);
        self.expansions.truncate(kept);
        self.expanded.resize(entries.len(), false);
        self.expansions.resize(entries.len(), None);
        self.entries = entries;
        if self.selected.is_some_and(|index| index >= kept) {
            self.selected = None;
        }
    }

    /// The entries as given.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The selected entry.
    #[must_use]
    pub const fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Selects the next or previous collapsible entry, wrapping at either end.
    pub fn select(&mut self, backwards: bool) {
        let tools: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| collapsible(entry))
            .map(|(index, _)| index)
            .collect();
        self.selected = if backwards {
            tools
                .iter()
                .rev()
                .find(|&&index| self.selected.is_some_and(|selected| index < selected))
                .or_else(|| tools.last())
        } else {
            tools
                .iter()
                .find(|&&index| self.selected.is_none_or(|selected| index > selected))
                .or_else(|| tools.first())
        }
        .copied();
    }

    /// Selects the first collapsible entry with a row at or after `row`, or
    /// the last one before it, given the starts [`Self::rows`] reported.
    pub fn select_from_row(&mut self, row: usize, starts: &[usize]) {
        let candidates = || {
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| collapsible(entry))
                .map(|(index, _)| index)
        };
        let end = |index: usize| {
            starts
                .get(index.saturating_add(1))
                .copied()
                .unwrap_or(usize::MAX)
        };
        self.selected = candidates()
            .find(|&index| end(index) > row)
            .or_else(|| candidates().next_back());
    }

    /// Toggles the selected entry; without a selection this is a no-op.
    pub fn toggle(&mut self) {
        if let Some(index) = self.selected {
            self.expanded[index] = !self.expanded[index];
        }
    }

    /// Expands every collapsible entry, or collapses them all when all are
    /// expanded already.
    pub fn toggle_all(&mut self) {
        let all = self
            .entries
            .iter()
            .zip(&self.expanded)
            .all(|(entry, expanded)| !collapsible(entry) || *expanded);
        for (entry, expanded) in self.entries.iter().zip(&mut self.expanded) {
            if collapsible(entry) {
                *expanded = !all;
            }
        }
    }

    /// Expanded tool entries whose expansion is not known yet.
    #[must_use]
    pub fn pending(&self) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&index| {
                self.expanded[index]
                    && self.entries[index].speaker == Speaker::Tool
                    && self.expansions[index].is_none()
            })
            .collect()
    }

    /// Drops every supplied expansion, so [`Self::pending`] asks again.
    pub fn forget_expansions(&mut self) {
        self.expansions.fill(None);
    }

    /// Supplies what an expanded entry shows.
    pub fn set_expansion(&mut self, index: usize, expansion: Expansion) {
        if let Some(slot) = self.expansions.get_mut(index) {
            *slot = Some(expansion);
        }
    }

    /// Renders rows with a header row on the selected and expanded entries.
    #[must_use]
    pub fn rows(&self, display: Display) -> DetailRows {
        self.render(display, false)
    }

    /// Renders every entry expanded, without header rows.
    #[must_use]
    pub fn expanded_rows(&self, display: Display) -> DetailRows {
        self.render(display, true)
    }

    fn render(&self, display: Display, everything: bool) -> DetailRows {
        let width = display.width.max(MIN_WIDTH);
        let mut out: Vec<String> = Vec::new();
        let mut selected_row = None;
        let mut starts = Vec::with_capacity(self.entries.len());
        let push = |out: &mut Vec<String>, text: &str| {
            out.extend(text.trim_end().lines().map(str::to_owned));
        };
        let mut cache = self.cache.as_ref().map(|cache| {
            cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        for (index, entry) in self.entries.iter().enumerate() {
            if index > 0 {
                out.push(String::new());
            }
            starts.push(out.len());
            let expanded = everything || self.expanded[index];
            let selected = !everything && self.selected == Some(index);
            if selected || (!everything && expanded) {
                if selected {
                    selected_row = Some(out.len());
                }
                let kind = if entry.speaker == Speaker::Reasoning {
                    "thought"
                } else {
                    "tool"
                };
                let state = if expanded { "expanded" } else { "collapsed" };
                let header = format!(
                    "{}{kind} {}: {state}",
                    if selected { "> " } else { "  " },
                    index.saturating_add(1)
                );
                out.extend(wrap(&header, width));
            }
            let mut body = String::new();
            let full = Display {
                tool_lines: usize::MAX,
                ..display
            };
            // A collapsed body depends on the entry and the display alone.
            if !expanded && let Some(cache) = cache.as_mut() {
                let rows = cache.body(index, entry, full, || {
                    let mut body = String::new();
                    collapsed_body(entry, full, &mut body);
                    body.trim_end().lines().map(str::to_owned).collect()
                });
                out.extend(rows);
                continue;
            }
            match (entry.speaker, expanded, self.expansions[index].as_ref()) {
                (_, false, _) => collapsed_body(entry, full, &mut body),
                (Speaker::Tool, true, Some(Expansion::Text(text))) => {
                    render_entry(&Entry::tool(text), full, &Lanes::default(), &mut body);
                }
                (Speaker::Tool, true, Some(Expansion::Rows(rows))) => {
                    out.extend(rows.iter().cloned());
                    continue;
                }
                _ => render_entry(entry, full, &Lanes::default(), &mut body),
            }
            push(&mut out, &body);
        }
        while out.last().is_some_and(String::is_empty) {
            out.pop();
        }
        DetailRows {
            rows: out,
            selected: selected_row,
            starts,
        }
    }
}

/// Renders a collapsed viewer entry: one `thought:` row for reasoning, the
/// recorded text for everything else.
fn collapsed_body(entry: &Entry, display: Display, body: &mut String) {
    if entry.speaker == Speaker::Reasoning {
        let first = first_line(&sanitize(&entry.text));
        render_entry(
            &Entry::reasoning(format!("thought: {first}")),
            display,
            &Lanes::default(),
            body,
        );
    } else {
        render_entry(entry, display, &Lanes::default(), body);
    }
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
    /// How Markdown in the answer is drawn. Off by default.
    pub markdown: crate::markdown::MarkdownStyle,
    /// Opens the marker beside a user message.
    pub user_rail: String,
    /// Opens a secondary row: an informational notice or an error's hint.
    pub dim: String,
    /// Opens the `warning:` label.
    pub warning: String,
    /// Opens the `error:` label.
    pub error: String,
}

impl Default for Lanes {
    fn default() -> Self {
        Self {
            reasoning: "\u{1b}[2m".to_owned(),
            reset: "\u{1b}[0m".to_owned(),
            markdown: crate::markdown::MarkdownStyle::default(),
            user_rail: String::new(),
            dim: String::new(),
            warning: String::new(),
            error: String::new(),
        }
    }
}

/// Wraps `text` in `open` and `reset`, or leaves it bare when there is no style.
fn painted(open: &str, text: &str, reset: &str) -> String {
    if open.is_empty() {
        text.to_owned()
    } else {
        format!("{open}{text}{reset}")
    }
}

/// Writes one row without trailing spaces: a row is never padded, so what is
/// copied from scrollback ends where the text does.
fn push_row(out: &mut String, row: &str) {
    out.push_str(row.trim_end_matches(' '));
    out.push('\n');
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
        // A finished answer is whole, so its tables can be measured and aligned.
        let aligned;
        let source = if lanes.markdown.enabled {
            aligned = crate::markdown::align_tables(&text, &lanes.markdown, display.ascii, width);
            &aligned
        } else {
            &text
        };
        AssistantRows::with_markdown(lanes.markdown.clone()).rows_with_ascii(
            source,
            width,
            display.ascii,
        )
    } else {
        wrap(&text, width)
    };

    let reset = lanes.reset.as_str();
    match entry.speaker {
        Speaker::User => {
            let marker = if display.ascii { ">" } else { "\u{258c}" };
            let rail = painted(&lanes.user_rail, marker, reset);
            let first = body.first().map_or("", String::as_str);
            push_row(out, &format!("{rail} {first}"));
            for line in body.iter().skip(1) {
                push_row(out, &format!("  {line}"));
            }
        }
        Speaker::Assistant => {
            for line in &body {
                push_row(out, line);
            }
        }
        // Reasoning is drawn in a secondary colour and indented, so it is
        // visually a separate lane from the answer rather than mixed into it.
        Speaker::Reasoning => {
            for line in &body {
                push_row(out, &format!("{}  {line}{}", lanes.reasoning, lanes.reset));
            }
        }
        Speaker::Tool => {
            let keep = display.tool_lines.min(body.len());
            for line in body.iter().take(keep) {
                push_row(out, &format!("  {line}"));
            }
            if keep < body.len() {
                push_row(
                    out,
                    &format!("  ... {} more line(s)", body.len().saturating_sub(keep)),
                );
            }
        }
        Speaker::Notice => match entry.severity {
            Severity::Info => {
                for line in &body {
                    push_row(out, &painted(&lanes.dim, &format!("[{line}]"), reset));
                }
            }
            Severity::Warning => labelled(out, "warning:", &lanes.warning, &text, display, reset),
            Severity::Error => {
                labelled(out, "error:", &lanes.error, &text, display, reset);
                let hint = entry.hint.as_deref().map(sanitize).unwrap_or_default();
                let width = display.width.saturating_sub(2).max(MIN_WIDTH);
                for line in wrap(&hint, width) {
                    if !line.trim().is_empty() {
                        push_row(out, &painted(&lanes.dim, &format!("  {line}"), reset));
                    }
                }
            }
        },
    }
}

/// Writes a notice under a word naming its severity: the label on the first
/// row, the rest indented under the text.
fn labelled(out: &mut String, label: &str, open: &str, text: &str, display: Display, reset: &str) {
    let indent = str_width(label).saturating_add(1);
    let width = display.width.saturating_sub(indent).max(MIN_WIDTH);
    let label = painted(open, label, reset);
    for (index, line) in wrap(text, width).iter().enumerate() {
        if index == 0 {
            push_row(out, &format!("{label} {line}"));
        } else {
            push_row(out, &format!("{:indent$}{line}", ""));
        }
    }
}

/// Minimum nonzero text width used by transcript wrapping.
pub const MIN_WIDTH: usize = 1;

/// Wrapped assistant rows, cached across appended, sanitized text deltas.
///
/// Complete source lines keep their rows and fence state. Only the unfinished
/// line is rewrapped, so completing a fence or appending code cannot move an
/// earlier line. A width change rewraps from the committed base; a cleared
/// source resets the cache.
#[derive(Clone, Debug, Default)]
pub struct AssistantRows {
    /// Rows of each complete line after `base`, with the line's end offset and
    /// the fence state after it.
    finished: Vec<(usize, Vec<String>, Option<Fence>)>,
    covered: usize,
    /// Bytes of the source already handed out by `take_through`.
    base: usize,
    /// The fence state at `base`.
    base_fence: Option<Fence>,
    width: Option<usize>,
    ascii: bool,
    fence: Option<Fence>,
    markdown: crate::markdown::MarkdownStyle,
}

impl AssistantRows {
    /// Draws Markdown prose with `style` from now on, rewrapping if it changed.
    pub fn set_markdown(&mut self, markdown: &crate::markdown::MarkdownStyle) {
        if &self.markdown != markdown {
            self.markdown = markdown.clone();
            self.rewrap();
        }
    }

    /// Returns rows that draw Markdown prose with `style`.
    #[must_use]
    pub fn with_markdown(markdown: crate::markdown::MarkdownStyle) -> Self {
        Self {
            markdown,
            ..Self::default()
        }
    }

    /// Returns all rows for text that grows by appending deltas.
    ///
    /// Prose uses ordinary word wrapping. Inside backtick or tilde fences,
    /// whitespace is preserved, tabs expand to eight-column stops, and each
    /// source line is one row, however wide, so it copies as written.
    #[must_use]
    pub fn rows(&mut self, text: &str, width: usize) -> Vec<String> {
        self.rows_with_ascii(text, width, false)
    }

    /// Returns wrapped rows using ASCII continuation markers when requested.
    ///
    /// Only the text after the committed base is drawn.
    #[must_use]
    pub fn rows_with_ascii(&mut self, text: &str, width: usize, ascii: bool) -> Vec<String> {
        let width = width.max(MIN_WIDTH);
        if text.len() < self.covered {
            *self = Self::with_markdown(std::mem::take(&mut self.markdown));
        }
        if self.width != Some(width) || self.ascii != ascii {
            self.width = Some(width);
            self.ascii = ascii;
            self.rewrap();
        }
        let sealed = self.cover(text);
        let mut rows: Vec<String> = self
            .finished
            .iter()
            .flat_map(|(_, rows, _)| rows.iter().cloned())
            .collect();
        // A partial closing fence must not change the cached state until its
        // newline arrives: the next delta may still make it ordinary code.
        let mut fence = self.fence;
        rows.extend(assistant_line(
            &text[sealed..],
            width,
            &mut fence,
            ascii,
            &self.markdown,
            false,
        ));
        rows
    }

    /// Hands out the rows for `text[base..offset]` and moves the base to
    /// `offset`, so they are never drawn again.
    ///
    /// `offset` is a line start, or a point inside the unfinished line, whose
    /// part before `offset` is then drawn as a complete line.
    #[must_use]
    pub fn take_through(&mut self, text: &str, offset: usize) -> Vec<String> {
        let width = self.width.unwrap_or(MIN_WIDTH);
        let offset = offset.min(text.len());
        if offset <= self.base {
            return Vec::new();
        }
        self.cover(&text[..offset]);
        let keep = self
            .finished
            .iter()
            .take_while(|(end, _, _)| *end <= offset)
            .count();
        let mut rows = Vec::new();
        for (_, line, fence) in self.finished.drain(..keep) {
            rows.extend(line);
            self.base_fence = fence;
        }
        if offset > self.covered {
            let line = &text[self.covered..offset];
            rows.extend(assistant_line(
                line,
                width,
                &mut self.fence,
                self.ascii,
                &self.markdown,
                true,
            ));
            self.base_fence = self.fence;
            self.covered = offset;
        }
        self.base = offset;
        rows
    }

    /// Whether the committed base is inside a code fence.
    #[must_use]
    pub const fn base_in_code(&self) -> bool {
        self.base_fence.is_some()
    }

    /// Wraps the complete lines after `covered`, returning the new `covered`.
    fn cover(&mut self, text: &str) -> usize {
        let width = self.width.unwrap_or(MIN_WIDTH);
        let start = self.covered.min(text.len());
        let sealed = text[start..]
            .rfind('\n')
            .map_or(start, |at| start.saturating_add(at).saturating_add(1));
        let mut end = start;
        for line in text[start..sealed].split_inclusive('\n') {
            end = end.saturating_add(line.len());
            let rows = assistant_line(
                line.strip_suffix('\n').unwrap_or(line),
                width,
                &mut self.fence,
                self.ascii,
                &self.markdown,
                true,
            );
            self.finished.push((end, rows, self.fence));
        }
        self.covered = sealed.max(self.covered);
        self.covered
    }

    /// Drops the cached rows so they are wrapped again from the base.
    fn rewrap(&mut self) {
        self.finished.clear();
        self.covered = self.base;
        self.fence = self.base_fence;
    }
}

#[derive(Clone, Copy, Debug)]
struct Fence {
    marker: u8,
    length: usize,
    language: crate::highlight::Language,
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
    let info = &rest[length..];
    (length >= 3).then(|| {
        (
            Fence {
                marker,
                length,
                language: crate::highlight::Language::from_info(info),
            },
            info,
        )
    })
}

fn assistant_line(
    line: &str,
    width: usize,
    fence: &mut Option<Fence>,
    ascii: bool,
    markdown: &crate::markdown::MarkdownStyle,
    complete: bool,
) -> Vec<String> {
    // A fence line is syntax rather than content, so styled output leaves it
    // out; copying a code block from scrollback then gives only the code.
    let candidate = fence_start(line);
    if let Some(open) = *fence {
        if candidate.is_some_and(|(close, rest)| {
            close.marker == open.marker && close.length >= open.length && rest.trim().is_empty()
        }) {
            *fence = None;
            return if markdown.enabled {
                Vec::new()
            } else {
                wrap(line, width)
            };
        }
        return vec![code_row(line, open.language, markdown)];
    }
    if let Some((open, rest)) = candidate
        && (open.marker != b'`' || !rest.contains('`'))
    {
        *fence = Some(open);
        if !markdown.enabled {
            return wrap(line, width);
        }
        let indent = line
            .len()
            .saturating_sub(line.trim_start_matches(' ').len());
        return rest
            .split_whitespace()
            .next()
            .map(|language| {
                vec![format!(
                    "{:indent$}{}{language}{}",
                    "", markdown.dim, markdown.reset
                )]
            })
            .unwrap_or_default();
    }
    let styled = if complete {
        crate::markdown::style_line(line, markdown, ascii, width)
    } else {
        crate::markdown::style_open_line(line, markdown, ascii, width)
    };
    let body_width = if styled.indent == 0 {
        width
    } else {
        width.saturating_sub(styled.indent).max(8)
    };
    wrap(&styled.body, body_width)
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let prefix = if index == 0 {
                &styled.first
            } else {
                &styled.continuation
            };
            format!("{prefix}{row}")
        })
        .collect()
}

/// The kind of Markdown block a streaming answer is in.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Block {
    /// Between blocks: everything before is sealed.
    #[default]
    None,
    Paragraph,
    List,
    Quote,
    Table,
    /// Inside a fence.
    Code,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LineKind {
    #[default]
    Other,
    Blank,
    Row,
    Delimiter,
}

/// Finds where the finished blocks of a streaming answer end.
///
/// A block is finished once a later line shows it cannot change: its blank
/// line, the next heading, a fence of its own, the end of a table. Only
/// complete lines are examined, so text after the last newline is never
/// sealed.
#[derive(Clone, Debug, Default)]
pub struct Sealer {
    scanned: usize,
    sealed: usize,
    block: Block,
    fence: Option<Fence>,
    table_rows: usize,
    previous: LineKind,
}

impl Sealer {
    /// Examines the complete lines of `text` not seen yet and returns the
    /// sealed byte offset: always a line start, never decreasing.
    pub fn advance(&mut self, text: &str) -> usize {
        if text.len() < self.scanned {
            self.reset();
        }
        while let Some(at) = text[self.scanned..].find('\n') {
            let start = self.scanned;
            let end = start.saturating_add(at).saturating_add(1);
            self.line(&text[start..end.saturating_sub(1)], start, end);
            self.scanned = end;
        }
        self.sealed
    }

    /// The block the unsealed text is in.
    #[must_use]
    pub const fn block(&self) -> Block {
        self.block
    }

    /// Rows of the table being written, counting its header and delimiter.
    #[must_use]
    pub const fn open_table_rows(&self) -> usize {
        if matches!(self.block, Block::Table) {
            self.table_rows
        } else {
            0
        }
    }

    /// The start of the line not yet complete.
    #[must_use]
    pub const fn line_start(&self) -> usize {
        self.scanned
    }

    /// Forgets everything, for an answer that was cleared.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn seal(&mut self, at: usize) {
        self.sealed = self.sealed.max(at);
    }

    fn line(&mut self, line: &str, start: usize, end: usize) {
        let candidate = fence_start(line);
        if let Some(open) = self.fence {
            if candidate.is_some_and(|(close, rest)| {
                close.marker == open.marker && close.length >= open.length && rest.trim().is_empty()
            }) {
                self.fence = None;
                self.block = Block::None;
                self.seal(end);
            }
            self.previous = LineKind::Other;
            return;
        }
        let row = crate::markdown::is_row(line);
        let kind = if crate::markdown::is_delimiter(line) {
            LineKind::Delimiter
        } else if row {
            LineKind::Row
        } else if line.trim_matches(' ').is_empty() {
            LineKind::Blank
        } else {
            LineKind::Other
        };
        let previous = std::mem::replace(&mut self.previous, kind);
        if self.block == Block::Table {
            if row {
                self.table_rows = self.table_rows.saturating_add(1);
                return;
            }
            self.seal(start);
            self.block = Block::None;
            self.table_rows = 0;
        }
        if let Some((open, rest)) = candidate
            && (open.marker != b'`' || !rest.contains('`'))
        {
            self.seal(start);
            self.fence = Some(open);
            self.block = Block::Code;
            return;
        }
        if kind == LineKind::Blank {
            self.seal(end);
            self.block = Block::None;
            return;
        }
        let indent = line
            .len()
            .saturating_sub(line.trim_start_matches(' ').len());
        let rest = &line[indent..];
        if indent <= 3
            && (crate::markdown::heading(rest).is_some() || crate::markdown::is_rule(rest))
        {
            self.seal(end);
            self.block = Block::None;
            return;
        }
        if row && matches!(previous, LineKind::Row | LineKind::Delimiter) {
            self.block = Block::Table;
            self.table_rows = 2;
            return;
        }
        let item = crate::markdown::bullet(rest).is_some() || ordered_item(rest);
        if self.block == Block::List && !item && indent >= 2 {
            return;
        }
        let quote = indent <= 3 && rest.starts_with('>');
        let next = if item {
            Block::List
        } else if quote {
            Block::Quote
        } else {
            Block::Paragraph
        };
        if next != self.block {
            self.seal(start);
            self.block = next;
        }
    }
}

/// Returns true for an ordered list item: digits, `.` or `)`, then a space.
fn ordered_item(text: &str) -> bool {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    (1..=9).contains(&digits)
        && matches!(text.as_bytes().get(digits), Some(b'.' | b')'))
        && text.as_bytes().get(digits.saturating_add(1)) == Some(&b' ')
}

/// Erases from the cursor to the end of the row in the current background.
const ERASE_TO_END: &str = "\u{1b}[K";

/// Draws one line of fenced code whole: tabs expanded, highlighted, on the
/// code background when there is one. It is never wrapped, so the terminal
/// soft-wraps a long row and copying it gives the line as written.
fn code_row(
    source: &str,
    language: crate::highlight::Language,
    markdown: &crate::markdown::MarkdownStyle,
) -> String {
    let row = crate::highlight::highlight(&expand_tabs(source), language, &markdown.code_block);
    let bg = markdown.code_bg.as_str();
    if bg.is_empty() {
        return row;
    }
    // Each token's reset would end the background, so it is reopened.
    let close = markdown.code_block.reset.as_str();
    let row = if close.is_empty() {
        row
    } else {
        row.replace(close, &format!("{close}{bg}"))
    };
    format!("{bg}{row}{ERASE_TO_END}{}", crate::engine::Style::RESET)
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
    render_prompt_with_ascii(prompt, input, width, false)
}

/// Renders a prompt echo with ASCII newline decoration when requested.
#[must_use]
pub fn render_prompt_with_ascii(prompt: &str, input: &str, width: usize, ascii: bool) -> String {
    let room = width.saturating_sub(str_width(prompt));
    let input = crate::editor::displayed_with_ascii(input, ascii);
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

/// Renders a draft on the rows it wraps to, with an editable caret.
///
/// `column` counts the displayed prefix, including one cell for each newline
/// and tab, as returned by [`crate::editor::Composer::cursor_column`]. Rows
/// come from [`crate::editor::layout`]: the prompt marker leads the first row
/// and later rows are indented to the text after it. A draft that fits on one
/// row keeps the single-row viewport.
#[must_use]
pub fn render_draft_at(
    prompt: &str,
    input: &str,
    column: usize,
    width: usize,
) -> (Vec<String>, (u16, u16)) {
    let (marker, indent) = truncate_to_width(prompt, width.saturating_sub(1));
    let rows = crate::editor::layout(input, indent, indent, width);
    if rows.len() <= 1 {
        let (row, caret) = render_prompt_at(prompt, input, column, width);
        return (vec![row], (0, u16::try_from(caret).unwrap_or(u16::MAX)));
    }
    let caret = caret_index(input, column);
    let (caret_row, caret_column) = crate::editor::caret_in(input, &rows, caret);
    let continuation = " ".repeat(indent);
    let chars: Vec<char> = input.chars().collect();
    let drawn = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let text: String = chars
                .get(row.start..row.end)
                .unwrap_or_default()
                .iter()
                .collect();
            let lead = if index == 0 { marker } else { &continuation };
            // A cluster wider than the whole row is cut rather than overflowing it.
            let shown = crate::editor::displayed(&text);
            let (shown, _) = truncate_to_width(&shown, width.saturating_sub(indent));
            format!("{lead}{shown}")
        })
        .collect();
    let caret_column = indent
        .saturating_add(caret_column)
        .min(width.saturating_sub(1));
    (
        drawn,
        (
            u16::try_from(caret_row).unwrap_or(u16::MAX),
            u16::try_from(caret_column).unwrap_or(u16::MAX),
        ),
    )
}

/// Returns the character index whose displayed prefix is `column` wide.
fn caret_index(input: &str, column: usize) -> usize {
    let mut used = 0usize;
    let mut at = 0usize;
    for cluster in graphemes(input) {
        if used >= column {
            break;
        }
        used = used.saturating_add(str_width(&crate::editor::displayed(cluster)));
        at = at.saturating_add(cluster.chars().count());
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed(text: &str) -> usize {
        Sealer::default().advance(text)
    }

    #[test]
    fn a_paragraph_seals_at_its_blank_line() {
        assert_eq!(sealed("one\ntwo\n"), 0);
        assert_eq!(sealed("one\ntwo\n\nthree\n"), "one\ntwo\n\n".len());
    }

    #[test]
    fn a_fence_seals_at_its_closing_line_and_not_before() {
        let open = "intro\n```rust\nfn main() {}\n\n";
        let mut sealer = Sealer::default();
        assert_eq!(sealer.advance(open), "intro\n".len());
        assert_eq!(sealer.block(), Block::Code);
        let closed = format!("{open}```\n");
        assert_eq!(sealer.advance(&closed), closed.len());
        assert_eq!(sealer.block(), Block::None);
    }

    #[test]
    fn a_heading_seals_at_its_line_end() {
        assert_eq!(sealed("# Title\nbody"), "# Title\n".len());
        assert_eq!(sealed("text\n## Next\n"), "text\n## Next\n".len());
    }

    #[test]
    fn a_table_seals_at_the_first_line_that_is_not_a_row() {
        let table = "| a | b |\n| - | - |\n| 1 | 2 |\n";
        let mut sealer = Sealer::default();
        assert_eq!(sealer.advance(table), 0);
        assert_eq!(sealer.block(), Block::Table);
        assert_eq!(sealer.open_table_rows(), 3);
        let after = format!("{table}after\n");
        assert_eq!(sealer.advance(&after), table.len());
        assert_eq!(sealer.block(), Block::Paragraph);
    }

    #[test]
    fn a_list_seals_at_a_blank_or_unindented_line() {
        assert_eq!(sealed("- one\n  more\n- two\n"), 0);
        assert_eq!(sealed("- one\n- two\n\n"), "- one\n- two\n\n".len());
        assert_eq!(sealed("- one\n1. two\nafter\n"), "- one\n1. two\n".len());
        assert_eq!(sealed("text\n- item\n"), "text\n".len());
    }

    #[test]
    fn an_unfinished_line_is_never_sealed() {
        assert_eq!(
            sealed("one\n\n# heading without its newline"),
            "one\n\n".len()
        );
        assert_eq!(sealed("unfinished"), 0);
    }

    #[test]
    fn sealing_one_delta_at_a_time_matches_sealing_the_whole_text() {
        let answer = "# Report\n\nIntro line\nsecond line\n\n- a\n  more\n- b\n> quoted\n\
            plain\n| a | b |\n| - | - |\n| 1 | 2 |\ntail\n```\ncode\n\n```\n---\nend";
        let mut whole = Sealer::default();
        let expected = whole.advance(answer);
        let mut stepped = Sealer::default();
        let mut last = 0;
        for end in 1..=answer.len() {
            if answer.is_char_boundary(end) {
                let offset = stepped.advance(&answer[..end]);
                assert!(offset >= last, "the sealed offset went back at {end}");
                assert!(answer[..offset].is_empty() || answer[..offset].ends_with('\n'));
                last = offset;
            }
        }
        assert_eq!(last, expected);
        assert_eq!(expected, answer.len() - "end".len());
    }

    #[test]
    fn a_long_draft_wraps_at_forty_columns_with_the_caret_on_the_right_row() {
        let draft = "x".repeat(100);
        let (rows, caret) = render_draft_at("> ", &draft, 100, 40);
        assert_eq!(
            rows,
            vec![
                format!("> {}", "x".repeat(38)),
                format!("  {}", "x".repeat(38)),
                format!("  {}", "x".repeat(24)),
            ]
        );
        assert_eq!(caret, (2, 26));
        assert_eq!(render_draft_at("> ", &draft, 50, 40).1, (1, 14));
        assert_eq!(render_draft_at("> ", &draft, 38, 40).1, (1, 2));
        assert_eq!(render_draft_at("> ", &draft, 0, 40).1, (0, 2));
        assert!(rows.iter().all(|row| str_width(row) <= 40));
    }

    fn renders(cache: &SharedBodies) -> usize {
        cache
            .lock()
            .map(|cache| cache.renders())
            .unwrap_or_default()
    }

    #[test]
    fn the_viewer_renders_only_new_entries() {
        let cache = SharedBodies::default();
        let mut entries = vec![
            Entry::user("one"),
            Entry::assistant("A1"),
            Entry::tool("read a.txt"),
        ];
        let display = Display::default();
        let first = ToolDetails::new(entries.clone())
            .with_cache(std::sync::Arc::clone(&cache))
            .rows(display);
        assert_eq!(renders(&cache), 3);
        entries.push(Entry::assistant("A2"));
        let second = ToolDetails::new(entries.clone())
            .with_cache(std::sync::Arc::clone(&cache))
            .rows(display);
        assert_eq!(renders(&cache), 4, "only the new entry is rendered");
        assert_eq!(second, ToolDetails::new(entries.clone()).rows(display));
        assert!(second.rows.starts_with(&first.rows));

        // A changed entry renders again, and so does everything after it.
        entries[1] = Entry::assistant("A1 changed");
        let changed = ToolDetails::new(entries.clone())
            .with_cache(std::sync::Arc::clone(&cache))
            .rows(display);
        assert_eq!(renders(&cache), 7);
        assert_eq!(changed, ToolDetails::new(entries).rows(display));
    }

    #[test]
    fn a_width_change_invalidates_the_viewer_cache() {
        let cache = SharedBodies::default();
        let entries = vec![Entry::assistant("word ".repeat(30)), Entry::user("two")];
        let narrow = Display {
            width: 40,
            ..Display::default()
        };
        let wide = Display {
            width: 100,
            ..Display::default()
        };
        let details = ToolDetails::new(entries.clone()).with_cache(std::sync::Arc::clone(&cache));
        let _ = details.rows(narrow);
        assert_eq!(renders(&cache), 2);
        let rows = details.rows(wide);
        assert_eq!(renders(&cache), 4);
        assert_eq!(rows, ToolDetails::new(entries.clone()).rows(wide));
        let ascii = Display {
            ascii: true,
            ..wide
        };
        let _ = details.rows(ascii);
        assert_eq!(renders(&cache), 6, "an ASCII change renders again too");
        let _ = details.rows(ascii);
        assert_eq!(renders(&cache), 6);
    }

    #[test]
    fn tool_disclosure_keeps_surrounding_entries_and_source_text() {
        let content = (0..300)
            .map(|row| format!("TOOL-{row:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        let entries = vec![
            Entry::user("before"),
            Entry::tool(content),
            Entry::assistant("after"),
        ];
        let mut details = ToolDetails::new(entries.clone());
        let display = Display::default();
        let untouched = details.rows(display);
        assert_eq!(
            untouched.rows.join("\n"),
            render(
                &entries,
                Display {
                    tool_lines: usize::MAX,
                    ..display
                }
            )
        );
        assert_eq!(untouched.starts, [0, 2, 303]);
        details.toggle();
        assert_eq!(details.rows(display).selected, None);
        details.select(false);
        details.toggle();
        let expanded = details.rows(display);
        assert!(expanded.rows[expanded.selected.expect("selection")].contains("expanded"));
        assert!(expanded.rows.join("\n").contains("TOOL-299"));
        details.set_expansion(1, Expansion::Text("STORED-BODY".to_owned()));
        let stored = details.rows(display).rows.join("\n");
        assert!(stored.contains("STORED-BODY") && !stored.contains("TOOL-000"));
        assert!(stored.starts_with("\u{258c} before"));
        assert!(stored.ends_with("after"));
        details.toggle();
        assert!(details.rows(display).rows.join("\n").contains("TOOL-000"));
        details.select(true);
        assert_eq!(details.selected(), Some(1), "one tool wraps to itself");
        assert_eq!(details.entries(), entries.as_slice(), "source stays intact");

        details.toggle();
        let mut grown = entries.clone();
        grown.push(Entry::assistant("later"));
        details.replace(grown);
        assert_eq!(details.selected(), Some(1));
        assert!(
            details
                .rows(display)
                .rows
                .join("\n")
                .contains("STORED-BODY")
        );
        let mut changed = entries;
        changed[1] = Entry::tool("other");
        details.replace(changed);
        assert!(details.pending().is_empty());
        assert_eq!(details.selected(), None);
    }

    #[test]
    fn expanded_tool_details_are_not_capped() {
        let text = (0..1100)
            .map(|row| format!("{row:04} {}", "x".repeat(34)))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.len() >= 40 * 1024);
        let mut details = ToolDetails::new(vec![Entry::tool(text), Entry::assistant("after")]);
        details.select(false);
        details.toggle();
        assert_eq!(details.pending(), [0]);
        for width in [80, 12] {
            let rows = details
                .rows(Display {
                    width,
                    ascii: true,
                    ..Display::default()
                })
                .rows;
            let joined = rows.join("\n");
            assert!(joined.contains("expanded"));
            assert!(joined.contains("1099"), "the last line is shown");
            assert!(!joined.contains("more line(s)"));
            assert!(!joined.contains("byte limit"));
            assert!(joined.ends_with("after"));
            assert!(rows.iter().all(|row| str_width(row) <= width));
        }
    }

    #[test]
    fn reasoning_entries_collapse_by_default() {
        let mut details = ToolDetails::new(vec![
            Entry::reasoning("first idea\nsecond idea"),
            Entry::assistant("answer"),
        ]);
        let plain = |rows: &[String]| sanitize(&rows.join("\n"));
        let display = Display::default();
        let collapsed = plain(&details.rows(display).rows);
        assert!(collapsed.contains("thought: first idea"), "{collapsed}");
        assert!(!collapsed.contains("second idea"));
        details.select(false);
        assert_eq!(details.selected(), Some(0));
        details.toggle();
        let expanded = plain(&details.rows(display).rows);
        assert!(expanded.contains("second idea"), "{expanded}");
        assert!(details.pending().is_empty(), "reasoning needs no expansion");
        details.toggle();
        details.toggle_all();
        assert!(plain(&details.rows(display).rows).contains("second idea"));
        details.toggle_all();
        assert!(!plain(&details.rows(display).rows).contains("second idea"));
        let everything = plain(&details.expanded_rows(display).rows);
        assert!(everything.contains("second idea") && !everything.contains("thought 1"));
    }

    #[test]
    fn tool_selection_skips_other_lanes_and_nonexpandable_tools() {
        let mut fixed = Entry::tool("fixed");
        fixed.expandable = false;
        let mut details = ToolDetails::new(vec![
            Entry::tool("first"),
            Entry::assistant("answer"),
            fixed,
            Entry::tool("last"),
        ]);
        details.select(false);
        assert_eq!(details.selected, Some(0));
        details.select(false);
        assert_eq!(details.selected, Some(3));
        details.select(false);
        assert_eq!(details.selected, Some(0));
        details.select(true);
        assert_eq!(details.selected, Some(3));
        let mut empty = ToolDetails::new(vec![Entry::assistant("answer")]);
        empty.select(false);
        empty.toggle();
        let rows = empty.rows(Display::default());
        assert_eq!(
            (rows.rows, rows.selected),
            (vec![String::from("answer")], None)
        );
    }

    #[test]
    fn accessible_output_labels_every_line_and_preserves_content_without_controls() {
        let long = "W".repeat(300);
        let entries = [
            Entry::user(format!("{long}\nsecond line")),
            Entry::reasoning("thinking"),
            Entry::assistant("```rust\n  code\n```\x1b[2J\r\x08\x07"),
            Entry::tool("first result\nsecond result"),
            Entry::tool("another tool"),
            Entry::notice("finished"),
        ];
        let bytes = append_lines(&accessible_lines(&entries));
        let output = String::from_utf8(bytes).expect("UTF-8");
        assert_eq!(
            output,
            format!(
                "User: {long}\nUser: second line\nReasoning: thinking\nAssistant: ```rust\nAssistant:   code\nAssistant: ```\nTool: first result\nTool: second result\nTool: another tool\nNotice: finished\n"
            )
        );
        assert!(output.chars().all(|ch| !ch.is_control() || ch == '\n'));
        assert!(append_lines(&[]).is_empty());
    }

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
    fn multiline_drafts_wrap_and_bound_narrow_carets() {
        let input = "first\nab書👋🏽e\u{301}Z\nlast";
        for width in 0..=12 {
            for column in 0..=str_width(&crate::editor::displayed(input)) {
                let (rows, caret) = render_draft_at("> ", input, column, width);
                assert!(rows.len() >= 3);
                assert!(rows.iter().all(|row| str_width(row) <= width));
                assert!(usize::from(caret.1) < width.max(1));
                assert!(usize::from(caret.0) < rows.len());
            }
        }
        let (rows, caret) = render_draft_at("> ", input, 14, 8);
        assert_eq!(rows, ["> first", "  ab書👋🏽", "  e\u{301}Z", "  last"]);
        assert_eq!(caret, (2, 4));
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
    fn ascii_decorations_cover_wrapping_and_tool_summaries() {
        assert_eq!(
            render_prompt_with_ascii("> ", "first\nsecond", 80, true),
            "> first/second"
        );
        assert_eq!(
            render_prompt_with_ascii("> ", "界⏎\nsecond", 80, true),
            "> 界⏎/second"
        );
        let entries = [
            Entry::assistant("```rust\n    abcdefghijkl  mnop\n```"),
            Entry::tool("read_file\none\ntwo\nthree"),
            Entry::tool("glob_files failed\none\ntwo"),
        ];
        for width in 1..=24 {
            let display = Display {
                width,
                ascii: true,
                tool_lines: 1,
                ..Display::default()
            };
            let rendered = render(&entries, display);
            assert!(rendered.is_ascii(), "{rendered}");
            assert!(rendered.contains("more line(s)"));
            assert!(render_grouped(&entries, display).is_ascii());
            let rows = AssistantRows::default().rows_with_ascii(&entries[0].text, width, true);
            assert!(rows.iter().all(|row| row.is_ascii()));
            assert!(rows.iter().any(|row| row == "    abcdefghijkl  mnop"));
        }
        let text = "```\n    界↪éabcdefghij\n```";
        let rows = AssistantRows::default().rows_with_ascii(text, 10, true);
        assert_eq!(rows[1], "    界↪éabcdefghij", "source Unicode must survive");
    }

    #[test]
    fn switching_ascii_markers_invalidates_finished_streaming_rows() {
        let text = "- item\n  - nested\n";
        let style = crate::markdown::MarkdownStyle::attributes();
        let mut cache = AssistantRows::with_markdown(style.clone());
        assert!(
            cache
                .rows(text, 10)
                .iter()
                .any(|row| row.contains('\u{2022}'))
        );
        let ascii = cache.rows_with_ascii(text, 10, true);
        assert!(ascii.iter().all(|row| row.is_ascii()), "{ascii:?}");
        assert_eq!(
            cache.rows(text, 10),
            AssistantRows::with_markdown(style).rows(text, 10)
        );
    }

    #[test]
    fn code_rows_are_never_wrapped() {
        let text = "before\n```rust\n    abcdefghijkl  mnop\n\n```\nafter";
        for width in [4, 12, 80] {
            let rendered = render(
                &[Entry::assistant(text)],
                Display {
                    width,
                    ..Display::default()
                },
            );
            assert!(
                rendered.contains("\n    abcdefghijkl  mnop\n\n"),
                "{rendered:?}"
            );
            assert!(!rendered.contains('\u{21aa}'), "{rendered:?}");
        }
        let styled = AssistantRows::with_markdown(crate::markdown::MarkdownStyle::attributes())
            .rows("```\n\tlet x = 1;\n```", 6);
        assert_eq!(styled, ["        let x = 1;"]);
    }

    #[test]
    fn fence_lines_are_hidden_and_the_language_is_a_dim_row() {
        let style = crate::markdown::MarkdownStyle::attributes();
        let rows = AssistantRows::with_markdown(style.clone())
            .rows("intro\n```rust ignore\nlet x = 1;\n```\nafter", 40);
        assert_eq!(
            rows,
            ["intro", "\u{1b}[2mrust\u{1b}[0m", "let x = 1;", "after"]
        );
        // Without an info string the fence leaves no row at all.
        let rows = AssistantRows::with_markdown(style.clone()).rows("```\ncode\n```", 40);
        assert_eq!(rows, ["code"]);
        // An unclosed fence still draws its rows as code.
        let rows = AssistantRows::with_markdown(style.clone()).rows("~~~py\n  x  = 1\n", 40);
        assert_eq!(rows, ["\u{1b}[2mpy\u{1b}[0m", "  x  = 1", ""]);
        // A fence inside a list item keeps the item's indent.
        let rows = AssistantRows::with_markdown(style).rows("- item\n  ```sh\n  ls -l\n  ```", 40);
        assert_eq!(
            rows[1..],
            ["  \u{1b}[2msh\u{1b}[0m".to_owned(), "  ls -l".to_owned()]
        );
    }

    #[test]
    fn code_rows_carry_the_code_background_at_truecolor() {
        let mut style = crate::markdown::MarkdownStyle::attributes();
        style.code_block.keyword = "\u{1b}[38;2;1;2;3m".to_owned();
        style.code_block.reset = "\u{1b}[0m".to_owned();
        style.code_bg = "\u{1b}[48;2;28;28;28m".to_owned();
        let rows = AssistantRows::with_markdown(style.clone()).rows("```rust\nlet x = 1;\n```", 40);
        assert_eq!(
            rows[1],
            "\u{1b}[48;2;28;28;28m\u{1b}[38;2;1;2;3mlet\u{1b}[0m\u{1b}[48;2;28;28;28m x = 1;\u{1b}[K\u{1b}[0m"
        );
        let mut grid = crate::Grid::new(20, 2).unwrap();
        grid.feed(rows[1].as_bytes()).unwrap();
        assert_eq!(grid.row_text(0), "let x = 1;");
        // At sixteen colours there is no background, so rows stay plain.
        style.code_bg = String::new();
        let rows = AssistantRows::with_markdown(style).rows("```rust\nlet x = 1;\n```", 40);
        assert_eq!(rows[1], "\u{1b}[38;2;1;2;3mlet\u{1b}[0m x = 1;");
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
        assert!(
            previous
                .iter()
                .any(|row| row == "    call(\"alpha  beta\",  gamma);")
        );
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
            assert!(rows.iter().any(|row| row == "    abcdefghijkl"), "{rows:?}");
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
    fn code_tabs_and_graphemes_stay_whole_at_narrow_widths() {
        let rows = AssistantRows::default().rows("~~~\n\t書e\u{301}👩‍💻書\n~~~", 12);
        assert_eq!(rows, ["~~~", "        書e\u{301}👩‍💻書", "~~~"]);
        for width in 1..=12 {
            let rows = AssistantRows::default().rows("```\n                abcdef\n```", width);
            assert!(
                rows.iter().any(|row| row == "                abcdef"),
                "{rows:?}"
            );
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
            .map(|(index, row)| {
                let row = format!("{}{row}", if index == 0 { "\u{258c} " } else { "  " });
                row.trim_end_matches(' ').to_owned()
            })
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
        assert_eq!(rendered, "\u{258c} hello");
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
        assert!(lines[0].starts_with("\u{258c} "));
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
                ..Lanes::default()
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
        assert_eq!(rendered, "\u{258c} hi\n\nhello");
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
        assert_eq!(render(&[Entry::user("")], Display::default()), "\u{258c}");
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

    fn styled_lanes() -> Lanes {
        Lanes {
            reset: "<reset>".to_owned(),
            user_rail: "<rail>".to_owned(),
            dim: "<dim>".to_owned(),
            warning: "<warning>".to_owned(),
            error: "<error>".to_owned(),
            ..Lanes::default()
        }
    }

    #[test]
    fn a_user_message_wraps_whole_under_its_rail() {
        let first = "alpha ".repeat(30);
        let second = "omega ".repeat(21);
        let text = format!("{}\n{}", first.trim_end(), second.trim_end());
        assert!(text.chars().count() >= 300, "{}", text.len());
        let rendered = render_lanes(&[Entry::user(&text)], Display::default(), &styled_lanes());
        let rows: Vec<&str> = rendered.lines().collect();
        assert!((4..=5).contains(&rows.len()), "{rendered}");
        assert!(
            rows[0].starts_with("<rail>\u{258c}<reset> alpha"),
            "{rendered}"
        );
        for row in rows.iter().skip(1) {
            assert!(
                row.starts_with("  ") && !row.contains('\u{258c}'),
                "{rendered}"
            );
        }
        let words: Vec<&str> = rows
            .iter()
            .flat_map(|row| row.split_whitespace())
            .skip(1)
            .collect();
        assert_eq!(
            words.join(" "),
            text.split_whitespace().collect::<Vec<_>>().join(" ")
        );
        assert!(
            rows.iter()
                .all(|row| str_width(&row.replace("<rail>", "").replace("<reset>", "")) <= 80),
            "{rendered}"
        );
    }

    #[test]
    fn ascii_user_rail_is_a_marker() {
        let display = Display {
            ascii: true,
            width: 12,
            ..Display::default()
        };
        let rendered = render(&[Entry::user("one two three four")], display);
        assert_eq!(rendered, "> one two\n  three four");
        assert!(!rendered.contains('\u{258c}'), "{rendered}");
        let colourless = render(&[Entry::user("hi")], Display::default());
        assert_eq!(
            colourless, "\u{258c} hi",
            "the shape carries the rail without colour"
        );
    }

    #[test]
    fn warning_and_error_entries_are_labelled_words() {
        let display = Display {
            width: 24,
            ..Display::default()
        };
        let lanes = styled_lanes();
        let warning = render_lanes(
            &[Entry::warning("the server did not start in time")],
            display,
            &lanes,
        );
        let rows: Vec<&str> = warning.lines().collect();
        assert_eq!(rows[0], "<warning>warning:<reset> the server did");
        assert!(rows.len() > 1, "{warning}");
        for row in rows.iter().skip(1) {
            assert!(row.starts_with(&" ".repeat(9)), "{warning}");
            assert!(!row.starts_with(&" ".repeat(10)), "{warning}");
        }
        let error = render_lanes(
            &[Entry::error("the turn failed: no route", None)],
            display,
            &lanes,
        );
        let rows: Vec<&str> = error.lines().collect();
        assert_eq!(rows[0], "<error>error:<reset> the turn failed:");
        assert_eq!(rows[1], "       no route");
        let info = render_lanes(&[Entry::notice("cancelled")], display, &lanes);
        assert_eq!(info, "<dim>[cancelled]<reset>");
        let plain = render(&[Entry::error("stopped", None)], Display::default());
        assert_eq!(
            plain, "error: stopped",
            "the label is a word without colour"
        );
        assert_eq!(
            render(&[Entry::warning("slow")], Display::default()),
            "warning: slow"
        );
    }

    #[test]
    fn an_error_hint_is_its_own_dim_row() {
        let entry = Entry::error(
            "the turn failed: the endpoint returned HTTP 401",
            Some("run `rune connect` to store a new key".to_owned()),
        );
        let rendered = render_lanes(
            std::slice::from_ref(&entry),
            Display::default(),
            &styled_lanes(),
        );
        assert_eq!(
            rendered,
            "<error>error:<reset> the turn failed: the endpoint returned HTTP 401\n\
             <dim>  run `rune connect` to store a new key<reset>"
        );
        assert_eq!(
            accessible_lines(&[entry]),
            [
                "Notice: the turn failed: the endpoint returned HTTP 401",
                "Notice: run `rune connect` to store a new key",
            ]
        );
    }

    #[test]
    fn no_rendered_row_ends_with_a_space() {
        let entries = [
            Entry::user("first line \n\n   \nlast "),
            Entry::user(""),
            Entry::assistant("answer  \n```\ncode   \n```\n"),
            Entry::reasoning("thinking "),
            Entry::tool("out \n\n"),
            Entry::notice(" "),
            Entry::warning("warned \n"),
            Entry::error("failed ", Some("hint \n\nmore ".to_owned())),
        ];
        for ascii in [false, true] {
            let display = Display {
                ascii,
                width: 10,
                ..Display::default()
            };
            for lanes in [Lanes::default(), styled_lanes()] {
                let rendered = render_lanes(&entries, display, &lanes);
                for row in rendered.lines() {
                    assert!(!row.ends_with(' '), "{row:?} in {rendered:?}");
                }
            }
        }
    }
}
