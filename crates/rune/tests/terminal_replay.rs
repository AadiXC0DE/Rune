//! R-034: CI replays actual process output, including PTY resize boundaries.
//! Fixtures compare every screen row and the caret. The narrow fixture also
//! compares the answer recovered from scrollback, where clipping loses bytes.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

use std::fmt::Write as _;

use rune_term::Grid;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Capture {
    stage: String,
    cols: u16,
    rows: u16,
    bytes: Vec<u8>,
}

fn record(script: &str, scenario: Option<&str>) -> Vec<u8> {
    let mut command = std::process::Command::new("python3");
    command.args(["-c", script, env!("CARGO_BIN_EXE_rune")]);
    if let Some(scenario) = scenario {
        command.arg(scenario);
    }
    let output = command
        .output()
        .expect("python3 is required for the Unix PTY replay gate");
    assert!(
        output.status.success(),
        "PTY replay failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn screen_snapshot(stage: &str, cols: u16, rows: u16, grid: &Grid) -> String {
    let cursor = grid.cursor();
    let mut snapshot = format!(
        "{stage}: {cols}x{rows}, caret {},{}\n",
        cursor.col, cursor.row
    );
    for row in 0..rows {
        let mut line = grid.row_text(row);
        if let Some(id) = line.strip_prefix("session ") {
            line = "session ".to_owned() + &"*".repeat(id.len());
        }
        // The fixture's temporary path and session id vary each launch. Only
        // that suffix is masked; model, context, prompt and menu stay exact.
        let line = line
            .split_once(" | /")
            .map_or(line.as_str(), |(status, _)| status);
        writeln!(snapshot, "{row:02}|{line}|").expect("snapshot row");
    }
    snapshot
}

fn fixed_size_replay(script: &str, cols: u16, rows: u16) -> String {
    let captures: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&record(script, None)).expect("fixed-size captures");
    let mut snapshot = String::new();
    for (stage, bytes) in captures {
        let mut grid = Grid::new(cols, rows).expect("grid");
        grid.feed(&bytes).expect("replay");
        snapshot.push_str(&screen_snapshot(&stage, cols, rows, &grid));
    }
    snapshot
}

fn event_replay(scenario: &str) -> String {
    let captures: Vec<Capture> =
        serde_json::from_slice(&record(include_str!("terminal_replay.py"), Some(scenario)))
            .expect("event captures");
    let first = captures.first().expect("at least one capture");
    let mut grid = Grid::new(first.cols, first.rows).expect("grid");
    let mut snapshot = String::new();
    let mut scrollback = Vec::new();
    for capture in captures {
        grid.resize(capture.cols, capture.rows).expect("resize");
        for byte in capture.bytes {
            // Grid intentionally retains no scrollback. Preserve the row that
            // leaves the screen when a byte triggers a linefeed or autowrap.
            let top = grid.row_text(0);
            let stats = grid.feed(&[byte]).expect("replay byte");
            if stats.scrolled {
                assert_eq!(stats.scroll_rows, 1, "one byte scrolled multiple rows");
                scrollback.push(top);
            }
        }
        snapshot.push_str(&screen_snapshot(
            &capture.stage,
            capture.cols,
            capture.rows,
            &grid,
        ));
    }
    if scenario == "narrow" {
        let screen = grid.text();
        let answer: Vec<&str> = scrollback
            .iter()
            .map(String::as_str)
            .chain(screen.lines())
            .filter(|line| {
                (!line.is_empty() && line.chars().all(|c| c == 'W'))
                    || matches!(*line, "END-LONG-W" | "ORD")
            })
            .collect();
        assert_eq!(
            answer.concat(),
            "W".repeat(300) + "END-LONG-WORD",
            "lost or duplicated answer characters"
        );
        snapshot.push_str("answer rows in scrollback and screen:\n");
        for line in answer {
            writeln!(snapshot, "|{line}|").expect("answer row");
        }
    }
    snapshot
}

#[test]
fn four_row_compact_mode_message_matches_provider_traffic() {
    let captures: Vec<Capture> =
        serde_json::from_slice(&record(include_str!("terminal_replay.py"), Some("compact")))
            .expect("compact captures");
    assert_eq!(captures.len(), 3);
    let mut grid = Grid::new(80, 4).expect("grid");
    for capture in captures {
        assert_eq!((capture.cols, capture.rows), (80, 4));
        grid.feed(&capture.bytes).expect("replay");
        let screen = grid.text();
        assert_eq!(
            screen
                .matches("compact mode (4 rows): prompts work; resize for full interface")
                .count(),
            1,
            "{}: {screen}",
            capture.stage
        );
        assert!(!screen.contains("resize to continue"), "{screen}");
        let input = grid.row_text(grid.cursor().row);
        if capture.stage == "compact-draft" {
            assert_eq!(input, "> narrow height");
        } else {
            assert_eq!(input, ">");
        }
        if capture.stage == "compact-answer" {
            assert!(screen.contains("COMPACT-REPLY"), "{screen}");
        }
    }
}

#[test]
fn long_draft_grids() {
    let actual = fixed_size_replay(include_str!("terminal_draft.py"), 80, 24);
    assert_eq!(
        actual,
        include_str!("fixtures/terminal_replay/long_draft.grid")
    );
}

#[test]
fn short_menu_grids() {
    let actual = fixed_size_replay(include_str!("terminal_menu.py"), 32, 8);
    assert_eq!(
        actual,
        include_str!("fixtures/terminal_replay/short_menu.grid")
    );
    let resized = event_replay("menu-resize");
    assert_eq!(
        resized,
        include_str!("fixtures/terminal_replay/menu_resize.grid")
    );
}

#[test]
fn draft_resize_grids() {
    let actual = event_replay("resize");
    assert_eq!(
        actual,
        include_str!("fixtures/terminal_replay/draft_resize.grid")
    );
}

#[test]
fn twelve_column_text_grids_and_scrollback() {
    let actual = event_replay("narrow");
    assert_eq!(actual, include_str!("fixtures/terminal_replay/narrow.grid"));
}

#[test]
fn full_transcript_scrolls_tools_and_replies_and_preserves_the_draft_and_caret() {
    let captures: Vec<Capture> = serde_json::from_slice(&record(
        include_str!("terminal_replay.py"),
        Some("transcript"),
    ))
    .expect("transcript captures");
    let mut grid = Grid::new(80, 24).expect("grid");
    let mut main = None;
    let mut draft = None;
    let mut opened = None;
    for capture in captures {
        grid.resize(capture.cols, capture.rows).expect("resize");
        // Grid models one screen. Replay the terminal's alternate-screen swap
        // explicitly, retaining the main image and saved cursor as a real PTY
        // terminal does on DECSET/DECRST 1049.
        let bytes = capture.bytes.as_slice();
        let mut start = 0;
        for at in 0..bytes.len().saturating_sub(7) {
            let sequence = &bytes[at..at + 8];
            if sequence == b"\x1b[?1049h" || sequence == b"\x1b[?1049l" {
                grid.feed(&bytes[start..at]).expect("replay before swap");
                if sequence.ends_with(b"h") {
                    assert!(main.is_none(), "second alternate-screen owner");
                    main = Some(grid.clone());
                    grid = Grid::new(capture.cols, capture.rows).expect("alternate grid");
                } else {
                    grid = main.take().expect("main screen was saved");
                    grid.resize(capture.cols, capture.rows)
                        .expect("main resize");
                }
                start = at + 8;
            }
        }
        grid.feed(&bytes[start..]).expect("replay after swap");
        let text = grid.text();
        match capture.stage.as_str() {
            "draft" => draft = Some(grid.clone()),
            "opened" => {
                assert!(text.contains("> inspect"), "{text}");
                assert!(text.contains("read_file"), "{text}");
                opened = Some(grid.clone());
            }
            "page-down" => assert!(text.contains("TOOL-30"), "{text}"),
            "end" | "down" => assert!(text.contains("REPLY-40"), "{text}"),
            "up" => {
                assert!(text.contains("REPLY-17"), "{text}");
                assert!(!text.contains("REPLY-40"), "{text}");
            }
            "home" => assert_eq!(Some(&grid), opened.as_ref()),
            "closed" => assert_eq!(Some(&grid), draft.as_ref(), "draft screen or caret changed"),
            "edited" => assert!(text.contains("draft Z界 tail"), "{text}"),
            "resized" => assert!(text.contains("Transcript"), "{text}"),
            "live-end" => {
                assert!(text.contains("LIVE-PARTIAL"), "{text}");
                assert!(
                    !text.contains("LIVE-END"),
                    "snapshot contained future output"
                );
            }
            "live-closed" => assert!(text.contains("live draft"), "{text}"),
            "live-edited" => assert!(text.contains("live dZraft"), "{text}"),
            "empty" => {
                assert!(text.contains("Transcript"), "{text}");
                assert!(!text.contains("TOOL-"), "old tools leaked into new session");
                assert!(
                    !text.contains("REPLY-"),
                    "old replies leaked into new session"
                );
            }
            _ => {}
        }
    }
    assert!(main.is_none(), "viewer left the alternate screen open");
}

#[test]
fn fenced_code_continuations_are_stable_while_streaming_and_when_finished() {
    let captures: Vec<Capture> =
        serde_json::from_slice(&record(include_str!("terminal_replay.py"), Some("code")))
            .expect("code captures");
    let mut grid = Grid::new(32, 24).expect("grid");
    let mut scrollback = Vec::new();
    let first_row = "    abcdefghijklmnopqrstuvwxyz";
    let continuation = "    ↪ 0123456789  END-CODE";
    assert_eq!(captures.len(), 3);
    for capture in captures {
        for byte in capture.bytes {
            let top = grid.row_text(0);
            let stats = grid.feed(&[byte]).expect("replay byte");
            if stats.scrolled {
                scrollback.push(top);
            }
        }
        let screen = grid.text();
        let rows: Vec<&str> = scrollback
            .iter()
            .map(String::as_str)
            .chain(screen.lines())
            .collect();
        assert!(rows.contains(&first_row), "{}: {rows:?}", capture.stage);
        let expected = if capture.stage == "code-started" {
            "    ↪ 0123456789"
        } else {
            continuation
        };
        assert!(rows.contains(&expected), "{}: {rows:?}", capture.stage);
        if capture.stage == "code-finished" {
            assert!(rows.contains(&"AFTER-CODE"), "{rows:?}");
            assert_eq!(rows.iter().filter(|row| **row == first_row).count(), 1);
            assert_eq!(rows.iter().filter(|row| **row == continuation).count(), 1);
            assert_eq!(rows.iter().filter(|row| **row == "```").count(), 1);
        }
    }
}
