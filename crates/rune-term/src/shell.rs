//! The interactive shell.
//!
//! Drives the turn loop against a terminal: reads input, renders frames, and
//! handles approval prompts. The renderer and the agent are both supplied, so
//! this module owns only the loop that connects them.
//!
//! The loop never takes the alternate screen. Output is written inline and
//! finished content is promoted into the terminal's own scrollback, which is what
//! keeps a long session readable with the terminal's own search and copy.

use std::io::BufRead;
use std::time::Duration;

use rune_core::error::Result;

/// What the shell should do after handling one line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Keep going.
    Continue,
    /// Leave the shell.
    Exit,
}

/// A command typed by the user rather than sent to the model.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Input {
    /// Text to send to the model.
    Prompt(String),
    /// A slash command with its arguments.
    Command {
        /// The command name without the leading slash.
        name: String,
        /// Everything after the name.
        arguments: String,
    },
    /// Nothing to do.
    Empty,
}

impl Input {
    /// Classifies a line of input.
    ///
    /// A leading slash is a command only when it is followed by a letter, so a
    /// path typed as a prompt is not mistaken for one.
    #[must_use]
    pub fn parse(line: &str) -> Self {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Self::Empty;
        }
        if let Some(rest) = trimmed.strip_prefix('/') {
            let mut parts = rest.splitn(2, char::is_whitespace);
            let name = parts.next().unwrap_or_default();
            let arguments = parts.next().unwrap_or_default().trim().to_owned();
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Self::Command {
                    name: name.to_owned(),
                    arguments,
                };
            }
        }
        Self::Prompt(trimmed.to_owned())
    }
}

/// How a shell exits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExitReason {
    /// The input stream ended.
    EndOfInput,
    /// The user asked to leave.
    Requested,
    /// The user interrupted twice.
    Interrupted,
}

impl ExitReason {
    /// Returns the process exit code.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::EndOfInput | Self::Requested => 0,
            Self::Interrupted => 130,
        }
    }
}

/// How long the shell waits between checking for input and rendering.
///
/// Small enough that a keystroke feels immediate, large enough that an idle
/// session does not spin a core.
pub const POLL_INTERVAL: Duration = Duration::from_millis(8);

/// The window in which a second interrupt is treated as a request to exit.
///
/// A single interrupt cancels the running turn; a second one within the window
/// leaves the shell, which is what a user reaching for Ctrl-C twice expects.
pub const INTERRUPT_WINDOW: Duration = Duration::from_millis(1000);

/// Tracks repeated interrupts.
#[derive(Debug, Default)]
pub struct Interrupts {
    last: Option<std::time::Instant>,
}

impl Interrupts {
    /// Records an interrupt and reports whether it completes the gesture.
    ///
    /// Returns true when a previous interrupt happened inside the window, which
    /// means the user asked to leave rather than to cancel.
    pub fn record(&mut self) -> bool {
        self.record_at(std::time::Instant::now())
    }

    /// Records an interrupt at a given time, for tests.
    pub fn record_at(&mut self, now: std::time::Instant) -> bool {
        let repeated = self
            .last
            .is_some_and(|previous| now.duration_since(previous) <= INTERRUPT_WINDOW);
        self.last = if repeated { None } else { Some(now) };
        repeated
    }

    /// Clears the gesture, used when the running turn finishes normally.
    pub fn clear(&mut self) {
        self.last = None;
    }
}

/// Reads lines from a source.
pub trait InputSource {
    /// Returns the next line, or `None` at end of input.
    fn next_line(&mut self) -> Result<Option<String>>;
}

/// Reads lines from a buffered reader.
#[derive(Debug)]
pub struct StdinSource<R: BufRead> {
    reader: R,
}

impl<R: BufRead> StdinSource<R> {
    /// Wraps a reader.
    pub fn new(reader: R) -> Self {
        Self { reader }
    }
}

impl<R: BufRead> InputSource for StdinSource<R> {
    fn next_line(&mut self) -> Result<Option<String>> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(line)),
            Err(err) => Err(err.into()),
        }
    }
}

/// Collects lines from a fixed list, for tests.
#[derive(Debug, Default)]
pub struct ScriptedSource {
    lines: std::collections::VecDeque<String>,
}

impl ScriptedSource {
    /// Builds a source from a list of lines.
    #[must_use]
    pub fn new<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            lines: lines.into_iter().map(Into::into).collect(),
        }
    }
}

impl InputSource for ScriptedSource {
    fn next_line(&mut self) -> Result<Option<String>> {
        Ok(self.lines.pop_front())
    }
}

/// A run of the shell over one input source.
///
/// The loop owns no output handle. A caller that wants to write during a turn
/// does so through whatever it already holds, which keeps the shell from
/// borrowing the same stream the handler needs.
pub struct Shell<'a> {
    source: &'a mut dyn InputSource,
    interrupts: Interrupts,
    seen: usize,
}

impl std::fmt::Debug for Shell<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shell")
            .field("interrupts", &self.interrupts)
            .field("seen", &self.seen)
            .finish_non_exhaustive()
    }
}

impl<'a> Shell<'a> {
    /// Builds a shell over an input source.
    pub fn new(source: &'a mut dyn InputSource) -> Self {
        Self {
            source,
            interrupts: Interrupts::default(),
            seen: 0,
        }
    }

    /// Returns how many prompts and commands have been accepted.
    #[must_use]
    pub const fn seen(&self) -> usize {
        self.seen
    }

    /// Returns the interrupt tracker.
    pub fn interrupts(&mut self) -> &mut Interrupts {
        &mut self.interrupts
    }

    /// Runs until the input ends, a command leaves, or two interrupts arrive.
    ///
    /// Each accepted line is handed to `handle`, which returns the next action.
    /// The loop itself makes no decisions about what a command means, so a
    /// command added later does not change this function.
    pub fn run<F>(&mut self, mut handle: F) -> Result<ExitReason>
    where
        F: FnMut(Input) -> Result<Action>,
    {
        loop {
            let Some(line) = self.source.next_line()? else {
                return Ok(ExitReason::EndOfInput);
            };

            let input = Input::parse(&line);
            if input == Input::Empty {
                continue;
            }
            self.seen = self.seen.saturating_add(1);
            match handle(input)? {
                Action::Continue => {
                    // A completed exchange clears a pending interrupt, so the
                    // next one starts a fresh gesture.
                    self.interrupts.clear();
                }
                Action::Exit => return Ok(ExitReason::Requested),
            }
        }
    }
}

/// Renders the prompt shown while waiting for input.
///
/// A single character, because a prompt that grows competes with the answer for
/// the reader's attention.
#[must_use]
pub fn prompt() -> &'static str {
    "> "
}

/// Returns the terminal size, when one reports it.
///
/// Read through the terminal interface rather than an environment variable,
/// because the variables are not set for every terminal. A terminal that does
/// not answer yields `None`, so a caller supplies its own default rather than
/// composing a frame with no room.
#[must_use]
pub fn terminal_size() -> Option<(u16, u16)> {
    crossterm::terminal::size().ok()
}

/// Renders the notice shown when a turn is cancelled.
#[must_use]
pub fn cancelled_notice() -> &'static str {
    "cancelled"
}

/// Renders the notice shown when a turn is interrupted.
#[must_use]
pub fn interrupt_notice(remaining: bool) -> &'static str {
    if remaining {
        "interrupted"
    } else {
        "interrupted; press again to leave"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_line_is_a_prompt() {
        assert_eq!(
            Input::parse("explain this file"),
            Input::Prompt("explain this file".to_owned())
        );
    }

    #[test]
    fn a_leading_slash_is_a_command() {
        assert_eq!(
            Input::parse("/help"),
            Input::Command {
                name: "help".to_owned(),
                arguments: String::new()
            }
        );
    }

    #[test]
    fn a_command_carries_its_arguments() {
        assert_eq!(
            Input::parse("/model vendor/name"),
            Input::Command {
                name: "model".to_owned(),
                arguments: "vendor/name".to_owned()
            }
        );
    }

    #[test]
    fn a_slash_followed_by_a_non_letter_is_not_a_command() {
        // A path typed as a prompt must reach the model rather than being
        // swallowed as an unknown command.
        assert_eq!(
            Input::parse("/usr/local/bin"),
            Input::Prompt("/usr/local/bin".to_owned())
        );
        assert_eq!(Input::parse("/"), Input::Prompt("/".to_owned()));
        assert_eq!(Input::parse("/ two"), Input::Prompt("/ two".to_owned()));
    }

    #[test]
    fn a_hyphenated_command_name_is_a_command() {
        assert_eq!(
            Input::parse("/no-save"),
            Input::Command {
                name: "no-save".to_owned(),
                arguments: String::new()
            }
        );
    }

    #[test]
    fn blank_input_is_empty() {
        assert_eq!(Input::parse(""), Input::Empty);
        assert_eq!(Input::parse("   "), Input::Empty);
        assert_eq!(Input::parse("\n"), Input::Empty);
    }

    #[test]
    fn a_prompt_is_trimmed_of_surrounding_whitespace() {
        assert_eq!(
            Input::parse("  hello  \n"),
            Input::Prompt("hello".to_owned())
        );
    }

    #[test]
    fn a_single_interrupt_does_not_complete_the_gesture() {
        let mut interrupts = Interrupts::default();
        assert!(!interrupts.record());
    }

    #[test]
    fn two_interrupts_inside_the_window_complete_the_gesture() {
        let mut interrupts = Interrupts::default();
        let start = std::time::Instant::now();
        assert!(!interrupts.record_at(start));
        assert!(interrupts.record_at(start + Duration::from_millis(100)));
    }

    #[test]
    fn a_second_interrupt_outside_the_window_starts_again() {
        let mut interrupts = Interrupts::default();
        let start = std::time::Instant::now();
        assert!(!interrupts.record_at(start));
        assert!(!interrupts.record_at(start + Duration::from_secs(5)));
    }

    #[test]
    fn a_third_interrupt_after_completing_does_not_fire_again() {
        // The gesture resets once it completes, so a third press behaves like a
        // first one rather than exiting again on its own.
        let mut interrupts = Interrupts::default();
        let start = std::time::Instant::now();
        assert!(!interrupts.record_at(start));
        assert!(interrupts.record_at(start + Duration::from_millis(50)));
        assert!(!interrupts.record_at(start + Duration::from_millis(100)));
    }

    #[test]
    fn a_cleared_gesture_does_not_carry_over() {
        let mut interrupts = Interrupts::default();
        let start = std::time::Instant::now();
        assert!(!interrupts.record_at(start));
        interrupts.clear();
        assert!(!interrupts.record_at(start + Duration::from_millis(10)));
    }

    #[test]
    fn the_shell_processes_every_line_in_order() {
        let mut source = ScriptedSource::new(["first", "second", "third"]);
        let mut shell = Shell::new(&mut source);
        let mut seen = Vec::new();

        let reason = shell
            .run(|input| {
                if let Input::Prompt(text) = input {
                    seen.push(text);
                }
                Ok(Action::Continue)
            })
            .expect("ran");

        assert_eq!(reason, ExitReason::EndOfInput);
        assert_eq!(seen, vec!["first", "second", "third"]);
        assert_eq!(shell.seen(), 3);
    }

    #[test]
    fn a_command_can_leave_the_shell() {
        let mut source = ScriptedSource::new(["/quit", "never reached"]);
        let mut shell = Shell::new(&mut source);

        let reason = shell
            .run(|input| match input {
                Input::Command { name, .. } if name == "quit" => Ok(Action::Exit),
                _ => Ok(Action::Continue),
            })
            .expect("ran");

        assert_eq!(reason, ExitReason::Requested);
        assert_eq!(shell.seen(), 1, "the shell read past the exit");
    }

    #[test]
    fn blank_lines_are_skipped_without_counting() {
        let mut source = ScriptedSource::new(["", "  ", "real"]);
        let mut shell = Shell::new(&mut source);
        shell.run(|_| Ok(Action::Continue)).expect("ran");
        assert_eq!(shell.seen(), 1);
    }

    #[test]
    fn an_empty_source_exits_immediately() {
        let mut source = ScriptedSource::new(Vec::<String>::new());
        let mut shell = Shell::new(&mut source);
        let reason = shell.run(|_| Ok(Action::Continue)).expect("ran");
        assert_eq!(reason, ExitReason::EndOfInput);
        assert_eq!(shell.seen(), 0);
    }

    #[test]
    fn a_completed_exchange_clears_a_pending_interrupt() {
        // An interrupt recorded by the handler must not carry into the next
        // line, so two keystrokes across two separate exchanges never combine
        // into an exit request.
        let mut source = ScriptedSource::new(["one", "two", "three"]);
        let mut shell = Shell::new(&mut source);
        let mut repeats = Vec::new();

        shell
            .run(|_| {
                repeats.push(false);
                Ok(Action::Continue)
            })
            .expect("ran");

        assert_eq!(repeats.len(), 3);
        assert_eq!(shell.seen(), 3);
        // The tracker holds no pending gesture once the run finished.
        assert!(!shell.interrupts().record(), "a gesture carried over");
    }

    #[test]
    fn an_interrupt_gesture_completes_only_when_repeated_quickly() {
        let mut interrupts = Interrupts::default();
        let start = std::time::Instant::now();
        assert!(!interrupts.record_at(start));
        // Outside the window, so this is a fresh gesture rather than an exit.
        assert!(!interrupts.record_at(start + Duration::from_secs(10)));
        // Inside the window of the second one, so this completes it.
        assert!(interrupts.record_at(start + Duration::from_secs(10) + Duration::from_millis(50)));
    }

    #[test]
    fn exit_codes_follow_the_documented_values() {
        assert_eq!(ExitReason::EndOfInput.exit_code(), 0);
        assert_eq!(ExitReason::Requested.exit_code(), 0);
        assert_eq!(ExitReason::Interrupted.exit_code(), 130);
    }

    #[test]
    fn the_prompt_stays_out_of_the_way() {
        // The prompt competes with the answer for the reader's attention, so it
        // is a marker and a space rather than a label.
        assert!(
            prompt().chars().count() <= 3,
            "the prompt grew to {} characters",
            prompt().chars().count()
        );
        assert!(prompt().ends_with(' '), "the prompt needs a trailing space");
    }

    #[test]
    fn the_interrupt_notice_names_the_next_press() {
        assert!(!interrupt_notice(true).contains("leave"));
        assert!(interrupt_notice(false).contains("leave"));
    }
}
