//! A short picture of the workspace for the first message of `rune ask`: the
//! branch, the changed files and the top-level names.

use std::fmt::Write as _;
use std::time::Duration;

use camino::Utf8Path;

/// How long each `git` command may take.
const GIT_LIMIT: Duration = Duration::from_secs(1);

/// Most `git status` lines shown.
const MAX_STATUS_LINES: usize = 20;

/// Most top-level names shown.
const MAX_NAMES: usize = 40;

/// Largest snapshot block, in bytes.
pub const MAX_SNAPSHOT_BYTES: usize = 1536;

/// Renders the snapshot block, or `None` when there is nothing to show.
#[must_use]
pub fn render(workspace: &Utf8Path) -> Option<String> {
    let git = |args: &[&str]| rune_tools::git_diff::git_output(workspace, args, GIT_LIMIT);
    // A repository with no commit yet has no `HEAD` to abbreviate, only the
    // branch it points at.
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"])
        .or_else(|| git(&["symbolic-ref", "--short", "-q", "HEAD"]))
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .map(|name| {
            if name == "HEAD" {
                "HEAD (detached)".to_owned()
            } else {
                name
            }
        });
    let status = branch.as_ref().and_then(|_| {
        git(&["status", "--short", "--untracked-files=normal"]).map(|output| {
            output
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
    });
    fit(branch.as_deref(), status.as_deref(), &top_level(workspace))
}

/// The names at the top of the workspace, sorted, directories with a `/`.
fn top_level(workspace: &Utf8Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(workspace) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if name == ".git" {
                return None;
            }
            let directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
            Some(if directory { format!("{name}/") } else { name })
        })
        .collect();
    names.sort();
    names
}

/// The largest block within [`MAX_SNAPSHOT_BYTES`]: status lines are dropped
/// from the end first, then top-level names.
fn fit(branch: Option<&str>, status: Option<&[String]>, names: &[String]) -> Option<String> {
    let changed = status.unwrap_or_default();
    let mut shown_status = changed.len().min(MAX_STATUS_LINES);
    let mut shown_names = names.len().min(MAX_NAMES);
    loop {
        let block = build(branch, status, shown_status, names, shown_names)?;
        if block.len() <= MAX_SNAPSHOT_BYTES {
            return Some(block);
        }
        if shown_status > 0 {
            shown_status = shown_status.saturating_sub(1);
        } else if shown_names > 0 {
            shown_names = shown_names.saturating_sub(1);
        } else {
            return Some(block);
        }
    }
}

/// The block with the first `shown_status` status lines and `shown_names` names.
fn build(
    branch: Option<&str>,
    status: Option<&[String]>,
    shown_status: usize,
    names: &[String],
    shown_names: usize,
) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(branch) = branch {
        let count = status.map_or(0, <[String]>::len);
        lines.push(match count {
            0 => format!("branch: {branch}"),
            1 => format!("branch: {branch}, 1 changed file"),
            count => format!("branch: {branch}, {count} changed files"),
        });
        if let Some(status) = status {
            lines.extend(status.iter().take(shown_status).cloned());
            if status.len() > shown_status {
                lines.push(format!(
                    "... and {} more",
                    status.len().saturating_sub(shown_status)
                ));
            }
        }
    }
    if !names.is_empty() {
        let mut line = format!("top level: {}", names[..shown_names].join(", "));
        if names.len() > shown_names {
            let more = names.len().saturating_sub(shown_names);
            if shown_names > 0 {
                line.push_str(", ");
            }
            let _ = write!(line, "and {more} more");
        }
        lines.push(line);
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "<workspace_snapshot>\n{}\n</workspace_snapshot>",
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("UTF-8 path");
        (dir, path)
    }

    fn git(root: &Utf8Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.test",
            ])
            .args(args)
            .current_dir(root)
            .output()
            .expect("git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    fn repository() -> (tempfile::TempDir, Utf8PathBuf) {
        let (dir, root) = workspace();
        git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("README.md"), "readme\n").expect("readme");
        std::fs::create_dir(root.join("src")).expect("src");
        std::fs::write(root.join("src/lib.rs"), "\n").expect("lib");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "start"]);
        (dir, root)
    }

    #[test]
    fn the_snapshot_names_the_branch_changes_and_top_level() {
        let (_dir, root) = repository();
        std::fs::write(root.join("README.md"), "changed\n").expect("change");
        std::fs::write(root.join("notes.txt"), "new\n").expect("new");
        let block = render(&root).expect("snapshot");
        assert_eq!(
            block,
            "<workspace_snapshot>\nbranch: main, 2 changed files\n M README.md\n?? notes.txt\n\
             top level: README.md, notes.txt, src/\n</workspace_snapshot>"
        );
    }

    #[test]
    fn a_long_status_is_cut_with_a_count_and_fits_the_cap() {
        let (_dir, root) = repository();
        for index in 0..300 {
            std::fs::write(root.join(format!("untracked_{index:03}.txt")), "x").expect("file");
        }
        let block = render(&root).expect("snapshot");
        assert!(block.len() <= MAX_SNAPSHOT_BYTES, "{}", block.len());
        assert!(block.contains("branch: main, 300 changed files"), "{block}");
        assert!(block.contains("... and 280 more"), "{block}");
        assert_eq!(
            block.lines().filter(|line| line.starts_with("??")).count(),
            20
        );
        assert!(block.contains(", and 262 more"), "{block}");
    }

    #[test]
    fn outside_git_only_the_top_level_is_shown() {
        let (_dir, root) = workspace();
        std::fs::write(root.join("a.txt"), "a").expect("file");
        std::fs::create_dir(root.join("docs")).expect("dir");
        assert_eq!(
            render(&root).expect("snapshot"),
            "<workspace_snapshot>\ntop level: a.txt, docs/\n</workspace_snapshot>"
        );
    }

    #[test]
    fn an_empty_workspace_has_no_snapshot() {
        let (_dir, root) = workspace();
        assert_eq!(render(&root), None);
    }

    #[test]
    fn a_large_block_drops_status_lines_before_names() {
        let status: Vec<String> = (0..20)
            .map(|index| format!("?? {}", "s".repeat(60) + &index.to_string()))
            .collect();
        let names: Vec<String> = (0..40)
            .map(|index| format!("{}{index}", "n".repeat(20)))
            .collect();
        let block = fit(Some("main"), Some(&status), &names).expect("block");
        assert!(block.len() <= MAX_SNAPSHOT_BYTES, "{}", block.len());
        assert!(block.contains("... and"), "{block}");
        assert!(block.contains(&names[39]), "{block}");
    }
}
