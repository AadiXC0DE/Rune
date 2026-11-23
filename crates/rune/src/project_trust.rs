//! The user's approvals of project files that widen what runs.
//!
//! An approval names the SHA-256 digest of the exact bytes it covers, so any
//! change to the file asks again. Approvals live in user state, where a
//! repository cannot write them and a clone carries none.

use std::collections::BTreeMap;

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::config::{Diagnostic, Hook};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;
use rune_policy::trust::canonical_workspace;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The project hooks file, relative to the workspace.
pub const HOOKS_FILE: &str = ".rune/hooks.toml";
const APPROVALS_FILE: &str = "project-approvals.json";
const MAX_APPROVALS_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Record {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hooks: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rules: Option<String>,
}

/// Approved digests, keyed by canonical workspace.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Approvals {
    workspaces: BTreeMap<String, Record>,
}

fn approvals_file(paths: &Paths) -> Utf8PathBuf {
    paths.state_root.join(APPROVALS_FILE)
}

/// Reads the approvals. A missing file approves nothing; a damaged one is an
/// error rather than a silent loss of what the user approved.
pub fn load(paths: &Paths) -> Result<Approvals> {
    let path = approvals_file(paths);
    let Some(text) = rune_core::paths::read_private(&path, MAX_APPROVALS_BYTES)? else {
        return Ok(Approvals::default());
    };
    if text.trim().is_empty() {
        return Ok(Approvals::default());
    }
    serde_json::from_str(&text).map_err(|err| {
        RuneError::new(
            ErrorCode::CorruptRecord,
            format!("the project approvals could not be read: {err}"),
        )
        .with_hint(format!("move `{path}` aside to start from no approvals"))
    })
}

/// The SHA-256 of `bytes`, in lower-case hex.
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn key(workspace: &Utf8Path) -> String {
    canonical_workspace(workspace).into_string()
}

impl Approvals {
    /// Whether the hooks file with this digest is approved for the workspace.
    pub fn hooks_approved(&self, workspace: &Utf8Path, digest: &str) -> bool {
        self.workspaces
            .get(&key(workspace))
            .and_then(|record| record.hooks.as_deref())
            == Some(digest)
    }

    /// Approves the hooks file with this digest for the workspace.
    pub fn approve_hooks(&mut self, workspace: &Utf8Path, digest: &str) {
        self.workspaces.entry(key(workspace)).or_default().hooks = Some(digest.to_owned());
    }

    /// Whether the rules file with this digest is approved for the workspace.
    pub fn rules_approved(&self, workspace: &Utf8Path, digest: &str) -> bool {
        self.workspaces
            .get(&key(workspace))
            .and_then(|record| record.rules.as_deref())
            == Some(digest)
    }

    /// Approves the rules file with this digest for the workspace.
    pub fn approve_rules(&mut self, workspace: &Utf8Path, digest: &str) {
        self.workspaces.entry(key(workspace)).or_default().rules = Some(digest.to_owned());
    }

    /// Clears every approval of the workspace.
    pub fn reset(&mut self, workspace: &Utf8Path) {
        self.workspaces.remove(&key(workspace));
    }

    /// Writes the approvals, readable only by their owner.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        rune_core::paths::create_dir_private(&paths.state_root)?;
        rune_core::paths::write_private(&approvals_file(paths), &text)
    }
}

/// The project hooks file of a workspace, as read.
pub enum ProjectHooks {
    /// No file, or one that defines no hooks.
    None,
    /// A file that cannot be used; none of its hooks run.
    Malformed(Diagnostic),
    /// The hooks of a usable file and the digest of its exact bytes.
    Present { digest: String, hooks: Vec<Hook> },
}

impl ProjectHooks {
    /// Reads `<workspace>/.rune/hooks.toml`.
    pub fn read(workspace: &Utf8Path) -> Self {
        match rune_core::config::read_project_hooks(&workspace.join(HOOKS_FILE)) {
            Ok(Some((_, hooks))) if hooks.is_empty() => Self::None,
            Ok(Some((text, hooks))) => Self::Present {
                digest: digest(text.as_bytes()),
                hooks,
            },
            Ok(None) => Self::None,
            Err(diagnostic) => Self::Malformed(diagnostic),
        }
    }
}

/// Says why a hooks file was ignored, in one line.
pub fn ignored(diagnostic: &Diagnostic) -> String {
    format!("{HOOKS_FILE} was ignored: {}", diagnostic.message)
}

/// What a run takes from the project hooks file.
pub struct Loaded {
    /// Approved hooks, to run after the user's hooks of the same event.
    pub hooks: Vec<Hook>,
    /// Whether the file has hooks that are not approved and will not run.
    pub unapproved: bool,
    /// Why the file was ignored, when it was.
    pub problem: Option<String>,
}

/// Reads the project hooks of a workspace and keeps only approved ones.
pub fn session_hooks(paths: &Paths, workspace: &Utf8Path) -> Result<Loaded> {
    let mut loaded = Loaded {
        hooks: Vec::new(),
        unapproved: false,
        problem: None,
    };
    match ProjectHooks::read(workspace) {
        ProjectHooks::None => {}
        ProjectHooks::Malformed(diagnostic) => loaded.problem = Some(ignored(&diagnostic)),
        ProjectHooks::Present { digest, hooks } => {
            if load(paths)?.hooks_approved(workspace, &digest) {
                loaded.hooks = hooks;
            } else {
                loaded.unapproved = true;
            }
        }
    }
    Ok(loaded)
}

/// One hook as the user reviews it: `  {event} {name}: {argv}`.
pub fn hook_line(hook: &Hook) -> String {
    let argv: Vec<String> = hook.argv.iter().map(|argument| shown(argument)).collect();
    format!(
        "  {} {}: {}",
        hook.event.name(),
        visible(&hook.name),
        argv.join(" ")
    )
}

/// An argument, quoted when it holds a space or anything that would make the
/// line read as something other than the exact command.
fn shown(argument: &str) -> String {
    if argument.is_empty()
        || argument
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\\')
    {
        format!("{argument:?}")
    } else {
        argument.to_owned()
    }
}

/// Text with control characters escaped, so a name cannot redraw the line.
fn visible(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Paths, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let paths = Paths {
            config_root: root.join("config"),
            state_root: root.join("state"),
            data_root: root.join("data"),
        };
        let workspace = root.join("work");
        std::fs::create_dir_all(workspace.join(".rune")).expect("workspace");
        (dir, paths, workspace)
    }

    fn write_hooks(workspace: &Utf8Path, argument: &str) {
        std::fs::write(
            workspace.join(HOOKS_FILE),
            format!(
                "[[hooks]]\nevent = 'turn_end'\nname = 'lint'\nargv = ['/bin/sh', '{argument}']\n"
            ),
        )
        .expect("hooks file");
    }

    fn present(workspace: &Utf8Path) -> (String, Vec<Hook>) {
        match ProjectHooks::read(workspace) {
            ProjectHooks::Present { digest, hooks } => (digest, hooks),
            _ => panic!("expected a usable hooks file"),
        }
    }

    #[test]
    fn a_changed_file_needs_approval_again() {
        let (_dir, paths, workspace) = fixture();
        write_hooks(&workspace, "lint.sh");
        let (first, hooks) = present(&workspace);
        assert!(hooks.iter().all(|hook| hook.project));
        let mut approvals = load(&paths).expect("load");
        assert!(!approvals.hooks_approved(&workspace, &first));
        approvals.approve_hooks(&workspace, &first);
        approvals.save(&paths).expect("save");

        let loaded = session_hooks(&paths, &workspace).expect("hooks");
        assert_eq!(loaded.hooks.len(), 1);
        assert!(!loaded.unapproved);

        write_hooks(&workspace, "lint2.sh");
        let (second, _) = present(&workspace);
        assert_ne!(first, second);
        assert!(
            !load(&paths)
                .expect("load")
                .hooks_approved(&workspace, &second)
        );
        let loaded = session_hooks(&paths, &workspace).expect("hooks");
        assert!(loaded.hooks.is_empty());
        assert!(loaded.unapproved);
    }

    #[test]
    fn reset_clears_hook_approval() {
        let (_dir, paths, workspace) = fixture();
        write_hooks(&workspace, "lint.sh");
        let (digest, _) = present(&workspace);
        let mut approvals = load(&paths).expect("load");
        approvals.approve_hooks(&workspace, &digest);
        approvals.approve_rules(&workspace, "abc");
        approvals.save(&paths).expect("save");

        let saved = std::fs::read_to_string(approvals_file(&paths)).expect("saved");
        let value: serde_json::Value = serde_json::from_str(&saved).expect("JSON");
        let record = &value[canonical_workspace(&workspace).as_str()];
        assert_eq!(record["hooks"], digest.as_str());
        assert_eq!(record["rules"], "abc");

        let mut approvals = load(&paths).expect("load");
        assert!(approvals.hooks_approved(&workspace, &digest));
        assert!(approvals.rules_approved(&workspace, "abc"));
        approvals.reset(&workspace);
        approvals.save(&paths).expect("save");
        let approvals = load(&paths).expect("load");
        assert!(!approvals.hooks_approved(&workspace, &digest));
        assert!(!approvals.rules_approved(&workspace, "abc"));
    }

    #[test]
    fn a_malformed_hooks_file_runs_nothing() {
        let (_dir, paths, workspace) = fixture();
        std::fs::write(workspace.join(HOOKS_FILE), "extra = 1\n").expect("hooks file");
        let loaded = session_hooks(&paths, &workspace).expect("hooks");
        assert!(loaded.hooks.is_empty());
        assert!(!loaded.unapproved);
        let problem = loaded.problem.expect("problem");
        assert!(
            problem.starts_with(".rune/hooks.toml was ignored: "),
            "{problem}"
        );
    }

    #[test]
    fn hook_lines_quote_arguments_with_spaces() {
        let (_dir, _paths, workspace) = fixture();
        std::fs::write(
            workspace.join(HOOKS_FILE),
            "[[hooks]]\nevent = 'pre_tool'\nname = 'guard'\nargv = ['/bin/sh', '-c', 'echo hi']\n",
        )
        .expect("hooks file");
        let (_, hooks) = present(&workspace);
        assert_eq!(
            hook_line(&hooks[0]),
            "  pre_tool guard: /bin/sh -c \"echo hi\""
        );
    }
}
