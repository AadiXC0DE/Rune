//! R-011 regression through the real binary and a 32x8 PTY.

#![cfg(unix)]
#![allow(clippy::expect_used)]

#[test]
fn completion_and_model_menus_fit_a_short_terminal_and_close_cleanly() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_menu.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix terminal menu regression");
    assert!(
        output.status.success(),
        "terminal menu regression failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&output.stdout).expect("terminal captures");
    assert_eq!(captures.len(), 4);
    for (stage, bytes) in captures {
        let mut grid = rune_term::Grid::new(32, 8).expect("grid");
        grid.feed(&bytes).expect("feed");
        let screen = grid.text();
        let input = screen
            .lines()
            .nth(usize::from(grid.cursor().row))
            .expect("input row");
        assert!(matches!(input, ">" | "> /"), "{stage}: {screen}");
        assert_eq!(
            screen.matches("ctrl-c cancel").count(),
            1,
            "{stage}: {screen}"
        );
        assert_eq!(screen.matches("| auto |").count(), 1, "{stage}: {screen}");
        match stage.as_str() {
            "completion-last" => assert!(screen.contains("> /quit"), "{screen}"),
            "model-last" => assert!(screen.contains("> menu-model-15"), "{screen}"),
            _ => {
                assert_eq!(input, ">", "{stage}: {screen}");
                assert!(!screen.contains("models from"), "{stage}: {screen}");
                assert!(!screen.contains("type to narrow"), "{stage}: {screen}");
                assert!(!screen.contains(" of "), "{stage}: {screen}");
                assert!(!screen.contains("menu-model-15"), "{stage}: {screen}");
                assert!(!screen.contains("/quit"), "{stage}: {screen}");
            }
        }
    }
}
