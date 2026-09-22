//! Terminal engine, frame rendering, transcript, input, and themes.

#![forbid(unsafe_code)]
// Tests assert by panicking. The guards that forbid panicking apply to the
// shipped build, where a panic on user input is a defect.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod engine;
pub mod shell;
pub mod width;

pub use engine::{Bounds, Cell, Color, Cursor, DiffSpan, FeedStats, Grid, Style, flag};
pub use width::{WrappedLine, char_width, graphemes, str_width, truncate_to_width, wrap};
