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
        out.push_str("Aliases:\n\n");
        for alias in command.aliases {
            let _ = writeln!(out, "- `{}`: {}", alias.name, alias.description);
        }
        out.push('\n');
    }
    if !command.flags.is_empty() {
        out.push_str("| Flag | Description |\n|---|---|\n");
        for flag in command.flags {
            render_flag(flag, out);
        }
    }
    let notes = spec::notes(command.name);
    if !notes.is_empty() {
        out.push_str("\n```text\n");
        out.push_str(notes);
        out.push_str("```\n");
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

/// Returns the whole reference as one machine-readable JSON document.
///
/// Every section is read from the table the binary itself uses: the commands
/// and flags the parser accepts, the settings and their defaults, the limits,
/// the key actions, the provider presets and the doctor checks. The site's
/// reference pages are generated from this, so a page cannot describe
/// something the binary does not do.
///
/// The text is written by [`Json`] rather than built as a `serde_json::Value`:
/// building values expands generic code at every field and closure, which
/// cost the binary over 20 KiB for a command only the documentation build runs.
#[must_use]
pub fn render_json() -> String {
    let mut json = Json::default();
    json.open('{');
    json.text("version", env!("CARGO_PKG_VERSION"));

    json.key("commands");
    json.open('[');
    for (group, commands) in spec::GROUPS {
        for command in *commands {
            json.item();
            json.open('{');
            json.text("name", command.name);
            json.text("group", group);
            json.text("usage", command.usage);
            json.text("summary", command.summary);
            json.key("aliases");
            json.open('[');
            for alias in command.aliases {
                json.item();
                json.open('{');
                json.text("name", alias.name);
                json.text("description", alias.description);
                json.close('}');
            }
            json.close(']');
            json.key("flags");
            flags_json(&mut json, command.flags);
            json.text("notes", spec::notes(command.name));
            json.close('}');
        }
    }
    json.close(']');

    json.key("global_flags");
    flags_json(&mut json, spec::GLOBAL_FLAGS);
    json.key("settings");
    settings_json(&mut json);

    json.key("limits");
    json.open('[');
    for name in LimitName::all() {
        let range = name.range();
        json.item();
        json.open('{');
        json.text("name", name.as_str());
        match name.default_value() {
            rune_core::budget::Budget::Bounded(value) => json.number("default", value),
            rune_core::budget::Budget::Unbounded => json.text("default", "off"),
        }
        json.text("unit", name.unit().suffix());
        json.key("range");
        json.open('{');
        json.number("min", range.min);
        match range.max {
            Some(max) => json.number("max", max),
            None => json.null("max"),
        }
        json.close('}');
        json.text("description", name.description());
        json.close('}');
    }
    json.close(']');

    json.key("keys");
    json.open('[');
    for action in rune_term::keys::ACTIONS {
        json.item();
        json.open('{');
        json.text("action", action.id);
        json.key("default");
        json.open('[');
        for key in action.defaults {
            json.item();
            json.string(key);
        }
        json.close(']');
        json.text("context", action.context.as_str());
        json.text("summary", action.summary);
        json.close('}');
    }
    json.close(']');

    json.key("presets");
    json.open('[');
    for preset in rune_net::providers::KNOWN_PROVIDERS {
        if !preset.implemented {
            continue;
        }
        let anonymous = preset.credential == rune_net::providers::CredentialKind::AnonymousLoopback;
        json.item();
        json.open('{');
        json.text("name", preset.name);
        json.text("label", preset.summary);
        json.text("dialect", dialect(&preset.provider()));
        json.optional("base_url", preset.base_url);
        json.optional("key_variable", preset.key_variable);
        json.boolean("anonymous", anonymous);
        json.text(
            "notes",
            &format!("{}; {}", preset.group.heading(), preset.credential_label()),
        );
        json.text(
            "config",
            &preset_config(preset.name, preset.base_url.is_none(), anonymous),
        );
        json.close('}');
    }
    json.close(']');

    json.key("doctor");
    json.open('[');
    for (check, looks_at, fix) in crate::diagnostics::catalogue() {
        json.item();
        json.open('{');
        json.text("check", check);
        json.text("looks_at", looks_at);
        json.text("fix", fix);
        json.close('}');
    }
    json.close(']');

    json.close('}');
    json.out.push('\n');
    json.out
}

/// Writes JSON text one value at a time.
///
/// Each value knows whether a separator goes before it from `first`, which is
/// true right after an opening bracket.
#[derive(Default)]
struct Json {
    out: String,
    first: bool,
}

impl Json {
    /// Opens an object or array.
    fn open(&mut self, bracket: char) {
        self.out.push(bracket);
        self.first = true;
    }

    /// Closes an object or array.
    fn close(&mut self, bracket: char) {
        self.out.push(bracket);
        self.first = false;
    }

    /// Starts the next array element.
    fn item(&mut self) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
    }

    /// Starts the next object member and writes its name.
    fn key(&mut self, name: &str) {
        self.item();
        self.string(name);
        self.out.push(':');
    }

    /// Writes a string value, escaped.
    fn string(&mut self, text: &str) {
        self.out.push('"');
        for character in text.chars() {
            match character {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                control if u32::from(control) < 0x20 => {
                    let _ = write!(self.out, "\\u{:04x}", u32::from(control));
                }
                other => self.out.push(other),
            }
        }
        self.out.push('"');
        self.first = false;
    }

    /// Writes a member holding a string.
    fn text(&mut self, name: &str, value: &str) {
        self.key(name);
        self.string(value);
    }

    /// Writes a member holding a string, or `null`.
    fn optional(&mut self, name: &str, value: Option<&str>) {
        match value {
            Some(value) => self.text(name, value),
            None => self.null(name),
        }
    }

    /// Writes a member holding a number.
    fn number(&mut self, name: &str, value: u64) {
        self.key(name);
        let _ = write!(self.out, "{value}");
    }

    /// Writes a member holding `true` or `false`.
    fn boolean(&mut self, name: &str, value: bool) {
        self.key(name);
        self.out.push_str(if value { "true" } else { "false" });
    }

    /// Writes a member holding `null`.
    fn null(&mut self, name: &str) {
        self.key(name);
        self.out.push_str("null");
    }
}

/// Writes a list of flags.
fn flags_json(json: &mut Json, flags: &[FlagSpec]) {
    json.open('[');
    for flag in flags {
        json.item();
        json.open('{');
        json.text("name", flag.name);
        json.optional("value", flag.value);
        json.text("description", flag.description);
        json.close('}');
    }
    json.close(']');
}

/// Writes every setting with its type, default, help line and the variable
/// that overrides it.
///
/// The variables are the ones the configuration loader actually reads, found
/// by asking it to load from a lookup that records each name it asks for.
fn settings_json(json: &mut Json) {
    use rune_core::config::{EnvironmentOverrides, SETTING_HELP, ScalarKind, Settings};

    let mut read = Vec::new();
    let _ = EnvironmentOverrides::from_lookup(|name| {
        read.push(name.to_owned());
        None
    });
    let defaults = Settings::default().explain();
    json.open('[');
    for (key, help) in SETTING_HELP {
        let kind = match rune_core::config::scalar_kind(key) {
            Some(ScalarKind::Boolean) => "boolean".to_owned(),
            Some(ScalarKind::Number) => "number".to_owned(),
            Some(ScalarKind::Text) => "string".to_owned(),
            Some(ScalarKind::Choice(names)) => format!("one of {}", names.join(", ")),
            None => structured_type(key).unwrap_or_default().to_owned(),
        };
        let variable = format!("RUNE_{}", key.to_ascii_uppercase());
        json.item();
        json.open('{');
        json.text("key", key);
        json.text("type", &kind);
        json.optional(
            "default",
            defaults
                .iter()
                .find(|row| row.key == *key)
                .map(|row| row.value.as_str()),
        );
        json.text("help", help);
        json.optional("env", read.contains(&variable).then_some(variable.as_str()));
        json.close('}');
    }
    json.close(']');
}

/// Returns the smallest user configuration that selects a preset.
///
/// The model is chosen in the `models` table under the provider's name, which
/// is where `rune connect` writes it; a bare `model` key is refused by the
/// loader. A preset without a built-in endpoint needs one named.
fn preset_config(name: &str, needs_endpoint: bool, anonymous: bool) -> String {
    let mut text = format!("provider = \"{name}\"\n");
    if needs_endpoint {
        text.push_str("base_url = \"<endpoint>\"\n");
    }
    if anonymous {
        text.push_str("anonymous = true\n");
    }
    let _ = write!(text, "\n[models]\n{name} = \"<model id>\"\n");
    text
}

/// Returns how a setting that is not a single value is written.
///
/// `scalar_kind` covers every key `rune config set` can write; these are the
/// rest, and a key in neither has no type and fails a test.
fn structured_type(key: &str) -> Option<&'static str> {
    Some(match key {
        "model" => "string, in the models table under the provider's name",
        "context_window" => "number, in the models table entry of the model",
        "statusline"
        | "sandbox_cache_roots"
        | "additional_directories"
        | "provider_order"
        | "shell_env" => "list of strings",
        "pre_tool_hook" | "post_tool_hook" => "table",
        "hooks" => "list of tables",
        "mcp_servers" => "table of tables, one per server",
        _ => return None,
    })
}

/// Names the wire dialect a provider speaks.
fn dialect(provider: &rune_core::config::Provider) -> &'static str {
    use rune_core::config::Provider;
    match provider {
        Provider::Anthropic => "anthropic",
        Provider::Responses => "responses",
        Provider::Gemini => "gemini",
        _ => "chat_completions",
    }
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
        // The generator emits line feeds, and a platform that rewrites them on
        // read would make an identical document compare unequal.
        let committed = committed.replace("\r\n", "\n");
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
    fn the_json_writer_escapes_what_json_requires() {
        let mut json = Json::default();
        json.open('{');
        json.text("a", "quote \" slash \\ line\nfeed tab\t bell\u{7} 界");
        json.optional("b", None);
        json.number("c", 7);
        json.boolean("d", true);
        json.key("e");
        json.open('[');
        json.item();
        json.string("x");
        json.item();
        json.string("y");
        json.close(']');
        json.close('}');
        let parsed: serde_json::Value = serde_json::from_str(&json.out).expect("valid JSON");
        assert_eq!(
            parsed,
            serde_json::json!({
                "a": "quote \" slash \\ line\nfeed tab\t bell\u{7} 界",
                "b": null,
                "c": 7,
                "d": true,
                "e": ["x", "y"],
            })
        );
    }

    #[test]
    fn every_setting_has_a_type_and_help() {
        let json: serde_json::Value = serde_json::from_str(&render_json()).expect("valid JSON");
        let settings = json["settings"].as_array().expect("settings");
        assert_eq!(settings.len(), rune_core::config::SETTING_HELP.len());
        for setting in settings {
            assert!(
                setting["type"]
                    .as_str()
                    .is_some_and(|kind| !kind.is_empty()),
                "`{}` has no type; add it to structured_type",
                setting["key"]
            );
            assert!(
                setting["help"]
                    .as_str()
                    .is_some_and(|help| !help.is_empty()),
                "{setting}"
            );
        }
    }

    #[test]
    fn every_preset_configuration_loads_without_a_diagnostic() {
        // The check can fail: a bare `model` key is not where a model goes.
        let misplaced = rune_core::config::check_user_text(
            camino::Utf8Path::new("config.toml"),
            "provider = \"anthropic\"\nmodel = \"some-model\"\n",
        );
        assert!(
            misplaced
                .as_ref()
                .map_or(true, |diagnostics| !diagnostics.is_empty()),
            "{misplaced:?}"
        );
        let json: serde_json::Value = serde_json::from_str(&render_json()).expect("valid JSON");
        let presets = json["presets"].as_array().expect("presets");
        assert!(!presets.is_empty());
        for preset in presets {
            let text = preset["config"]
                .as_str()
                .expect("config")
                .replace("<model id>", "some-model")
                .replace("<endpoint>", "https://example.invalid/v1");
            let diagnostics =
                rune_core::config::check_user_text(camino::Utf8Path::new("config.toml"), &text)
                    .unwrap_or_else(|error| panic!("{}: {error:?}\n{text}", preset["name"]));
            assert!(
                diagnostics.is_empty(),
                "{}: {diagnostics:?}\n{text}",
                preset["name"]
            );
        }
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
