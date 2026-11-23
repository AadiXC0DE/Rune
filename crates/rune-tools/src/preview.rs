//! What an edit, a write or a patch would change, for an approval to show.
//!
//! The terminal renders these as a diff under its approval prompt, and the ACP
//! server sends them to the client with the permission request, so the person
//! deciding sees the same change wherever they are asked.

use camino::Utf8Path;
use serde_json::Value;

use crate::apply_patch::{Patch, Section};

/// Largest file or new content an approval reads or shows, in bytes.
pub const PREVIEW_BYTES: usize = 256 * 1024;

/// One change a call would make to one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProposedChange {
    /// The file, as the call names it.
    pub path: String,
    /// The text that would be replaced; `None` for a file the call creates.
    pub old_text: Option<String>,
    /// The text that would take its place.
    pub new_text: String,
}

/// Returns the old and new text of each edit an `edit_file` call asks for.
#[must_use]
pub fn edit_pairs(arguments: &Value) -> Vec<(String, String)> {
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match arguments.get("edits").and_then(Value::as_array) {
        Some(edits) => edits
            .iter()
            .map(|edit| (text(edit, "old_string"), text(edit, "new_string")))
            .collect(),
        None => vec![(text(arguments, "old_string"), text(arguments, "new_string"))],
    }
}

/// Returns the old and new text of each part of a patch: an added file is all
/// new, and a deleted one is not diffed.
#[must_use]
pub fn patch_pairs(patch: &Patch) -> Vec<(String, String)> {
    patch
        .sections
        .iter()
        .flat_map(|section| match section {
            Section::Add { contents, .. } => vec![(String::new(), contents.clone())],
            Section::Delete { .. } => Vec::new(),
            Section::Update { chunks, .. } => chunks
                .iter()
                .map(|chunk| (chunk.old_lines().join("\n"), chunk.new_lines().join("\n")))
                .collect(),
        })
        .collect()
}

/// Returns the changes an `edit_file`, `write_file` or `apply_patch` call
/// would make, and nothing for any other tool.
///
/// Only files inside `workspace` are read, so a write aimed elsewhere never
/// sends that file's text to whoever is asked. A file or new content larger
/// than [`PREVIEW_BYTES`] is left out rather than cut.
#[must_use]
pub fn proposed_changes(
    name: &str,
    arguments: &Value,
    workspace: &Utf8Path,
) -> Vec<ProposedChange> {
    let path = || {
        arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match name {
        "edit_file" => edit_pairs(arguments)
            .into_iter()
            .map(|(old, new)| ProposedChange {
                path: path(),
                old_text: Some(old),
                new_text: new,
            })
            .collect(),
        "write_file" => {
            let path = path();
            let Some(content) = arguments.get("content").and_then(Value::as_str) else {
                return Vec::new();
            };
            if path.is_empty() || content.len() > PREVIEW_BYTES {
                return Vec::new();
            }
            match current_text(workspace, &path) {
                Current::Missing => vec![ProposedChange {
                    path,
                    old_text: None,
                    new_text: content.to_owned(),
                }],
                Current::Text(old) => vec![ProposedChange {
                    path,
                    old_text: Some(old),
                    new_text: content.to_owned(),
                }],
                Current::Unreadable => Vec::new(),
            }
        }
        "apply_patch" => {
            let input = arguments
                .get("input")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let Ok(patch) = crate::apply_patch::parse(input) else {
                return Vec::new();
            };
            patch
                .sections
                .iter()
                .flat_map(|section| match section {
                    Section::Add { path, contents } => vec![ProposedChange {
                        path: path.clone(),
                        old_text: None,
                        new_text: contents.clone(),
                    }],
                    Section::Delete { .. } => Vec::new(),
                    Section::Update { path, chunks, .. } => chunks
                        .iter()
                        .map(|chunk| ProposedChange {
                            path: path.clone(),
                            old_text: Some(chunk.old_lines().join("\n")),
                            new_text: chunk.new_lines().join("\n"),
                        })
                        .collect(),
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// What a file holds now, as far as an approval may say.
enum Current {
    Missing,
    Text(String),
    /// Outside the workspace, too large, not text, or not readable.
    Unreadable,
}

fn current_text(workspace: &Utf8Path, path: &str) -> Current {
    let named = Utf8Path::new(path);
    let target = workspace.join(named);
    let inside = target.starts_with(workspace)
        && !named
            .components()
            .any(|part| part == camino::Utf8Component::ParentDir);
    match std::fs::metadata(&target) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && inside => Current::Missing,
        Ok(meta) if inside && meta.len() <= u64::try_from(PREVIEW_BYTES).unwrap_or(u64::MAX) => {
            std::fs::read_to_string(&target).map_or(Current::Unreadable, Current::Text)
        }
        _ => Current::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace() -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        (dir, path)
    }

    #[test]
    fn an_edit_names_each_replacement() {
        let (_dir, root) = workspace();
        let changes = proposed_changes(
            "edit_file",
            &json!({"path": "a.txt", "edits": [
                {"old_string": "one", "new_string": "1"},
                {"old_string": "two", "new_string": "2"},
            ]}),
            &root,
        );
        assert_eq!(
            changes,
            [
                ProposedChange {
                    path: "a.txt".to_owned(),
                    old_text: Some("one".to_owned()),
                    new_text: "1".to_owned(),
                },
                ProposedChange {
                    path: "a.txt".to_owned(),
                    old_text: Some("two".to_owned()),
                    new_text: "2".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn a_write_shows_the_file_it_replaces_only_inside_the_workspace() {
        let (_dir, root) = workspace();
        std::fs::write(root.join("old.txt"), "before\n").expect("seed");
        let write = |path: &str| {
            proposed_changes(
                "write_file",
                &json!({"path": path, "content": "after\n"}),
                &root,
            )
        };
        assert_eq!(write("old.txt")[0].old_text.as_deref(), Some("before\n"));
        assert_eq!(write("new.txt")[0].old_text, None);
        // An absolute path inside the workspace is read; one that leaves it
        // never is.
        assert_eq!(
            write(root.join("old.txt").as_str())[0].old_text.as_deref(),
            Some("before\n")
        );
        assert!(write("../outside.txt").is_empty());
    }

    #[test]
    fn a_patch_lists_added_and_updated_files() {
        let (_dir, root) = workspace();
        let input = "*** Begin Patch\n*** Add File: new.txt\n+hello\n*** Update File: a.txt\n@@\n-one\n+uno\n*** End Patch\n";
        let changes = proposed_changes("apply_patch", &json!({"input": input}), &root);
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert_eq!(changes[0].path, "new.txt");
        assert_eq!(changes[0].old_text, None);
        assert_eq!(changes[1].path, "a.txt");
        assert_eq!(changes[1].old_text.as_deref(), Some("one"));
        assert_eq!(changes[1].new_text, "uno");
    }

    #[test]
    fn other_tools_propose_nothing() {
        let (_dir, root) = workspace();
        assert!(proposed_changes("shell", &json!({"command": "ls"}), &root).is_empty());
    }
}
