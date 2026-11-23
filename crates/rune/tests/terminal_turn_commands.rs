//! Slash commands during a turn, argument value menus and custom command
//! expansion, through the real binary and a PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

/// Runs one scenario and returns each captured stage as a drawn grid.
fn scenario(name: &str) -> Vec<(String, rune_term::Grid)> {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_turn_commands.py")),
            env!("CARGO_BIN_EXE_rune"),
            name,
        ])
        .output()
        .expect("python3 is required for the Unix turn commands test");
    assert!(
        output.status.success(),
        "turn commands scenario {name} failed:\n{}\n{}",
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

/// Returns the row the caret is on.
fn caret_row(grid: &rune_term::Grid) -> String {
    grid.row_text(grid.cursor().row)
}

#[test]
fn a_safe_command_runs_during_a_turn_and_another_waits() {
    let stages = scenario("turn");
    let (_, finished) = stages.last().expect("finished");
    let screen = finished.text();
    assert!(screen.contains("ANSWERED-first"), "{screen}");
    assert!(!screen.contains("steering: /status"), "{screen}");
    // The waiting command stays in the draft for after the turn.
    assert_eq!(caret_row(finished), "> /model", "{screen}");
}

#[test]
fn the_permissions_menu_offers_the_modes_and_sets_one() {
    let stages = scenario("values");
    let (_, menu) = &stages[0];
    let screen = menu.text();
    assert!(screen.lines().any(|row| row == "> ask"), "{screen}");
    assert!(
        screen.lines().any(|row| row == "  auto (current)"),
        "{screen}"
    );
    let (_, set) = &stages[1];
    let screen = set.text();
    assert!(screen.contains("permissions set to"), "{screen}");
    assert!(screen.contains("full"), "{screen}");
}

#[test]
fn a_custom_command_expands_into_the_draft() {
    let stages = scenario("expand");
    let (_, expanded) = &stages[0];
    let screen = expanded.text();
    assert_eq!(caret_row(expanded), "> say hello to Ana", "{screen}");
}
