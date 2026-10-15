//! R-115: token provenance before and after fixture responses in the real TUI.

#![cfg(unix)]
#![allow(clippy::expect_used)]

#[test]
fn footer_and_status_distinguish_provider_usage_from_estimates() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_token_provenance.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix token provenance test");
    assert!(
        output.status.success(),
        "token provenance fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&output.stdout).expect("captures");
    assert_eq!(captures.len(), 8);
    let mut grid = rune_term::Grid::new(120, 24).expect("grid");
    for (stage, bytes) in captures {
        grid.feed(&bytes).expect("replay");
        let screen = grid.text();
        let footer = screen
            .lines()
            .find(|row| row.contains("provenance-fixture | auto | ctx"))
            .expect("footer");
        if stage.starts_with("provider") {
            assert!(footer.contains("(provider-reported)"), "{stage}: {screen}");
            assert!(!footer.contains('~'), "{stage}: {screen}");
            if stage.ends_with("status") {
                assert!(
                    screen.contains("context source provider-reported"),
                    "{screen}"
                );
                assert!(screen.contains("context        1.3k"), "{screen}");
            }
        } else {
            assert!(footer.contains("ctx ~"), "{stage}: {screen}");
            assert!(footer.contains("(request bytes)"), "{stage}: {screen}");
            assert!(!footer.contains("provider-reported"), "{stage}: {screen}");
            if stage.ends_with("status") {
                assert!(
                    screen.contains("context source request bytes (estimate)"),
                    "{screen}"
                );
            }
        }
    }
}
