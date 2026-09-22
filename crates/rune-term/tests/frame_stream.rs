// Integration tests assert by panicking.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! A committed frame stream must be self-contained.
//!
//! Every committed frame is verified against a replay into the previous screen,
//! but that only proves the delta is consistent with the state the surface
//! remembers. Feeding the accumulated stream into a cleared grid proves the
//! stronger property a terminal actually relies on: the bytes carry every
//! attribute they depend on.

use rune_core::config::PermissionMode;
use rune_term::engine::Grid;
use rune_term::footer::{self, FooterState};
use rune_term::frame::{FrameSurface, Regions, compose};
use rune_term::theme::Theme;

fn state() -> FooterState {
    FooterState {
        model: "claude-sonnet-4".to_owned(),
        permission_mode: PermissionMode::Auto,
        workspace: "/Users/dev/rune".to_owned(),
        context_used: 24_500,
        context_limit: 200_000,
        session_id: "9f2c1a7b4e".to_owned(),
    }
}

#[test]
fn a_committed_stream_rebuilds_the_screen_on_a_cleared_terminal() {
    let theme = Theme::fx_dark();
    let layout = footer::solve((80, 24), 1, false, footer::DEFAULT_MINIMUM_ROWS);
    let footer = footer::render(&state(), &layout, &theme, 80, true);
    let transcript: Vec<String> = (0..40).map(|index| format!("line {index}")).collect();
    let prompt = vec![theme.sgr(rune_term::theme::Slot::UserRail, true) + "> "];

    let mut surface = FrameSurface::new(80, 24).expect("surface");
    let mut stream: Vec<u8> = Vec::new();
    let mut frames = 0usize;
    for tick in 0..60 {
        let mut lines = transcript.clone();
        lines.push(format!("tick {tick}"));
        let regions = Regions::new(&lines, &footer)
            .with_prompt(&prompt)
            .with_activity(Some("working"))
            .with_cursor(0, 2);
        let target = compose(&regions, 80, 24).expect("compose");
        let commit = surface.commit(&target).expect("commit");
        if commit.is_empty() {
            continue;
        }
        frames = frames.saturating_add(1);
        stream.extend_from_slice(&commit.bytes);

        let mut cleared = Grid::new(80, 24).expect("cleared");
        cleared.feed(&stream).expect("feed");
        assert_eq!(cleared, target, "frame {tick} did not rebuild its screen");
    }
    assert!(frames > 0, "no frame was committed");

    let regions = Regions::new(&transcript, &footer).with_prompt(&prompt);
    let target = compose(&regions, 80, 24).expect("compose");
    surface.commit(&target).expect("commit");
    let repeat = surface.commit(&target).expect("commit");
    assert!(
        repeat.is_empty(),
        "a repeated frame wrote {} bytes",
        repeat.bytes.len()
    );
}
