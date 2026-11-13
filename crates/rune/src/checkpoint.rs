//! Workspace snapshots in a private git directory.
//!
//! The shadow directory lives under the state root and has its own index, so
//! the workspace's repository, index, refs and hooks are never read or written.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::{self, Paths};
use rune_exec::session::{Input, POLL_INTERVAL, Process};
use rune_session::event::{Boundary, RestoreDirection, SessionEvent};
use sha2::{Digest as _, Sha256};

/// Directory under the state root that holds one shadow per workspace.
const CHECKPOINTS_DIR: &str = "checkpoints";
/// File in a shadow naming the workspace it belongs to.
const WORKSPACE_FILE: &str = "rune-workspace";
/// Untracked files larger than this are left out of a snapshot.
const LARGE_FILE_BYTES: u64 = 2_097_152;
/// Deadline of one git command.
const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);
/// Most output kept from one git command.
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
/// Largest a workspace's checkpoint store may grow before old sessions are removed.
pub const STORE_CAP: u64 = 1_073_741_824;
/// Longest a restore waits for queued snapshots.
const RESTORE_WAIT: Duration = Duration::from_secs(60);
/// Deadline of the garbage collection that follows a removed session.
const GC_DEADLINE: Duration = Duration::from_secs(120);

/// A private git directory that snapshots one workspace.
pub struct Shadow {
    git_dir: Utf8PathBuf,
    workspace: Utf8PathBuf,
    index: Utf8PathBuf,
    exclude: Utf8PathBuf,
    /// Directory holding one folder per session, to tell which sessions still exist.
    sessions: Utf8PathBuf,
}

/// One file that differs between two trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

/// How a file differs between two trees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl FileChange {
    /// The same change seen from the other side: an addition becomes a deletion.
    fn reversed(&self) -> Self {
        Self {
            path: self.path.clone(),
            kind: match self.kind {
                ChangeKind::Added => ChangeKind::Deleted,
                ChangeKind::Modified => ChangeKind::Modified,
                ChangeKind::Deleted => ChangeKind::Added,
            },
            added: self.removed,
            removed: self.added,
        }
    }

    /// `{A|M|D} {path} +{added} -{removed}`; binary files and deletions have no counts.
    pub fn line(&self) -> String {
        let letter = match self.kind {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
        };
        match (self.kind, self.added, self.removed) {
            (ChangeKind::Deleted, _, _) | (_, None, _) | (_, _, None) => {
                format!("{letter} {}", self.path)
            }
            (_, Some(added), Some(removed)) => {
                format!("{letter} {} +{added} -{removed}", self.path)
            }
        }
    }
}

/// What one snapshot recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub tree: String,
    pub commit: String,
    pub parent: Option<String>,
    pub skipped: Vec<String>,
}

impl Shadow {
    /// Opens or creates the shadow for a workspace. None when git is missing or the
    /// workspace is not inside a git work tree.
    pub fn open(paths: &Paths, workspace: &Utf8Path) -> Result<Option<Self>> {
        let workspace = rune_policy::trust::canonical_workspace(workspace);
        if git_root(&workspace).is_none() || !git_available(&workspace) {
            return Ok(None);
        }
        let shadow = Self::at(paths, workspace);
        let marker = shadow.git_dir.join(WORKSPACE_FILE);
        match std::fs::read_to_string(&marker) {
            Ok(recorded) => {
                let recorded = recorded.trim_end_matches(['\n', '\r']);
                if recorded != shadow.workspace.as_str() {
                    return Err(RuneError::new(
                        ErrorCode::Internal,
                        format!("checkpoint store {} belongs to {recorded}", shadow.git_dir),
                    ));
                }
                shadow.make_private()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                paths.ensure_roots()?;
                paths::create_dir_private(&paths.state_root.join(CHECKPOINTS_DIR))?;
                shadow.create()?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(Some(shadow))
    }

    /// The shadow of a canonical workspace path, whether or not it exists yet.
    fn at(paths: &Paths, workspace: Utf8PathBuf) -> Self {
        let git_dir = shadow_dir(paths, &workspace);
        Self {
            index: git_dir.join("rune-index"),
            exclude: git_dir.join("info").join("rune-exclude"),
            sessions: paths.sessions_dir(),
            git_dir,
            workspace,
        }
    }

    /// Initialises a new shadow directory and its configuration.
    fn create(&self) -> Result<()> {
        paths::create_dir_private(&self.git_dir)?;
        // Shared as 0600, so git writes every file readable by the user alone,
        // and without the template, whose sample hooks are world-readable.
        run_git(
            &[
                "init".to_owned(),
                "--bare".to_owned(),
                "--quiet".to_owned(),
                format!("--shared={PRIVATE_SHARING}"),
                "--template=".to_owned(),
                self.git_dir.to_string(),
            ],
            &self.workspace,
            &environment(None),
            DEFAULT_DEADLINE,
        )?;
        let hooks = self.hooks_dir();
        paths::create_dir_private(&hooks)?;
        paths::create_dir_private(&self.git_dir.join("info"))?;
        let settings = [
            ("core.sharedRepository", PRIVATE_SHARING),
            ("core.bare", "false"),
            ("core.hooksPath", hooks.as_str()),
            ("core.fsmonitor", "false"),
            ("core.autocrlf", "false"),
            ("core.quotepath", "false"),
            ("core.symlinks", "true"),
            ("gc.auto", "0"),
            ("commit.gpgsign", "false"),
            ("maintenance.auto", "false"),
        ];
        for (key, value) in settings {
            self.git(&["config", key, value], DEFAULT_DEADLINE)?;
        }
        paths::write_private(
            &self.git_dir.join(WORKSPACE_FILE),
            &format!("{}\n", self.workspace),
        )
    }

    /// Writes a snapshot of the working tree and points `refs/rune/<session>` at it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn snapshot(&self, session: &str, message: &str) -> Result<Snapshot> {
        let (tree, skipped) = self.stage()?;
        self.commit(session, message, tree, skipped)
    }

    /// Writes the working tree into the shadow index and returns its tree and skipped files.
    fn stage(&self) -> Result<(String, Vec<String>)> {
        let base = self.base_excludes();
        self.write_excludes(&base, &[])?;
        let listed = self.snapshot_git(
            &["ls-files", "-z", "--others", "--exclude-standard"],
            DEFAULT_DEADLINE,
        )?;
        let mut skipped = Vec::new();
        for path in listed
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let path = String::from_utf8_lossy(path).into_owned();
            let large = std::fs::symlink_metadata(self.workspace.join(&path))
                .is_ok_and(|meta| meta.len() > LARGE_FILE_BYTES);
            if large {
                skipped.push(path);
            }
        }
        skipped.sort();
        if !skipped.is_empty() {
            self.write_excludes(&base, &skipped)?;
        }
        self.snapshot_git(&["add", "-A", "--", "."], DEFAULT_DEADLINE)?;
        let tree = trimmed(&self.snapshot_git(&["write-tree"], DEFAULT_DEADLINE)?);
        Ok((tree, skipped))
    }

    /// Commits a staged tree on top of the session ref and moves the ref to it.
    fn commit(
        &self,
        session: &str,
        message: &str,
        tree: String,
        skipped: Vec<String>,
    ) -> Result<Snapshot> {
        let reference = format!("refs/rune/{session}");
        let parent = self.parent(&reference)?;
        let mut arguments = vec!["commit-tree", tree.as_str()];
        if let Some(parent) = &parent {
            arguments.extend(["-p", parent.as_str()]);
        }
        arguments.extend(["-m", message]);
        let commit = trimmed(&self.snapshot_git(&arguments, DEFAULT_DEADLINE)?);
        self.snapshot_git(&["update-ref", &reference, &commit], DEFAULT_DEADLINE)?;
        Ok(Snapshot {
            tree,
            commit,
            parent,
            skipped,
        })
    }

    /// Files that differ from `from` to `to`; binary files have no line counts.
    pub fn diff(&self, from: &str, to: &str) -> Result<Vec<FileChange>> {
        let base = ["diff-tree", "-r", "-z", "--no-renames"];
        let mut status_args = base.to_vec();
        status_args.extend(["--name-status", from, to]);
        let status = self.git(&status_args, DEFAULT_DEADLINE)?;
        let mut numstat_args = base.to_vec();
        numstat_args.extend(["--numstat", from, to]);
        let numstat = self.git(&numstat_args, DEFAULT_DEADLINE)?;
        let mut counts: BTreeMap<String, (Option<u64>, Option<u64>)> = BTreeMap::new();
        for record in numstat
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
        {
            let text = String::from_utf8_lossy(record);
            let mut parts = text.splitn(3, '\t');
            if let (Some(added), Some(removed), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            {
                counts.insert(path.to_owned(), (added.parse().ok(), removed.parse().ok()));
            }
        }
        let mut fields = status
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty());
        let mut changes = Vec::new();
        while let (Some(code), Some(path)) = (fields.next(), fields.next()) {
            let kind = match code.first() {
                Some(b'A') => ChangeKind::Added,
                Some(b'D') => ChangeKind::Deleted,
                _ => ChangeKind::Modified,
            };
            let path = String::from_utf8_lossy(path).into_owned();
            let (added, removed) = counts.get(&path).copied().unwrap_or((None, None));
            changes.push(FileChange {
                path,
                kind,
                added,
                removed,
            });
        }
        Ok(changes)
    }

    /// Makes the working tree match `target`, touching only paths that differ from
    /// `current`. Returns the changes applied.
    ///
    /// Every path is checked before anything is written, so an unsafe entry
    /// leaves the workspace as it was.
    pub fn restore(&self, current: &str, target: &str) -> Result<Vec<FileChange>> {
        let changes = self.diff(current, target)?;
        for change in &changes {
            self.check_safe(&change.path)?;
        }
        // Deletions go first, so a directory that becomes a file (or the other
        // way round) is out of the way before the new entry is written.
        for change in changes
            .iter()
            .filter(|change| change.kind == ChangeKind::Deleted)
        {
            self.remove_path(&change.path)?;
        }
        for change in changes
            .iter()
            .filter(|change| change.kind != ChangeKind::Deleted)
        {
            self.write_path(target, &change.path)?;
        }
        Ok(changes)
    }

    /// The tree a commit or tree id names.
    pub fn tree_of(&self, object: &str) -> Result<String> {
        let spec = format!("{object}^{{tree}}");
        Ok(trimmed(&self.git(
            &["rev-parse", "--verify", &spec],
            DEFAULT_DEADLINE,
        )?))
    }

    /// Refuses a path that could write outside the workspace or into a repository.
    fn check_safe(&self, path: &str) -> Result<()> {
        let refused = || {
            RuneError::new(
                ErrorCode::UnsafePath,
                format!("checkpoint path {path} is not safe to restore"),
            )
        };
        let relative = Utf8Path::new(path);
        let components: Vec<camino::Utf8Component<'_>> = relative.components().collect();
        if components.is_empty() {
            return Err(refused());
        }
        let mut directory = self.workspace.clone();
        for (index, component) in components.iter().enumerate() {
            let camino::Utf8Component::Normal(name) = component else {
                return Err(refused());
            };
            if name.eq_ignore_ascii_case(".git") {
                return Err(refused());
            }
            if index.saturating_add(1) < components.len() {
                directory.push(name);
                let linked = std::fs::symlink_metadata(&directory)
                    .is_ok_and(|meta| meta.file_type().is_symlink());
                if linked {
                    return Err(refused());
                }
            }
        }
        Ok(())
    }

    /// Writes one path from `target` through a temporary file and a rename.
    fn write_path(&self, target: &str, path: &str) -> Result<()> {
        let listed = self.git(&["ls-tree", "-z", target, "--", path], DEFAULT_DEADLINE)?;
        let entry = listed.split(|byte| *byte == 0).next().unwrap_or_default();
        let entry = String::from_utf8_lossy(entry);
        let meta = entry.split('\t').next().unwrap_or_default();
        let mut fields = meta.split(' ');
        let (Some(mode), Some(_), Some(object)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(RuneError::new(
                ErrorCode::Internal,
                format!("checkpoint has no entry for {path}"),
            ));
        };
        if mode == "160000" {
            return Ok(());
        }
        let bytes = self.git(&["cat-file", "blob", object], DEFAULT_DEADLINE)?;
        let destination = self.workspace.join(path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let name = destination.file_name().unwrap_or("file");
        let temporary = destination.with_file_name(format!(".{name}.rune-restore"));
        let _ = std::fs::remove_file(&temporary);
        if mode == "120000" {
            #[cfg(unix)]
            std::os::unix::fs::symlink(String::from_utf8_lossy(&bytes).as_ref(), &temporary)?;
            #[cfg(not(unix))]
            std::fs::write(&temporary, &bytes)?;
        } else {
            std::fs::write(&temporary, &bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let bits = if mode == "100755" { 0o755 } else { 0o644 };
                std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(bits))?;
            }
        }
        std::fs::rename(&temporary, &destination)?;
        Ok(())
    }

    /// Removes one path, then the directories it leaves empty, up to the workspace.
    fn remove_path(&self, path: &str) -> Result<()> {
        let destination = self.workspace.join(path);
        if std::fs::symlink_metadata(&destination).is_ok_and(|meta| meta.is_dir()) {
            return Ok(());
        }
        match std::fs::remove_file(&destination) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut directory = destination.parent();
        while let Some(current) = directory {
            if current == self.workspace || !current.starts_with(&self.workspace) {
                break;
            }
            if std::fs::remove_dir(current).is_err() {
                break;
            }
            directory = current.parent();
        }
        Ok(())
    }

    /// Size in bytes of the shadow directory.
    pub fn size(&self) -> u64 {
        directory_size(self.git_dir.as_std_path())
    }

    /// Removes the checkpoints of other sessions until the store is no larger than `cap`.
    ///
    /// Sessions whose folder is gone go first, then the oldest by their latest
    /// checkpoint. The ref of `current` is never removed.
    pub fn enforce_cap(&self, current: &str, cap: u64) -> Result<()> {
        if self.size() <= cap {
            return Ok(());
        }
        let listed = self.git(
            &[
                "for-each-ref",
                "--sort=committerdate",
                "--format=%(refname)",
                "refs/rune/",
            ],
            DEFAULT_DEADLINE,
        )?;
        let own = format!("refs/rune/{current}");
        let mut candidates: Vec<(bool, String)> = String::from_utf8_lossy(&listed)
            .lines()
            .map(str::trim)
            .filter(|reference| !reference.is_empty() && *reference != own)
            .map(|reference| {
                let session = reference.trim_start_matches("refs/rune/");
                (self.sessions.join(session).is_dir(), reference.to_owned())
            })
            .collect();
        // A stable sort keeps the age order within each group.
        candidates.sort_by_key(|(exists, _)| *exists);
        for (_, reference) in candidates {
            self.git(&["update-ref", "-d", &reference], DEFAULT_DEADLINE)?;
            self.git(
                &["reflog", "expire", "--expire=now", "--all"],
                DEFAULT_DEADLINE,
            )?;
            self.git(&["gc", "--prune=now", "--quiet"], GC_DEADLINE)?;
            if self.size() <= cap {
                break;
            }
        }
        Ok(())
    }

    /// The commit a session ref names, or None when the ref does not exist.
    fn parent(&self, reference: &str) -> Result<Option<String>> {
        match self.snapshot_git(
            &["rev-parse", "--verify", "--quiet", reference],
            DEFAULT_DEADLINE,
        ) {
            Ok(output) => Ok(Some(trimmed(&output))),
            Err(failure) if failure.exit == Some(1) => Ok(None),
            Err(failure) => Err(failure.error),
        }
    }

    /// The workspace repository's own exclude rules, when it has any.
    fn base_excludes(&self) -> String {
        git_root(&self.workspace)
            .and_then(|root| std::fs::read(root.join(".git").join("info").join("exclude")).ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }

    /// Writes the exclude file: the base rules, then one anchored line per path.
    fn write_excludes(&self, base: &str, skipped: &[String]) -> Result<()> {
        let mut contents = base.to_owned();
        if !contents.is_empty() && !contents.ends_with('\n') {
            contents.push('\n');
        }
        for path in skipped {
            contents.push('/');
            for character in path.chars() {
                if matches!(character, '\\' | '*' | '?' | '[' | '!' | '#') {
                    contents.push('\\');
                }
                contents.push(character);
            }
            contents.push('\n');
        }
        paths::write_private(&self.exclude, &contents)
    }

    /// Runs a git command of a snapshot, which also reads the exclude file.
    fn snapshot_git(&self, args: &[&str], timeout: Duration) -> GitResult {
        let excludes = format!("core.excludesFile={}", self.exclude);
        let mut arguments = vec!["-c", excludes.as_str()];
        arguments.extend_from_slice(args);
        self.git_status(&arguments, args.first().copied().unwrap_or("git"), timeout)
    }

    /// Runs git against the shadow and the workspace.
    fn git(&self, args: &[&str], timeout: Duration) -> Result<Vec<u8>> {
        self.git_status(args, args.first().copied().unwrap_or("git"), timeout)
            .map_err(|failure| failure.error)
    }

    /// Runs git against the shadow and the workspace, keeping the exit code of a failure.
    fn git_status(&self, args: &[&str], name: &str, timeout: Duration) -> GitResult {
        let hooks = format!("core.hooksPath={}", self.hooks_dir());
        let mut argv = vec![
            format!("--git-dir={}", self.git_dir),
            format!("--work-tree={}", self.workspace),
            "-c".to_owned(),
            hooks,
            "-c".to_owned(),
            "core.fsmonitor=false".to_owned(),
            "-c".to_owned(),
            "gc.auto=0".to_owned(),
            "-c".to_owned(),
            format!("core.sharedRepository={PRIVATE_SHARING}"),
        ];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        run_git_named(
            &argv,
            name,
            &self.workspace,
            &environment(Some(&self.index)),
            timeout,
        )
    }

    fn hooks_dir(&self) -> Utf8PathBuf {
        self.git_dir.join("no-hooks")
    }

    /// Takes group and other access off a store made before stores were
    /// private, once: a store that records the private sharing is left alone.
    fn make_private(&self) -> Result<()> {
        // The store's own file, not the override every git call carries.
        let shared = run_git(
            &[
                "config".to_owned(),
                "--file".to_owned(),
                self.git_dir.join("config").to_string(),
                "--get".to_owned(),
                "core.sharedRepository".to_owned(),
            ],
            &self.workspace,
            &environment(None),
            DEFAULT_DEADLINE,
        )
        .unwrap_or_default();
        if String::from_utf8_lossy(&shared).trim() == PRIVATE_SHARING {
            return Ok(());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut stack = vec![self.git_dir.clone()];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir)? {
                    let path = entry?.path();
                    let metadata = std::fs::symlink_metadata(&path)?;
                    if metadata.file_type().is_symlink() {
                        continue;
                    }
                    let mode = metadata.permissions().mode();
                    if mode & 0o077 != 0 {
                        std::fs::set_permissions(
                            &path,
                            std::fs::Permissions::from_mode(mode & 0o700),
                        )?;
                    }
                    if metadata.is_dir()
                        && let Ok(path) = Utf8PathBuf::from_path_buf(path)
                    {
                        stack.push(path);
                    }
                }
            }
        }
        self.git(
            &["config", "core.sharedRepository", PRIVATE_SHARING],
            DEFAULT_DEADLINE,
        )?;
        Ok(())
    }
}

/// How the checkpoint store is shared: with the user alone.
const PRIVATE_SHARING: &str = "0600";

/// One checkpoint as the session log records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointRecord {
    pub turn: u64,
    pub boundary: Boundary,
    pub snapshot: Snapshot,
    pub complete: bool,
}

impl CheckpointRecord {
    fn event(&self) -> SessionEvent {
        SessionEvent::WorkspaceCheckpoint {
            turn: self.turn,
            boundary: self.boundary,
            tree: self.snapshot.tree.clone(),
            commit: self.snapshot.commit.clone(),
            parent: self.snapshot.parent.clone(),
            skipped: self.snapshot.skipped.clone(),
            complete: self.complete,
        }
    }
}

/// Shows a line in the transcript of the session that owns the checkpoints.
pub type Notify = Arc<dyn Fn(String) + Send + Sync>;

/// Appends a checkpoint event to the session log.
pub type Record = Arc<dyn Fn(SessionEvent) -> Result<()> + Send + Sync>;

/// The tree a turn started from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnStart {
    pub tree: String,
    /// False when the snapshot was not ready before the turn changed files.
    pub complete: bool,
}

/// A session's checkpoints and restores, in the order they happened.
#[derive(Clone, Debug, Default)]
pub struct Timeline {
    /// Tree or commit the workspace last held: the newest checkpoint or restore target.
    current: Option<String>,
    /// Every checkpoint tree with its turn, oldest first.
    trees: Vec<(u64, String)>,
    starts: BTreeMap<u64, TurnStart>,
    /// Turns whose start and end trees differ, oldest first.
    changed: Vec<u64>,
    undone: BTreeSet<u64>,
    /// Turn and saved commit of each undo not yet redone, oldest first.
    redo: Vec<(u64, String)>,
}

impl Timeline {
    /// Rebuilds the timeline from a session's events.
    pub fn from_events(events: &[SessionEvent]) -> Self {
        let mut timeline = Self::default();
        for event in events {
            timeline.apply(event);
        }
        timeline
    }

    /// Takes one event into account; events of other kinds are ignored.
    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::WorkspaceCheckpoint {
                turn,
                boundary,
                tree,
                complete,
                ..
            } => {
                match boundary {
                    Boundary::Start => {
                        self.starts.insert(
                            *turn,
                            TurnStart {
                                tree: tree.clone(),
                                complete: *complete,
                            },
                        );
                    }
                    Boundary::End => {
                        // A start identical to the previous checkpoint was not
                        // recorded, so the turn started from the current tree.
                        if !self.starts.contains_key(turn)
                            && let Some(current) = &self.current
                        {
                            self.starts.insert(
                                *turn,
                                TurnStart {
                                    tree: current.clone(),
                                    complete: true,
                                },
                            );
                        }
                        let changed = self
                            .starts
                            .get(turn)
                            .is_some_and(|start| start.tree != *tree);
                        if changed {
                            if !self.changed.contains(turn) {
                                self.changed.push(*turn);
                            }
                            self.undone.remove(turn);
                            self.redo.clear();
                        }
                    }
                }
                self.trees.push((*turn, tree.clone()));
                self.current = Some(tree.clone());
            }
            SessionEvent::Restored {
                turn,
                direction,
                target,
                ..
            } => {
                match direction {
                    RestoreDirection::Undo => {
                        self.undone.insert(*turn);
                        if let SessionEvent::Restored { saved, .. } = event {
                            self.redo.push((*turn, saved.clone()));
                        }
                    }
                    RestoreDirection::Redo => {
                        self.redo.pop();
                        self.undone.remove(turn);
                    }
                    RestoreDirection::Turn => {}
                }
                self.current = Some(target.clone());
            }
            _ => {}
        }
    }

    /// Turn and start tree of the newest turn that changed files and is not undone.
    pub fn undo_target(&self) -> Option<(u64, String)> {
        self.changed
            .iter()
            .rev()
            .find(|turn| !self.undone.contains(turn))
            .and_then(|turn| {
                self.starts
                    .get(turn)
                    .map(|start| (*turn, start.tree.clone()))
            })
    }

    /// Turn and saved commit of the newest undo not yet redone.
    pub fn redo_target(&self) -> Option<(u64, String)> {
        self.redo.last().cloned()
    }

    /// The start checkpoint recorded for a turn.
    pub fn turn_start(&self, turn: u64) -> Option<&TurnStart> {
        self.starts.get(&turn)
    }

    /// The tree the workspace held before `turn`: its start checkpoint, or the
    /// newest checkpoint of an earlier turn when the turn recorded none.
    pub fn before_turn(&self, turn: u64) -> Option<TurnStart> {
        if let Some(start) = self.starts.get(&turn) {
            return Some(start.clone());
        }
        self.trees
            .iter()
            .rev()
            .find(|(recorded, _)| *recorded < turn)
            .map(|(_, tree)| TurnStart {
                tree: tree.clone(),
                complete: true,
            })
    }

    /// The tree or commit the workspace was last known to hold.
    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }
}

/// A restore asked for: the turn it refers to, what to restore to, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreRequest {
    pub turn: u64,
    pub target: String,
    pub direction: RestoreDirection,
}

/// What a restore did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreReport {
    /// The files restored, listed as the turn changed them: an undo of a turn
    /// that created a file lists it as added. A redo lists what it puts back.
    Done(Vec<FileChange>),
    /// Files changed since the session's last checkpoint; nothing was written.
    OutsideChanges(Vec<FileChange>),
}

/// The refusal for a turn whose start snapshot was not ready in time.
pub fn incomplete(turn: u64) -> RuneError {
    RuneError::new(
        ErrorCode::InvalidState,
        format!(
            "the checkpoint for turn {turn} is incomplete (it was not ready before the turn changed files); restore refused"
        ),
    )
}

/// Restores the workspace for one request, the procedure every entry point shares.
///
/// Takes a snapshot first, so the state being replaced can be brought back, and
/// refuses without `force` when files changed since the session's last
/// checkpoint. A dry run returns the changes and writes nothing.
pub fn restore_to(
    shadow: &Shadow,
    session: &str,
    timeline: &mut Timeline,
    request: &RestoreRequest,
    force: bool,
    dry_run: bool,
    record: &Record,
) -> Result<RestoreReport> {
    if request.direction != RestoreDirection::Redo
        && timeline
            .before_turn(request.turn)
            .is_some_and(|start| !start.complete)
    {
        return Err(incomplete(request.turn));
    }
    let (before, skipped) = shadow.stage()?;
    if !force && let Some(current) = timeline.current() {
        let known = shadow.tree_of(current)?;
        if known != before {
            let outside = shadow.diff(&known, &before)?;
            if !outside.is_empty() {
                return Ok(RestoreReport::OutsideChanges(outside));
            }
        }
    }
    // Going back lists the turn's own changes; going forward lists what returns.
    let as_turn = |changes: Vec<FileChange>| -> Vec<FileChange> {
        if request.direction == RestoreDirection::Redo {
            changes
        } else {
            changes.iter().map(FileChange::reversed).collect()
        }
    };
    if dry_run {
        return Ok(RestoreReport::Done(as_turn(
            shadow.diff(&before, &request.target)?,
        )));
    }
    let saved = shadow.commit(session, "before restore", before.clone(), skipped)?;
    let changes = as_turn(shadow.restore(&before, &request.target)?);
    let event = SessionEvent::Restored {
        turn: request.turn,
        direction: request.direction,
        target: request.target.clone(),
        saved: saved.commit,
    };
    record(event.clone())?;
    timeline.apply(&event);
    Ok(RestoreReport::Done(changes))
}

/// One snapshot the worker is asked to take.
struct Job {
    turn: u64,
    boundary: Boundary,
}

/// What the worker publishes to the threads that wait on it.
#[derive(Default)]
struct State {
    /// Turn of the latest start snapshot asked for.
    requested: u64,
    /// Turn of the latest start snapshot taken.
    started: u64,
    /// Turns whose start snapshot was not ready when a change began.
    late: BTreeSet<u64>,
    latest: Option<CheckpointRecord>,
    /// Jobs sent and jobs finished, so a restore can wait for the queue to drain.
    sent: u64,
    done: u64,
    timeline: Timeline,
}

struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Takes snapshots on one background thread, in the order asked.
pub struct Checkpoints {
    sender: Option<Sender<Job>>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    shadow: Arc<Mutex<Shadow>>,
    session: String,
    record: Record,
}

impl Checkpoints {
    pub fn start(shadow: Shadow, session: String, record: Record, notify: Notify) -> Self {
        Self::spawn(shadow, session, record, notify, Duration::ZERO)
    }

    /// Starts a worker that sleeps for `delay` before each snapshot.
    #[cfg(test)]
    pub fn start_with_delay(
        shadow: Shadow,
        session: String,
        record: Record,
        notify: Notify,
        delay: Duration,
    ) -> Self {
        Self::spawn(shadow, session, record, notify, delay)
    }

    fn spawn(
        shadow: Shadow,
        session: String,
        record: Record,
        notify: Notify,
        delay: Duration,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
        });
        let (sender, receiver) = mpsc::channel::<Job>();
        let worker_shared = Arc::clone(&shared);
        let shadow = Arc::new(Mutex::new(shadow));
        let worker_shadow = Arc::clone(&shadow);
        let worker_session = session.clone();
        let worker_record = Arc::clone(&record);
        let worker = std::thread::Builder::new()
            .name("rune-checkpoints".to_owned())
            .spawn(move || {
                let mut reported = false;
                let mut capped = false;
                for job in receiver {
                    if !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                    let shadow = worker_shadow.lock().unwrap_or_else(PoisonError::into_inner);
                    let mut outcome = take(
                        &shadow,
                        &worker_session,
                        &worker_record,
                        &worker_shared,
                        &job,
                    );
                    if outcome.is_ok() && job.boundary == Boundary::End && !capped {
                        capped = true;
                        outcome = shadow.enforce_cap(&worker_session, STORE_CAP);
                    }
                    drop(shadow);
                    {
                        let mut state = worker_shared.lock();
                        state.done = state.done.saturating_add(1);
                    }
                    worker_shared.ready.notify_all();
                    if let Err(error) = outcome
                        && !reported
                    {
                        reported = true;
                        notify(format!("checkpoint failed: {}", error.message()));
                    }
                }
            })
            .ok();
        if worker.is_none() {
            // Without a worker nothing would mark a start snapshot taken.
            shared.lock().started = u64::MAX;
        }
        Self {
            sender: worker.as_ref().map(|_| sender),
            shared,
            worker,
            shadow,
            session,
            record,
        }
    }

    /// Starts from the checkpoints and restores a resumed session already recorded.
    pub fn seed(&self, timeline: Timeline) {
        self.shared.lock().timeline = timeline;
    }

    /// The session's checkpoints and restores so far.
    pub fn timeline(&self) -> Timeline {
        self.shared.lock().timeline.clone()
    }

    /// Blocks until every queued snapshot is written, at most `wait`.
    /// Returns false on timeout.
    pub fn wait_idle(&self, wait: Duration) -> bool {
        let state = self.shared.lock();
        let (state, _) = self
            .shared
            .ready
            .wait_timeout_while(state, wait, |state| state.done < state.sent)
            .unwrap_or_else(PoisonError::into_inner);
        state.done >= state.sent
    }

    /// Waits for queued snapshots, then restores for the request `choose` picks.
    ///
    /// `choose` sees the timeline and returns None when there is nothing to do.
    pub fn restore(
        &self,
        choose: impl FnOnce(&Timeline) -> Result<Option<RestoreRequest>>,
        force: bool,
        dry_run: bool,
    ) -> Result<Option<(RestoreRequest, RestoreReport)>> {
        if !self.wait_idle(RESTORE_WAIT) {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                "a checkpoint is still being written; try again in a moment",
            ));
        }
        let shadow = self.shadow.lock().unwrap_or_else(PoisonError::into_inner);
        let mut timeline = self.timeline();
        let Some(request) = choose(&timeline)? else {
            return Ok(None);
        };
        let report = restore_to(
            &shadow,
            &self.session,
            &mut timeline,
            &request,
            force,
            dry_run,
            &self.record,
        )?;
        self.shared.lock().timeline = timeline;
        Ok(Some((request, report)))
    }

    /// Queues the start snapshot of `turn`.
    pub fn turn_started(&self, turn: u64) {
        self.shared.lock().requested = turn;
        self.send(turn, Boundary::Start);
    }

    /// The turn of the latest start snapshot asked for.
    pub fn turn(&self) -> u64 {
        self.shared.lock().requested
    }

    /// Blocks until the start snapshot of the current turn is written, at most `wait`.
    /// Returns false on timeout.
    pub fn wait_for_start(&self, wait: Duration) -> bool {
        let state = self.shared.lock();
        let (mut state, _) = self
            .shared
            .ready
            .wait_timeout_while(state, wait, |state| state.started < state.requested)
            .unwrap_or_else(PoisonError::into_inner);
        if state.started >= state.requested {
            return true;
        }
        let turn = state.requested;
        state.late.insert(turn);
        false
    }

    /// Queues the end snapshot of `turn`.
    pub fn turn_finished(&self, turn: u64) {
        self.send(turn, Boundary::End);
    }

    /// The latest checkpoint written by this session.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn latest(&self) -> Option<CheckpointRecord> {
        self.shared.lock().latest.clone()
    }

    fn send(&self, turn: u64, boundary: Boundary) {
        if let Some(sender) = &self.sender {
            let mut state = self.shared.lock();
            if sender.send(Job { turn, boundary }).is_ok() {
                state.sent = state.sent.saturating_add(1);
            }
        }
    }
}

impl Drop for Checkpoints {
    /// Waits for the queued snapshots, so the end of the last turn is kept.
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Takes one snapshot and records it, unless it repeats the previous one.
fn take(shadow: &Shadow, session: &str, record: &Record, shared: &Shared, job: &Job) -> Result<()> {
    let staged = shadow.stage();
    let (complete, previous) = {
        let mut state = shared.lock();
        let complete = match job.boundary {
            Boundary::Start => {
                state.started = state.started.max(job.turn);
                let late = state.late.remove(&job.turn);
                shared.ready.notify_all();
                !late
            }
            Boundary::End => true,
        };
        (
            complete,
            state
                .latest
                .as_ref()
                .map(|record| record.snapshot.tree.clone()),
        )
    };
    let (tree, skipped) = staged?;
    if complete && previous.as_deref() == Some(tree.as_str()) {
        return Ok(());
    }
    let side = match job.boundary {
        Boundary::Start => "start",
        Boundary::End => "end",
    };
    let message = format!("turn {} {side}", job.turn);
    let snapshot = shadow.commit(session, &message, tree, skipped)?;
    let checkpoint = CheckpointRecord {
        turn: job.turn,
        boundary: job.boundary,
        snapshot,
        complete,
    };
    let event = checkpoint.event();
    record(event.clone())?;
    let mut state = shared.lock();
    state.timeline.apply(&event);
    state.latest = Some(checkpoint);
    Ok(())
}

/// A failed git command: the error, and the exit code when it exited.
#[derive(Debug)]
struct GitFailure {
    error: RuneError,
    exit: Option<i32>,
}

type GitResult = std::result::Result<Vec<u8>, GitFailure>;

impl From<GitFailure> for RuneError {
    fn from(failure: GitFailure) -> Self {
        failure.error
    }
}

impl From<RuneError> for GitFailure {
    fn from(error: RuneError) -> Self {
        Self { error, exit: None }
    }
}

/// Removes a session's checkpoints from its workspace's store.
#[cfg_attr(not(test), allow(dead_code))]
pub fn remove_session(paths: &Paths, workspace: &Utf8Path, session: &str) -> Result<()> {
    let shadow = Shadow::at(paths, rune_policy::trust::canonical_workspace(workspace));
    if !shadow.git_dir.join(WORKSPACE_FILE).is_file() {
        return Ok(());
    }
    shadow.git(
        &["update-ref", "-d", &format!("refs/rune/{session}")],
        DEFAULT_DEADLINE,
    )?;
    Ok(())
}

/// Size in bytes of a workspace's checkpoint store, or None when it has none.
#[cfg_attr(not(test), allow(dead_code))]
pub fn store_size(paths: &Paths, workspace: &Utf8Path) -> Option<u64> {
    let shadow = Shadow::at(paths, rune_policy::trust::canonical_workspace(workspace));
    shadow
        .git_dir
        .join(WORKSPACE_FILE)
        .is_file()
        .then(|| shadow.size())
}

/// The shadow directory of a canonical workspace path.
fn shadow_dir(paths: &Paths, workspace: &Utf8Path) -> Utf8PathBuf {
    let digest = Sha256::digest(workspace.as_str().as_bytes());
    let mut key = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        let _ = write!(key, "{byte:02x}");
    }
    paths.state_root.join(CHECKPOINTS_DIR).join(key)
}

/// The nearest directory from the workspace upward that contains `.git`.
fn git_root(workspace: &Utf8Path) -> Option<&Utf8Path> {
    workspace.ancestors().find(|dir| dir.join(".git").exists())
}

/// True when `git --version` starts and exits successfully.
fn git_available(workspace: &Utf8Path) -> bool {
    run_git(
        &["--version".to_owned()],
        workspace,
        &environment(None),
        DEFAULT_DEADLINE,
    )
    .is_ok()
}

/// The whole environment a git command gets.
fn environment(index: Option<&Utf8Path>) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    let inherited: &[&str] = if cfg!(windows) {
        &["PATH", "SystemRoot", "TEMP"]
    } else {
        &["PATH"]
    };
    for name in inherited {
        if let Some(value) = std::env::var_os(name).and_then(|value| value.into_string().ok()) {
            environment.insert((*name).to_owned(), value);
        }
    }
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let fixed = [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", null),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("LC_ALL", "C"),
        ("GIT_AUTHOR_NAME", "rune"),
        ("GIT_AUTHOR_EMAIL", "rune@localhost"),
        ("GIT_COMMITTER_NAME", "rune"),
        ("GIT_COMMITTER_EMAIL", "rune@localhost"),
    ];
    for (name, value) in fixed {
        environment.insert(name.to_owned(), value.to_owned());
    }
    if let Some(index) = index {
        environment.insert("GIT_INDEX_FILE".to_owned(), index.to_string());
    }
    environment
}

/// Runs git with these arguments, naming the command by its first argument.
fn run_git(
    args: &[String],
    cwd: &Utf8Path,
    environment: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let name = args.first().map_or("git", String::as_str).to_owned();
    run_git_named(args, &name, cwd, environment, timeout).map_err(|failure| failure.error)
}

/// Runs git under a deadline and returns its standard output.
fn run_git_named(
    args: &[String],
    name: &str,
    cwd: &Utf8Path,
    environment: &BTreeMap<String, String>,
    timeout: Duration,
) -> GitResult {
    let mut argv = vec!["git".to_owned()];
    argv.extend_from_slice(args);
    let started = Instant::now();
    let process = Process::start_argv(
        &argv,
        Some(cwd.as_std_path()),
        MAX_OUTPUT_BYTES,
        environment,
        Input::Closed,
    )
    .map_err(RuneError::from)?;
    let exit = loop {
        if let Some(exit) = process.exit() {
            break exit;
        }
        if started.elapsed() >= timeout {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                format!("git {name} did not finish in {} s", timeout.as_secs()),
            )
            .into());
        }
        std::thread::sleep(POLL_INTERVAL);
    };
    process.drain_output(Duration::from_millis(250));
    if !exit.is_success() {
        let stderr = process.stderr().since(0).bytes;
        let stderr = String::from_utf8_lossy(&stderr);
        let line = stderr.lines().next().unwrap_or_default().trim();
        return Err(GitFailure {
            error: RuneError::new(ErrorCode::Internal, format!("git {name} failed: {line}")),
            exit: exit.status(),
        });
    }
    let stdout = process.stdout().since(0);
    if stdout.produced > MAX_OUTPUT_BYTES as u64 {
        return Err(RuneError::new(
            ErrorCode::TooLarge,
            format!("git {name} output exceeds {MAX_OUTPUT_BYTES} bytes"),
        )
        .into());
    }
    Ok(stdout.bytes)
}

fn trimmed(output: &[u8]) -> String {
    String::from_utf8_lossy(output).trim().to_owned()
}

/// Sums the sizes of the files under a directory, without following symlinks.
fn directory_size(directory: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    let mut total = 0_u64;
    for entry in entries.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.is_dir() {
            total = total.saturating_add(directory_size(&entry.path()));
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A temporary git repository and a state root outside it.
    struct Fixture {
        _root: tempfile::TempDir,
        workspace: Utf8PathBuf,
        paths: Paths,
    }

    fn git_present() -> bool {
        let present = std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !present {
            eprintln!("skipped: git is not installed");
        }
        present
    }

    /// Runs git in the real repository with a fixed identity.
    fn real_git(workspace: &Utf8Path, args: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(workspace)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().expect("tempdir");
        let base = Utf8PathBuf::from_path_buf(root.path().canonicalize().expect("canonical"))
            .expect("utf8");
        let workspace = base.join("work");
        std::fs::create_dir(&workspace).expect("workspace");
        real_git(&workspace, &["init", "-q"]);
        std::fs::write(workspace.join("a.txt"), "a\n").expect("file");
        real_git(&workspace, &["add", "-A"]);
        real_git(&workspace, &["commit", "-qm", "init"]);
        let paths = Paths {
            config_root: base.join("config"),
            state_root: base.join("state"),
            data_root: base.join("data"),
        };
        Fixture {
            _root: root,
            workspace,
            paths,
        }
    }

    fn open(fixture: &Fixture) -> Shadow {
        Shadow::open(&fixture.paths, &fixture.workspace)
            .expect("open")
            .expect("a git workspace has a shadow")
    }

    fn names(shadow: &Shadow, tree: &str) -> Vec<String> {
        let output = std::process::Command::new("git")
            .arg(format!("--git-dir={}", shadow.git_dir))
            .args(["ls-tree", "-r", "--name-only", tree])
            .output()
            .expect("ls-tree");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn a_command_made_file_is_captured() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let shadow = open(&fixture);
        let first = shadow.snapshot("s1", "turn 1 start").expect("snapshot");
        let status = std::process::Command::new("sh")
            .args(["-c", "echo x > made-by-command"])
            .current_dir(&fixture.workspace)
            .status()
            .expect("sh");
        assert!(status.success());
        let second = shadow.snapshot("s1", "turn 1 end").expect("snapshot");
        assert!(names(&shadow, &second.tree).contains(&"made-by-command".to_owned()));
        assert!(!names(&shadow, &first.tree).contains(&"made-by-command".to_owned()));
        assert_eq!(first.parent, None);
        assert_eq!(second.parent.as_deref(), Some(first.commit.as_str()));
        eprintln!("shadow size: {} bytes", shadow.size());
    }

    #[test]
    fn an_ignored_file_is_not_captured() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        std::fs::write(fixture.workspace.join(".gitignore"), "build/\n").expect("ignore");
        std::fs::create_dir(fixture.workspace.join("build")).expect("build");
        std::fs::write(fixture.workspace.join("build/out"), "o").expect("out");
        let shadow = open(&fixture);
        let snapshot = shadow.snapshot("s1", "turn 1 start").expect("snapshot");
        let listed = names(&shadow, &snapshot.tree);
        assert!(listed.contains(&".gitignore".to_owned()), "{listed:?}");
        assert!(
            !listed.iter().any(|name| name.starts_with("build/")),
            "{listed:?}"
        );
    }

    #[test]
    fn large_untracked_files_are_skipped_and_listed() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        std::fs::write(
            fixture.workspace.join("big.bin"),
            vec![7_u8; 3 * 1024 * 1024],
        )
        .expect("big");
        std::fs::write(fixture.workspace.join("small.bin"), vec![7_u8; 1024 * 1024])
            .expect("small");
        let shadow = open(&fixture);
        let snapshot = shadow.snapshot("s1", "turn 1 start").expect("snapshot");
        assert_eq!(snapshot.skipped, vec!["big.bin".to_owned()]);
        let listed = names(&shadow, &snapshot.tree);
        assert!(!listed.contains(&"big.bin".to_owned()), "{listed:?}");
        assert!(listed.contains(&"small.bin".to_owned()), "{listed:?}");
    }

    #[test]
    fn the_real_repository_is_untouched() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        std::fs::write(fixture.workspace.join("a.txt"), "changed\n").expect("edit");
        std::fs::write(fixture.workspace.join("new.txt"), "new\n").expect("new");
        let record = || {
            (
                real_git(&fixture.workspace, &["status", "--porcelain=v2"]),
                std::fs::read(fixture.workspace.join(".git/index")).expect("index"),
                real_git(&fixture.workspace, &["for-each-ref"]),
            )
        };
        let before = record();
        let shadow = open(&fixture);
        shadow.snapshot("s1", "turn 1 start").expect("snapshot");
        std::fs::write(fixture.workspace.join("a.txt"), "again\n").expect("edit");
        std::fs::write(fixture.workspace.join("new.txt"), "newer\n").expect("edit");
        let after_edit = record();
        shadow.snapshot("s1", "turn 1 end").expect("snapshot");
        let after = record();
        assert_eq!(after.1, before.1, "the index changed");
        assert_eq!(after.2, before.2, "the refs changed");
        assert_eq!(after.0, after_edit.0, "the status changed");
        assert_eq!(after_edit.1, before.1, "the index changed");
    }

    #[test]
    fn hooks_in_the_real_repository_never_run() {
        use std::os::unix::fs::PermissionsExt as _;

        if !git_present() {
            return;
        }
        let fixture = fixture();
        let marker = fixture.workspace.join("hook-ran");
        for hook in ["pre-commit", "post-index-change"] {
            let path = fixture.workspace.join(".git/hooks").join(hook);
            std::fs::write(&path, format!("#!/bin/sh\ntouch '{marker}'\n")).expect("hook");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
        }
        std::fs::write(fixture.workspace.join("b.txt"), "b\n").expect("file");
        let shadow = open(&fixture);
        shadow.snapshot("s1", "turn 1 start").expect("snapshot");
        assert!(!marker.exists(), "a hook of the real repository ran");
    }

    #[test]
    fn no_repository_means_no_shadow() {
        if !git_present() {
            return;
        }
        let root = tempfile::tempdir().expect("tempdir");
        let base = Utf8PathBuf::from_path_buf(root.path().canonicalize().expect("canonical"))
            .expect("utf8");
        let workspace = base.join("plain");
        std::fs::create_dir(&workspace).expect("workspace");
        let paths = Paths {
            config_root: base.join("config"),
            state_root: base.join("state"),
            data_root: base.join("data"),
        };
        assert!(Shadow::open(&paths, &workspace).expect("open").is_none());
        assert!(!paths.state_root.join(CHECKPOINTS_DIR).exists());
    }

    #[test]
    fn a_nested_repository_is_not_entered() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let nested = fixture.workspace.join("vendor/lib");
        std::fs::create_dir_all(&nested).expect("nested");
        real_git(&nested, &["init", "-q"]);
        std::fs::write(nested.join("inner.txt"), "inner\n").expect("file");
        real_git(&nested, &["add", "-A"]);
        real_git(&nested, &["commit", "-qm", "inner"]);
        let shadow = open(&fixture);
        let snapshot = shadow.snapshot("s1", "turn 1 start").expect("snapshot");
        let output = std::process::Command::new("git")
            .arg(format!("--git-dir={}", shadow.git_dir))
            .args(["ls-tree", "-r", &snapshot.tree])
            .output()
            .expect("ls-tree");
        let listing = String::from_utf8_lossy(&output.stdout);
        assert!(
            listing
                .lines()
                .any(|line| line.starts_with("160000 commit ") && line.ends_with("\tvendor/lib")),
            "{listing}"
        );
        assert!(!listing.contains("vendor/lib/"), "{listing}");
    }

    /// Bytes that do not compress, so each file grows the store by about its size.
    fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    /// Snapshots the workspace for `session` holding only its own large file.
    /// Waits first so each session's checkpoint has a later commit time.
    fn snapshot_alone(fixture: &Fixture, shadow: &Shadow, session: &str, seed: u64) {
        std::thread::sleep(Duration::from_millis(1100));
        for entry in std::fs::read_dir(&fixture.workspace).expect("workspace") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|extension| extension == "bin") {
                std::fs::remove_file(path).expect("remove");
            }
        }
        std::fs::write(
            fixture.workspace.join(format!("{session}.bin")),
            noise(seed, 512 * 1024),
        )
        .expect("file");
        shadow.snapshot(session, "turn 1 end").expect("snapshot");
    }

    fn references(shadow: &Shadow) -> Vec<String> {
        let listed = shadow
            .git(
                &["for-each-ref", "--format=%(refname)", "refs/rune/"],
                DEFAULT_DEADLINE,
            )
            .expect("refs");
        String::from_utf8_lossy(&listed)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_cap_removes_the_oldest_sessions_first() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let shadow = open(&fixture);
        for session in ["oldest", "middle", "current"] {
            std::fs::create_dir_all(fixture.paths.sessions_dir().join(session)).expect("dir");
        }
        snapshot_alone(&fixture, &shadow, "oldest", 1);
        snapshot_alone(&fixture, &shadow, "middle", 2);
        let cap = shadow.size() + 256 * 1024;
        snapshot_alone(&fixture, &shadow, "current", 3);
        assert!(shadow.size() > cap);

        shadow.enforce_cap("current", cap).expect("cap");
        assert!(shadow.size() <= cap, "{} > {cap}", shadow.size());
        assert_eq!(
            references(&shadow),
            vec![
                "refs/rune/current".to_owned(),
                "refs/rune/middle".to_owned()
            ]
        );
        // Only the current session is never removed, however small the cap.
        shadow.enforce_cap("current", 0).expect("cap");
        assert_eq!(references(&shadow), vec!["refs/rune/current".to_owned()]);
        assert_eq!(
            store_size(&fixture.paths, &fixture.workspace),
            Some(shadow.size())
        );
    }

    #[test]
    fn refs_of_removed_sessions_are_deleted_first() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let shadow = open(&fixture);
        for session in ["kept", "current"] {
            std::fs::create_dir_all(fixture.paths.sessions_dir().join(session)).expect("dir");
        }
        snapshot_alone(&fixture, &shadow, "kept", 1);
        snapshot_alone(&fixture, &shadow, "gone", 2);
        let cap = shadow.size() + 256 * 1024;
        snapshot_alone(&fixture, &shadow, "current", 3);

        shadow.enforce_cap("current", cap).expect("cap");
        assert_eq!(
            references(&shadow),
            vec!["refs/rune/current".to_owned(), "refs/rune/kept".to_owned()]
        );
        remove_session(&fixture.paths, &fixture.workspace, "kept").expect("remove");
        assert_eq!(references(&shadow), vec!["refs/rune/current".to_owned()]);
    }

    fn checkpoint(turn: u64, boundary: Boundary, tree: &str) -> SessionEvent {
        SessionEvent::WorkspaceCheckpoint {
            turn,
            boundary,
            tree: tree.to_owned(),
            commit: format!("commit-{tree}"),
            parent: None,
            skipped: Vec::new(),
            complete: true,
        }
    }

    fn restored(turn: u64, direction: RestoreDirection, target: &str, saved: &str) -> SessionEvent {
        SessionEvent::Restored {
            turn,
            direction,
            target: target.to_owned(),
            saved: saved.to_owned(),
        }
    }

    #[test]
    fn restore_writes_only_changed_paths() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let shadow = open(&fixture);
        let kept = fixture.workspace.join("kept.txt");
        std::fs::write(&kept, "kept\n").expect("file");
        let before = shadow.snapshot("s", "turn 1 start").expect("snapshot");
        std::fs::write(fixture.workspace.join("a.txt"), "changed\n").expect("edit");
        let after = shadow.snapshot("s", "turn 1 end").expect("snapshot");
        let stamp = std::fs::metadata(&kept)
            .expect("meta")
            .modified()
            .expect("mtime");
        std::thread::sleep(Duration::from_millis(20));

        let changes = shadow.restore(&after.tree, &before.tree).expect("restore");
        assert_eq!(
            changes.iter().map(FileChange::line).collect::<Vec<_>>(),
            vec!["M a.txt +1 -1".to_owned()]
        );
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("a.txt")).expect("read"),
            "a\n"
        );
        let unchanged = std::fs::metadata(&kept)
            .expect("meta")
            .modified()
            .expect("mtime");
        assert_eq!(unchanged, stamp);
    }

    #[test]
    fn restore_removes_files_the_target_lacks() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let shadow = open(&fixture);
        let before = shadow.snapshot("s", "turn 1 start").expect("snapshot");
        std::fs::create_dir_all(fixture.workspace.join("made/by")).expect("dir");
        std::fs::write(fixture.workspace.join("made/by/command"), "x\n").expect("file");
        let after = shadow.snapshot("s", "turn 1 end").expect("snapshot");

        let changes = shadow.restore(&after.tree, &before.tree).expect("restore");
        assert_eq!(
            changes.iter().map(FileChange::line).collect::<Vec<_>>(),
            vec!["D made/by/command".to_owned()]
        );
        assert!(
            !fixture.workspace.join("made").exists(),
            "empty directories are removed"
        );
        assert!(fixture.workspace.join("a.txt").exists());
    }

    #[test]
    fn restore_keeps_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt as _;
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let shadow = open(&fixture);
        let script = fixture.workspace.join("run.sh");
        std::fs::write(&script, "#!/bin/sh\necho one\n").expect("file");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let first = shadow.snapshot("s", "turn 1 start").expect("snapshot");
        std::fs::write(&script, "#!/bin/sh\necho two\n").expect("edit");
        let second = shadow.snapshot("s", "turn 1 end").expect("snapshot");

        shadow.restore(&second.tree, &first.tree).expect("undo");
        let mode = std::fs::metadata(&script)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "undo keeps the mode");
        shadow.restore(&first.tree, &second.tree).expect("redo");
        let mode = std::fs::metadata(&script)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "redo keeps the mode");
        assert_eq!(
            std::fs::read_to_string(&script).expect("read"),
            "#!/bin/sh\necho two\n"
        );
    }

    #[test]
    fn a_store_from_before_private_stores_is_tightened_on_open() {
        use std::os::unix::fs::PermissionsExt as _;
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let shadow = open(&fixture);
        shadow.snapshot("s", "turn 1 start").expect("snapshot");
        let config = shadow.git_dir.join("config");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).expect("mode");
        shadow
            .git(
                &["config", "--unset", "core.sharedRepository"],
                DEFAULT_DEADLINE,
            )
            .expect("unset");
        let reopened = open(&fixture);
        let mode = std::fs::metadata(&config)
            .expect("config")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let shared = real_git(
            &fixture.workspace,
            &[
                "config",
                "--file",
                reopened.git_dir.join("config").as_str(),
                "--get",
                "core.sharedRepository",
            ],
        );
        assert_eq!(String::from_utf8_lossy(&shared).trim(), PRIVATE_SHARING);
    }

    #[test]
    fn unsafe_paths_are_refused_before_writing() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let shadow = open(&fixture);
        for path in [
            "../escape",
            ".git/config",
            "a/../../b",
            "/etc/passwd",
            "sub/.GIT/x",
        ] {
            let error = shadow.check_safe(path).expect_err(path);
            assert_eq!(
                error.message(),
                format!("checkpoint path {path} is not safe to restore")
            );
        }
        // A directory that is now a link out of the workspace is not written through.
        std::fs::create_dir(fixture.workspace.join("link")).expect("dir");
        std::fs::write(fixture.workspace.join("link/file.txt"), "inside\n").expect("file");
        std::fs::write(fixture.workspace.join("a.txt"), "edited\n").expect("edit");
        let target = shadow.snapshot("s", "turn 1 start").expect("snapshot");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::remove_dir_all(fixture.workspace.join("link")).expect("remove");
        std::os::unix::fs::symlink(outside.path(), fixture.workspace.join("link")).expect("link");
        std::fs::write(fixture.workspace.join("a.txt"), "later\n").expect("edit");
        let current = shadow.snapshot("s", "turn 1 end").expect("snapshot");

        let error = shadow
            .restore(&current.tree, &target.tree)
            .expect_err("refused");
        assert_eq!(
            error.message(),
            "checkpoint path link/file.txt is not safe to restore"
        );
        assert!(!outside.path().join("file.txt").exists());
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("a.txt")).expect("read"),
            "later\n",
            "nothing is written when one path is refused"
        );
    }

    #[test]
    fn timeline_steps_back_over_turns_without_changes() {
        // Turns 1 and 3 changed files; turn 2 recorded nothing because its
        // trees repeated the previous checkpoint.
        let events = vec![
            checkpoint(1, Boundary::Start, "t0"),
            checkpoint(1, Boundary::End, "t1"),
            checkpoint(3, Boundary::End, "t3"),
        ];
        let mut timeline = Timeline::from_events(&events);
        assert_eq!(timeline.undo_target(), Some((3, "t1".to_owned())));
        assert_eq!(
            timeline.before_turn(2).map(|start| start.tree),
            Some("t1".to_owned())
        );
        timeline.apply(&restored(3, RestoreDirection::Undo, "t1", "saved-3"));
        assert_eq!(timeline.undo_target(), Some((1, "t0".to_owned())));
        assert_eq!(timeline.redo_target(), Some((3, "saved-3".to_owned())));
        timeline.apply(&restored(1, RestoreDirection::Undo, "t0", "saved-1"));
        assert_eq!(timeline.undo_target(), None);
        assert_eq!(timeline.redo_target(), Some((1, "saved-1".to_owned())));
        timeline.apply(&restored(1, RestoreDirection::Redo, "saved-1", "saved-r"));
        assert_eq!(timeline.redo_target(), Some((3, "saved-3".to_owned())));
        assert_eq!(timeline.undo_target(), Some((1, "t0".to_owned())));
        assert_eq!(timeline.current(), Some("saved-1"));
    }

    #[test]
    fn a_new_change_clears_redo() {
        let mut timeline = Timeline::from_events(&[
            checkpoint(1, Boundary::Start, "t0"),
            checkpoint(1, Boundary::End, "t1"),
            restored(1, RestoreDirection::Undo, "t0", "saved-1"),
        ]);
        assert!(timeline.redo_target().is_some());
        // A turn that changes nothing keeps the redo target.
        timeline.apply(&checkpoint(2, Boundary::Start, "t0"));
        assert!(timeline.redo_target().is_some());
        timeline.apply(&checkpoint(3, Boundary::End, "t4"));
        assert_eq!(timeline.redo_target(), None);
        assert_eq!(timeline.undo_target(), Some((3, "t0".to_owned())));
    }

    /// The workspace checkpoints a session log holds, in order.
    fn recorded(fixture: &Fixture, id: &rune_core::id::SessionId) -> Vec<SessionEvent> {
        rune_session::store::load_read_only(&fixture.paths.session_dir(id))
            .expect("log")
            .events
            .into_iter()
            .map(|frame| frame.event)
            .filter(|event| matches!(event, SessionEvent::WorkspaceCheckpoint { .. }))
            .collect()
    }

    /// Records checkpoints into a new interactive session log.
    fn recorder_sink(fixture: &Fixture, id: &rune_core::id::SessionId) -> Record {
        let recorder =
            Mutex::new(crate::session_log::Recorder::create(&fixture.paths, id).expect("recorder"));
        Arc::new(move |event| {
            recorder
                .lock()
                .expect("recorder")
                .workspace_checkpoint(event)
        })
    }

    fn worker(fixture: &Fixture, delay: Duration) -> (Checkpoints, rune_core::id::SessionId) {
        let id = rune_core::id::SessionId::generate();
        let notify: Notify = Arc::new(|line| panic!("unexpected notice: {line}"));
        let checkpoints = Checkpoints::start_with_delay(
            open(fixture),
            id.as_str().to_owned(),
            recorder_sink(fixture, &id),
            notify,
            delay,
        );
        (checkpoints, id)
    }

    #[test]
    fn identical_trees_record_one_checkpoint() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let (checkpoints, id) = worker(&fixture, Duration::ZERO);
        checkpoints.turn_started(1);
        assert!(checkpoints.wait_for_start(Duration::from_secs(30)));
        checkpoints.turn_finished(1);
        checkpoints.turn_started(2);
        assert!(checkpoints.wait_for_start(Duration::from_secs(30)));
        std::fs::write(fixture.workspace.join("a.txt"), "changed\n").expect("edit");
        checkpoints.turn_finished(2);
        let latest = loop {
            let latest = checkpoints.latest().expect("a checkpoint");
            if latest.turn == 2 {
                break latest;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(latest.boundary, Boundary::End);
        drop(checkpoints);
        let events = recorded(&fixture, &id);
        let shape: Vec<(u64, Boundary, bool)> = events
            .iter()
            .map(|event| match event {
                SessionEvent::WorkspaceCheckpoint {
                    turn,
                    boundary,
                    complete,
                    ..
                } => (*turn, *boundary, *complete),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            shape,
            vec![(1, Boundary::Start, true), (2, Boundary::End, true)]
        );
        let SessionEvent::WorkspaceCheckpoint { commit, .. } = &events[0] else {
            unreachable!()
        };
        let SessionEvent::WorkspaceCheckpoint { parent, .. } = &events[1] else {
            unreachable!()
        };
        assert_eq!(parent.as_deref(), Some(commit.as_str()));
    }

    #[test]
    fn a_slow_start_snapshot_is_marked_incomplete() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let (checkpoints, id) = worker(&fixture, Duration::from_millis(500));
        checkpoints.turn_started(1);
        let started = Instant::now();
        assert!(!checkpoints.wait_for_start(Duration::from_millis(50)));
        assert!(started.elapsed() < Duration::from_millis(450));
        drop(checkpoints);
        let events = recorded(&fixture, &id);
        assert!(
            matches!(
                events.as_slice(),
                [SessionEvent::WorkspaceCheckpoint {
                    turn: 1,
                    boundary: Boundary::Start,
                    complete: false,
                    ..
                }]
            ),
            "{events:?}"
        );
    }

    #[test]
    fn a_failed_snapshot_is_reported_once() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let shadow = open(&fixture);
        std::fs::remove_dir_all(&shadow.git_dir).expect("remove shadow");
        let id = rune_core::id::SessionId::generate();
        let record = recorder_sink(&fixture, &id);
        let notices = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&notices);
        let notify: Notify = Arc::new(move |line| sink.lock().expect("notices").push(line));
        let checkpoints = Checkpoints::start(shadow, id.as_str().to_owned(), record, notify);
        checkpoints.turn_started(1);
        assert!(checkpoints.wait_for_start(Duration::from_secs(30)));
        checkpoints.turn_finished(1);
        drop(checkpoints);
        let notices = notices.lock().expect("notices");
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].starts_with("checkpoint failed: git "),
            "{notices:?}"
        );
        assert!(recorded(&fixture, &id).is_empty());
    }

    #[test]
    fn a_shadow_of_another_workspace_is_refused() {
        if !git_present() {
            return;
        }
        let fixture = fixture();
        let shadow = open(&fixture);
        paths::write_private(&shadow.git_dir.join(WORKSPACE_FILE), "/elsewhere\n").expect("marker");
        let error = Shadow::open(&fixture.paths, &fixture.workspace)
            .err()
            .expect("refused");
        assert!(
            error.to_string().contains("belongs to /elsewhere"),
            "{error}"
        );
    }
}
