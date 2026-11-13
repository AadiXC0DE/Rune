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
        name: "budget",
        arguments: "[dollars|off]",
        summary: "show or set the session's spend limit",
    },
    Builtin {
        name: "network",
        arguments: "[on|off]",
        summary: "show or set network access for shell commands",
    },
    Builtin {
        name: "mcp",
        arguments: "[reconnect|login|disable <server>]",
        summary: "show MCP servers, or reconnect, log in to, or disable one",
    },
    Builtin {
        name: "plan",
        arguments: "[task|off]",
        summary: "plan read-only before acting, or leave plan mode",
    },
    Builtin {
        name: "fork",
        arguments: "",
        summary: "branch this conversation and continue in the branch",
    },
    Builtin {
        name: "permissions",
        arguments: "[mode|delete <n>]",
        summary: "show the mode and saved rules, switch mode, or delete a saved rule",
    },
    Builtin {
        name: "effort",
        arguments: "[level]",
        summary: "show the reasoning effort, or set it for the next requests",
    },
    Builtin {
        name: "status",
        arguments: "",
        summary: "show the model, provider, context, and session",
    },
    Builtin {
        name: "statusline",
        arguments: "[field]",
        summary: "show the status fields, or turn one on or off for this session",
    },
    Builtin {
        name: "cost",
        arguments: "",
        summary: "show what this session has spent",
    },
    Builtin {
        name: "tasks",
        arguments: "",
        summary: "list running commands, show their output or stop them",
    },
    Builtin {
        name: "compact",
        arguments: "[preview]",
        summary: "summarize older turns, or preview record counts without changes",
    },
    Builtin {
        name: "context",
        arguments: "",
        summary: "show what fills the context window",
    },
    Builtin {
        name: "theme",
        arguments: "[name]",
        summary: "choose a colour theme, or switch to one named here",
    },
    Builtin {
        name: "reload",
        arguments: "",
        summary: "re-read instructions, skills and the repository map now",
    },
    Builtin {
        name: "pin",
        arguments: "[constraint]",
        summary: "pin a user constraint across compaction, or list current pins",
    },
    Builtin {
        name: "undo",
        arguments: "[preview|confirm]",
        summary: "restore the files of the last turn that changed them, or preview it",
    },
    Builtin {
        name: "redo",
        arguments: "",
        summary: "put back what the last /undo restored",
    },
    Builtin {
        name: "tree",
        arguments: "",
        summary: "show the turns recorded in this session",
    },
    Builtin {
        name: "copy",
        arguments: "[code [n]|all]",
        summary: "put the last reply, a code block or the conversation on the clipboard",
    },
    Builtin {
        name: "diff",
        arguments: "[path]",
        summary: "show the changes this session made",
    },
    Builtin {
        name: "export",
        arguments: "[path] [--json]",
        summary: "write this conversation to a file",
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
        name: "resume",
        arguments: "[id]",
        summary: "switch to a saved session",
    },
    Builtin {
        name: "goal",
        arguments: "[--budget <tokens>] [objective]",
        summary: "set or show the active objective and optional token budget",
    },
    Builtin {
        name: "history",
        arguments: "[here|session-id|clear]",
        summary: "show recorded prompts",
    },
    Builtin {
        name: "help",
        arguments: "",
        summary: "list commands and active keyboard bindings",
    },
    Builtin {
        name: "init",
        arguments: "",
        summary: "ask the model to write AGENTS.md for this repository",
    },
    Builtin {
        name: "retry",
        arguments: "",
        summary: "send the last failed or interrupted message again",
    },
    Builtin {
        name: "connect",
        arguments: "[provider]",
        summary: "connect a provider or replace its key",
    },
    Builtin {
        name: "terminal-setup",
        arguments: "",
        summary: "configure this terminal so Shift-Enter inserts a newline",
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

/// Returns the indexes of `names` that match `word`, best first.
///
/// Names that start with the word come first, then names that contain its
/// characters in order, each group in table order, so the name being typed
/// stays above a longer one that merely contains it. Both comparisons ignore
/// case. An empty word matches every name.
#[must_use]
pub fn fuzzy_rank(word: &str, names: &[&str]) -> Vec<usize> {
    let word = word.to_lowercase();
    let mut ranked = Vec::new();
    let mut later = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let name = name.to_lowercase();
        if name.starts_with(&word) {
            ranked.push(index);
        } else if is_subsequence(&word, &name) {
            later.push(index);
        }
    }
    ranked.extend(later);
    ranked
}

/// Returns true when every character of `word` appears in `name` in order.
fn is_subsequence(word: &str, name: &str) -> bool {
    let mut rest = name.chars();
    word.chars().all(|wanted| rest.any(|found| found == wanted))
}

/// Returns the candidates matching a word being typed after a slash, in the
/// order of [`fuzzy_rank`]. An empty word matches everything, which is what
/// makes a bare `/` show the whole list.
#[must_use]
pub fn matching(word: &str) -> Vec<&'static Builtin> {
    let names: Vec<&str> = BUILTINS.iter().map(|entry| entry.name).collect();
    fuzzy_rank(word, &names)
        .into_iter()
        .filter_map(|index| BUILTINS.get(index))
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
        assert_eq!(got[..2], ["model", "models"]);
        let got: Vec<&str> = matching("mode").iter().map(|e| e.name).collect();
        assert_eq!(got, vec!["model", "models"]);
        // `/mod` is the case that must offer both, so a user typing it can pick.
        assert_eq!(matching("mod").len(), 2);
    }

    #[test]
    fn fuzzy_rank_puts_prefix_matches_first() {
        let names = ["settings", "status", "stats", "cost"];
        assert_eq!(fuzzy_rank("st", &names), [1, 2, 0, 3]);
        assert_eq!(fuzzy_rank("", &names), [0, 1, 2, 3]);
        assert_eq!(fuzzy_rank("STAT", &names), [1, 2]);
    }

    #[test]
    fn fuzzy_rank_matches_subsequences() {
        let names = ["status", "settings", "copy"];
        assert_eq!(fuzzy_rank("sts", &names), [0, 1]);
        assert!(fuzzy_rank("tss", &names).is_empty());
        assert!(fuzzy_rank("zzz", &names).is_empty());
        let got: Vec<&str> = matching("sts").iter().map(|e| e.name).collect();
        assert!(got.contains(&"status"), "{got:?}");
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
                .find(|line| {
                    line.strip_prefix('/')
                        .and_then(|rest| rest.strip_prefix(entry.name))
                        .is_some_and(|rest| rest.starts_with(' '))
                })
                .unwrap_or_else(|| panic!("{} is missing", entry.name));
            let at = line
                .find(entry.summary)
                .unwrap_or_else(|| panic!("no summary on {line:?}"));
            assert_eq!(at, expected, "ragged on {line:?}");
        }
    }
}
