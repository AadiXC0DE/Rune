//! User-defined slash commands.
//!
//! A repository can ship a workflow as a markdown file with a frontmatter block.
//! Running the command expands its body with the arguments the user typed and
//! hands the result to the composer for review: nothing here submits a turn.
//!
//! A project command never shadows a built-in. A conflict is reported rather
//! than resolved, because silently preferring one of two commands with the same
//! name is how a workflow stops being the one that runs.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::budget::LimitName;
use rune_core::error::{ErrorCode, Result, RuneError};
use serde::{Deserialize, Serialize};

/// Directories searched for commands, relative to a workspace.
pub const WORKSPACE_ROOTS: &[&str] = &[".rune/commands", ".claude/commands"];

/// Directory searched for commands, relative to the config root.
pub const CONFIG_ROOT: &str = "commands";

/// Largest command file accepted.
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;

/// Largest expansion produced from one command.
pub const MAX_EXPANSION_BYTES: usize = 256 * 1024;

/// Built-in command names a user command may not take.
///
/// A conflict is reported rather than resolved: a command that silently replaced
/// a built-in would make the built-in unreachable with no way to notice.
pub const RESERVED: &[&str] = &["help", "quit", "exit", "clear", "compact"];

/// Where a command came from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Shipped with the harness.
    Builtin,
    /// Declared by the configuration directory.
    User,
    /// Declared inside the workspace.
    Project,
}

impl Origin {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// One command.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Command {
    /// Name, without the leading slash.
    pub name: String,
    /// One-line description shown in a listing.
    pub description: String,
    /// Hint for the arguments, shown where completions are offered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// Body, with the frontmatter removed.
    pub body: String,
    /// Where it came from.
    pub origin: Origin,
    /// File it was read from, absent for a built-in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Utf8PathBuf>,
}

impl Command {
    /// Returns true when this command came from a repository.
    #[must_use]
    pub const fn is_project(&self) -> bool {
        matches!(self.origin, Origin::Project)
    }

    /// Expands the body with the arguments the user typed.
    ///
    /// `$ARGUMENTS` becomes every argument joined by a space. `$1` through `$9`
    /// become the positional arguments, and a trailing `:default` in the
    /// placeholder supplies a value when that position is absent.
    pub fn expand(&self, arguments: &[String]) -> Result<String> {
        let joined = arguments.join(" ");
        let mut out = String::with_capacity(self.body.len().saturating_add(joined.len()));

        let mut rest = self.body.as_str();
        while let Some(index) = rest.find('$') {
            out.push_str(&rest[..index]);
            rest = &rest[index..];

            if let Some(after) = rest.strip_prefix("$ARGUMENTS") {
                out.push_str(&joined);
                rest = after;
                continue;
            }

            // A positional placeholder is `$` then one digit, optionally
            // followed by a default introduced by a colon.
            let mut chars = rest.chars();
            chars.next();
            match chars.next() {
                Some(digit) if digit.is_ascii_digit() && digit != '0' => {
                    let position = usize::try_from(digit.to_digit(10).unwrap_or(0))
                        .unwrap_or(1)
                        .saturating_sub(1);
                    if arguments
                        .get(position)
                        .is_some_and(|value| !value.is_empty())
                    {
                        out.push_str(arguments.get(position).map_or("", String::as_str));
                        // A supplied argument replaces the placeholder and its
                        // default alike, so the suffix is consumed here too.
                        rest = skip_default(&rest[2..]);
                        continue;
                    }
                    match take_default(&rest[2..]) {
                        Some(default) => out.push_str(&default),
                        None => {
                            return Err(RuneError::new(
                                ErrorCode::MissingField,
                                format!(
                                    "command `/{name}` needs argument ${}",
                                    digit,
                                    name = self.name
                                ),
                            )
                            .with_hint(format!(
                                "pass the argument, as in `/{name} <value>`",
                                name = self.name
                            )));
                        }
                    }
                    rest = skip_default(&rest[2..]);
                }
                // A lone dollar sign is not a placeholder, and neither is one
                // followed by something that is not a digit.
                _ => {
                    out.push('$');
                    rest = &rest[1..];
                }
            }
        }
        out.push_str(rest);

        if out.len() > MAX_EXPANSION_BYTES {
            return Err(RuneError::too_large(
                "expansion",
                out.len(),
                MAX_EXPANSION_BYTES,
            ));
        }
        Ok(out)
    }
}

/// Reads a `:default` suffix from the text following a positional placeholder.
///
/// The default runs to the first whitespace, so `$1:main` supplies `main` and
/// `$1:a b` supplies `a` and leaves ` b` in the body.
fn take_default(text: &str) -> Option<String> {
    let rest = text.strip_prefix(':')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let value = &rest[..end];
    if value.is_empty() {
        return None;
    }
    Some(value.to_owned())
}

/// Advances past a `:default` suffix.
fn skip_default(text: &str) -> &str {
    let Some(rest) = text.strip_prefix(':') else {
        return text;
    };
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    &rest[end..]
}

/// A candidate that could not be loaded.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Warning {
    /// File that was refused.
    pub path: Utf8PathBuf,
    /// Why it was refused.
    pub reason: String,
}

/// Outcome of one discovery pass.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Discovery {
    /// Commands that loaded, ordered by name.
    pub commands: Vec<Command>,
    /// Candidates that were refused.
    pub warnings: Vec<Warning>,
}

/// Discovers the commands visible from a workspace.
pub fn discover(workspace: &Utf8Path, config_root: &Utf8Path) -> Result<Discovery> {
    let mut out = Discovery::default();
    let mut seen: BTreeMap<String, Origin> = BTreeMap::new();

    for root in WORKSPACE_ROOTS {
        scan_root(&workspace.join(root), Origin::Project, &mut seen, &mut out);
    }
    scan_root(
        &config_root.join(CONFIG_ROOT),
        Origin::User,
        &mut seen,
        &mut out,
    );

    out.commands
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(out)
}

/// Loads every command file under one root.
fn scan_root(
    root: &Utf8Path,
    origin: Origin,
    seen: &mut BTreeMap<String, Origin>,
    out: &mut Discovery,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let cap = crate::resolve_limit(LimitName::ListEntries).max(1);

    for entry in entries.flatten().take(cap) {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        let Some(name) = path.file_stem().map(str::to_owned) else {
            continue;
        };
        load_one(&path, &name, origin, seen, out);
    }
}

/// Loads one command file, recording a warning instead of failing.
///
/// One malformed file must not hide the others, so a refusal is recorded and the
/// scan continues.
fn load_one(
    path: &Utf8Path,
    name: &str,
    origin: Origin,
    seen: &mut BTreeMap<String, Origin>,
    out: &mut Discovery,
) {
    let warn = |reason: String, out: &mut Discovery| {
        out.warnings.push(Warning {
            path: path.to_owned(),
            reason,
        });
    };

    if RESERVED.contains(&name) {
        warn(
            format!("`/{name}` is a built-in command, so this file was skipped"),
            out,
        );
        return;
    }

    let Ok(text) = std::fs::read_to_string(path) else {
        warn(String::from("the file could not be read"), out);
        return;
    };
    if text.len() > MAX_COMMAND_BYTES {
        warn(
            format!("the file is larger than {MAX_COMMAND_BYTES} bytes"),
            out,
        );
        return;
    }

    let (frontmatter, body) = match split_frontmatter(&text) {
        Ok(parts) => parts,
        Err(reason) => {
            warn(reason, out);
            return;
        }
    };

    // A project command never replaces one already loaded: which of two files
    // with the same name wins would depend on directory order.
    if let Some(existing) = seen.get(name) {
        warn(
            format!(
                "`/{name}` is already declared at the {} level, so this file was skipped",
                existing.as_str()
            ),
            out,
        );
        return;
    }

    let description = frontmatter
        .description
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| first_line(&body));
    if description.trim().is_empty() {
        warn(String::from("the command has no description"), out);
        return;
    }

    seen.insert(name.to_owned(), origin);
    out.commands.push(Command {
        name: name.to_owned(),
        description: description.trim().to_owned(),
        argument_hint: frontmatter.argument_hint,
        body,
        origin,
        source: Some(path.to_owned()),
    });
}

/// Frontmatter fields a command file may declare.
#[derive(Debug, Default)]
struct Frontmatter {
    description: Option<String>,
    argument_hint: Option<String>,
}

/// Splits a file into its frontmatter and its body.
///
/// A file with no frontmatter block is accepted whole, so a bare markdown file
/// works as a command.
fn split_frontmatter(text: &str) -> std::result::Result<(Frontmatter, String), String> {
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = trimmed.strip_prefix("---") else {
        return Ok((Frontmatter::default(), trimmed.to_owned()));
    };
    let rest = rest.strip_prefix('\n').unwrap_or(rest);

    let Some(end) = rest.find("\n---") else {
        return Err(String::from(
            "the frontmatter block is opened but never closed",
        ));
    };
    let header = &rest[..end];
    let body = rest[end..]
        .trim_start_matches("\n---")
        .trim_start_matches('\n')
        .to_owned();

    Ok((parse_frontmatter(header)?, body))
}

/// Reads the frontmatter fields this module understands.
///
/// Only a small set of keys is read. An unknown key is ignored rather than
/// rejected, so a file written for another tool still loads.
fn parse_frontmatter(header: &str) -> std::result::Result<Frontmatter, String> {
    let mut out = Frontmatter::default();
    for line in header.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("`{line}` is not a `key: value` pair"));
        };
        let value = value.trim().trim_matches(['"', '\'']).to_owned();
        match key.trim() {
            "description" => out.description = Some(value),
            "argument-hint" | "argument_hint" => out.argument_hint = Some(value),
            _ => {}
        }
    }
    Ok(out)
}

/// Returns the first non-blank line of a body.
fn first_line(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .trim_start_matches('#')
        .trim()
        .to_owned()
}

/// Renders a listing for a terminal.
///
/// A project command is marked, so it is not mistaken for a built-in.
#[must_use]
pub fn render_listing(commands: &[Command]) -> String {
    if commands.is_empty() {
        return "no commands are defined".to_owned();
    }
    let mut out = String::new();
    let width = commands
        .iter()
        .map(|command| command.name.len())
        .max()
        .unwrap_or(0);
    for command in commands {
        let marker = match command.origin {
            Origin::Project => " (project)",
            Origin::User => "",
            Origin::Builtin => " (built-in)",
        };
        // A hint that already carries its own brackets is shown as written.
        let hint = command
            .argument_hint
            .as_ref()
            .map_or(String::new(), |hint| {
                let trimmed = hint.trim();
                if trimmed.starts_with('<') && trimmed.ends_with('>') {
                    format!(" {trimmed}")
                } else {
                    format!(" <{trimmed}>")
                }
            });
        let _ = writeln!(
            out,
            "/{:<width$}{hint}{marker}  {description}",
            command.name,
            description = command.description,
        );
    }
    out.trim_end().to_owned()
}

/// Renders a listing as JSON.
#[must_use]
pub fn to_json(commands: &[Command]) -> serde_json::Value {
    serde_json::Value::Array(
        commands
            .iter()
            .map(|command| {
                serde_json::json!({
                    "name": command.name,
                    "description": command.description,
                    "argument_hint": command.argument_hint,
                    "origin": command.origin.as_str(),
                    "source": command.source,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn command(name: &str, body: &str) -> Command {
        Command {
            name: name.to_owned(),
            description: "does a thing".to_owned(),
            argument_hint: None,
            body: body.to_owned(),
            origin: Origin::Project,
            source: None,
        }
    }

    fn write(root: &Utf8Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, contents).expect("write");
    }

    #[test]
    fn arguments_are_substituted() {
        let expanded = command("fix", "Fix $ARGUMENTS please.").expand(&["the parser".to_owned()]);
        assert_eq!(expanded.expect("expanded"), "Fix the parser please.");
    }

    #[test]
    fn several_arguments_join_with_a_space() {
        let expanded = command("fix", "$ARGUMENTS")
            .expand(&["one".to_owned(), "two".to_owned()])
            .expect("expanded");
        assert_eq!(expanded, "one two");
    }

    #[test]
    fn a_positional_placeholder_takes_the_matching_argument() {
        let expanded = command("mv", "from $1 to $2")
            .expand(&["a".to_owned(), "b".to_owned()])
            .expect("expanded");
        assert_eq!(expanded, "from a to b");
    }

    #[test]
    fn a_positional_placeholder_falls_back_to_its_default() {
        let expanded = command("branch", "base $1:main now")
            .expand(&[])
            .expect("expanded");
        assert_eq!(expanded, "base main now");
    }

    #[test]
    fn a_given_argument_beats_the_default() {
        let expanded = command("branch", "base $1:main now")
            .expand(&["dev".to_owned()])
            .expect("expanded");
        assert_eq!(expanded, "base dev now");
    }

    #[test]
    fn a_missing_required_argument_names_the_placeholder() {
        let err = command("mv", "from $1 to $2")
            .expand(&["a".to_owned()])
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::MissingField);
        assert!(err.message().contains("$2"), "{}", err.message());
        assert!(err.hint().is_some());
    }

    #[test]
    fn an_empty_argument_does_not_satisfy_a_placeholder() {
        let err = command("mv", "$1")
            .expand(&[String::new()])
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::MissingField);
    }

    #[test]
    fn a_lone_dollar_sign_is_left_alone() {
        let expanded = command("cost", "it costs $ 5 and a$b")
            .expand(&[])
            .expect("expanded");
        assert_eq!(expanded, "it costs $ 5 and a$b");
    }

    #[test]
    fn a_dollar_before_zero_is_not_a_positional() {
        let expanded = command("zero", "$0 is the program")
            .expand(&[])
            .expect("expanded");
        assert_eq!(expanded, "$0 is the program");
    }

    #[test]
    fn placeholders_and_arguments_mix_in_one_body() {
        let expanded = command("mix", "$1 then $ARGUMENTS then $2:b")
            .expand(&["a".to_owned()])
            .expect("expanded");
        assert_eq!(expanded, "a then a then b");
    }

    #[test]
    fn frontmatter_supplies_the_description_and_hint() {
        let (front, body) = split_frontmatter(
            "---\ndescription: Review a pull request\nargument-hint: <number>\n---\nBody here.\n",
        )
        .expect("split");
        assert_eq!(front.description.as_deref(), Some("Review a pull request"));
        assert_eq!(front.argument_hint.as_deref(), Some("<number>"));
        assert_eq!(body, "Body here.\n");
    }

    #[test]
    fn a_file_without_frontmatter_loads_whole() {
        let (front, body) = split_frontmatter("Just a body.\n").expect("split");
        assert!(front.description.is_none());
        assert_eq!(body, "Just a body.\n");
    }

    #[test]
    fn an_unclosed_frontmatter_block_is_refused() {
        let err = split_frontmatter("---\ndescription: x\nBody").expect_err("refused");
        assert!(err.contains("never closed"), "{err}");
    }

    #[test]
    fn a_frontmatter_line_that_is_not_a_pair_is_refused() {
        let err = split_frontmatter("---\njust words\n---\nBody").expect_err("refused");
        assert!(err.contains("key: value"), "{err}");
    }

    #[test]
    fn an_unknown_frontmatter_key_is_ignored() {
        let (front, _) = split_frontmatter("---\nunknown: value\n---\nBody").expect("split");
        assert!(front.description.is_none());
    }

    #[test]
    fn quoted_frontmatter_values_lose_their_quotes() {
        let (front, _) =
            split_frontmatter("---\ndescription: \"quoted text\"\n---\n").expect("split");
        assert_eq!(front.description.as_deref(), Some("quoted text"));
    }

    #[test]
    fn a_project_command_is_discovered_and_marked() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        write(
            root,
            ".rune/commands/fix.md",
            "---\ndescription: Fix it\n---\nBody",
        );

        let discovery = discover(root, root).expect("discovered");
        assert_eq!(discovery.commands.len(), 1);
        let command = &discovery.commands[0];
        assert_eq!(command.name, "fix");
        assert!(command.is_project());
        assert!(render_listing(&discovery.commands).contains("(project)"));
    }

    #[test]
    fn a_config_root_command_is_discovered() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let config = root.join("config");
        write(
            &config,
            "commands/deploy.md",
            "---\ndescription: Deploy\n---\nBody",
        );

        let discovery = discover(root, &config).expect("discovered");
        assert_eq!(discovery.commands.len(), 1);
        assert_eq!(discovery.commands[0].origin, Origin::User);
    }

    #[test]
    fn a_description_falls_back_to_the_first_body_line() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        write(root, ".rune/commands/note.md", "# Add a note\n\nBody");

        let discovery = discover(root, root).expect("discovered");
        assert_eq!(discovery.commands[0].description, "Add a note");
    }

    #[test]
    fn a_built_in_name_is_refused_and_reported() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        write(
            root,
            ".rune/commands/help.md",
            "---\ndescription: Sneaky\n---\nBody",
        );

        let discovery = discover(root, root).expect("discovered");
        assert!(discovery.commands.is_empty(), "{:#?}", discovery.commands);
        assert_eq!(discovery.warnings.len(), 1);
        assert!(
            discovery.warnings[0].reason.contains("built-in"),
            "{}",
            discovery.warnings[0].reason
        );
    }

    #[test]
    fn a_duplicate_name_across_roots_is_reported() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        write(
            root,
            ".rune/commands/fix.md",
            "---\ndescription: First\n---\nBody",
        );
        write(
            root,
            ".claude/commands/fix.md",
            "---\ndescription: Second\n---\nBody",
        );

        let discovery = discover(root, root).expect("discovered");
        assert_eq!(discovery.commands.len(), 1);
        assert_eq!(discovery.commands[0].description, "First");
        assert_eq!(discovery.warnings.len(), 1);
        assert!(
            discovery.warnings[0].reason.contains("already declared"),
            "{}",
            discovery.warnings[0].reason
        );
    }

    #[test]
    fn a_malformed_file_is_skipped_while_others_load() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        write(
            root,
            ".rune/commands/good.md",
            "---\ndescription: Fine\n---\nBody",
        );
        write(
            root,
            ".rune/commands/bad.md",
            "---\ndescription: x\nUnclosed",
        );

        let discovery = discover(root, root).expect("discovered");
        assert_eq!(discovery.commands.len(), 1);
        assert_eq!(discovery.commands[0].name, "good");
        assert_eq!(discovery.warnings.len(), 1);
    }

    #[test]
    fn a_non_markdown_file_is_not_a_command() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        write(root, ".rune/commands/notes.txt", "not a command");

        let discovery = discover(root, root).expect("discovered");
        assert!(discovery.commands.is_empty());
        assert!(discovery.warnings.is_empty());
    }

    #[test]
    fn commands_are_listed_by_name() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        write(
            root,
            ".rune/commands/zeta.md",
            "---\ndescription: Z\n---\nB",
        );
        write(
            root,
            ".rune/commands/alpha.md",
            "---\ndescription: A\n---\nB",
        );

        let discovery = discover(root, root).expect("discovered");
        let names: Vec<&str> = discovery
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[test]
    fn a_missing_root_is_not_an_error() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let discovery = discover(root, root).expect("discovered");
        assert!(discovery.commands.is_empty());
        assert!(discovery.warnings.is_empty());
    }

    #[test]
    fn an_already_bracketed_hint_is_not_wrapped_again() {
        let mut with_hint = command("deploy", "B");
        with_hint.argument_hint = Some("<env>".to_owned());
        let rendered = render_listing(&[with_hint]);
        assert!(rendered.contains("<env>"), "{rendered}");
        assert!(!rendered.contains("<<"), "{rendered}");
    }

    #[test]
    fn a_bare_hint_is_bracketed() {
        let mut with_hint = command("deploy", "B");
        with_hint.argument_hint = Some("env".to_owned());
        assert!(render_listing(&[with_hint]).contains("<env>"));
    }

    #[test]
    fn listing_nothing_says_so() {
        assert_eq!(render_listing(&[]), "no commands are defined");
    }

    #[test]
    fn the_listing_shows_the_argument_hint() {
        let mut with_hint = command("deploy", "B");
        with_hint.argument_hint = Some("<environment>".to_owned());
        assert!(render_listing(&[with_hint]).contains("<environment>"));
    }

    #[test]
    fn the_json_listing_names_every_field() {
        let value = to_json(&[command("fix", "B")]);
        assert_eq!(value[0]["name"], "fix");
        assert_eq!(value[0]["origin"], "project");
    }

    #[test]
    fn an_expansion_past_the_cap_is_refused() {
        let big = command("big", &"x".repeat(MAX_EXPANSION_BYTES + 1));
        let err = big.expand(&[]).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[test]
    fn every_origin_has_a_wire_name() {
        for origin in [Origin::Builtin, Origin::User, Origin::Project] {
            assert!(!origin.as_str().is_empty());
        }
    }

    #[test]
    fn expansion_never_adds_the_leading_slash() {
        // The composer receives the body, not the invocation.
        let expanded = command("fix", "$ARGUMENTS")
            .expand(&["x".to_owned()])
            .expect("ok");
        assert_eq!(expanded, "x");
        assert!(!expanded.starts_with('/'));
    }
}
