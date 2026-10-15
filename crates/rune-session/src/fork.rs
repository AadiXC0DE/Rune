//! Independent saved branches made from an explicit inclusive event boundary.

use std::collections::BTreeSet;

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::{EventSeq, SessionId};
use rune_core::paths::{self, Paths};

use crate::event::SessionEvent;
use crate::store::{SessionStore, load_read_only, write_log_atomic};

/// Copies the source journal through `at` into a new, independently writable
/// session and records its origin. Frames keep their positions and timestamps.
/// The source is opened only for reading, including when it has a live writer.
/// Tool calls must all have results at the chosen boundary. Child sessions are
/// refused because their authority belongs to their parent.
///
/// Only journal history is inherited. Workspace files, mutation journals,
/// manual compaction archives, and the global usage ledger are not copied.
pub fn fork(
    paths: &Paths,
    source: &SessionId,
    destination: &SessionId,
    at: EventSeq,
) -> Result<()> {
    let state = load_read_only(&paths.session_dir(source))?;
    if state.parent.is_some() {
        return Err(RuneError::new(
            ErrorCode::Unsupported,
            "a child session cannot be forked independently of its parent",
        ));
    }
    let end = state
        .events
        .iter()
        .position(|frame| frame.seq == at)
        .ok_or_else(|| {
            RuneError::invalid_field("at", format!("no saved event at sequence {at}"))
        })?;
    let prefix = &state.events[..=end];
    let mut pending = BTreeSet::new();
    let mut content_calls = BTreeSet::new();
    let mut partial_results = false;
    for frame in prefix {
        match &frame.event {
            SessionEvent::AssistantContent { message }
                if pending.is_empty() && content_calls.is_empty() =>
            {
                let message: serde_json::Value = serde_json::from_str(message)?;
                if let Some(parts) = message["parts"].as_array() {
                    for part in parts.iter().filter(|part| part["type"] == "tool_call") {
                        if let Some(id) = part["id"].as_str() {
                            content_calls.insert(id.to_owned());
                        }
                    }
                }
            }
            SessionEvent::ToolCall { call_id, .. } => {
                rune_core::id::ToolCallId::new(call_id.clone())?;
                content_calls.remove(call_id);
                if partial_results {
                    return Err(RuneError::invalid_field(
                        "at",
                        "a new tool call interrupts an incomplete result group",
                    ));
                }
                if !pending.insert(call_id.as_str()) {
                    return Err(RuneError::invalid_field(
                        "at",
                        "duplicate unanswered tool call",
                    ));
                }
            }
            SessionEvent::ToolResult { call_id, .. } => {
                if !pending.remove(call_id.as_str()) {
                    return Err(RuneError::invalid_field(
                        "at",
                        "tool result has no matching call",
                    ));
                }
                partial_results = !pending.is_empty();
            }
            SessionEvent::UserMessage { .. }
            | SessionEvent::AssistantMessage { .. }
            | SessionEvent::AssistantContent { .. }
            | SessionEvent::TurnStarted { .. }
            | SessionEvent::TurnFinished { .. }
            | SessionEvent::TurnCancelled { .. }
            | SessionEvent::TurnFailed { .. }
            | SessionEvent::TurnInterrupted { .. }
                if !pending.is_empty() =>
            {
                return Err(RuneError::invalid_field(
                    "at",
                    "a conversation boundary interrupts unanswered tool calls",
                ));
            }
            _ => {}
        }
    }
    if !pending.is_empty() || !content_calls.is_empty() {
        return Err(
            RuneError::invalid_field("at", "fork boundary leaves unanswered tool calls")
                .with_hint("choose an event after all results in the tool group are saved"),
        );
    }

    // Reserve the destination exclusively. Never replace an existing session,
    // even if two callers chose the same identifier or the directory is empty.
    paths.ensure_roots()?;
    paths::create_dir_private(&paths.sessions_dir())?;
    let dir = paths.session_dir(destination);
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(paths::DIR_MODE);
    }
    builder.create(&dir).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            RuneError::new(
                ErrorCode::AlreadyExists,
                format!("session `{destination}` already exists"),
            )
        } else {
            error.into()
        }
    })?;
    let result = (|| {
        write_log_atomic(&dir, prefix)?;
        let store = SessionStore::open(&dir)?;
        let report = store.append(SessionEvent::ForkedFrom {
            session: source.to_string(),
            through: at.0,
        })?;
        if let Some(warning) = report.metadata_warning {
            return Err(warning);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}
