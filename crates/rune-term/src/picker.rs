//! Choosing one item from a list.
//!
//! The picker draws into the same region as the input line rather than taking
//! the alternate screen, so the transcript above it stays readable while a
//! choice is being made and the terminal's own scrollback keeps working.
//!
//! Nothing here reads the terminal or writes to it: [`Picker::rows`] returns
//! the rows to draw, and the caller decides when they are written. That is what
//! makes a selection assertable in a test without a terminal.

use crate::theme::{Slot, Theme};

/// Closes a styled row.
const RESET: &str = "\u{1b}[0m";

/// How many items are shown at once when the caller names no window.
pub const DEFAULT_WINDOW: usize = 10;

/// A single-column list a user narrows by typing and moves through with the
/// arrow keys.
///
/// The selection is a position in the matching list rather than a marked item,
/// so an entry that appears twice stays independently selectable and the same
/// item can be reached by different queries.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Picker {
    title: String,
    items: Vec<String>,
    /// Each item lowercased once, so narrowing a long catalog does not
    /// re-case every entry on every keystroke.
    folded: Vec<String>,
    /// What the user has typed. Empty means everything matches.
    query: String,
    /// Index into the matching entries, not into `items`.
    cursor: usize,
    /// First matching entry on screen. Moved only far enough to keep the
    /// cursor visible, so the list does not jump while a user steps through it.
    offset: usize,
    window: usize,
    /// Index into `items` of the entry already in effect, marked so a user can
    /// see where they are without selecting it.
    current: Option<usize>,
}

impl Picker {
    /// Builds a picker over `items`, with the first match selected.
    ///
    /// A window of zero would show nothing, so it is raised to one.
    #[must_use]
    pub fn new(title: impl Into<String>, items: Vec<String>, window: usize) -> Self {
        let folded = items.iter().map(|item| item.to_lowercase()).collect();
        Self {
            title: title.into(),
            items,
            folded,
            query: String::new(),
            cursor: 0,
            offset: 0,
            window: window.max(1),
            current: None,
        }
    }

    /// Marks the entry already in effect.
    ///
    /// A value that is not in the list is ignored, so a caller does not have to
    /// check first.
    #[must_use]
    pub fn with_current(mut self, current: Option<&str>) -> Self {
        self.current =
            current.and_then(|current| self.items.iter().position(|item| item == current));
        self
    }

    /// Returns the heading shown above the items.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns the number of entries, ignoring the query.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Returns whether there is nothing to choose from at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Returns what the user has typed to narrow the list.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Narrows the list to entries containing `query`, ignoring case.
    ///
    /// The cursor returns to the first match, because the entry it was on may
    /// no longer be in the list at all.
    pub fn set_query(&mut self, query: &str) {
        if self.query == query {
            return;
        }
        query.clone_into(&mut self.query);
        self.cursor = 0;
        self.offset = 0;
    }

    /// Returns the positions in `items` that match the query, in list order.
    #[must_use]
    pub fn matches(&self) -> Vec<usize> {
        if self.query.is_empty() {
            return (0..self.items.len()).collect();
        }
        let needle = self.query.to_lowercase();
        self.folded
            .iter()
            .enumerate()
            .filter(|(_, folded)| folded.contains(&needle))
            .map(|(index, _)| index)
            .collect()
    }

    /// Returns whether any entry matches the query.
    #[must_use]
    pub fn has_matches(&self) -> bool {
        self.query.is_empty() || !self.matches().is_empty()
    }

    /// Returns the index of the highlighted entry within `items`.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.matches().get(self.cursor).copied().unwrap_or(0)
    }

    /// Returns the highlighted entry.
    #[must_use]
    pub fn selected(&self) -> Option<&str> {
        let matches = self.matches();
        matches
            .get(self.cursor)
            .and_then(|index| self.items.get(*index))
            .map(String::as_str)
    }

    /// Returns every entry, in order, ignoring the query.
    #[must_use]
    pub fn items(&self) -> &[String] {
        &self.items
    }

    /// Moves the highlight up one entry.
    ///
    /// Moving past the first entry stops rather than wrapping, so holding a key
    /// down cannot send the highlight to the far end of a long list.
    ///
    /// Returns whether the highlight moved.
    pub fn up(&mut self) -> bool {
        let Some(next) = self.cursor.checked_sub(1) else {
            return false;
        };
        self.cursor = next;
        self.follow();
        true
    }

    /// Moves the highlight down one entry, stopping at the last.
    ///
    /// Returns whether the highlight moved.
    pub fn down(&mut self) -> bool {
        let count = self.matches().len();
        let Some(next) = self.cursor.checked_add(1).filter(|next| *next < count) else {
            return false;
        };
        self.cursor = next;
        self.follow();
        true
    }

    /// Moves the highlight one window up.
    pub fn page_up(&mut self) -> bool {
        self.to(self.cursor.saturating_sub(self.window))
    }

    /// Moves the highlight one window down.
    pub fn page_down(&mut self) -> bool {
        self.to(self.cursor.saturating_add(self.window))
    }

    /// Moves the highlight onto a position in the matching list.
    ///
    /// Returns whether the highlight moved.
    pub fn to(&mut self, position: usize) -> bool {
        if position >= self.matches().len() || position == self.cursor {
            return false;
        }
        self.cursor = position;
        self.follow();
        true
    }

    /// Moves the window so the highlight is inside it.
    fn follow(&mut self) {
        let last = self.offset.saturating_add(self.window);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= last {
            // The highlight lands on the window's last row, which is where a
            // reader expects it after a step rather than a page.
            self.offset = self.cursor.saturating_add(1).saturating_sub(self.window);
        }
    }

    /// Returns the rows to draw, styled for the theme.
    ///
    /// The highlighted entry carries a marker as well as a color, so a terminal
    /// without color and a reader who cannot see the difference both still know
    /// which entry is highlighted.
    #[must_use]
    pub fn rows(&self, theme: &Theme, truecolor: bool) -> Vec<String> {
        let accent = theme.sgr(Slot::Accent, truecolor);
        let dim = theme.sgr(Slot::Dim, truecolor);
        let matches = self.matches();
        let shown = matches
            .iter()
            .skip(self.offset)
            .take(self.window)
            .copied()
            .collect::<Vec<usize>>();

        if shown.is_empty() {
            return vec![styled(&dim, &format!("  no match for `{}`", self.query))];
        }

        let mut rows: Vec<String> = Vec::with_capacity(shown.len().saturating_add(1));
        for (position, index) in shown.iter().enumerate() {
            let Some(item) = self.items.get(*index) else {
                continue;
            };
            let here = self.offset.saturating_add(position) == self.cursor;
            let mut row = if here {
                styled(&accent, &format!("> {item}"))
            } else {
                format!("  {item}")
            };
            // Which entry is already in effect, so a user can tell that the
            // highlighted row is not necessarily the active one.
            if self.current == Some(*index) {
                row.push_str(&styled(&dim, " (current)"));
            }
            rows.push(row);
        }

        // Which part of a long list is on screen, so a narrowed list is not
        // mistaken for a list that ends here.
        if matches.len() > self.window {
            let first = self.offset.saturating_add(1);
            let last = self.offset.saturating_add(rows.len());
            rows.push(styled(
                &dim,
                &format!("  {first}-{last} of {}", matches.len()),
            ));
        }
        rows
    }

    /// Returns the keys the picker responds to.
    #[must_use]
    pub const fn hint() -> &'static str {
        "type to narrow, up/down choose, enter accept, esc cancel"
    }
}

/// Wraps `text` in `open`, closing it again.
///
/// A theme without color yields an empty sequence, and a row surrounded by two
/// empty strings would carry escapes the terminal has nothing to do with. The
/// text is returned untouched in that case so a colorless terminal receives
/// exactly the characters it draws.
fn styled(open: &str, text: &str) -> String {
    if open.is_empty() {
        return text.to_owned();
    }
    format!("{open}{text}{RESET}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("item-{index}")).collect()
    }

    fn models() -> Vec<String> {
        vec![
            "claude-sonnet-4".to_owned(),
            "claude-opus-4".to_owned(),
            "gpt-5".to_owned(),
        ]
    }

    #[test]
    fn the_first_entry_starts_highlighted() {
        let picker = Picker::new("models", items(3), DEFAULT_WINDOW);
        assert_eq!(picker.cursor(), 0);
        assert_eq!(picker.selected(), Some("item-0"));
    }

    #[test]
    fn moving_down_and_up_changes_the_highlight() {
        let mut picker = Picker::new("models", items(3), DEFAULT_WINDOW);
        assert!(picker.down());
        assert_eq!(picker.selected(), Some("item-1"));
        assert!(picker.up());
        assert_eq!(picker.selected(), Some("item-0"));
    }

    #[test]
    fn the_highlight_stops_at_both_ends() {
        // Wrapping would make a held key land on the far end of the list, which
        // is not where the user was looking.
        let mut picker = Picker::new("models", items(2), DEFAULT_WINDOW);
        assert!(!picker.up());
        assert_eq!(picker.cursor(), 0);
        assert!(picker.down());
        assert!(!picker.down());
        assert_eq!(picker.cursor(), 1);
    }

    #[test]
    fn an_empty_list_selects_nothing() {
        let mut picker = Picker::new("models", Vec::new(), DEFAULT_WINDOW);
        assert!(picker.is_empty());
        assert_eq!(picker.selected(), None);
        assert!(!picker.down());
    }

    #[test]
    fn typing_narrows_the_list_and_picks_a_substring() {
        let mut picker = Picker::new("models", models(), DEFAULT_WINDOW);
        picker.set_query("opus");
        assert_eq!(picker.matches().len(), 1);
        assert_eq!(picker.selected(), Some("claude-opus-4"));
    }

    #[test]
    fn narrowing_ignores_case() {
        let mut picker = Picker::new("models", models(), DEFAULT_WINDOW);
        picker.set_query("CLAUDE");
        assert_eq!(picker.matches().len(), 2);
    }

    #[test]
    fn narrowing_moves_the_highlight_back_to_the_first_match() {
        // The entry the highlight was on may not be in the new list at all.
        let mut picker = Picker::new("models", models(), DEFAULT_WINDOW);
        picker.down();
        picker.down();
        assert_eq!(picker.cursor(), 2);
        picker.set_query("claude");
        assert_eq!(picker.cursor(), 0);
        assert_eq!(picker.selected(), Some("claude-sonnet-4"));
    }

    #[test]
    fn a_query_matching_nothing_reports_itself() {
        let theme = Theme::no_color();
        let mut picker = Picker::new("models", models(), DEFAULT_WINDOW);
        assert!(picker.has_matches());
        picker.set_query("zzz");
        assert!(!picker.has_matches());
        assert_eq!(picker.selected(), None);
        let rows = picker.rows(&theme, false);
        assert!(rows.iter().any(|row| row.contains("no match")), "{rows:?}");
    }

    #[test]
    fn clearing_the_query_restores_every_entry() {
        let mut picker = Picker::new("models", models(), DEFAULT_WINDOW);
        picker.set_query("opus");
        picker.set_query("");
        assert_eq!(picker.matches().len(), 3);
    }

    #[test]
    fn the_window_follows_the_highlight_down_a_long_list() {
        let mut picker = Picker::new("models", items(20), 4);
        for _ in 0..9 {
            picker.down();
        }
        assert_eq!(picker.cursor(), 9);
        assert_eq!(picker.offset, 6);
    }

    #[test]
    fn the_window_follows_the_highlight_back_up() {
        let mut picker = Picker::new("models", items(20), 4);
        for _ in 0..9 {
            picker.down();
        }
        for _ in 0..4 {
            picker.up();
        }
        assert_eq!(picker.cursor(), 5);
        assert_eq!(picker.offset, 5);
    }

    #[test]
    fn a_page_moves_by_one_window() {
        let mut picker = Picker::new("models", items(20), 5);
        assert!(picker.page_down());
        assert_eq!(picker.cursor(), 5);
        assert!(picker.page_up());
        assert_eq!(picker.cursor(), 0);
    }

    #[test]
    fn the_marker_moves_with_the_highlight() {
        let theme = Theme::no_color();
        let mut picker = Picker::new("models", items(3), DEFAULT_WINDOW);
        let first = picker.rows(&theme, false);
        assert_eq!(first.first().map(String::as_str), Some("> item-0"));
        assert_eq!(first.get(1).map(String::as_str), Some("  item-1"));

        picker.down();
        let second = picker.rows(&theme, false);
        assert_eq!(second.first().map(String::as_str), Some("  item-0"));
        assert_eq!(second.get(1).map(String::as_str), Some("> item-1"));
    }

    #[test]
    fn only_the_window_is_drawn() {
        let theme = Theme::no_color();
        let mut picker = Picker::new("models", items(20), 4);
        picker.to(9);
        let rows = picker.rows(&theme, false);
        // Four entries plus the position line.
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().any(|row| row.contains("item-9")), "{rows:?}");
    }

    #[test]
    fn a_list_that_fits_carries_no_position_line() {
        let theme = Theme::no_color();
        let picker = Picker::new("models", items(3), DEFAULT_WINDOW);
        let rows = picker.rows(&theme, false);
        assert_eq!(rows.len(), 3);
        assert!(!rows.iter().any(|row| row.contains(" of ")), "{rows:?}");
    }

    #[test]
    fn the_entry_in_effect_is_marked_without_changing_its_value() {
        let theme = Theme::no_color();
        let picker = Picker::new("models", models(), DEFAULT_WINDOW).with_current(Some("gpt-5"));
        let rows = picker.rows(&theme, false);
        assert!(
            rows.iter().any(|row| row == "  gpt-5 (current)"),
            "{rows:?}"
        );
        // The marker is presentation only, so the chosen value stays the id.
        assert_eq!(picker.selected(), Some("claude-sonnet-4"));
    }

    #[test]
    fn a_current_entry_outside_the_list_is_ignored() {
        let picker = Picker::new("models", models(), DEFAULT_WINDOW).with_current(Some("nope"));
        assert_eq!(picker.current, None);
    }

    #[test]
    fn a_narrowed_list_shows_its_extent() {
        let theme = Theme::no_color();
        let mut picker = Picker::new("models", items(30), 4);
        picker.set_query("item-1");
        picker.set_query("item-");
        let rows = picker.rows(&theme, false);
        assert!(
            rows.last().is_some_and(|row| row.contains(" of ")),
            "{rows:?}"
        );
    }

    #[test]
    fn the_highlighted_row_is_colored_when_the_theme_has_color() {
        let theme = Theme::fx_dark();
        let picker = Picker::new("models", items(2), DEFAULT_WINDOW);
        let rows = picker.rows(&theme, true);
        assert!(
            rows.first().is_some_and(|row| row.contains("\u{1b}[")),
            "{rows:?}"
        );
    }

    #[test]
    fn a_colorless_theme_emits_no_escapes_at_all() {
        // A row wrapped in an empty open sequence and a reset would send bytes
        // the terminal has no use for, and a test reading row text would see
        // characters that are not on screen.
        let theme = Theme::no_color();
        let picker = Picker::new("models", items(12), 2).with_current(Some("item-0"));
        for row in picker.rows(&theme, false) {
            assert!(!row.contains('\u{1b}'), "{row:?}");
        }
    }
}
