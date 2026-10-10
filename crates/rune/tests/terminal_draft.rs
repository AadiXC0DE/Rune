//! Draft editing regressions through the real binary and an 80-column PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

#[test]
fn alt_enter_sends_one_two_line_prompt_with_an_exact_newline_to_the_provider() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_draft.py"),
            env!("CARGO_BIN_EXE_rune"),
            "newline",
        ])
        .output()
        .expect("python3 is required for the Unix terminal newline test");
    assert!(
        output.status.success(),
        "terminal newline test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&output.stdout).expect("terminal captures");
    assert_eq!(captures.len(), 3);
    for (stage, bytes) in captures {
        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        grid.feed(&bytes).expect("feed");
        let caret = grid.cursor();
        let input = grid.row_text(caret.row);
        let expected = match stage.as_str() {
            "first-line" => "> first 界 line",
            "newline" => "> first 界 line⏎",
            "second-line" => "> first 界 line⏎second é line",
            _ => panic!("unexpected capture {stage}"),
        };
        assert_eq!(input, expected, "{stage}");
        assert_eq!(
            usize::from(caret.col),
            rune_term::width::str_width(expected),
            "{stage}"
        );
    }
}

#[test]
fn redo_restores_unicode_drafts_and_carets_in_a_real_terminal() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_draft.py"),
            env!("CARGO_BIN_EXE_rune"),
            "redo",
        ])
        .output()
        .expect("python3 is required for the Unix terminal redo regression");
    assert!(
        output.status.success(),
        "terminal redo regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&output.stdout).expect("terminal captures");
    assert_eq!(captures.len(), 5);
    for (stage, bytes) in captures {
        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        grid.feed(&bytes).expect("feed");
        let screen = grid.text();
        let caret = grid.cursor();
        let input = screen
            .lines()
            .nth(usize::from(caret.row))
            .expect("input row");
        let (text, column) = match stage.as_str() {
            "original" | "undone" => ("> 界ab", 5),
            "inserted" | "redone" => ("> 界ae\u{301}b", 6),
            "moved" => ("> 界ab", 2),
            _ => panic!("unexpected capture {stage}"),
        };
        assert_eq!(input, text, "{stage}: {screen}");
        assert_eq!(caret.col, column, "{stage}: {screen}");
    }
}

#[test]
fn undo_restores_unicode_drafts_and_carets_in_a_real_terminal() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_draft.py"),
            env!("CARGO_BIN_EXE_rune"),
            "undo",
        ])
        .output()
        .expect("python3 is required for the Unix terminal undo regression");
    assert!(
        output.status.success(),
        "terminal undo regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&output.stdout).expect("terminal captures");
    assert_eq!(captures.len(), 6);
    for (stage, bytes) in captures {
        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        grid.feed(&bytes).expect("feed");
        let screen = grid.text();
        let caret = grid.cursor();
        let input = screen
            .lines()
            .nth(usize::from(caret.row))
            .expect("input row");
        let (text, column) = match stage.as_str() {
            "original" | "deleted" | "undo-insert" => ("> 界ab", 5),
            "inserted" | "undo-delete" => ("> 界ae\u{301}b", 6),
            "moved" => ("> 界ab", 2),
            _ => panic!("unexpected capture {stage}"),
        };
        assert_eq!(input, text, "{stage}: {screen}");
        assert_eq!(caret.col, column, "{stage}: {screen}");
    }
}

#[test]
fn long_drafts_scroll_with_the_caret_in_a_real_terminal() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_draft.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix terminal draft regression");
    assert!(
        output.status.success(),
        "terminal draft regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&output.stdout).expect("terminal captures");
    assert_eq!(captures.len(), 5);
    for (stage, bytes) in captures {
        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        grid.feed(&bytes).expect("feed");
        let screen = grid.text();
        let caret = grid.cursor();
        let input = screen
            .lines()
            .nth(usize::from(caret.row))
            .expect("input row");
        assert!(input.starts_with("> "), "{stage}: {screen}");
        match stage.as_str() {
            "end" | "left" => {
                assert!(input.contains("TAIL-END"), "{stage}: {screen}");
                assert_eq!(caret.col, 79, "{stage}: {screen}");
                if stage == "left" {
                    assert_eq!(input.chars().nth(usize::from(caret.col)), Some('D'));
                }
            }
            "edited" | "end-again" => {
                assert!(input.contains("TAIL-ENZ"), "{stage}: {screen}");
                assert_eq!(caret.col, 79, "{stage}: {screen}");
            }
            "home" => {
                assert_eq!(input, "> ".to_owned() + &"a".repeat(78));
                assert_eq!(caret.col, 2);
            }
            _ => panic!("unexpected capture {stage}"),
        }
    }
}
