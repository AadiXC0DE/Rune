//! R-073 acceptance: read /help and exercise the advertised keys in a PTY.
//! Permission and question keys also run through the existing terminal_approval
//! and terminal_question PTY tests, including all three cancellation keys.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

fn run(args: &[&str]) -> Vec<(String, Vec<u8>, String, u16)> {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_help.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .args(args)
        .output()
        .expect("python3 is required for the Unix interactive help test");
    assert!(
        output.status.success(),
        "interactive help test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("PTY captures")
}

#[test]
fn every_help_binding_has_a_terminal_action_including_cancellation_and_paste() {
    let captures = run(&[]);
    assert!(captures.len() > 50);
    let mut viewer_rows = std::collections::BTreeMap::new();
    for (stage, bytes, text, column) in captures {
        let mut grid = rune_term::Grid::new(100, 24).expect("grid");
        let mut main = None;
        let mut start = 0;
        // Model the terminal's saved main screen across viewer/editor swaps.
        for at in 0..bytes.len().saturating_sub(7) {
            if bytes[at..].starts_with(b"\x1b[?1049h") || bytes[at..].starts_with(b"\x1b[?1049l") {
                grid.feed(&bytes[start..at]).expect("before screen swap");
                if bytes[at + 7] == b'h' {
                    main = Some(grid.clone());
                    grid = rune_term::Grid::new(100, 24).expect("alternate screen");
                } else {
                    grid = main.take().expect("saved main screen");
                }
                start = at + 8;
            }
        }
        grid.feed(&bytes[start..]).expect("captured output");
        if stage.starts_with("viewer-") && !stage.starts_with("viewer-close-") {
            assert!(main.is_some(), "{stage}");
            assert!(grid.text().contains("Transcript |"), "{stage}");
            viewer_rows.insert(stage, grid.row_text(0));
        } else {
            assert!(main.is_none(), "{stage}");
            assert_eq!(grid.row_text(grid.cursor().row), text.trim_end(), "{stage}");
            assert_eq!(grid.cursor().col, column, "{stage}");
            if stage == "paste" {
                assert_eq!(grid.row_text(grid.cursor().row - 2), "> xA");
                assert_eq!(grid.row_text(grid.cursor().row - 1), "  B C");
            }
            if stage == "newline" {
                assert_eq!(grid.row_text(grid.cursor().row - 1), "> first");
            }
        }
    }
    assert_ne!(viewer_rows["viewer-open"], viewer_rows["viewer-down"]);
    assert_eq!(viewer_rows["viewer-open"], viewer_rows["viewer-up"]);
    assert_ne!(viewer_rows["viewer-up"], viewer_rows["viewer-page-down"]);
    assert_eq!(viewer_rows["viewer-up"], viewer_rows["viewer-page-up"]);
    assert_ne!(viewer_rows["viewer-home"], viewer_rows["viewer-end"]);
    assert_eq!(viewer_rows["viewer-open"], viewer_rows["viewer-home"]);
}

#[test]
fn line_input_help_reports_the_active_mode() {
    assert!(run(&["--accessible"]).is_empty());
}
