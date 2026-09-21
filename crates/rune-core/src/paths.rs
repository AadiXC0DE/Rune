//! State, configuration, and data paths.
//!
//! Rune follows the XDG base directory specification rather than placing
//! everything in a single dot directory, so backup, sync, and audit tooling
//! behaves the way it expects. Two variables override the result:
//!
//! - `RUNE_HOME` sets the state root, which is where sessions, usage, logs, and
//!   credentials live.
//! - `RUNE_CONFIG` sets the configuration file path directly.
//!
//! Every created directory is mode 0700 and every created file is mode 0600.
//! A file that has been widened, hard linked, or symlinked is refused rather
//! than read, because these files can contain credentials and transcripts.

use std::fmt;

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, Result, RuneError};

/// Directory mode for every directory Rune creates.
pub const DIR_MODE: u32 = 0o700;

/// File mode for every file Rune creates.
pub const FILE_MODE: u32 = 0o600;

/// Names used inside the state root.
pub mod names {
    /// Configuration file in the config root.
    pub const CONFIG_FILE: &str = "config.toml";
    /// Session directory in the state root.
    pub const SESSIONS_DIR: &str = "sessions";
    /// Usage ledger in the state root.
    pub const USAGE_FILE: &str = "usage.jsonl";
    /// Prompt history in the state root.
    pub const HISTORY_FILE: &str = "history.jsonl";
    /// Credential file in the state root.
    pub const CREDENTIALS_FILE: &str = "credentials.json";
    /// Advisory lock for the credential file.
    pub const CREDENTIALS_LOCK: &str = "credentials.lock";
    /// Theme directory in the data root.
    pub const THEMES_DIR: &str = "themes";
    /// Managed skill directory in the data root.
    pub const SKILLS_DIR: &str = "skills";
    /// Log directory in the state root.
    pub const LOGS_DIR: &str = "logs";
    /// Trace file in the log directory.
    pub const TRACE_FILE: &str = "trace.log";
    /// Project instructions file name.
    pub const INSTRUCTIONS_FILE: &str = "AGENTS.md";
    /// User system prompt override in the config root.
    pub const SYSTEM_PROMPT_FILE: &str = "SYSTEM.md";
    /// Project configuration file name.
    pub const PROJECT_CONFIG_FILE: &str = ".rune.toml";
    /// Legacy directory used before the XDG move.
    pub const LEGACY_DIR: &str = ".rune";
}

/// Resolved filesystem layout for one process.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Paths {
    /// Directory holding the configuration file.
    pub config_root: Utf8PathBuf,
    /// Directory holding sessions, usage, logs, and credentials.
    pub state_root: Utf8PathBuf,
    /// Directory holding managed skills and themes.
    pub data_root: Utf8PathBuf,
}

impl Paths {
    /// Resolves paths from the process environment.
    #[must_use]
    pub fn from_process() -> Self {
        Self::resolve(
            std::env::var("HOME").ok().as_deref(),
            std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
            std::env::var("XDG_STATE_HOME").ok().as_deref(),
            std::env::var("XDG_DATA_HOME").ok().as_deref(),
            std::env::var("RUNE_HOME").ok().as_deref(),
        )
    }

    /// Resolves paths from explicit inputs.
    ///
    /// Kept separate from [`Paths::from_process`] so the resolution matrix can
    /// be tested without mutating the process environment.
    #[must_use]
    pub fn resolve(
        home: Option<&str>,
        xdg_config: Option<&str>,
        xdg_state: Option<&str>,
        xdg_data: Option<&str>,
        rune_home: Option<&str>,
    ) -> Self {
        let home = home.unwrap_or("");

        let config_root = match non_empty(xdg_config) {
            Some(value) => Utf8PathBuf::from(value).join("rune"),
            None => join_home(home, ".config/rune"),
        };

        // RUNE_HOME wins over the XDG variables, and both win over the default.
        let state_root = match non_empty(rune_home) {
            Some(value) => Utf8PathBuf::from(value),
            None => match non_empty(xdg_state) {
                Some(value) => Utf8PathBuf::from(value).join("rune"),
                None => join_home(home, ".local/state/rune"),
            },
        };

        let data_root = match non_empty(xdg_data) {
            Some(value) => Utf8PathBuf::from(value).join("rune"),
            None => join_home(home, ".local/share/rune"),
        };

        Self {
            config_root,
            state_root,
            data_root,
        }
    }

    /// Path of the configuration file, honoring `RUNE_CONFIG`.
    #[must_use]
    pub fn config_file(&self, override_path: Option<&str>) -> Utf8PathBuf {
        match non_empty(override_path) {
            Some(value) => Utf8PathBuf::from(value),
            None => self.config_root.join(names::CONFIG_FILE),
        }
    }

    /// Path of the optional user system prompt override.
    #[must_use]
    pub fn system_prompt_file(&self) -> Utf8PathBuf {
        self.config_root.join(names::SYSTEM_PROMPT_FILE)
    }

    /// Path of the sessions directory.
    #[must_use]
    pub fn sessions_dir(&self) -> Utf8PathBuf {
        self.state_root.join(names::SESSIONS_DIR)
    }

    /// Path of one session directory.
    #[must_use]
    pub fn session_dir(&self, id: &crate::id::SessionId) -> Utf8PathBuf {
        self.sessions_dir().join(id.as_str())
    }

    /// Path of the usage ledger.
    #[must_use]
    pub fn usage_file(&self) -> Utf8PathBuf {
        self.state_root.join(names::USAGE_FILE)
    }

    /// Path of the prompt history file.
    #[must_use]
    pub fn history_file(&self) -> Utf8PathBuf {
        self.state_root.join(names::HISTORY_FILE)
    }

    /// Path of the credential file.
    #[must_use]
    pub fn credentials_file(&self) -> Utf8PathBuf {
        self.state_root.join(names::CREDENTIALS_FILE)
    }

    /// Path of the credential lock file.
    #[must_use]
    pub fn credentials_lock(&self) -> Utf8PathBuf {
        self.state_root.join(names::CREDENTIALS_LOCK)
    }

    /// Path of the theme directory.
    #[must_use]
    pub fn themes_dir(&self) -> Utf8PathBuf {
        self.data_root.join(names::THEMES_DIR)
    }

    /// Path of the managed skill directory.
    #[must_use]
    pub fn skills_dir(&self) -> Utf8PathBuf {
        self.data_root.join(names::SKILLS_DIR)
    }

    /// Path of the log directory.
    #[must_use]
    pub fn logs_dir(&self) -> Utf8PathBuf {
        self.state_root.join(names::LOGS_DIR)
    }

    /// Path of the default trace file.
    #[must_use]
    pub fn trace_file(&self) -> Utf8PathBuf {
        self.logs_dir().join(names::TRACE_FILE)
    }

    /// Path of the legacy directory, used only to warn about a migration.
    #[must_use]
    pub fn legacy_dir(home: Option<&str>) -> Utf8PathBuf {
        join_home(home.unwrap_or(""), names::LEGACY_DIR)
    }

    /// Creates the state and data roots with restrictive permissions.
    ///
    /// Idempotent. Existing directories are verified rather than changed, so a
    /// widened directory is reported instead of silently trusted.
    pub fn ensure_roots(&self) -> Result<()> {
        for root in [&self.config_root, &self.state_root, &self.data_root] {
            create_dir_private(root)?;
        }
        Ok(())
    }

    /// Returns true when a legacy directory exists and the new layout is unused.
    #[must_use]
    pub fn legacy_migration_needed(home: Option<&str>) -> bool {
        let legacy = Self::legacy_dir(home);
        if !legacy.as_std_path().exists() {
            return false;
        }
        let resolved = Self::from_process();
        !resolved.state_root.as_std_path().exists()
    }
}

impl fmt::Display for Paths {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "config={} state={} data={}",
            self.config_root, self.state_root, self.data_root
        )
    }
}

/// Returns the value when it is present and not empty.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// Joins a relative path onto a home directory.
fn join_home(home: &str, relative: &str) -> Utf8PathBuf {
    if home.is_empty() {
        Utf8PathBuf::from(relative)
    } else {
        Utf8PathBuf::from(home).join(relative)
    }
}

/// Creates a directory with mode 0700, verifying any existing directory.
///
/// The verification uses metadata without following symlinks, so a symlinked
/// state directory is refused rather than followed.
///
/// Only the final component is verified. Parent directories are system-owned
/// paths such as `~/.local/share` that legitimately carry wider modes, so they
/// are created without a mode check.
pub fn create_dir_private(path: &Utf8Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(RuneError::new(
                    ErrorCode::UnsafePath,
                    format!("`{path}` is a symbolic link"),
                )
                .with_hint("state directories must be real directories"));
            }
            if !meta.is_dir() {
                return Err(RuneError::new(
                    ErrorCode::UnsafePath,
                    format!("`{path}` exists and is not a directory"),
                ));
            }
            verify_dir_mode(path, &meta)?;
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            set_mode(path, DIR_MODE)?;
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

/// Verifies that a directory has no group or other bits set.
///
/// Directory link counts are not checked: a directory's link count is at least
/// two by definition, so a link check belongs on files only.
#[cfg(unix)]
fn verify_dir_mode(path: &Utf8Path, meta: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let mode = meta.mode() & 0o777;
    if mode & !DIR_MODE != 0 {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` has mode {mode:o}, expected {DIR_MODE:o} or narrower"),
        )
        .with_hint(format!("run `chmod {DIR_MODE:o} {path}`")));
    }
    Ok(())
}

/// Verifies directory permissions on platforms without POSIX modes.
#[cfg(not(unix))]
fn verify_dir_mode(_path: &Utf8Path, _meta: &std::fs::Metadata) -> Result<()> {
    Ok(())
}

/// Writes a file with mode 0600, refusing to replace a symlink.
pub fn write_private(path: &Utf8Path, contents: &str) -> Result<()> {
    use std::io::Write;

    // Parent directories are created without a mode check: they may be
    // system-owned paths such as a temp directory, which legitimately carry a
    // wider mode. The file itself is what must be private.
    if let Some(parent) = path.parent() {
        if !parent.as_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(RuneError::new(
                ErrorCode::UnsafePath,
                format!("`{path}` is a symbolic link"),
            ));
        }
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    set_open_mode(&mut options, FILE_MODE);

    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);
    set_mode(path, FILE_MODE)?;
    Ok(())
}

/// Reads a file after verifying it is a regular, single-linked, private file.
pub fn read_private(path: &Utf8Path, max_bytes: u64) -> Result<Option<String>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    if meta.file_type().is_symlink() {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` is a symbolic link"),
        )
        .with_hint("remove the link and write a real file"));
    }
    if !meta.is_file() {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` exists and is not a regular file"),
        ));
    }
    verify_mode(path, &meta, FILE_MODE)?;

    if meta.len() > max_bytes {
        return Err(RuneError::too_large(
            path.as_str(),
            usize::try_from(meta.len()).unwrap_or(usize::MAX),
            usize::try_from(max_bytes).unwrap_or(usize::MAX),
        ));
    }

    Ok(Some(std::fs::read_to_string(path)?))
}

/// Verifies that a path's mode has no group or other bits set.
#[cfg(unix)]
fn verify_mode(path: &Utf8Path, meta: &std::fs::Metadata, expected: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let mode = meta.mode() & 0o777;
    if mode & !expected != 0 {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` has mode {mode:o}, expected {expected:o} or narrower"),
        )
        .with_hint(format!("run `chmod {expected:o} {path}`")));
    }
    if meta.nlink() != 1 {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` has {} hard links", meta.nlink()),
        )
        .with_hint("Rune refuses files with multiple links"));
    }
    Ok(())
}

/// Verifies permissions on platforms without POSIX modes.
#[cfg(not(unix))]
fn verify_mode(_path: &Utf8Path, _meta: &std::fs::Metadata, _expected: u32) -> Result<()> {
    Ok(())
}

/// Applies a mode to a path.
#[cfg(unix)]
fn set_mode(path: &Utf8Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

/// Applies a mode to a path.
#[cfg(not(unix))]
fn set_mode(_path: &Utf8Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Applies a creation mode to an open options builder.
#[cfg(unix)]
fn set_open_mode(options: &mut std::fs::OpenOptions, mode: u32) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(mode);
}

/// Applies a creation mode to an open options builder.
#[cfg(not(unix))]
fn set_open_mode(_options: &mut std::fs::OpenOptions, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_variables_are_used_when_present() {
        let paths = Paths::resolve(
            Some("/home/u"),
            Some("/cfg"),
            Some("/st"),
            Some("/data"),
            None,
        );
        assert_eq!(paths.config_root, "/cfg/rune");
        assert_eq!(paths.state_root, "/st/rune");
        assert_eq!(paths.data_root, "/data/rune");
    }

    #[test]
    fn defaults_are_used_when_xdg_is_absent() {
        let paths = Paths::resolve(Some("/home/u"), None, None, None, None);
        assert_eq!(paths.config_root, "/home/u/.config/rune");
        assert_eq!(paths.state_root, "/home/u/.local/state/rune");
        assert_eq!(paths.data_root, "/home/u/.local/share/rune");
    }

    #[test]
    fn rune_home_overrides_the_state_root_only() {
        let paths = Paths::resolve(
            Some("/home/u"),
            Some("/cfg"),
            Some("/st"),
            Some("/data"),
            Some("/custom"),
        );
        assert_eq!(paths.state_root, "/custom");
        assert_eq!(paths.config_root, "/cfg/rune");
        assert_eq!(paths.data_root, "/data/rune");
    }

    #[test]
    fn empty_variables_fall_through_to_defaults() {
        let paths = Paths::resolve(Some("/home/u"), Some(""), Some(""), Some(""), Some(""));
        assert_eq!(paths.config_root, "/home/u/.config/rune");
        assert_eq!(paths.state_root, "/home/u/.local/state/rune");
        assert_eq!(paths.data_root, "/home/u/.local/share/rune");
    }

    #[test]
    fn missing_home_still_produces_relative_defaults() {
        let paths = Paths::resolve(None, None, None, None, None);
        assert_eq!(paths.config_root, ".config/rune");
        assert_eq!(paths.state_root, ".local/state/rune");
    }

    #[test]
    fn config_override_wins() {
        let paths = Paths::resolve(Some("/home/u"), None, None, None, None);
        assert_eq!(paths.config_file(Some("/etc/rune.toml")), "/etc/rune.toml");
        assert_eq!(paths.config_file(None), "/home/u/.config/rune/config.toml");
    }

    #[test]
    fn derived_paths_are_consistent() {
        let paths = Paths::resolve(Some("/home/u"), None, None, None, None);
        assert_eq!(paths.sessions_dir(), "/home/u/.local/state/rune/sessions");
        assert_eq!(paths.usage_file(), "/home/u/.local/state/rune/usage.jsonl");
        assert_eq!(
            paths.credentials_file(),
            "/home/u/.local/state/rune/credentials.json"
        );
        assert_eq!(paths.themes_dir(), "/home/u/.local/share/rune/themes");
        assert_eq!(
            paths.trace_file(),
            "/home/u/.local/state/rune/logs/trace.log"
        );
    }

    #[test]
    fn session_dir_uses_the_identifier() {
        let paths = Paths::resolve(Some("/home/u"), None, None, None, None);
        let id: crate::id::SessionId = "AbCdEf123456".parse().expect("id");
        assert_eq!(
            paths.session_dir(&id),
            "/home/u/.local/state/rune/sessions/AbCdEf123456"
        );
    }

    #[test]
    fn create_dir_private_is_idempotent_and_restrictive() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let target = Utf8PathBuf::from_path_buf(dir.path().join("state")).expect("utf8");
        create_dir_private(&target).expect("create");
        create_dir_private(&target).expect("create again");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target)
                .expect("meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, DIR_MODE);
        }
    }

    #[cfg(unix)]
    #[test]
    fn widened_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().expect("tempdir");
        let target = Utf8PathBuf::from_path_buf(dir.path().join("state")).expect("utf8");
        std::fs::create_dir(&target).expect("create");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let err = create_dir_private(&target).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
        assert!(err.detail().hint.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_is_refused() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).expect("create");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let target = Utf8PathBuf::from_path_buf(link).expect("utf8");

        let err = create_dir_private(&target).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
    }

    #[test]
    fn write_private_round_trips() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let target = Utf8PathBuf::from_path_buf(dir.path().join("sub/file.json")).expect("utf8");
        write_private(&target, "{\"a\":1}").expect("write");
        let read = read_private(&target, 1024).expect("read").expect("present");
        assert_eq!(read, "{\"a\":1}");
    }

    #[test]
    fn read_private_returns_none_for_missing_file() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let target = Utf8PathBuf::from_path_buf(dir.path().join("absent")).expect("utf8");
        assert!(read_private(&target, 1024).expect("read").is_none());
    }

    #[test]
    fn read_private_rejects_oversized_file() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let target = Utf8PathBuf::from_path_buf(dir.path().join("big")).expect("utf8");
        write_private(&target, &"x".repeat(100)).expect("write");
        let err = read_private(&target, 10).expect_err("too large");
        assert_eq!(err.code(), ErrorCode::TooLarge);
    }

    #[cfg(unix)]
    #[test]
    fn read_private_rejects_widened_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().expect("tempdir");
        let target = Utf8PathBuf::from_path_buf(dir.path().join("creds")).expect("utf8");
        write_private(&target, "secret").expect("write");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        let err = read_private(&target, 1024).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
    }

    #[cfg(unix)]
    #[test]
    fn read_private_rejects_hard_linked_file() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let target = Utf8PathBuf::from_path_buf(dir.path().join("creds")).expect("utf8");
        write_private(&target, "secret").expect("write");
        std::fs::hard_link(&target, dir.path().join("copy")).expect("link");

        let err = read_private(&target, 1024).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
    }

    #[cfg(unix)]
    #[test]
    fn write_private_refuses_to_replace_a_symlink() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "original").expect("write");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&victim, &link).expect("symlink");
        let target = Utf8PathBuf::from_path_buf(link).expect("utf8");

        assert!(write_private(&target, "overwritten").is_err());
        assert_eq!(std::fs::read_to_string(&victim).expect("read"), "original");
    }
}
