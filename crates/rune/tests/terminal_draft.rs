//! R-004 regression through the real binary and an 80-column PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

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
