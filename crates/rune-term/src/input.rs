//! Reading keys from a terminal.
//!
//! A line editor needs each keystroke as it arrives, not each line once the
//! terminal decides it is finished. That requires the terminal to stop
//! collecting input itself and to stop echoing it, which is what this module
//! arranges and, more importantly, undoes.
//!
//! Echo being off is what makes a visible cursor possible: with the terminal
//! echoing, every key appears twice and the cursor is wherever the terminal left
//! it rather than where the editor thinks it is.

use std::io::IsTerminal;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::editor::Composer;
use crate::keys::{ACTIONS, Context, Keymap};
use crate::probe::Tri;

/// Columns of the prompt marker until the session reports its layout.
const DEFAULT_PROMPT_WIDTH: usize = 2;

/// Terminal columns until the session reports its layout.
const DEFAULT_WIDTH: usize = 80;

/// How long a blocking key read waits before checking for a redraw request.
const REDRAW_POLL: Duration = Duration::from_millis(100);

/// Describes the bindings used by this reader and the session's input modes.
/// Line input leaves editing to the terminal instead of the Rune composer.
/// The rebindable keys come from the action table and the keymap in use.
#[must_use]
pub fn render_bindings_help(active: bool, keymap: &Keymap) -> String {
    if !active {
        return "Input: terminal line mode\nEnter submits one line; end of input leaves the session.\nComposer shortcuts, menus, and live steering require interactive key input."
            .to_owned();
    }
    let rows = |contexts: &[Context]| {
        ACTIONS
            .iter()
            .filter(|info| contexts.contains(&info.context))
            .map(|info| {
                let keys: Vec<String> = keymap
                    .keys(info.id)
                    .iter()
                    .map(|name| key_label(name))
                    .collect();
                let keys = if keys.is_empty() {
                    "(none)".to_owned()
                } else {
                    keys.join(" / ")
                };
                format!("  {keys:<32} {}  ({})", info.summary, info.id)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Keyboard bindings (interactive key input)
Actions are rebound by id in keys.toml; `rune keys` lists them.

Composer:
{}
  Left / Right  move by one grapheme, including across newlines
  Up / Down     move between rows of a long or multiline draft
  Backspace     delete the grapheme before the caret
  Delete        delete the grapheme at the caret
  Ctrl-D        delete at the caret; empty idle draft leaves the session
  Alt-T         cycle how reasoning is shown

While a turn runs (composer keys also apply):
{}

Idle prompt and menus:
  Up / Down     recall prompts from the first / last draft row;
                select a menu row when a menu is open
  Enter         accept an incomplete command; run a complete command
  Tab / Enter   accept a path, model, or history selection
                Ctrl-R again in history search switches to all workspaces
  Esc / Ctrl-C / Ctrl-D  cancel model/history menus (Ctrl-D: empty query)
                cancelling preserves the draft and caret
  Esc           close a path menu first; otherwise clear the idle draft

Permission and question menus:
  Up / Down     select an option without editing the draft
  Enter         confirm the selected option
  Esc / Ctrl-C / Ctrl-D  cancel the turn, preserving the draft and caret

Interrupting:
  Esc           with an empty draft, stop the running turn now;
                with text, press twice within {} ms
  Ctrl-C        clear the draft (Up brings it back); with an empty
                draft, stop the turn, or press twice to leave when idle

Transcript:
{}
  Up / Down     scroll one row
  PageUp / PageDown  scroll one page
  Home / End    jump to the beginning / end
  Tab / Shift-Tab  select the next / previous tool
  Space         collapse or expand the selected tool (16 KiB / 200 rows)
  Ctrl-C        close and restore the draft and caret

Paste:
  Bracketed paste inserts at the caret without submitting or completing.
  Newlines and tabs stay in the draft; CR/CRLF become LF; other controls
  are removed. Terminals without bracketed paste send ordinary keys.",
        rows(&[Context::Editor]),
        rows(&[Context::Turn]),
        crate::shell::ESCAPE_CANCEL_WINDOW.as_millis(),
        rows(&[Context::Viewer]),
    )
}

/// Spells a key file name the way help writes keys: `ctrl+r` is `Ctrl-R`.
fn key_label(name: &str) -> String {
    let parts: Vec<&str> = name.split('+').collect();
    let parts = if name.ends_with("++") || name == "+" {
        let mut parts: Vec<&str> = name.trim_end_matches('+').split('+').collect();
        parts.retain(|part| !part.is_empty());
        parts.push("+");
        parts
    } else {
        parts
    };
    parts
        .iter()
        .map(|part| match *part {
            "pageup" => "PageUp".to_owned(),
            "pagedown" => "PageDown".to_owned(),
            part => {
                let mut chars = part.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().chain(chars).collect()
                })
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// What a key asks the session to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyAction {
    /// Nothing changed that the caller needs to act on.
    Ignored,
    /// The line was submitted.
    Submit,
    /// The user asked to leave.
    Interrupt,
    /// The user asked to cancel what is running.
    ///
    /// Control-C. It clears a partly typed line first, because that is what the
    /// key does in every other shell, and only then reaches the work.
    Cancel,
    /// The user pressed Escape.
    ///
    /// Kept apart from [`KeyAction::Cancel`] because the two carry different
    /// gestures: Escape is also the key that clears a line, so cancelling on a
    /// single press would make a stray Escape stop the work the user was about
    /// to steer.
    Escape,
    /// The user moved the selection up.
    Up,
    /// The user moved the selection down.
    Down,
    /// The user asked to complete what is being typed.
    Complete,
    /// Open the full transcript without editing the composer.
    Transcript,
    /// Edit the draft in the configured external editor.
    ExternalEditor,
    /// Search recorded prompts without submitting the composer.
    HistorySearch,
    /// Pause the process, as the terminal's suspend key does.
    Suspend,
    /// Cycle how reasoning is shown: collapsed, tail, full.
    CycleReasoning,
    /// Move to the next permission mode: ask, auto, then plan.
    ModeCycle,
    /// Take the newest message waiting for a turn back into the draft.
    PullQueued,
    /// Hold the draft as a new turn that starts once the running one ends.
    Queue,
}

/// Navigation while the transcript owns the terminal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TranscriptAction {
    /// Leave the viewer and restore the draft.
    Close,
    /// Scroll one row up.
    Up,
    /// Scroll one row down.
    Down,
    /// Scroll one page up.
    PageUp,
    /// Scroll one page down.
    PageDown,
    /// Jump to the beginning.
    Home,
    /// Jump to the end.
    End,
    /// Select the next tool entry.
    NextTool,
    /// Select the previous tool entry.
    PreviousTool,
    /// Collapse or expand the selected tool entry.
    ToggleTool,
    /// Ignore editing, paste, and non-input events.
    Ignored,
}

/// Maps viewer input without touching a composer or its editing history.
#[must_use]
pub fn transcript_event(keymap: &Keymap, event: &Event) -> TranscriptAction {
    let Event::Key(key) = event else {
        return TranscriptAction::Ignored;
    };
    if key.kind == KeyEventKind::Release {
        return TranscriptAction::Ignored;
    }
    if keymap.action(Context::Viewer, key) == Some("viewer_close") {
        return TranscriptAction::Close;
    }
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match (key.code, control) {
        (KeyCode::Char('c'), true) => TranscriptAction::Close,
        (KeyCode::Up, _) => TranscriptAction::Up,
        (KeyCode::Down, _) => TranscriptAction::Down,
        (KeyCode::PageUp, _) => TranscriptAction::PageUp,
        (KeyCode::PageDown, _) => TranscriptAction::PageDown,
        (KeyCode::Home, _) => TranscriptAction::Home,
        (KeyCode::End, _) => TranscriptAction::End,
        (KeyCode::Tab, false) => TranscriptAction::NextTool,
        (KeyCode::BackTab, false) => TranscriptAction::PreviousTool,
        (KeyCode::Char(' '), false) => TranscriptAction::ToggleTool,
        _ => TranscriptAction::Ignored,
    }
}

/// Reads keys and drives a composer.
///
/// Restores the terminal on drop, including when the process is unwinding, so a
/// session that ends for any reason does not leave the terminal without echo.
#[derive(Debug)]
pub struct KeyReader {
    composer: Composer,
    active: bool,
    prompt_width: usize,
    width: usize,
    menu_open: bool,
    focus: Tri,
    keymap: Keymap,
    context: Context,
}

impl KeyReader {
    /// Puts the terminal into the mode a key reader needs.
    ///
    /// A terminal that cannot report keys keeps the terminal's own line
    /// collection, so a session over a pipe still works rather than hanging.
    ///
    /// Bracketed paste is turned on with raw mode. Without it a paste arrives
    /// as keystrokes, so each line break in it is an Enter that submits the
    /// line so far and each tab asks for a completion.
    #[must_use]
    pub fn new() -> Self {
        let active =
            std::io::stdin().is_terminal() && crossterm::terminal::enable_raw_mode().is_ok();
        if active {
            install_panic_hook();
            crate::modes::install_signal_handlers();
            // A terminal that cannot bracket a paste delivers it as keystrokes,
            // which still works, so a failure is ignored.
            let _ = crossterm::ExecutableCommand::execute(
                &mut std::io::stdout(),
                crossterm::event::EnableBracketedPaste,
            );
            crate::modes::mark(|modes| {
                modes.raw = true;
                modes.bracketed_paste = true;
            });
        }
        Self {
            composer: Composer::new(),
            active,
            prompt_width: DEFAULT_PROMPT_WIDTH,
            width: DEFAULT_WIDTH,
            menu_open: false,
            focus: Tri::Unknown,
            keymap: Keymap::defaults(),
            context: Context::Menu,
        }
    }

    /// Keeps canonical line input without enabling raw mode or terminal escapes.
    #[must_use]
    pub fn line_mode() -> Self {
        Self {
            composer: Composer::new(),
            active: false,
            prompt_width: DEFAULT_PROMPT_WIDTH,
            width: DEFAULT_WIDTH,
            menu_open: false,
            focus: Tri::Unknown,
            keymap: Keymap::defaults(),
            context: Context::Menu,
        }
    }

    /// Records how the draft is drawn, so Up and Down move by drawn rows.
    pub const fn set_layout(&mut self, prompt_width: usize, width: usize) {
        self.prompt_width = prompt_width;
        self.width = width;
    }

    /// Records whether a menu under the draft takes the vertical arrows.
    pub const fn set_menu_open(&mut self, open: bool) {
        self.menu_open = open;
    }

    /// Replaces the keys every action answers to.
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
    }

    /// Returns the keys every action answers to.
    #[must_use]
    pub const fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// Records where keys are being read, which decides what a key means.
    ///
    /// A reader starts in [`Context::Menu`]; the session names the idle
    /// prompt and the running turn around the reads it makes there.
    pub const fn set_context(&mut self, context: Context) {
        self.context = context;
    }

    /// Returns whether the terminal window has focus, as its last focus report
    /// said: [`Tri::Unknown`] until one arrives.
    #[must_use]
    pub const fn focus(&self) -> Tri {
        self.focus
    }

    /// Returns whether keys are being read directly.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// Returns the line being edited.
    #[must_use]
    pub fn line(&self) -> &str {
        self.composer.text()
    }

    /// Returns the displayed prefix width, counting each newline and tab as
    /// one column. A multiline renderer resolves it into the caret's row and
    /// column within that row.
    #[must_use]
    pub fn column(&self) -> usize {
        self.composer.cursor_column()
    }

    /// Returns the byte offset of the caret in the draft.
    #[must_use]
    pub fn cursor_byte(&self) -> usize {
        self.line()
            .char_indices()
            .nth(self.composer.cursor())
            .map_or(self.line().len(), |(offset, _)| offset)
    }

    /// Replaces a completed token as one undoable edit, preserving its suffix.
    pub fn complete_path(&mut self, range: std::ops::Range<usize>, replacement: &str) {
        self.composer.replace_range(range, replacement);
    }

    /// Inserts text typed before the reader started at the caret, as one
    /// undoable edit.
    pub fn type_ahead(&mut self, text: &str) {
        self.composer.insert(text);
    }

    /// Empties the line.
    pub fn clear(&mut self) {
        self.composer.clear();
    }

    /// Replaces the line with the previous entry of a prompt history.
    ///
    /// The entries are supplied by the caller rather than held here, because
    /// which prompts are worth recalling depends on the session, not on the
    /// line editor. Returns whether the line changed.
    pub fn recall_previous(&mut self, entries: &[String]) -> bool {
        self.composer.history_previous(entries)
    }

    /// Walks back toward the line that was being edited when recall started.
    ///
    /// Returns whether the line changed.
    pub fn recall_next(&mut self, entries: &[String]) -> bool {
        self.composer.history_next(entries)
    }

    /// Puts text on the line, replacing whatever is there.
    pub fn replace(&mut self, text: &str) {
        self.composer.set(text);
    }

    /// Runs a query with a separate composer, then restores the entire draft.
    /// Query edits cannot change the draft's caret, recall, kill, or undo state.
    pub fn with_temporary_line<T>(&mut self, action: impl FnOnce(&mut Self) -> T) -> T {
        let draft = std::mem::take(&mut self.composer);
        let result = action(self);
        self.composer = draft;
        result
    }

    /// Restores a selected prompt as one undoable draft edit.
    pub fn restore_prompt(&mut self, text: &str) {
        self.composer.replace_draft(text);
        self.composer.move_end();
    }

    /// Hands the terminal to VISUAL, EDITOR, or vi, then reloads the draft.
    /// Failed edits leave the complete composer untouched.
    pub fn edit_external(&mut self) -> std::io::Result<()> {
        let edited = if self.active {
            let suspension = TerminalSuspension::new()?;
            let result = crate::external_editor::edit(self.line());
            suspension.resume()?;
            result?
        } else {
            crate::external_editor::edit(self.line())?
        };
        let edited = edited.replace("\r\n", "\n").replace('\r', "\n");
        let edited = crate::transcript::sanitize(&edited);
        self.composer.replace_draft(&edited);
        Ok(())
    }

    /// Waits for one key and applies it.
    ///
    /// Blocks until a key arrives, so the caller can render between keystrokes
    /// without polling. A terminal that is not reporting keys returns
    /// [`KeyAction::Ignored`] immediately, and the caller falls back to reading
    /// whole lines.
    pub fn read_key(&mut self) -> KeyAction {
        if !self.active {
            return KeyAction::Ignored;
        }
        // Polled rather than blocking, so a resumed process redraws without
        // waiting for a key.
        loop {
            if crate::modes::redraw_pending() {
                return KeyAction::Ignored;
            }
            if crossterm::event::poll(REDRAW_POLL).unwrap_or(true) {
                break;
            }
        }
        let Ok(event) = crossterm::event::read() else {
            return KeyAction::Ignored;
        };
        self.handle(event).unwrap_or(KeyAction::Ignored)
    }

    /// Returns the next key if one is already waiting, without blocking.
    ///
    /// Used while a turn runs, where the reader cannot afford to wait for a key
    /// that may never come: the turn's own progress still has to be drawn.
    /// Returns `None` when nothing is ready.
    pub fn poll_key(&mut self, timeout: Duration) -> Option<KeyAction> {
        if !self.active || !crossterm::event::poll(timeout).unwrap_or(false) {
            return None;
        }
        let Ok(event) = crossterm::event::read() else {
            return None;
        };
        self.handle(event)
    }

    /// Polls a choice without changing the draft, caret, or editing history.
    ///
    /// Used by permission prompts, where typing and pasting must never become
    /// steering input or replace the correction being edited.
    pub fn poll_choice(&mut self, timeout: Duration) -> Option<KeyAction> {
        let draft = std::mem::take(&mut self.composer);
        let action = self.poll_key(timeout);
        self.composer = draft;
        action
    }

    /// Polls viewer navigation, leaving the complete composer untouched.
    pub fn poll_transcript(&mut self, timeout: Duration) -> Option<TranscriptAction> {
        if !self.active || !crossterm::event::poll(timeout).unwrap_or(false) {
            return None;
        }
        let event = crossterm::event::read().ok()?;
        self.note_focus(&event);
        Some(transcript_event(&self.keymap, &event))
    }

    /// Records a focus report.
    const fn note_focus(&mut self, event: &Event) {
        match event {
            Event::FocusGained => self.focus = Tri::Yes,
            Event::FocusLost => self.focus = Tri::No,
            _ => {}
        }
    }

    /// Applies one terminal event, returning `None` for one that is not input.
    ///
    /// Shared by both ways of reading, so a paste means the same thing whether
    /// it arrives at the prompt or while a turn runs.
    pub fn handle(&mut self, event: Event) -> Option<KeyAction> {
        match event {
            // A release also arrives on some terminals, and acting on it would
            // insert every character twice.
            Event::Key(key) if key.kind != KeyEventKind::Release => Some(self.apply(key)),
            Event::Paste(text) => {
                self.paste(&text);
                Some(KeyAction::Ignored)
            }
            // A focus report is not input: it is recorded, never typed.
            Event::FocusGained | Event::FocusLost => {
                self.note_focus(&event);
                None
            }
            _ => None,
        }
    }

    /// Inserts pasted text at the cursor, line breaks included.
    ///
    /// A terminal sends a pasted line break as a carriage return, so both
    /// spellings become a newline that stays part of the line. Escapes and
    /// other control characters are dropped, because the line is drawn back to
    /// the terminal and a pasted sequence would act on it.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        // Nothing is withheld: the line is what is submitted, and a marker in
        // place of the text would send the marker. The drawn row is cut to the
        // window whatever its length.
        self.composer
            .paste(&crate::transcript::sanitize(&text), usize::MAX);
    }

    /// Applies one key to the line.
    ///
    /// Split from reading so a test drives the same mapping a terminal does.
    /// The keymap decides first; a key no action holds keeps its fixed
    /// meaning or is typed.
    pub fn apply(&mut self, key: KeyEvent) -> KeyAction {
        match self.keymap.action(self.context, &key) {
            Some(id) => self.run_action(id),
            None => self.apply_unbound(key),
        }
    }

    /// Runs one action of the keymap.
    fn run_action(&mut self, id: &str) -> KeyAction {
        match id {
            "submit" => {
                if self.composer.newline_after_backslash() {
                    KeyAction::Ignored
                } else {
                    KeyAction::Submit
                }
            }
            "newline" => {
                self.composer.insert("\n");
                KeyAction::Ignored
            }
            // Tab completes rather than inserting a tab. Pasted tabs stay in
            // the draft and are displayed as spaces.
            "complete" => KeyAction::Complete,
            "queue" => KeyAction::Queue,
            "pull_queued" => KeyAction::PullQueued,
            "interrupt" => KeyAction::Escape,
            // While a turn runs, Escape is the interrupt gesture, so clearing
            // on another key only empties the draft.
            "clear" if self.context == Context::Turn => {
                self.composer.clear();
                KeyAction::Ignored
            }
            "clear" => KeyAction::Escape,
            "history_search" => KeyAction::HistorySearch,
            "transcript" => KeyAction::Transcript,
            "external_editor" => KeyAction::ExternalEditor,
            // An open menu keeps the key, so it never changes the mode behind it.
            "mode_cycle" if self.menu_open => KeyAction::Ignored,
            "mode_cycle" => KeyAction::ModeCycle,
            "line_start" => {
                if !self.composer.move_line_start() {
                    self.composer.move_home();
                }
                KeyAction::Ignored
            }
            "line_end" => {
                if !self.composer.move_line_end() {
                    self.composer.move_end();
                }
                KeyAction::Ignored
            }
            "kill_to_line_start" => {
                self.composer.kill_to_start();
                KeyAction::Ignored
            }
            "kill_to_line_end" => {
                self.composer.kill_to_end();
                KeyAction::Ignored
            }
            "yank" => {
                self.composer.yank();
                KeyAction::Ignored
            }
            "yank_pop" => {
                self.composer.yank_pop();
                KeyAction::Ignored
            }
            "undo" => {
                self.composer.undo();
                KeyAction::Ignored
            }
            "redo" => {
                self.composer.redo();
                KeyAction::Ignored
            }
            "kill_word_back" => {
                self.composer.kill_word_back();
                KeyAction::Ignored
            }
            "kill_word_forward" => {
                self.composer.kill_word_forward();
                KeyAction::Ignored
            }
            "word_left" => {
                self.composer.move_word_left();
                KeyAction::Ignored
            }
            "word_right" => {
                self.composer.move_word_right();
                KeyAction::Ignored
            }
            _ => KeyAction::Ignored,
        }
    }

    /// Applies a key that no action holds.
    fn apply_unbound(&mut self, key: KeyEvent) -> KeyAction {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Menus keep Tab and Escape even when their actions moved elsewhere,
        // so a picker can always be accepted and left.
        let menu = matches!(self.context, Context::Menu | Context::Approval);

        match (key.code, control, alt) {
            (KeyCode::Char('c'), true, _) => KeyAction::Cancel,
            #[cfg(unix)]
            (KeyCode::Char('z'), true, _) => KeyAction::Suspend,
            (KeyCode::Char('d'), true, _) => {
                if self.composer.is_empty() {
                    KeyAction::Interrupt
                } else {
                    self.composer.delete_forward();
                    KeyAction::Ignored
                }
            }
            (KeyCode::Char('t'), false, true) => KeyAction::CycleReasoning,
            (KeyCode::Tab, _, _) if menu => KeyAction::Complete,
            (KeyCode::Esc, _, _) if menu => KeyAction::Escape,
            (KeyCode::Backspace, _, _) => {
                self.composer.delete_back();
                KeyAction::Ignored
            }
            (KeyCode::Delete, _, _) => {
                self.composer.delete_forward();
                KeyAction::Ignored
            }
            (KeyCode::Left, _, _) => {
                self.composer.move_left();
                KeyAction::Ignored
            }
            (KeyCode::Right, _, _) => {
                self.composer.move_right();
                KeyAction::Ignored
            }
            (KeyCode::Up, _, true) => KeyAction::Ignored,
            // An open menu takes the vertical arrows. Otherwise they move
            // between drawn rows and are reported only from the first or last
            // row, where the session recalls an earlier prompt.
            (KeyCode::Up, _, _) => {
                if !self.menu_open
                    && self
                        .composer
                        .move_up(self.prompt_width, self.prompt_width, self.width)
                {
                    KeyAction::Ignored
                } else {
                    KeyAction::Up
                }
            }
            (KeyCode::Down, _, _) => {
                if !self.menu_open
                    && self
                        .composer
                        .move_down(self.prompt_width, self.prompt_width, self.width)
                {
                    KeyAction::Ignored
                } else {
                    KeyAction::Down
                }
            }
            // A control combination that is not a known binding must not insert
            // its letter, which is what a naive fallthrough would do.
            (KeyCode::Char(_), true, _) => KeyAction::Ignored,
            (KeyCode::Char(c), _, _) => {
                self.composer.insert(&c.to_string());
                KeyAction::Ignored
            }
            _ => KeyAction::Ignored,
        }
    }
}

impl Default for KeyReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Restores key input even when spawning or reading the editor fails.
struct TerminalSuspension(bool);

impl TerminalSuspension {
    fn new() -> std::io::Result<Self> {
        let guard = Self(true);
        crate::modes::pause_for_child();
        Ok(guard)
    }

    fn resume(mut self) -> std::io::Result<()> {
        Self::restore()?;
        self.0 = false;
        Ok(())
    }

    fn restore() -> std::io::Result<()> {
        crate::modes::reapply();
        Ok(())
    }
}

impl Drop for TerminalSuspension {
    fn drop(&mut self) {
        if self.0 {
            let _ = Self::restore();
        }
    }
}

impl Drop for KeyReader {
    fn drop(&mut self) {
        if self.active {
            restore_terminal();
            self.active = false;
        }
    }
}

/// What one key or paste does to a secret being read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SecretStep {
    /// Keep reading.
    Continue,
    /// Enter was pressed and the secret is complete.
    Submit,
    /// Input ended before anything was typed.
    End,
    /// Control-C was pressed.
    Interrupt,
}

/// Applies one terminal event to a secret being read.
///
/// A paste is taken whole with its line breaks dropped, because a key copied
/// with the end of its line is still one key, and it is Enter that finishes it.
pub fn secret_event(secret: &mut String, event: Event) -> SecretStep {
    let key = match event {
        Event::Paste(text) => {
            secret.extend(text.chars().filter(|c| !c.is_control()));
            return SecretStep::Continue;
        }
        Event::Key(key) if key.kind != KeyEventKind::Release => key,
        _ => return SecretStep::Continue,
    };
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match (key.code, control) {
        (KeyCode::Enter, _) => SecretStep::Submit,
        (KeyCode::Char('c'), true) => SecretStep::Interrupt,
        (KeyCode::Char('d'), true) if secret.is_empty() => SecretStep::End,
        (KeyCode::Char('u'), true) => {
            secret.clear();
            SecretStep::Continue
        }
        (KeyCode::Backspace, _) => {
            secret.pop();
            SecretStep::Continue
        }
        (KeyCode::Char(c), false) => {
            secret.push(c);
            SecretStep::Continue
        }
        _ => SecretStep::Continue,
    }
}

/// Reads one line from the terminal without showing it.
///
/// A credential that is echoed stays on screen and in the terminal's
/// scrollback. Raw mode keeps the terminal from echoing, and bracketed paste
/// delivers a pasted key in one piece, so a line break copied with it does not
/// end the read. Returns `None` when input ends before anything is typed, and
/// an interrupted error for Control-C.
pub fn read_secret() -> std::io::Result<Option<String>> {
    /// Puts the terminal back however the read ends.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            restore_terminal();
        }
    }

    crossterm::terminal::enable_raw_mode()?;
    let _restore = Restore;
    install_panic_hook();
    let _ = crossterm::ExecutableCommand::execute(
        &mut std::io::stdout(),
        crossterm::event::EnableBracketedPaste,
    );
    crate::modes::mark(|modes| {
        modes.raw = true;
        modes.bracketed_paste = true;
    });
    let mut secret = String::new();
    loop {
        match secret_event(&mut secret, crossterm::event::read()?) {
            SecretStep::Continue => {}
            SecretStep::Submit => return Ok(Some(secret)),
            SecretStep::End => return Ok(None),
            SecretStep::Interrupt => {
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
        }
    }
}

/// Undoes what the reader and the renderer change about the terminal.
///
/// The reader turns on raw mode and bracketed paste, and the renderer hides the
/// cursor while it writes a frame, so a process that stops partway through one
/// would otherwise leave the cursor hidden as well.
fn restore_terminal() {
    crate::modes::restore();
}

thread_local! {
    /// Only this thread's caught worker panic bypasses the ordinary hook.
    static CATCHING_WORKER_PANIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Catches a worker panic without writing into the renderer's live region.
///
/// The caller must discard the worker's partially mutated state on failure and
/// pass the returned panic payload to the renderer. Other threads and aborting
/// builds retain the ordinary panic hook and terminal restoration.
pub fn catch_worker_panic<T>(work: impl FnOnce() -> T) -> std::thread::Result<T> {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            CATCHING_WORKER_PANIC.set(self.0);
        }
    }

    install_panic_hook();
    let _restore = Restore(CATCHING_WORKER_PANIC.replace(true));
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
}

/// Restores the terminal before a panic ends the process.
///
/// The release build aborts on a panic, so nothing unwinds and the reader's
/// drop never runs: without this a panic leaves the shell in raw mode, with no
/// echo and a hidden cursor. A build that unwinds is left to the drop, because
/// a panic there can be caught, as a turn's worker's is, and the session then
/// goes on in the mode it needs. Caught workers return their diagnostic to the
/// renderer; every other panic still invokes the previous hook. An aborting
/// build restores the terminal before printing the fatal report.
fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // A caught worker panic is reported by the session, which goes on
            // in the modes it needs; every other panic restores first, so its
            // message is readable.
            if CATCHING_WORKER_PANIC.get() && !cfg!(panic = "abort") {
                return;
            }
            restore_terminal();
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn control(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn focus_events_update_focus_and_are_not_keys() {
        let mut reader = reader();
        typed(&mut reader, "draft");
        assert_eq!(reader.focus(), Tri::Unknown);
        assert_eq!(reader.handle(Event::FocusLost), None);
        assert_eq!(reader.focus(), Tri::No);
        assert_eq!(reader.handle(Event::FocusGained), None);
        assert_eq!(reader.focus(), Tri::Yes);
        assert_eq!(reader.line(), "draft");
        assert_eq!(reader.column(), 5);
        // A key afterwards still edits the line and leaves focus as it was.
        assert!(reader.handle(Event::Key(key(KeyCode::Char('s')))).is_some());
        assert_eq!(reader.line(), "drafts");
        assert_eq!(reader.focus(), Tri::Yes);
    }

    #[test]
    fn path_completion_preserves_unicode_suffix_and_is_one_undoable_edit() {
        let mut reader = reader();
        reader.replace("界 read fiOLD please e\u{301}");
        for _ in crate::width::graphemes("OLD please e\u{301}") {
            reader.apply(key(KeyCode::Left));
        }
        let original_caret = reader.cursor_byte();
        assert_eq!(original_caret, "界 read fi".len());
        let start = "界 read ".len();
        reader.complete_path(start..start + 5, "'file space.txt'");
        assert_eq!(reader.line(), "界 read 'file space.txt' please e\u{301}");
        assert_eq!(reader.cursor_byte(), "界 read 'file space.txt'".len());
        reader.apply(control('_'));
        assert_eq!(reader.line(), "界 read fiOLD please e\u{301}");
        assert_eq!(reader.cursor_byte(), original_caret);
        reader.apply(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT));
        assert_eq!(reader.line(), "界 read 'file space.txt' please e\u{301}");
        assert_eq!(reader.cursor_byte(), "界 read 'file space.txt'".len());
    }

    #[test]
    fn type_ahead_is_one_undoable_edit_at_the_caret() {
        let mut reader = reader();
        reader.type_ahead("");
        assert_eq!(reader.line(), "");
        reader.replace("ab");
        reader.apply(key(KeyCode::Left));
        reader.type_ahead("hello ");
        assert_eq!(reader.line(), "ahello b");
        assert_eq!(reader.cursor_byte(), "ahello ".len());
        reader.apply(control('_'));
        assert_eq!(reader.line(), "ab");
    }

    #[test]
    fn history_search_requests_a_picker_without_editing_or_submitting() {
        let mut reader = reader();
        reader.replace("draft 界");
        reader.apply(key(KeyCode::Left));
        let before = (reader.line().to_owned(), reader.cursor_byte());
        assert_eq!(
            reader.handle(Event::Key(control('r'))),
            Some(KeyAction::HistorySearch)
        );
        assert_eq!((reader.line().to_owned(), reader.cursor_byte()), before);
        let mut release = control('r');
        release.kind = KeyEventKind::Release;
        assert_eq!(reader.handle(Event::Key(release)), None);
        assert_eq!((reader.line().to_owned(), reader.cursor_byte()), before);
    }

    #[test]
    fn search_queries_preserve_draft_state_and_restoration_is_one_undoable_edit() {
        let mut reader = reader();
        reader.replace("draft 界");
        reader.apply(key(KeyCode::Left));
        reader.apply(key(KeyCode::Char('!')));
        let before = (reader.line().to_owned(), reader.cursor_byte());
        let result = reader.with_temporary_line(|query| {
            assert_eq!(query.line(), "");
            query.apply(key(KeyCode::Char('x')));
            query.apply(control('u'));
            Err::<(), _>("cancelled")
        });
        assert_eq!(result, Err("cancelled"));
        assert_eq!((reader.line().to_owned(), reader.cursor_byte()), before);
        reader.restore_prompt("older middle word\n界");
        assert_eq!(reader.line(), "older middle word\n界");
        assert_eq!(reader.cursor_byte(), reader.line().len());
        reader.apply(key(KeyCode::Left));
        reader.restore_prompt("older middle word\n界");
        assert_eq!(reader.cursor_byte(), reader.line().len());
        reader.apply(control('_'));
        assert_eq!((reader.line().to_owned(), reader.cursor_byte()), before);
        reader.apply(control('_'));
        assert_eq!(reader.line(), "draft 界");
    }

    #[test]
    fn editor_binding_requests_handoff_without_changing_the_draft() {
        let mut reader = reader();
        reader.replace("draft 界");
        reader.apply(key(KeyCode::Left));
        let before = (reader.line().to_owned(), reader.column());
        assert_eq!(
            reader.handle(Event::Key(control('g'))),
            Some(KeyAction::ExternalEditor)
        );
        assert_eq!((reader.line().to_owned(), reader.column()), before);
        let mut release = control('g');
        release.kind = KeyEventKind::Release;
        assert_eq!(reader.handle(Event::Key(release)), None);
        assert_eq!((reader.line().to_owned(), reader.column()), before);
    }

    #[test]
    fn transcript_shortcut_and_navigation_leave_the_complete_draft_untouched() {
        let mut reader = reader();
        let history = vec![String::from("earlier prompt")];
        reader.replace("draft 界 tail");
        reader.apply(key(KeyCode::Left));
        reader.recall_previous(&history);
        reader.recall_next(&history);
        let before = (reader.line().to_owned(), reader.column());
        assert_eq!(reader.apply(control('o')), KeyAction::Transcript);
        for (event, expected) in [
            (Event::Key(key(KeyCode::Up)), TranscriptAction::Up),
            (Event::Key(key(KeyCode::Down)), TranscriptAction::Down),
            (Event::Key(key(KeyCode::PageUp)), TranscriptAction::PageUp),
            (
                Event::Key(key(KeyCode::PageDown)),
                TranscriptAction::PageDown,
            ),
            (Event::Key(key(KeyCode::Home)), TranscriptAction::Home),
            (Event::Key(key(KeyCode::End)), TranscriptAction::End),
            (Event::Key(key(KeyCode::Tab)), TranscriptAction::NextTool),
            (
                Event::Key(key(KeyCode::BackTab)),
                TranscriptAction::PreviousTool,
            ),
            (
                Event::Key(key(KeyCode::Char(' '))),
                TranscriptAction::ToggleTool,
            ),
            (
                Event::Key(key(KeyCode::Char('x'))),
                TranscriptAction::Ignored,
            ),
            (
                Event::Paste(String::from("replace\nthis")),
                TranscriptAction::Ignored,
            ),
            (Event::Key(key(KeyCode::Enter)), TranscriptAction::Ignored),
            (Event::Resize(32, 8), TranscriptAction::Ignored),
            (Event::Key(key(KeyCode::Esc)), TranscriptAction::Close),
            (Event::Key(control('o')), TranscriptAction::Close),
            (Event::Key(control('c')), TranscriptAction::Close),
        ] {
            assert_eq!(transcript_event(&Keymap::defaults(), &event), expected);
            assert_eq!((reader.line().to_owned(), reader.column()), before);
        }
        reader.recall_previous(&history);
        reader.recall_next(&history);
        assert_eq!((reader.line().to_owned(), reader.column()), before);
    }

    /// Builds a reader with keys enabled, without touching a real terminal.
    fn reader() -> KeyReader {
        KeyReader {
            composer: Composer::new(),
            active: true,
            prompt_width: DEFAULT_PROMPT_WIDTH,
            width: DEFAULT_WIDTH,
            menu_open: false,
            focus: Tri::Unknown,
            keymap: Keymap::defaults(),
            context: Context::Menu,
        }
    }

    fn typed(reader: &mut KeyReader, text: &str) {
        for c in text.chars() {
            reader.apply(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn shift_enter_inserts_a_newline() {
        let mut reader = reader();
        typed(&mut reader, "a");
        let shift = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(reader.apply(shift), KeyAction::Ignored);
        typed(&mut reader, "b");
        assert_eq!(reader.line(), "a\nb");
    }

    #[test]
    fn ctrl_j_inserts_a_newline() {
        let mut reader = reader();
        typed(&mut reader, "a");
        assert_eq!(reader.apply(control('j')), KeyAction::Ignored);
        typed(&mut reader, "b");
        assert_eq!(reader.line(), "a\nb");
    }

    #[test]
    fn backslash_enter_replaces_the_backslash() {
        let enter_after = |text: &str| {
            let mut reader = reader();
            typed(&mut reader, text);
            let action = reader.apply(key(KeyCode::Enter));
            (action, reader.line().to_owned())
        };
        assert_eq!(enter_after("a\\"), (KeyAction::Ignored, "a\n".to_owned()));
        // A backslash alone becomes a newline too.
        assert_eq!(enter_after("\\"), (KeyAction::Ignored, "\n".to_owned()));
        // Two backslashes keep the first.
        assert_eq!(enter_after("\\\\"), (KeyAction::Ignored, "\\\n".to_owned()));
        // With the caret away from the backslash, Enter submits.
        assert_eq!(enter_after("a\\b"), (KeyAction::Submit, "a\\b".to_owned()));
        let mut middle = reader();
        typed(&mut middle, "a\\b");
        middle.apply(key(KeyCode::Left));
        assert_eq!(middle.apply(key(KeyCode::Enter)), KeyAction::Ignored);
        assert_eq!(middle.line(), "a\nb");
    }

    #[test]
    fn plain_enter_submits() {
        let mut reader = reader();
        typed(&mut reader, "a");
        assert_eq!(reader.apply(key(KeyCode::Enter)), KeyAction::Submit);
        assert_eq!(reader.line(), "a");
        // Control+Enter is not one of the newline keys.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        assert_eq!(reader.apply(enter), KeyAction::Submit);
    }

    #[test]
    fn alt_enter_still_inserts_a_newline() {
        let mut reader = reader();
        typed(&mut reader, "a");
        let alt = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(reader.apply(alt), KeyAction::Ignored);
        assert_eq!(reader.line(), "a\n");
    }

    #[test]
    fn alt_letters_edit_instead_of_inserting() {
        let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
        let mut reader = reader();
        typed(&mut reader, "alpha beta");
        reader.apply(alt('b'));
        assert_eq!(reader.column(), 6);
        reader.apply(alt('f'));
        assert_eq!(reader.column(), 10);
        reader.apply(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(reader.line(), "alpha ");
        reader.apply(control('y'));
        assert_eq!(reader.line(), "alpha beta");
        reader.apply(key(KeyCode::Home));
        reader.apply(alt('d'));
        assert_eq!(reader.line(), " beta");
        reader.apply(control('y'));
        assert_eq!(reader.line(), "alpha beta");
        reader.apply(alt('y'));
        assert_eq!(reader.line(), "beta beta");
        assert_eq!(reader.column(), 4);
    }

    #[test]
    fn alt_up_pulls_a_queued_message_without_moving_the_caret() {
        let mut reader = reader();
        reader.set_context(Context::Turn);
        typed(&mut reader, "one");
        reader.apply(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        typed(&mut reader, "two");
        let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(reader.apply(alt_up), KeyAction::PullQueued);
        assert_eq!(reader.column(), 7);
        assert_eq!(reader.apply(key(KeyCode::Up)), KeyAction::Ignored);
        assert_eq!(reader.column(), 3);
    }

    #[test]
    fn arrows_go_to_an_open_menu() {
        let mut reader = reader();
        typed(&mut reader, "one");
        reader.apply(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        typed(&mut reader, "two");
        reader.set_menu_open(true);
        assert_eq!(reader.apply(key(KeyCode::Up)), KeyAction::Up);
        assert_eq!(reader.column(), 7, "the caret stays while a menu is open");
        reader.set_menu_open(false);
        assert_eq!(reader.apply(key(KeyCode::Up)), KeyAction::Ignored);
        assert_eq!(reader.column(), 3);
        assert_eq!(reader.apply(key(KeyCode::Up)), KeyAction::Up);
        assert_eq!(reader.apply(key(KeyCode::Down)), KeyAction::Ignored);
        assert_eq!(reader.column(), 7);
        assert_eq!(reader.apply(key(KeyCode::Down)), KeyAction::Down);
        assert_eq!(reader.line(), "one\ntwo");
    }

    #[test]
    fn typing_builds_the_line() {
        let mut reader = reader();
        typed(&mut reader, "hello");
        assert_eq!(reader.line(), "hello");
        assert_eq!(reader.column(), 5);
    }

    #[test]
    fn enter_submits_and_escape_is_its_own_gesture() {
        // Escape is reported apart from Control-C because the two mean
        // different things while a turn runs: Escape arms a cancel that a
        // second press confirms, while Control-C cancels at once.
        let mut reader = reader();
        assert_eq!(reader.apply(key(KeyCode::Enter)), KeyAction::Submit);
        assert_eq!(reader.apply(key(KeyCode::Esc)), KeyAction::Escape);
        assert_ne!(
            reader.apply(key(KeyCode::Esc)),
            reader.apply(control('c')),
            "escape and control-c must be told apart"
        );
    }

    #[test]
    fn alt_enter_inserts_a_newline_at_the_caret_and_enter_submits_it() {
        let mut reader = reader();
        reader.replace("界tail");
        reader.apply(key(KeyCode::Home));
        reader.apply(key(KeyCode::Right));
        let newline = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(reader.handle(Event::Key(newline)), Some(KeyAction::Ignored));
        assert_eq!(reader.line(), "界\ntail");
        assert_eq!(reader.composer.cursor(), 2);
        assert_eq!(reader.column(), 3);

        // The inserted break is a regular draft edit, including its caret.
        reader.apply(control('_'));
        assert_eq!(reader.line(), "界tail");
        assert_eq!(reader.column(), 2);
        reader.apply(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT));
        assert_eq!(reader.line(), "界\ntail");
        assert_eq!(reader.column(), 3);
        typed(&mut reader, "second ");
        assert_eq!(
            reader.handle(Event::Key(key(KeyCode::Enter))),
            Some(KeyAction::Submit)
        );
        assert_eq!(reader.line(), "界\nsecond tail");
    }

    #[test]
    fn releasing_alt_enter_does_not_insert_another_newline() {
        let mut reader = reader();
        typed(&mut reader, "first");
        let mut newline = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(reader.handle(Event::Key(newline)), Some(KeyAction::Ignored));
        newline.kind = KeyEventKind::Release;
        assert_eq!(reader.handle(Event::Key(newline)), None);
        assert_eq!(reader.line(), "first\n");
        assert_eq!(reader.column(), 6);
    }

    #[test]
    fn control_c_cancels_and_control_d_leaves_an_empty_line() {
        let mut reader = reader();
        assert_eq!(reader.apply(control('c')), KeyAction::Cancel);
        assert_eq!(reader.apply(control('d')), KeyAction::Interrupt);
        typed(&mut reader, "x");
        // On a line with content, control-d is a forward delete rather than an
        // exit, which is what the shell it imitates does.
        assert_eq!(reader.apply(control('d')), KeyAction::Ignored);
    }

    #[test]
    fn backspace_removes_the_character_before_the_cursor() {
        let mut reader = reader();
        typed(&mut reader, "abc");
        reader.apply(key(KeyCode::Backspace));
        assert_eq!(reader.line(), "ab");
    }

    #[test]
    fn the_arrow_keys_move_the_cursor() {
        let mut reader = reader();
        typed(&mut reader, "abc");
        reader.apply(key(KeyCode::Left));
        assert_eq!(reader.column(), 2);
        reader.apply(key(KeyCode::Right));
        assert_eq!(reader.column(), 3);
        reader.apply(key(KeyCode::Home));
        assert_eq!(reader.column(), 0);
        reader.apply(key(KeyCode::End));
        assert_eq!(reader.column(), 3);
    }

    #[test]
    fn the_vertical_arrows_are_reported_rather_than_applied() {
        // They move a picker or recall a prompt, both of which the caller owns,
        // so the reader must not consume them as cursor movement.
        let mut reader = reader();
        typed(&mut reader, "abc");
        assert_eq!(reader.apply(key(KeyCode::Up)), KeyAction::Up);
        assert_eq!(reader.apply(key(KeyCode::Down)), KeyAction::Down);
        assert_eq!(reader.line(), "abc");
        assert_eq!(reader.column(), 3);
    }

    #[test]
    fn recalling_walks_the_history_and_stops_at_the_newest() {
        let history = vec!["first".to_owned(), "second".to_owned()];
        let mut reader = reader();
        assert!(reader.recall_previous(&history));
        assert_eq!(reader.line(), "second");
        assert!(reader.recall_previous(&history));
        assert_eq!(reader.line(), "first");
        // At the oldest entry the walk stops rather than wrapping.
        assert!(!reader.recall_previous(&history));
        assert_eq!(reader.line(), "first");
        assert!(reader.recall_next(&history));
        assert_eq!(reader.line(), "second");
    }

    #[test]
    fn recalling_restores_the_draft_when_it_walks_past_the_newest() {
        // A half-typed line must survive a look back through the history.
        let history = vec!["earlier".to_owned()];
        let mut reader = reader();
        typed(&mut reader, "half typed");
        reader.recall_previous(&history);
        assert_eq!(reader.line(), "earlier");
        reader.recall_next(&history);
        assert_eq!(reader.line(), "half typed");
    }

    #[test]
    fn an_empty_history_recalls_nothing() {
        let mut reader = reader();
        typed(&mut reader, "kept");
        assert!(!reader.recall_previous(&[]));
        assert_eq!(reader.line(), "kept");
    }

    #[test]
    fn text_is_inserted_at_the_cursor_rather_than_appended() {
        // The point of a line editor is that the caret is where typing goes.
        let mut reader = reader();
        typed(&mut reader, "ac");
        reader.apply(key(KeyCode::Left));
        reader.apply(key(KeyCode::Char('b')));
        assert_eq!(reader.line(), "abc");
        assert_eq!(reader.column(), 2);
    }

    #[test]
    fn a_control_key_never_inserts_its_letter() {
        // Without the explicit arm these letters reach the line and a keystroke
        // meant as a command becomes text.
        let mut reader = reader();
        for c in ['p', 'n', 'o', 'r', 't', 'y'] {
            reader.apply(control(c));
        }
        assert_eq!(reader.line(), "");
    }

    #[test]
    fn control_u_and_control_k_cut_both_sides_of_the_cursor() {
        let mut reader = reader();
        typed(&mut reader, "hello world");
        reader.apply(key(KeyCode::Home));
        for _ in 0..6 {
            reader.apply(key(KeyCode::Right));
        }
        reader.apply(control('k'));
        assert_eq!(reader.line(), "hello ");
        reader.apply(control('u'));
        assert_eq!(reader.line(), "");
    }

    #[test]
    fn control_y_restores_control_u_text_with_the_caret_at_its_end() {
        let mut reader = reader();
        typed(&mut reader, "abc");
        assert_eq!(
            reader.handle(Event::Key(control('u'))),
            Some(KeyAction::Ignored)
        );
        assert_eq!(reader.line(), "");
        assert_eq!(reader.column(), 0);

        assert_eq!(
            reader.handle(Event::Key(control('y'))),
            Some(KeyAction::Ignored)
        );
        assert_eq!(reader.line(), "abc");
        assert_eq!(reader.column(), 3);
    }

    #[test]
    fn control_y_inserts_control_k_text_at_the_caret() {
        let mut reader = reader();
        typed(&mut reader, "ab界");
        reader.apply(key(KeyCode::Left));
        reader.apply(control('k'));
        assert_eq!(reader.line(), "ab");
        reader.apply(key(KeyCode::Home));

        assert_eq!(reader.apply(control('y')), KeyAction::Ignored);
        assert_eq!(reader.line(), "界ab");
        assert_eq!(reader.column(), 2);
    }

    #[test]
    fn control_y_without_killed_text_leaves_the_draft_and_caret_unchanged() {
        let mut reader = reader();
        typed(&mut reader, "abc");
        reader.apply(key(KeyCode::Left));

        assert_eq!(reader.apply(control('y')), KeyAction::Ignored);
        assert_eq!(reader.line(), "abc");
        assert_eq!(reader.column(), 2);
    }

    #[test]
    fn control_w_removes_the_word_before_the_cursor() {
        let mut reader = reader();
        typed(&mut reader, "one two");
        reader.apply(control('w'));
        assert_eq!(reader.line(), "one ");
    }

    #[test]
    fn undo_binding_restores_unicode_edits_and_their_carets() {
        for binding in [
            control('_'),
            control('7'),
            KeyEvent::new(
                KeyCode::Char('_'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        ] {
            for grapheme in ["e\u{301}", "👍🏽", "👩‍💻", "界"] {
                for deletion in [KeyCode::Backspace, KeyCode::Delete] {
                    let mut reader = reader();
                    reader.replace("界ab");
                    reader.apply(key(KeyCode::Left));
                    let original = (reader.line().to_owned(), reader.composer.cursor());
                    reader.handle(Event::Paste(grapheme.to_owned()));
                    if deletion == KeyCode::Delete {
                        reader.apply(key(KeyCode::Left));
                    }
                    let inserted = (reader.line().to_owned(), reader.composer.cursor());
                    let inserted_column = reader.column();
                    reader.handle(Event::Key(key(deletion)));
                    assert_eq!(reader.line(), original.0);
                    // Undo restores the saved caret, even after navigation.
                    reader.apply(key(KeyCode::Home));

                    assert_eq!(reader.handle(Event::Key(binding)), Some(KeyAction::Ignored));
                    assert_eq!(reader.line(), inserted.0);
                    assert_eq!(reader.composer.cursor(), inserted.1);
                    assert_eq!(reader.column(), inserted_column);

                    assert_eq!(reader.handle(Event::Key(binding)), Some(KeyAction::Ignored));
                    assert_eq!(reader.line(), original.0);
                    assert_eq!(reader.composer.cursor(), original.1);
                    assert_eq!(reader.column(), 3);
                    // An exhausted undo history leaves the draft untouched.
                    assert_eq!(reader.handle(Event::Key(binding)), Some(KeyAction::Ignored));
                    assert_eq!(reader.line(), original.0);
                    assert_eq!(reader.composer.cursor(), original.1);
                }
            }
        }
    }

    #[test]
    fn releasing_undo_does_not_revert_another_edit() {
        let mut reader = reader();
        reader.replace("ab");
        reader.apply(key(KeyCode::Left));
        reader.handle(Event::Key(key(KeyCode::Char('x'))));
        reader.handle(Event::Key(key(KeyCode::Char('界'))));
        let mut undo = control('_');
        assert_eq!(reader.handle(Event::Key(undo)), Some(KeyAction::Ignored));
        assert_eq!(reader.line(), "axb");
        assert_eq!(reader.column(), 2);
        undo.kind = KeyEventKind::Release;
        assert_eq!(reader.handle(Event::Key(undo)), None);
        assert_eq!(reader.line(), "axb");
        assert_eq!(reader.column(), 2);
    }

    #[test]
    fn alt_r_redoes_unicode_edits_and_restores_their_carets() {
        let redo = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT);
        for grapheme in ["e\u{301}", "👍🏽", "👩‍💻", "界"] {
            for deletion in [KeyCode::Backspace, KeyCode::Delete] {
                let mut reader = reader();
                reader.replace("界ab");
                reader.apply(key(KeyCode::Left));
                reader.handle(Event::Paste(grapheme.to_owned()));
                if deletion == KeyCode::Delete {
                    reader.apply(key(KeyCode::Left));
                }
                let inserted = (
                    reader.line().to_owned(),
                    reader.composer.cursor(),
                    reader.column(),
                );
                reader.handle(Event::Key(key(deletion)));
                let deleted = (
                    reader.line().to_owned(),
                    reader.composer.cursor(),
                    reader.column(),
                );
                reader.handle(Event::Key(control('_')));
                reader.handle(Event::Key(control('_')));
                assert_eq!(reader.line(), "界ab");
                // Moving after undo must not change the caret restored by redo.
                reader.apply(key(KeyCode::Home));

                for expected in [inserted, deleted.clone(), deleted] {
                    assert_eq!(reader.handle(Event::Key(redo)), Some(KeyAction::Ignored));
                    assert_eq!(reader.line(), expected.0);
                    assert_eq!(reader.composer.cursor(), expected.1);
                    assert_eq!(reader.column(), expected.2);
                }
            }
        }
    }

    #[test]
    fn releasing_alt_r_does_not_redo_another_edit() {
        let mut reader = reader();
        reader.replace("ab");
        reader.apply(key(KeyCode::Left));
        typed(&mut reader, "x界");
        reader.apply(control('_'));
        reader.apply(control('_'));
        let mut redo = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT);
        assert_eq!(reader.handle(Event::Key(redo)), Some(KeyAction::Ignored));
        assert_eq!(reader.line(), "axb");
        assert_eq!(reader.column(), 2);
        redo.kind = KeyEventKind::Release;
        assert_eq!(reader.handle(Event::Key(redo)), None);
        assert_eq!(reader.line(), "axb");
        assert_eq!(reader.column(), 2);
    }

    #[test]
    fn alt_r_without_redo_history_leaves_the_draft_and_caret_unchanged() {
        let mut reader = reader();
        reader.replace("ab");
        reader.apply(key(KeyCode::Left));
        let redo = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT);
        assert_eq!(reader.handle(Event::Key(redo)), Some(KeyAction::Ignored));
        assert_eq!(reader.line(), "ab");
        assert_eq!(reader.column(), 1);

        typed(&mut reader, "x");
        reader.apply(control('_'));
        typed(&mut reader, "界");
        assert_eq!(reader.handle(Event::Key(redo)), Some(KeyAction::Ignored));
        assert_eq!(reader.line(), "a界b");
        assert_eq!(reader.column(), 3);
    }

    #[test]
    fn alt_arrows_move_by_word() {
        let mut reader = reader();
        typed(&mut reader, "one two");
        let mut word_left = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT);
        reader.apply(word_left);
        assert_eq!(reader.column(), 4);
        word_left = KeyEvent::new(KeyCode::Right, KeyModifiers::ALT);
        reader.apply(word_left);
        assert_eq!(reader.column(), 7);
    }

    #[test]
    fn clearing_empties_the_line() {
        let mut reader = reader();
        typed(&mut reader, "something");
        reader.clear();
        assert_eq!(reader.line(), "");
        assert_eq!(reader.column(), 0);
    }

    #[test]
    fn a_pasted_block_stays_on_the_line_rather_than_submitting_it() {
        // Without bracketed paste every line break in a pasted stack trace is
        // an Enter, so the first line starts a turn and each later line is
        // sent on its own.
        let mut reader = reader();
        typed(&mut reader, "why: ");
        let action = reader.handle(Event::Paste(
            "first line\r\n\tsecond line\rthird line".to_owned(),
        ));
        assert_eq!(action, Some(KeyAction::Ignored));
        assert_eq!(
            reader.line(),
            "why: first line\n\tsecond line\nthird line",
            "the pasted text was not kept whole"
        );
        // The pasted tab is text, not a request to complete.
        assert!(reader.line().contains('\t'));
    }

    #[test]
    fn a_paste_is_inserted_at_the_cursor() {
        let mut reader = reader();
        typed(&mut reader, "ac");
        reader.apply(key(KeyCode::Left));
        reader.handle(Event::Paste("b\nb".to_owned()));
        assert_eq!(reader.line(), "ab\nbc");
    }

    #[test]
    fn a_pasted_escape_sequence_does_not_reach_the_line() {
        // The line is drawn back to the terminal, so a sequence in it would act.
        let mut reader = reader();
        reader.handle(Event::Paste("a\u{1b}]52;c;eA==\u{7}b\u{1b}[2Jc".to_owned()));
        assert_eq!(reader.line(), "abc");
    }

    #[test]
    fn a_pasted_line_is_drawn_on_one_row_with_the_caret_after_it() {
        // A raw line break written to a terminal in raw mode moves down without
        // returning, so the row would spill into the rows below it.
        let mut reader = reader();
        reader.handle(Event::Paste("one\ntwo".to_owned()));
        let row = crate::transcript::render_prompt("> ", reader.line(), 80);
        assert!(!row.contains('\n'), "{row:?}");
        assert_eq!(
            crate::width::str_width(&row),
            2 + reader.column(),
            "the caret is not after the drawn text: {row:?}"
        );
    }

    #[test]
    fn a_secret_is_built_from_keys_and_pastes_and_ends_at_enter() {
        let mut secret = String::new();
        for c in "sk-ab".chars() {
            assert_eq!(
                secret_event(&mut secret, Event::Key(key(KeyCode::Char(c)))),
                SecretStep::Continue
            );
        }
        secret_event(&mut secret, Event::Key(key(KeyCode::Backspace)));
        secret_event(&mut secret, Event::Paste("cd\r\n".to_owned()));
        assert_eq!(
            secret_event(&mut secret, Event::Key(key(KeyCode::Enter))),
            SecretStep::Submit
        );
        assert_eq!(secret, "sk-acd");
    }

    #[test]
    fn a_secret_read_stops_at_control_c_and_at_the_end_of_input() {
        let mut secret = String::new();
        assert_eq!(
            secret_event(&mut secret, Event::Key(control('d'))),
            SecretStep::End
        );
        secret.push_str("typed");
        // With text on the line, Control-D is not the end of input.
        assert_eq!(
            secret_event(&mut secret, Event::Key(control('d'))),
            SecretStep::Continue
        );
        secret_event(&mut secret, Event::Key(control('u')));
        assert_eq!(secret, "");
        assert_eq!(
            secret_event(&mut secret, Event::Key(control('c'))),
            SecretStep::Interrupt
        );
    }

    #[test]
    fn worker_panic_hook_is_scoped_to_the_catching_thread() {
        if std::env::var_os("RUNE_R059_HOOK_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "input::tests::worker_panic_hook_is_scoped_to_the_catching_thread",
                    "--nocapture",
                ])
                .env("RUNE_R059_HOOK_CHILD", "1")
                .output()
                .expect("hook fixture subprocess");
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        // Only this subprocess changes the global hook. Fatal/unmanaged panics
        // must keep reaching it while a different worker is being caught.
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recorded = std::sync::Arc::clone(&calls);
        std::panic::set_hook(Box::new(move |_| {
            recorded.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }));
        assert!(
            catch_worker_panic(|| {
                assert!(catch_worker_panic(|| panic!("nested caught panic")).is_err());
                assert!(CATCHING_WORKER_PANIC.get());
                assert!(
                    std::thread::spawn(|| panic!("other thread"))
                        .join()
                        .is_err()
                );
                panic!("caught worker panic");
            })
            .is_err()
        );
        assert!(!CATCHING_WORKER_PANIC.get());
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(catch_worker_panic(|| 42).expect("successful work"), 42);
        assert!(!CATCHING_WORKER_PANIC.get());
        assert!(std::panic::catch_unwind(|| panic!("ordinary panic")).is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn restoring_turns_paste_off_and_shows_the_cursor() {
        // What a panic hook writes before an aborting build ends the process;
        // raw mode is restored beside it through the terminal settings.
        let out = crate::modes::restore_bytes(&crate::modes::ModeState {
            raw: true,
            bracketed_paste: true,
            ..crate::modes::ModeState::default()
        });
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\u{1b}[?2004l"), "{text:?}");
        assert!(text.contains("\u{1b}[?25h"), "{text:?}");
    }

    #[test]
    fn ctrl_z_is_a_suspend_key() {
        let mut reader = reader();
        reader.replace("draft");
        let action = reader.apply(control('z'));
        if cfg!(unix) {
            assert_eq!(action, KeyAction::Suspend);
        } else {
            assert_eq!(action, KeyAction::Ignored);
        }
        assert_eq!(reader.line(), "draft");
    }

    #[test]
    fn alt_t_cycles_reasoning_and_inserts_nothing() {
        let mut reader = reader();
        reader.replace("draft");
        let action = reader.apply(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT));
        assert_eq!(action, KeyAction::CycleReasoning);
        assert_eq!(reader.line(), "draft");
        assert!(
            render_bindings_help(true, &Keymap::defaults())
                .contains("Alt-T         cycle how reasoning is shown")
        );
    }

    #[test]
    fn a_rebound_queue_key_returns_the_queue_action() {
        let mut reader = reader();
        reader.set_context(Context::Turn);
        let tab = key(KeyCode::Tab);
        let control_q = control('q');
        assert_eq!(reader.apply(tab), KeyAction::Queue);
        assert_eq!(reader.apply(control_q), KeyAction::Ignored);

        let mut file = std::collections::BTreeMap::new();
        file.insert("queue".to_owned(), vec!["ctrl+q".to_owned()]);
        reader.set_keymap(Keymap::with_overrides(&file).expect("valid"));
        assert_eq!(reader.apply(control_q), KeyAction::Queue);
        assert_eq!(reader.apply(tab), KeyAction::Complete);
        reader.set_context(Context::Editor);
        assert_eq!(reader.apply(control_q), KeyAction::Ignored);
        assert_eq!(reader.apply(tab), KeyAction::Complete);
        assert_eq!(reader.line(), "");
    }

    #[test]
    fn a_rebound_clear_key_leaves_escape_to_menus_only() {
        let mut file = std::collections::BTreeMap::new();
        file.insert("clear".to_owned(), vec!["ctrl+x".to_owned()]);
        file.insert("interrupt".to_owned(), vec!["ctrl+t".to_owned()]);
        let mut reader = reader();
        reader.set_keymap(Keymap::with_overrides(&file).expect("valid"));
        let escape = key(KeyCode::Esc);
        reader.set_context(Context::Editor);
        assert_eq!(reader.apply(escape), KeyAction::Ignored);
        assert_eq!(reader.apply(control('x')), KeyAction::Escape);
        reader.set_context(Context::Menu);
        assert_eq!(reader.apply(escape), KeyAction::Escape);
        reader.set_context(Context::Turn);
        typed(&mut reader, "draft");
        assert_eq!(reader.apply(escape), KeyAction::Ignored);
        assert_eq!(reader.line(), "draft");
        assert_eq!(reader.apply(control('t')), KeyAction::Escape);
        assert_eq!(reader.apply(control('x')), KeyAction::Ignored);
        assert_eq!(reader.line(), "");
    }

    #[test]
    fn the_bindings_help_shows_the_keys_in_use() {
        let help = render_bindings_help(true, &Keymap::defaults());
        assert!(help.contains("Shift-Tab"), "{help}");
        assert!(help.contains("Home / Ctrl-A"), "{help}");
        assert!(help.contains("(mode_cycle)"), "{help}");
        let mut file = std::collections::BTreeMap::new();
        file.insert("transcript".to_owned(), vec!["ctrl+s".to_owned()]);
        let help = render_bindings_help(true, &Keymap::with_overrides(&file).expect("valid"));
        let row = help
            .lines()
            .find(|line| line.ends_with("(transcript)"))
            .expect("a transcript row");
        assert!(row.contains("Ctrl-S") && !row.contains("Ctrl-O"), "{row}");
    }

    #[test]
    fn shift_tab_cycles_modes() {
        let mut reader = reader();
        reader.replace("draft");
        let back_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(reader.apply(back_tab), KeyAction::ModeCycle);
        assert_eq!(reader.line(), "draft");
        reader.set_menu_open(true);
        assert_eq!(reader.apply(back_tab), KeyAction::Ignored);
        reader.set_menu_open(false);
        assert_eq!(reader.apply(back_tab), KeyAction::ModeCycle);
    }

    #[test]
    fn a_resize_is_not_input() {
        let mut reader = reader();
        assert_eq!(reader.handle(Event::Resize(80, 24)), None);
        assert_eq!(reader.line(), "");
    }

    #[test]
    fn a_wide_character_advances_the_column_by_its_display_width() {
        // The column is a display column, which is what places the cursor.
        let mut reader = reader();
        typed(&mut reader, "書");
        assert_eq!(reader.column(), 2);
    }
}
