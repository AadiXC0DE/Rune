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
    /// Name of each skill; `None` until first looked at.
    skills: Option<Vec<String>>,
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
                .upfront
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

    /// Records `path` as it is on disk now, so a change the model was already
    /// told about is not reported again.
    pub fn refresh(&mut self, path: &Utf8Path) {
        let wanted = path.canonicalize_utf8().ok();
        for file in &mut self.files {
            if file.path != path && file.path.canonicalize_utf8().ok() != wanted {
                continue;
            }
            let metadata = std::fs::metadata(&file.path).ok();
            file.digest = std::fs::read(&file.path).ok().map(|bytes| digest(&bytes));
            file.len = metadata.as_ref().map_or(0, std::fs::Metadata::len);
            file.modified = metadata.and_then(|metadata| metadata.modified().ok());
        }
    }

    /// Returns a line for each skill installed or removed since last seen.
    pub fn skill_changes(&mut self, workspace: &Utf8Path, config_root: &Utf8Path) -> Vec<String> {
        let home = rune_core::paths::home_directory();
        let found = rune_context::skills::discover(workspace, home.as_deref(), config_root)
            .unwrap_or_default();
        let now: Vec<String> = found.iter().map(|skill| skill.name.clone()).collect();
        let Some(before) = self.skills.replace(now.clone()) else {
            return Vec::new();
        };
        let mut lines: Vec<String> = found
            .iter()
            .filter(|skill| !before.contains(&skill.name))
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
                .map(|name| format!("Skill removed: {name}.")),
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

fn skills(workspace: &Utf8Path, home: Option<&Utf8Path>, config_root: &Utf8Path) -> Vec<String> {
    rune_context::skills::discover(workspace, home, config_root)
        .unwrap_or_default()
        .into_iter()
        .map(|skill| skill.name)
        .collect()
}

/// One watched instruction file as saved with a session's prefix.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileRecord {
    /// The file.
    pub path: Utf8PathBuf,
    /// The directory its rules apply to.
    pub scope: Utf8PathBuf,
    /// Its size when last seen.
    pub bytes: u64,
    /// Its SHA-256 when last seen, in hex; empty once seen deleted.
    pub sha256: String,
}

impl Watched {
    /// Returns the watched instruction files, for saving.
    #[must_use]
    pub fn file_records(&self) -> Vec<FileRecord> {
        self.files
            .iter()
            .map(|file| FileRecord {
                path: file.path.clone(),
                scope: file.scope.clone(),
                bytes: file.len,
                sha256: file.digest.map(hex).unwrap_or_default(),
            })
            .collect()
    }

    /// Returns the skill names last seen.
    #[must_use]
    pub fn skill_names(&self) -> Vec<String> {
        self.skills.clone().unwrap_or_default()
    }

    /// Returns the test command last seen.
    #[must_use]
    pub fn test_command(&self) -> Option<String> {
        self.test_command.clone()
    }

    /// Rebuilds what was watched from a saved prefix, so whatever changed on
    /// disk since is found as a change.
    #[must_use]
    pub fn seeded(files: &[FileRecord], skills: Vec<String>, test_command: Option<String>) -> Self {
        Self {
            files: files
                .iter()
                .map(|file| WatchedFile {
                    path: file.path.clone(),
                    scope: file.scope.clone(),
                    len: file.bytes,
                    modified: None,
                    digest: unhex(&file.sha256),
                })
                .collect(),
            skills: Some(skills),
            test_command,
            test_command_seen: true,
        }
    }
}

fn hex(digest: [u8; 32]) -> String {
    use std::fmt::Write as _;
    digest.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        let pair = text.get(index.saturating_mul(2)..index.saturating_mul(2).saturating_add(2))?;
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(digest)
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// The delivery key of a nested instruction file: its path as the rendered
/// `from` attribute shows it, relative to the workspace, so a resumed session
/// can find it in a rebuilt result.
fn rules_key(file: &rune_context::InstructionFile, workspace: &Utf8Path) -> String {
    let path = file
        .path
        .strip_prefix(workspace)
        .map_or_else(|_| file.path.to_string(), ToString::to_string);
    format!("rules:{path}")
}

/// Returns the nested instruction files the step's calls touched for the
/// first time, rendered for its last result, with their delivery keys.
///
/// A file applies to a call whose path lies in its directory or below. Each
/// file is sent once until the turn carrying it is summarised. The step's
/// total is bounded by `project_instructions_total_bytes`, narrowest files
/// first; a file left out is not marked delivered, so a later touch sends it.
#[must_use]
pub fn scoped_rules(
    history: &rune_agent::History,
    results: &[rune_agent::turn::CallResult],
    nested: &[rune_context::InstructionFile],
    workspace: &Utf8Path,
    limits: &rune_core::budget::BudgetSet,
) -> Option<rune_agent::turn::Appendix> {
    use rune_core::budget::LimitName;
    if nested.is_empty() {
        return None;
    }
    let mut chosen: Vec<&rune_context::InstructionFile> = Vec::new();
    for result in results.iter().filter(|result| result.executed) {
        for target in call_targets(&result.call.name, &result.call.arguments, workspace) {
            for file in rune_context::resolve_for_target(nested, &target) {
                let delivered = history.is_delivered(&rules_key(file, workspace));
                if !delivered && !chosen.iter().any(|known| known.path == file.path) {
                    chosen.push(file);
                }
            }
        }
    }
    let cap = limits.get_usize(LimitName::ProjectInstructionFileBytes);
    let mut room = limits.get_usize(LimitName::ProjectInstructionsTotalBytes);
    let rendered: Vec<String> = chosen
        .iter()
        .map(|file| rune_context::instructions::scoped_rules(file, workspace, cap))
        .collect();
    // Narrowest scope first, so the rules closest to the work are paid for first.
    let mut order: Vec<usize> = (0..chosen.len()).collect();
    order.sort_by_key(|index| std::cmp::Reverse(chosen[*index].scope.components().count()));
    let mut kept = vec![false; chosen.len()];
    for index in order {
        if rendered[index].len() <= room {
            room = room.saturating_sub(rendered[index].len());
            kept[index] = true;
        }
    }
    let mut text = Vec::new();
    let mut keys = Vec::new();
    for (index, file) in chosen.iter().enumerate().filter(|(index, _)| kept[*index]) {
        text.push(rendered[index].as_str());
        keys.push(rules_key(file, workspace));
    }
    (!text.is_empty()).then(|| rune_agent::turn::Appendix {
        text: text.join("\n"),
        keys,
    })
}

/// Returns the paths a call names, resolved against the workspace.
fn call_targets(name: &str, arguments: &str, workspace: &Utf8Path) -> Vec<Utf8PathBuf> {
    let Ok(arguments) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return Vec::new();
    };
    let field = |key: &str| {
        arguments
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let named: Vec<String> = match name {
        "read_file" | "edit_file" | "write_file" | "grep_files" | "glob_files" => {
            field("path").into_iter().collect()
        }
        "shell" => field("cwd").into_iter().collect(),
        "apply_patch" => field("input")
            .map(|input| rune_tools::apply_patch::paths(&input))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    named
        .into_iter()
        .map(|path| {
            let path = Utf8PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &serde_json::Value) -> rune_agent::turn::CallResult {
        rune_agent::turn::CallResult {
            call: rune_agent::turn::PreparedCall {
                id: "c".to_owned(),
                name: name.to_owned(),
                arguments: arguments.to_string(),
            },
            output: rune_tools::contract::ToolOutput::success("ok"),
            executed: true,
        }
    }

    #[test]
    fn a_refreshed_file_is_not_reported_again() {
        let root = tempfile::tempdir().expect("tempdir");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let workspace = base.join("work");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let path = workspace.join("AGENTS.md");
        std::fs::write(&path, "# Notes\n").expect("write");
        let mut watched = Watched::of(&workspace, &base.join("config"));
        let changed = |watched: &mut Watched| {
            watched
                .file_changes(&workspace)
                .into_iter()
                .filter(|(changed, _)| changed.ends_with("AGENTS.md"))
                .count()
        };

        std::fs::write(&path, "# Notes\n\n- use tabs\n").expect("write");
        watched.refresh(&path);
        assert_eq!(changed(&mut watched), 0);

        std::fs::write(&path, "# Notes\n\n- use tabs\n- small commits\n").expect("write");
        assert_eq!(changed(&mut watched), 1);
    }

    #[test]
    fn scoped_rules_are_delivered_once_on_first_touch() {
        let root = tempfile::tempdir().expect("tempdir");
        let base = Utf8PathBuf::from_path_buf(root.path().to_owned()).expect("utf8");
        let workspace = base.join("work");
        for (path, text) in [
            ("a/AGENTS.md", "Use tabs in a.\n"),
            ("a/b/c/AGENTS.md", "Deep rules.\n"),
            ("b/CLAUDE.md", "Use spaces in b.\n"),
        ] {
            let path = workspace.join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(path, text).expect("write");
        }
        let nested =
            rune_context::instructions::Options::new(&workspace, None, &base.join("config"))
                .discover()
                .expect("discover")
                .nested;
        let limits = rune_core::budget::BudgetSet::new();
        let history = rune_agent::History::new();
        let deliver = |results: &[rune_agent::turn::CallResult]| {
            scoped_rules(&history, results, &nested, &workspace, &limits)
        };

        // Two reads in one directory deliver its rules once.
        let both = [
            call("read_file", &serde_json::json!({"path": "a/x.txt"})),
            call("read_file", &serde_json::json!({"path": "a/y.txt"})),
        ];
        let appendix = deliver(&both).expect("rules for a");
        assert_eq!(
            appendix.text.matches("Use tabs in a.").count(),
            1,
            "{}",
            appendix.text
        );
        assert!(!appendix.text.contains("Use spaces in b."));
        assert_eq!(appendix.keys, vec!["rules:a/AGENTS.md".to_owned()]);

        // A deep path gets every file on the way, the widest first.
        let deep = deliver(&[call(
            "read_file",
            &serde_json::json!({"path": "a/b/c/x.rs"}),
        )])
        .expect("rules for a/b/c");
        let wide = deep.text.find("Use tabs in a.").expect("a");
        let narrow = deep.text.find("Deep rules.").expect("a/b/c");
        assert!(wide < narrow, "{}", deep.text);

        // A fallback file shows its real name.
        let other = deliver(&[call("read_file", &serde_json::json!({"path": "b/z.txt"}))])
            .expect("rules for b");
        assert!(
            other.text.contains("from=\"b/CLAUDE.md\""),
            "{}",
            other.text
        );

        // The root, a path outside the workspace and an unexecuted call deliver nothing.
        assert!(deliver(&[call("grep_files", &serde_json::json!({"pattern": "x"}))]).is_none());
        assert!(
            deliver(&[call(
                "grep_files",
                &serde_json::json!({"pattern": "x", "path": "."})
            )])
            .is_none()
        );
        assert!(
            deliver(&[call(
                "read_file",
                &serde_json::json!({"path": "/etc/hosts"})
            )])
            .is_none()
        );
        let mut refused = call("read_file", &serde_json::json!({"path": "a/x.txt"}));
        refused.executed = false;
        assert!(deliver(&[refused]).is_none());
    }
}
