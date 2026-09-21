//! A bounded terminal grid and the escape subset Rune emits.
//!
//! Rendering is a pure function of a byte stream: feed bytes, read a grid. The
//! grid never retains scrollback and never grows a collection past a documented
//! cap, so a hostile or corrupt stream can at worst fill the screen, and every
//! bound it can cross is reported as a typed error rather than absorbed.
//!
//! Placement is decided at codepoint granularity. A cell holds one base glyph
//! plus one combining mark, which covers what a terminal can actually store;
//! decisions that depend on a whole grapheme cluster, such as whether a joined
//! emoji fits in the remaining columns, are made by [`crate::width`] before
//! bytes are emitted.

use std::collections::HashMap;

use rune_core::error::{ErrorCode, Result, RuneError};

use crate::width;

/// Most parameters accepted in one CSI sequence.
pub const MAX_CSI_PARAMS: usize = 16;
/// Most intermediate bytes accepted in one CSI sequence.
pub const MAX_CSI_INTERMEDIATES: usize = 2;
/// Most bytes retained from one OSC payload.
pub const MAX_OSC_BYTES: usize = 4096;
/// Most distinct hyperlink targets one screen retains.
pub const MAX_HYPERLINKS: usize = 1024;
/// Most cells a grid allocates, about 43 screens at 200 by 120.
pub const MAX_CELLS: usize = 1 << 20;

/// Columns a horizontal tab advances to.
const TAB_WIDTH: u16 = 8;
/// Bytes one cell occupies in a checkpoint.
const CELL_RECORD: usize = 11;
/// Checkpoint container version. A reader accepts only the version it wrote.
const CHECKPOINT_VERSION: u8 = 1;
/// Magic prefix, so a truncated or misdirected buffer fails on the first byte.
const CHECKPOINT_MAGIC: &[u8; 4] = b"RTG1";
/// Marker for an absent hyperlink in a checkpoint.
const NO_LINK: u16 = u16::MAX;

/// A terminal color.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, Debug, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Color {
    /// The terminal's configured default.
    #[default]
    Default,
    /// A palette entry, 0 through 255.
    Indexed(u8),
    /// A direct color.
    Rgb(u8, u8, u8),
}

/// Attribute bits packed into [`Style::flags`].
pub mod flag {
    /// Bold or increased intensity.
    pub const BOLD: u16 = 1 << 0;
    /// Faint or decreased intensity.
    pub const DIM: u16 = 1 << 1;
    /// Italic.
    pub const ITALIC: u16 = 1 << 2;
    /// Underline.
    pub const UNDERLINE: u16 = 1 << 3;
    /// Reverse video.
    pub const REVERSE: u16 = 1 << 4;
    /// Struck through.
    pub const STRIKE: u16 = 1 << 5;
    /// Every bit a decoder accepts.
    pub const ALL: u16 = BOLD | DIM | ITALIC | UNDERLINE | REVERSE | STRIKE;
}

/// The attributes one cell was written with.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, Debug, Default, serde::Serialize, serde::Deserialize,
)]
pub struct Style {
    /// Foreground color.
    pub fg: Color,
    /// Background color.
    pub bg: Color,
    /// Attribute bitfield, see [`flag`].
    pub flags: u16,
    /// Interned hyperlink target, resolved through the owning grid.
    pub hyperlink: Option<u16>,
}

impl Style {
    /// The canonical sequence that clears every attribute.
    pub const RESET: &'static str = "\u{1b}[0m";
    /// Closes an open hyperlink.
    pub const HYPERLINK_CLOSE: &'static str = "\u{1b}]8;;\u{1b}\\";

    /// Returns the default style.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            flags: 0,
            hyperlink: None,
        }
    }

    /// Returns true when an attribute other than the hyperlink is set.
    #[must_use]
    pub const fn has_sgr(&self) -> bool {
        !matches!(self.fg, Color::Default) || !matches!(self.bg, Color::Default) || self.flags != 0
    }

    /// Sets one attribute bit.
    pub const fn set_flag(&mut self, bits: u16) {
        self.flags |= bits;
    }

    /// Clears one attribute bit.
    pub const fn clear_flag(&mut self, bits: u16) {
        self.flags &= !bits;
    }

    /// Returns true when every bit in `bits` is set.
    #[must_use]
    pub const fn has_flag(&self, bits: u16) -> bool {
        self.flags & bits == bits
    }

    /// Returns the canonical SGR sequence for this style, empty for default.
    ///
    /// Absolute rather than relative, so replaying it at the top of a line
    /// reproduces the same attributes without knowing what came before.
    #[must_use]
    pub fn sgr(&self) -> String {
        let mut params: Vec<String> = Vec::new();
        for (bit, code) in [
            (flag::BOLD, "1"),
            (flag::DIM, "2"),
            (flag::ITALIC, "3"),
            (flag::UNDERLINE, "4"),
            (flag::REVERSE, "7"),
            (flag::STRIKE, "9"),
        ] {
            if self.has_flag(bit) {
                params.push(code.to_owned());
            }
        }
        match self.fg {
            Color::Default => {}
            Color::Indexed(index) => params.push(format!("38;5;{index}")),
            Color::Rgb(r, g, b) => params.push(format!("38;2;{r};{g};{b}")),
        }
        match self.bg {
            Color::Default => {}
            Color::Indexed(index) => params.push(format!("48;5;{index}")),
            Color::Rgb(r, g, b) => params.push(format!("48;2;{r};{g};{b}")),
        }
        if params.is_empty() {
            return String::new();
        }
        let mut out = String::from("\u{1b}[");
        out.push_str(&params.join(";"));
        out.push('m');
        out
    }

    /// Applies an SGR parameter string.
    pub fn apply_sgr(&mut self, params: &str) {
        let parsed: Vec<u16> = params
            .split([';', ':'])
            .map(|token| token.trim().parse::<u16>().unwrap_or(0))
            .collect();
        self.apply_sgr_params(&parsed);
    }

    /// Applies SGR parameters.
    ///
    /// An unrecognized parameter is ignored rather than rejected, so a stream
    /// using an attribute this build does not know still renders the ones it
    /// does.
    pub fn apply_sgr_params(&mut self, params: &[u16]) {
        if params.is_empty() {
            self.reset_sgr();
            return;
        }
        let mut index = 0usize;
        while index < params.len() {
            let code = params[index];
            match code {
                0 => self.reset_sgr(),
                1 => self.set_flag(flag::BOLD),
                2 => self.set_flag(flag::DIM),
                3 => self.set_flag(flag::ITALIC),
                4 => self.set_flag(flag::UNDERLINE),
                7 => self.set_flag(flag::REVERSE),
                9 => self.set_flag(flag::STRIKE),
                22 => self.clear_flag(flag::BOLD | flag::DIM),
                23 => self.clear_flag(flag::ITALIC),
                24 => self.clear_flag(flag::UNDERLINE),
                25 | 27 => self.clear_flag(flag::REVERSE),
                // 21 is doubly underlined, which has no cell flag here.
                29 => self.clear_flag(flag::STRIKE),
                30..=37 => self.fg = Color::Indexed(indexed(code.saturating_sub(30))),
                39 => self.fg = Color::Default,
                40..=47 => self.bg = Color::Indexed(indexed(code.saturating_sub(40))),
                49 => self.bg = Color::Default,
                90..=97 => {
                    self.fg = Color::Indexed(indexed(code.saturating_sub(90).saturating_add(8)));
                }
                100..=107 => {
                    self.bg = Color::Indexed(indexed(code.saturating_sub(100).saturating_add(8)));
                }
                38 | 48 => {
                    let (color, consumed) = extended_color(params, index.saturating_add(1));
                    match color {
                        Some(Color::Default) | None => {}
                        Some(color) if code == 38 => self.fg = color,
                        Some(color) => self.bg = color,
                    }
                    index = index.saturating_add(consumed);
                }
                _ => {}
            }
            index = index.saturating_add(1);
        }
    }

    /// Clears the SGR attributes, keeping any open hyperlink.
    ///
    /// A hyperlink is opened and closed by its own sequence, so an SGR reset
    /// must not silently drop it.
    pub const fn reset_sgr(&mut self) {
        let hyperlink = self.hyperlink;
        self.fg = Color::Default;
        self.bg = Color::Default;
        self.flags = 0;
        self.hyperlink = hyperlink;
    }

    /// Clears the open hyperlink.
    pub const fn clear_hyperlink(&mut self) {
        self.hyperlink = None;
    }
}

/// Returns an SGR color code as a palette index.
///
/// The code is at most 107 at every call site, so the narrowing cannot lose a
/// value; the fallback exists so the function is total.
fn indexed(code: u16) -> u8 {
    u8::try_from(code).unwrap_or(u8::MAX)
}

/// Reads an extended color after a `38` or `48` introducer.
///
/// Returns the color and how many extra parameters it consumed, so a truncated
/// tail cannot make the caller read the next attribute as part of the color.
fn extended_color(params: &[u16], start: usize) -> (Option<Color>, usize) {
    match params.get(start).copied() {
        Some(5) => match params.get(start.saturating_add(1)).copied() {
            Some(index) => (
                Some(Color::Indexed(u8::try_from(index).unwrap_or(u8::MAX))),
                2,
            ),
            None => (None, params.len().saturating_sub(start)),
        },
        Some(2) => match params.get(start.saturating_add(1)..start.saturating_add(4)) {
            Some([r, g, b]) => (
                Some(Color::Rgb(
                    u8::try_from(*r).unwrap_or(u8::MAX),
                    u8::try_from(*g).unwrap_or(u8::MAX),
                    u8::try_from(*b).unwrap_or(u8::MAX),
                )),
                4,
            ),
            _ => (None, params.len().saturating_sub(start)),
        },
        Some(_) | None => (None, 0),
    }
}

/// One grid cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Cell {
    /// The base codepoint. A blank cell holds a space.
    pub codepoint: char,
    /// Columns the cell occupies: 1, 2 for a wide glyph, 0 for the trailing
    /// column of a wide glyph.
    pub width: u8,
    /// The attributes the cell was written with.
    pub style: Style,
    /// A combining mark attached to the base codepoint.
    ///
    /// One slot covers the clusters a terminal receives in practice, and keeps
    /// a cell small enough to copy without allocating.
    pub mark: Option<char>,
}

impl Cell {
    /// A blank cell, which is what an untouched position holds.
    pub const BLANK: Self = Self {
        codepoint: ' ',
        width: 1,
        style: Style::new(),
        mark: None,
    };

    /// Returns a blank cell carrying a style.
    #[must_use]
    pub const fn blank(style: Style) -> Self {
        Self {
            codepoint: ' ',
            width: 1,
            style,
            mark: None,
        }
    }

    /// Returns the trailing column of a wide glyph.
    #[must_use]
    pub const fn continuation(style: Style) -> Self {
        Self {
            codepoint: ' ',
            width: 0,
            style,
            mark: None,
        }
    }

    /// Returns true when the cell is the trailing column of a wide glyph.
    #[must_use]
    pub const fn is_continuation(&self) -> bool {
        self.width == 0
    }

    /// Appends the cell's glyph to a string.
    pub fn write_to(&self, out: &mut String) {
        if self.is_continuation() {
            return;
        }
        out.push(self.codepoint);
        if let Some(mark) = self.mark {
            out.push(mark);
        }
    }
}

/// Where the cursor is and how the next glyph will be placed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Cursor {
    /// Row, zero based.
    pub row: u16,
    /// Column, zero based.
    pub col: u16,
    /// True when the next glyph wraps to the following row.
    pub pending_wrap: bool,
}

/// Grid dimensions.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Bounds {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

/// One run of changed columns on one row.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct DiffSpan {
    /// Row the run sits on.
    pub row: u16,
    /// First changed column.
    pub start: u16,
    /// One past the last changed column.
    pub end: u16,
}

impl DiffSpan {
    /// Number of columns in the run.
    #[must_use]
    pub const fn len(&self) -> u16 {
        self.end.saturating_sub(self.start)
    }

    /// Returns true when the run holds no columns.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// What one call to [`Grid::feed`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct FeedStats {
    /// Highest row index written to.
    pub max_row_touched: u16,
    /// True when the grid scrolled at least once.
    pub scrolled: bool,
    /// Rows scrolled off the top.
    pub scroll_rows: u32,
    /// Bytes consumed, which on success is the whole slice.
    pub bytes_consumed: usize,
}

/// Interned hyperlink targets.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct LinkPool {
    targets: Vec<Option<String>>,
    index: HashMap<String, u16>,
}

impl LinkPool {
    /// Interns a target, reusing a slot when it is already retained.
    fn intern(&mut self, target: &str) -> Result<u16> {
        if let Some(id) = self.index.get(target) {
            return Ok(*id);
        }
        if self.index.len() >= MAX_HYPERLINKS {
            return Err(limit(
                "hyperlink_targets",
                self.index.len().saturating_add(1),
                MAX_HYPERLINKS,
                "links <= MAX_HYPERLINKS",
            )
            .with_hint("close hyperlinks before opening more"));
        }
        let owned = target.to_owned();
        let id = u16::try_from(self.targets.len()).map_err(|_| {
            RuneError::new(ErrorCode::Internal, "hyperlink slots exceeded a u16")
                .with_invariant("slots <= u16::MAX")
        })?;
        self.targets.push(Some(owned.clone()));
        self.index.insert(owned, id);
        Ok(id)
    }

    /// Returns the target in a slot.
    fn get(&self, id: u16) -> Option<&str> {
        self.targets
            .get(usize::from(id))
            .and_then(|slot| slot.as_deref())
    }

    /// Number of distinct retained targets.
    fn len(&self) -> usize {
        self.index.len()
    }
}

/// The parser state that survives between calls to [`Grid::feed`].
///
/// A stream arrives in arbitrary chunks, so a sequence split across two reads
/// must continue rather than restart.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
enum State {
    /// Not inside a sequence.
    #[default]
    Ground,
    /// After an escape, before the sequence kind is known.
    Escape(EscSeq),
    /// Inside a control sequence.
    Csi(CsiSeq),
    /// Inside an operating system command.
    Osc(OscSeq),
}

/// A control sequence under construction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct CsiSeq {
    params: [u16; MAX_CSI_PARAMS],
    count: u8,
    current: u16,
    has_current: bool,
    intermediates: [u8; MAX_CSI_INTERMEDIATES],
    intermediate_count: u8,
}

impl CsiSeq {
    /// Returns the completed parameters.
    fn params(&self) -> &[u16] {
        let count = usize::from(self.count).min(MAX_CSI_PARAMS);
        self.params.get(..count).unwrap_or(&[])
    }

    /// Returns true when the sequence carries a private parameter marker.
    fn is_private(&self) -> bool {
        self.intermediates.first() == Some(&b'?')
    }

    /// Appends one parameter, failing when the cap is already reached.
    fn push_param(&mut self, value: u16) -> Result<()> {
        let slot = usize::from(self.count);
        if slot >= MAX_CSI_PARAMS {
            return Err(limit(
                "csi_params",
                slot.saturating_add(1),
                MAX_CSI_PARAMS,
                "params <= MAX_CSI_PARAMS",
            ));
        }
        if let Some(entry) = self.params.get_mut(slot) {
            *entry = value;
        }
        self.count = self.count.saturating_add(1);
        self.current = 0;
        self.has_current = false;
        Ok(())
    }

    /// Records one intermediate byte, failing when the cap is reached.
    fn push_intermediate(&mut self, byte: u8) -> Result<()> {
        let slot = usize::from(self.intermediate_count);
        if slot >= MAX_CSI_INTERMEDIATES {
            return Err(limit(
                "csi_intermediates",
                slot.saturating_add(1),
                MAX_CSI_INTERMEDIATES,
                "intermediates <= MAX_CSI_INTERMEDIATES",
            ));
        }
        if let Some(entry) = self.intermediates.get_mut(slot) {
            *entry = byte;
        }
        self.intermediate_count = self.intermediate_count.saturating_add(1);
        Ok(())
    }
}

/// A non-CSI escape sequence under construction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct EscSeq {
    intermediates: [u8; MAX_CSI_INTERMEDIATES],
    intermediate_count: u8,
}

/// An operating system command under construction.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct OscSeq {
    data: Vec<u8>,
    /// True after an escape inside the payload, waiting for the terminator.
    escaped: bool,
}

/// A partially received UTF-8 character.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Utf8Pending {
    bytes: [u8; 4],
    len: u8,
    want: u8,
}

impl Utf8Pending {
    /// Adds one byte, returning a character once one is complete.
    fn push(&mut self, byte: u8) -> Option<char> {
        if self.len == 0 {
            if byte < 0x80 {
                return Some(char::from(byte));
            }
            self.want = match byte {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                // A stray continuation byte or an impossible lead is dropped
                // rather than decoded as a replacement glyph, which would put
                // a character on screen that the stream never sent.
                _ => return None,
            };
        } else if byte & 0xc0 != 0x80 {
            *self = Self::default();
            return self.push(byte);
        }
        if let Some(slot) = self.bytes.get_mut(usize::from(self.len)) {
            *slot = byte;
        }
        self.len = self.len.saturating_add(1);
        if self.len < self.want {
            return None;
        }
        let decoded = std::str::from_utf8(self.bytes.get(..usize::from(self.len))?)
            .ok()
            .and_then(|text| text.chars().next());
        *self = Self::default();
        decoded
    }

    /// Drops any partially received character.
    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// A terminal screen of fixed size.
#[derive(Clone, Debug)]
pub struct Grid {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
    cursor: Cursor,
    style: Style,
    saved: Option<(Cursor, Style)>,
    links: LinkPool,
    autowrap: bool,
    state: State,
    utf8: Utf8Pending,
    touched: u16,
    scrolled: bool,
    scroll_rows: u32,
}

/// Compares rendered state only.
///
/// Parser scratch that has no effect on what is on screen, such as a partially
/// received escape, is not part of grid identity.
impl PartialEq for Grid {
    fn eq(&self, other: &Self) -> bool {
        self.cols == other.cols
            && self.rows == other.rows
            && self.cells == other.cells
            && self.cursor == other.cursor
            && self.style == other.style
            && self.saved == other.saved
            && self.autowrap == other.autowrap
            && self.links == other.links
    }
}

impl Eq for Grid {}

impl Grid {
    /// Returns a grid of blank cells.
    ///
    /// Fails on a zero dimension, which would make the cursor position
    /// meaningless, and on a size past [`MAX_CELLS`].
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        if cols == 0 || rows == 0 {
            return Err(RuneError::invalid_field(
                "bounds",
                format!("a grid needs at least one column and one row, got {cols}x{rows}"),
            ));
        }
        let cells = cell_count(cols, rows);
        if cells > MAX_CELLS {
            return Err(RuneError::too_large("bounds", cells, MAX_CELLS)
                .with_invariant("cells <= MAX_CELLS"));
        }
        Ok(Self {
            cols,
            rows,
            cells: vec![Cell::BLANK; cells],
            cursor: Cursor::default(),
            style: Style::new(),
            saved: None,
            links: LinkPool::default(),
            autowrap: true,
            state: State::Ground,
            utf8: Utf8Pending::default(),
            touched: 0,
            scrolled: false,
            scroll_rows: 0,
        })
    }

    /// Returns the grid dimensions.
    #[must_use]
    pub const fn bounds(&self) -> Bounds {
        Bounds {
            cols: self.cols,
            rows: self.rows,
        }
    }

    /// Returns the cursor.
    #[must_use]
    pub const fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Returns the style new cells are written with.
    #[must_use]
    pub const fn style(&self) -> Style {
        self.style
    }

    /// Returns true when autowrap is enabled.
    #[must_use]
    pub const fn autowrap(&self) -> bool {
        self.autowrap
    }

    /// Returns the number of distinct hyperlink targets retained.
    #[must_use]
    pub fn hyperlink_count(&self) -> usize {
        self.links.len()
    }

    /// Resolves an interned hyperlink.
    #[must_use]
    pub fn hyperlink(&self, id: Option<u16>) -> Option<&str> {
        id.and_then(|id| self.links.get(id))
    }

    /// Resolves the hyperlink open at a cell.
    #[must_use]
    pub fn cell_hyperlink(&self, row: u16, col: u16) -> Option<&str> {
        self.cell(row, col)
            .and_then(|cell| self.hyperlink(cell.style.hyperlink))
    }

    /// Returns a cell, or `None` when the position is outside the grid.
    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.cells.get(self.index(row, col))
    }

    /// Returns one row as text, with trailing blanks removed.
    #[must_use]
    pub fn row_text(&self, row: u16) -> String {
        let mut out = String::new();
        if row >= self.rows {
            return out;
        }
        for col in 0..self.cols {
            if let Some(cell) = self.cells.get(self.index(row, col)) {
                cell.write_to(&mut out);
            }
        }
        let trimmed = out.trim_end_matches(' ').len();
        out.truncate(trimmed);
        out
    }

    /// Returns the whole screen as text, one line per row.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for row in 0..self.rows {
            if row > 0 {
                out.push('\n');
            }
            out.push_str(&self.row_text(row));
        }
        out
    }

    /// Returns the runs of columns that differ from another grid.
    ///
    /// One span per contiguous changed run, so a caller rewrites only what
    /// changed instead of repainting the screen.
    #[must_use]
    pub fn diff(&self, other: &Grid) -> Vec<DiffSpan> {
        let rows = self.rows.max(other.rows);
        let cols = self.cols.max(other.cols);
        let mut spans = Vec::new();
        for row in 0..rows {
            let mut start: Option<u16> = None;
            for col in 0..cols {
                if self.cell(row, col) == other.cell(row, col) {
                    if let Some(from) = start.take() {
                        spans.push(DiffSpan {
                            row,
                            start: from,
                            end: col,
                        });
                    }
                } else if start.is_none() {
                    start = Some(col);
                }
            }
            if let Some(from) = start {
                spans.push(DiffSpan {
                    row,
                    start: from,
                    end: cols,
                });
            }
        }
        spans
    }

    /// Resizes the grid, keeping the content that still fits and the cursor.
    ///
    /// A wide glyph that loses either of its columns becomes a blank, so the
    /// result never holds half a glyph.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        if cols == self.cols && rows == self.rows {
            return Ok(());
        }
        let mut next = Self::new(cols, rows)?;
        next.style = self.style;
        next.autowrap = self.autowrap;
        next.links = self.links.clone();
        next.saved = self.saved;
        let copy_rows = rows.min(self.rows);
        let copy_cols = cols.min(self.cols);
        for row in 0..copy_rows {
            for col in 0..copy_cols {
                let Some(cell) = self.cells.get(self.index(row, col)).copied() else {
                    continue;
                };
                let base_col = col.saturating_sub(1);
                let value = if cell.is_continuation() {
                    // The trailing column survives only when its base does.
                    match self
                        .cells
                        .get(self.index(row, base_col))
                        .filter(|_| base_col < copy_cols)
                    {
                        Some(_) => cell,
                        None => Cell::blank(cell.style),
                    }
                } else if u16::from(cell.width) > copy_cols.saturating_sub(col) {
                    Cell::blank(cell.style)
                } else {
                    cell
                };
                let index = next.index(row, col);
                if let Some(slot) = next.cells.get_mut(index) {
                    *slot = value;
                }
            }
        }
        next.cursor = Cursor {
            row: self.cursor.row.min(rows.saturating_sub(1)),
            col: self.cursor.col.min(cols.saturating_sub(1)),
            pending_wrap: self.cursor.pending_wrap && cols >= self.cols,
        };
        next.touched = next.cursor.row;
        *self = next;
        Ok(())
    }

    /// Clears the screen and every attribute.
    pub fn reset(&mut self) {
        self.cells.fill(Cell::BLANK);
        self.cursor = Cursor::default();
        self.style = Style::new();
        self.saved = None;
        self.links = LinkPool::default();
        self.state = State::Ground;
        self.utf8.clear();
        self.touched = 0;
    }

    /// Feeds bytes into the grid.
    ///
    /// The parser carries its state across calls, so a sequence split across
    /// two reads continues where it left off. On failure the grid holds what
    /// the accepted prefix produced and the parser returns to ground.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<FeedStats> {
        self.touched = self.cursor.row;
        self.scrolled = false;
        self.scroll_rows = 0;
        for byte in bytes {
            if let Err(err) = self.step(*byte) {
                self.state = State::Ground;
                self.utf8.clear();
                return Err(err);
            }
        }
        Ok(FeedStats {
            max_row_touched: self.touched,
            scrolled: self.scrolled,
            scroll_rows: self.scroll_rows,
            bytes_consumed: bytes.len(),
        })
    }

    /// Advances the parser by one byte.
    fn step(&mut self, byte: u8) -> Result<()> {
        match std::mem::take(&mut self.state) {
            State::Ground => self.step_ground(byte),
            State::Escape(seq) => self.step_escape(byte, seq),
            State::Csi(seq) => self.step_csi(byte, seq),
            State::Osc(seq) => self.step_osc(byte, seq),
        }
    }

    /// Handles a byte in ground state.
    fn step_ground(&mut self, byte: u8) -> Result<()> {
        match byte {
            0x1b => {
                self.utf8.clear();
                self.state = State::Escape(EscSeq::default());
            }
            0x0a => self.linefeed(),
            0x0d => {
                self.cursor.col = 0;
                self.cursor.pending_wrap = false;
            }
            0x08 => {
                self.cursor.col = self.cursor.col.saturating_sub(1);
                self.cursor.pending_wrap = false;
            }
            0x09 => self.tab(),
            0x07 | 0x7f => {}
            0x20.. => {
                if let Some(ch) = self.utf8.push(byte) {
                    self.print(ch);
                }
            }
            // Every other C0 control carries no meaning we render.
            _ => {}
        }
        Ok(())
    }

    /// Handles a byte after an escape.
    fn step_escape(&mut self, byte: u8, mut seq: EscSeq) -> Result<()> {
        match byte {
            b'[' => self.state = State::Csi(CsiSeq::default()),
            b']' => self.state = State::Osc(OscSeq::default()),
            0x1b => self.state = State::Escape(EscSeq::default()),
            0x20..=0x2f => {
                let slot = usize::from(seq.intermediate_count);
                if slot >= MAX_CSI_INTERMEDIATES {
                    return Err(limit(
                        "escape_intermediates",
                        slot.saturating_add(1),
                        MAX_CSI_INTERMEDIATES,
                        "intermediates <= MAX_CSI_INTERMEDIATES",
                    ));
                }
                if let Some(entry) = seq.intermediates.get_mut(slot) {
                    *entry = byte;
                }
                seq.intermediate_count = seq.intermediate_count.saturating_add(1);
                self.state = State::Escape(seq);
            }
            0x30..=0x7e => self.escape_action(byte),
            // A C0 control inside a sequence is executed and the sequence
            // continues, which is what a terminal does.
            _ => self.state = State::Escape(seq),
        }
        Ok(())
    }

    /// Executes a complete non-CSI escape sequence.
    fn escape_action(&mut self, final_byte: u8) {
        match final_byte {
            b'7' => self.saved = Some((self.cursor, self.style)),
            b'8' => {
                if let Some((cursor, style)) = self.saved {
                    self.cursor = Cursor {
                        row: cursor.row.min(self.rows.saturating_sub(1)),
                        col: cursor.col.min(self.cols.saturating_sub(1)),
                        pending_wrap: false,
                    };
                    self.style = style;
                }
            }
            b'c' => self.reset(),
            _ => {}
        }
    }

    /// Handles a byte inside a control sequence.
    fn step_csi(&mut self, byte: u8, mut seq: CsiSeq) -> Result<()> {
        match byte {
            0x1b => self.state = State::Escape(EscSeq::default()),
            b'0'..=b'9' => {
                seq.has_current = true;
                seq.current = seq
                    .current
                    .saturating_mul(10)
                    .saturating_add(u16::from(byte.saturating_sub(b'0')));
                self.state = State::Csi(seq);
            }
            b';' | b':' => {
                let value = if seq.has_current { seq.current } else { 0 };
                seq.push_param(value)?;
                self.state = State::Csi(seq);
            }
            0x3c..=0x3f | 0x20..=0x2f => {
                seq.push_intermediate(byte)?;
                self.state = State::Csi(seq);
            }
            0x40..=0x7e => {
                if seq.has_current || seq.count > 0 {
                    let value = if seq.has_current { seq.current } else { 0 };
                    seq.push_param(value)?;
                }
                self.execute(&seq, byte);
            }
            _ => self.state = State::Csi(seq),
        }
        Ok(())
    }

    /// Handles a byte inside an operating system command.
    fn step_osc(&mut self, byte: u8, mut seq: OscSeq) -> Result<()> {
        if seq.escaped {
            if byte == b'\\' {
                return self.osc_action(&seq.data);
            }
            // An escape inside a payload that is not the terminator ends the
            // command, and the byte starts a fresh sequence.
            self.step(0x1b)?;
            return self.step(byte);
        }
        match byte {
            0x07 => self.osc_action(&seq.data),
            0x1b => {
                seq.escaped = true;
                self.state = State::Osc(seq);
                Ok(())
            }
            _ => {
                if seq.data.len() >= MAX_OSC_BYTES {
                    return Err(RuneError::too_large(
                        "osc_bytes",
                        seq.data.len().saturating_add(1),
                        MAX_OSC_BYTES,
                    )
                    .with_invariant("osc payload <= MAX_OSC_BYTES"));
                }
                seq.data.push(byte);
                self.state = State::Osc(seq);
                Ok(())
            }
        }
    }

    /// Executes a complete operating system command.
    fn osc_action(&mut self, data: &[u8]) -> Result<()> {
        let Ok(text) = std::str::from_utf8(data) else {
            return Ok(());
        };
        let Some(rest) = text.strip_prefix("8;") else {
            return Ok(());
        };
        let target = rest.split_once(';').map_or("", |(_, target)| target);
        if target.is_empty() {
            self.style.clear_hyperlink();
            return Ok(());
        }
        let id = self.links.intern(target)?;
        self.style.hyperlink = Some(id);
        Ok(())
    }

    /// Executes a complete control sequence.
    fn execute(&mut self, seq: &CsiSeq, final_byte: u8) {
        let params = seq.params();
        let first = params.first().copied().unwrap_or(0);
        match final_byte {
            b'A' => {
                self.cursor.row = self.cursor.row.saturating_sub(param_or(first, 1));
                self.cursor.pending_wrap = false;
            }
            b'B' => {
                self.cursor.row = clamp(self.cursor.row, param_or(first, 1), self.rows);
                self.cursor.pending_wrap = false;
            }
            b'C' => {
                self.cursor.col = clamp(self.cursor.col, param_or(first, 1), self.cols);
                self.cursor.pending_wrap = false;
            }
            b'D' => {
                self.cursor.col = self.cursor.col.saturating_sub(param_or(first, 1));
                self.cursor.pending_wrap = false;
            }
            b'E' => {
                self.cursor.row = clamp(self.cursor.row, param_or(first, 1), self.rows);
                self.cursor.col = 0;
                self.cursor.pending_wrap = false;
            }
            b'F' => {
                self.cursor.row = self.cursor.row.saturating_sub(param_or(first, 1));
                self.cursor.col = 0;
                self.cursor.pending_wrap = false;
            }
            b'G' => {
                self.cursor.col = param_or(first, 1)
                    .saturating_sub(1)
                    .min(self.cols.saturating_sub(1));
                self.cursor.pending_wrap = false;
            }
            b'd' => {
                self.cursor.row = param_or(first, 1)
                    .saturating_sub(1)
                    .min(self.rows.saturating_sub(1));
                self.cursor.pending_wrap = false;
            }
            b'H' | b'f' => {
                self.cursor.row = param_or(first, 1)
                    .saturating_sub(1)
                    .min(self.rows.saturating_sub(1));
                self.cursor.col = param_or(params.get(1).copied().unwrap_or(0), 1)
                    .saturating_sub(1)
                    .min(self.cols.saturating_sub(1));
                self.cursor.pending_wrap = false;
            }
            b'J' => self.erase_in_display(first),
            b'K' => self.erase_in_line(first),
            b'm' => {
                if !seq.is_private() {
                    self.style.apply_sgr_params(params);
                }
            }
            b'h' | b'l' if seq.is_private() && params.contains(&7) => {
                self.autowrap = final_byte == b'h';
            }
            _ => {}
        }
    }

    /// Erases all or part of the screen.
    fn erase_in_display(&mut self, mode: u16) {
        match mode {
            1 => {
                for row in 0..self.cursor.row {
                    self.clear_row(row);
                }
                for col in 0..=self.cursor.col {
                    self.clear_cell(self.cursor.row, col);
                }
            }
            2 | 3 => {
                for row in 0..self.rows {
                    self.clear_row(row);
                }
            }
            _ => {
                for col in self.cursor.col..self.cols {
                    self.clear_cell(self.cursor.row, col);
                }
                for row in self.cursor.row.saturating_add(1)..self.rows {
                    self.clear_row(row);
                }
            }
        }
        self.touched = self.touched.max(self.cursor.row);
    }

    /// Erases all or part of one row.
    fn erase_in_line(&mut self, mode: u16) {
        let row = self.cursor.row;
        match mode {
            1 => {
                for col in 0..=self.cursor.col {
                    self.clear_cell(row, col);
                }
            }
            2 => self.clear_row(row),
            _ => {
                for col in self.cursor.col..self.cols {
                    self.clear_cell(row, col);
                }
            }
        }
        self.touched = self.touched.max(row);
    }

    /// Blanks one cell.
    fn clear_cell(&mut self, row: u16, col: u16) {
        if col >= self.cols || row >= self.rows {
            return;
        }
        let index = self.index(row, col);
        let style = self.style;
        if let Some(slot) = self.cells.get_mut(index) {
            *slot = Cell::blank(style);
        }
    }

    /// Blanks one row.
    fn clear_row(&mut self, row: u16) {
        for col in 0..self.cols {
            self.clear_cell(row, col);
        }
    }

    /// Advances to the next column stop.
    fn tab(&mut self) {
        self.cursor.pending_wrap = false;
        let target = self.cursor.col.saturating_add(TAB_WIDTH);
        let aligned = target.saturating_div(TAB_WIDTH).saturating_mul(TAB_WIDTH);
        self.cursor.col = aligned.min(self.cols.saturating_sub(1));
    }

    /// Advances to the next line, scrolling when already on the last one.
    fn linefeed(&mut self) {
        self.cursor.pending_wrap = false;
        if self.cursor.row.saturating_add(1) < self.rows {
            self.cursor.row = self.cursor.row.saturating_add(1);
            self.touched = self.touched.max(self.cursor.row);
        } else {
            self.scroll_up(1);
        }
    }

    /// Scrolls the screen up, discarding the top rows.
    fn scroll_up(&mut self, rows: u16) {
        let rows = rows.max(1).min(self.rows);
        let shift = usize::from(rows).saturating_mul(usize::from(self.cols));
        if shift >= self.cells.len() {
            self.cells.fill(Cell::BLANK);
        } else {
            self.cells.copy_within(shift.., 0);
            let split = self.cells.len().saturating_sub(shift);
            self.cells[split..].fill(Cell::BLANK);
        }
        self.scrolled = true;
        self.scroll_rows = self.scroll_rows.saturating_add(u32::from(rows));
        self.touched = self.rows.saturating_sub(1);
    }

    /// Writes one character at the cursor.
    fn print(&mut self, ch: char) {
        let glyph_width = width::char_width(ch);
        if glyph_width == 0 {
            self.attach_mark(ch);
            return;
        }
        let span = u16::from(glyph_width);
        if span > self.cols {
            // A glyph wider than the screen has nowhere to go.
            return;
        }
        if self.cursor.pending_wrap {
            self.cursor.pending_wrap = false;
            self.cursor.col = 0;
            self.linefeed();
        }
        if self.cursor.col.saturating_add(span) > self.cols {
            if self.autowrap {
                self.cursor.col = 0;
                self.linefeed();
            } else {
                self.cursor.col = self.cols.saturating_sub(span);
            }
        }
        self.write_cell(self.cursor.row, self.cursor.col, ch, glyph_width);
        self.touched = self.touched.max(self.cursor.row);
        // The cursor never leaves the grid. At the right edge it parks on the
        // glyph just written and remembers that the next one wraps, which is
        // what keeps a following backspace or cursor move meaningful.
        let next = self.cursor.col.saturating_add(span);
        if next >= self.cols {
            self.cursor.col = self.cols.saturating_sub(span.min(self.cols));
            self.cursor.pending_wrap = self.autowrap;
        } else {
            self.cursor.col = next;
            self.cursor.pending_wrap = false;
        }
    }

    /// Writes a glyph, first breaking any wide glyph it lands on.
    fn write_cell(&mut self, row: u16, col: u16, ch: char, glyph_width: u8) {
        let style = self.style;
        let span = u16::from(glyph_width);
        let to = col.saturating_add(span).min(self.cols);
        for probe in col..to {
            let index = self.index(row, probe);
            let Some(cell) = self.cells.get(index).copied() else {
                continue;
            };
            if cell.is_continuation() {
                // The left half of the glyph sits one column back.
                let left = self.index(row, probe.saturating_sub(1));
                if let Some(slot) = self.cells.get_mut(left) {
                    *slot = Cell::blank(style);
                }
                if let Some(slot) = self.cells.get_mut(index) {
                    *slot = Cell::blank(style);
                }
            } else if cell.width > 1 {
                let right = probe.saturating_add(1);
                if let Some(slot) = self.cells.get_mut(index) {
                    *slot = Cell::blank(style);
                }
                if right < self.cols {
                    let index = self.index(row, right);
                    if let Some(slot) = self.cells.get_mut(index) {
                        *slot = Cell::blank(style);
                    }
                }
            }
        }
        let index = self.index(row, col);
        if let Some(slot) = self.cells.get_mut(index) {
            *slot = Cell {
                codepoint: ch,
                width: glyph_width,
                style,
                mark: None,
            };
        }
        for extra in 1..span {
            let probe = col.saturating_add(extra);
            if probe >= self.cols {
                break;
            }
            let index = self.index(row, probe);
            if let Some(slot) = self.cells.get_mut(index) {
                *slot = Cell::continuation(style);
            }
        }
    }

    /// Attaches a combining mark to the glyph it follows.
    fn attach_mark(&mut self, ch: char) {
        let mut row = self.cursor.row;
        let mut col = self.cursor.col;
        if col == 0 {
            let Some(previous) = row.checked_sub(1) else {
                return;
            };
            row = previous;
            col = self.cols.saturating_sub(1);
        } else {
            col = col.saturating_sub(1);
        }
        loop {
            let index = self.index(row, col);
            let Some(cell) = self.cells.get(index).copied() else {
                return;
            };
            if cell.is_continuation() {
                let Some(previous) = col.checked_sub(1) else {
                    return;
                };
                col = previous;
                continue;
            }
            if let Some(slot) = self.cells.get_mut(index)
                && slot.mark.is_none()
            {
                slot.mark = Some(ch);
            }
            self.touched = self.touched.max(row);
            return;
        }
    }

    /// Returns a flat cell index.
    fn index(&self, row: u16, col: u16) -> usize {
        usize::from(row)
            .saturating_mul(usize::from(self.cols))
            .saturating_add(usize::from(col))
    }

    /// Returns the screen as bytes that [`Grid::from_checkpoint`] restores.
    ///
    /// Captures rendered state only. Parser state between sequences is not
    /// retained, so a checkpoint is a comparison device, not a resume point.
    #[must_use]
    pub fn checkpoint(&self) -> Vec<u8> {
        let body = self.encode_body();
        let mut out = Vec::with_capacity(body.len().saturating_add(13));
        out.extend_from_slice(CHECKPOINT_MAGIC);
        out.push(CHECKPOINT_VERSION);
        out.extend_from_slice(&fnv1a(&body).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Restores a grid written by [`Grid::checkpoint`].
    ///
    /// Rejects a buffer with the wrong magic, an unknown version, a mismatched
    /// checksum, or a length that does not match its own header.
    pub fn from_checkpoint(bytes: &[u8]) -> Result<Self> {
        let header = CHECKPOINT_MAGIC.len().saturating_add(9);
        if bytes.len() < header || bytes.get(..CHECKPOINT_MAGIC.len()) != Some(CHECKPOINT_MAGIC) {
            return Err(corrupt("checkpoint magic does not match"));
        }
        if bytes.get(CHECKPOINT_MAGIC.len()) != Some(&CHECKPOINT_VERSION) {
            return Err(RuneError::new(
                ErrorCode::UnsupportedVersion,
                format!("checkpoint version is not {CHECKPOINT_VERSION}"),
            )
            .with_invariant("version == CHECKPOINT_VERSION"));
        }
        let body = bytes.get(header..).unwrap_or(&[]);
        let expected = bytes
            .get(CHECKPOINT_MAGIC.len().saturating_add(1)..header)
            .and_then(|raw| <[u8; 8]>::try_from(raw).ok())
            .map(u64::from_le_bytes);
        if expected != Some(fnv1a(body)) {
            return Err(corrupt("checkpoint checksum does not match"));
        }
        Self::decode_body(body)
    }

    /// Encodes the rendered state without the container header.
    fn encode_body(&self) -> Vec<u8> {
        let mut enc = Encoder::default();
        enc.u16(self.cols);
        enc.u16(self.rows);
        enc.cursor(&self.cursor);
        enc.u8(u8::from(self.autowrap));
        match self.saved {
            Some((cursor, style)) => {
                enc.u8(1);
                enc.cursor(&cursor);
                enc.style(&style);
            }
            None => enc.u8(0),
        }
        enc.style(&self.style);
        enc.u16(u16::try_from(self.links.targets.len()).unwrap_or(u16::MAX));
        for (id, target) in self.links.targets.iter().enumerate() {
            let Some(target) = target else {
                continue;
            };
            enc.u16(u16::try_from(id).unwrap_or(u16::MAX));
            enc.string(target);
        }
        let mut styles: Vec<Style> = Vec::new();
        let mut indices: Vec<u16> = Vec::with_capacity(self.cells.len());
        for cell in &self.cells {
            let index = if let Some(index) = styles.iter().position(|style| *style == cell.style) {
                index
            } else {
                styles.push(cell.style);
                styles.len().saturating_sub(1)
            };
            indices.push(u16::try_from(index).unwrap_or(0));
        }
        enc.u16(u16::try_from(styles.len()).unwrap_or(u16::MAX));
        for style in &styles {
            enc.style(style);
        }
        for (cell, index) in self.cells.iter().zip(indices) {
            enc.u16(index);
            enc.u32(u32::from(cell.codepoint));
            enc.u8(cell.width);
            enc.u32(cell.mark.map_or(0, u32::from));
        }
        enc.finish()
    }

    /// Decodes a body produced by [`Grid::encode_body`].
    fn decode_body(body: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(body);
        let cols = dec.u16()?;
        let rows = dec.u16()?;
        if cols == 0 || rows == 0 {
            return Err(corrupt("checkpoint carries an empty dimension"));
        }
        let cells = cell_count(cols, rows);
        if cells > MAX_CELLS {
            return Err(RuneError::too_large("bounds", cells, MAX_CELLS));
        }
        let mut grid = Self::new(cols, rows)?;
        grid.cursor = dec.cursor()?;
        grid.autowrap = dec.u8()? != 0;
        if dec.u8()? != 0 {
            let cursor = dec.cursor()?;
            let style = dec.style()?;
            grid.saved = Some((cursor, style));
        }
        grid.style = dec.style()?;
        let link_count = usize::from(dec.u16()?);
        for _ in 0..link_count {
            let id = usize::from(dec.u16()?);
            let target = dec.string()?;
            if grid.links.targets.len() <= id {
                grid.links.targets.resize(id.saturating_add(1), None);
            }
            if let Some(slot) = grid.links.targets.get_mut(id) {
                *slot = Some(target.clone());
            }
            grid.links
                .index
                .insert(target, u16::try_from(id).unwrap_or(NO_LINK));
        }
        let style_count = usize::from(dec.u16()?);
        let mut styles = Vec::with_capacity(style_count);
        for _ in 0..style_count {
            styles.push(dec.style()?);
        }
        let needed = cells.saturating_mul(CELL_RECORD);
        if dec.remaining() != needed {
            return Err(corrupt("checkpoint cell block is the wrong length"));
        }
        for index in 0..cells {
            let style_index = usize::from(dec.u16()?);
            let codepoint = dec.u32()?;
            let cell_width = dec.u8()?;
            let mark = dec.u32()?;
            let style = styles
                .get(style_index)
                .copied()
                .ok_or_else(|| corrupt("checkpoint references a style outside its pool"))?;
            let codepoint = char::from_u32(codepoint)
                .ok_or_else(|| corrupt("checkpoint carries a codepoint that is not a character"))?;
            if cell_width > 2 {
                return Err(corrupt("checkpoint carries a cell wider than two columns"));
            }
            let mark = match mark {
                0 => None,
                value => Some(char::from_u32(value).ok_or_else(|| {
                    corrupt("checkpoint carries a combining mark that is not a character")
                })?),
            };
            if let Some(cell) = grid.cells.get_mut(index) {
                *cell = Cell {
                    codepoint,
                    width: cell_width,
                    style,
                    mark,
                };
            }
        }
        if !dec.is_empty() {
            return Err(corrupt("checkpoint carries trailing bytes"));
        }
        Ok(grid)
    }
}

/// Returns a parameter with its default applied.
const fn param_or(value: u16, default: u16) -> u16 {
    if value == 0 { default } else { value }
}

/// Advances a cursor coordinate by a count, clamped to the last index.
fn clamp(coordinate: u16, count: u16, extent: u16) -> u16 {
    coordinate
        .saturating_add(count)
        .min(extent.saturating_sub(1))
}

/// Returns the cell count for a grid size.
fn cell_count(cols: u16, rows: u16) -> usize {
    usize::from(cols).saturating_mul(usize::from(rows))
}

/// Returns a limit error, which counts items rather than bytes.
fn limit(field: &str, observed: usize, cap: usize, invariant: &str) -> RuneError {
    RuneError::new(
        ErrorCode::TooLarge,
        format!("`{field}` is {observed}, limit is {cap}"),
    )
    .with_invariant(invariant)
}

/// Returns a corrupt record error.
fn corrupt(message: impl Into<String>) -> RuneError {
    RuneError::new(ErrorCode::CorruptRecord, message).with_invariant("checkpoint")
}

/// Returns the FNV-1a digest of a byte slice.
#[must_use]
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Appends little-endian fields to a checkpoint.
#[derive(Default)]
struct Encoder {
    out: Vec<u8>,
}

impl Encoder {
    /// Appends one byte.
    fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    /// Appends a 16 bit field.
    fn u16(&mut self, value: u16) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    /// Appends a 32 bit field.
    fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    /// Appends a length-prefixed string.
    fn string(&mut self, value: &str) {
        let len = u16::try_from(value.len()).unwrap_or(u16::MAX);
        self.u16(len);
        let cut = usize::from(len).min(value.len());
        self.out
            .extend_from_slice(value.as_bytes().get(..cut).unwrap_or(&[]));
    }

    /// Appends a cursor.
    fn cursor(&mut self, cursor: &Cursor) {
        self.u16(cursor.row);
        self.u16(cursor.col);
        self.u8(u8::from(cursor.pending_wrap));
    }

    /// Appends a style as a fixed-size record.
    fn style(&mut self, style: &Style) {
        for color in [style.fg, style.bg] {
            match color {
                Color::Default => self.out.extend_from_slice(&[0, 0, 0, 0]),
                Color::Indexed(index) => self.out.extend_from_slice(&[1, index, 0, 0]),
                Color::Rgb(r, g, b) => self.out.extend_from_slice(&[2, r, g, b]),
            }
        }
        self.u16(style.flags);
        self.u16(style.hyperlink.unwrap_or(NO_LINK));
    }

    /// Returns the encoded bytes.
    fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// Reads little-endian fields from a checkpoint body.
struct Decoder<'a> {
    input: &'a [u8],
    at: usize,
}

impl<'a> Decoder<'a> {
    /// Returns a decoder over a body.
    const fn new(input: &'a [u8]) -> Self {
        Self { input, at: 0 }
    }

    /// Returns the number of unread bytes.
    fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.at)
    }

    /// Returns true when every byte has been read.
    const fn is_empty(&self) -> bool {
        self.at == self.input.len()
    }

    /// Reads a fixed-size array.
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let end = self.at.saturating_add(N);
        let raw = self
            .input
            .get(self.at..end)
            .ok_or_else(|| corrupt("checkpoint ended mid-field"))?;
        self.at = end;
        <[u8; N]>::try_from(raw).map_err(|_| corrupt("checkpoint field has the wrong length"))
    }

    /// Reads one byte.
    fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    /// Reads a 16 bit field.
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array::<2>()?))
    }

    /// Reads a 32 bit field.
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array::<4>()?))
    }

    /// Reads a length-prefixed string.
    fn string(&mut self) -> Result<String> {
        let len = usize::from(self.u16()?);
        let end = self.at.saturating_add(len);
        let raw = self
            .input
            .get(self.at..end)
            .ok_or_else(|| corrupt("checkpoint string runs past the end"))?;
        self.at = end;
        std::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| corrupt("checkpoint string is not UTF-8"))
    }

    /// Reads a cursor.
    fn cursor(&mut self) -> Result<Cursor> {
        Ok(Cursor {
            row: self.u16()?,
            col: self.u16()?,
            pending_wrap: self.u8()? != 0,
        })
    }

    /// Reads a fixed-size style record.
    fn style(&mut self) -> Result<Style> {
        let fg = self.color()?;
        let bg = self.color()?;
        let flags = self.u16()?;
        if flags & !flag::ALL != 0 {
            return Err(corrupt(
                "checkpoint carries attribute bits that are not defined",
            ));
        }
        let hyperlink = match self.u16()? {
            NO_LINK => None,
            id => Some(id),
        };
        Ok(Style {
            fg,
            bg,
            flags,
            hyperlink,
        })
    }

    /// Reads a color record.
    fn color(&mut self) -> Result<Color> {
        let raw = self.array::<4>()?;
        Ok(match raw[0] {
            0 => Color::Default,
            1 => Color::Indexed(raw[1]),
            2 => Color::Rgb(raw[1], raw[2], raw[3]),
            _ => return Err(corrupt("checkpoint carries an unknown color tag")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(cols: u16, rows: u16) -> Grid {
        Grid::new(cols, rows).expect("grid")
    }

    fn feed(grid: &mut Grid, text: &str) -> FeedStats {
        grid.feed(text.as_bytes()).expect("feed")
    }

    #[test]
    fn plain_text_lands_on_the_first_row() {
        let mut g = grid(10, 3);
        let stats = feed(&mut g, "hello");
        assert_eq!(g.row_text(0), "hello");
        assert_eq!(g.cursor().col, 5);
        assert!(!stats.scrolled);
        assert_eq!(stats.max_row_touched, 0);
    }

    #[test]
    fn recorded_stream_renders_a_screen() {
        let mut g = grid(12, 4);
        let stream = concat!(
            "\u{1b}[2J**rune**\r\n",
            "\u{1b}[1;36mthinking\u{1b}[0m...\r\n",
            "\u{1b}[33m> \u{1b}[0mnominal\r\n",
            "done",
        );
        let stats = feed(&mut g, stream);
        let rows: Vec<String> = (0..4).map(|row| g.row_text(row)).collect();
        assert_eq!(rows, vec!["**rune**", "thinking...", "> nominal", "done"]);
        assert_eq!(stats.max_row_touched, 3);
        assert!(!stats.scrolled);
    }

    #[test]
    fn cursor_movement_sequences_place_text() {
        let mut g = grid(8, 4);
        feed(&mut g, "\u{1b}[3;5Hx");
        assert_eq!(g.row_text(2), "    x");
        // The cursor sat one past the written glyph, so two back is column 3.
        feed(&mut g, "\u{1b}[A\u{1b}[2Dy");
        assert_eq!(g.row_text(1), "   y");
        feed(&mut g, "\u{1b}[1;1Hz");
        assert_eq!(g.row_text(0), "z");
    }

    #[test]
    fn erase_sequences_clear_without_moving_the_cursor() {
        let mut g = grid(8, 3);
        feed(&mut g, "abcdef\u{1b}[3D\u{1b}[K");
        assert_eq!(g.row_text(0), "abc");
        assert_eq!(g.cursor().col, 3);
        feed(&mut g, "\u{1b}[2J");
        assert_eq!(g.text(), "\n\n");
    }

    #[test]
    fn sgr_sets_the_style_of_written_cells() {
        let mut g = grid(8, 2);
        feed(&mut g, "\u{1b}[1;31mA");
        let style = g.cell(0, 0).expect("cell").style;
        assert!(style.has_flag(flag::BOLD));
        assert_eq!(style.fg, Color::Indexed(1));
        assert_eq!(g.style().sgr(), "\u{1b}[1;38;5;1m");
    }

    #[test]
    fn sgr_reset_clears_attributes_but_keeps_a_hyperlink() {
        let mut g = grid(8, 2);
        feed(&mut g, "\u{1b}]8;;https://example.com\u{1b}\\\u{1b}[1m");
        feed(&mut g, "\u{1b}[0mX");
        let style = g.cell(0, 0).expect("cell").style;
        assert_eq!(style.flags, 0);
        assert_eq!(g.cell_hyperlink(0, 0), Some("https://example.com"));
    }

    #[test]
    fn sgr_reset_survives_a_split_across_feeds() {
        let mut g = grid(8, 2);
        g.feed(b"\x1b[31").expect("prefix");
        g.feed(b"mX").expect("rest");
        assert_eq!(g.cell(0, 0).expect("cell").style.fg, Color::Indexed(1));
    }

    #[test]
    fn csi_split_across_feeds_still_executes() {
        let mut g = grid(8, 3);
        g.feed(b"\x1b[").expect("introducer");
        g.feed(b"2;3").expect("params");
        g.feed(b"Hx").expect("final");
        assert_eq!(g.row_text(1), "  x");
    }

    #[test]
    fn wide_glyph_occupies_two_columns() {
        let mut g = grid(8, 2);
        feed(&mut g, "中文");
        assert_eq!(g.row_text(0), "中文");
        assert_eq!(g.row_text(1), "");
        assert_eq!(g.cursor().col, 4);
        // The second column of the glyph is its continuation, not 文.
        assert!(g.cell(0, 1).expect("cell").is_continuation());
        assert!(g.cell(0, 3).expect("cell").is_continuation());
    }

    #[test]
    fn a_wide_glyph_at_the_edge_wraps_whole() {
        let mut g = grid(5, 3);
        feed(&mut g, "abcd中");
        assert_eq!(g.row_text(0), "abcd");
        assert_eq!(g.row_text(1), "中");
        assert_eq!(g.cursor().row, 1);
    }

    #[test]
    fn combining_mark_attaches_to_its_base() {
        let mut g = grid(8, 2);
        feed(&mut g, "e\u{301}x");
        assert_eq!(g.cell(0, 0).expect("cell").mark, Some('\u{301}'));
        assert_eq!(g.row_text(0), "e\u{301}x");
        assert_eq!(g.cursor().col, 2);
    }

    #[test]
    fn printing_at_the_last_column_wraps() {
        let mut g = grid(4, 3);
        let stats = feed(&mut g, "abcdef");
        assert_eq!(g.row_text(0), "abcd");
        assert_eq!(g.row_text(1), "ef");
        assert!(!stats.scrolled);
    }

    #[test]
    fn autowrap_off_overwrites_the_last_column() {
        let mut g = grid(4, 3);
        assert!(g.autowrap());
        feed(&mut g, "\u{1b}[?7labcdef");
        assert!(!g.autowrap());
        assert_eq!(g.row_text(0), "abcf");
        assert_eq!(g.row_text(1), "");
    }

    #[test]
    fn the_cursor_never_leaves_the_grid() {
        let mut g = grid(4, 2);
        feed(&mut g, "abcdefghij");
        assert!(g.cursor().col < 4);
        let mut g = grid(4, 2);
        feed(&mut g, "\u{1b}[?7labcdefghij");
        assert!(g.cursor().col < 4);
        let mut g = grid(4, 2);
        feed(&mut g, "\u{1b}[?7l中中中中");
        assert!(g.cursor().col < 4);
    }

    #[test]
    fn scrolling_reports_rows_and_keeps_the_bottom() {
        let mut g = grid(6, 2);
        let stats = feed(&mut g, "one\r\ntwo\r\nthree");
        assert!(stats.scrolled);
        assert_eq!(stats.scroll_rows, 1);
        assert_eq!(g.text(), "two\nthree");
    }

    #[test]
    fn linefeed_at_the_bottom_scrolls() {
        let mut g = grid(6, 2);
        let stats = feed(&mut g, "a\r\nb\r\n");
        assert!(stats.scrolled);
        assert_eq!(g.row_text(0), "b");
        assert_eq!(g.row_text(1), "");
    }

    #[test]
    fn unknown_escape_does_not_corrupt_the_grid() {
        let mut g = grid(10, 2);
        feed(&mut g, "a\u{1b}[9999z\u{1b}#8\u{1b}(Bb\u{1b}[?99999hc");
        assert_eq!(g.row_text(0), "abc");
        assert_eq!(g.cursor().col, 3);
    }

    #[test]
    fn unknown_osc_does_not_corrupt_the_grid() {
        let mut g = grid(10, 2);
        feed(&mut g, "a\u{1b}]0;window title\u{7}b");
        assert_eq!(g.row_text(0), "ab");
    }

    #[test]
    fn cursor_save_and_restore_round_trips() {
        let mut g = grid(8, 3);
        feed(&mut g, "\u{1b}[2;3H\u{1b}7\u{1b}[1;1Hx\u{1b}8y");
        assert_eq!(g.row_text(0), "x");
        assert_eq!(g.row_text(1), "  y");
    }

    #[test]
    fn hyperlink_is_recorded_on_the_cells_it_covers() {
        let mut g = grid(12, 2);
        feed(
            &mut g,
            "\u{1b}]8;;https://a.test\u{1b}\\link\u{1b}]8;;\u{1b}\\plain",
        );
        assert_eq!(g.cell_hyperlink(0, 0), Some("https://a.test"));
        assert_eq!(g.cell_hyperlink(0, 3), Some("https://a.test"));
        assert_eq!(g.cell_hyperlink(0, 4), None);
        assert_eq!(g.hyperlink_count(), 1);
    }

    #[test]
    fn repeated_hyperlink_targets_reuse_one_slot() {
        let mut g = grid(8, 2);
        for _ in 0..8 {
            feed(
                &mut g,
                "\u{1b}]8;;https://a.test\u{1b}\\x\u{1b}]8;;\u{1b}\\",
            );
        }
        assert_eq!(g.hyperlink_count(), 1);
    }

    #[test]
    fn params_past_the_cap_are_rejected() {
        let mut g = grid(8, 2);
        let err = g
            .feed(b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17m")
            .expect_err("params past the cap");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(
            err.detail().invariant.as_deref(),
            Some("params <= MAX_CSI_PARAMS")
        );
    }

    #[test]
    fn params_within_the_cap_are_accepted() {
        let mut g = grid(8, 2);
        let mut stream = String::from("\u{1b}[");
        for index in 0..MAX_CSI_PARAMS {
            if index > 0 {
                stream.push(';');
            }
            stream.push('1');
        }
        stream.push('m');
        g.feed(stream.as_bytes()).expect("exactly the cap");
    }

    #[test]
    fn intermediates_past_the_cap_are_rejected() {
        let mut g = grid(8, 2);
        let err = g
            .feed(b"\x1b[!!!m")
            .expect_err("intermediates past the cap");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(
            err.detail().invariant.as_deref(),
            Some("intermediates <= MAX_CSI_INTERMEDIATES")
        );
    }

    #[test]
    fn escape_intermediates_past_the_cap_are_rejected() {
        let mut g = grid(8, 2);
        let err = g.feed(b"\x1b###8").expect_err("intermediates past the cap");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn long_osc_payload_is_rejected() {
        let mut g = grid(8, 2);
        let mut stream = String::from("\u{1b}]8;;https://a.test/");
        while stream.len() <= MAX_OSC_BYTES.saturating_add(16) {
            stream.push('x');
        }
        stream.push('\u{7}');
        let err = g.feed(stream.as_bytes()).expect_err("osc past the cap");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(
            err.detail().invariant.as_deref(),
            Some("osc payload <= MAX_OSC_BYTES")
        );
    }

    #[test]
    fn osc_payload_up_to_the_cap_is_accepted() {
        let mut g = grid(8, 2);
        let mut stream = String::from("\u{1b}]8;;");
        while stream.len() < MAX_OSC_BYTES.saturating_sub(1) {
            stream.push('x');
        }
        stream.push('\u{7}');
        g.feed(stream.as_bytes()).expect("exactly the cap");
        assert_eq!(g.hyperlink_count(), 1);
    }

    #[test]
    fn hyperlink_pool_past_the_cap_is_rejected() {
        let mut g = grid(8, 2);
        for index in 0..MAX_HYPERLINKS {
            let link = format!("\u{1b}]8;;https://a.test/{index}\u{1b}\\x");
            g.feed(link.as_bytes()).expect("link within the cap");
        }
        assert_eq!(g.hyperlink_count(), MAX_HYPERLINKS);
        let err = g
            .feed(b"\x1b]8;;https://a.test/overflow\x1b\\x")
            .expect_err("links past the cap");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(
            err.detail().invariant.as_deref(),
            Some("links <= MAX_HYPERLINKS")
        );
    }

    #[test]
    fn grid_past_the_cell_cap_is_rejected() {
        let err = Grid::new(2048, 1024).expect_err("cells past the cap");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("bounds"));
    }

    #[test]
    fn grid_with_a_zero_dimension_is_rejected() {
        assert_eq!(
            Grid::new(0, 4).expect_err("zero cols").code(),
            ErrorCode::InvalidField
        );
        assert_eq!(
            Grid::new(4, 0).expect_err("zero rows").code(),
            ErrorCode::InvalidField
        );
    }

    #[test]
    fn after_a_bound_error_the_parser_recovers() {
        let mut g = grid(8, 2);
        assert!(
            g.feed(b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17m")
                .is_err()
        );
        feed(&mut g, "ok");
        assert_eq!(g.row_text(0), "ok");
    }

    #[test]
    fn diff_reports_only_changed_runs() {
        let mut before = grid(8, 3);
        feed(&mut before, "abc");
        let mut after = before.clone();
        feed(&mut after, "\r\nxy");
        let spans = before.diff(&after);
        assert_eq!(
            spans,
            vec![DiffSpan {
                row: 1,
                start: 0,
                end: 2
            }]
        );
    }

    #[test]
    fn diff_of_identical_grids_is_empty() {
        let mut g = grid(8, 3);
        feed(&mut g, "\u{1b}[31mred");
        assert!(g.diff(&g.clone()).is_empty());
    }

    #[test]
    fn resize_preserves_content_and_cursor() {
        let mut g = grid(6, 3);
        feed(&mut g, "ab\r\ncd\u{1b}[2;2H");
        g.resize(10, 5).expect("grow");
        assert_eq!(g.bounds(), Bounds { cols: 10, rows: 5 });
        assert_eq!(g.row_text(0), "ab");
        assert_eq!(g.row_text(1), "cd");
        assert_eq!(
            g.cursor(),
            Cursor {
                row: 1,
                col: 1,
                pending_wrap: false
            }
        );
    }

    #[test]
    fn resize_narrowing_drops_the_overflow() {
        let mut g = grid(8, 3);
        feed(&mut g, "abcdef");
        g.resize(4, 3).expect("shrink");
        assert_eq!(g.row_text(0), "abcd");
        assert_eq!(g.cursor().col, 3);
    }

    #[test]
    fn resize_never_leaves_half_a_wide_glyph() {
        let mut g = grid(8, 2);
        feed(&mut g, "a中b");
        // Two of the three retained columns hold the wide glyph, so it stays.
        g.resize(3, 2).expect("shrink");
        assert_eq!(g.row_text(0), "a中");
        let mut g = grid(8, 2);
        feed(&mut g, "a中b");
        g.resize(2, 2).expect("shrink");
        assert_eq!(g.row_text(0), "a");
    }

    #[test]
    fn resize_dropping_rows_keeps_the_top() {
        let mut g = grid(6, 3);
        feed(&mut g, "a\r\nb\r\nc");
        g.resize(6, 2).expect("shrink rows");
        assert_eq!(g.text(), "a\nb");
        assert_eq!(g.cursor().row, 1);
    }

    #[test]
    fn resize_growing_rows_adds_blanks() {
        let mut g = grid(6, 2);
        feed(&mut g, "a");
        g.resize(6, 4).expect("grow rows");
        assert_eq!(g.text(), "a\n\n\n");
    }

    #[test]
    fn resize_to_the_same_size_is_a_no_op() {
        let mut g = grid(6, 2);
        feed(&mut g, "\u{1b}[1;33mb");
        g.resize(6, 2).expect("same size");
        assert_eq!(g.row_text(0), "b");
        assert_eq!(g.cell(0, 0).expect("cell").style.fg, Color::Indexed(3));
    }

    #[test]
    fn checkpoint_round_trips() {
        let mut g = grid(12, 4);
        feed(
            &mut g,
            "\u{1b}[1;31mwarning\u{1b}[0m\r\n\u{1b}]8;;https://a.test\u{1b}\\link\u{1b}]8;;\u{1b}\\中文 e\u{301}",
        );
        g.resize(14, 5).expect("resize");
        let restored = Grid::from_checkpoint(&g.checkpoint()).expect("restore");
        assert_eq!(restored, g);
        assert_eq!(restored.text(), g.text());
        assert_eq!(restored.cell_hyperlink(1, 0), Some("https://a.test"));
    }

    #[test]
    fn checkpoint_carries_a_version_and_checksum() {
        let g = grid(4, 2);
        let bytes = g.checkpoint();
        assert_eq!(bytes.get(..4), Some(CHECKPOINT_MAGIC.as_slice()));
        assert_eq!(bytes.get(4), Some(&CHECKPOINT_VERSION));
        let mut corrupted = bytes.clone();
        let last = corrupted.len().saturating_sub(1);
        corrupted[last] ^= 0xff;
        let err = Grid::from_checkpoint(&corrupted).expect_err("corrupt body");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
    }

    #[test]
    fn checkpoint_rejects_undefined_attribute_bits() {
        let mut g = grid(4, 2);
        feed(&mut g, "\u{1b}[1mA");
        let mut bytes = g.checkpoint();
        // The body is cols, rows, cursor, autowrap, saved flag, live style,
        // link count, style count, then one record per pooled style. The first
        // pooled style's flags sit eight bytes into its record.
        let header = CHECKPOINT_MAGIC.len().saturating_add(9);
        let flags_at = header
            .saturating_add(2 + 2 + 5 + 1 + 1 + 12 + 2 + 2)
            .saturating_add(8);
        bytes[flags_at] |= 0x80;
        let body = bytes.get(header..).unwrap_or(&[]).to_vec();
        let checksum = fnv1a(&body).to_le_bytes();
        if let Some(slot) = bytes.get_mut(CHECKPOINT_MAGIC.len().saturating_add(1)..header) {
            slot.copy_from_slice(&checksum);
        }
        let err = Grid::from_checkpoint(&bytes).expect_err("undefined attribute bits");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
    }

    #[test]
    fn checkpoint_rejects_an_unknown_version() {
        let mut bytes = grid(4, 2).checkpoint();
        bytes[4] = CHECKPOINT_VERSION.saturating_add(1);
        let err = Grid::from_checkpoint(&bytes).expect_err("unknown version");
        assert_eq!(err.code(), ErrorCode::UnsupportedVersion);
    }

    #[test]
    fn checkpoint_rejects_foreign_and_truncated_buffers() {
        assert_eq!(
            Grid::from_checkpoint(b"not a grid")
                .expect_err("foreign")
                .code(),
            ErrorCode::CorruptRecord
        );
        let bytes = grid(4, 2).checkpoint();
        let cut = bytes.len().saturating_sub(4);
        assert!(Grid::from_checkpoint(&bytes[..cut]).is_err());
    }

    #[test]
    fn reset_clears_screen_and_attributes() {
        let mut g = grid(6, 2);
        feed(&mut g, "\u{1b}[1;31mtext\u{1b}c");
        assert_eq!(g.text(), "\n");
        assert!(g.style().sgr().is_empty());
        assert_eq!(g.cursor(), Cursor::default());
    }

    #[test]
    fn tabs_advance_to_a_column_stop() {
        let mut g = grid(20, 2);
        feed(&mut g, "a\tb");
        assert_eq!(g.row_text(0), "a       b");
    }

    #[test]
    fn utf8_split_across_feeds_decodes_once() {
        let mut g = grid(8, 2);
        let bytes = "中".as_bytes();
        g.feed(bytes.get(..1).expect("lead")).expect("lead");
        assert_eq!(g.row_text(0), "");
        g.feed(bytes.get(1..).expect("tail")).expect("tail");
        assert_eq!(g.row_text(0), "中");
        assert_eq!(g.cursor().col, 2);
    }

    #[test]
    fn lone_continuation_bytes_are_dropped() {
        let mut g = grid(8, 2);
        g.feed(b"a\xb0b").expect("feed");
        assert_eq!(g.row_text(0), "ab");
    }

    #[test]
    fn carriage_return_and_backspace_move_the_cursor() {
        let mut g = grid(8, 2);
        feed(&mut g, "abc\rZ");
        assert_eq!(g.row_text(0), "Zbc");
        feed(&mut g, "\u{8}Y");
        assert_eq!(g.row_text(0), "Ybc");
    }

    #[test]
    fn erase_from_cursor_forward_clears_the_tail() {
        let mut g = grid(8, 2);
        feed(&mut g, "abcdef\u{1b}[2D\u{1b}[0K");
        assert_eq!(g.row_text(0), "abcd");
    }

    #[test]
    fn erase_from_cursor_back_clears_the_head() {
        let mut g = grid(8, 2);
        feed(&mut g, "abcdef\u{1b}[1;4H\u{1b}[1K");
        assert_eq!(g.row_text(0), "    ef");
    }

    #[test]
    fn vpa_and_cha_position_the_cursor() {
        let mut g = grid(8, 4);
        feed(&mut g, "\u{1b}[3d\u{1b}[5Gx");
        assert_eq!(g.row_text(2), "    x");
    }

    #[test]
    fn csi_beyond_the_screen_clamps() {
        let mut g = grid(4, 2);
        feed(&mut g, "\u{1b}[99;99Hx");
        assert_eq!(g.row_text(1), "   x");
    }

    #[test]
    fn style_round_trips_through_sgr() {
        let mut style = Style::new();
        style.apply_sgr("1;2;3;4;7;9;38;2;10;20;30;48;5;200");
        assert_eq!(style.sgr(), "\u{1b}[1;2;3;4;7;9;38;2;10;20;30;48;5;200m");
        let mut replayed = Style::new();
        let sequence = style.sgr();
        replayed.apply_sgr(sequence.trim_start_matches("\u{1b}[").trim_end_matches('m'));
        assert_eq!(replayed, style);
    }

    #[test]
    fn bright_colors_map_onto_the_extended_palette() {
        let mut style = Style::new();
        style.apply_sgr("91;102");
        assert_eq!(style.fg, Color::Indexed(9));
        assert_eq!(style.bg, Color::Indexed(10));
    }

    #[test]
    fn a_truncated_extended_color_leaves_the_style_alone() {
        let mut style = Style::new();
        style.apply_sgr("38;5");
        assert_eq!(style.fg, Color::Default);
    }
}
