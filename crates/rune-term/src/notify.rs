//! Desktop notifications, sent through whichever channel the terminal has.

use crate::probe::TerminalCaps;

/// Longest title or body sent, in characters.
const MAX_TEXT: usize = 80;
/// Ends an OSC or DCS string.
const ST: &str = "\u{1b}\\";
/// The terminal bell.
const BEL: &str = "\u{7}";
/// Starts a tmux passthrough string.
const TMUX_PASSTHROUGH: &str = "\u{1b}Ptmux;";

/// The `notify` setting.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Notify {
    /// The terminal's own notification channel, or the bell.
    #[default]
    Auto,
    /// Always the bell.
    Bell,
    /// Nothing.
    Off,
}

impl Notify {
    /// Reads the setting's value; anything unknown is `auto`.
    #[must_use]
    pub fn from_setting(value: &str) -> Self {
        match value {
            "bell" => Self::Bell,
            "off" => Self::Off,
            _ => Self::Auto,
        }
    }
}

/// How a notification reaches the user.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    /// `OSC 99`, with a title and a body, shown only while unfocused.
    Osc99,
    /// `OSC 777 ; notify`.
    Osc777,
    /// `OSC 9`.
    Osc9,
    /// The bell.
    Bell,
    /// Nothing is sent.
    None,
}

/// Picks the channel for `setting` in the terminal `caps` describes.
#[must_use]
pub fn channel(setting: Notify, caps: &TerminalCaps) -> Channel {
    match setting {
        Notify::Off => return Channel::None,
        Notify::Bell => return Channel::Bell,
        Notify::Auto => {}
    }
    let env = &caps.env;
    let term = env.term.as_deref().unwrap_or_default();
    let program = env.term_program.as_deref().unwrap_or_default();
    if env.kitty_window_id.is_some() || term == "xterm-kitty" {
        Channel::Osc99
    } else if program == "ghostty" || term.starts_with("foot") {
        Channel::Osc777
    } else if program == "iTerm.app"
        || program == "WezTerm"
        || env.lc_terminal.as_deref() == Some("iTerm2")
    {
        Channel::Osc9
    } else {
        Channel::Bell
    }
}

/// Removes control characters and cuts the text to its longest length.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect()
}

/// Wraps one sequence so tmux passes it to the outer terminal.
fn passthrough(sequence: &str) -> String {
    format!(
        "{TMUX_PASSTHROUGH}{}{ST}",
        sequence.replace('\u{1b}', "\u{1b}\u{1b}")
    )
}

/// Returns the bytes that raise a notification with `title` and `body`
/// through `channel`, wrapped for tmux when `tmux` is set.
#[must_use]
pub fn sequence(channel: Channel, title: &str, body: &str, tmux: bool) -> Vec<u8> {
    let title = clean(title);
    let body = clean(body);
    let sequences = match channel {
        Channel::None => return Vec::new(),
        Channel::Bell => return BEL.as_bytes().to_vec(),
        Channel::Osc99 => vec![
            format!("\u{1b}]99;i=1:d=0:o=unfocused;{title}{ST}"),
            format!("\u{1b}]99;i=1:p=body;{body}{ST}"),
        ],
        Channel::Osc777 => vec![format!(
            "\u{1b}]777;notify;{};{}{ST}",
            title.replace(';', ","),
            body.replace(';', ",")
        )],
        Channel::Osc9 => vec![format!("\u{1b}]9;{title}: {body}{ST}")],
    };
    sequences
        .iter()
        .map(|sequence| {
            if tmux {
                passthrough(sequence)
            } else {
                sequence.clone()
            }
        })
        .collect::<String>()
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::TerminalEnv;

    fn caps(env: TerminalEnv) -> TerminalCaps {
        TerminalCaps::from_env(env)
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    #[test]
    fn channel_follows_the_terminal() {
        let some = |value: &str| Some(value.to_owned());
        let cases = [
            (
                TerminalEnv {
                    kitty_window_id: some("1"),
                    term_program: some("iTerm.app"),
                    ..TerminalEnv::default()
                },
                Channel::Osc99,
            ),
            (
                TerminalEnv {
                    term: some("xterm-kitty"),
                    ..TerminalEnv::default()
                },
                Channel::Osc99,
            ),
            (
                TerminalEnv {
                    term_program: some("ghostty"),
                    ..TerminalEnv::default()
                },
                Channel::Osc777,
            ),
            (
                TerminalEnv {
                    term: some("foot-extra"),
                    ..TerminalEnv::default()
                },
                Channel::Osc777,
            ),
            (
                TerminalEnv {
                    term_program: some("iTerm.app"),
                    ..TerminalEnv::default()
                },
                Channel::Osc9,
            ),
            (
                TerminalEnv {
                    term_program: some("WezTerm"),
                    ..TerminalEnv::default()
                },
                Channel::Osc9,
            ),
            (
                TerminalEnv {
                    term: some("xterm-256color"),
                    lc_terminal: some("iTerm2"),
                    ..TerminalEnv::default()
                },
                Channel::Osc9,
            ),
            (
                TerminalEnv {
                    term: some("xterm-256color"),
                    term_program: some("Apple_Terminal"),
                    ..TerminalEnv::default()
                },
                Channel::Bell,
            ),
            (TerminalEnv::default(), Channel::Bell),
        ];
        for (env, expected) in cases {
            assert_eq!(
                channel(Notify::Auto, &caps(env.clone())),
                expected,
                "{env:?}"
            );
            assert_eq!(channel(Notify::Bell, &caps(env.clone())), Channel::Bell);
            assert_eq!(channel(Notify::Off, &caps(env)), Channel::None);
        }
        assert_eq!(Notify::from_setting("auto"), Notify::Auto);
        assert_eq!(Notify::from_setting("bell"), Notify::Bell);
        assert_eq!(Notify::from_setting("off"), Notify::Off);
    }

    #[test]
    fn osc777_replaces_semicolons() {
        let bytes = sequence(Channel::Osc777, "ru;ne", "finished; in a;b", false);
        assert_eq!(
            text(&bytes),
            "\u{1b}]777;notify;ru,ne;finished, in a,b\u{1b}\\"
        );
    }

    #[test]
    fn osc9_joins_title_and_body() {
        let bytes = sequence(Channel::Osc9, "rune", "finished in work", false);
        assert_eq!(text(&bytes), "\u{1b}]9;rune: finished in work\u{1b}\\");
    }

    #[test]
    fn tmux_wraps_osc_but_not_bell() {
        let bytes = sequence(Channel::Osc9, "rune", "finished in work", true);
        assert_eq!(
            text(&bytes),
            "\u{1b}Ptmux;\u{1b}\u{1b}]9;rune: finished in work\u{1b}\u{1b}\\\u{1b}\\"
        );
        assert_eq!(sequence(Channel::Bell, "rune", "x", true), b"\x07");
        assert_eq!(sequence(Channel::Bell, "rune", "x", false), b"\x07");
        assert!(sequence(Channel::None, "rune", "x", true).is_empty());
        let both = text(&sequence(Channel::Osc99, "rune", "x", true));
        assert_eq!(both.matches("\u{1b}Ptmux;").count(), 2, "{both:?}");
    }

    #[test]
    fn osc99_marks_unfocused() {
        let bytes = sequence(Channel::Osc99, "rune", "waiting for your answer", false);
        assert_eq!(
            text(&bytes),
            "\u{1b}]99;i=1:d=0:o=unfocused;rune\u{1b}\\\u{1b}]99;i=1:p=body;waiting for your answer\u{1b}\\"
        );
    }

    #[test]
    fn control_characters_are_removed_and_text_is_cut() {
        let long = "x".repeat(200);
        let bytes = sequence(
            Channel::Osc9,
            "ru\u{7}ne",
            &format!("a\u{1b}b{long}"),
            false,
        );
        let sent = text(&bytes);
        let inner = sent
            .strip_prefix("\u{1b}]9;")
            .and_then(|rest| rest.strip_suffix("\u{1b}\\"))
            .expect("one sequence");
        assert!(!inner.chars().any(char::is_control), "{inner:?}");
        let (title, body) = inner.split_once(": ").expect("title and body");
        assert_eq!(title, "rune");
        assert_eq!(body.chars().count(), 80);
        assert!(body.starts_with("abx"));
    }
}
