//! The resume picker lists, filters and switches saved sessions in a terminal.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use rune_term::Grid;
use serde::Deserialize;

#[derive(Deserialize)]
struct Capture {
    stage: String,
    bytes: Vec<u8>,
}

/// Returns the scrollback and the screen after `bytes`.
fn screen(bytes: &[u8]) -> String {
    let mut grid = Grid::new(80, 24).expect("grid");
    let mut rows = Vec::new();
    for byte in bytes {
        let top = grid.row_text(0);
        let stats = grid.feed(&[*byte]).expect("terminal output");
        if stats.scrolled {
            rows.push(top);
        }
    }
    rows.extend((0..24).map(|row| grid.row_text(row)));
    rows.join("\n")
}

#[test]
fn the_resume_picker_filters_toggles_scope_and_switches_sessions() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_resume_picker.py")),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix resume picker test");
    assert!(
        output.status.success(),
        "resume picker test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<Capture> = serde_json::from_slice(&output.stdout).expect("captures");
    let stages: Vec<&str> = captures
        .iter()
        .map(|capture| capture.stage.as_str())
        .collect();
    assert_eq!(
        stages,
        [
            "picker-open",
            "picker-filtered",
            "picker-everywhere",
            "picker-back",
            "resumed",
            "slash-resumed"
        ]
    );
    for capture in &captures {
        let mut grid = Grid::new(80, 24).expect("grid");
        grid.feed(&capture.bytes).expect("terminal output");
        let shown = (0..24)
            .map(|row| grid.row_text(row))
            .collect::<Vec<_>>()
            .join("\n");
        let text = screen(&capture.bytes);
        match capture.stage.as_str() {
            "picker-open" => {
                assert!(shown.contains("sessions in this workspace"), "{shown}");
                assert!(shown.contains("beta task"), "{shown}");
                assert!(shown.contains("alpha task"), "{shown}");
                assert!(!shown.contains("other beta work"), "{shown}");
                assert!(
                    shown.find("beta task") < shown.find("alpha task"),
                    "{shown}"
                );
                assert!(shown.contains("> beta task"), "{shown}");
                assert!(shown.contains("REPLY beta task"), "{shown}");
            }
            "picker-filtered" => {
                assert!(shown.contains("beta task"), "{shown}");
                assert!(!shown.contains("alpha task"), "{shown}");
            }
            "picker-everywhere" => {
                assert!(shown.contains("sessions in every workspace"), "{shown}");
                assert!(shown.contains("other beta work"), "{shown}");
                assert!(shown.contains("beta task"), "{shown}");
                assert!(!shown.contains("alpha task"), "{shown}");
            }
            "picker-back" => {
                assert!(shown.contains("sessions in this workspace"), "{shown}");
                assert!(!shown.contains("other beta work"), "{shown}");
                assert!(shown.contains("beta task"), "{shown}");
            }
            "resumed" => {
                assert!(!shown.contains("sessions in"), "{shown}");
                assert_eq!(text.matches("REPLY beta task").count(), 1, "{text}");
                assert!(!shown.contains("alpha"), "{shown}");
            }
            _ => {
                assert!(shown.contains("resumed session"), "{shown}");
                assert!(shown.contains("(alpha task); /resume "), "{shown}");
                assert!(shown.contains("REPLY alpha task"), "{shown}");
                assert!(!shown.contains("sessions in this workspace"), "{shown}");
            }
        }
    }
}
