//! Bounded project definitions for synchronous subagent roles.

use std::fs::File;
use std::io::Read as _;

use camino::Utf8Path;
use rune_core::error::{ErrorCode, Result, RuneError};
use serde::Deserialize;

use crate::subagent::MAX_INSTRUCTIONS_BYTES;

const MAX_ROLE_BYTES: usize = 32 * 1024;
const MAX_ROLE_TOOLS: usize = 128;

/// Project-provided instructions and a tool ceiling, never additional authority.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SubagentRole {
    /// Instructions appended to the parent's instructions.
    pub instructions: String,
    /// Tools to intersect with the child's inherited tool set. Empty denies all.
    pub tools: Vec<String>,
}

impl SubagentRole {
    /// Loads `.rune/agents/<name>.json` inside the active workspace.
    ///
    /// Names cannot contain path components. Symbolic links and nonregular files
    /// are refused, including linked definition directories. Definitions cannot
    /// supply commands, roots, permission modes, or policy overrides.
    pub fn load(workspace: &Utf8Path, name: &str) -> Result<Self> {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(RuneError::invalid_field(
                "agent",
                "role names must be 1 to 64 ASCII letters, digits, hyphens, or underscores",
            ));
        }
        let root = std::fs::canonicalize(workspace)?;
        let mut path = root.clone();
        for (component, directory) in [
            (".rune", true),
            ("agents", true),
            (format!("{name}.json").as_str(), false),
        ] {
            path.push(component);
            let metadata = std::fs::symlink_metadata(&path).map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    RuneError::new(
                        ErrorCode::NotFound,
                        format!("subagent role `{name}` was not found in .rune/agents"),
                    )
                } else {
                    err.into()
                }
            })?;
            if metadata.is_symlink()
                || (directory && !metadata.is_dir())
                || (!directory && !metadata.is_file())
            {
                return Err(RuneError::new(
                    ErrorCode::UnsafePath,
                    "subagent role requires real directories and a regular definition file",
                ));
            }
        }
        if !std::fs::canonicalize(&path)?.starts_with(&root) {
            return Err(RuneError::new(
                ErrorCode::UnsafePath,
                "subagent role definition is outside the workspace",
            ));
        }
        let file = File::open(&path)?;
        if !file.metadata()?.is_file() {
            return Err(RuneError::new(
                ErrorCode::UnsafePath,
                "subagent role definition must be a regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.take((MAX_ROLE_BYTES as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_ROLE_BYTES {
            return Err(RuneError::too_large(
                "subagent role",
                bytes.len(),
                MAX_ROLE_BYTES,
            ));
        }
        let role: Self = serde_json::from_slice(&bytes).map_err(|err| {
            RuneError::new(
                ErrorCode::InvalidConfiguration,
                format!("invalid subagent role `{name}`: {err}"),
            )
        })?;
        if role.instructions.trim().is_empty() {
            return Err(RuneError::invalid_field(
                "instructions",
                "role instructions must not be empty",
            ));
        }
        if role.instructions.len() > MAX_INSTRUCTIONS_BYTES {
            return Err(RuneError::too_large(
                "role instructions",
                role.instructions.len(),
                MAX_INSTRUCTIONS_BYTES,
            ));
        }
        if role.tools.len() > MAX_ROLE_TOOLS {
            return Err(RuneError::too_large(
                "role tools",
                role.tools.len(),
                MAX_ROLE_TOOLS,
            ));
        }
        for (index, tool) in role.tools.iter().enumerate() {
            if tool.is_empty()
                || tool.len() > 128
                || !tool.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
                })
                || role.tools[..index].contains(tool)
            {
                return Err(RuneError::invalid_field(
                    "tools",
                    "role tools must be unique tool names of 1 to 128 ASCII bytes",
                ));
            }
        }
        Ok(role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let temp = tempfile::tempdir().expect("workspace");
        let root = camino::Utf8PathBuf::from_path_buf(temp.path().to_owned()).expect("UTF-8");
        std::fs::create_dir_all(root.join(".rune/agents")).expect("roles");
        (temp, root)
    }

    #[test]
    fn loads_instructions_and_an_explicit_tool_ceiling() {
        let (_temp, root) = workspace();
        let path = root.join(".rune/agents/reviewer.json");
        std::fs::write(
            &path,
            r#"{"instructions":"Review only.","tools":["read_file"]}"#,
        )
        .expect("role");
        let role = SubagentRole::load(&root, "reviewer").expect("load");
        assert_eq!(role.instructions, "Review only.");
        assert_eq!(role.tools, ["read_file"]);
        std::fs::write(&path, r#"{"instructions":"Think only.","tools":[]}"#).expect("role");
        assert!(
            SubagentRole::load(&root, "reviewer")
                .expect("empty ceiling")
                .tools
                .is_empty()
        );
    }

    #[test]
    fn rejects_paths_missing_roles_and_nonregular_definitions() {
        let (_temp, root) = workspace();
        for name in [
            "",
            "../escape",
            "/tmp/role",
            "a/b",
            "a\\b",
            ".",
            "..",
            "reviewer.json",
            "a b",
            "rôle",
            &"a".repeat(65),
        ] {
            assert_eq!(
                SubagentRole::load(&root, name).expect_err(name).code(),
                ErrorCode::InvalidField
            );
        }
        assert_eq!(
            SubagentRole::load(&root, "missing")
                .expect_err("missing")
                .code(),
            ErrorCode::NotFound
        );
        std::fs::create_dir(root.join(".rune/agents/reviewer.json")).expect("directory");
        assert_eq!(
            SubagentRole::load(&root, "reviewer")
                .expect_err("directory")
                .code(),
            ErrorCode::UnsafePath
        );
    }

    #[test]
    fn rejects_malformed_or_authority_bearing_definitions_and_bounds_input() {
        let (_temp, root) = workspace();
        let path = root.join(".rune/agents/reviewer.json");
        for document in [
            serde_json::json!({"instructions":"Review"}),
            serde_json::json!({"instructions":"", "tools":[]}),
            serde_json::json!({"instructions":"Review", "tools":"read_file"}),
            serde_json::json!({"instructions":"Review", "tools":["*"]}),
            serde_json::json!({"instructions":"Review", "tools":["read_file", "read_file"]}),
            serde_json::json!({"instructions":"Review", "tools":[], "permission_mode":"full-access"}),
            serde_json::json!({"instructions":"Review", "tools":[], "roots":["/"]}),
            serde_json::json!({"instructions":"Review", "tools":[], "command":"touch escaped"}),
        ] {
            std::fs::write(&path, document.to_string()).expect("role");
            assert!(SubagentRole::load(&root, "reviewer").is_err(), "{document}");
        }
        for bytes in [b"not JSON".to_vec(), vec![0xff], vec![b' '; MAX_ROLE_BYTES + 1], serde_json::json!({"instructions":"a".repeat(MAX_INSTRUCTIONS_BYTES + 1), "tools":[]}).to_string().into_bytes(), serde_json::json!({"instructions":"Review", "tools":vec!["read_file"; MAX_ROLE_TOOLS + 1]}).to_string().into_bytes()] {
            std::fs::write(&path, bytes).expect("role");
            assert!(SubagentRole::load(&root, "reviewer").is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_files_and_both_definition_directories() {
        use std::os::unix::fs::symlink;

        for component in [".rune", ".rune/agents", ".rune/agents/reviewer.json"] {
            let (_temp, root) = workspace();
            let outside = tempfile::tempdir().expect("outside");
            let link = root.join(component);
            if component == ".rune/agents/reviewer.json" {
                let target = outside.path().join("role.json");
                std::fs::write(&target, r#"{"instructions":"SECRET","tools":[]}"#)
                    .expect("outside role");
                symlink(target, &link).expect("file link");
            } else {
                std::fs::remove_dir_all(&link).expect("remove directory");
                symlink(outside.path(), &link).expect("directory link");
            }
            assert_eq!(
                SubagentRole::load(&root, "reviewer")
                    .expect_err("link")
                    .code(),
                ErrorCode::UnsafePath
            );
        }
    }
}
