//! R-075: retry status through the real binary, HTTP transport, and renderer.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

#[test]
fn two_provider_failures_show_pending_and_active_retries_in_one_status_row() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_retry.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix terminal retry test");
    assert!(
        output.status.success(),
        "terminal retry failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&output.stdout).expect("captures");
    assert_eq!(captures.len(), 5);
    let mut grid = rune_term::Grid::new(80, 24).expect("grid");
    for (status, bytes) in captures {
        grid.feed(&bytes).expect("replay");
        let screen = grid.text();
        assert_eq!(screen.matches("ctrl-c cancel").count(), 1, "{screen}");
        assert_eq!(screen.matches("| auto |").count(), 1, "{screen}");
        if status == "completed" {
            assert!(!screen.contains("provider retry"), "{screen}");
            assert!(screen.contains("RETRY-RECOVERED"), "{screen}");
        } else {
            assert_eq!(screen.matches("provider retry").count(), 1, "{screen}");
            assert!(screen.contains(&status), "{screen}");
            let row = screen
                .lines()
                .find(|row| row.contains("provider retry"))
                .expect("retry row");
            assert!(
                row.contains("| auto |"),
                "retry escaped the status row: {screen}"
            );
        }
    }
}
