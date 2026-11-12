//! R-034: CI replays actual process output, including PTY resize boundaries.
//! Fixtures compare every screen row and the caret. The narrow fixture also
//! compares the answer recovered from scrollback, where clipping loses bytes.
//! Normalize CRLF in checked-in grids; all screen content stays exact.
//!
//! Updating a grid: run the test with RUNE_BLESS_GRIDS=1, for example
//! `RUNE_BLESS_GRIDS=1 cargo test -p rune --test terminal_replay`. Each changed fixture
//! is rewritten and its changed lines are printed. Review the fixture diff with
//! `git diff crates/rune/tests/fixtures` before committing; a grid that changed
//! without a reason is a regression, not a new baseline.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

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
    command.args([
        "-c",
        &support::python_script(script),
        env!("CARGO_BIN_EXE_rune"),
    ]);
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

fn fixture_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/terminal_replay")
        .join(name)
}

/// Compares a replayed grid with its fixture, or rewrites the fixture when
/// `RUNE_BLESS_GRIDS` is exactly `1`.
fn compare_grid(name: &str, actual: &str) {
    let path = fixture_path(name);
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    let old: Vec<&str> = expected.split_inclusive('\n').collect();
    let new: Vec<&str> = actual.split_inclusive('\n').collect();
    let total = old.len().max(new.len());
    let mut changed = 0_usize;
    let mut pairs = String::new();
    for index in 0..total {
        let (before, after) = (old.get(index), new.get(index));
        if before == after {
            continue;
        }
        changed = changed.saturating_add(1);
        if let Some(line) = before {
            writeln!(pairs, "-{}", line.trim_end_matches('\n')).expect("diff row");
        }
        if let Some(line) = after {
            writeln!(pairs, "+{}", line.trim_end_matches('\n')).expect("diff row");
        }
    }
    let bless = std::env::var_os("RUNE_BLESS_GRIDS").is_some_and(|value| value == "1");
    if bless {
        if changed > 0 {
            std::fs::write(&path, actual).expect("write blessed grid");
        }
        eprintln!("blessed {name}: {changed} of {total} lines differ\n{pairs}");
        return;
    }
    assert!(
        changed == 0,
        "{name}: {changed} of {total} lines differ\n{pairs}run with RUNE_BLESS_GRIDS=1 to accept the new grid, then review the fixture diff"
    );
}

/// Returns what follows an elapsed time that is `run` plus the start of `tail`.
fn after_duration<'a>(run: &str, tail: &'a str) -> Option<&'a str> {
    let number = run.bytes().filter(|byte| *byte == b'.').count() <= 1;
    let clock = tail
        .strip_prefix('m')
        .filter(|_| !run.contains('.'))
        .and_then(|seconds| {
            let two = seconds.get(..2)?;
            let rest = seconds.get(2..)?;
            two.bytes()
                .all(|byte| byte.is_ascii_digit())
                .then_some(rest)?
                .strip_prefix('s')
        });
    let unit = clock.or_else(|| {
        number
            .then(|| tail.strip_prefix("ms").or_else(|| tail.strip_prefix('s')))
            .flatten()
    })?;
    (!unit.starts_with(|c: char| c.is_ascii_alphanumeric())).then_some(unit)
}

/// Replaces each elapsed time with `<t>`, since it differs between runs.
fn mask_durations(line: &str) -> String {
    let mut masked = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find(|c: char| c.is_ascii_digit()) {
        let (before, from_digit) = rest.split_at(start);
        masked.push_str(before);
        let past_figures = from_digit.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
        let figures = from_digit.len().saturating_sub(past_figures.len());
        let run = from_digit
            .get(..figures)
            .unwrap_or_default()
            .trim_end_matches('.');
        let (number, tail) = from_digit.split_at(run.len());
        rest = if let Some(after) = after_duration(number, tail) {
            masked.push_str("<t>");
            after
        } else {
            masked.push_str(number);
            tail
        };
    }
    masked.push_str(rest);
    masked
}

/// Replaces each figure of the context estimate and of the room left with `#`.
///
/// The estimate counts the system prompt, which holds the temporary workspace
/// path, and that path is longer on some platforms than on others. The width
/// is kept, so the layout of the status line is still compared exactly.
fn mask_estimates(line: &str) -> String {
    let figure = |c: char| c.is_ascii_digit() || c == '.';
    let hidden = |count: usize| "#".repeat(count);
    let mut after_estimate = String::with_capacity(line.len());
    for (index, piece) in line.split("ctx ~").enumerate() {
        if index == 0 {
            after_estimate.push_str(piece);
            continue;
        }
        let tail = piece.trim_start_matches(figure);
        after_estimate.push_str("ctx ~");
        after_estimate.push_str(&hidden(piece.len().saturating_sub(tail.len())));
        after_estimate.push_str(tail);
    }
    let pieces: Vec<&str> = after_estimate.split("k left)").collect();
    let mut masked = String::with_capacity(line.len());
    for (index, piece) in pieces.iter().enumerate() {
        if index.saturating_add(1) == pieces.len() {
            masked.push_str(piece);
            break;
        }
        let head = piece.trim_end_matches(figure);
        masked.push_str(head);
        masked.push_str(&hidden(piece.len().saturating_sub(head.len())));
        masked.push_str("k left)");
    }
    masked
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
        // An approval names the fixture's temporary directory.
        if line.starts_with("  in /") {
            "  in <dir>".clone_into(&mut line);
        }
        // The fixture's temporary path and session id vary each launch. Only
        // that suffix is masked; model, prompt and menu stay exact.
        let line = line
            .split_once(" | /")
            .map_or(line.as_str(), |(status, _)| status);
        let line = mask_durations(&mask_estimates(line));
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
    compare_grid("long_draft.grid", &actual);
}

#[test]
fn short_menu_grids() {
    let actual = fixed_size_replay(include_str!("terminal_menu.py"), 32, 8);
    compare_grid("short_menu.grid", &actual);
    let resized = event_replay("menu-resize");
    compare_grid("menu_resize.grid", &resized);
}

#[test]
fn completion_selection_survives_resize_and_ignored_keys_but_resets_on_edit() {
    let captures: Vec<Capture> = serde_json::from_slice(&record(
        include_str!("terminal_replay.py"),
        Some("completion-resize"),
    ))
    .expect("completion resize captures");
    assert_eq!(captures.len(), 7);
    let mut grid = Grid::new(80, 24).expect("grid");
    for capture in captures {
        let size = if capture.stage == "completion-shrunk" {
            (32, 10)
        } else {
            (80, 24)
        };
        assert_eq!((capture.cols, capture.rows), size);
        grid.resize(capture.cols, capture.rows).expect("resize");
        grid.feed(&capture.bytes).expect("replay");
        let screen = grid.text();
        let input = grid.row_text(grid.cursor().row);
        match capture.stage.as_str() {
            "completion-accepted" => assert_eq!(input, "> /new", "{screen}"),
            "completion-edited" => {
                assert_eq!(input, "> /m", "{screen}");
                assert!(
                    screen.lines().any(|row| row.starts_with("> /model ")),
                    "{screen}"
                );
            }
            "completion-cleared" => {
                assert_eq!(input, ">", "{screen}");
                assert!(!screen.contains("> /model"), "{screen}");
            }
            _ => {
                assert_eq!(input, "> /", "{}: {screen}", capture.stage);
                assert!(
                    screen
                        .lines()
                        .any(|row| row == "> /new" || row.starts_with("> /new ")),
                    "{}: {screen}",
                    capture.stage
                );
            }
        }
    }
}

#[test]
fn settled_help_preserves_every_command_description_in_scrollback_at_40_and_80_columns() {
    for cols in [40, 80] {
        let captures: Vec<Capture> = serde_json::from_slice(&record(
            include_str!("terminal_replay.py"),
            Some(&format!("help-{cols}")),
        ))
        .expect("help captures");
        assert_eq!(captures.len(), 1);
        let mut grid = Grid::new(cols, 24).expect("grid");
        let mut scrollback = Vec::new();
        for capture in captures {
            assert_eq!(capture.cols, cols);
            for byte in capture.bytes {
                let top = grid.row_text(0);
                let stats = grid.feed(&[byte]).expect("replay byte");
                if stats.scrolled {
                    assert_eq!(stats.scroll_rows, 1);
                    scrollback.push(top);
                }
            }
        }
        // Recover the rows a user can copy, including those above the screen.
        assert!(!scrollback.is_empty(), "help never reached scrollback");
        let screen = grid.text();
        let text = scrollback.join("\n") + "\n" + &screen;
        let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for line in rune_term::commands::render_help().lines().chain([
            "/history clear  forget every recorded prompt",
            "/wrap-fixture (project)  Recover this full custom description with 界e\u{301} and its final words from scrollback.",
        ]) {
            let expected = line.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                normalized.contains(&expected),
                "{cols} columns lost help text {expected:?}:\n{text}"
            );
        }
        // A word longer than either width must survive hard breaks as well.
        let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
        let expected = "/long-fixture(project)".to_owned() + &"W".repeat(140) + "END-COMMAND";
        assert!(compact.contains(&expected), "{cols} columns: {text}");
        let cursor = grid.cursor();
        assert_eq!(grid.row_text(cursor.row), ">");
        assert_eq!(cursor.col, 2);
    }
}

#[test]
fn draft_resize_grids() {
    let actual = event_replay("resize");
    compare_grid("draft_resize.grid", &actual);
}

#[test]
fn twelve_column_text_grids_and_scrollback() {
    let actual = event_replay("narrow");
    compare_grid("narrow.grid", &actual);
}

/// The screen rows of one stage of a snapshot, without their frame.
fn stage_rows<'a>(snapshot: &'a str, stage: &str) -> Vec<&'a str> {
    let header = format!("{stage}: ");
    snapshot
        .lines()
        .skip_while(|line| !line.starts_with(&header))
        .skip(1)
        .map_while(|line| line.split_once('|')?.1.strip_suffix('|'))
        .collect()
}

#[test]
fn mask_durations_replaces_only_time_tokens() {
    assert_eq!(mask_durations("a 0.3s b"), "a <t> b");
    assert_eq!(mask_durations("14ms)"), "<t>)");
    assert_eq!(mask_durations("3m05s"), "<t>");
    assert_eq!(mask_durations("12 lines"), "12 lines");
    assert_eq!(mask_durations("v2s3"), "v2s3");
}

#[test]
fn tool_row_grids() {
    let actual = event_replay("tools");
    compare_grid("tools.grid", &actual);
    let rows = stage_rows(&actual, "tools-finished");
    assert!(
        rows.iter()
            .any(|row| row.starts_with("  ✓ read_file notes.txt")),
        "{actual}"
    );
    assert!(
        rows.iter()
            .any(|row| row.starts_with("  ✗ read_file missing.txt")),
        "{actual}"
    );
    assert!(
        rows.iter().any(|row| row.starts_with("  ✓ read 4 files")),
        "{actual}"
    );
    assert!(!rows.iter().any(|row| row.contains("part-")), "{actual}");
}

#[test]
fn shell_tail_grids() {
    let actual = event_replay("shell-tail");
    let running = stage_rows(&actual, "shell-running");
    let call = running
        .iter()
        .position(|row| row.contains("Running for i in"))
        .unwrap_or_else(|| panic!("running row: {actual}"));
    let tail: Vec<&&str> = running[call + 1..]
        .iter()
        .take_while(|row| row.starts_with("    TAIL-"))
        .collect();
    assert!((1..=3).contains(&tail.len()), "{actual}");
    assert!(tail.iter().any(|row| row.contains("TAIL-2")), "{actual}");
    let finished = stage_rows(&actual, "shell-finished");
    assert!(
        finished.iter().all(|row| !row.starts_with("    TAIL-")),
        "{actual}"
    );
    let header = "shell-finished: ";
    let settled = &actual[actual.find(header).expect("finished stage")..];
    compare_grid("shell_tail.grid", settled);
}

#[test]
fn edit_diff_grids() {
    let actual = event_replay("edit");
    compare_grid("edit.grid", &actual);
    let rows = stage_rows(&actual, "edit-finished");
    let call = rows
        .iter()
        .position(|row| row.contains("edit_file"))
        .expect("edit_file row");
    let below = rows.get(call..).expect("rows below the call");
    assert!(below.iter().any(|row| row.contains("+ BETA")), "{actual}");
    assert!(below.iter().any(|row| row.contains("- beta")), "{actual}");
}

#[test]
fn failed_turn_grids() {
    let actual = event_replay("failure");
    compare_grid("failure.grid", &actual);
    let rows = stage_rows(&actual, "failure-reported");
    let failed = rows
        .iter()
        .position(|row| row.starts_with("error: the turn failed: "))
        .expect("failure row");
    assert!(rows.contains(&"\u{258c} fail"), "{actual}");
    assert_eq!(
        failed.checked_sub(1).and_then(|row| rows.get(row)).copied(),
        Some(""),
        "{actual}"
    );
    let hint = rows.get(failed.saturating_add(1)).expect("hint row");
    assert!(
        hint.starts_with("  ") && !hint.trim().is_empty() && !hint.contains('['),
        "{actual}"
    );
}

#[test]
fn approval_prompt_grids() {
    let actual = event_replay("approval");
    compare_grid("approval.grid", &actual);
    let open = stage_rows(&actual, "approval-open").join("\n");
    assert!(open.contains("permission required"), "{actual}");
    assert!(open.contains("shell wants to run:"), "{actual}");
    assert!(open.contains("  echo GRID-APPROVAL"), "{actual}");
    assert!(open.contains("  y  run once"), "{actual}");
    assert!(open.contains("  n  deny"), "{actual}");
    let cancelled = stage_rows(&actual, "approval-cancelled").join("\n");
    assert!(cancelled.contains("cancelled"), "{actual}");
}

#[test]
fn streamed_markdown_grids() {
    let actual = event_replay("markdown");
    compare_grid("markdown.grid", &actual);
    let streaming = stage_rows(&actual, "markdown-streaming");
    assert!(
        streaming.iter().any(|row| row.contains("fn main")),
        "{actual}"
    );
    let finished = stage_rows(&actual, "markdown-finished");
    assert!(
        finished.iter().any(|row| row.contains("MARKDOWN-END")),
        "{actual}"
    );
    assert!(
        finished
            .iter()
            .any(|row| row.contains("name") && row.contains("size")),
        "{actual}"
    );
}

#[test]
fn reasoning_grids() {
    let actual = event_replay("reasoning");
    compare_grid("reasoning.grid", &actual);
    let finished = stage_rows(&actual, "reasoning-finished");
    assert!(
        finished.iter().all(|row| !row.contains("THINK-")),
        "{actual}"
    );
    assert!(
        finished.iter().any(|row| row.contains("thought for")),
        "{actual}"
    );
    assert!(
        finished.iter().any(|row| row.contains("REASONED")),
        "{actual}"
    );
    let full = event_replay("reasoning-full");
    let rows = stage_rows(&full, "reasoning-full-finished");
    for n in 1..=5 {
        let marker = format!("THINK-{n}");
        assert!(rows.iter().any(|row| row.contains(&marker)), "{full}");
    }
    assert!(
        rows.iter().all(|row| !row.contains("thought for")),
        "{full}"
    );
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
                assert!(text.contains("\u{258c} inspect"), "{text}");
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
            "tool-selected" => assert!(text.contains("> tool 3: full"), "{text}"),
            "tool-collapsed" | "tool-recollapsed" => {
                assert!(text.contains("collapsed"), "{text}");
                assert!(text.contains("more line(s)"), "{text}");
                assert!(!text.contains("TOOL-20"), "{text}");
            }
            "tool-expanded" => {
                assert!(text.contains("expanded"), "{text}");
                assert!(text.contains("TOOL-20"), "{text}");
            }
            "tool-surrounding-start" => assert!(text.contains("\u{258c} inspect"), "{text}"),
            "tool-surrounding-end" => assert!(text.contains("REPLY-40"), "{text}"),
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
fn long_code_rows_are_written_whole_and_copy_clean() {
    let captures: Vec<Capture> =
        serde_json::from_slice(&record(include_str!("terminal_replay.py"), Some("code")))
            .expect("code captures");
    let mut grid = Grid::new(32, 24).expect("grid");
    let mut scrollback = Vec::new();
    let line = "    abcdefghijklmnopqrstuvwxyz0123456789  END-CODE";
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
        assert!(
            rows.iter().all(|row| !row.contains('\u{21aa}')),
            "{}: {rows:?}",
            capture.stage
        );
        if capture.stage == "code-started" {
            // The live row is cut to the region and says that it goes on.
            assert!(
                rows.iter()
                    .any(|row| row.starts_with("    abcdefghij") && row.ends_with('\u{2026}')),
                "{rows:?}"
            );
        }
        if capture.stage == "code-finished" {
            assert!(rows.contains(&"AFTER-CODE"), "{rows:?}");
            assert!(rows.iter().all(|row| !row.starts_with("```")), "{rows:?}");
            let start = rows
                .windows(2)
                .position(|pair| format!("{}{}", pair[0], pair[1]) == line)
                .unwrap_or_else(|| panic!("the code line is not whole: {rows:?}"));
            assert_eq!(rows[start.saturating_sub(1)], "rust", "{rows:?}");
            assert_eq!(
                rows.windows(2)
                    .filter(|pair| format!("{}{}", pair[0], pair[1]) == line)
                    .count(),
                1,
                "{rows:?}"
            );
        }
    }
}

/// Replays a scenario byte by byte, returning each stage with the rows that
/// scrolled off the screen so far followed by the screen.
fn rows_by_stage(scenario: &str) -> Vec<(String, Vec<String>)> {
    let captures: Vec<Capture> =
        serde_json::from_slice(&record(include_str!("terminal_replay.py"), Some(scenario)))
            .expect("captures");
    let first = captures.first().expect("at least one capture");
    let mut grid = Grid::new(first.cols, first.rows).expect("grid");
    let mut scrollback = Vec::new();
    let mut stages = Vec::new();
    for capture in captures {
        for byte in capture.bytes {
            let top = grid.row_text(0);
            let stats = grid.feed(&[byte]).expect("replay byte");
            if stats.scrolled {
                assert_eq!(stats.scroll_rows, 1, "one byte scrolled multiple rows");
                scrollback.push(top);
            }
        }
        let mut rows = scrollback.clone();
        rows.extend(grid.text().lines().map(str::to_owned));
        stages.push((capture.stage, rows));
    }
    stages
}

fn occurrences(rows: &[String], text: &str) -> usize {
    rows.iter().filter(|row| row.as_str() == text).count()
}

#[test]
fn a_streamed_answer_is_readable_from_its_first_line() {
    let stages = rows_by_stage("progressive");
    assert_eq!(stages.len(), 2);
    let (stage, paused) = &stages[0];
    assert_eq!(stage, "progressive-paused");
    for index in 1..=149 {
        let para = format!("PARA-{index:03}");
        assert_eq!(
            occurrences(paused, &para),
            1,
            "{para} at the pause: {paused:?}"
        );
    }
    let (_, finished) = &stages[1];
    for index in 1..=300 {
        let para = format!("PARA-{index:03}");
        assert_eq!(
            occurrences(finished, &para),
            1,
            "{para} at the end: {finished:?}"
        );
    }
}

#[test]
fn a_tall_code_block_streams_without_growing_the_region() {
    let stages = rows_by_stage("tall-code");
    assert_eq!(stages.len(), 2);
    let (_, paused) = &stages[0];
    // Rows that left the screen are the first of the rows seen.
    assert_eq!(occurrences(&paused[..paused.len() - 24], "CODE-001"), 1);
    for index in 1..=80 {
        let code = format!("CODE-{index:03}");
        assert!(
            occurrences(paused, &code) <= 1,
            "{code} at the pause: {paused:?}"
        );
    }
    let (_, finished) = &stages[1];
    for index in 1..=120 {
        let code = format!("CODE-{index:03}");
        assert_eq!(
            occurrences(finished, &code),
            1,
            "{code} at the end: {finished:?}"
        );
    }
    let screen = &finished[finished.len() - 24..];
    assert!(
        !screen.iter().any(|row| row.contains("more lines above")),
        "{screen:?}"
    );
}

#[test]
fn resizing_during_a_stream_leaves_no_ghosts() {
    let captures: Vec<Capture> = serde_json::from_slice(&record(
        include_str!("terminal_replay.py"),
        Some("resize-stream"),
    ))
    .expect("captures");
    let first = captures.first().expect("at least one capture");
    let mut grid = Grid::new(first.cols, first.rows).expect("grid");
    let mut scrollback = Vec::new();
    let mut snapshot = String::new();
    let mut stages = Vec::new();
    for capture in captures {
        grid.resize(capture.cols, capture.rows).expect("resize");
        for byte in capture.bytes {
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
        let screen: Vec<String> = (0..capture.rows).map(|row| grid.row_text(row)).collect();
        let caret = usize::from(grid.cursor().row);
        let mut seen = scrollback.clone();
        seen.extend(screen.iter().cloned());
        stages.push((capture.stage, screen, caret, seen));
    }
    let names: Vec<&str> = stages.iter().map(|stage| stage.0.as_str()).collect();
    assert_eq!(names, ["streaming", "shrunk", "grown", "finished"]);
    for (stage, screen, caret, seen) in &stages {
        let hints = screen
            .iter()
            .filter(|row| row.starts_with("ctrl-c interrupt") || row.starts_with("/ commands"))
            .count();
        assert_eq!(hints, 1, "{stage}: hint rows: {screen:#?}");
        let status = screen
            .iter()
            .filter(|row| row.starts_with("replay-fixture"))
            .count();
        assert_eq!(status, 1, "{stage}: status rows: {screen:#?}");
        assert!(
            screen[*caret].starts_with('>'),
            "{stage}: the caret is not on the prompt row: {screen:#?}"
        );
        for index in 1..=30 {
            let row = format!("STREAM-{index:02}");
            assert!(
                occurrences(seen, &row) <= 1,
                "{stage}: {row} is shown twice: {seen:#?}"
            );
        }
    }
    let streaming = &stages[0].3;
    let finished = &stages[3].3;
    for index in 1..=30 {
        let row = format!("STREAM-{index:02}");
        if index <= 15 {
            assert_eq!(occurrences(streaming, &row), 1, "{row}: {streaming:#?}");
        } else {
            assert_eq!(occurrences(finished, &row), 1, "{row}: {finished:#?}");
        }
    }
    compare_grid("resize_stream.grid", &snapshot);
}
