//! `rune config get`, `set`, `unset`, `path` and `edit`.
//!
//! A set or unset edits one top-level line of the user file and leaves every
//! other byte as it was, so comments, spacing and tables survive.

use std::process::ExitCode;

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::config::{self, Diagnostic, Settings, USER_KEYS};
use rune_core::error::{ErrorCode, Result, RuneError};

/// Exit status of a key that names no setting.
pub const EXIT_UNKNOWN_KEY: u8 = 2;

/// First line of a user file that `rune config edit` creates.
pub const NEW_FILE_HEADER: &str =
    "# Rune configuration. rune config --explain lists every setting.\n";

/// The error for a key that names no setting, with the nearest one as a hint.
pub fn unknown_setting<'a>(key: &str, known: impl IntoIterator<Item = &'a str>) -> RuneError {
    let err = RuneError::new(ErrorCode::InvalidField, format!("unknown setting {key}"));
    match config::suggest(key, known) {
        Some(suggestion) => err.with_hint(format!("did you mean {suggestion}?")),
        None => err.with_hint("run rune config --explain to list every setting"),
    }
}

/// Prints the effective value of one setting.
pub fn get(settings: &Settings, key: &str, json: bool) -> std::result::Result<(), RuneError> {
    let rows = settings.explain();
    let Some(row) = rows.iter().find(|row| row.key == key) else {
        return Err(unknown_setting(
            key,
            rows.iter().map(|row| row.key.as_str()),
        ));
    };
    if json {
        let value = serde_json::json!({
            "key": row.key,
            "value": row.value,
            "source": row.source,
        });
        println!("{value}");
    } else {
        println!("{}", row.value);
    }
    Ok(())
}

/// Writes one scalar setting into the user file and returns the line written.
pub fn set(path: &Utf8Path, key: &str, raw: &str) -> Result<String> {
    if !USER_KEYS.contains(&key) {
        return Err(unknown_setting(key, USER_KEYS.iter().copied()));
    }
    let literal = config::scalar_literal(key, raw)?;
    let line = format!("{key} = {literal}");
    create_parent(path)?;
    rune_policy::settings::update(path, |previous| {
        let text = previous.unwrap_or("");
        let edited = set_line(text, key, &line, path)?;
        validate(path, &edited, key)?;
        Ok(edited)
    })?;
    Ok(line)
}

/// Removes one top-level setting from the user file.
pub fn unset(path: &Utf8Path, key: &str) -> Result<()> {
    if !USER_KEYS.contains(&key) {
        return Err(unknown_setting(key, USER_KEYS.iter().copied()));
    }
    let not_set = || RuneError::new(ErrorCode::NotFound, format!("{key} is not set in {path}"));
    if !path.exists() {
        return Err(not_set());
    }
    rune_policy::settings::update(path, |previous| {
        let text = previous.unwrap_or("");
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let Some(index) = top_level_line(&lines, key) else {
            return Err(not_set());
        };
        let edited: String = lines
            .iter()
            .enumerate()
            .filter(|(position, _)| *position != index)
            .map(|(_, line)| *line)
            .collect();
        validate(path, &edited, key)?;
        Ok(edited)
    })?;
    Ok(())
}

/// Prints the user file path, and with `json` the project file path too.
pub fn print_path(user: &Utf8Path, workspace: &Utf8Path, json: bool) {
    if json {
        let value = serde_json::json!({
            "path": user,
            "project": workspace.join(".rune.toml"),
        });
        println!("{value}");
    } else {
        println!("{user}");
    }
}

/// Opens the user file in the editor, then reports what a load makes of it.
pub fn edit(path: &Utf8Path) -> Result<ExitCode> {
    if !path.exists() {
        create_parent(path)?;
        rune_core::paths::write_private(path, NEW_FILE_HEADER)?;
    }
    let editor = editor_command();
    let mut words = editor.split_whitespace();
    let program = words.next().unwrap_or(DEFAULT_EDITOR);
    let status = std::process::Command::new(program)
        .args(words)
        .arg(path.as_str())
        .status()
        .map_err(|err| {
            RuneError::new(
                ErrorCode::IoFailure,
                format!("could not run {program}: {err}"),
            )
            .with_hint("set VISUAL or EDITOR to the editor to use")
        })?;
    if !status.success() {
        return Err(RuneError::new(
            ErrorCode::IoFailure,
            format!("{program} exited with {status}"),
        ));
    }
    let text = rune_core::paths::read_private(path, config::MAX_CONFIG_BYTES)?.unwrap_or_default();
    match config::check_user_text(path, &text) {
        Ok(diagnostics) if diagnostics.is_empty() => println!("{path} is valid"),
        Ok(diagnostics) => diagnostics.iter().for_each(print_diagnostic),
        Err(diagnostic) => print_diagnostic(&diagnostic),
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(windows)]
const DEFAULT_EDITOR: &str = "notepad";
#[cfg(not(windows))]
const DEFAULT_EDITOR: &str = "vi";

fn editor_command() -> String {
    ["VISUAL", "EDITOR"]
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| DEFAULT_EDITOR.to_owned())
}

fn print_diagnostic(diagnostic: &Diagnostic) {
    println!("{}", diagnostic.message);
    if let Some(hint) = &diagnostic.hint {
        println!("  {hint}");
    }
}

fn create_parent(path: &Utf8Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_str().is_empty()) {
        rune_core::paths::create_dir_private(parent)?;
    }
    Ok(())
}

/// Returns the index of the first line that starts a table.
fn first_table(lines: &[&str]) -> usize {
    lines
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .unwrap_or(lines.len())
}

/// Returns the top-level line that defines `key` as `key = ...`.
fn top_level_line(lines: &[&str], key: &str) -> Option<usize> {
    lines[..first_table(lines)].iter().position(|line| {
        line.trim_start()
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

/// Replaces or inserts the top-level line for `key`.
fn set_line(text: &str, key: &str, line: &str, path: &Utf8Path) -> Result<String> {
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
    let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
    if let Some(index) = top_level_line(&borrowed, key) {
        let ending = line_ending(&lines[index]);
        lines[index] = format!("{line}{ending}");
        return Ok(lines.concat());
    }
    let defined = toml::from_str::<toml::Table>(text).is_ok_and(|table| table.contains_key(key));
    if defined {
        return Err(RuneError::new(
            ErrorCode::InvalidField,
            format!("{key} is defined inside a table in {path}; use rune config edit"),
        ));
    }
    let ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let top = first_table(&borrowed);
    let last = borrowed[..top].iter().rposition(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty() && !trimmed.starts_with('#')
    });
    let position = match last {
        Some(index) => {
            if let Some(previous) = lines.get_mut(index)
                && line_ending(previous).is_empty()
            {
                previous.push_str(ending);
            }
            index.saturating_add(1)
        }
        None => 0,
    };
    lines.insert(position, format!("{line}{ending}"));
    Ok(lines.concat())
}

fn line_ending(line: &str) -> &'static str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

/// Refuses an edit the next load would not read cleanly for `key`.
fn validate(path: &Utf8Path, text: &str, key: &str) -> Result<()> {
    let refused = |diagnostic: Diagnostic| {
        let err = RuneError::new(ErrorCode::InvalidConfiguration, diagnostic.message);
        match diagnostic.hint {
            Some(hint) => err.with_hint(hint),
            None => err,
        }
    };
    let diagnostics = config::check_user_text(path, text).map_err(refused)?;
    let prefix = format!("{key}.");
    if let Some(diagnostic) = diagnostics.into_iter().find(|diagnostic| {
        diagnostic
            .key
            .as_deref()
            .is_some_and(|name| name == key || name.starts_with(&prefix))
    }) {
        return Err(refused(diagnostic));
    }
    Ok(())
}

/// Returns the user file path, honouring `RUNE_CONFIG`.
pub fn user_file(paths: &rune_core::paths::Paths) -> Utf8PathBuf {
    paths.config_file(std::env::var("RUNE_CONFIG").ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> Utf8PathBuf {
        Utf8PathBuf::from("/config.toml")
    }

    #[test]
    fn a_top_level_line_is_replaced_in_place() {
        let text = "# keep\ntheme   =  \"light\" # old\n\n[pre_tool_hook]\ntheme = 1\n";
        let edited = set_line(text, "theme", "theme = \"dark\"", &path()).expect("set");
        assert_eq!(
            edited,
            "# keep\ntheme = \"dark\"\n\n[pre_tool_hook]\ntheme = 1\n"
        );
    }

    #[test]
    fn a_comment_only_file_gets_the_line_at_the_top() {
        let text = "# one\n# two\n";
        let edited = set_line(text, "theme", "theme = \"dark\"", &path()).expect("set");
        assert_eq!(edited, "theme = \"dark\"\n# one\n# two\n");
    }

    #[test]
    fn a_dotted_definition_is_refused() {
        let text = "[theme]\nname = \"x\"\n";
        let err = set_line(text, "theme", "theme = \"dark\"", &path()).expect_err("table");
        assert_eq!(
            err.message(),
            "theme is defined inside a table in /config.toml; use rune config edit"
        );
    }

    #[test]
    fn a_missing_final_newline_is_added_before_inserting() {
        let edited = set_line("offline = true", "theme", "theme = \"dark\"", &path()).expect("set");
        assert_eq!(edited, "offline = true\ntheme = \"dark\"\n");
    }
}
