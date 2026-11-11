//! Streamed text is drawn in synchronized frames at a bounded rate, and a key
//! pressed while it streams is drawn at once.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";
const END_SYNC: &[u8] = b"\x1b[?2026l";

#[derive(serde::Deserialize)]
struct Paced {
    elapsed_ms: f64,
    key_ms: Option<f64>,
    bytes: Vec<u8>,
}

fn run(scenario: &str) -> Paced {
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            &support::python_script(include_str!("terminal_pacing.py")),
            env!("CARGO_BIN_EXE_rune"),
            scenario,
        ])
        .output()
        .expect("python3 is required for the Unix terminal pacing test");
    assert!(
        output.status.success(),
        "terminal pacing failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("pacing result")
}

/// Splits bytes at every begin mark, dropping what came before the first.
fn frames(bytes: &[u8]) -> Vec<&[u8]> {
    let starts: Vec<usize> = bytes
        .windows(BEGIN_SYNC.len())
        .enumerate()
        .filter(|(_, window)| *window == BEGIN_SYNC)
        .map(|(start, _)| start)
        .collect();
    starts
        .iter()
        .zip(starts.iter().skip(1).copied().chain([bytes.len()]))
        .map(|(&start, end)| &bytes[start..end])
        .collect()
}

#[test]
fn streamed_deltas_are_drawn_in_whole_synchronized_frames_at_a_bounded_rate() {
    let paced = run("count");
    let frames = frames(&paced.bytes);
    let limit = paced.elapsed_ms / 16.0 + 2.0;
    assert!(
        (frames.len() as f64) <= limit,
        "{} frames in {:.0} ms, limit {limit:.1}",
        frames.len(),
        paced.elapsed_ms
    );
    assert!(!frames.is_empty());
    for frame in &frames {
        assert!(
            frame.ends_with(END_SYNC),
            "a frame was left open: {:?}",
            String::from_utf8_lossy(frame)
        );
        assert_eq!(
            frame
                .windows(END_SYNC.len())
                .filter(|window| *window == END_SYNC)
                .count(),
            1,
            "{:?}",
            String::from_utf8_lossy(frame)
        );
    }
}

#[test]
fn a_key_pressed_while_text_streams_is_drawn_at_once() {
    let paced = run("key");
    let key_ms = paced.key_ms.expect("key timing");
    assert!(key_ms < 100.0, "the key was drawn after {key_ms:.1} ms");
}

#[test]
fn a_dumb_terminal_receives_no_synchronized_output_marks() {
    let paced = run("dumb");
    let text = String::from_utf8_lossy(&paced.bytes);
    assert!(text.contains("PACING-END"), "{text:?}");
    assert!(!text.contains("\u{1b}[?2026"), "{text:?}");
}
