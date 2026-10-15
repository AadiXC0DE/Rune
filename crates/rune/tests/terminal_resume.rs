//! R-038 and R-039: saved exchanges and context estimates appear before resume input.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

use rune_term::Grid;
use serde::Deserialize;

#[derive(Deserialize)]
struct Capture {
    stage: String,
    cols: u16,
    bytes: Vec<u8>,
}

#[test]
fn interactive_resume_replays_saved_exchanges_once_before_input() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_resume.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix terminal resume regression");
    assert!(
        output.status.success(),
        "terminal resume regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<Capture> = serde_json::from_slice(&output.stdout).expect("resume captures");
    assert_eq!(captures.len(), 5);
    for capture in captures {
        let mut grid = Grid::new(capture.cols, 24).expect("grid");
        let mut rows = Vec::new();
        // Grid keeps only the screen. Save each row leaving it to reconstruct
        // terminal scrollback, just as the existing PTY replay gate does.
        for byte in capture.bytes {
            let top = grid.row_text(0);
            let stats = grid.feed(&[byte]).expect("resume output");
            if stats.scrolled {
                assert_eq!(stats.scroll_rows, 1);
                rows.push(top);
            }
        }
        let scrolled = !rows.is_empty();
        rows.extend((0..24).map(|row| grid.row_text(row)));
        let text = rows.join("\n");
        let caret = grid.cursor();
        if capture.stage.starts_with("resume-") && capture.cols == 80 {
            assert!(text.contains("ctx ~1.2k (saved usage)"), "{text}");
            assert!(!text.contains("ctx 0%"), "{text}");
        }
        if capture.stage == "new-session-before-input" {
            assert!(!text.contains("SAVED-REPLY"), "{text}");
            assert!(!text.contains("saved prompt"), "{text}");
        } else if capture.stage == "resume-long-before-input" {
            assert_eq!(text.matches("> long saved prompt").count(), 1, "{text}");
            for index in 1..=40 {
                assert_eq!(
                    text.matches(&format!("SAVED-ROW-{index:02}")).count(),
                    1,
                    "{text}"
                );
            }
            assert!(scrolled);
        } else {
            assert_eq!(text.matches("> saved prompt").count(), 1, "{text}");
            for line in ["SAVED-REPLY", "Unicode 世界 e\u{301}", "REPLY-END"] {
                assert_eq!(text.matches(line).count(), 1, "{text}");
            }
            assert!(text.find("> saved prompt") < text.find("SAVED-REPLY"));
            assert!(text.find("SAVED-REPLY") < text.find("REPLY-END"));
            if capture.stage == "resume-id-before-input" {
                assert_eq!(text.matches("> continue saved prompt").count(), 1, "{text}");
                assert_eq!(text.matches("CONTINUED-REPLY").count(), 1, "{text}");
                assert!(text.find("REPLY-END") < text.find("> continue saved prompt"));
            }
        }
        let draft = if capture.stage == "resume-last-draft" {
            "> draft"
        } else {
            "> "
        };
        assert_eq!(grid.row_text(caret.row), draft.trim_end(), "{text}");
        assert_eq!(usize::from(caret.col), draft.len(), "{text}");
    }
}
