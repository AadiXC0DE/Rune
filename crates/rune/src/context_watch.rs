//! What the instructions were built from, watched so a change reaches the
//! model once, as an update, instead of rewriting the cached prefix.

use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest as _, Sha256};

/// Instruction files at most this large are sent again in full after a change.
pub const RESENT_FILE_BYTES: u64 = 8_192;

/// One instruction file as it was when last seen.
#[derive(Debug)]
struct WatchedFile {
    path: Utf8PathBuf,
    scope: Utf8PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
    /// `None` once the file was seen deleted.
    digest: Option<[u8; 32]>,
}

/// The inputs of the instructions, as last seen.
#[derive(Debug, Default)]
pub struct Watched {
    files: Vec<WatchedFile>,
    /// Name and location of each skill; `None` until first looked at.
    skills: Option<Vec<(String, Utf8PathBuf)>>,
    /// The detected test command, once first looked at.
    test_command: Option<String>,
    /// Whether the test command was looked at.
    test_command_seen: bool,
}

impl Watched {
    /// Records the instruction files, skills and test command of `workspace` now.
    #[must_use]
    pub fn of(workspace: &Utf8Path, config_root: &Utf8Path) -> Self {
        let home = rune_core::paths::home_directory();
        let files =
            rune_context::instructions::Options::new(workspace, home.as_deref(), config_root)
                .discover()
                .unwrap_or_default()
                .into_iter()
                .map(|file| {
                    let metadata = std::fs::metadata(&file.path).ok();
                    WatchedFile {
                        digest: std::fs::read(&file.path).ok().map(|bytes| digest(&bytes)),
                        len: metadata.as_ref().map_or(0, std::fs::Metadata::len),
                        modified: metadata.and_then(|metadata| metadata.modified().ok()),
                        path: file.path,
                        scope: file.scope,
                    }
                })
                .collect();
        Self {
            files,
            skills: Some(skills(workspace, home.as_deref(), config_root)),
            test_command: rune_context::prompt::detect_test_command(workspace),
            test_command_seen: true,
        }
    }

    /// Returns a line for each instruction file that changed since last seen,
    /// with the file's path. A file is read only when its size or modification
    /// time moved.
    pub fn file_changes(&mut self, workspace: &Utf8Path) -> Vec<(Utf8PathBuf, String)> {
        let mut lines = Vec::new();
        for file in &mut self.files {
            let shown = file
                .path
                .strip_prefix(workspace)
                .map_or_else(|_| file.path.clone(), Utf8Path::to_path_buf);
            let metadata = match std::fs::metadata(&file.path) {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if file.digest.take().is_some() {
                        file.len = 0;
                        file.modified = None;
                        lines.push((
                            file.path.clone(),
                            format!("{shown} was deleted; its rules no longer apply."),
                        ));
                    }
                    continue;
                }
                Err(_) => continue,
            };
            let modified = metadata.modified().ok();
            if file.digest.is_some() && metadata.len() == file.len && modified == file.modified {
                continue;
            }
            let Ok(bytes) = std::fs::read(&file.path) else {
                continue;
            };
            file.len = metadata.len();
            file.modified = modified;
            let now = digest(&bytes);
            if file.digest == Some(now) {
                continue;
            }
            file.digest = Some(now);
            let line = if metadata.len() <= RESENT_FILE_BYTES {
                let rendered =
                    rune_context::instructions::render(&[&rune_context::InstructionFile {
                        path: file.path.clone(),
                        scope: file.scope.clone(),
                        content: String::from_utf8_lossy(&bytes).into_owned(),
                        declared_bytes: metadata.len(),
                    }]);
                format!(
                    "{shown} changed on disk. Its rules are now:\n{}",
                    rendered.trim_end()
                )
            } else {
                format!("{shown} changed on disk; read it before relying on its rules.")
            };
            lines.push((file.path.clone(), line));
        }
        lines
    }

    /// Returns a line for each skill installed or removed since last seen.
    pub fn skill_changes(&mut self, workspace: &Utf8Path, config_root: &Utf8Path) -> Vec<String> {
        let home = rune_core::paths::home_directory();
        let found = rune_context::skills::discover(workspace, home.as_deref(), config_root)
            .unwrap_or_default();
        let now: Vec<(String, Utf8PathBuf)> = found
            .iter()
            .map(|skill| (skill.name.clone(), skill.location.clone()))
            .collect();
        let Some(before) = self.skills.replace(now.clone()) else {
            return Vec::new();
        };
        let mut lines: Vec<String> = found
            .iter()
            .filter(|skill| !before.contains(&(skill.name.clone(), skill.location.clone())))
            .map(|skill| {
                format!(
                    "Skill installed: {}",
                    rune_context::catalog::catalog_line(skill)
                )
            })
            .collect();
        lines.extend(
            before
                .iter()
                .filter(|known| !now.contains(known))
                .map(|(name, _)| format!("Skill removed: {name}.")),
        );
        lines
    }

    /// Returns a line when the detected test command changed since last seen.
    pub fn test_command_change(&mut self, workspace: &Utf8Path) -> Option<String> {
        let now = rune_context::prompt::detect_test_command(workspace);
        let before = std::mem::replace(&mut self.test_command, now.clone());
        if !std::mem::replace(&mut self.test_command_seen, true) {
            return None;
        }
        (before != now).then(|| match now {
            Some(command) => format!("Tests now run with `{command}`."),
            None => "No test command is detected any more.".to_owned(),
        })
    }
}

fn skills(
    workspace: &Utf8Path,
    home: Option<&Utf8Path>,
    config_root: &Utf8Path,
) -> Vec<(String, Utf8PathBuf)> {
    rune_context::skills::discover(workspace, home, config_root)
        .unwrap_or_default()
        .into_iter()
        .map(|skill| (skill.name, skill.location))
        .collect()
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
