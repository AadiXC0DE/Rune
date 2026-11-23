//! The transcript viewer follows a streaming turn, searches and expands
//! stored results, through the real binary and a PTY.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

/// Runs one scenario and returns each captured stage as a drawn grid.
fn scenario(name: &str) -> Vec<(String, rune_term::Grid)> {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_viewer.py")),
            env!("CARGO_BIN_EXE_rune"),
            name,
        ])
        .output()
        .expect("python3 is required for the Unix viewer test");
    assert!(
        output.status.success(),
        "viewer scenario {name} failed:\n{}\n{}",
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

/// Returns the grid captured at a stage.
fn stage<'a>(stages: &'a [(String, rune_term::Grid)], name: &str) -> &'a rune_term::Grid {
    &stages
        .iter()
        .find(|(stage, _)| stage == name)
        .unwrap_or_else(|| panic!("no {name} stage"))
        .1
}

#[test]
fn the_viewer_follows_a_streaming_answer_and_searches() {
    let stages = scenario("live");
    let opened = stage(&stages, "opened").text();
    assert!(opened.contains("EARLY-29"), "{opened}");
    assert!(opened.contains("/ search   n/N next"), "{opened}");
    let followed = stage(&stages, "followed").text();
    assert!(followed.contains("LIVE-ONE"), "{followed}");
    let below = stage(&stages, "below").text();
    assert!(below.contains("new lines below, G to follow"), "{below}");
    assert!(!below.contains("LATER-11"), "{below}");
    let bottom = stage(&stages, "bottom").text();
    assert!(bottom.contains("LATER-11"), "{bottom}");
    assert!(!bottom.contains("new lines below"), "{bottom}");
    let searched = stage(&stages, "searched");
    assert_eq!(searched.row_text(0), "match 1/1 for \"NEEDLE\"");
    assert!(searched.text().contains("NEEDLE first"));
}

#[test]
fn an_expanded_result_shows_the_whole_file() {
    let stages = scenario("result");
    let expanded = stage(&stages, "expanded").text();
    assert!(expanded.contains("LAST-LINE-OF-FILE"), "{expanded}");
    let middle = stage(&stages, "middle");
    assert_eq!(middle.row_text(0), "match 1/1 for \"01200\"");
    assert!(middle.text().contains("file line 01200"));
}
