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
}

/// Reads keys and drives a composer.
///
/// Restores the terminal on drop, including when the process is unwinding, so a
/// session that ends for any reason does not leave the terminal without echo.
#[derive(Debug)]
pub struct KeyReader {
    composer: Composer,
    active: bool,
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
            // A terminal that cannot bracket a paste delivers it as keystrokes,
            // which still works, so a failure is ignored.
            let _ = crossterm::ExecutableCommand::execute(
                &mut std::io::stdout(),
                crossterm::event::EnableBracketedPaste,
            );
        }
        Self {
            composer: Composer::new(),
            active,
        }
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

    /// Returns the cursor as a count of columns from the start of the line.
    #[must_use]
    pub fn column(&self) -> usize {
        self.composer.cursor_column()
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
    pub fn apply(&mut self, key: KeyEvent) -> KeyAction {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match (key.code, control, alt) {
            (KeyCode::Char('c'), true, _) => KeyAction::Cancel,
            (KeyCode::Char('d'), true, _) => {
                if self.composer.is_empty() {
                    KeyAction::Interrupt
                } else {
                    self.composer.delete_forward();
                    KeyAction::Ignored
                }
            }
            (KeyCode::Char('a'), true, _) => {
                self.composer.move_home();
                KeyAction::Ignored
            }
            (KeyCode::Char('e'), true, _) => {
                self.composer.move_end();
                KeyAction::Ignored
            }
            (KeyCode::Char('u'), true, _) => {
                self.composer.kill_to_start();
                KeyAction::Ignored
            }
            (KeyCode::Char('k'), true, _) => {
                self.composer.kill_to_end();
                KeyAction::Ignored
            }
            (KeyCode::Char('w'), true, _) => {
                self.composer.delete_word();
                KeyAction::Ignored
            }
            (KeyCode::Char('b'), true, _) | (KeyCode::Left, _, true) => {
                self.composer.move_word_left();
                KeyAction::Ignored
            }
            (KeyCode::Char('f'), true, _) | (KeyCode::Right, _, true) => {
                self.composer.move_word_right();
                KeyAction::Ignored
            }
            (KeyCode::Enter, _, _) => KeyAction::Submit,
            // Tab completes rather than inserting a tab: a prompt is a single
            // line, so a tab character has nothing to align.
            (KeyCode::Tab, _, _) => KeyAction::Complete,
            (KeyCode::Esc, _, _) => KeyAction::Escape,
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
            // The vertical arrows are reported rather than applied, because
            // what they mean depends on what is on screen: a picker moves its
            // selection, and a plain line recalls an earlier prompt.
            (KeyCode::Up, _, _) => KeyAction::Up,
            (KeyCode::Down, _, _) => KeyAction::Down,
            (KeyCode::Home, _, _) => {
                self.composer.move_home();
                KeyAction::Ignored
            }
            (KeyCode::End, _, _) => {
                self.composer.move_end();
                KeyAction::Ignored
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
    write_restore(&mut std::io::stdout());
    let _ = crossterm::terminal::disable_raw_mode();
}

/// Writes the sequences that turn bracketed paste off and show the cursor.
fn write_restore(out: &mut impl std::io::Write) {
    let _ = crossterm::QueueableCommand::queue(out, crossterm::event::DisableBracketedPaste);
    let _ = crossterm::QueueableCommand::queue(out, crossterm::cursor::Show);
    let _ = out.flush();
}

/// Restores the terminal before a panic ends the process.
///
/// The release build aborts on a panic, so nothing unwinds and the reader's
/// drop never runs: without this a panic leaves the shell in raw mode, with no
/// echo and a hidden cursor. A build that unwinds is left to the drop, because
/// a panic there can be caught, as a turn's worker's is, and the session then
/// goes on in the mode it needs. The previous hook still runs, after the
/// terminal is back, so its report is printed with ordinary line endings.
fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if cfg!(panic = "abort") {
                restore_terminal();
            }
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

    /// Builds a reader with keys enabled, without touching a real terminal.
    fn reader() -> KeyReader {
        KeyReader {
            composer: Composer::new(),
            active: true,
        }
    }

    fn typed(reader: &mut KeyReader, text: &str) {
        for c in text.chars() {
            reader.apply(key(KeyCode::Char(c)));
        }
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
    fn control_w_removes_the_word_before_the_cursor() {
        let mut reader = reader();
        typed(&mut reader, "one two");
        reader.apply(control('w'));
        assert_eq!(reader.line(), "one ");
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
    fn restoring_turns_paste_off_and_shows_the_cursor() {
        // What a panic hook writes before an aborting build ends the process;
        // raw mode is restored beside it through the terminal settings.
        let mut out: Vec<u8> = Vec::new();
        write_restore(&mut out);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\u{1b}[?2004l"), "{text:?}");
        assert!(text.contains("\u{1b}[?25h"), "{text:?}");
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
