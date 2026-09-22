//! Command specification table.
//!
//! One table drives parsing, help rendering, and completion, so the three
//! cannot drift apart. Every command states whether it needs the network stack
//! or the renderer, which is what lets the binary skip initializing them.

use crate::cli::Command;

/// What a command requires from the runtime.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Requirements {
    /// Command may read configuration and state.
    pub config: bool,
    /// Command may reach the network.
    pub network: bool,
    /// Command may render an interactive session.
    pub terminal: bool,
}

impl Requirements {
    /// Nothing beyond argument parsing.
    pub const NONE: Self = Self {
        config: false,
        network: false,
        terminal: false,
    };

    /// Configuration only.
    pub const CONFIG: Self = Self {
        config: true,
        network: false,
        terminal: false,
    };

    /// Configuration and network.
    pub const NETWORK: Self = Self {
        config: true,
        network: true,
        terminal: false,
    };

    /// Everything.
    pub const FULL: Self = Self {
        config: true,
        network: true,
        terminal: true,
    };
}

/// One command in the table.
#[derive(Clone, Copy, Debug)]
pub struct CommandSpec {
    /// Canonical name.
    pub name: &'static str,
    /// Alternative names.
    pub aliases: &'static [&'static str],
    /// One-line summary shown in the command list.
    pub summary: &'static str,
    /// Usage line shown by `rune <command> --help`.
    pub usage: &'static str,
    /// Flags accepted by this command, for help rendering.
    pub flags: &'static [FlagSpec],
    /// What the command needs from the runtime.
    pub requirements: Requirements,
    /// Whether the command produces JSON with `--json`.
    pub supports_json: bool,
}

/// One flag in a command's help.
#[derive(Clone, Copy, Debug)]
pub struct FlagSpec {
    /// Flag spelling including leading dashes.
    pub name: &'static str,
    /// Value placeholder, absent for a boolean flag.
    pub value: Option<&'static str>,
    /// Description.
    pub description: &'static str,
}

/// Boolean flag helper.
const fn flag(name: &'static str, description: &'static str) -> FlagSpec {
    FlagSpec {
        name,
        value: None,
        description,
    }
}

/// Value flag helper.
const fn option(name: &'static str, value: &'static str, description: &'static str) -> FlagSpec {
    FlagSpec {
        name,
        value: Some(value),
        description,
    }
}

/// Global flags accepted before any command.
pub const GLOBAL_FLAGS: &[FlagSpec] = &[
    option("--model", "id", "Override the model for this process."),
    option(
        "--provider",
        "name",
        "Override the provider for this process.",
    ),
    option(
        "--effort",
        "level",
        "Override the reasoning effort: auto, none, minimal, low, medium, high, xhigh, max.",
    ),
    flag(
        "--fast",
        "Request fast mode where the provider supports it.",
    ),
    flag("--no-fast", "Disable fast mode for this process."),
    option(
        "--permission-mode",
        "mode",
        "Override the permission mode: ask, auto, full-access.",
    ),
    option(
        "--limit",
        "name=value",
        "Override one limit. Repeatable. Use off to disable a limit.",
    ),
    option(
        "--add-dir",
        "path",
        "Add a workspace directory for this process. Repeatable.",
    ),
    flag(
        "--no-additional-dirs",
        "Ignore saved additional directories for this process.",
    ),
    flag("--offline", "Refuse every outbound network request."),
    flag("--json", "Emit machine-readable output where supported."),
    option("--theme", "name", "Override the theme for this process."),
    option(
        "--provider-order",
        "a,b",
        "Prefer these upstream providers in order.",
    ),
    flag(
        "--provider-strict",
        "Restrict requests to the listed providers.",
    ),
    flag("-h, --help", "Print help."),
    flag("-v, --version", "Print the version."),
];

/// Commands that start an interactive session rather than performing one action.
pub const RUN: &[CommandSpec] = &[
    CommandSpec {
        name: "ask",
        aliases: &[],
        summary: "Run one request without an interactive session",
        usage: "rune ask [flags] <prompt>",
        flags: &[
            flag("--json", "Print one JSON object instead of Markdown."),
            flag("--no-save", "Do not create a session."),
            option("--image", "path", "Attach an image. Repeatable."),
            option("--max-steps", "n", "Limit model steps for this run."),
            option("--timeout", "secs", "Fail the run after this long."),
            flag(
                "--prompt-permissions",
                "Prompt for approval. Requires a terminal.",
            ),
        ],
        requirements: Requirements::FULL,
        supports_json: true,
    },
    CommandSpec {
        name: "acp",
        aliases: &[],
        summary: "Serve the Agent Client Protocol over standard input and output",
        usage: "rune acp [--log-file <path>]",
        flags: &[option(
            "--log-file",
            "path",
            "Write diagnostics to this file.",
        )],
        requirements: Requirements::NETWORK,
        supports_json: false,
    },
    CommandSpec {
        name: "review",
        aliases: &[],
        summary: "Review the pending changes in the workspace",
        usage: "rune review [context]",
        flags: &[],
        requirements: Requirements::FULL,
        supports_json: false,
    },
    CommandSpec {
        name: "connect",
        aliases: &[],
        summary: "Connect a model provider",
        usage: "rune connect [<name>]",
        flags: &[],
        requirements: Requirements::CONFIG,
        supports_json: false,
    },
];

/// Commands operating on sessions and local records.
pub const SESSIONS: &[CommandSpec] = &[
    CommandSpec {
        name: "sessions",
        aliases: &[],
        summary: "List sessions",
        usage: "rune sessions [--all] [--limit <n>] [--cursor <c>]",
        flags: &[
            flag("--all", "Include sessions from every workspace."),
            option("--limit", "n", "Page size, 1 to 100."),
            option("--cursor", "c", "Continue from a previous page."),
            flag("--json", "Emit JSON."),
        ],
        requirements: Requirements::CONFIG,
        supports_json: true,
    },
    CommandSpec {
        name: "session",
        aliases: &[],
        summary: "Inspect, migrate, or recover one session",
        usage: "rune session <last|id> [--json] | rune session migrate <id> | rune session recover <id>",
        flags: &[
            option(
                "--id",
                "id",
                "Read the argument as an exact session identifier.",
            ),
            flag("--allow-large", "Permit migrating an oversized session."),
            flag("--json", "Emit JSON."),
        ],
        requirements: Requirements::CONFIG,
        supports_json: true,
    },
    CommandSpec {
        name: "tree",
        aliases: &[],
        summary: "Show the branch structure of a session",
        usage: "rune tree [last|id] [--json]",
        flags: &[
            option("--id", "id", "Read the argument as an exact identifier."),
            flag("--json", "Emit JSON."),
        ],
        requirements: Requirements::CONFIG,
        supports_json: true,
    },
    CommandSpec {
        name: "usage",
        aliases: &[],
        summary: "Report token usage recorded on this machine",
        usage: "rune usage [--period <24h|7d|30d>] [--json]",
        flags: &[
            option("--period", "span", "One of 24h, 7d, or 30d."),
            flag("--json", "Emit JSON."),
        ],
        requirements: Requirements::CONFIG,
        supports_json: true,
    },
];

/// Account and configuration commands.
pub const ACCOUNT: &[CommandSpec] = &[
    CommandSpec {
        name: "auth",
        aliases: &[],
        summary: "Show or manage stored credentials",
        usage: "rune auth [status|logout] [--json]",
        flags: &[flag("--json", "Emit JSON.")],
        requirements: Requirements::CONFIG,
        supports_json: true,
    },
    CommandSpec {
        name: "models",
        aliases: &[],
        summary: "List the models of the connected provider",
        usage: "rune models [--json]",
        flags: &[flag("--json", "Emit JSON.")],
        requirements: Requirements::NETWORK,
        supports_json: true,
    },
    CommandSpec {
        name: "permissions",
        aliases: &[],
        summary: "Show the permission mode and rules",
        usage: "rune permissions [--explain <target>] [--json]",
        flags: &[
            option(
                "--explain",
                "target",
                "Explain the decision for one target.",
            ),
            flag("--json", "Emit JSON."),
        ],
        requirements: Requirements::CONFIG,
        supports_json: true,
    },
    CommandSpec {
        name: "projects",
        aliases: &[],
        summary: "Inspect or change workspace trust",
        usage: "rune projects [status|approve|reject|reset]",
        flags: &[flag("--json", "Emit JSON.")],
        requirements: Requirements::NONE,
        supports_json: true,
    },
    CommandSpec {
        name: "config",
        aliases: &[],
        summary: "Show the resolved configuration and where each value came from",
        usage: "rune config [--explain] [--json]",
        flags: &[
            flag("--explain", "Show the source layer of every key."),
            flag("--json", "Emit JSON."),
        ],
        requirements: Requirements::CONFIG,
        supports_json: true,
    },
    CommandSpec {
        name: "limits",
        aliases: &[],
        summary: "List every limit with its effective value and source",
        usage: "rune limits [--json]",
        flags: &[flag("--json", "Emit JSON.")],
        requirements: Requirements::CONFIG,
        supports_json: true,
    },
    CommandSpec {
        name: "workspace",
        aliases: &[],
        summary: "Manage additional workspace directories",
        usage: "rune workspace <list|add <path>|remove <path>|clear>",
        flags: &[flag("--json", "Emit JSON.")],
        requirements: Requirements::CONFIG,
        supports_json: true,
    },
    CommandSpec {
        name: "prompt",
        aliases: &[],
        summary: "Show the assembled system prompt",
        usage: "rune prompt [--show]",
        flags: &[flag("--show", "Print the assembled prompt.")],
        requirements: Requirements::CONFIG,
        supports_json: false,
    },
];

/// Diagnostic and maintenance commands.
pub const MAINTENANCE: &[CommandSpec] = &[
    CommandSpec {
        name: "status",
        aliases: &[],
        summary: "Show the resolved configuration and runtime state",
        usage: "rune status [--json]",
        flags: &[flag("--json", "Emit JSON.")],
        requirements: Requirements::CONFIG,
        supports_json: true,
    },
    CommandSpec {
        name: "doctor",
        aliases: &[],
        summary: "Check the local setup without starting a turn",
        usage: "rune doctor [--json]",
        flags: &[flag("--json", "Emit JSON.")],
        requirements: Requirements::CONFIG,
        supports_json: true,
    },
    CommandSpec {
        name: "upgrade",
        aliases: &[],
        summary: "Upgrade the installed binary",
        usage: "rune upgrade [--channel <stable|dev>]",
        flags: &[option("--channel", "name", "Release channel.")],
        requirements: Requirements::NETWORK,
        supports_json: false,
    },
    CommandSpec {
        name: "uninstall",
        aliases: &[],
        summary: "Remove the installed binary and, with confirmation, local state",
        usage: "rune uninstall [--keep-state] [--yes]",
        flags: &[
            flag("--keep-state", "Leave the state directory in place."),
            flag("--yes", "Do not prompt for confirmation."),
        ],
        requirements: Requirements::CONFIG,
        supports_json: false,
    },
    CommandSpec {
        name: "reference",
        aliases: &[],
        summary: "Print the generated command reference",
        usage: "rune reference [--write <path>]",
        flags: &[option("--write", "path", "Write the reference to a file.")],
        requirements: Requirements::NONE,
        supports_json: false,
    },
    CommandSpec {
        name: "help",
        aliases: &["-h", "--help"],
        summary: "Print help",
        usage: "rune help [command]",
        flags: &[],
        requirements: Requirements::NONE,
        supports_json: false,
    },
    CommandSpec {
        name: "version",
        aliases: &["-v", "--version"],
        summary: "Print the version",
        usage: "rune version",
        flags: &[],
        requirements: Requirements::NONE,
        supports_json: false,
    },
];

/// Every command group, in help order.
pub const GROUPS: &[(&str, &[CommandSpec])] = &[
    ("Run", RUN),
    ("Sessions and local records", SESSIONS),
    ("Account and configuration", ACCOUNT),
    ("Diagnostics and maintenance", MAINTENANCE),
];

/// Every command, flattened.
#[must_use]
pub fn all_commands() -> Vec<&'static CommandSpec> {
    GROUPS.iter().flat_map(|(_, specs)| specs.iter()).collect()
}

/// Looks up a command by name or alias.
#[must_use]
pub fn find(name: &str) -> Option<&'static CommandSpec> {
    all_commands()
        .into_iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name))
}

/// Resolves a parsed command to its specification.
#[must_use]
pub fn spec_for(command: Command) -> Option<&'static CommandSpec> {
    find(command.as_str())
}

/// Returns true when a flag takes a value.
///
/// Read from the same table the parser and the reference use, so a flag declared
/// to take a value cannot be parsed as a boolean and silently swallow nothing.
/// A flag that is genuine but undeclared here is treated as a boolean, which is
/// the safe reading: it consumes no argument that belongs to the command.
#[must_use]
pub fn takes_value(name: &str) -> bool {
    GLOBAL_FLAGS
        .iter()
        .chain(all_commands().iter().flat_map(|spec| spec.flags.iter()))
        .any(|flag| flag.name == name && flag.value.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for spec in all_commands() {
            assert!(seen.insert(spec.name), "duplicate command {}", spec.name);
        }
    }

    #[test]
    fn aliases_do_not_collide_with_names() {
        let names: std::collections::HashSet<_> =
            all_commands().iter().map(|spec| spec.name).collect();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for spec in all_commands() {
            for alias in spec.aliases {
                assert!(!names.contains(alias), "alias `{alias}` shadows a command");
                assert!(seen.insert(alias), "alias `{alias}` is declared twice");
            }
        }
    }

    #[test]
    fn every_command_has_a_summary_and_usage() {
        for spec in all_commands() {
            assert!(!spec.summary.is_empty(), "{} has no summary", spec.name);
            assert!(
                spec.usage.starts_with("rune "),
                "{} usage does not start with the program name",
                spec.name
            );
            assert!(
                !spec.summary.ends_with('.'),
                "{} summary ends with a period",
                spec.name
            );
        }
    }

    #[test]
    fn flags_are_kebab_case() {
        for spec in all_commands() {
            for flag in spec.flags {
                let name = flag.name.trim_start_matches('-');
                assert!(
                    !name.contains('_'),
                    "{} flag `{}` is not kebab-case",
                    spec.name,
                    flag.name
                );
            }
        }
    }

    #[test]
    fn find_resolves_names_and_aliases() {
        assert_eq!(find("ask").map(|s| s.name), Some("ask"));
        assert_eq!(find("--help").map(|s| s.name), Some("help"));
        assert_eq!(find("-v").map(|s| s.name), Some("version"));
        assert!(find("nonexistent").is_none());
    }

    #[test]
    fn json_support_is_declared_consistently() {
        for spec in all_commands() {
            let declares_json = spec.flags.iter().any(|f| f.name == "--json");
            assert_eq!(
                declares_json, spec.supports_json,
                "{} declares --json={} but supports_json={}",
                spec.name, declares_json, spec.supports_json
            );
        }
    }

    #[test]
    fn help_and_version_need_nothing_from_the_runtime() {
        for name in ["help", "version"] {
            let spec = find(name).expect("present");
            assert!(!spec.requirements.network, "{name} needs no network");
            assert!(!spec.requirements.terminal, "{name} needs no terminal");
        }
    }

    #[test]
    fn only_interactive_commands_require_the_terminal() {
        for spec in all_commands() {
            if matches!(spec.name, "ask" | "review") {
                assert!(spec.requirements.terminal, "{} needs a terminal", spec.name);
            }
        }
    }
}
