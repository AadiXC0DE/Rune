//! Help rendering.
//!
//! Help is generated from the specification table so it cannot drift from what
//! the parser accepts.

use std::fmt::Write as _;

use crate::spec::{self, FlagSpec};

/// Renders the top-level help.
#[must_use]
pub fn render_top_level() -> String {
    let mut out = String::new();
    out.push_str("rune: a native coding agent harness\n\n");
    out.push_str("Usage:\n  rune [global flags] [command] [flags] [arguments]\n\n");
    out.push_str("Run `rune` with no command to start an interactive session.\n\n");

    for (group, specs) in spec::GROUPS {
        out.push_str(group);
        out.push_str(":\n");
        let width = specs.iter().map(|s| s.name.len()).max().unwrap_or(0);
        for item in *specs {
            let _ = writeln!(out, "  {:<width$}  {}", item.name, item.summary);
        }
        out.push('\n');
    }

    out.push_str("Global flags:\n");
    for flag in spec::GLOBAL_FLAGS {
        out.push_str(&render_flag(flag, 2));
    }
    out.push('\n');
    out.push_str("Run `rune help <command>` for the flags of one command.\n");
    out
}

/// Renders help for one command.
#[must_use]
pub fn render_command(name: &str) -> Option<String> {
    let item = spec::find(name)?;
    let mut out = String::new();
    let _ = writeln!(out, "rune {}: {}\n", item.name, item.summary);
    out.push_str("Usage:\n  ");
    out.push_str(item.usage);
    out.push('\n');

    if !item.aliases.is_empty() {
        out.push_str("\nAliases:\n  ");
        out.push_str(&item.aliases.join(", "));
        out.push('\n');
    }

    if !item.flags.is_empty() {
        out.push_str("\nFlags:\n");
        for flag in item.flags {
            out.push_str(&render_flag(flag, 2));
        }
    }

    if item.supports_json {
        out.push_str("\nThis command accepts --json for machine-readable output.\n");
    }

    Some(out)
}

/// Renders one flag with its value placeholder and description.
fn render_flag(flag: &FlagSpec, indent: usize) -> String {
    let padding = " ".repeat(indent);
    let label = match flag.value {
        Some(value) => format!("{} <{}>", flag.name, value),
        None => flag.name.to_owned(),
    };
    format!("{padding}{label}\n{padding}    {}\n", flag.description)
}

/// Renders the error shown for an unknown command.
#[must_use]
pub fn unknown_command_hint() -> String {
    "run `rune help` to list the commands".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_help_lists_every_command() {
        let help = render_top_level();
        for item in spec::all_commands() {
            assert!(
                help.contains(item.name),
                "help omits command `{}`",
                item.name
            );
        }
    }

    #[test]
    fn top_level_help_lists_every_global_flag() {
        let help = render_top_level();
        for flag in spec::GLOBAL_FLAGS {
            assert!(help.contains(flag.name), "help omits flag `{}`", flag.name);
        }
    }

    #[test]
    fn top_level_help_names_every_group() {
        let help = render_top_level();
        for (group, _) in spec::GROUPS {
            assert!(help.contains(group), "help omits group `{group}`");
        }
    }

    #[test]
    fn every_command_help_is_rendered() {
        for item in spec::all_commands() {
            let help =
                render_command(item.name).unwrap_or_else(|| panic!("no help for `{}`", item.name));
            assert!(help.contains(item.usage), "{} usage missing", item.name);
            for flag in item.flags {
                assert!(
                    help.contains(flag.name),
                    "{} help omits flag `{}`",
                    item.name,
                    flag.name
                );
            }
        }
    }

    #[test]
    fn aliases_resolve_to_command_help() {
        let help = render_command("--help").expect("present");
        assert!(help.starts_with("rune help"));
    }

    #[test]
    fn unknown_command_has_no_help() {
        assert!(render_command("frobnicate").is_none());
    }

    #[test]
    fn json_support_is_mentioned_in_help() {
        let help = render_command("sessions").expect("present");
        assert!(help.contains("--json"));
    }

    #[test]
    fn help_text_has_no_double_hyphen_dashes() {
        // Guards the house style: a double hyphen is a flag prefix or an
        // em-dash substitute, never prose.
        let help = render_top_level();
        for line in help.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('-') && !trimmed.contains("--") {
                continue;
            }
            // Only flag lines may contain a double hyphen.
            assert!(
                !trimmed.starts_with("A ") && !trimmed.ends_with(" -"),
                "suspicious dash usage: {trimmed}"
            );
        }
    }
}
