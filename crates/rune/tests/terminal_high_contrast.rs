//! R-071 acceptance through the real binary in truecolor and indexed PTYs.

#![cfg(unix)]
#![allow(clippy::expect_used)]

#[test]
fn high_contrast_draws_explicit_pairs_and_no_color_suppresses_them() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("terminal_high_contrast.py"),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix high-contrast terminal test");
    assert!(
        output.status.success(),
        "high-contrast terminal acceptance failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&output.stdout).expect("terminal captures");
    assert_eq!(captures.len(), 4);
    for (stage, bytes) in captures {
        let mut grid = rune_term::Grid::new(100, 24).expect("grid");
        grid.feed(&bytes).expect("terminal output");
        assert!(grid.text().contains("ctrl-c cancel"), "{stage}");
        assert!(grid.text().contains("/model"), "{stage}");
        let colorless = stage.starts_with("no-color");
        let truecolor = stage == "truecolor";
        let theme = if colorless {
            rune_term::theme::Theme::no_color()
        } else {
            rune_term::theme::Theme::high_contrast()
        };
        let mut colored_cells = 0_usize;
        for row in 0..24 {
            for col in 0..100 {
                let style = grid.cell(row, col).expect("cell").style;
                if colorless {
                    assert_eq!(style.fg, rune_term::Color::Default, "{stage}");
                    assert_eq!(style.bg, rune_term::Color::Default, "{stage}");
                } else if style.fg != rune_term::Color::Default {
                    colored_cells = colored_cells.saturating_add(1);
                    assert!(!style.has_flag(rune_term::flag::DIM), "{stage}");
                    assert!(
                        rune_term::theme::SLOTS.iter().any(|slot| {
                            let pair = theme.style(*slot, truecolor);
                            pair.fg == style.fg && pair.bg == style.bg
                        }),
                        "unexpected pair in {stage}: {style:?}"
                    );
                }
            }
        }
        if !colorless {
            assert!(colored_cells > 30, "theme was not applied in {stage}");
        }
    }
}
