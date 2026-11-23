//! A saved `rune ask --checkpoints` run snapshots the workspace around its turn.
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use rune_testkit::scripted::{ScriptedProvider, Turn};
use sha2::{Digest as _, Sha256};

fn git_present() -> bool {
    let present = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !present {
        eprintln!("skipped: git is not installed");
    }
    present
}

fn git(workspace: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
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

/// A committed git workspace and the state root of an isolated run.
fn workspace(root: &Path) -> PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let workspace = std::fs::canonicalize(&workspace).expect("canonical workspace");
    git(&workspace, &["init", "-q"]);
    std::fs::write(workspace.join("a.txt"), "a\n").expect("file");
    git(&workspace, &["add", "-A"]);
    git(
        &workspace,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "init",
        ],
    );
    workspace
}

fn rune(root: &Path, workspace: &Path, base_url: &str, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rune"));
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("HOME", root.join("home"))
        .env("RUNE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("RUNE_PROVIDER", "chat_completions")
        .env("RUNE_BASE_URL", base_url)
        .env("RUNE_MODEL", "scripted")
        .env("RUNE_API_KEY_ENV", "RUNE_SCENARIO_KEY")
        .env("RUNE_SCENARIO_KEY", "scenario-key")
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()
        .expect("the binary to run")
}

fn tree_names(shadow: &Path, tree: &str) -> Vec<String> {
    let output = Command::new("git")
        .arg(format!("--git-dir={}", shadow.display()))
        .args(["ls-tree", "-r", "--name-only", tree])
        .output()
        .expect("ls-tree");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Sums the sizes of the files under a directory.
fn size(directory: &Path) -> u64 {
    std::fs::read_dir(directory)
        .expect("directory")
        .map(|entry| entry.expect("entry"))
        .map(|entry| {
            let meta = std::fs::symlink_metadata(entry.path()).expect("metadata");
            if meta.is_dir() {
                size(&entry.path())
            } else {
                meta.len()
            }
        })
        .sum()
}

#[test]
fn ask_checkpoints_capture_a_command_made_file() {
    if !git_present() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let workspace = workspace(root);
    let status_before = git(&workspace, &["status", "--porcelain=v2"]);
    let index_before = std::fs::read(workspace.join(".git/index")).expect("index");
    let refs_before = git(&workspace, &["for-each-ref"]);
    let provider = ScriptedProvider::start(vec![
        Turn::call(
            "call_1",
            "shell",
            r#"{"action": "run", "command": "echo x > made-by-command"}"#,
        ),
        Turn::text("done"),
    ]);
    let output = rune(
        root,
        &workspace,
        &provider.base_url(),
        &[
            "--permission-mode",
            "full-access",
            "--allow-unsandboxed",
            "ask",
            "--json",
            "--checkpoints",
            "--",
            "go",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(workspace.join("made-by-command").exists());

    let last = rune(
        root,
        &workspace,
        &provider.base_url(),
        &["session", "last", "--json"],
    );
    assert!(last.status.success(), "{last:?}");
    let summary: serde_json::Value = serde_json::from_slice(&last.stdout).expect("summary");
    let id = summary["id"].as_str().expect("session id");
    let log = std::fs::read_to_string(root.join("state/sessions").join(id).join("events.jsonl"))
        .expect("event log");
    let checkpoints: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("frame"))
        .map(|frame| frame["event"].clone())
        .filter(|event| event["kind"] == "workspace_checkpoint")
        .collect();
    assert_eq!(checkpoints.len(), 2, "{log}");
    assert_eq!(checkpoints[0]["boundary"], "start");
    assert_eq!(checkpoints[1]["boundary"], "end");
    assert_eq!(checkpoints[0]["complete"], true);
    assert_eq!(checkpoints[1]["parent"], checkpoints[0]["commit"]);

    let digest = Sha256::digest(workspace.to_str().expect("UTF-8").as_bytes());
    let mut key = String::new();
    for byte in &digest[..8] {
        write!(key, "{byte:02x}").expect("hex");
    }
    let shadow = root.join("state/checkpoints").join(key);
    let start = tree_names(&shadow, checkpoints[0]["tree"].as_str().expect("tree"));
    let end = tree_names(&shadow, checkpoints[1]["tree"].as_str().expect("tree"));
    assert!(end.contains(&"made-by-command".to_owned()), "{end:?}");
    assert!(!start.contains(&"made-by-command".to_owned()), "{start:?}");
    eprintln!("checkpoint store: {} bytes", size(&shadow));

    // The workspace repository gained an untracked file and nothing else.
    assert_eq!(
        std::fs::read(workspace.join(".git/index")).expect("index"),
        index_before
    );
    assert_eq!(git(&workspace, &["for-each-ref"]), refs_before);
    let status_after =
        String::from_utf8(git(&workspace, &["status", "--porcelain=v2"])).expect("status");
    assert_eq!(
        status_after,
        format!(
            "{}? made-by-command\n",
            String::from_utf8(status_before).expect("status")
        )
    );
}

#[test]
fn checkpoints_need_a_saved_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let output = rune(
        root,
        &workspace,
        "http://127.0.0.1:9/v1",
        &["ask", "--no-save", "--checkpoints", "--", "go"],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--checkpoints needs a saved session; drop --no-save"),
        "{stderr}"
    );
}

/// Runs one saved `ask` turn whose shell call creates `made-by-command`.
fn command_turn(root: &Path) -> (PathBuf, ScriptedProvider) {
    let workspace = workspace(root);
    let provider = ScriptedProvider::start(vec![
        Turn::call(
            "call_1",
            "shell",
            r#"{"action": "run", "command": "mkdir -p out && echo x > out/made-by-command"}"#,
        ),
        Turn::text("done"),
    ]);
    let output = rune(
        root,
        &workspace,
        &provider.base_url(),
        &[
            "--permission-mode",
            "full-access",
            "--allow-unsandboxed",
            "ask",
            "--checkpoints",
            "--",
            "go",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(workspace.join("out/made-by-command").exists());
    (workspace, provider)
}

#[test]
fn session_restore_dry_run_lists_changes() {
    if !git_present() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let (workspace, provider) = command_turn(root);

    let text = rune(
        root,
        &workspace,
        &provider.base_url(),
        &["session", "restore", "last", "--turn", "1", "--dry-run"],
    );
    assert!(text.status.success(), "{text:?}");
    assert_eq!(
        String::from_utf8_lossy(&text.stdout),
        "undo preview: restoring to before turn 1 would change\nA out/made-by-command +1 -0\n"
    );
    let json = rune(
        root,
        &workspace,
        &provider.base_url(),
        &[
            "session",
            "restore",
            "last",
            "--turn",
            "1",
            "--dry-run",
            "--json",
        ],
    );
    assert!(json.status.success(), "{json:?}");
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).expect("json");
    assert_eq!(
        value,
        serde_json::json!({
            "turn": 1,
            "dry_run": true,
            "changes": [{"path": "out/made-by-command", "kind": "added", "added": 1, "removed": 0}],
        })
    );
    assert!(
        workspace.join("out/made-by-command").exists(),
        "a dry run changes nothing"
    );
}

#[test]
fn session_restore_puts_back_a_command_made_change() {
    if !git_present() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let (workspace, provider) = command_turn(root);
    let last = rune(
        root,
        &workspace,
        &provider.base_url(),
        &["session", "last", "--json"],
    );
    let summary: serde_json::Value = serde_json::from_slice(&last.stdout).expect("summary");
    let id = summary["id"].as_str().expect("session id").to_owned();

    // Another directory is refused before anything is read from the store.
    let elsewhere = root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("dir");
    let refused = rune(
        root,
        &elsewhere,
        &provider.base_url(),
        &["session", "restore", &id, "--turn", "1"],
    );
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains(&format!(
            "session {id} ran in {}; run this command there",
            workspace.display()
        )),
        "{stderr}"
    );

    // A file edited outside the session refuses the restore without --force.
    std::fs::write(workspace.join("a.txt"), "edited\n").expect("edit");
    let outside = rune(
        root,
        &workspace,
        &provider.base_url(),
        &["session", "restore", "last", "--turn", "1"],
    );
    assert_eq!(outside.status.code(), Some(1), "{outside:?}");
    assert!(
        String::from_utf8_lossy(&outside.stdout).starts_with(
            "these files changed outside this session since the last checkpoint:\nM a.txt +1 -1\n"
        ),
        "{outside:?}"
    );
    assert!(workspace.join("out/made-by-command").exists());
    std::fs::write(workspace.join("a.txt"), "a\n").expect("edit back");

    let restored = rune(
        root,
        &workspace,
        &provider.base_url(),
        &["session", "restore", "last", "--turn", "1"],
    );
    assert!(restored.status.success(), "{restored:?}");
    assert_eq!(
        String::from_utf8_lossy(&restored.stdout),
        "restored the workspace to before turn 1\nA out/made-by-command +1 -0\n"
    );
    assert!(
        !workspace.join("out").exists(),
        "the emptied directory is removed"
    );
    let log = std::fs::read_to_string(root.join("state/sessions").join(&id).join("events.jsonl"))
        .expect("event log");
    let restore: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("frame"))
        .map(|frame| frame["event"].clone())
        .filter(|event| event["kind"] == "restored")
        .collect();
    assert_eq!(restore.len(), 1, "{log}");
    assert_eq!(restore[0]["direction"], "turn");
    assert_eq!(restore[0]["turn"], 1);
}

/// Every file Rune leaves under its state root is readable by the user only:
/// files 0600 (git's read-only objects 0400) and directories 0700, the
/// checkpoint store included.
#[cfg(unix)]
#[test]
fn every_state_file_is_private() {
    use std::os::unix::fs::PermissionsExt as _;

    if !git_present() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let workspace = workspace(root);
    let provider = ScriptedProvider::start(vec![
        Turn::call(
            "call_1",
            "shell",
            r#"{"action": "run", "command": "echo x > made-by-command"}"#,
        ),
        Turn::call(
            "call_2",
            "write_file",
            r#"{"path": "written.txt", "content": "y\n"}"#,
        ),
        Turn::text("done"),
        // The reminder to check a turn that changed files.
        Turn::text("checked"),
    ]);
    let output = rune(
        root,
        &workspace,
        &provider.base_url(),
        &[
            "--permission-mode",
            "full-access",
            "--allow-unsandboxed",
            "ask",
            "--json",
            "--checkpoints",
            "--",
            "go",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut open = Vec::new();
    let mut stack = vec![root.join("state")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("state directory") {
            let entry = entry.expect("entry");
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).expect("metadata");
            let mode = metadata.permissions().mode() & 0o777;
            if metadata.file_type().is_symlink() {
                continue;
            }
            // Git keeps its objects read-only, so 0400 is as private as 0600.
            let private = if metadata.is_dir() {
                mode == 0o700
            } else {
                mode == 0o600 || mode == 0o400
            };
            if !private {
                open.push(format!("{:o} {}", mode, path.display()));
            }
            if metadata.is_dir() {
                stack.push(path);
            }
        }
    }
    assert!(open.is_empty(), "files not private:\n{}", open.join("\n"));
}
