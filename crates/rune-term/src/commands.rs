//! The commands a session understands, with the line that describes each.
//!
//! One table, so the help text, the dropdown shown while a command is typed,
//! and the dispatcher cannot disagree about what exists. A command that is
//! handled but unlisted is one nobody finds, and a listed command that is not
//! handled is worse.

/// One command the session handles itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Builtin {
    /// Name without the leading slash.
    pub name: &'static str,
    /// Argument hint, shown after the name. Empty when it takes none.
    pub arguments: &'static str,
    /// One line describing what it does.
    pub summary: &'static str,
}

/// Every command the session handles, in the order they are listed.
///
/// The dispatcher matches on these names, so an entry here is a command that
/// works. `/models` is listed because it is accepted as a spelling of `/model`,
/// and a spelling nobody can see is one nobody uses.
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "model",
        arguments: "[id]",
        summary: "choose a model, or switch to one named here",
    },
    Builtin {
        name: "models",
        arguments: "",
        summary: "same as /model",
    },
    Builtin {
        name: "status",
        arguments: "",
        summary: "show the model, provider, context, and session",
    },
    Builtin {
        name: "cost",
        arguments: "",
        summary: "show what this session has spent",
    },
    Builtin {
        name: "compact",
        arguments: "",
        summary: "summarize older turns to free the context window",
    },
    Builtin {
        name: "undo",
        arguments: "",
        summary: "put back the files the last turn changed",
    },
    Builtin {
        name: "tree",
        arguments: "",
        summary: "show the turns recorded in this session",
    },
    Builtin {
        name: "copy",
        arguments: "",
        summary: "put the last reply on the clipboard",
    },
    Builtin {
        name: "new",
        arguments: "",
        summary: "start a fresh conversation",
    },
    Builtin {
        name: "rename",
        arguments: "<title>",
        summary: "name this session",
    },
    Builtin {
        name: "history",
        arguments: "[here|session-id|clear]",
        summary: "show recorded prompts",
    },
    Builtin {
        name: "help",
        arguments: "",
        summary: "list the commands, and any this project defines",
    },
    Builtin {
        name: "quit",
        arguments: "",
        summary: "leave the session",
    },
];

/// Returns the command with this name, when there is one.
#[must_use]
pub fn find(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|entry| entry.name == name)
}

/// Renders the table as the help text.
///
/// The name column is padded to the longest name so the descriptions line up,
/// and the two commands that take no argument are still shown with their
/// argument column empty rather than misaligned.
#[must_use]
pub fn render_help() -> String {
    use std::fmt::Write as _;

    let width = BUILTINS
        .iter()
        .map(|entry| entry.name.len().saturating_add(entry.arguments.len()))
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for entry in BUILTINS {
        let left = if entry.arguments.is_empty() {
            format!("/{}", entry.name)
        } else {
            format!("/{} {}", entry.name, entry.arguments)
        };
        let _ = writeln!(
            out,
            "{left:<width$}  {}",
            entry.summary,
            width = width.saturating_add(3)
        );
    }
    out.trim_end().to_owned()
}

/// Returns the candidates matching a word being typed after a slash.
///
/// A candidate matches when its name starts with what was typed. An empty word
/// matches everything, which is what makes a bare `/` show the whole list.
/// Matching is exact on the prefix rather than fuzzy, because a slash command
/// is short and a fuzzy match would order a longer name above the one being
/// typed.
#[must_use]
pub fn matching(word: &str) -> Vec<&'static Builtin> {
    BUILTINS
        .iter()
        .filter(|entry| entry.name.starts_with(word))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_command_has_a_description() {
        for entry in BUILTINS {
            assert!(
                !entry.summary.trim().is_empty(),
                "`/{}` has no description",
                entry.name
            );
        }
    }

    #[test]
    fn the_names_are_unique_and_carry_no_slash() {
        // A duplicated name would be listed twice and one copy would never
        // match; a leading slash would never match what is typed after it.
        let mut seen = std::collections::BTreeSet::new();
        for entry in BUILTINS {
            assert!(
                seen.insert(entry.name),
                "`/{}` is listed more than once",
                entry.name
            );
            assert!(
                !entry.name.starts_with('/'),
                "`{}` should not carry the slash",
                entry.name
            );
            assert!(
                entry
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "`{}` is not a command name",
                entry.name
            );
        }
    }

    #[test]
    fn a_bare_slash_offers_every_command() {
        assert_eq!(matching("").len(), BUILTINS.len());
    }

    #[test]
    fn a_prefix_narrows_the_list_in_table_order() {
        let got: Vec<&str> = matching("mo").iter().map(|e| e.name).collect();
        assert_eq!(got, vec!["model", "models"]);
        let got: Vec<&str> = matching("mode").iter().map(|e| e.name).collect();
        assert_eq!(got, vec!["model", "models"]);
        // `/mod` is the case that must offer both, so a user typing it can pick.
        assert_eq!(matching("mod").len(), 2);
    }

    #[test]
    fn an_exact_name_is_still_offered() {
        // Typing the whole name must not make the list vanish, or the row a
        // user is looking at disappears as they finish typing it.
        let got: Vec<&str> = matching("help").iter().map(|e| e.name).collect();
        assert_eq!(got, vec!["help"]);
    }

    #[test]
    fn something_that_is_not_a_command_offers_nothing() {
        assert!(matching("zzz").is_empty());
        assert!(matching("usage").is_empty());
    }

    #[test]
    fn a_name_resolves_to_its_entry() {
        assert_eq!(find("model").map(|e| e.name), Some("model"));
        assert_eq!(find("models").map(|e| e.name), Some("models"));
        assert_eq!(find("nope"), None);
    }

    #[test]
    fn the_help_text_lists_every_command_with_its_description() {
        let help = render_help();
        for entry in BUILTINS {
            assert!(help.contains(entry.name), "{} is missing", entry.name);
            assert!(help.contains(entry.summary), "{} has no line", entry.name);
        }
    }

    #[test]
    fn the_help_text_lines_up_its_columns() {
        // Every description starts at the same column, so the list reads as a
        // table rather than a ragged block. The column is found from the first
        // line and checked against every other.
        let help = render_help();
        let lines: Vec<&str> = help.lines().collect();
        assert!(lines.len() > 1, "{help}");
        // The name column is at most this wide, so the description starts after
        // the padded name field.
        let expected = BUILTINS
            .iter()
            .map(|e| {
                if e.arguments.is_empty() {
                    e.name.len().saturating_add(1)
                } else {
                    e.name
                        .len()
                        .saturating_add(1)
                        .saturating_add(e.arguments.len())
                        .saturating_add(1)
                }
            })
            .max()
            .unwrap_or(0)
            .saturating_add(3);
        for entry in BUILTINS {
            let line = lines
                .iter()
                .find(|line| line.contains(entry.name))
                .unwrap_or_else(|| panic!("{} is missing", entry.name));
            let at = line
                .find(entry.summary)
                .unwrap_or_else(|| panic!("no summary on {line:?}"));
            assert_eq!(at, expected, "ragged on {line:?}");
        }
    }
}
