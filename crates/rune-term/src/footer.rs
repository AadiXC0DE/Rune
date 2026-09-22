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

use rune_core::config::PermissionMode;

use crate::engine::{Style, flag};
use crate::theme::{Base, Slot, Theme};
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

/// The key hints on the row above the status line.
pub const HINTS: &str = "ctrl-c cancel  /help commands  esc clear";

/// Everything the status line displays.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct FooterState {
    /// Model the session is talking to.
    pub model: String,
    /// Effective permission mode.
    pub permission_mode: PermissionMode,
    /// Working directory, shown as given.
    pub workspace: String,
    /// Tokens already spent from the context window.
    pub context_used: u64,
    /// Size of the context window, zero when the provider did not state one.
    pub context_limit: u64,
    /// Identifier of the session, shown shortened.
    pub session_id: String,
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
    truecolor: bool,
) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    if layout.too_small {
        return vec![fit(too_small_row(state, layout, theme, truecolor), width)];
    }
    vec![
        fit(hint_row(theme, truecolor), width),
        fit(status_row(state, theme, truecolor), width),
    ]
}

/// Returns the message shown when the terminal cannot hold the layout.
fn too_small_row(state: &FooterState, layout: &Layout, theme: &Theme, truecolor: bool) -> String {
    let mut row = paint(theme, Slot::Error, truecolor, flag::BOLD);
    let _ = write!(
        row,
        "terminal is {} rows, too small for the interface; resize to continue",
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
fn hint_row(theme: &Theme, truecolor: bool) -> String {
    let mut row = paint(theme, Slot::Dim, truecolor, flag::DIM);
    row.push_str(HINTS);
    row.push_str(&reset(theme));
    row
}

/// Returns the status line.
fn status_row(state: &FooterState, theme: &Theme, truecolor: bool) -> String {
    let mut row = String::new();
    if !state.model.is_empty() {
        row.push_str(&paint(theme, Slot::Accent, truecolor, flag::BOLD));
        row.push_str(&state.model);
        row.push_str(&reset(theme));
        row.push_str(&divider(theme, truecolor));
    }
    row.push_str(&paint(theme, Slot::Dim, truecolor, 0));
    row.push_str(state.permission_mode.label());
    row.push_str(&reset(theme));
    row.push_str(&divider(theme, truecolor));
    row.push_str(&context_field(state, theme, truecolor));
    if !state.workspace.is_empty() {
        row.push_str(&divider(theme, truecolor));
        row.push_str(&paint(theme, Slot::Dim, truecolor, 0));
        row.push_str(state.workspace.trim_end_matches('/'));
        row.push_str(&reset(theme));
    }
    if !state.session_id.is_empty() {
        row.push(' ');
        row.push_str(&paint(theme, Slot::Dim, truecolor, flag::DIM));
        row.push_str(state.short_session());
        row.push_str(&reset(theme));
    }
    row
}

/// Returns the context usage field of the status line.
///
/// An unstated window prints only the amount spent, since a percentage of a
/// window the provider never stated would be a guess.
fn context_field(state: &FooterState, theme: &Theme, truecolor: bool) -> String {
    let mut field = String::new();
    if state.context_limit == 0 {
        field.push_str(&paint(theme, Slot::Dim, truecolor, 0));
        let _ = write!(field, "ctx {}", format_tokens(state.context_used));
    } else {
        let slot = if state.context_is_tight() {
            Slot::Error
        } else {
            Slot::Success
        };
        field.push_str(&paint(theme, slot, truecolor, 0));
        let _ = write!(
            field,
            "ctx {}% ({} left)",
            state.context_percent(),
            format_tokens(state.context_remaining())
        );
    }
    field.push_str(&reset(theme));
    field
}

/// Returns a styled field divider.
fn divider(theme: &Theme, truecolor: bool) -> String {
    let mut out = paint(theme, Slot::Divider, truecolor, 0);
    out.push_str(" | ");
    out.push_str(&reset(theme));
    out
}

/// Returns the escape sequence that selects a slot with attribute bits.
fn paint(theme: &Theme, slot: Slot, truecolor: bool, bits: u16) -> String {
    if theme.base() == Base::Mono {
        return String::new();
    }
    let mut style = theme.style(slot, truecolor);
    style.set_flag(bits);
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
            model: "claude-sonnet-4".to_owned(),
            permission_mode: PermissionMode::Auto,
            workspace: "/Users/dev/rune/".to_owned(),
            context_used: 24_500,
            context_limit: 200_000,
            session_id: "9f2c1a7b4e".to_owned(),
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
        let rows = strip(&render(state, &layout, &Theme::fx_dark(), width, true));
        rows.last().expect("status row").clone()
    }

    #[test]
    fn a_terminal_below_the_minimum_is_compact() {
        let layout = solve((80, 4), 1, false, DEFAULT_MINIMUM_ROWS);
        assert!(layout.too_small);
        assert_eq!(layout.footer_rows, COMPACT_ROWS);
        assert_eq!(layout.transcript_rows, 0);
        assert_eq!(layout.prompt_rows, 0);
        let rows = render(&state(), &layout, &Theme::fx_dark(), 80, true);
        assert_eq!(rows.len(), 1);
        assert!(strip(&rows)[0].contains("too small"));
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
        let rows = render(&state(), &layout, &Theme::fx_dark(), 12, true);
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
        let rows = render(&state(), &layout, &Theme::fx_dark(), 100, true);
        assert_eq!(rows.len(), usize::from(layout.footer_rows));
    }

    #[test]
    fn every_row_fits_the_requested_width() {
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        let mut wide = state();
        wide.workspace = "/Users/dev/a very long workspace path that runs on".to_owned();
        wide.model = "a-model-name-that-is-quite-long-indeed".to_owned();
        wide.context_used = 199_999;
        for width in [8usize, 20, 40, 200] {
            let rows = render(&wide, &layout, &Theme::fx_dark(), width, true);
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
            let rows = render(&wide, &layout, &Theme::fx_dark(), width, true);
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
        let rows = strip(&render(&state(), &layout, &Theme::fx_dark(), 100, true));
        assert!(rows[0].contains("ctrl-c cancel"), "{:?}", rows[0]);
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
    fn an_unstated_context_window_shows_only_what_was_spent() {
        let mut unknown = state();
        unknown.context_limit = 0;
        unknown.context_used = 4_200;
        assert_eq!(unknown.context_percent(), 0);
        assert_eq!(unknown.context_remaining(), 0);
        let status = status_of(&unknown, 100);
        assert!(status.contains("ctx 4.2k"), "{status}");
        assert!(!status.contains('%'), "{status}");
    }

    #[test]
    fn a_colorless_theme_emits_no_escapes() {
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        let rows = render(&state(), &layout, &Theme::no_color(), 100, true);
        for row in &rows {
            assert!(!row.contains('\u{1b}'), "escape in {row:?}");
        }
    }

    #[test]
    fn without_truecolor_the_status_row_is_quantized() {
        let layout = solve((100, 40), 1, false, DEFAULT_MINIMUM_ROWS);
        let rows = render(&state(), &layout, &Theme::fx_dark(), 100, false);
        assert!(rows[1].contains("38;5;"), "{:?}", rows[1]);
        assert!(!rows[1].contains("38;2;"), "{:?}", rows[1]);
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
