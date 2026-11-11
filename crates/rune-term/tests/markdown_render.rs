// Integration tests assert by panicking.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

//! Snapshots of answers as the transcript draws them.
//!
//! Each snapshot is the rows at a few widths with escapes shown as `\e`, so a
//! change in markers, indents or styling reads as a diff.

use std::fmt::Write as _;

use rune_term::markdown::MarkdownStyle;
use rune_term::transcript::AssistantRows;

/// Draws `text` at each width, one block of rows per width.
fn rows_at(text: &str, style: &MarkdownStyle, widths: &[usize]) -> String {
    let mut out = String::new();
    for width in widths {
        let _ = writeln!(out, "width {width}");
        for row in AssistantRows::with_markdown(style.clone()).rows(text, *width) {
            let _ = writeln!(out, "|{}", row.replace('\u{1b}', "\\e"));
        }
    }
    out
}

#[test]
fn lists_nest_and_hang() {
    let text = "\
- first level
  - second level
    - third level
1. ordered one
2) ordered two
   10. nested number that also runs long enough to wrap
- [ ] open task
- [x] done task
- [X] also done
- a long item whose text runs well past the narrow width and wraps under itself";
    insta::assert_snapshot!(rows_at(text, &MarkdownStyle::attributes(), &[20, 40, 80]));
}
