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
    /// Workspace trust records in the state root.
    pub const TRUST_FILE: &str = "trust.json";
    /// Advisory lock for the credential file.
    pub const CREDENTIALS_LOCK: &str = "credentials.lock";
    /// Theme directory in the data root.
    pub const THEMES_DIR: &str = "themes";
    /// Managed skill directory in the data root.
    pub const SKILLS_DIR: &str = "skills";
    /// Log directory in the state root.
    pub const LOGS_DIR: &str = "logs";
    /// Cached models.dev catalog in the state root.
    ///
    /// A cache rather than a fixture, because the capacity of a model is a fact
    /// about the model and changes when a new one is released. Holding it on
    /// disk means a session started without a network still knows the window of
    /// the model it is talking to.
    pub const MODELS_CACHE_FILE: &str = "models-dev.json";
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
    ///
    /// A root that no variable places is built from [`home_directory`], and is
    /// left relative when there is none. A relative root resolves against the
    /// working directory, which is usually a repository, so a process that reads
    /// or writes through these paths confirms them with
    /// [`Paths::require_absolute`] first.
    #[must_use]
    pub fn from_process() -> Self {
        let home = home_directory();
        Self::resolve(
            home.as_deref().map(Utf8Path::as_str),
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

    /// Returns the paths when every root is absolute.
    ///
    /// A relative root would place the user configuration and the credential
    /// file inside whatever directory the process runs from, so a cloned
    /// repository could supply the user layer and receive the credentials.
    pub fn require_absolute(self) -> Result<Self> {
        for (name, root) in [
            ("configuration", &self.config_root),
            ("state", &self.state_root),
            ("data", &self.data_root),
        ] {
            if !root.is_absolute() {
                return Err(RuneError::new(
                    ErrorCode::UnsafePath,
                    format!("the {name} directory `{root}` is not an absolute path"),
                )
                .with_hint(
                    "set HOME (USERPROFILE on Windows) to the home directory, \
                     or set the XDG base directory variables to absolute paths",
                ));
            }
        }
        Ok(self)
    }

    /// Path of the configuration file, honoring `RUNE_CONFIG`.
    #[must_use]
    pub fn config_file(&self, override_path: Option<&str>) -> Utf8PathBuf {
        match non_empty(override_path) {
            Some(value) => Utf8PathBuf::from(value),
            None => self.config_root.join(names::CONFIG_FILE),
        }
    }

    /// Path of the workspace trust records.
    #[must_use]
    pub fn trust_file(&self) -> Utf8PathBuf {
        self.state_root.join(names::TRUST_FILE)
    }

    /// Path of the optional user system prompt override.
    #[must_use]
    pub fn system_prompt_file(&self) -> Utf8PathBuf {
        self.config_root.join(names::SYSTEM_PROMPT_FILE)
    }

    /// Path of the directory skills are installed into.
    ///
    /// This is the directory discovery scans, so a skill written here is visible
    /// to the catalog without further registration.
    #[must_use]
    pub fn managed_skills_dir(&self) -> Utf8PathBuf {
        self.config_root.join(names::SKILLS_DIR)
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

    /// Path of the cached model catalog.
    #[must_use]
    pub fn models_cache_file(&self) -> Utf8PathBuf {
        self.state_root.join(names::MODELS_CACHE_FILE)
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

/// Returns the current user's home directory, when it is an absolute path.
///
/// The variable differs by platform: `HOME` is normally unset in a Windows
/// shell, which sets `USERPROFILE`, or `HOMEDRIVE` and `HOMEPATH`, instead.
/// A value that is not absolute is passed over, because a path built on it
/// would resolve against the working directory.
#[must_use]
pub fn home_directory() -> Option<Utf8PathBuf> {
    home_from(|name| std::env::var(name).ok())
}

/// Resolves the home directory from an arbitrary lookup.
fn home_from(lookup: impl Fn(&str) -> Option<String>) -> Option<Utf8PathBuf> {
    let read = |name: &str| lookup(name).filter(|value| !value.is_empty());
    let split = || Some(format!("{}{}", read("HOMEDRIVE")?, read("HOMEPATH")?));
    [read("HOME"), read("USERPROFILE"), split()]
        .into_iter()
        .flatten()
        .map(Utf8PathBuf::from)
        .find(|path| path.is_absolute())
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
    if let Some(parent) = path.parent().filter(|parent| !parent.as_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
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
    if let Some(parent) = path.parent().filter(|parent| !parent.as_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }

    let existing = std::fs::symlink_metadata(path);
    if existing.is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(RuneError::new(
            ErrorCode::UnsafePath,
            format!("`{path}` is a symbolic link"),
        ));
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

/// Replaces a private file with a complete, synced file staged in the same
/// directory. An interrupted write leaves the previous file intact.
///
/// Staging files are created exclusively with mode 0600 and removed on errors.
/// A killed process may leave a private staging file, but never publishes it.
pub fn write_private_atomic(path: &Utf8Path, contents: &str) -> Result<()> {
    write_private_atomic_before_replace(path, contents, |_| Ok(()))
}

fn write_private_atomic_before_replace(
    path: &Utf8Path,
    contents: &str,
    before_replace: impl FnOnce(&Utf8Path) -> Result<()>,
) -> Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_str().is_empty())
        .unwrap_or_else(|| Utf8Path::new("."));
    std::fs::create_dir_all(parent)?;
    verify_replacement_target(path)?;

    let (staging, mut file) = loop {
        let mut random = [0; 16];
        getrandom::getrandom(&mut random).map_err(|err| std::io::Error::other(err.to_string()))?;
        let staging = parent.join(format!(
            ".rune-private-{:032x}.tmp",
            u128::from_le_bytes(random)
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        set_open_mode(&mut options, FILE_MODE);
        match options.open(&staging) {
            Ok(file) => break (PrivateStagingFile(staging), file),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
    };

    // The closure owns the handle, closing it before staging cleanup on every
    // error path, including on platforms that cannot unlink an open file.
    (|| {
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        before_replace(&staging.0)?;
        verify_replacement_target(path)?;
        std::fs::rename(&staging.0, path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })()
}

fn verify_replacement_target(path: &Utf8Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err(RuneError::new(
                    ErrorCode::UnsafePath,
                    format!("`{path}` is not a regular file"),
                ));
            }
            verify_mode(path, &meta, FILE_MODE)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Removes unpublished staging files on success or failure.
#[derive(Debug)]
struct PrivateStagingFile(Utf8PathBuf);

impl Drop for PrivateStagingFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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

    /// Returns a directory that is absolute on the platform running the test.
    fn absolute() -> String {
        let dir = std::env::temp_dir();
        assert!(dir.is_absolute(), "{}", dir.display());
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn a_root_built_without_a_home_is_refused() {
        // Relative roots resolve against the working directory, which would let
        // a repository commit the user configuration and receive the
        // credential file.
        let err = Paths::resolve(None, None, None, None, None)
            .require_absolute()
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::UnsafePath);
        assert!(err.message().contains(".config"), "{}", err.message());
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn roots_under_an_absolute_home_are_accepted() {
        let home = absolute();
        let paths = Paths::resolve(Some(&home), None, None, None, None)
            .require_absolute()
            .expect("accepted");
        assert!(paths.config_root.starts_with(&home));
    }

    #[test]
    fn roots_placed_by_variables_need_no_home() {
        let base = absolute();
        let paths = Paths::resolve(None, Some(&base), Some(&base), Some(&base), None);
        assert!(paths.require_absolute().is_ok());
    }

    #[test]
    fn the_home_directory_falls_back_to_the_windows_profile() {
        let profile = absolute();
        let found = home_from(|name| (name == "USERPROFILE").then(|| profile.clone()));
        assert_eq!(found.as_deref(), Some(Utf8Path::new(&profile)));
    }

    #[test]
    fn the_home_directory_joins_the_windows_drive_and_path() {
        let full = absolute();
        let found = home_from(|name| match name {
            "HOMEDRIVE" => Some(String::new()),
            "HOMEPATH" => Some(full.clone()),
            _ => None,
        });
        // An empty drive is not a drive, so nothing is joined.
        assert_eq!(found, None);

        let (drive, path) = full.split_at(1);
        let found = home_from(|name| match name {
            "HOMEDRIVE" => Some(drive.to_owned()),
            "HOMEPATH" => Some(path.to_owned()),
            _ => None,
        });
        assert_eq!(found.as_deref(), Some(Utf8Path::new(&full)));
    }

    #[test]
    fn a_relative_home_is_passed_over() {
        let profile = absolute();
        let found = home_from(|name| match name {
            "HOME" => Some("relative/home".to_owned()),
            "USERPROFILE" => Some(profile.clone()),
            _ => None,
        });
        assert_eq!(found.as_deref(), Some(Utf8Path::new(&profile)));
        assert_eq!(
            home_from(|name| (name == "HOME").then(|| "relative".to_owned())),
            None
        );
    }

    #[test]
    fn a_home_directory_is_found_whatever_the_platform_calls_it() {
        assert!(home_directory().is_some(), "no home directory was found");
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

    const OLD_CREDENTIALS: &str = r#"{"version":1,"entries":{"old":{"value":"old-secret"}}}"#;
    const NEW_CREDENTIALS: &str = r#"{"version":1,"entries":{"new":{"value":"new-secret"}}}"#;

    #[test]
    fn atomic_replacement_publishes_complete_private_json_and_cleans_staging() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let target = Utf8PathBuf::from_path_buf(dir.path().join("credentials.json")).expect("utf8");
        // Exercise both initial creation and replacement.
        for contents in [OLD_CREDENTIALS, NEW_CREDENTIALS] {
            write_private_atomic(&target, contents).expect("replace");
            assert_eq!(
                read_private(&target, 1024).expect("read").expect("present"),
                contents
            );
            serde_json::from_str::<serde_json::Value>(
                &std::fs::read_to_string(&target).expect("read"),
            )
            .expect("complete JSON");
            assert_eq!(std::fs::read_dir(dir.path()).expect("directory").count(), 1);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&target)
                        .expect("metadata")
                        .permissions()
                        .mode()
                        & 0o777,
                    FILE_MODE
                );
            }
        }
    }

    #[test]
    fn failure_before_atomic_replacement_preserves_authority_and_cleans_staging() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let target = Utf8PathBuf::from_path_buf(dir.path().join("credentials.json")).expect("utf8");
        write_private(&target, OLD_CREDENTIALS).expect("old file");
        let result = write_private_atomic_before_replace(&target, NEW_CREDENTIALS, |staging| {
            assert_eq!(
                read_private(staging, 1024)
                    .expect("private staging")
                    .expect("present"),
                NEW_CREDENTIALS
            );
            assert_eq!(
                std::fs::read_to_string(&target).expect("old file"),
                OLD_CREDENTIALS
            );
            Err(std::io::Error::other("injected failure before replacement").into())
        });
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(&target).expect("old file"),
            OLD_CREDENTIALS
        );
        assert_eq!(std::fs::read_dir(dir.path()).expect("directory").count(), 1);
    }

    #[test]
    fn killing_a_writer_before_atomic_replacement_preserves_complete_credentials() {
        use std::time::{Duration, Instant};

        for existing in [false, true] {
            let dir = tempfile::TempDir::new().expect("tempdir");
            let root = Utf8Path::from_path(dir.path()).expect("utf8");
            let target = root.join("credentials.json");
            if existing {
                write_private(&target, OLD_CREDENTIALS).expect("old file");
            }
            let mut child =
                std::process::Command::new(std::env::current_exe().expect("test executable"))
                    .args(["--exact", "paths::tests::atomic_credential_writer_child"])
                    .env("RUNE_ATOMIC_CREDENTIAL_TEST_ROOT", root)
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn writer");
            let deadline = Instant::now() + Duration::from_secs(10);
            let ready = root.join("ready");
            while !ready.exists() && Instant::now() < deadline {
                if child.try_wait().expect("writer status").is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let paused = ready.exists();
            child.kill().expect("kill writer");
            assert!(!child.wait().expect("reap writer").success());
            assert!(paused, "writer did not reach the replacement boundary");

            if existing {
                let body = read_private(&target, 1024)
                    .expect("read")
                    .expect("old file");
                assert_eq!(body, OLD_CREDENTIALS);
                serde_json::from_str::<serde_json::Value>(&body).expect("complete JSON");
            } else {
                assert!(!target.exists(), "initial write must remain unpublished");
            }
            let staging = std::fs::read_to_string(&ready).expect("staging path");
            assert_eq!(
                read_private(Utf8Path::new(&staging), 1024)
                    .expect("private staging")
                    .expect("present"),
                NEW_CREDENTIALS
            );

            // An orphan from the killed writer cannot interfere with retrying.
            write_private_atomic(&target, NEW_CREDENTIALS).expect("retry replacement");
            assert_eq!(
                read_private(&target, 1024)
                    .expect("read")
                    .expect("new file"),
                NEW_CREDENTIALS
            );
        }
    }

    #[test]
    fn atomic_credential_writer_child() {
        let Ok(root) = std::env::var("RUNE_ATOMIC_CREDENTIAL_TEST_ROOT") else {
            return;
        };
        let root = Utf8Path::new(&root);
        write_private_atomic_before_replace(
            &root.join("credentials.json"),
            NEW_CREDENTIALS,
            |staging| {
                std::fs::write(root.join("ready.tmp"), staging.as_str())?;
                std::fs::rename(root.join("ready.tmp"), root.join("ready"))?;
                loop {
                    std::thread::park();
                }
            },
        )
        .expect("write credentials");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_replacement_refuses_unsafe_targets_without_modifying_them() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::TempDir::new().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let target = root.join("credentials.json");
        write_private(&target, OLD_CREDENTIALS).expect("old file");
        let link = root.join("symlink");
        symlink(&target, &link).expect("symlink");
        assert_eq!(
            write_private_atomic(&link, NEW_CREDENTIALS)
                .expect_err("refuse symlink")
                .code(),
            ErrorCode::UnsafePath
        );
        std::fs::remove_file(&link).expect("remove symlink");

        std::fs::hard_link(&target, &link).expect("hard link");
        assert_eq!(
            write_private_atomic(&target, NEW_CREDENTIALS)
                .expect_err("refuse hard link")
                .code(),
            ErrorCode::UnsafePath
        );
        std::fs::remove_file(&link).expect("remove hard link");

        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(
            write_private_atomic(&target, NEW_CREDENTIALS)
                .expect_err("refuse widened file")
                .code(),
            ErrorCode::UnsafePath
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("unchanged authority"),
            OLD_CREDENTIALS
        );
        assert_eq!(std::fs::read_dir(root).expect("directory").count(), 1);
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
