//! The instructions and tools a session's requests open with, saved beside
//! the session so a resume with the same model sends the same bytes and the
//! provider's cached prefix still applies.

use camino::Utf8PathBuf;
use rune_core::error::Result;
use rune_core::id::SessionId;
use rune_core::paths::Paths;
use rune_core::tool::ToolSpec;
use serde::{Deserialize, Serialize};

use crate::context_watch::FileRecord;

/// The format written by this build.
const VERSION: u32 = 1;

/// Largest prefix file read back.
const MAX_PREFIX_BYTES: u64 = 8 * 1024 * 1024;

/// What a session's requests open with, and what it was built from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPrefix {
    /// Format version.
    pub version: u32,
    /// The provider the prefix was built for.
    pub provider: String,
    /// The model the prefix was built for.
    pub model: String,
    /// The base instructions, without the repository map.
    pub instructions: String,
    /// Leading bytes of `instructions` that are the same in every workspace.
    pub stable_len: usize,
    /// What the repository map added after the base, exactly as sent.
    pub map: String,
    /// The tools offered, in order, byte for byte.
    pub tools: Vec<ToolSpec>,
    /// The instruction files the instructions were built from.
    pub instruction_files: Vec<FileRecord>,
    /// The skills the catalog listed.
    pub skills: Vec<String>,
    /// The detected test command.
    pub test_command: Option<String>,
}

impl SavedPrefix {
    /// Returns a prefix in the current format.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: &str,
        model: &str,
        instructions: String,
        stable_len: usize,
        map: String,
        tools: Vec<ToolSpec>,
        instruction_files: Vec<FileRecord>,
        skills: Vec<String>,
        test_command: Option<String>,
    ) -> Self {
        Self {
            version: VERSION,
            provider: provider.to_owned(),
            model: model.to_owned(),
            instructions,
            stable_len,
            map,
            tools,
            instruction_files,
            skills,
            test_command,
        }
    }

    /// Returns true when a resumed session may open with this prefix: the same
    /// provider and model, and every saved tool still served.
    #[must_use]
    pub fn fits(&self, provider: &str, model: &str, served: impl Fn(&str) -> bool) -> bool {
        self.version == VERSION
            && self.provider == provider
            && self.model == model
            && self.tools.iter().all(|tool| served(&tool.name))
    }
}

/// Where a session keeps its prefix.
fn path(paths: &Paths, id: &SessionId) -> Utf8PathBuf {
    paths.session_dir(id).join("prefix.json")
}

/// Saves a session's prefix, replacing the earlier one.
pub fn save(paths: &Paths, id: &SessionId, prefix: &SavedPrefix) -> Result<()> {
    let text = serde_json::to_string(prefix).map_err(|err| {
        rune_core::error::RuneError::new(rune_core::error::ErrorCode::Internal, err.to_string())
    })?;
    rune_core::paths::write_private_atomic(&path(paths, id), &text)
}

/// Reads a session's prefix; `None` when it is missing, unreadable or from
/// another format, in which case the prefix is rebuilt.
#[must_use]
pub fn load(paths: &Paths, id: &SessionId) -> Option<SavedPrefix> {
    let text = rune_core::paths::read_private(&path(paths, id), MAX_PREFIX_BYTES).ok()??;
    serde_json::from_str::<SavedPrefix>(&text)
        .ok()
        .filter(|prefix| prefix.version == VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix() -> SavedPrefix {
        SavedPrefix::new(
            "anthropic",
            "claude-sonnet-5-5",
            "base rules".to_owned(),
            4,
            "\n\nmap".to_owned(),
            vec![ToolSpec {
                name: "read_file".to_owned(),
                description: "Read a file.".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            vec![FileRecord {
                path: "/w/AGENTS.md".into(),
                scope: "/w".into(),
                bytes: 12,
                sha256: "00".repeat(32),
            }],
            vec!["alpha".to_owned()],
            Some("cargo test".to_owned()),
        )
    }

    #[test]
    fn the_prefix_file_round_trips() {
        let root = tempfile::tempdir().expect("tempdir");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let paths = Paths {
            config_root: base.join("config"),
            state_root: base.join("state"),
            data_root: base.join("data"),
        };
        let id = SessionId::generate();
        rune_core::paths::create_dir_private(&paths.session_dir(&id)).expect("session dir");
        assert!(load(&paths, &id).is_none());
        save(&paths, &id, &prefix()).expect("save");
        assert_eq!(load(&paths, &id), Some(prefix()));
        // Another format version is rebuilt rather than trusted.
        let mut other = prefix();
        other.version = VERSION.saturating_add(1);
        save(&paths, &id, &other).expect("save");
        assert!(load(&paths, &id).is_none());
    }

    #[test]
    fn a_different_model_rebuilds_the_prefix() {
        let saved = prefix();
        let served = |_: &str| true;
        assert!(saved.fits("anthropic", "claude-sonnet-5-5", served));
        assert!(!saved.fits("anthropic", "claude-opus-5-5", served));
        assert!(!saved.fits("openai", "claude-sonnet-5-5", served));
    }

    #[test]
    fn a_missing_tool_rebuilds_the_prefix() {
        let saved = prefix();
        assert!(!saved.fits("anthropic", "claude-sonnet-5-5", |name| name != "read_file"));
    }
}
