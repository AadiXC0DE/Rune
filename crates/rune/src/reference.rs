//! Generating the command reference from the specification table.
//!
//! The table is what the parser and the help text both read, so generating the
//! reference from it is what keeps the three from disagreeing. A command that is
//! documented because it is in the table cannot be a command that does not exist.

use std::fmt::Write as _;

use rune_core::budget::LimitName;

use crate::spec::{self, CommandSpec, FlagSpec};

/// Renders the whole reference.
#[must_use]
pub fn render() -> String {
    let mut out = String::new();
    out.push_str("# Command reference\n\n");
    out.push_str(
        "Generated from the command table the parser reads, so every entry here is a \
         command the binary accepts.\n",
    );

    out.push_str("\n## Global flags\n\n");
    out.push_str("Accepted anywhere on the command line.\n\n");
    out.push_str("| Flag | Value | Description |\n|---|---|---|\n");
    for flag in spec::GLOBAL_FLAGS {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} |",
            flag.name,
            flag.value
                .map_or("-".to_owned(), |value| format!("`{value}`")),
            flag.description
        );
    }

    for (group, commands) in spec::GROUPS {
        let _ = writeln!(out, "\n## {group}\n");
        for command in *commands {
            render_command(command, &mut out);
        }
    }

    out.push_str("\n## Limits\n\n");
    out.push_str(
        "Every limit takes a count, or `off` to remove it. A hard ceiling still \
         applies to a limit set to `off`.\n\n",
    );
    out.push_str("| Name | Default | Unit | Range | Description |\n|---|---|---|---|---|\n");
    for name in LimitName::all() {
        let range = name.range();
        let upper = range
            .max
            .map_or_else(|| "unbounded".to_owned(), |max| max.to_string());
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} to {upper} | {} |",
            name.as_str(),
            name.default_value(),
            name.unit().suffix(),
            range.min,
            name.description()
        );
    }

    out
}

/// Renders one command.
fn render_command(command: &CommandSpec, out: &mut String) {
    let _ = writeln!(out, "### `{}`\n", command.usage);
    let _ = writeln!(out, "{}\n", command.summary);
    if !command.aliases.is_empty() {
        let _ = writeln!(
            out,
            "Aliases: {}\n",
            command
                .aliases
                .iter()
                .map(|alias| format!("`{alias}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if command.flags.is_empty() {
        return;
    }
    out.push_str("| Flag | Description |\n|---|---|\n");
    for flag in command.flags {
        render_flag(flag, out);
    }
}

/// Renders one flag row.
fn render_flag(flag: &FlagSpec, out: &mut String) {
    let shown = match flag.value {
        Some(value) => format!("`{} <{value}>`", flag.name),
        None => format!("`{}`", flag.name),
    };
    let _ = writeln!(out, "| {shown} | {} |", flag.description);
}

/// Writes the reference to a file, for the documentation build.
///
/// Present so the committed reference and the generator cannot drift: the test
/// below regenerates it and compares, so a stale file fails the build.
pub fn write_to(path: &camino::Utf8Path) -> rune_core::error::Result<()> {
    rune_core::paths::write_private(path, &render())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_command_in_the_table_appears_in_the_reference() {
        // The generator reads the table the parser reads, so a command that is
        // accepted is documented. This fails if either side stops agreeing.
        let text = render();
        for command in spec::all_commands() {
            assert!(
                text.contains(command.usage),
                "`{}` is missing from the reference",
                command.usage
            );
        }
    }

    #[test]
    fn every_global_flag_appears() {
        let text = render();
        for flag in spec::GLOBAL_FLAGS {
            assert!(text.contains(flag.name), "`{}` is missing", flag.name);
        }
    }

    #[test]
    fn every_limit_appears_with_its_default_and_range() {
        let text = render();
        for name in LimitName::all() {
            assert!(
                text.contains(name.as_str()),
                "limit `{}` is missing",
                name.as_str()
            );
            assert!(
                text.contains(name.description()),
                "limit `{}` is listed without its description",
                name.as_str()
            );
        }
    }

    #[test]
    fn the_reference_names_the_opt_out() {
        // The spelling is what a user types, so the reference must state it.
        assert!(render().contains("`off`"));
    }

    #[test]
    fn the_committed_reference_matches_the_generator() {
        // A hand-edited or stale reference would describe commands that no
        // longer exist, which is the failure this whole module exists to stop.
        let path = camino::Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(camino::Utf8Path::parent)
            .expect("workspace root")
            .join("COMMANDS.md");
        let Ok(committed) = std::fs::read_to_string(&path) else {
            panic!("the command reference is missing at {path}");
        };
        assert_eq!(
            committed,
            render(),
            "the committed reference differs from the generator"
        );
    }

    #[test]
    fn writing_the_reference_creates_the_file() {
        // Exercised so the writer is not dead code, and so a caller has one
        // obvious command that regenerates the committed document.
        let dir = tempfile::tempdir().expect("temp");
        let path = camino::Utf8Path::from_path(dir.path())
            .expect("utf8")
            .join("COMMANDS.md");
        write_to(&path).expect("written");
        let written = std::fs::read_to_string(&path).expect("read");
        assert_eq!(written, render());
    }

    #[test]
    fn the_reference_is_not_empty() {
        let text = render();
        assert!(
            text.lines().count() > 50,
            "the reference is suspiciously short"
        );
    }
}
