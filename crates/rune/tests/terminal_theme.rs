//! A light terminal gets the light palette with no configuration, and the
//! theme picker previews each theme live and puts the first one back on Esc.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use rune_term::engine::Color;

const LIGHT_ACCENT: Color = Color::Rgb(0x09, 0x69, 0xda);

/// Returns the foreground of the model name in the status row after `bytes`.
fn model_colour(bytes: &[u8]) -> Color {
    let mut grid = rune_term::Grid::new(160, 24).expect("grid");
    grid.feed(bytes).expect("replay");
    for row in (0..24).rev() {
        let text = grid.row_text(row);
        if let Some(byte) = text.find("theme-fixture") {
            let col = u16::try_from(text[..byte].chars().count()).expect("column");
            return grid.cell(row, col).expect("cell").style.fg;
        }
    }
    panic!("no status row names the model:\n{}", grid.text());
}

#[test]
fn a_light_background_picks_the_light_theme_and_the_picker_previews() {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_theme.py")),
            env!("CARGO_BIN_EXE_rune"),
        ])
        .output()
        .expect("python3 is required for the Unix terminal theme test");
    assert!(
        output.status.success(),
        "terminal theme failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).expect("result");
    let bytes: Vec<u8> = result["bytes"]
        .as_array()
        .expect("bytes")
        .iter()
        .map(|byte| u8::try_from(byte.as_u64().expect("byte")).expect("byte"))
        .collect();
    let mark =
        |name: &str| usize::try_from(result["marks"][name].as_u64().expect("mark")).expect("mark");
    assert_eq!(model_colour(&bytes[..mark("first")]), LIGHT_ACCENT);
    let previewed = model_colour(&bytes[..mark("down")]);
    assert_ne!(previewed, LIGHT_ACCENT);
    assert_eq!(model_colour(&bytes[..mark("escape")]), LIGHT_ACCENT);
}
