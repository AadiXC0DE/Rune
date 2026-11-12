//! Choosing a saved session to resume.
//!
//! The list is drawn by the caller through a closure, so the same picker runs
//! before a session starts, with a renderer of its own, and inside a session,
//! through the session's renderer.

use camino::Utf8Path;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;
use rune_session::event::SessionEvent;
use rune_session::store::{SessionState, load_read_only};
use rune_term::input::{KeyAction, KeyReader};
use rune_term::picker::{DEFAULT_WINDOW, Picker};
use rune_term::theme::{ColorDepth, Slot, Theme};
use rune_term::width::{str_width, truncate_to_width};

use crate::session_log::{self, Summary};

/// Shown when the workspace has no saved session.
const EMPTY: &str = "no saved sessions here; tab shows every workspace";

/// Closes a styled row.
const RESET: &str = "\u{1b}[0m";

/// Rows the frame needs besides the list: the title, the query, the
/// preview, the range row and the status rows of a session.
const FRAME_ROWS: usize = 10;

/// Draws the picker's rows, with the caret as a row within them and a column.
pub type Draw<'a> = dyn FnMut(&[String], (u16, u16)) -> Result<()> + 'a;

/// One saved session as the picker lists it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ResumeRow {
    /// The listing row.
    pub summary: Summary,
    /// The last exchange: the user's line, then two lines of the reply.
    pub preview: [String; 3],
}

/// Lists saved sessions, in this workspace or in every workspace.
#[must_use]
pub fn rows(paths: &Paths, workspace: Option<&Utf8Path>) -> Vec<ResumeRow> {
    session_log::list_scoped(paths, workspace)
        .into_iter()
        .map(|summary| {
            let preview = load_read_only(&summary.dir)
                .map(|state| preview(&state))
                .unwrap_or_default();
            ResumeRow { summary, preview }
        })
        .collect()
}

/// Returns the title a row shows: the stored one, or one derived from the
/// first prompt.
fn title_of(summary: &Summary) -> String {
    match (&summary.title, &summary.first_prompt) {
        (Some(title), _) => title.clone(),
        (None, Some(prompt)) => session_log::derive_title(prompt),
        (None, None) => "untitled".to_owned(),
    }
}

/// Formats how long ago a session was last active.
fn age(now_ms: u64, then_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(then_ms) / 1000;
    let minutes = seconds / 60;
    let hours = minutes / 60;
    if seconds < 60 {
        "now".to_owned()
    } else if minutes < 60 {
        format!("{minutes}m")
    } else if hours < 48 {
        format!("{hours}h")
    } else {
        format!("{}d", hours / 24)
    }
}

/// Cuts `text` to `width` columns, marking a cut with an ellipsis.
fn fit(text: &str, width: usize) -> String {
    if str_width(text) <= width {
        return text.to_owned();
    }
    let (kept, _) = truncate_to_width(text, width.saturating_sub(1));
    format!("{kept}…")
}

/// Renders one row: title, age, turns and model.
#[must_use]
pub fn row_text(row: &ResumeRow, now_ms: u64, width: usize) -> String {
    let summary = &row.summary;
    let tail = format!(
        "  {}  {} turns  {}",
        age(now_ms, summary.updated_at_ms()),
        summary.turns,
        summary.model.as_deref().unwrap_or("-"),
    );
    let room = width.saturating_sub(str_width(&tail));
    let title = rune_term::transcript::sanitize(&title_of(summary)).replace('\n', " ");
    format!("{}{tail}", fit(&title, room))
}

/// Returns the last exchange of a session.
///
/// Row 0 is the first line of the last user message, rows 1 and 2 the first
/// two non-empty lines of the last reply. A missing part is an empty row.
#[must_use]
pub fn preview(state: &SessionState) -> [String; 3] {
    let mut user = None;
    let mut reply = None;
    for frame in rune_session::replay::replay_events(&state.events) {
        match &frame.event {
            SessionEvent::UserMessage { text } => user = Some(text.clone()),
            SessionEvent::AssistantMessage { text, .. } if !text.trim().is_empty() => {
                reply = Some(text.clone());
            }
            _ => {}
        }
    }
    let clean = |line: &str| rune_term::transcript::sanitize(line.trim());
    let mut out: [String; 3] = Default::default();
    if let Some(user) = user {
        out[0] = format!("> {}", clean(user.lines().next().unwrap_or("")));
    }
    if let Some(reply) = reply {
        for (slot, line) in out[1..]
            .iter_mut()
            .zip(reply.lines().filter(|line| !line.trim().is_empty()))
        {
            *slot = clean(line);
        }
    }
    out
}

/// Returns the current time in milliseconds since the epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Builds the picker for one scope.
fn picker_for(listed: &[ResumeRow], everywhere: bool, width: usize) -> Picker {
    let title = if everywhere {
        "sessions in every workspace (tab: this workspace)"
    } else {
        "sessions in this workspace (tab: all workspaces)"
    };
    let now = now_ms();
    // The picker draws a two-column marker before each item.
    let items: Vec<String> = listed
        .iter()
        .map(|row| row_text(row, now, width.saturating_sub(2)))
        .collect();
    let haystack = items
        .iter()
        .zip(listed)
        .map(|(item, row)| {
            format!(
                "{item}\u{0}{}",
                row.summary.first_prompt.as_deref().unwrap_or("")
            )
        })
        .collect();
    let mut picker = Picker::new(title, items, DEFAULT_WINDOW);
    picker.set_haystack(haystack);
    picker
}

/// Wraps `text` in a style, leaving it bare when the theme has none.
fn styled(open: &str, text: &str) -> String {
    if open.is_empty() {
        return text.to_owned();
    }
    format!("{open}{text}{RESET}")
}

/// Refuses a session recorded in another workspace.
fn refuse_elsewhere(summary: &Summary, workspace: &Utf8Path) -> Result<()> {
    let here = rune_policy::trust::canonical_workspace(workspace);
    match summary.workspace.as_deref() {
        Some(there) if there != here.as_str() => Err(RuneError::new(
            ErrorCode::InvalidConfiguration,
            format!("session {} ran in {there}", summary.id),
        )
        .with_hint(format!("run `cd {there} && rune --resume {}`", summary.id))),
        _ => Ok(()),
    }
}

/// Runs the picker and returns the chosen session's id.
///
/// Typing narrows the list by title and first prompt, Tab switches between
/// this workspace and every workspace, Enter accepts, and Escape, Ctrl-C or
/// Ctrl-D leave without a choice.
pub fn pick(
    reader: &mut KeyReader,
    draw: &mut Draw<'_>,
    paths: &Paths,
    workspace: &Utf8Path,
) -> Result<Option<String>> {
    let depth = rune_term::theme::detect_depth(
        "auto",
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
        |key| std::env::var(key).ok(),
    );
    pick_styled(reader, draw, paths, workspace, &Theme::dark(), depth)
}

/// Runs the picker with the colours of a running session.
pub fn pick_styled(
    reader: &mut KeyReader,
    draw: &mut Draw<'_>,
    paths: &Paths,
    workspace: &Utf8Path,
    theme: &Theme,
    depth: ColorDepth,
) -> Result<Option<String>> {
    // A reader that sees no keys would leave this loop waiting forever.
    if !reader.is_active() {
        return Ok(None);
    }
    let draft = reader.line().to_owned();
    reader.clear();
    let dim = theme.sgr(Slot::Dim, depth);
    let (mut cols, _) = rune_term::shell::terminal_size().unwrap_or((80, 24));
    let mut everywhere = false;
    let mut listed = rows(paths, Some(workspace));
    let mut picker = picker_for(&listed, everywhere, usize::from(cols));

    let chosen = loop {
        let (width, height) = rune_term::shell::terminal_size().unwrap_or((80, 24));
        if width != cols {
            cols = width;
            picker = picker_for(&listed, everywhere, usize::from(cols));
            picker.set_query(reader.line());
        }
        picker.set_window(DEFAULT_WINDOW.min(usize::from(height).saturating_sub(FRAME_ROWS)));
        let mut frame = vec![picker.title().to_owned(), format!("> {}", reader.line())];
        if listed.is_empty() {
            frame.push(styled(&dim, &format!("  {EMPTY}")));
        } else {
            frame.extend(picker.rows(theme, depth));
        }
        let shown = picker
            .selected()
            .and_then(|_| listed.get(picker.cursor()))
            .map(|row| row.preview.clone())
            .unwrap_or_default();
        let room = usize::from(cols).saturating_sub(4);
        frame.extend(
            shown
                .iter()
                .map(|line| styled(&dim, &format!("    {}", fit(line, room)))),
        );
        let column = u16::try_from(reader.column().saturating_add(2)).unwrap_or(u16::MAX);
        draw(&frame, (1, column))?;

        match reader.read_key() {
            KeyAction::Suspend => rune_term::modes::suspend(),
            KeyAction::Submit => {
                if picker.selected().is_some()
                    && let Some(row) = listed.get(picker.cursor())
                {
                    if let Err(err) = refuse_elsewhere(&row.summary, workspace) {
                        reader.replace(&draft);
                        return Err(err);
                    }
                    break Some(row.summary.id.clone());
                }
            }
            KeyAction::Interrupt | KeyAction::Cancel | KeyAction::Escape => break None,
            KeyAction::Up => {
                picker.up();
            }
            KeyAction::Down => {
                picker.down();
            }
            KeyAction::Complete => {
                everywhere = !everywhere;
                listed = rows(paths, (!everywhere).then_some(workspace));
                picker = picker_for(&listed, everywhere, usize::from(cols));
                picker.set_query(reader.line());
            }
            KeyAction::Transcript
            | KeyAction::ExternalEditor
            | KeyAction::HistorySearch
            | KeyAction::CycleReasoning
            | KeyAction::ModeCycle
            | KeyAction::TogglePlan
            | KeyAction::PullQueued
            | KeyAction::Queue => {}
            KeyAction::Ignored => picker.set_query(reader.line()),
        }
    };
    reader.replace(&draft);
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::id::{EventSeq, SessionId};
    use rune_session::event::EventFrame;

    /// Writes one session holding `events` and returns its directory's owner.
    fn stored(events: Vec<SessionEvent>) -> (tempfile::TempDir, Paths, SessionId) {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = Paths {
            state_root: root.join("state"),
            config_root: root.join("config"),
            data_root: root.join("data"),
        };
        let id: SessionId = "sessionrrrrr".parse().expect("id");
        drop(rune_session::store::SessionStore::create(&paths, &id).expect("create"));
        let frames: Vec<EventFrame> = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| {
                EventFrame::new(
                    EventSeq(u64::try_from(index).expect("index").saturating_add(1)),
                    1_000,
                    event,
                )
            })
            .collect();
        rune_session::store::write_log_atomic(&paths.session_dir(&id), &frames).expect("log");
        (dir, paths, id)
    }

    fn row(title: Option<&str>, first_prompt: Option<&str>) -> ResumeRow {
        let mut events = Vec::new();
        if let Some(title) = title {
            events.push(SessionEvent::TitleSet {
                title: title.to_owned(),
            });
        }
        if let Some(prompt) = first_prompt {
            events.push(SessionEvent::UserMessage {
                text: prompt.to_owned(),
            });
        }
        events.push(SessionEvent::ModelSet {
            model: "fixture-model".to_owned(),
            effort: rune_core::config::Effort::Max,
        });
        let (_dir, paths, _) = stored(events);
        let mut listed = rows(&paths, None);
        assert_eq!(listed.len(), 1);
        listed.remove(0)
    }

    #[test]
    fn row_text_formats_age_and_turns() {
        let row = row(Some("parser work"), None);
        let then = row.summary.updated_at_ms();
        let minute = 60_000;
        let hour = 60 * minute;
        assert_eq!(
            row_text(&row, then + 5_000, 80),
            "parser work  now  0 turns  fixture-model"
        );
        assert!(row_text(&row, then + 7 * minute, 80).contains("  7m  "));
        assert!(row_text(&row, then + 47 * hour, 80).contains("  47h  "));
        assert!(row_text(&row, then + 72 * hour, 80).contains("  3d  "));

        let mut unnamed = row.clone();
        unnamed.summary.model = None;
        assert!(row_text(&unnamed, then, 80).ends_with("0 turns  -"));

        let mut long = row.clone();
        long.summary.title = Some("x".repeat(100));
        let text = row_text(&long, then, 40);
        assert_eq!(str_width(&text), 40, "{text}");
        assert!(text.contains("x…  now"), "{text}");
    }

    #[test]
    fn preview_takes_the_last_exchange() {
        let (_dir, paths, id) = stored(vec![
            SessionEvent::UserMessage {
                text: "first prompt".to_owned(),
            },
            SessionEvent::AssistantMessage {
                turn: 1,
                text: "first reply".to_owned(),
            },
            SessionEvent::UserMessage {
                text: "second prompt\nmore detail".to_owned(),
            },
            SessionEvent::AssistantMessage {
                turn: 2,
                text: "\nreply line one\n\n  reply line two\nreply line three".to_owned(),
            },
        ]);
        let state = session_log::inspect(&paths, &id).expect("state");
        assert_eq!(
            preview(&state),
            ["> second prompt", "reply line one", "reply line two"]
        );
        let listed = rows(&paths, None);
        assert_eq!(listed[0].preview, preview(&state));

        let (_dir, paths, id) = stored(vec![SessionEvent::TitleSet {
            title: "nothing yet".to_owned(),
        }]);
        let state = session_log::inspect(&paths, &id).expect("state");
        assert_eq!(preview(&state), ["", "", ""]);
    }

    #[test]
    fn untitled_sessions_use_the_first_prompt() {
        let untitled = row(None, Some("fix the lexer\nwith detail"));
        let then = untitled.summary.updated_at_ms();
        let text = row_text(&untitled, then, 80);
        assert!(text.starts_with("fix the lexer  now"), "{text}");
        let titled = row(Some("renamed"), Some("fix the lexer"));
        assert!(row_text(&titled, then, 80).starts_with("renamed  now"));
        let bare = row(None, None);
        assert!(row_text(&bare, then, 80).starts_with("untitled  now"));
    }
}
