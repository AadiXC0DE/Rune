//! The status footer: key hints and one line of session state.
//!
//! Sizing is a pure function of the terminal height, so the division between
//! the transcript, the prompt, the activity line, and the footer can be checked
//! without a terminal. Everything the footer renders is clipped to the width it
//! was handed, so a narrow terminal loses the end of a line rather than
//! wrapping it and pushing the prompt off the screen.
//!
//! Nothing here reads the environment or the terminal. The state it displays is
//! passed in, which keeps a render reproducible from its arguments.

use std::fmt::Write as _;
use std::time::Duration;

use rune_core::config::PermissionMode;

use crate::engine::{Style, flag};
use crate::theme::{Base, ColorDepth, Slot, Theme};
use crate::width::{str_width, truncate_to_width};

/// Rows the footer occupies when the terminal is too small for the layout.
pub const COMPACT_ROWS: u16 = 1;

/// Rows the status line and the hint line occupy.
pub const FOOTER_ROWS: u16 = 2;

/// Rows the prompt occupies when nothing else is known.
pub const DEFAULT_PROMPT_ROWS: u16 = 1;

/// The smallest terminal height the full layout is drawn in.
pub const DEFAULT_MINIMUM_ROWS: u16 = 6;

/// The share of the context window at which the reading is marked as a problem.
pub const CONTEXT_WARN_PERCENT: u8 = 80;

/// Space between two key hints.
const HINT_GAP: &str = "   ";

/// What the interface is doing, which decides the keys the hint row names.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum UiState {
    /// Waiting at the prompt.
    #[default]
    Idle,
    /// A turn is running.
    Running,
    /// A completion menu, path menu or picker is open.
    Menu,
    /// An approval or question picker is open.
    Approval,
}

/// Returns the key hints that work in an interface state.
#[must_use]
pub const fn hints(ui: UiState) -> &'static str {
    match ui {
        UiState::Idle => "/ commands",
        UiState::Running => "ctrl-c interrupt   enter steer",
        UiState::Menu => "up/down select   tab complete   esc close",
        UiState::Approval => "up/down select   enter confirm   esc cancel",
    }
}

/// One field of the status row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatusField {
    /// The model name.
    Model,
    /// The permission mode, or plan mode.
    Mode,
    /// Commands run without a sandbox.
    Unsandboxed,
    /// Context window usage.
    Context,
    /// Messages waiting for the running turn.
    Queued,
    /// Reasoning effort, when one is set.
    Effort,
    /// Commands may use the network.
    Net,
    /// The repository branch.
    Branch,
    /// Connected and configured MCP servers.
    Mcp,
    /// Estimated spend.
    Cost,
    /// Commands still running in the background.
    Tasks,
    /// The working directory.
    Workspace,
    /// The short session identifier.
    Session,
}

impl StatusField {
    /// Every field, highest priority first, which is also the display order.
    pub const ALL: [Self; 13] = [
        Self::Model,
        Self::Mode,
        Self::Unsandboxed,
        Self::Context,
        Self::Queued,
        Self::Effort,
        Self::Net,
        Self::Branch,
        Self::Mcp,
        Self::Cost,
        Self::Tasks,
        Self::Workspace,
        Self::Session,
    ];

    /// Returns the name the `statusline` setting uses.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Mode => "mode",
            Self::Unsandboxed => "unsandboxed",
            Self::Context => "context",
            Self::Queued => "queued",
            Self::Effort => "effort",
            Self::Net => "net",
            Self::Branch => "branch",
            Self::Mcp => "mcp",
            Self::Cost => "cost",
            Self::Tasks => "tasks",
            Self::Workspace => "workspace",
            Self::Session => "session",
        }
    }

    /// Returns the field a name stands for, ignoring case.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.trim();
        Self::ALL
            .into_iter()
            .find(|field| field.name().eq_ignore_ascii_case(name))
    }
}

/// Everything the status line displays.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct FooterState {
    /// Pending or active provider retry, absent outside a retried request.
    pub provider_retry: Option<ProviderRetry>,
    /// Model the session is talking to.
    pub model: String,
    /// Effective permission mode.
    pub permission_mode: PermissionMode,
    /// Whether the session is in plan mode, which overrides the mode shown.
    pub planning: bool,
    /// Whether commands run without a sandbox, because none works here and
    /// the user allowed that.
    pub unsandboxed: bool,
    /// Working directory, shown as given.
    pub workspace: String,
    /// Observed or estimated conversation tokens.
    pub context_used: u64,
    /// Source of an estimate, absent for live provider-reported readings.
    pub context_source: Option<&'static str>,
    /// Size of the context window, zero when the provider did not state one.
    pub context_limit: u64,
    /// Identifier of the session, shown shortened.
    pub session_id: String,
    /// Estimated spend so far, already formatted, when prices are known.
    pub cost: Option<String>,
    /// What the interface is doing, which picks the hints.
    pub ui: UiState,
    /// Hints after the idle set, lowest priority last; each is shown only when
    /// it fits whole.
    pub idle_hints: Vec<&'static str>,
    /// Hints after the running set, lowest priority last; each is shown only
    /// when it fits whole.
    pub running_hints: Vec<&'static str>,
    /// A key gesture waiting for its second press, shown in place of the
    /// state's hints.
    pub gesture_hint: Option<&'static str>,
    /// Fields shown, highest priority first; `None` shows every field in the
    /// default order.
    pub fields: Option<Vec<StatusField>>,
    /// Repository branch, or a shortened commit when detached.
    pub branch: Option<String>,
    /// Reasoning effort level, absent when it is automatic.
    pub effort: Option<String>,
    /// Messages queued to run as turns after the running one.
    pub queued: usize,
    /// Whether commands may use the network.
    pub net: bool,
    /// Connected and configured MCP servers.
    pub mcp: Option<(usize, usize)>,
    /// Commands still running in the background.
    pub tasks: usize,
}

/// The provider attempt shown in the renderer-owned status row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProviderRetry {
    /// Attempt to be sent, or currently being streamed, counting from one.
    pub attempt: usize,
    /// Maximum requests allowed for this step.
    pub max_attempts: usize,
    /// Pending wait, absent once the retry starts.
    pub delay: Option<Duration>,
    /// When the next attempt is sent.
    pub retry_at: Option<std::time::SystemTime>,
    /// What failed, as the failure kind's wire name, such as `rate_limited`.
    pub kind: &'static str,
    /// When the endpoint was first found unreachable in this step.
    pub unreachable_since: Option<std::time::Instant>,
}

impl FooterState {
    /// Returns the tokens left in the context window.
    ///
    /// An unstated window reports zero left rather than an arbitrary large
    /// number, so a caller cannot present a budget the provider never promised.
    #[must_use]
    pub const fn context_remaining(&self) -> u64 {
        self.context_limit.saturating_sub(self.context_used)
    }

    /// Returns the share of the context window in use, as a percentage.
    ///
    /// Clamped to `100`, so a reading past the window is a full bar rather than
    /// a percentage that a caller formatting it as a byte would truncate.
    #[must_use]
    pub fn context_percent(&self) -> u8 {
        if self.context_limit == 0 {
            return 0;
        }
        let percent = 100u64
            .saturating_mul(self.context_used.min(self.context_limit))
            .checked_div(self.context_limit)
            .unwrap_or(0);
        u8::try_from(percent).unwrap_or(100)
    }

    /// Returns true when the context window is close to full.
    #[must_use]
    pub fn context_is_tight(&self) -> bool {
        self.context_percent() >= CONTEXT_WARN_PERCENT
    }

    /// Returns the session identifier shortened for display.
    #[must_use]
    pub fn short_session(&self) -> &str {
        self.session_id.get(..8).unwrap_or(&self.session_id)
    }
}

/// How the terminal height is divided between the regions.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Layout {
    /// Total terminal height.
    pub rows: u16,
    /// Rows available to the transcript.
    pub transcript_rows: u16,
    /// Rows reserved for the activity line.
    pub activity_rows: u16,
    /// Rows the prompt occupies.
    pub prompt_rows: u16,
    /// Rows the footer occupies.
    pub footer_rows: u16,
    /// True when the terminal cannot hold the minimum layout.
    pub too_small: bool,
}

impl Layout {
    /// Returns the rows the prompt and footer together occupy.
    #[must_use]
    pub const fn bottom_rows(&self) -> u16 {
        self.prompt_rows.saturating_add(self.footer_rows)
    }
}

/// Divides a terminal height between the transcript, the activity line, the
/// prompt, and the footer.
///
/// `minimum_rows` is the height below which the interface degrades rather than
/// squeeze. An undersized terminal gets a single message row in place of
/// everything, because a transcript with no rows is indistinguishable from a
/// renderer that has stopped working.
#[must_use]
pub fn solve(
    terminal: (u16, u16),
    prompt_rows: u16,
    has_activity: bool,
    minimum_rows: u16,
) -> Layout {
    let rows = terminal.1;
    let prompt_rows = prompt_rows.max(1);
    let activity_rows = u16::from(has_activity);
    let bottom = prompt_rows
        .saturating_add(FOOTER_ROWS)
        .saturating_add(activity_rows);
    if rows < minimum_rows || rows < bottom.saturating_add(1) {
        return Layout {
            rows,
            transcript_rows: 0,
            activity_rows: 0,
            prompt_rows: 0,
            footer_rows: COMPACT_ROWS,
            too_small: true,
        };
    }
    Layout {
        rows,
        transcript_rows: rows.saturating_sub(bottom),
        activity_rows,
        prompt_rows,
        footer_rows: FOOTER_ROWS,
        too_small: false,
    }
}

/// Returns the footer rows for a state, top row first.
///
/// The hint row comes first so the status line sits on the bottom edge of the
/// screen, where a reader looks for it.
#[must_use]
pub fn render(
    state: &FooterState,
    layout: &Layout,
    theme: &Theme,
    width: usize,
    depth: ColorDepth,
) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    if layout.too_small {
        return vec![fit(too_small_row(state, layout, theme, depth), width)];
    }
    vec![
        fit(hint_row(state, theme, depth, width), width),
        fit(status_row(state, theme, depth, width), width),
    ]
}

/// Returns the message shown when the terminal cannot hold the layout.
fn too_small_row(state: &FooterState, layout: &Layout, theme: &Theme, depth: ColorDepth) -> String {
    if let Some(retry) = state.provider_retry {
        return retry_field(retry, theme, depth);
    }
    let mut row = paint(theme, Slot::Error, depth, flag::BOLD);
    let _ = write!(
        row,
        "compact mode ({} rows): prompts work; resize for full interface",
        layout.rows
    );
    row.push_str(&reset(theme));
    // The model stays visible, since it tells the user which session they are
    // looking at even when nothing else fits.
    if !state.model.is_empty() {
        let _ = write!(row, "  [{}]", state.model);
    }
    row
}

/// Returns the hint row.
fn hint_row(state: &FooterState, theme: &Theme, depth: ColorDepth, width: usize) -> String {
    let mut text = state.gesture_hint.unwrap_or(hints(state.ui)).to_owned();
    let extra: &[&str] = match state.ui {
        _ if state.gesture_hint.is_some() => &[],
        UiState::Idle => &state.idle_hints,
        UiState::Running => &state.running_hints,
        UiState::Menu | UiState::Approval => &[],
    };
    for hint in extra {
        let wider = str_width(&text)
            .saturating_add(HINT_GAP.len())
            .saturating_add(str_width(hint));
        if wider > width {
            break;
        }
        text.push_str(HINT_GAP);
        text.push_str(hint);
    }
    let mut row = paint(theme, Slot::Dim, depth, flag::DIM);
    row.push_str(&text);
    row.push_str(&reset(theme));
    row
}

/// Returns the status line, dropping whole fields from the lowest priority
/// until it fits `width`.
///
/// The retry field and the highest-priority field are never dropped; only
/// when they alone are wider does the row get clipped.
fn status_row(state: &FooterState, theme: &Theme, depth: ColorDepth, width: usize) -> String {
    // While a retry waits, the activity row counts it down; the field names
    // the attempt once it is under way.
    let retry = state
        .provider_retry
        .filter(|retry| retry.delay.is_none())
        .map(|retry| retry_field(retry, theme, depth));
    let mut shown: Vec<(StatusField, String)> = state
        .fields
        .as_deref()
        .unwrap_or(&StatusField::ALL)
        .iter()
        .filter_map(|&field| field_text(state, field, theme, depth).map(|text| (field, text)))
        .collect();
    loop {
        let row = join_fields(retry.as_deref(), &shown, theme, depth);
        if shown.len() <= 1 || str_width(&row) <= width {
            return row;
        }
        shown.pop();
    }
}

/// Joins fields with dividers; the session identifier takes a single space.
fn join_fields(
    retry: Option<&str>,
    shown: &[(StatusField, String)],
    theme: &Theme,
    depth: ColorDepth,
) -> String {
    let mut row = String::new();
    let mut first = true;
    if let Some(retry) = retry {
        row.push_str(retry);
        first = false;
    }
    for (field, text) in shown {
        if !first {
            if *field == StatusField::Session {
                row.push(' ');
            } else {
                row.push_str(&divider(theme, depth));
            }
        }
        row.push_str(text);
        first = false;
    }
    row
}

/// Returns one field's styled text, or `None` when it has nothing to show.
fn field_text(
    state: &FooterState,
    field: StatusField,
    theme: &Theme,
    depth: ColorDepth,
) -> Option<String> {
    let styled = |slot: Slot, bits: u16, text: &str| {
        let mut out = paint(theme, slot, depth, bits);
        out.push_str(text);
        out.push_str(&reset(theme));
        out
    };
    match field {
        StatusField::Model => {
            (!state.model.is_empty()).then(|| styled(Slot::Accent, flag::BOLD, &state.model))
        }
        StatusField::Mode => Some(if state.planning {
            styled(Slot::Dim, 0, "plan")
        } else if state.permission_mode == PermissionMode::FullAccess {
            styled(Slot::Warning, 0, state.permission_mode.label())
        } else {
            styled(Slot::Dim, 0, state.permission_mode.label())
        }),
        StatusField::Unsandboxed => state
            .unsandboxed
            .then(|| styled(Slot::Warning, 0, "unsandboxed")),
        StatusField::Context => Some(context_field(state, theme, depth)),
        StatusField::Queued => {
            (state.queued > 0).then(|| styled(Slot::Dim, 0, &format!("queued {}", state.queued)))
        }
        StatusField::Effort => state
            .effort
            .as_ref()
            .map(|level| styled(Slot::Dim, 0, &format!("effort {level}"))),
        StatusField::Net => state.net.then(|| styled(Slot::Dim, 0, "net")),
        StatusField::Branch => state
            .branch
            .as_ref()
            .map(|branch| styled(Slot::Dim, 0, branch)),
        StatusField::Mcp => {
            state
                .mcp
                .filter(|&(_, configured)| configured > 0)
                .map(|(connected, configured)| {
                    let slot = if connected < configured {
                        Slot::Warning
                    } else {
                        Slot::Dim
                    };
                    styled(slot, 0, &format!("mcp {connected}/{configured}"))
                })
        }
        StatusField::Cost => state.cost.as_ref().map(|cost| styled(Slot::Dim, 0, cost)),
        StatusField::Tasks => {
            (state.tasks > 0).then(|| styled(Slot::Dim, 0, &format!("{} running", state.tasks)))
        }
        StatusField::Workspace => (!state.workspace.is_empty())
            .then(|| styled(Slot::Dim, 0, state.workspace.trim_end_matches('/'))),
        StatusField::Session => (!state.session_id.is_empty())
            .then(|| styled(Slot::Dim, flag::DIM, state.short_session())),
    }
}

/// Puts the attempt and pending delay first so they survive width clipping.
fn retry_field(retry: ProviderRetry, theme: &Theme, depth: ColorDepth) -> String {
    let mut field = paint(theme, Slot::Accent, depth, flag::BOLD);
    let _ = write!(
        field,
        "provider retry {}/{}",
        retry.attempt, retry.max_attempts
    );
    if let Some(delay) = retry.delay {
        let _ = write!(field, " in {}ms", delay.as_millis());
    }
    field.push_str(&reset(theme));
    field
}

/// Returns the context usage field of the status line.
///
/// An unstated window prints only the amount spent, since a percentage of a
/// window the provider never stated would be a guess.
fn context_field(state: &FooterState, theme: &Theme, depth: ColorDepth) -> String {
    let mut field = String::new();
    if let Some(source) = state.context_source {
        let slot = if state.context_is_tight() {
            Slot::Error
        } else {
            Slot::Dim
        };
        field.push_str(&paint(theme, slot, depth, 0));
        let _ = write!(
            field,
            "ctx ~{} ({source})",
            format_tokens(state.context_used)
        );
        if state.context_limit > 0 {
            let _ = write!(
                field,
                " ({} left)",
                format_tokens(state.context_remaining())
            );
        }
    } else if state.context_limit == 0 {
        field.push_str(&paint(theme, Slot::Dim, depth, 0));
        if state.context_used == 0 {
            field.push_str("ctx ?");
        } else {
            let _ = write!(
                field,
                "ctx ? {} (provider-reported)",
                format_tokens(state.context_used)
            );
        }
    } else {
        let slot = if state.context_is_tight() {
            Slot::Error
        } else {
            Slot::Success
        };
        field.push_str(&paint(theme, slot, depth, 0));
        let _ = write!(
            field,
            "ctx {}% (provider-reported) ({} left)",
            state.context_percent(),
            format_tokens(state.context_remaining())
        );
    }
    field.push_str(&reset(theme));
    field
}

/// Returns a styled field divider.
fn divider(theme: &Theme, depth: ColorDepth) -> String {
    let mut out = paint(theme, Slot::Divider, depth, 0);
    out.push_str(" | ");
    out.push_str(&reset(theme));
    out
}

/// Returns the escape sequence that selects a slot with attribute bits.
fn paint(theme: &Theme, slot: Slot, depth: ColorDepth, bits: u16) -> String {
    if theme.base() == Base::Mono {
        return String::new();
    }
    let mut style = theme.style(slot, depth);
    style.set_flag(bits);
    if theme.base() == Base::HighContrast {
        style.clear_flag(flag::DIM);
    }
    style.sgr()
}

/// Returns the sequence that closes a styled run.
fn reset(theme: &Theme) -> String {
    if theme.base() == Base::Mono {
        return String::new();
    }
    Style::RESET.to_owned()
}

/// Returns a row clipped to a width, closing a style the cut left open.
fn fit(row: String, width: usize) -> String {
    if str_width(&row) <= width {
        return row;
    }
    let (prefix, _) = truncate_to_width(&row, width);
    let mut out = prefix.to_owned();
    if row.contains('\u{1b}') {
        out.push_str(Style::RESET);
    }
    out
}

/// Formats a token count for display.
#[must_use]
pub fn format_tokens(count: u64) -> String {
    if count < 1_000 {
        return count.to_string();
    }
    if count < 1_000_000 {
        let tenths = count.checked_div(100).unwrap_or(0);
        return format!("{}.{}k", tenths.checked_div(10).unwrap_or(0), tenths % 10);
    }
    let tenths = count.checked_div(100_000).unwrap_or(0);
    format!("{}.{}M", tenths.checked_div(10).unwrap_or(0), tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> FooterState {
        FooterState {
            provider_retry: None,
            model: "claude-sonnet-4".to_owned(),
            permission_mode: PermissionMode::Auto,
            planning: false,
            unsandboxed: false,
            workspace: "/Users/dev/rune/".to_owned(),
            context_used: 24_500,
            context_source: None,
            context_limit: 200_000,
            session_id: "9f2c1a7b4e".to_owned(),
            cost: None,
            ..FooterState::default()
        }
    }

    /// Removes escape sequences so a row can be asserted on as text.
    fn strip(rows: &[String]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                let mut out = String::new();
                let mut rest = row.as_str();
                while let Some(start) = rest.find('\u{1b}') {
                    out.push_str(rest.get(..start).unwrap_or_default());
                    let tail = rest.get(start..).unwrap_or_default();
                    match tail.find('m') {
                        Some(end) => rest = tail.get(end.saturating_add(1)..).unwrap_or_default(),
                        None => rest = "",
                    }
                }
                out.push_str(rest);
                out
            })
            .collect()
    }

    fn status_of(state: &FooterState, width: usize) -> String {
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        let rows = strip(&render(
            state,
            &layout,
            &Theme::dark(),
            width,
            ColorDepth::TrueColor,
        ));
        rows.last().expect("status row").clone()
    }

    #[test]
    fn a_known_spend_is_shown_after_the_context_and_an_unknown_one_is_not() {
        let mut priced = state();
        priced.cost = Some("$0.0123".to_owned());
        let shown = status_of(&priced, 200);
        assert!(shown.contains("$0.0123"), "{shown}");
        assert!(shown.find("ctx") < shown.find("$0.0123"), "{shown}");
        assert!(!status_of(&state(), 200).contains('$'));
    }

    #[test]
    fn a_terminal_below_the_minimum_is_compact() {
        let layout = solve((80, 4), 1, false, DEFAULT_MINIMUM_ROWS);
        assert!(layout.too_small);
        assert_eq!(layout.footer_rows, COMPACT_ROWS);
        assert_eq!(layout.transcript_rows, 0);
        assert_eq!(layout.prompt_rows, 0);
        let rows = render(&state(), &layout, &Theme::dark(), 80, ColorDepth::TrueColor);
        assert_eq!(rows.len(), 1);
        let message = strip(&rows).remove(0);
        assert!(
            message.starts_with("compact mode (4 rows): prompts work; resize for full interface")
        );
        assert!(!message.contains("resize to continue"));
        for row in &rows {
            assert!(str_width(row) <= 80, "row too wide: {row:?}");
        }
    }

    #[test]
    fn a_terminal_without_room_for_the_prompt_is_compact() {
        let layout = solve((80, 5), 4, false, 3);
        assert!(layout.too_small);
    }

    #[test]
    fn a_terminal_without_room_for_the_transcript_is_compact() {
        // Exactly the prompt and footer, with no row left for output.
        let layout = solve((80, 3), 1, false, 1);
        assert!(layout.too_small);
    }

    #[test]
    fn a_compact_row_still_fits_a_narrow_terminal() {
        let layout = solve((12, 3), 1, false, DEFAULT_MINIMUM_ROWS);
        let rows = render(&state(), &layout, &Theme::dark(), 12, ColorDepth::TrueColor);
        assert_eq!(rows.len(), 1);
        assert!(str_width(&rows[0]) <= 12, "row too wide: {:?}", rows[0]);
    }

    #[test]
    fn the_full_layout_reserves_a_row_for_every_region() {
        let layout = solve((100, 40), 2, true, DEFAULT_MINIMUM_ROWS);
        assert!(!layout.too_small);
        assert_eq!(layout.footer_rows, FOOTER_ROWS);
        assert_eq!(layout.prompt_rows, 2);
        assert_eq!(layout.activity_rows, 1);
        assert_eq!(layout.transcript_rows, 35);
        assert_eq!(
            layout
                .transcript_rows
                .saturating_add(layout.bottom_rows())
                .saturating_add(layout.activity_rows),
            layout.rows
        );
    }

    #[test]
    fn render_returns_exactly_the_footer_rows() {
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        let rows = render(
            &state(),
            &layout,
            &Theme::dark(),
            100,
            ColorDepth::TrueColor,
        );
        assert_eq!(rows.len(), usize::from(layout.footer_rows));
    }

    #[test]
    fn provider_retry_uses_one_existing_row_in_full_and_compact_layouts() {
        let mut state = state();
        state.provider_retry = Some(ProviderRetry {
            attempt: 2,
            max_attempts: 3,
            delay: Some(Duration::from_millis(250)),
            retry_at: None,
            kind: "rate_limited",
            unreachable_since: None,
        });
        for height in [4, 24] {
            let layout = solve((80, height), 1, false, DEFAULT_MINIMUM_ROWS);
            for theme in [Theme::no_color(), Theme::high_contrast()] {
                let rows = strip(&render(&state, &layout, &theme, 80, ColorDepth::TrueColor));
                assert_eq!(rows.len(), usize::from(layout.footer_rows));
                // The full layout leaves a waiting retry to the activity row.
                let status = rows.last().expect("status");
                assert_eq!(
                    status.starts_with("provider retry 2/3 in 250ms"),
                    height == 4,
                    "{status}"
                );
                for width in [12, 32, 80] {
                    for row in render(&state, &layout, &theme, width, ColorDepth::TrueColor) {
                        assert!(str_width(&row) <= width);
                    }
                }
            }
        }
        state.provider_retry.as_mut().expect("retry").delay = None;
        let status = status_of(&state, 80);
        assert!(status.starts_with("provider retry 2/3 |"), "{status}");
        assert!(!status.contains("250ms"), "{status}");
        assert!(status.is_ascii(), "{status}");
    }

    #[test]
    fn the_footer_names_an_unsandboxed_session() {
        let mut state = state();
        assert!(!status_of(&state, 120).contains("unsandboxed"));
        state.unsandboxed = true;
        let status = status_of(&state, 120);
        let mode = status
            .find(state.permission_mode.label())
            .expect("mode chip");
        let chip = status.find("unsandboxed").expect("unsandboxed chip");
        assert!(mode < chip, "{status}");
    }

    #[test]
    fn every_row_fits_the_requested_width() {
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        let mut wide = state();
        wide.workspace = "/Users/dev/a very long workspace path that runs on".to_owned();
        wide.model = "a-model-name-that-is-quite-long-indeed".to_owned();
        wide.context_used = 199_999;
        for width in [8usize, 20, 40, 200] {
            let rows = render(&wide, &layout, &Theme::dark(), width, ColorDepth::TrueColor);
            for row in &rows {
                assert!(str_width(row) <= width, "row too wide for {width}: {row:?}");
            }
        }
    }

    #[test]
    fn a_wide_row_never_exceeds_the_requested_width() {
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        let mut wide = state();
        wide.model =
            "\u{4e2d}\u{6587}\u{4e2d}\u{6587}\u{4e2d}\u{6587}\u{1f680}\u{1f680}".to_owned();
        wide.workspace = "\u{4e2d}\u{6587}/\u{1f680}".to_owned();
        for width in [9usize, 13, 21, 55] {
            let rows = render(&wide, &layout, &Theme::dark(), width, ColorDepth::TrueColor);
            for row in &rows {
                assert!(str_width(row) <= width, "row too wide for {width}: {row:?}");
            }
        }
    }

    #[test]
    fn the_status_row_reports_the_model_permission_and_context() {
        let status = status_of(&state(), 100);
        assert!(status.contains("claude-sonnet-4"), "{status}");
        assert!(status.contains("auto"), "{status}");
        assert!(status.contains("12%"), "{status}");
        assert!(status.contains("175.5k left"), "{status}");
        assert!(status.contains("/Users/dev/rune"), "{status}");
        assert!(status.contains("9f2c1a7b"), "{status}");
    }

    #[test]
    fn the_hint_row_is_above_the_status_row() {
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        let rows = strip(&render(
            &state(),
            &layout,
            &Theme::dark(),
            100,
            ColorDepth::TrueColor,
        ));
        assert!(rows[0].contains("/ commands"), "{:?}", rows[0]);
        assert!(rows[1].contains("claude-sonnet-4"), "{:?}", rows[1]);
    }

    #[test]
    fn a_high_context_reading_is_marked() {
        let mut full = state();
        full.context_used = 190_000;
        assert!(full.context_is_tight());
        let status = status_of(&full, 100);
        assert!(status.contains("95%"), "{status}");
    }

    #[test]
    fn a_reading_past_the_window_is_a_full_bar() {
        let mut over = state();
        over.context_used = 500_000;
        assert_eq!(over.context_percent(), 100);
        assert_eq!(over.context_remaining(), 0);
        let status = status_of(&over, 100);
        assert!(status.contains("100%"), "{status}");
    }

    #[test]
    fn a_resume_estimate_names_its_source_even_below_one_percent() {
        let mut resumed = state();
        resumed.context_used = 1234;
        for source in ["saved usage", "history bytes"] {
            resumed.context_source = Some(source);
            for window in [0, 200_000] {
                resumed.context_limit = window;
                let status = status_of(&resumed, 120);
                assert!(
                    status.contains(&format!("ctx ~1.2k ({source})")),
                    "{status}"
                );
                assert!(!status.contains("ctx 0%"), "{status}");
                assert_eq!(status.contains("198.7k left"), window > 0, "{status}");
            }
        }
    }

    #[test]
    fn an_unstated_context_window_shows_only_what_was_spent() {
        let mut unknown = state();
        unknown.context_limit = 0;
        unknown.context_used = 4_200;
        assert_eq!(unknown.context_percent(), 0);
        assert_eq!(unknown.context_remaining(), 0);
        let status = status_of(&unknown, 100);
        assert!(
            status.contains("ctx ? 4.2k (provider-reported)"),
            "{status}"
        );
        assert!(!status.contains('%'), "{status}");
    }

    #[test]
    fn an_unknown_window_with_nothing_spent_shows_a_question_mark() {
        let mut unknown = state();
        unknown.context_limit = 0;
        unknown.context_used = 0;
        unknown.context_source = None;
        let status = status_of(&unknown, 100);
        assert!(status.contains("ctx ?"), "{status}");
        assert!(!status.contains("ctx ? 0"), "{status}");
        assert!(!status.contains("provider-reported"), "{status}");
        assert!(!status.contains('%'), "{status}");
    }

    #[test]
    fn provider_readings_and_byte_estimates_have_distinct_labels() {
        let mut state = state();
        for window in [0, 200_000] {
            state.context_limit = window;
            state.context_source = None;
            let reported = status_of(&state, 160);
            assert!(reported.contains("(provider-reported)"), "{reported}");
            assert!(!reported.contains('~'), "{reported}");
            state.context_source = Some("request bytes");
            let estimated = status_of(&state, 160);
            assert!(
                estimated.contains("ctx ~24.5k (request bytes)"),
                "{estimated}"
            );
            assert!(!estimated.contains("provider-reported"), "{estimated}");
        }
    }

    #[test]
    fn a_colorless_theme_emits_no_escapes() {
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        let rows = render(
            &state(),
            &layout,
            &Theme::no_color(),
            100,
            ColorDepth::TrueColor,
        );
        for row in &rows {
            assert!(!row.contains('\u{1b}'), "escape in {row:?}");
        }
    }

    #[test]
    fn full_access_mode_is_drawn_as_a_warning() {
        let theme = Theme::dark();
        let warning = theme.sgr(Slot::Warning, ColorDepth::TrueColor);
        let mut state = state();
        state.permission_mode = PermissionMode::FullAccess;
        let row = status_row(&state, &theme, ColorDepth::TrueColor, 200);
        assert!(row.contains(&format!("{warning}full access")), "{row:?}");
        state.permission_mode = PermissionMode::Auto;
        let row = status_row(&state, &theme, ColorDepth::TrueColor, 200);
        assert!(!row.contains(&format!("{warning}auto")), "{row:?}");
    }

    #[test]
    fn at_256_colors_the_status_row_is_quantized() {
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        let rows = render(&state(), &layout, &Theme::dark(), 100, ColorDepth::Ansi256);
        assert!(rows[1].contains("38;5;"), "{:?}", rows[1]);
        assert!(!rows[1].contains("38;2;"), "{:?}", rows[1]);
    }

    #[test]
    fn high_contrast_footer_keeps_metadata_bright_on_black() {
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        for depth in [ColorDepth::Ansi256, ColorDepth::TrueColor] {
            let rows = render(&state(), &layout, &Theme::high_contrast(), 100, depth);
            for row in rows {
                let mut grid = crate::engine::Grid::new(100, 1).expect("grid");
                grid.feed(row.as_bytes()).expect("footer");
                for col in 0..100 {
                    let cell = grid.cell(0, col).expect("cell");
                    if cell.style.fg != crate::engine::Color::Default {
                        assert!(!cell.style.has_flag(flag::DIM), "dimmed metadata: {row:?}");
                        assert_eq!(
                            cell.style.bg,
                            if depth == ColorDepth::TrueColor {
                                crate::engine::Color::Rgb(0, 0, 0)
                            } else {
                                crate::engine::Color::Indexed(16)
                            }
                        );
                    }
                }
            }
        }
    }

    fn hint_of(state: &FooterState, width: usize) -> String {
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        let rows = strip(&render(
            state,
            &layout,
            &Theme::dark(),
            width,
            ColorDepth::TrueColor,
        ));
        rows.first().expect("hint row").clone()
    }

    fn full() -> FooterState {
        FooterState {
            model: "replay-model-01".to_owned(),
            context_used: 2_700,
            context_source: Some("request bytes"),
            context_limit: 0,
            queued: 2,
            effort: Some("high".to_owned()),
            net: true,
            branch: Some("main".to_owned()),
            mcp: Some((1, 1)),
            cost: Some("$0.0123".to_owned()),
            tasks: 1,
            ..state()
        }
    }

    #[test]
    fn hints_follow_the_interface_state() {
        let mut state = state();
        for (ui, expected) in [
            (UiState::Idle, "/ commands"),
            (UiState::Running, "ctrl-c interrupt   enter steer"),
            (UiState::Menu, "up/down select   tab complete   esc close"),
            (
                UiState::Approval,
                "up/down select   enter confirm   esc cancel",
            ),
        ] {
            state.ui = ui;
            assert_eq!(hints(ui), expected);
            assert_eq!(hint_of(&state, 100), expected);
        }
        state.idle_hints = vec!["ctrl-j newline"];
        state.ui = UiState::Idle;
        assert_eq!(hint_of(&state, 100), "/ commands   ctrl-j newline");
        assert_eq!(hint_of(&state, 20), "/ commands");
        state.ui = UiState::Running;
        assert_eq!(hint_of(&state, 100), "ctrl-c interrupt   enter steer");
        state.ui = UiState::Idle;
        assert_eq!(hint_of(&state, 12), "/ commands");
    }

    #[test]
    fn a_gesture_hint_replaces_the_state_hints() {
        let mut state = full();
        state.ui = UiState::Running;
        state.running_hints = vec!["tab queue"];
        state.gesture_hint = Some("esc again to interrupt");
        assert_eq!(hint_of(&state, 100), "esc again to interrupt");
    }

    #[test]
    fn running_hints_drop_whole_from_the_lowest_priority() {
        let mut state = state();
        state.ui = UiState::Running;
        state.idle_hints = vec!["ctrl-j newline"];
        state.running_hints = vec!["tab queue", "alt-up edit queued"];
        assert_eq!(
            hint_of(&state, 100),
            "ctrl-c interrupt   enter steer   tab queue   alt-up edit queued"
        );
        assert_eq!(
            hint_of(&state, 50),
            "ctrl-c interrupt   enter steer   tab queue"
        );
        assert_eq!(hint_of(&state, 35), "ctrl-c interrupt   enter steer");
        state.ui = UiState::Idle;
        assert_eq!(hint_of(&state, 100), "/ commands   ctrl-j newline");
    }

    #[test]
    fn low_priority_fields_drop_whole_on_narrow_rows() {
        let state = full();
        assert_eq!(
            status_of(&state, 200),
            "replay-model-01 | auto | ctx ~2.7k (request bytes) | queued 2 | effort high | net \
             | main | mcp 1/1 | $0.0123 | 1 running | /Users/dev/rune 9f2c1a7b"
        );
        assert_eq!(
            status_of(&state, 120),
            "replay-model-01 | auto | ctx ~2.7k (request bytes) | queued 2 | effort high | net \
             | main | mcp 1/1 | $0.0123 | 1 running"
        );
        assert_eq!(
            status_of(&state, 119),
            "replay-model-01 | auto | ctx ~2.7k (request bytes) | queued 2 | effort high | net \
             | main | mcp 1/1 | $0.0123"
        );
        assert_eq!(
            status_of(&state, 60),
            "replay-model-01 | auto | ctx ~2.7k (request bytes)"
        );
        assert_eq!(status_of(&state, 32), "replay-model-01 | auto");
        assert_eq!(status_of(&state, 16), "replay-model-01");
        assert_eq!(status_of(&state, 9), "replay-mo");
        let mut retried = state;
        retried.provider_retry = Some(ProviderRetry {
            attempt: 2,
            max_attempts: 3,
            delay: None,
            retry_at: None,
            kind: "unavailable",
            unreachable_since: None,
        });
        assert_eq!(
            status_of(&retried, 40),
            "provider retry 2/3 | replay-model-01"
        );
    }

    #[test]
    fn statusline_setting_orders_and_limits_fields() {
        let mut state = full();
        state.fields = Some(vec![StatusField::Session]);
        assert_eq!(status_of(&state, 120), "9f2c1a7b");
        state.fields = Some(vec![
            StatusField::Context,
            StatusField::Branch,
            StatusField::Model,
        ]);
        assert_eq!(
            status_of(&state, 120),
            "ctx ~2.7k (request bytes) | main | replay-model-01"
        );
        assert_eq!(status_of(&state, 34), "ctx ~2.7k (request bytes) | main");
        assert_eq!(status_of(&state, 30), "ctx ~2.7k (request bytes)");
        state.fields = Some(Vec::new());
        assert_eq!(status_of(&state, 120), "");
        let names: Vec<&str> = StatusField::ALL.iter().map(|field| field.name()).collect();
        assert_eq!(names, rune_core::config::STATUSLINE_FIELDS);
        assert_eq!(
            StatusField::from_name(" Branch "),
            Some(StatusField::Branch)
        );
        assert_eq!(StatusField::from_name("retry"), None);
    }

    #[test]
    fn mcp_field_warns_when_a_server_is_missing() {
        let theme = Theme::dark();
        let depth = ColorDepth::TrueColor;
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        let row_of = |mcp: Option<(usize, usize)>| {
            let state = FooterState { mcp, ..state() };
            render(&state, &layout, &theme, 200, depth)
                .pop()
                .expect("status row")
        };
        let warning = paint(&theme, Slot::Warning, depth, 0);
        let dim = paint(&theme, Slot::Dim, depth, 0);
        assert_ne!(warning, dim);
        let missing = row_of(Some((1, 2)));
        assert!(
            missing.contains(&format!("{warning}mcp 1/2")),
            "{missing:?}"
        );
        let all = row_of(Some((2, 2)));
        assert!(all.contains(&format!("{dim}mcp 2/2")), "{all:?}");
        for none in [None, Some((0, 0))] {
            let row = strip(&[row_of(none)]).remove(0);
            assert!(!row.contains("mcp"), "{row}");
        }
    }

    #[test]
    fn token_counts_are_formatted_at_each_scale() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(24_500), "24.5k");
        assert_eq!(format_tokens(200_000), "200.0k");
        assert_eq!(format_tokens(999_999), "999.9k");
        assert_eq!(format_tokens(1_250_000), "1.2M");
    }
}
