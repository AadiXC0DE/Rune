// Integration tests assert by panicking.
#![allow(clippy::expect_used)]

//! Transcript text must survive promotion into terminal scrollback.

use rune_term::engine::Grid;
use rune_term::inline::{Frame, Inline};
use rune_term::transcript::{self, Display, Entry};

/// Captures rows as they leave the grid, like a terminal's scrollback buffer.
fn replay(grid: &mut Grid, scrollback: &mut Vec<String>, bytes: &[u8]) {
    for byte in bytes {
        let top = grid.row_text(0);
        let stats = grid.feed(std::slice::from_ref(byte)).expect("feed");
        if stats.scrolled {
            assert_eq!(stats.scroll_rows, 1);
            scrollback.push(top);
        }
    }
}

#[test]
fn a_long_word_and_its_marker_survive_twelve_column_scrollback() {
    let text = "W".repeat(300) + "\nEND-LONG-WORD";
    let display = Display {
        width: 12,
        ..Display::default()
    };
    let rendered = transcript::render(&[Entry::assistant(&text)], display);
    let rows: Vec<String> = rendered.lines().map(str::to_owned).collect();
    let mut inline = Inline::new(12);
    inline.set_max_rows(7);
    let mut grid = Grid::new(12, 8).expect("grid");
    let mut scrollback = Vec::new();
    let footer = vec!["status".to_owned()];
    let prompt = vec!["> ".to_owned()];
    let mut frame = Frame {
        footer: &footer,
        prompt: &prompt,
        caret: (0, 2),
        ..Frame::default()
    };

    replay(&mut grid, &mut scrollback, &inline.frame(&frame));
    frame.arriving = &rows;
    replay(&mut grid, &mut scrollback, &inline.frame(&frame));
    frame.arriving = &[];
    frame.settled = &rows;
    replay(&mut grid, &mut scrollback, &inline.frame(&frame));
    frame.settled = &[];
    replay(&mut grid, &mut scrollback, &inline.frame(&frame));

    // Flush the remaining transcript rows into scrollback, then recover only
    // the history rather than reading the source or the still-visible screen.
    let blank_rows = vec![String::new(); 8];
    frame.settled = &blank_rows;
    replay(&mut grid, &mut scrollback, &inline.frame(&frame));
    let recovered: String = scrollback
        .iter()
        .filter(|row| !row.is_empty() && row.as_str() != "status")
        .cloned()
        .collect();
    let long_word_length: usize = scrollback
        .iter()
        .filter(|row| !row.is_empty() && row.chars().all(|ch| ch == 'W'))
        .map(String::len)
        .sum();
    assert_eq!(long_word_length, 300);
    assert!(recovered.ends_with("END-LONG-WORD"), "{recovered:?}");
    assert_eq!(recovered, text.replace('\n', ""));
}

#[test]
fn indented_and_bracketed_entries_fit_twelve_columns_without_losing_text() {
    let text = "W".repeat(300) + "\nEND-LONG-WORD";
    let display = Display {
        width: 12,
        tool_lines: usize::MAX,
        ..Display::default()
    };
    for entry in [
        Entry::user(&text),
        Entry::reasoning(&text),
        Entry::tool(&text),
        Entry::notice(&text),
    ] {
        let rendered = transcript::render(&[entry], display);
        let plain = transcript::sanitize(&rendered);
        assert!(plain.lines().all(|row| rune_term::str_width(row) <= 12));
        let recovered: String = plain
            .chars()
            .filter(|ch| !matches!(ch, ' ' | '\n' | '>' | '[' | ']'))
            .collect();
        assert_eq!(recovered, text.replace('\n', ""));
    }
}
