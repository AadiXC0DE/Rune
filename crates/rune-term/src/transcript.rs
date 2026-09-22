//! Rendering a conversation for a terminal.
//!
//! The transcript decides what a turn looks like: which lines are the user's,
//! which are the model's, and which are the tools'. It is a pure projection, so
//! what a reader sees on a resumed session is decided by the log rather than by
//! whatever happened to be printed at the time.

use std::fmt::Write as _;

use crate::width::{str_width, truncate_to_width, wrap};

/// Who produced a line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Speaker {
    /// The user.
    User,
    /// The model.
    Assistant,
    /// A tool call or its result.
    Tool,
    /// A note from the harness rather than from either party.
    Notice,
}

/// One entry in a transcript.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// Who produced it.
    pub speaker: Speaker,
    /// The text, already free of terminal control sequences.
    pub text: String,
    /// Whether the text may be expanded rather than summarized.
    pub expandable: bool,
}

impl Entry {
    /// Builds an entry from the user.
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::User,
            text: text.into(),
            expandable: true,
        }
    }

    /// Builds an entry from the model.
    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Assistant,
            text: text.into(),
            expandable: true,
        }
    }

    /// Builds an entry for a tool call.
    #[must_use]
    pub fn tool(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Tool,
            text: text.into(),
            expandable: true,
        }
    }

    /// Builds an entry for a harness note.
    #[must_use]
    pub fn notice(text: impl Into<String>) -> Self {
        Self {
            speaker: Speaker::Notice,
            text: text.into(),
            expandable: false,
        }
    }
}

/// How much of a transcript to show.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Display {
    /// Width available, in terminal columns.
    pub width: usize,
    /// Lines a tool entry keeps before it is summarized.
    pub tool_lines: usize,
    /// Lines an assistant entry keeps before it is summarized.
    pub assistant_lines: usize,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            width: 80,
            tool_lines: 12,
            assistant_lines: 0,
        }
    }
}

/// Renders a transcript.
///
/// An assistant message is never summarized: it is the thing the reader asked
/// for. Tool output is, because it is usually long and rarely read in full.
#[must_use]
pub fn render(entries: &[Entry], display: Display) -> String {
    let mut out = String::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        render_entry(entry, display, &mut out);
    }
    out.trim_end().to_owned()
}

/// Renders one entry into `out`.
fn render_entry(entry: &Entry, display: Display, out: &mut String) {
    let width = display.width.max(MIN_WIDTH);
    let body = wrap(&sanitize(&entry.text), width.saturating_sub(2));

    match entry.speaker {
        Speaker::User => {
            let _ = writeln!(out, "> {}", body.first().map_or("", String::as_str));
            for line in body.iter().skip(1) {
                let _ = writeln!(out, "  {line}");
            }
        }
        Speaker::Assistant => {
            for line in &body {
                let _ = writeln!(out, "{line}");
            }
        }
        Speaker::Tool => {
            let keep = display.tool_lines.min(body.len());
            for line in body.iter().take(keep) {
                let _ = writeln!(out, "  {line}");
            }
            if keep < body.len() {
                let _ = writeln!(
                    out,
                    "  ... {} more line(s)",
                    body.len().saturating_sub(keep)
                );
            }
        }
        Speaker::Notice => {
            for line in &body {
                let _ = writeln!(out, "[{line}]");
            }
        }
    }
}

/// Narrowest body a transcript renders at.
pub const MIN_WIDTH: usize = 20;

/// Removes control sequences a terminal would act on.
///
/// Model output and tool output both reach a terminal unescaped otherwise, and a
/// carriage return or an erase sequence in that stream would rewrite what the
/// reader already saw. Newlines and tabs are kept; everything else in the
/// control range is dropped.
#[must_use]
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' | '\t' => out.push(ch),
            // An escape sequence runs until a byte that ends it. Dropping only
            // the escape byte would leave the parameters as text.
            '\u{1b}' => {
                consume_escape(&mut chars);
            }
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}

/// Consumes the rest of an escape sequence, including its introducer.
fn consume_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let Some(&next) = chars.peek() else {
        return;
    };
    match next {
        // A control sequence runs until a final byte in the range `@` to `~`.
        '[' => {
            chars.next();
            for ch in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&ch) {
                    return;
                }
            }
        }
        // An operating system command runs until a bell or a string terminator.
        ']' | 'P' | '^' | '_' => {
            chars.next();
            while let Some(ch) = chars.next() {
                if ch == '\u{7}' {
                    return;
                }
                if ch == '\u{1b}' && chars.peek() == Some(&'\\') {
                    chars.next();
                    return;
                }
            }
        }
        // An intermediate byte starts a sequence that runs to a final byte, as
        // in a character-set designation, so the introducer and both bytes go.
        '\u{20}'..='\u{2f}' => {
            chars.next();
            for ch in chars.by_ref() {
                if ('\u{30}'..='\u{7e}').contains(&ch) {
                    return;
                }
            }
        }
        // A two-character sequence: the designator is the whole sequence.
        _ => {
            chars.next();
        }
    }
}

/// Renders a single line of a prompt being typed.
///
/// Used where the caller echoes input itself rather than letting the terminal
/// do it.
#[must_use]
pub fn render_prompt(prompt: &str, input: &str, width: usize) -> String {
    let room = width.saturating_sub(str_width(prompt));
    let (shown, _) = truncate_to_width(input, room.max(1));
    format!("{prompt}{shown}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_assistant_message_renders_in_full() {
        let text = "one\n".repeat(40);
        let rendered = render(&[Entry::assistant(text)], Display::default());
        assert_eq!(
            rendered.lines().count(),
            40,
            "assistant text was summarized"
        );
    }

    #[test]
    fn tool_output_is_summarized_and_says_how_much_was_hidden() {
        let display = Display {
            tool_lines: 3,
            ..Display::default()
        };
        let text = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render(&[Entry::tool(text)], display);
        assert!(rendered.contains("line 3"), "{rendered}");
        assert!(!rendered.contains("line 4"), "{rendered}");
        assert!(rendered.contains("7 more line(s)"), "{rendered}");
    }

    #[test]
    fn tool_output_within_the_bound_is_not_summarized() {
        let display = Display {
            tool_lines: 5,
            ..Display::default()
        };
        let rendered = render(&[Entry::tool("one\ntwo")], display);
        assert!(rendered.contains("two"));
        assert!(!rendered.contains("more line"), "{rendered}");
    }

    #[test]
    fn a_user_line_is_marked_so_it_is_distinguishable() {
        let rendered = render(&[Entry::user("hello")], Display::default());
        assert_eq!(rendered, "> hello");
    }

    #[test]
    fn a_user_line_wraps_under_its_marker() {
        let display = Display {
            width: 24,
            ..Display::default()
        };
        let rendered = render(&[Entry::user("a".repeat(60))], display);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.len() > 1, "{rendered}");
        assert!(lines[0].starts_with("> "));
        for line in lines.iter().skip(1) {
            assert!(
                line.starts_with("  "),
                "continuation lost its indent: {rendered}"
            );
        }
    }

    #[test]
    fn a_notice_is_bracketed() {
        let rendered = render(&[Entry::notice("cancelled")], Display::default());
        assert_eq!(rendered, "[cancelled]");
    }

    #[test]
    fn entries_are_separated_by_one_blank_line() {
        let rendered = render(
            &[Entry::user("hi"), Entry::assistant("hello")],
            Display::default(),
        );
        assert_eq!(rendered, "> hi\n\nhello");
    }

    #[test]
    fn an_erase_sequence_is_removed_rather_than_printed() {
        // A terminal would act on these, rewriting what the reader already saw.
        let rendered = sanitize("keep\u{1b}[2Kdrop");
        assert_eq!(rendered, "keepdrop");
    }

    #[test]
    fn a_carriage_return_is_removed() {
        assert_eq!(sanitize("over\rwritten"), "overwritten");
    }

    #[test]
    fn a_hyperlink_sequence_is_removed_whole() {
        let rendered = sanitize("see \u{1b}]8;;https://example.com\u{7}here\u{1b}]8;;\u{7} now");
        assert_eq!(rendered, "see here now");
    }

    #[test]
    fn a_string_terminator_ends_a_hyperlink() {
        let rendered = sanitize("a\u{1b}]8;;http://x\u{1b}\\b");
        assert_eq!(rendered, "ab");
    }

    #[test]
    fn a_character_set_escape_is_removed_whole() {
        // An intermediate byte and its final byte both belong to the sequence,
        // so the following plain text is the first thing kept.
        assert_eq!(sanitize("a\u{1b}(Bplain"), "aplain");
    }

    #[test]
    fn a_two_character_escape_keeps_what_follows() {
        assert_eq!(sanitize("a\u{1b}=b"), "ab");
    }

    #[test]
    fn newlines_tabs_and_plain_text_survive() {
        assert_eq!(sanitize("a\nb\tc"), "a\nb\tc");
    }

    #[test]
    fn a_bell_alone_is_dropped() {
        assert_eq!(sanitize("a\u{7}b"), "ab");
    }

    #[test]
    fn an_unterminated_escape_does_not_loop_or_panic() {
        assert_eq!(sanitize("tail\u{1b}["), "tail");
        assert_eq!(sanitize("tail\u{1b}]"), "tail");
        assert_eq!(sanitize("\u{1b}"), "");
    }

    #[test]
    fn non_ascii_text_is_preserved() {
        assert_eq!(sanitize("héllo → wörld"), "héllo → wörld");
    }

    #[test]
    fn wide_characters_still_wrap_on_display_width() {
        let display = Display {
            width: 20,
            ..Display::default()
        };
        // Each glyph is two columns wide, so only a few fit per line.
        let rendered = render(&[Entry::assistant("書".repeat(30))], display);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.len() >= 2, "{rendered}");
        for line in lines {
            assert!(str_width(line) <= 20, "a line overflowed: {line:?}");
        }
    }

    #[test]
    fn a_narrow_terminal_still_renders() {
        let display = Display {
            width: 1,
            ..Display::default()
        };
        let rendered = render(&[Entry::assistant("hello world")], display);
        assert!(!rendered.is_empty());
        // The minimum width applies rather than a zero-width layout.
        for line in rendered.lines() {
            assert!(str_width(line) <= MIN_WIDTH, "{line:?}");
        }
    }

    #[test]
    fn an_empty_transcript_renders_nothing() {
        assert_eq!(render(&[], Display::default()), "");
    }

    #[test]
    fn an_empty_entry_renders_its_marker_only() {
        assert_eq!(render(&[Entry::user("")], Display::default()), ">");
    }

    #[test]
    fn a_control_sequence_in_model_output_cannot_rewrite_the_screen() {
        let entry = Entry::assistant("real\u{1b}[1AFAKE");
        let rendered = render(&[entry], Display::default());
        assert_eq!(rendered, "realFAKE");
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    }

    #[test]
    fn a_prompt_echo_truncates_rather_than_wrapping() {
        let rendered = render_prompt("> ", &"x".repeat(100), 20);
        assert_eq!(str_width(&rendered), 20);
        assert!(rendered.starts_with("> "));
    }
}
