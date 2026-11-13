//! Large pastes held as chips, and pastes that arrive as keystrokes, through
//! the real binary and a PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

/// Runs one scenario and returns each captured stage as a drawn grid.
fn scenario(name: &str) -> Vec<(String, rune_term::Grid)> {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_paste.py")),
            env!("CARGO_BIN_EXE_rune"),
            name,
        ])
        .output()
        .expect("python3 is required for the Unix paste test");
    assert!(
        output.status.success(),
        "paste scenario {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&output.stdout).expect("captures");
    captures
        .into_iter()
        .map(|(stage, bytes)| {
            let mut grid = rune_term::Grid::new(100, 24).expect("grid");
            grid.feed(&bytes).expect("frame");
            (stage, grid)
        })
        .collect()
}

/// Returns the row `above` rows over the caret.
fn row_over_caret(grid: &rune_term::Grid, above: u16) -> String {
    grid.row_text(grid.cursor().row.saturating_sub(above))
}

#[test]
fn a_large_paste_is_drawn_as_a_chip_and_sent_whole() {
    let stages = scenario("chip");
    assert_eq!(stages.len(), 1);
    let (_, pasted) = &stages[0];
    let screen = pasted.text();
    assert_eq!(row_over_caret(pasted, 0), "> [pasted #1: 5000 lines]");
    assert!(!screen.contains("line 4999"), "{screen}");
}

#[test]
fn a_burst_with_an_enter_inside_does_not_submit() {
    let stages = scenario("burst");
    assert_eq!(stages.len(), 1);
    let (_, typed) = &stages[0];
    assert_eq!(row_over_caret(typed, 0), "  cd");
    assert_eq!(row_over_caret(typed, 1), "> ab");
}
