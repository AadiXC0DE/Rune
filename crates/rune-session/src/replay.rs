//! Conversation projection of the streaming journal.
//!
//! Deltas are provisional. A saved final outcome replaces them. Otherwise replay exposes one partial message and an interrupted
//! boundary, without manufacturing usage or a completed provider response.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use crate::event::{EventFrame, SessionEvent};

/// Projects journal records into conversation events, borrowing settled frames.
/// Synthetic frames retain the last delta's position and timestamp; they are
/// display/history records and must not be appended back to the event log.
pub fn replay_events(frames: &[EventFrame]) -> impl Iterator<Item = Cow<'_, EventFrame>> {
    // Old logs ended at AssistantMessage. Journalled turns require an explicit
    // outcome, including when the process dies while writing its final message.
    let journalled: BTreeSet<u64> = frames
        .iter()
        .filter_map(|frame| match frame.event {
            SessionEvent::AssistantDelta { turn, .. }
            | SessionEvent::AssistantReset { turn }
            | SessionEvent::TurnFinished { turn } => Some(turn),
            _ => None,
        })
        .collect();
    let settled: BTreeSet<u64> = frames
        .iter()
        .filter_map(|frame| match frame.event {
            SessionEvent::TurnFinished { turn }
            | SessionEvent::TurnCancelled { turn }
            | SessionEvent::TurnFailed { turn, .. } => Some(turn),
            SessionEvent::AssistantMessage { turn, .. } if !journalled.contains(&turn) => {
                Some(turn)
            }
            _ => None,
        })
        .collect();
    let mut partials: BTreeMap<u64, (usize, String)> = BTreeMap::new();
    for (index, frame) in frames.iter().enumerate() {
        match &frame.event {
            SessionEvent::AssistantDelta { turn, text } if !settled.contains(turn) => {
                let partial = partials.entry(*turn).or_default();
                partial.0 = index;
                partial.1.push_str(text);
            }
            SessionEvent::AssistantReset { turn } if !settled.contains(turn) => {
                partials.insert(*turn, (index, String::new()));
            }
            SessionEvent::AssistantMessage { turn, text } if !settled.contains(turn) => {
                partials.insert(*turn, (index, text.clone()));
            }
            _ => {}
        }
    }
    frames.iter().enumerate().flat_map(move |(index, frame)| {
        let turn = match frame.event {
            SessionEvent::AssistantDelta { turn, .. }
            | SessionEvent::AssistantReset { turn }
            | SessionEvent::AssistantMessage { turn, .. } => Some(turn),
            _ => None,
        };
        let projected = if let Some(turn) =
            turn.filter(|turn| partials.get(turn).is_some_and(|partial| partial.0 == index))
        {
            let text = partials
                .remove(&turn)
                .map(|partial| partial.1)
                .unwrap_or_default();
            let message = EventFrame::new(
                frame.seq,
                frame.timestamp_ms,
                SessionEvent::AssistantMessage { turn, text },
            );
            let boundary = EventFrame::new(
                frame.seq,
                frame.timestamp_ms,
                SessionEvent::TurnInterrupted { turn },
            );
            [Some(Cow::Owned(message)), Some(Cow::Owned(boundary))]
        } else if matches!(
            frame.event,
            SessionEvent::AssistantDelta { .. } | SessionEvent::AssistantReset { .. }
        ) {
            [None, None]
        } else {
            [Some(Cow::Borrowed(frame)), None]
        };
        projected.into_iter().flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::error::ErrorCode;
    use rune_core::id::EventSeq;

    fn replay(events: Vec<SessionEvent>) -> Vec<SessionEvent> {
        let frames: Vec<_> = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| {
                let frame = EventFrame::new(
                    EventSeq((index as u64).saturating_add(1)),
                    index as u64,
                    event,
                );
                EventFrame::decode(&frame.encode().expect("encoded")).expect("decoded")
            })
            .collect();
        replay_events(&frames)
            .map(|frame| frame.event.clone())
            .collect()
    }

    fn delta(text: &str) -> SessionEvent {
        SessionEvent::AssistantDelta {
            turn: 1,
            text: text.to_owned(),
        }
    }

    fn message(text: &str) -> SessionEvent {
        SessionEvent::AssistantMessage {
            turn: 1,
            text: text.to_owned(),
        }
    }

    #[test]
    fn unfinished_deltas_replay_as_one_partial_and_one_interrupted_boundary() {
        let events = replay(vec![
            SessionEvent::TurnStarted { turn: 1 },
            delta("STREAM-01\n"),
            delta("STREAM-02\n"),
            delta("STREAM-03\n"),
            SessionEvent::UserMessage {
                text: "continue".to_owned(),
            },
        ]);
        assert_eq!(
            events,
            vec![
                SessionEvent::TurnStarted { turn: 1 },
                message("STREAM-01\nSTREAM-02\nSTREAM-03\n"),
                SessionEvent::TurnInterrupted { turn: 1 },
                SessionEvent::UserMessage {
                    text: "continue".to_owned()
                },
            ]
        );
    }

    #[test]
    fn a_retry_discards_the_failed_attempt_even_if_killed_before_new_text() {
        let reset = SessionEvent::AssistantReset { turn: 1 };
        assert_eq!(
            replay(vec![delta("discard me"), reset.clone(), delta("keep me")]),
            vec![
                message("keep me"),
                SessionEvent::TurnInterrupted { turn: 1 }
            ]
        );
        assert_eq!(
            replay(vec![delta("discard me"), reset]),
            vec![message(""), SessionEvent::TurnInterrupted { turn: 1 }]
        );
    }

    #[test]
    fn final_outcomes_supersede_deltas_including_an_empty_completed_answer() {
        for boundary in [
            SessionEvent::TurnFinished { turn: 1 },
            SessionEvent::TurnCancelled { turn: 1 },
            SessionEvent::TurnFailed {
                turn: 1,
                code: ErrorCode::IncompleteStream,
                message: "cut".to_owned(),
            },
        ] {
            assert_eq!(
                replay(vec![delta("answer"), message("answer"), boundary.clone()]),
                vec![message("answer"), boundary.clone()]
            );
            assert_eq!(
                replay(vec![delta("provisional"), boundary.clone()]),
                vec![boundary]
            );
        }
    }

    #[test]
    fn death_while_saving_the_final_message_does_not_mark_the_turn_complete() {
        assert_eq!(
            replay(vec![delta("provisional"), message("final text")]),
            vec![
                message("final text"),
                SessionEvent::TurnInterrupted { turn: 1 }
            ]
        );
    }

    #[test]
    fn old_logs_still_replay_their_saved_messages_as_before() {
        assert_eq!(
            replay(vec![message("legacy answer")]),
            vec![message("legacy answer")]
        );
    }
}
