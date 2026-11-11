//! The key binding file, `keys.toml` in the config root.
//!
//! A flat table of action id to a key name or a list of key names. An action
//! named there loses its default keys; every other action keeps them.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use camino::Utf8Path;
use rune_core::error::{Result, RuneError};
use rune_term::keys::{ACTIONS, Keymap};

/// Reads the key binding file, returning `None` when there is none.
///
/// # Errors
///
/// Returns an invalid-field error naming the file when it cannot be read,
/// is not a table of key names, or binds keys the keymap refuses.
pub fn load(path: &Utf8Path) -> Result<Option<Keymap>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(invalid(path, format!("cannot read the keys file: {err}"))),
    };
    let overrides = parse(&text).map_err(|message| invalid(path, message))?;
    Keymap::with_overrides(&overrides)
        .map(Some)
        .map_err(|message| invalid(path, message))
}

/// Turns the file's table into action ids and key names.
fn parse(text: &str) -> std::result::Result<BTreeMap<String, Vec<String>>, String> {
    let table: toml::Table = text.parse().map_err(|err: toml::de::Error| {
        format!("the keys file is not valid TOML: {}", err.message())
    })?;
    let not_keys = |id: &str| format!("`{id}` needs a key name or a list of key names");
    table
        .into_iter()
        .map(|(id, value)| {
            let keys = match value {
                toml::Value::String(name) => vec![name],
                toml::Value::Array(items) => items
                    .into_iter()
                    .map(|item| match item {
                        toml::Value::String(name) => Ok(name),
                        _ => Err(not_keys(&id)),
                    })
                    .collect::<std::result::Result<Vec<_>, _>>()?,
                _ => return Err(not_keys(&id)),
            };
            Ok((id, keys))
        })
        .collect()
}

fn invalid(path: &Utf8Path, message: String) -> RuneError {
    RuneError::invalid_field("keys", message).with_hint(format!(
        "fix {path} or remove it; run `rune keys` to see the action ids"
    ))
}

/// Lists every action with the keys it answers to.
#[must_use]
pub fn render(path: &Utf8Path, present: bool, keymap: &Keymap) -> String {
    let state = if present { "present" } else { "absent" };
    let mut text = format!("keys file: {path} ({state})\n");
    for info in ACTIONS {
        let _ = writeln!(
            text,
            "{:<20} {:<9} {}  {}",
            info.id,
            info.context.as_str(),
            keymap.keys(info.id).join(", "),
            info.summary
        );
    }
    text
}

/// The same listing as JSON rows.
#[must_use]
pub fn json(keymap: &Keymap) -> serde_json::Value {
    ACTIONS
        .iter()
        .map(|info| {
            serde_json::json!({
                "id": info.id,
                "context": info.context.as_str(),
                "keys": keymap.keys(info.id),
                "summary": info.summary,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(text: &str) -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("temp");
        let path = Utf8Path::from_path(dir.path())
            .expect("utf8")
            .join("keys.toml");
        std::fs::write(&path, text).expect("written");
        (dir, path)
    }

    #[test]
    fn a_missing_or_empty_file_keeps_the_defaults() {
        let (_dir, path) = write("");
        let keymap = load(&path).expect("valid").expect("present");
        assert_eq!(keymap.keys("queue"), ["tab"]);
        assert!(
            load(&path.with_file_name("absent.toml"))
                .expect("ok")
                .is_none()
        );
    }

    #[test]
    fn strings_and_lists_both_name_keys() {
        let (_dir, path) = write("queue = \"ctrl+q\"\nnewline = [\"shift+enter\", \"ctrl+j\"]\n");
        let keymap = load(&path).expect("valid").expect("present");
        assert_eq!(keymap.keys("queue"), ["ctrl+q"]);
        assert_eq!(keymap.keys("newline"), ["shift+enter", "ctrl+j"]);
    }

    #[test]
    fn a_bad_file_names_itself_in_the_hint() {
        for text in [
            "queue = 3",
            "queue = [\"tab\", 1]",
            "queue = ",
            "nothing = \"x\"",
        ] {
            let (_dir, path) = write(text);
            let err = load(&path).expect_err(text);
            let hint = err.detail().hint.clone().unwrap_or_default();
            assert!(hint.contains(path.as_str()), "{text}: {hint}");
            assert!(hint.contains("rune keys"), "{text}: {hint}");
        }
    }
}
