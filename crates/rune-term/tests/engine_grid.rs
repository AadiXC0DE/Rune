// Integration tests assert by panicking.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! End-to-end tests for the terminal engine.
//!
//! Each case feeds a recorded byte stream and snapshots the resulting grid, so
//! a change in placement or styling shows up as a reviewable diff rather than
//! as a test that only asserts a length.

use std::fmt::Write as _;

use rune_core::error::ErrorCode;
use rune_term::engine::{
    Bounds, Color, Cursor, Grid, MAX_CSI_PARAMS, MAX_HYPERLINKS, MAX_OSC_BYTES, flag,
};
use rune_term::width::{str_width, wrap};

/// Renders a grid with visible column and row boundaries, for snapshotting.
fn render(grid: &Grid) -> String {
    let bounds = grid.bounds();
    let mut out = String::new();
    let _ = writeln!(out, "{}x{}", bounds.cols, bounds.rows);
    for row in 0..bounds.rows {
        let mut line = String::from("|");
        for col in 0..bounds.cols {
            let cell = grid.cell(row, col).expect("cell in bounds");
            if cell.is_continuation() {
                line.push('>');
            } else if cell.style.hyperlink.is_some() {
                line.push('@');
            } else {
                line.push(cell.codepoint);
            }
        }
        line.push('|');
        out.push_str(line.trim_end());
        out.push('\n');
    }
    let cursor = grid.cursor();
    let _ = writeln!(
        out,
        "cursor {}:{} wrap={}",
        cursor.row, cursor.col, cursor.pending_wrap
    );
    for row in 0..bounds.rows {
        let _ = writeln!(out, "row {row}: {:?}", grid.row_text(row));
    }
    out
}

/// A transcript as it would arrive from a provider stream.
const TRANSCRIPT: &str = concat!(
    "\u{1b}[2J\u{1b}[H",
    "\u{1b}[1m**rune**\u{1b}[0m\r\n",
    "\u{1b}[2mplanning\u{1b}[0m the change\r\n",
    "\u{1b}]8;;https://example.test/issue/7\u{1b}\\issue 7\u{1b}]8;;\u{1b}\\\r\n",
    "\u{1b}[33m!\u{1b}[0m \u{1b}[4mwarning\u{1b}[0m: nothing to do\r\n",
    "done",
);

#[test]
fn recorded_transcript_matches_its_snapshot() {
    let mut grid = Grid::new(24, 6).expect("grid");
    let stats = grid.feed(TRANSCRIPT.as_bytes()).expect("feed");
    assert_eq!(stats.bytes_consumed, TRANSCRIPT.len());
    assert_eq!(stats.max_row_touched, 4);
    assert!(!stats.scrolled);
    insta::assert_snapshot!("transcript", render(&grid));
}

#[test]
fn recorded_stream_that_scrolls_matches_its_snapshot() {
    let mut grid = Grid::new(12, 3).expect("grid");
    let stats = grid
        .feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive")
        .expect("feed");
    assert!(stats.scrolled);
    assert_eq!(stats.scroll_rows, 2);
    assert_eq!(stats.max_row_touched, 2);
    insta::assert_snapshot!("scrolled", render(&grid));
}

#[test]
fn styled_and_linked_screen_matches_its_snapshot() {
    let mut grid = Grid::new(20, 4).expect("grid");
    let stream = concat!(
        "\u{1b}[1;4;38;5;208mbold underline\u{1b}[0m\r\n",
        "\u{1b}[38;2;10;20;30;48;2;200;210;220mtruecolor\u{1b}[0m\r\n",
        "\u{1b}[7mreverse\u{1b}[0m\u{1b}[9mstrike\u{1b}[0m",
    );
    grid.feed(stream.as_bytes()).expect("feed");
    assert_eq!(grid.cell(0, 0).expect("cell").style.fg, Color::Indexed(208));
    assert!(
        grid.cell(0, 0)
            .expect("cell")
            .style
            .has_flag(flag::UNDERLINE)
    );
    insta::assert_snapshot!("styled", render(&grid));
}

/// Recorded resize cases: a stream, a target size, and what must survive.
const RESIZE_CASES: &[(&str, Bounds, u16, u16)] = &[
    ("alpha\r\nbeta", Bounds { cols: 10, rows: 3 }, 20, 6),
    ("alpha\r\nbeta", Bounds { cols: 10, rows: 3 }, 4, 3),
    ("abcdefghij", Bounds { cols: 10, rows: 2 }, 6, 2),
    ("a中b", Bounds { cols: 8, rows: 2 }, 3, 2),
    ("a中b", Bounds { cols: 8, rows: 2 }, 2, 2),
    ("press", Bounds { cols: 6, rows: 2 }, 6, 4),
];

#[test]
fn recorded_resize_cases_match_their_snapshot() {
    let mut out = String::new();
    for (stream, from, cols, rows) in RESIZE_CASES {
        let mut grid = Grid::new(from.cols, from.rows).expect("grid");
        grid.feed(stream.as_bytes()).expect("feed");
        let before = render(&grid);
        grid.resize(*cols, *rows).expect("resize");
        // Not `---`, which is the snapshot file's own header delimiter.
        let _ = writeln!(out, "== {from:?} -> {cols}x{rows}");
        out.push_str("before:\n");
        out.push_str(&before);
        out.push_str("after:\n");
        out.push_str(&render(&grid));
    }
    insta::assert_snapshot!("resize", out);
}

#[test]
fn resize_preserves_content_and_cursor_for_every_recorded_case() {
    for (stream, from, cols, rows) in RESIZE_CASES {
        let mut grid = Grid::new(from.cols, from.rows).expect("grid");
        grid.feed(stream.as_bytes()).expect("feed");
        let before: Vec<String> = (0..from.rows).map(|row| grid.row_text(row)).collect();
        let (from_cols, from_rows) = (usize::from(from.cols), usize::from(from.rows));
        let cursor = grid.cursor();
        grid.resize(*cols, *rows).expect("resize");

        assert_eq!(
            grid.bounds(),
            Bounds {
                cols: *cols,
                rows: *rows
            }
        );
        assert!(grid.cursor().row < *rows, "cursor row left the grid");
        assert!(grid.cursor().col < *cols, "cursor col left the grid");

        let keep_rows = from_rows.min(usize::from(*rows));
        let keep_cols = from_cols.min(usize::from(*cols));
        for (row, original) in before.iter().enumerate().take(keep_rows) {
            let after = grid.row_text(u16::try_from(row).expect("row in range"));
            // Growing keeps every row whole; narrowing keeps the prefix that
            // still fits and never resurrects text from a dropped column.
            if *cols >= from.cols {
                assert_eq!(&after, original, "row {row} changed while growing");
            } else {
                assert!(
                    original.starts_with(&after) || str_width(original) > *cols as usize,
                    "row {row} gained text while narrowing: {after:?} from {original:?}"
                );
                assert!(after.chars().count() <= keep_cols);
            }
            assert!(
                str_width(&after) <= *cols as usize,
                "row {row} overflowed its width"
            );
        }
        // Rows dropped by a shrink are gone, not hidden.
        for row in *rows..from.rows {
            assert_eq!(grid.row_text(row), "");
        }
        if *rows >= from.rows && *cols >= from.cols {
            assert_eq!(grid.cursor().row, cursor.row);
        }
    }
}

#[test]
fn an_unknown_escape_leaves_the_grid_intact() {
    let mut plain = Grid::new(16, 3).expect("grid");
    plain.feed(b"abc\ndef").expect("feed");

    let mut noisy = Grid::new(16, 3).expect("grid");
    noisy
        .feed(b"a\x1b[?99999h\x1b[99;99X\x1b#8\x1b(0\x1b)0bc\ndef")
        .expect("feed");

    assert_eq!(noisy.text(), plain.text());
    assert_eq!(noisy.cursor(), plain.cursor());
    assert_eq!(noisy, plain);
}

#[test]
fn cjk_and_emoji_wrap_at_the_right_column() {
    // Six columns hold three wide glyphs and nothing more.
    let lines = wrap("中文中文", 6);
    assert_eq!(lines, vec!["中文中", "文"]);
    assert_eq!(str_width(&lines[0]), 6);

    // A five column line holds two wide glyphs, leaving one column unusable.
    let lines = wrap("中文中文", 5);
    assert_eq!(lines, vec!["中文", "中文"]);

    // A joined emoji is one cluster and is never split.
    let lines = wrap("a👨‍👩‍👧b", 3);
    assert_eq!(lines, vec!["a👨‍👩‍👧", "b"]);
    assert_eq!(str_width(&lines[0]), 3);
}

#[test]
fn a_combining_mark_attaches_to_its_base() {
    let mut grid = Grid::new(8, 2).expect("grid");
    grid.feed("e\u{301}\u{301}x".as_bytes()).expect("feed");
    let first = grid.cell(0, 0).expect("cell");
    assert_eq!(first.codepoint, 'e');
    assert_eq!(first.mark, Some('\u{301}'));
    // A second mark on the same base is dropped rather than displacing text.
    assert_eq!(grid.cell(0, 1).expect("cell").codepoint, 'x');
    assert_eq!(grid.row_text(0), "e\u{301}x");
}

#[test]
fn a_styled_wrap_keeps_its_style_on_both_lines() {
    let lines = wrap("\u{1b}[1;36mthinking hard\u{1b}[0m", 8);
    assert_eq!(lines.len(), 2);
    for line in &lines {
        let mut grid = Grid::new(16, 2).expect("grid");
        grid.feed(line.as_bytes()).expect("feed");
        let style = grid.cell(0, 0).expect("cell").style;
        assert!(style.has_flag(flag::BOLD), "line lost its weight: {line:?}");
        assert_eq!(
            style.fg,
            Color::Indexed(6),
            "line lost its colour: {line:?}"
        );
        // The style is closed again, so a following line is not tinted.
        grid.feed(b"\r\nplain").expect("feed");
        assert!(grid.cell(1, 0).expect("cell").style.sgr().is_empty());
    }
}

#[test]
fn a_checkpoint_restores_the_identical_grid() {
    let mut grid = Grid::new(24, 6).expect("grid");
    grid.feed(TRANSCRIPT.as_bytes()).expect("feed");
    grid.feed(b"\x1b[1;36m").expect("feed");

    let restored = Grid::from_checkpoint(&grid.checkpoint()).expect("restore");
    assert_eq!(restored, grid);
    assert_eq!(restored.text(), grid.text());
    assert_eq!(restored.bounds(), grid.bounds());
    assert_eq!(restored.cursor(), grid.cursor());
    assert_eq!(restored.style(), grid.style());
    assert_eq!(restored.hyperlink_count(), grid.hyperlink_count());
    assert_eq!(
        restored.cell_hyperlink(2, 0),
        Some("https://example.test/issue/7")
    );
    // The restored grid keeps rendering, which a naive field copy would not.
    let mut continued = restored;
    continued.feed(b"more").expect("feed");
    assert!(continued.text().contains("more"));
}

#[test]
fn every_bound_reports_a_typed_error() {
    let mut grid = Grid::new(8, 2).expect("grid");
    let mut params = String::from("\u{1b}[");
    for index in 0..=MAX_CSI_PARAMS {
        if index > 0 {
            params.push(';');
        }
        params.push('1');
    }
    params.push('m');
    assert_eq!(
        grid.feed(params.as_bytes()).expect_err("params").code(),
        ErrorCode::TooLarge
    );

    let mut grid = Grid::new(8, 2).expect("grid");
    let long_osc = format!("\u{1b}]8;;{}\u{7}", "x".repeat(MAX_OSC_BYTES + 1));
    assert_eq!(
        grid.feed(long_osc.as_bytes()).expect_err("osc").code(),
        ErrorCode::TooLarge
    );

    let mut grid = Grid::new(8, 2).expect("grid");
    for index in 0..=MAX_HYPERLINKS {
        let link = format!("\u{1b}]8;;https://a.test/{index}\u{1b}\\");
        let result = grid.feed(link.as_bytes());
        if index == MAX_HYPERLINKS {
            assert_eq!(result.expect_err("links").code(), ErrorCode::TooLarge);
        } else {
            result.expect("link within the cap");
        }
    }

    assert_eq!(
        Grid::new(4096, 4096).expect_err("cells").code(),
        ErrorCode::TooLarge
    );
    assert_eq!(
        Grid::new(0, 1).expect_err("zero").code(),
        ErrorCode::InvalidField
    );
}

#[test]
fn the_cursor_round_trips_through_a_save_and_restore() {
    let mut grid = Grid::new(12, 4).expect("grid");
    grid.feed(b"\x1b[2;4H\x1b7\x1b[4;1Hx\x1b8y").expect("feed");
    assert_eq!(
        grid.cursor(),
        Cursor {
            row: 1,
            col: 4,
            pending_wrap: false
        }
    );
    assert_eq!(grid.row_text(1), "   y");
    assert_eq!(grid.row_text(3), "x");
}
