//! The scrolling transcript screen.
//!
//! The inline transcript keeps the newest rows against the prompt, which suits
//! a reader watching a live session. Reading a whole conversation needs the
//! opposite: every row, scrollable and searchable, on the terminal's alternate
//! screen so the live session underneath is left where it was.
//!
//! The alternate screen holds one image, so ownership of it is a token that
//! cannot be copied and the machine that hands it out holds at most one. A
//! second surface asking for the screen while the transcript owns it is
//! refused rather than allowed to paint over the first. Frames go through
//! [`crate::frame::compose`], so the transcript is diffed, committed, and
//! verified by replay exactly like every other frame.

use rune_core::error::Result;

use crate::engine::Grid;
use crate::frame::{FrameCommit, FrameSurface, Regions};
use crate::transcript::sanitize;

/// The screen a frame is drawn on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Screen {
    /// The live session, written inline with finished content promoted into the
    /// terminal's own scrollback.
    #[default]
    Main,
    /// The whole conversation, on the terminal's alternate screen.
    Transcript,
}

/// Exclusive ownership of the alternate screen.
///
/// The token is neither `Clone` nor `Copy` and only [`Screens`] mints one, so
/// the alternate screen cannot end up with two owners that overwrite each
/// other's cells.
#[derive(Debug)]
pub struct AlternateScreen {
    /// Sealed so a token can only come from [`Screens`].
    _sealed: (),
}

/// Decides which screen is drawn and holds the alternate screen.
#[derive(Debug, Default)]
pub struct Screens {
    current: Screen,
    owner: Option<AlternateScreen>,
}

impl Screens {
    /// Returns a machine on the main screen with the alternate screen free.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current: Screen::Main,
            owner: None,
        }
    }

    /// Returns the screen being drawn.
    #[must_use]
    pub const fn current(&self) -> Screen {
        self.current
    }

    /// Returns the token owning the alternate screen, when the transcript is
    /// open.
    #[must_use]
    pub const fn alternate(&self) -> Option<&AlternateScreen> {
        self.owner.as_ref()
    }

    /// Returns how many surfaces own the alternate screen: zero or one.
    #[must_use]
    pub const fn owners(&self) -> usize {
        if self.owner.is_some() { 1 } else { 0 }
    }

    /// Opens the transcript on the alternate screen.
    ///
    /// Returns true when the screen changed. Entering while the transcript
    /// already owns the screen is a no-op and grants nothing, so the alternate
    /// screen never has a second owner.
    pub fn enter(&mut self) -> bool {
        if self.current == Screen::Transcript {
            return false;
        }
        self.current = Screen::Transcript;
        self.owner = Some(AlternateScreen { _sealed: () });
        true
    }

    /// Closes the transcript and returns to the main screen.
    ///
    /// Returns true when the screen changed. Leaving while the main screen is
    /// showing is a no-op.
    pub fn leave(&mut self) -> bool {
        if self.current == Screen::Main {
            return false;
        }
        self.current = Screen::Main;
        self.owner = None;
        true
    }
}

/// The whole conversation, as a view that scrolls, searches, and jumps.
///
/// The rows are owned rather than borrowed from the inline renderer, because
/// this view outlives a single frame and must not shift under the reader while
/// it is being read.
#[derive(Debug)]
pub struct Transcript {
    rows: Vec<String>,
    /// Row sitting at the top of the viewport.
    top: usize,
    query: Option<String>,
    hits: Vec<usize>,
    /// Index into `hits` of the match the reader is on.
    selected: Option<usize>,
}

impl Transcript {
    /// Returns a transcript over the conversation rows, oldest first.
    #[must_use]
    pub fn new(rows: Vec<String>) -> Self {
        Self {
            rows,
            top: 0,
            query: None,
            hits: Vec::new(),
            selected: None,
        }
    }

    /// Returns the number of rows in the conversation.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.rows.len()
    }

    /// Returns true when there is nothing to show.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Returns the conversation rows.
    #[must_use]
    pub fn rows(&self) -> &[String] {
        self.rows.as_slice()
    }

    /// Returns the row at the top of the viewport.
    #[must_use]
    pub const fn top(&self) -> usize {
        self.top
    }

    /// Returns the active query.
    #[must_use]
    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    /// Returns the rows matching the active query.
    #[must_use]
    pub fn matches(&self) -> &[usize] {
        &self.hits
    }

    /// Returns the position within [`Transcript::matches`] the reader is on.
    #[must_use]
    pub const fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Returns the conversation rows a screen of `rows` rows holds.
    ///
    /// The header takes a row while a search is active, so navigation and
    /// composition have to agree on the count of rows left for the
    /// conversation. This is the one place it is decided.
    #[must_use]
    pub fn body(&self, rows: u16) -> usize {
        usize::from(rows).saturating_sub(usize::from(self.query.is_some()))
    }

    /// Returns the rows on screen, at most `height` of them.
    ///
    /// `height` counts conversation rows, which is [`Transcript::body`] for the
    /// screen being drawn. A viewport that would run past the last row is
    /// pulled back, so the last row of the conversation is the last row on
    /// screen rather than one floating above a gap.
    #[must_use]
    pub fn visible(&self, height: usize) -> &[String] {
        let top = self.top.min(self.deepest(height));
        let end = top.saturating_add(height).min(self.rows.len());
        self.rows.get(top..end).unwrap_or_default()
    }

    /// Moves the viewport down by `lines`, stopping at the last screen.
    ///
    /// Scrolling past the end stays at the end; it does not wrap to the top.
    pub fn scroll_down(&mut self, lines: usize, height: usize) {
        self.top = self.top.saturating_add(lines).min(self.deepest(height));
    }

    /// Moves the viewport up by `lines`, stopping at the first row.
    ///
    /// No height is needed: the first row is the bound whichever rows are on
    /// screen.
    pub fn scroll_up(&mut self, lines: usize) {
        self.top = self.top.saturating_sub(lines);
    }

    /// Moves the viewport down by one screen, stopping at the last screen.
    pub fn page_down(&mut self, height: usize) {
        self.scroll_down(height, height);
    }

    /// Moves the viewport up by one screen, stopping at the first row.
    pub fn page_up(&mut self, height: usize) {
        self.scroll_up(height);
    }

    /// Puts a row at the top of the viewport, clamped to the conversation.
    ///
    /// A jump past the end lands on the last screen instead of past the
    /// content.
    pub fn jump(&mut self, row: usize, height: usize) {
        self.top = row.min(self.deepest(height));
    }

    /// Searches for `query` and returns the rows that match.
    ///
    /// Case is folded for ASCII, which keeps a search over a long conversation
    /// allocation free; a query outside the ASCII range matches exactly. An
    /// empty query clears the search.
    ///
    /// The reader is put on the first match at or after the top row, so a
    /// search from the middle of the conversation continues forward, and on the
    /// first match when there is none below. The match is scrolled into view.
    pub fn search(&mut self, query: &str, height: usize) -> &[usize] {
        self.query = (!query.is_empty()).then(|| query.to_owned());
        self.hits = if query.is_empty() {
            Vec::new()
        } else {
            self.rows
                .iter()
                .enumerate()
                .filter(|(_, row)| contains(row, query))
                .map(|(index, _)| index)
                .collect()
        };
        self.selected = if self.hits.is_empty() {
            None
        } else {
            Some(
                self.hits
                    .iter()
                    .position(|row| *row >= self.top)
                    .unwrap_or(0),
            )
        };
        if let Some(row) = self
            .selected
            .and_then(|index| self.hits.get(index))
            .copied()
        {
            self.reveal(row, height);
        }
        &self.hits
    }

    /// Selects the next match, stopping at the last one.
    pub fn next_match(&mut self, height: usize) {
        let Some(last) = self.hits.len().checked_sub(1) else {
            return;
        };
        let next = self
            .selected
            .map_or(0, |index| index.saturating_add(1))
            .min(last);
        self.selected = Some(next);
        if let Some(row) = self.hits.get(next).copied() {
            self.reveal(row, height);
        }
    }

    /// Selects the previous match, stopping at the first one.
    pub fn previous_match(&mut self, height: usize) {
        if self.hits.is_empty() {
            return;
        }
        let previous = self.selected.map_or(0, |index| index.saturating_sub(1));
        self.selected = Some(previous);
        if let Some(row) = self.hits.get(previous).copied() {
            self.reveal(row, height);
        }
    }

    /// Returns the header for the current search, when one is active.
    ///
    /// The count is what tells a reader whether to keep paging: a search that
    /// found nothing and a search on its last match look the same without it.
    #[must_use]
    pub fn header(&self) -> Option<String> {
        let query = sanitize(self.query.as_deref()?);
        if self.hits.is_empty() {
            return Some(format!("no matches for {query:?}"));
        }
        let position = self.selected.map_or(1, |index| index.saturating_add(1));
        Some(format!(
            "match {position}/{} for {query:?}",
            self.hits.len()
        ))
    }

    /// Returns the highest row that can sit at the top of a viewport `height`
    /// rows tall.
    const fn deepest(&self, height: usize) -> usize {
        self.rows.len().saturating_sub(height)
    }

    /// Moves the viewport the least amount that puts `row` on screen.
    fn reveal(&mut self, row: usize, height: usize) {
        if row < self.top {
            self.top = row;
        } else if row >= self.top.saturating_add(height) {
            // The match sits below the viewport, so it becomes the last visible
            // row and the reader keeps the context above it.
            self.top = row.saturating_sub(height.saturating_sub(1));
        }
        self.top = self.top.min(self.deepest(height));
    }
}

/// Composes the transcript screen for a terminal `cols` by `rows`.
///
/// The header is written first when a search is active and the conversation
/// fills the rest, padded below so the top of the viewport stays at the top of
/// the screen. Composition goes through [`crate::frame::compose`], so this
/// screen is committed, diffed, and verified by replay like any other frame.
pub fn compose(transcript: &Transcript, cols: u16, rows: u16) -> Result<Grid> {
    let body = transcript.body(rows);
    let mut lines: Vec<String> = transcript.visible(body).to_vec();
    lines.resize(body, String::new());
    if let Some(header) = transcript.header() {
        lines.insert(0, header);
    }
    crate::frame::compose(&Regions::new(&lines, &[]), cols, rows)
}

/// Opens the transcript over the main screen.
///
/// Returns the commit that puts it on the terminal, or `None` when the
/// transcript already owned the screen, in which case nothing is written and no
/// second owner is granted. The alternate screen is given up again when the
/// frame cannot be composed or committed, so the machine never owns a screen
/// that was not drawn.
pub fn enter(
    screens: &mut Screens,
    surface: &mut FrameSurface,
    transcript: &Transcript,
    cols: u16,
    rows: u16,
) -> Result<Option<FrameCommit>> {
    if !screens.enter() {
        return Ok(None);
    }
    match compose(transcript, cols, rows).and_then(|target| surface.commit(&target)) {
        Ok(commit) => Ok(Some(commit)),
        Err(err) => {
            let _ = screens.leave();
            Err(err)
        }
    }
}

/// Closes the transcript and puts the main screen back.
///
/// Returns the commit, or `None` when the transcript was not open. The caller
/// keeps the grid it supplied: the surface's shadow still holds the transcript,
/// so the main frame is written again rather than assumed to be there. A write
/// that cannot be verified leaves the transcript owning the screen it is still
/// showing.
pub fn leave(
    screens: &mut Screens,
    surface: &mut FrameSurface,
    main: &Grid,
) -> Result<Option<FrameCommit>> {
    if !screens.leave() {
        return Ok(None);
    }
    match surface.commit(main) {
        Ok(commit) => Ok(Some(commit)),
        Err(err) => {
            let _ = screens.enter();
            Err(err)
        }
    }
}

/// Returns true when a row holds a query, folding ASCII case.
///
/// An empty query matches nothing rather than everything.
fn contains(row: &str, query: &str) -> bool {
    let needle = query.as_bytes();
    if needle.is_empty() {
        return false;
    }
    row.as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::compose as compose_main;
    use crate::width::str_width;

    const COLS: u16 = 30;
    const ROWS: u16 = 6;

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn numbered(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("row {index}")).collect()
    }

    fn texts(rows: &[String]) -> Vec<&str> {
        rows.iter().map(String::as_str).collect()
    }

    /// Returns the inline screen the shell draws, for the restore test.
    fn main_screen(cols: u16, rows: u16) -> Grid {
        let transcript = lines(&["one", "two", "three"]);
        let prompt = lines(&["> "]);
        let footer = lines(&["status"]);
        let regions = Regions::new(&transcript, &footer).with_prompt(&prompt);
        compose_main(&regions, cols, rows).expect("main screen")
    }

    #[test]
    fn an_empty_transcript_shows_nothing_and_stays_put() {
        let mut transcript = Transcript::new(Vec::new());
        assert!(transcript.visible(10).is_empty());
        assert!(transcript.is_empty());

        transcript.scroll_down(4, 10);
        transcript.scroll_up(4);
        transcript.page_down(10);
        transcript.page_up(10);
        transcript.jump(3, 10);
        assert_eq!(transcript.top(), 0, "an empty transcript moved");

        assert!(transcript.search("anything", 10).is_empty());
        transcript.next_match(10);
        transcript.previous_match(10);
        assert_eq!(transcript.selected(), None);
        assert_eq!(
            transcript.header(),
            Some("no matches for \"anything\"".to_owned())
        );
    }

    #[test]
    fn content_shorter_than_the_screen_cannot_scroll() {
        let mut transcript = Transcript::new(lines(&["one", "two"]));
        transcript.scroll_down(10, ROWS.into());
        transcript.page_down(ROWS.into());
        transcript.jump(1, ROWS.into());
        assert_eq!(transcript.top(), 0);
        assert_eq!(texts(transcript.visible(ROWS.into())), ["one", "two"]);
    }

    #[test]
    fn scrolling_stops_at_the_first_and_last_row() {
        let mut transcript = Transcript::new(numbered(12));
        assert_eq!(transcript.top(), 0);

        transcript.scroll_up(1);
        assert_eq!(transcript.top(), 0, "scrolling up wrapped to the end");

        transcript.scroll_down(40, 4);
        assert_eq!(transcript.top(), 8);
        transcript.scroll_down(1, 4);
        assert_eq!(transcript.top(), 8, "scrolling down wrapped to the top");
        assert_eq!(
            texts(transcript.visible(4)),
            ["row 8", "row 9", "row 10", "row 11"]
        );

        transcript.scroll_up(1);
        assert_eq!(
            texts(transcript.visible(4)),
            ["row 7", "row 8", "row 9", "row 10"]
        );
        transcript.scroll_up(100);
        assert_eq!(transcript.top(), 0);
    }

    #[test]
    fn a_page_lands_the_next_screen_at_the_top() {
        let mut transcript = Transcript::new(numbered(12));
        transcript.page_down(4);
        assert_eq!(
            texts(transcript.visible(4)),
            ["row 4", "row 5", "row 6", "row 7"]
        );
        transcript.page_down(4);
        assert_eq!(
            texts(transcript.visible(4)),
            ["row 8", "row 9", "row 10", "row 11"]
        );
        transcript.page_down(4);
        assert_eq!(transcript.top(), 8, "a page past the end wrapped");

        transcript.page_up(4);
        assert_eq!(
            texts(transcript.visible(4)),
            ["row 4", "row 5", "row 6", "row 7"]
        );
        transcript.page_up(8);
        assert_eq!(transcript.top(), 0, "a page before the start wrapped");
    }

    #[test]
    fn a_jump_clamps_to_the_conversation() {
        let mut transcript = Transcript::new(numbered(12));
        transcript.jump(5, 4);
        assert_eq!(texts(transcript.visible(4)).first().copied(), Some("row 5"));
        transcript.jump(99, 4);
        assert_eq!(transcript.top(), 8, "a jump past the end left a gap");
        assert_eq!(transcript.visible(4).len(), 4);
        transcript.jump(0, 4);
        assert_eq!(transcript.top(), 0);
    }

    #[test]
    fn a_search_ignores_ascii_case() {
        let mut transcript = Transcript::new(lines(&["Warning: disk full", "all good"]));
        assert_eq!(transcript.search("warning", 2), [0].as_slice());
        assert_eq!(transcript.search("WARNING", 2), [0].as_slice());
        assert_eq!(transcript.query(), Some("WARNING"));
        assert!(transcript.search("missing", 2).is_empty());
        assert_eq!(transcript.selected(), None);
    }

    #[test]
    fn an_empty_query_clears_the_search() {
        let mut transcript = Transcript::new(lines(&["alpha"]));
        assert_eq!(transcript.search("alpha", 3), [0].as_slice());
        assert_eq!(
            transcript.header(),
            Some("match 1/1 for \"alpha\"".to_owned())
        );

        assert!(transcript.search("", 3).is_empty());
        assert_eq!(transcript.query(), None);
        assert_eq!(transcript.selected(), None);
        assert_eq!(transcript.header(), None);
    }

    #[test]
    fn a_search_starts_at_the_top_row_and_wraps_when_none_is_below() {
        let mut transcript = Transcript::new(lines(&["hit", "a", "b", "hit"]));
        transcript.jump(3, 2);
        assert_eq!(transcript.top(), 2);
        assert_eq!(transcript.search("hit", 2), [0, 3].as_slice());
        assert_eq!(
            transcript.selected(),
            Some(1),
            "the search ignored the row it started from"
        );

        let mut transcript = Transcript::new(lines(&["hit", "a", "b", "c"]));
        transcript.jump(3, 2);
        assert_eq!(transcript.search("hit", 2), [0].as_slice());
        assert_eq!(
            transcript.selected(),
            Some(0),
            "a search with nothing below did not wrap"
        );
        assert_eq!(transcript.top(), 0);
    }

    #[test]
    fn a_search_scrolls_its_match_into_view() {
        let mut transcript = Transcript::new(numbered(40));
        transcript.scroll_down(10, 5);
        transcript.search("row 30", 5);
        assert_eq!(transcript.selected(), Some(0));
        assert_eq!(transcript.top(), 26, "the match was left off screen");
        assert_eq!(
            texts(transcript.visible(5)),
            ["row 26", "row 27", "row 28", "row 29", "row 30"]
        );
    }

    #[test]
    fn next_and_previous_stop_at_the_ends() {
        let mut transcript = Transcript::new(lines(&["alpha", "beta", "alpha", "x", "alpha"]));
        assert_eq!(transcript.search("alpha", 3), [0, 2, 4].as_slice());
        assert_eq!(transcript.selected(), Some(0));

        transcript.next_match(3);
        assert_eq!(transcript.selected(), Some(1));
        transcript.next_match(3);
        assert_eq!(transcript.selected(), Some(2));
        transcript.next_match(3);
        assert_eq!(
            transcript.selected(),
            Some(2),
            "next ran past the last match"
        );

        transcript.previous_match(3);
        assert_eq!(transcript.selected(), Some(1));
        transcript.previous_match(3);
        assert_eq!(transcript.selected(), Some(0));
        transcript.previous_match(3);
        assert_eq!(
            transcript.selected(),
            Some(0),
            "previous ran past the first match"
        );
    }

    #[test]
    fn the_header_counts_the_matches_while_a_search_is_active() {
        let mut transcript = Transcript::new(lines(&["alpha", "beta", "alpha"]));
        assert_eq!(
            transcript.header(),
            None,
            "an unsearched screen has a header"
        );

        transcript.search("alpha", 3);
        assert_eq!(
            transcript.header(),
            Some("match 1/2 for \"alpha\"".to_owned())
        );
        transcript.next_match(3);
        assert_eq!(
            transcript.header(),
            Some("match 2/2 for \"alpha\"".to_owned())
        );
    }

    #[test]
    fn the_header_sits_above_the_conversation_only_while_searching() {
        let transcript = Transcript::new(numbered(20));
        let grid = compose(&transcript, COLS, ROWS).expect("compose");
        assert_eq!(grid.row_text(0), "row 0");
        assert_eq!(grid.row_text(5), "row 5");

        let mut searched = Transcript::new(numbered(20));
        searched.search("row 1", ROWS.into());
        let grid = compose(&searched, COLS, ROWS).expect("compose");
        assert_eq!(grid.row_text(0), "match 1/11 for \"row 1\"");
        assert_eq!(grid.row_text(1), "row 0");
        assert_eq!(grid.row_text(5), "row 4");
    }

    #[test]
    fn a_header_wider_than_the_screen_is_clipped() {
        let mut transcript = Transcript::new(lines(&["alpha"]));
        transcript.search(&"needle ".repeat(8), ROWS.into());
        let grid = compose(&transcript, 12, ROWS).expect("compose");
        for row in 0..ROWS {
            let text = grid.row_text(row);
            assert!(str_width(&text) <= 12, "row {row} is too wide: {text:?}");
        }
    }

    #[test]
    fn a_search_over_content_shorter_than_the_screen_keeps_the_header_on_top() {
        let mut transcript = Transcript::new(lines(&["alpha", "beta"]));
        transcript.search("alpha", ROWS.into());
        let grid = compose(&transcript, COLS, ROWS).expect("compose");
        assert_eq!(grid.row_text(0), "match 1/1 for \"alpha\"");
        assert_eq!(grid.row_text(1), "alpha");
        assert_eq!(grid.row_text(2), "beta");
        for row in 3..ROWS {
            assert_eq!(grid.row_text(row), "", "row {row} is not blank");
        }
    }

    #[test]
    fn a_transcript_with_no_rows_composes_an_empty_screen() {
        let grid = compose(&Transcript::new(Vec::new()), COLS, ROWS).expect("compose");
        for row in 0..ROWS {
            assert_eq!(grid.row_text(row), "", "row {row} is not blank");
        }
    }

    #[test]
    fn a_second_owner_of_the_alternate_screen_is_refused() {
        let mut screens = Screens::new();
        assert_eq!(screens.current(), Screen::Main);
        assert_eq!(screens.owners(), 0);
        assert!(screens.alternate().is_none());

        assert!(screens.enter());
        assert_eq!(screens.current(), Screen::Transcript);
        assert_eq!(screens.owners(), 1);
        assert!(screens.alternate().is_some());

        assert!(
            !screens.enter(),
            "a second surface was granted the alternate screen"
        );
        assert_eq!(screens.owners(), 1);
    }

    #[test]
    fn leaving_a_closed_transcript_changes_nothing() {
        let mut screens = Screens::new();
        assert!(screens.enter());
        assert!(screens.leave());
        assert_eq!(screens.current(), Screen::Main);
        assert_eq!(screens.owners(), 0);

        assert!(
            !screens.leave(),
            "leaving an unopened transcript changed the screen"
        );
        assert_eq!(screens.current(), Screen::Main);
        assert!(screens.enter(), "the machine did not reopen after leaving");
    }

    #[test]
    fn entering_an_open_transcript_writes_nothing() {
        let transcript = Transcript::new(numbered(20));
        let mut screens = Screens::new();
        let mut surface = FrameSurface::new(COLS, ROWS).expect("surface");

        let first = enter(&mut screens, &mut surface, &transcript, COLS, ROWS)
            .expect("enter")
            .expect("the transcript opened");
        assert!(!first.is_empty());

        let again = enter(&mut screens, &mut surface, &transcript, COLS, ROWS).expect("enter");
        assert!(again.is_none(), "a second entry wrote to the terminal");
        assert_eq!(screens.owners(), 1);
        assert_eq!(screens.current(), Screen::Transcript);
    }

    #[test]
    fn closing_a_closed_transcript_writes_nothing() {
        let main = main_screen(COLS, ROWS);
        let mut screens = Screens::new();
        let mut surface = FrameSurface::new(COLS, ROWS).expect("surface");
        assert!(
            leave(&mut screens, &mut surface, &main)
                .expect("leave")
                .is_none()
        );
        assert!(surface.last_bytes().is_empty());
    }

    #[test]
    fn a_recorded_stream_rebuilds_the_transcript_screen() {
        let mut transcript = Transcript::new(numbered(40));
        // A search puts the header on every frame, so the stream has to carry
        // it as well as the rows.
        transcript.search("row 1", ROWS.into());
        let height = transcript.body(ROWS);

        let mut screens = Screens::new();
        let mut surface = FrameSurface::new(COLS, ROWS).expect("surface");
        let mut stream: Vec<u8> = Vec::new();
        let opened = enter(&mut screens, &mut surface, &transcript, COLS, ROWS)
            .expect("enter")
            .expect("the transcript opened");
        stream.extend_from_slice(&opened.bytes);

        let mut frames = 0usize;
        for step in 0..8 {
            match step {
                0 => transcript.scroll_down(1, height),
                1 => transcript.page_down(height),
                2 => transcript.page_up(height),
                3 => transcript.scroll_down(3, height),
                4 => transcript.jump(30, height),
                5 => transcript.next_match(height),
                6 => transcript.previous_match(height),
                _ => transcript.jump(0, height),
            }
            let target = compose(&transcript, COLS, ROWS).expect("compose");
            let commit = surface.commit(&target).expect("commit");
            stream.extend_from_slice(&commit.bytes);
            if !commit.is_empty() {
                frames = frames.saturating_add(1);
            }

            let mut cleared = Grid::new(COLS, ROWS).expect("cleared");
            cleared.feed(&stream).expect("feed");
            assert_eq!(cleared, target, "step {step} did not rebuild its screen");
        }
        assert!(frames > 0, "no frame was committed");
    }

    #[test]
    fn leaving_the_transcript_restores_the_main_screen_byte_for_byte() {
        let main = main_screen(80, 24);
        let mut screens = Screens::new();
        let mut surface = FrameSurface::new(80, 24).expect("surface");
        let mut stream: Vec<u8> = Vec::new();
        stream.extend_from_slice(&surface.commit(&main).expect("commit").bytes);

        let mut transcript = Transcript::new(numbered(60));
        transcript.jump(40, 16);
        let opened = enter(&mut screens, &mut surface, &transcript, 80, 24)
            .expect("enter")
            .expect("the transcript opened");
        assert!(!opened.is_empty(), "the transcript drew nothing");
        stream.extend_from_slice(&opened.bytes);

        let closed = leave(&mut screens, &mut surface, &main)
            .expect("leave")
            .expect("the transcript closed");
        stream.extend_from_slice(&closed.bytes);

        let mut cleared = Grid::new(80, 24).expect("cleared");
        cleared.feed(&stream).expect("feed");
        assert_eq!(
            cleared.checkpoint(),
            main.checkpoint(),
            "the round trip changed the main screen"
        );
        assert!(
            surface.commit(&main).expect("commit").is_empty(),
            "the surface still held the transcript"
        );
    }
}
