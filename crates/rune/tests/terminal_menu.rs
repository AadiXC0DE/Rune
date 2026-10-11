//! Completion menu regressions through the real binary and PTYs.

#![cfg(unix)]
#![allow(clippy::expect_used)]

#[test]
fn an_unknown_tagged_model_can_be_picked_and_sent_while_invalid_ids_are_refused() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_menu.py"),
            env!("CARGO_BIN_EXE_rune"),
            "tagged-model",
        ])
        .output()
        .expect("python3 is required for the Unix tagged model regression");
    assert!(
        output.status.success(),
        "tagged model picker failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("terminal and request captures");
    let requests = result["requests"].as_array().expect("provider requests");
    assert!(!requests.is_empty());
    for request in requests {
        assert_eq!(request["model"], "qwen3:latest");
    }
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_value(result["captures"].clone()).expect("terminal captures");
    assert_eq!(captures.len(), 5);
    for (stage, bytes) in captures {
        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        grid.feed(&bytes).expect("frame");
        let screen = grid.text();
        let expected = match stage.as_str() {
            "empty-catalog" => "no match for ``",
            "invalid-tag" => "no match for `qwen3:`",
            "typed-tag" => "> use qwen3:latest (not listed)",
            "chosen-tag" => "model set to qwen3:latest",
            _ => "TAGGED-MODEL-REPLY",
        };
        assert!(screen.contains(expected), "{stage}: {screen}");
    }
}

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

#[test]
fn discovered_custom_slash_command_can_be_completed_and_expanded_in_a_terminal() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_menu.py"),
            env!("CARGO_BIN_EXE_rune"),
            "custom-completion",
        ])
        .output()
        .expect("python3 is required for the Unix custom completion regression");
    assert!(
        output.status.success(),
        "custom command completion failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&output.stdout).expect("terminal captures");
    assert_eq!(captures.len(), 5);
    for (stage, bytes) in captures {
        let mut grid = rune_term::Grid::new(80, 24).expect("grid");
        grid.feed(&bytes).expect("frame");
        let expected = match stage.as_str() {
            "custom-menu-tab" | "custom-menu-enter" => {
                assert!(
                    grid.text()
                        .contains("> /audit-command  Describe audit command files."),
                    "{stage}: {}",
                    grid.text()
                );
                "> /audit"
            }
            "custom-expanded" => {
                assert!(grid.text().contains("Describe audit command files."));
                assert!(
                    grid.text()
                        .contains("/audit-command expanded; edit before sending")
                );
                ">"
            }
            _ => "> /audit-command",
        };
        assert_eq!(grid.row_text(grid.cursor().row), expected, "{stage}");
        assert_eq!(
            usize::from(grid.cursor().col),
            expected.len().max(2),
            "{stage}"
        );
    }
}
