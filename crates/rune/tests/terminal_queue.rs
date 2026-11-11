//! Follow-up messages queued while a turn runs, through the real binary and a PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

/// Runs one scenario and returns each captured stage as a drawn grid.
fn scenario(name: &str) -> Vec<(String, rune_term::Grid)> {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_queue.py")),
            env!("CARGO_BIN_EXE_rune"),
            name,
        ])
        .output()
        .expect("python3 is required for the Unix queue test");
    assert!(
        output.status.success(),
        "queue scenario {name} failed:\n{}\n{}",
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
fn queued_messages_run_in_order_once_the_turn_ends() {
    let stages = scenario("order");
    assert_eq!(stages.len(), 2);
    let (_, queued) = &stages[0];
    assert_eq!(row_over_caret(queued, 0), ">");
    assert_eq!(row_over_caret(queued, 1), "queued 2: third");
    assert_eq!(row_over_caret(queued, 2), "queued 1: second");
    let (_, finished) = &stages[1];
    let screen = finished.text();
    assert!(!screen.contains("queued 1:"), "{screen}");
    assert!(screen.contains("ANSWERED-second"), "{screen}");
    assert_eq!(row_over_caret(finished, 0), ">");
}

#[test]
fn alt_up_takes_back_steering_then_the_queued_message() {
    let stages = scenario("pull");
    assert_eq!(stages.len(), 2);
    let (_, steering) = &stages[0];
    let screen = steering.text();
    assert!(!screen.contains("steering: now"), "{screen}");
    assert_eq!(row_over_caret(steering, 0), "> now");
    assert_eq!(row_over_caret(steering, 1), "queued 1: later");
    let (_, queued) = &stages[1];
    let screen = queued.text();
    assert!(!screen.contains("queued 1:"), "{screen}");
    assert_eq!(row_over_caret(queued, 0), "  later");
    assert_eq!(row_over_caret(queued, 1).trim(), "");
    assert_eq!(row_over_caret(queued, 2), "> now");
}

#[test]
fn an_interrupt_returns_the_queue_to_the_draft() {
    let stages = scenario("interrupt");
    assert_eq!(stages.len(), 1);
    let (_, interrupted) = &stages[0];
    let screen = interrupted.text();
    assert!(!screen.contains("queued 1:"), "{screen}");
    assert_eq!(row_over_caret(interrupted, 0), "> x");
}

#[test]
fn a_rebound_queue_key_queues_and_tab_does_not() {
    let stages = scenario("rebound");
    assert_eq!(stages.len(), 1);
    let (_, queued) = &stages[0];
    assert_eq!(row_over_caret(queued, 0), ">");
    assert_eq!(row_over_caret(queued, 1), "queued 1: second");
}
