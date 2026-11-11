//! Workspace snapshots in a private git directory.
//!
//! The shadow directory lives under the state root and has its own index, so
//! the workspace's repository, index, refs and hooks are never read or written.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::{self, Paths};
use rune_exec::session::{Input, POLL_INTERVAL, Process};
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

/// A private git directory that snapshots one workspace.
pub struct Shadow {
    git_dir: Utf8PathBuf,
    workspace: Utf8PathBuf,
    index: Utf8PathBuf,
    exclude: Utf8PathBuf,
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
        let git_dir = shadow_dir(paths, &workspace);
        let shadow = Self {
            index: git_dir.join("rune-index"),
            exclude: git_dir.join("info").join("rune-exclude"),
            git_dir,
            workspace,
        };
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
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => shadow.create()?,
            Err(error) => return Err(error.into()),
        }
        Ok(Some(shadow))
    }

    /// Initialises a new shadow directory and its configuration.
    fn create(&self) -> Result<()> {
        paths::create_dir_private(&self.git_dir)?;
        run_git(
            &[
                "init".to_owned(),
                "--bare".to_owned(),
                "--quiet".to_owned(),
                self.git_dir.to_string(),
            ],
            &self.workspace,
            &environment(None),
            DEFAULT_DEADLINE,
        )?;
        let hooks = self.hooks_dir();
        std::fs::create_dir_all(&hooks)?;
        let settings = [
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
    pub fn snapshot(&self, session: &str, message: &str) -> Result<Snapshot> {
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

    /// Size in bytes of the shadow directory.
    pub fn size(&self) -> u64 {
        directory_size(self.git_dir.as_std_path())
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
