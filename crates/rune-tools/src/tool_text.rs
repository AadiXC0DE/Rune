//! Replacement tool descriptions, read from the file `RUNE_TOOL_TEXT` names.

use std::collections::BTreeMap;

use rune_core::error::{ErrorCode, Result, RuneError};
use serde::Deserialize;

/// The replacement text for one tool.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// The tool's description.
    #[serde(default)]
    pub description: Option<String>,
    /// Property descriptions by property name; an empty text removes one.
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}

/// Replacement text by tool name.
pub type ToolText = BTreeMap<String, Entry>;

/// The environment variable that names the file.
pub const VARIABLE: &str = "RUNE_TOOL_TEXT";

/// Largest file read.
const MAX_FILE_BYTES: usize = 256 * 1024;

/// Reads the file `RUNE_TOOL_TEXT` names; `Ok(None)` when the variable is unset or empty.
///
/// # Errors
///
/// Returns `not_found` when the file cannot be read, `too_large` past 256 KiB,
/// and `invalid_field` when it is not the expected JSON.
pub fn from_env() -> Result<Option<ToolText>> {
    match std::env::var(VARIABLE) {
        Ok(path) if !path.trim().is_empty() => read(path.trim()).map(Some),
        _ => Ok(None),
    }
}

/// Reads one tool text file.
///
/// # Errors
///
/// As [`from_env`].
pub fn read(path: &str) -> Result<ToolText> {
    let bytes = std::fs::read(path).map_err(|err| {
        RuneError::new(
            ErrorCode::NotFound,
            format!("{VARIABLE} names `{path}`, which cannot be read: {err}"),
        )
    })?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(RuneError::too_large(VARIABLE, bytes.len(), MAX_FILE_BYTES));
    }
    serde_json::from_slice(&bytes)
        .map_err(|err| RuneError::invalid_field(VARIABLE, format!("{path}: {err}")))
}
