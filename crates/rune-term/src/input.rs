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
    Cancel,
    /// The user moved the selection up.
    Up,
    /// The user moved the selection down.
    Down,
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
    #[must_use]
    pub fn new() -> Self {
        let active =
            std::io::stdin().is_terminal() && crossterm::terminal::enable_raw_mode().is_ok();
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
        let Event::Key(key) = event else {
            return KeyAction::Ignored;
        };
        // A release also arrives on some terminals, and acting on it would
        // insert every character twice.
        if key.kind == KeyEventKind::Release {
            return KeyAction::Ignored;
        }
        self.apply(key)
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
            (KeyCode::Esc, _, _) => KeyAction::Cancel,
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
            let _ = crossterm::terminal::disable_raw_mode();
            self.active = false;
        }
    }
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
    fn enter_submits_and_escape_cancels() {
        let mut reader = reader();
        assert_eq!(reader.apply(key(KeyCode::Enter)), KeyAction::Submit);
        assert_eq!(reader.apply(key(KeyCode::Esc)), KeyAction::Cancel);
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
    fn a_wide_character_advances_the_column_by_its_display_width() {
        // The column is a display column, which is what places the cursor.
        let mut reader = reader();
        typed(&mut reader, "書");
        assert_eq!(reader.column(), 2);
    }
}
