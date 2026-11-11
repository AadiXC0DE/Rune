// Integration tests assert by panicking.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

//! Snapshots of answers as the transcript draws them.
//!
//! Each snapshot is the rows at a few widths with escapes shown as `\e`, so a
//! change in markers, indents or styling reads as a diff.

use std::fmt::Write as _;

use rune_term::engine::Style;
use rune_term::highlight::CodeStyle;
use rune_term::markdown::{LinkContext, MarkdownStyle, STRIKE};
use rune_term::theme::{ColorDepth, Slot, Theme};
use rune_term::transcript::{AssistantRows, Display, Entry, Lanes, render_lanes};

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

/// The answer styling a session draws with the dark theme at `depth`.
fn session_style(depth: ColorDepth) -> MarkdownStyle {
    let theme = Theme::dark();
    let sgr = |slot| theme.sgr(slot, depth);
    MarkdownStyle {
        enabled: true,
        heading: format!("\u{1b}[1m{}", sgr(Slot::Heading)),
        bold: "\u{1b}[1m".to_owned(),
        italic: "\u{1b}[3m".to_owned(),
        code: sgr(Slot::String),
        strike: STRIKE.to_owned(),
        link: format!("{}\u{1b}[4m", sgr(Slot::Link)),
        dim: sgr(Slot::Dim),
        reset: Style::RESET.to_owned(),
        code_block: CodeStyle {
            keyword: sgr(Slot::Keyword),
            string: sgr(Slot::String),
            number: sgr(Slot::Number),
            comment: sgr(Slot::Comment),
            function: sgr(Slot::Function),
            reset: Style::RESET.to_owned(),
        },
        code_bg: sgr(Slot::CodeBg),
        links: LinkContext {
            // Linked at full colour, written out where colour is off.
            hyperlinks: depth != ColorDepth::None,
            ..LinkContext::default()
        },
    }
}

#[test]
fn every_element_at_three_widths_and_depths() {
    let text = "\
# Release notes ##

A paragraph with **strong**, *emphasis* and `code` in it.
See [the guide](https://example.test/guide) or https://example.test/raw.
![diagram](https://example.test/d.png) and ~~struck~~ text.

> A quoted line long enough that it wraps onto a second row.

---

- outer item
  - inner item
    - innermost item
- [ ] open task
- [x] done task

```rust
fn main() {
    let total = 42; // the answer
}
```

| key | value |
|---|---|
| a | 1 |
| bb | 22 |

| name | role | note |
|---|:-:|---|
| ada | lead | wrote the first program for an engine |
| bo | dev | short |

<details>raw html</details>";
    let mut out = String::new();
    for (name, depth) in [
        ("truecolor", ColorDepth::TrueColor),
        ("ansi16", ColorDepth::Ansi16),
        ("none", ColorDepth::None),
    ] {
        let lanes = Lanes {
            markdown: session_style(depth),
            ..Lanes::default()
        };
        for width in [20, 40, 80] {
            let display = Display {
                width,
                ..Display::default()
            };
            let _ = writeln!(out, "{name} width {width}");
            for row in render_lanes(&[Entry::assistant(text)], display, &lanes).lines() {
                let _ = writeln!(out, "|{}", row.replace('\u{1b}', "\\e"));
            }
        }
    }
    insta::assert_snapshot!(out);
}
