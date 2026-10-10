//! Draft editing regressions through the real binary and an 80-column PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

#[test]
fn external_editor_reloads_the_draft_and_restores_the_terminal_on_success_and_failure() {
    for scenario in [
        "editor",
        "editor-fallback",
        "editor-steering",
        "editor-failed",
        "editor-invalid",
        "editor-unchanged",
        "editor-missing",
    ] {
        let output = std::process::Command::new("python3")
            .args([
                "-c",
                include_str!("terminal_draft.py"),
                env!("CARGO_BIN_EXE_rune"),
                scenario,
            ])
            .output()
            .expect("python3 is required for the Unix editor test");
        assert!(
            output.status.success(),
            "{scenario}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let captures: Vec<(String, Vec<u8>)> =
            serde_json::from_slice(&output.stdout).expect("captures");
        let changed = matches!(scenario, "editor" | "editor-fallback" | "editor-steering");
        assert_eq!(captures.len(), if changed { 5 } else { 3 });
        for (stage, bytes) in captures {
            let mut grid = rune_term::Grid::new(80, 24).expect("grid");
            let mut main = None;
            let mut start = 0;
            for at in 0..bytes.len().saturating_sub(7) {
                let sequence = &bytes[at..at + 8];
                if sequence == b"\x1b[?1049h" || sequence == b"\x1b[?1049l" {
                    grid.feed(&bytes[start..at]).expect("before swap");
                    if sequence.ends_with(b"h") {
                        main = Some(grid.clone());
                        grid = rune_term::Grid::new(80, 24).expect("editor grid");
                    } else {
                        grid = main.take().expect("saved main screen");
                    }
                    start = at + 8;
                }
            }
            grid.feed(&bytes[start..]).expect("after swap");
            assert!(
                main.is_none(),
                "{scenario}: editor did not restore main screen"
            );
            let (expected, column) = match stage.as_str() {
                "original" | "undone" => ("> draft 界", 8),
                "returned" | "redone" if changed => ("  second 界", 11),
                "returned" => ("> draft 界", 8),
                "typed" if changed => ("  second 界!", 12),
                "typed" => ("> draft !界", 9),
                _ => panic!("unexpected stage {stage}"),
            };
            assert_eq!(
                grid.row_text(grid.cursor().row),
                expected,
                "{scenario}/{stage}"
            );
            assert_eq!(grid.cursor().col, column, "{scenario}/{stage}");
            assert_eq!(
                grid.text().matches("ctrl-c cancel").count(),
                1,
                "{scenario}/{stage}"
            );
        }
    }
}

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
            "newline" => "",
            "second-line" => "  second é line",
            _ => panic!("unexpected capture {stage}"),
        };
        assert_eq!(input, expected, "{stage}");
        assert_eq!(
            usize::from(caret.col),
            rune_term::width::str_width(expected).max(2),
            "{stage}"
        );
        if stage != "first-line" {
            assert_eq!(grid.row_text(caret.row - 1), "> first 界 line", "{stage}");
        }
    }
}

#[test]
fn pasted_multiline_drafts_show_each_edited_line_and_its_terminal_caret() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_draft.py"),
            env!("CARGO_BIN_EXE_rune"),
            "multiline",
        ])
        .output()
        .expect("python3 is required for the Unix multiline draft test");
    assert!(
        output.status.success(),
        "multiline draft test failed:\n{}\n{}",
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
        let rows: Vec<&str> = screen.lines().collect();
        let first = rows
            .iter()
            .position(|row| row.starts_with("> first 界"))
            .expect("first draft row");
        let (edited_row, column) = match stage.as_str() {
            "pasted" => (2, 10),
            "third-edited" | "end-again" => (2, 11),
            "second-edited" => (1, 11),
            "first-edited" => (0, 11),
            _ => panic!("unexpected capture {stage}"),
        };
        assert_eq!(
            rows[first],
            if stage == "first-edited" || stage == "end-again" {
                "> first 界Y"
            } else {
                "> first 界"
            },
            "{stage}"
        );
        assert_eq!(
            rows[first + 1],
            if matches!(
                stage.as_str(),
                "second-edited" | "first-edited" | "end-again"
            ) {
                "  second e\u{301}X"
            } else {
                "  second e\u{301}"
            },
            "{stage}"
        );
        assert_eq!(
            rows[first + 2],
            if stage == "pasted" {
                "  third 👩‍💻"
            } else {
                "  third 👩‍💻Z"
            },
            "{stage}"
        );
        assert_eq!(
            usize::from(grid.cursor().row),
            first + edited_row,
            "{stage}: {screen}"
        );
        assert_eq!(grid.cursor().col, column, "{stage}: {screen}");
        assert_eq!(
            screen.matches("ctrl-c cancel").count(),
            1,
            "{stage}: {screen}"
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
