//! Project instruction discovery and assembly.
//!
//! Instructions come from `AGENTS.md` files in four places: the user file in the
//! configuration root, which applies to every workspace; each ancestor of the
//! workspace below the home directory; the workspace root; and each directory
//! between the workspace root and a tool target. A target receives the whole
//! chain arranged from widest to narrowest, so a package can add rules for its
//! own subtree without restating the rules above it.
//!
//! Only the primary workspace is read. Additional directories never contribute,
//! because instructions steer the agent and a repository that is merely open in
//! the session must not be able to supply them.
//!
//! Discovery walks the workspace subtree so the chain for any target is already
//! known when a tool call arrives. The walk is capped at `list_entries`
//! directories, skips hidden directories, and never follows a symlinked
//! directory, so a large or hostile tree cannot make it unbounded.

use std::collections::VecDeque;
use std::fmt::Write as _;

use camino::{Utf8Path, Utf8PathBuf};

use rune_core::budget::LimitName;
use rune_core::error::Result;
use rune_core::paths::{Paths, names};

use crate::resolve_limit;

/// One instruction file, together with the directory it governs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InstructionFile {
    /// Path of the file on disk.
    pub path: Utf8PathBuf,
    /// Directory whose subtree receives these rules.
    pub scope: Utf8PathBuf,
    /// Contents as read, cut at one byte past the per-file cap.
    pub content: String,
    /// Size of the file on disk, which may exceed the content that was read.
    pub declared_bytes: u64,
}

/// Where instructions are read from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Options {
    /// Primary workspace root.
    pub workspace: Utf8PathBuf,
    /// Home directory bounding the upward walk, absent when unresolvable.
    pub home: Option<Utf8PathBuf>,
    /// Configuration root holding the user-wide file.
    pub config_root: Utf8PathBuf,
    /// Whether instructions are read at all.
    pub enabled: bool,
}

impl Options {
    /// Builds options for one workspace, with reading enabled.
    #[must_use]
    pub fn new(workspace: &Utf8Path, home: Option<&Utf8Path>, config_root: &Utf8Path) -> Self {
        Self {
            workspace: workspace.to_owned(),
            home: home.map(Utf8Path::to_owned),
            config_root: config_root.to_owned(),
            enabled: true,
        }
    }

    /// Returns options that read nothing.
    ///
    /// This is the switch a `context = false` setting drives, so a repository
    /// can turn instruction loading off for itself.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// Discovers every candidate file, with the widest scope first.
    pub fn discover(&self) -> Result<Vec<InstructionFile>> {
        if !self.enabled {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        Self::push_file(&mut files, &self.config_root, &self.workspace);
        self.push_ancestors(&mut files);
        self.push_tree(&mut files);
        Ok(files)
    }

    /// Reads one candidate, recording it when it exists and is usable.
    fn push_file(out: &mut Vec<InstructionFile>, directory: &Utf8Path, scope: &Utf8Path) {
        let path = directory.join(names::INSTRUCTIONS_FILE);
        let Some((content, declared_bytes)) = read_candidate(&path) else {
            return;
        };
        out.push(InstructionFile {
            path,
            scope: scope.to_owned(),
            content,
            declared_bytes,
        });
    }

    /// Records every ancestor of the workspace below the home directory.
    ///
    /// The home directory itself is excluded, so a file in `$HOME` never applies
    /// to work inside a workspace below it.
    fn push_ancestors(&self, out: &mut Vec<InstructionFile>) {
        let Some(home) = self.home.as_deref() else {
            return;
        };
        let mut current = self.workspace.parent();
        while let Some(directory) = current {
            if !below(home, directory) {
                break;
            }
            Self::push_file(out, directory, directory);
            current = directory.parent();
        }
    }

    /// Records the workspace root and every file below it, breadth first.
    fn push_tree(&self, out: &mut Vec<InstructionFile>) {
        let cap = resolve_limit(LimitName::ListEntries);
        let mut queue = VecDeque::new();
        queue.push_back(self.workspace.clone());
        let mut visited = 0_usize;
        while let Some(directory) = queue.pop_front() {
            visited = visited.saturating_add(1);
            Self::push_file(out, &directory, &directory);
            for child in child_directories(&directory) {
                if visited.saturating_add(queue.len()) >= cap {
                    break;
                }
                queue.push_back(child);
            }
        }
    }
}

/// Discovers instructions for a workspace, using the process configuration root.
pub fn discover(workspace: &Utf8Path, home: Option<&Utf8Path>) -> Result<Vec<InstructionFile>> {
    let config_root = Paths::from_process().config_root;
    Options::new(workspace, home, &config_root).discover()
}

/// Returns the files that apply to a target, widest scope first.
///
/// A file applies when its scope is the target's own directory or an ancestor of
/// it, so a call outside a nested package never receives that package's rules.
/// Files are ranked by their distance from the target: the narrowest comes last,
/// and files at equal distance keep discovery order, which places the user-wide
/// file before the workspace root file.
#[must_use]
pub fn resolve_for_target<'a>(
    files: &'a [InstructionFile],
    target: &Utf8Path,
) -> Vec<&'a InstructionFile> {
    let anchor = endpoint(target);
    let mut ranked: Vec<(usize, &InstructionFile)> = files
        .iter()
        .filter_map(|file| distance(&file.scope, anchor).map(|distance| (distance, file)))
        .collect();
    // Farthest scope first, so the narrowest rules are read last and win. A
    // stable sort keeps discovery order between scopes at the same distance.
    ranked.sort_by_key(|(distance, _)| std::cmp::Reverse(*distance));
    ranked.into_iter().map(|(_, file)| file).collect()
}

/// Renders a resolved chain into the prompt.
///
/// Each file becomes a section labelled with its scope and path. A file over
/// `project_instruction_file_bytes` is shortened at a line boundary and marked
/// inline, and every file that was shortened or left out is also recorded as an
/// omission naming its path, so a reader can tell that instructions are
/// incomplete. The narrowest file is reserved first, so the rules closest to the
/// work are never the ones dropped to fit `project_instructions_total_bytes`.
///
/// The rendered text, omission records included, stays within the combined cap.
#[must_use]
pub fn render(files: &[&InstructionFile]) -> String {
    let per_file = resolve_limit(LimitName::ProjectInstructionFileBytes);
    let total = resolve_limit(LimitName::ProjectInstructionsTotalBytes);
    let sections: Vec<Section> = files.iter().map(|file| section(file, per_file)).collect();
    if sections.is_empty() {
        return String::new();
    }

    // Narrowest scope first, so the rules closest to the work are paid for
    // before the ones that merely widen it. Emission stays in chain order.
    let mut kept: Vec<Option<&str>> = vec![None; sections.len()];
    let mut remaining = total;
    for (index, part) in sections.iter().enumerate().rev() {
        let carried = part.text.len();
        let record = part.dropped.as_str();
        if carried <= remaining {
            remaining = remaining.saturating_sub(carried);
            kept[index] = Some(part.text.as_str());
        } else if record.len() <= remaining {
            remaining = remaining.saturating_sub(record.len());
            kept[index] = Some(record);
        }
    }

    let mut out = String::new();
    for part in kept.into_iter().flatten() {
        out.push_str(part);
    }
    out
}

/// A rendered instruction section.
#[derive(Debug)]
struct Section {
    /// Framed text, marker, and the record a truncated file always carries.
    text: String,
    /// Record naming the file when the whole section does not fit.
    dropped: String,
}

/// Renders one section, shortening the content when it exceeds `cap`.
fn section(file: &InstructionFile, cap: usize) -> Section {
    let retained = prefix(&file.content, cap);
    let truncated = file.declared_bytes > retained as u64;
    let mut text = String::new();
    let _ = writeln!(
        text,
        "<project-instructions scope=\"{}\" from=\"{}\">",
        escape(file.scope.as_str()),
        escape(file.path.as_str())
    );
    text.push_str(file.content.get(..retained).unwrap_or_default().trim_end());
    text.push('\n');
    if truncated {
        let _ = writeln!(
            text,
            "<instructions-truncated limit=\"{}\" observed_bytes=\"{}\" retained_bytes=\"{}\" />",
            LimitName::ProjectInstructionFileBytes.as_str(),
            file.declared_bytes,
            retained
        );
        text.push_str(&omission(
            file,
            LimitName::ProjectInstructionFileBytes.as_str(),
        ));
    }
    text.push_str("</project-instructions>\n");
    Section {
        text,
        dropped: omission(file, LimitName::ProjectInstructionsTotalBytes.as_str()),
    }
}

/// Renders a record naming one file that was not included in full.
fn omission(file: &InstructionFile, reason: &str) -> String {
    format!(
        "<project-instructions-omitted from=\"{}\" reason=\"{reason}\" />\n",
        escape(file.path.as_str())
    )
}

/// Returns the directory a target's rules are ranked against.
fn endpoint(target: &Utf8Path) -> &Utf8Path {
    match std::fs::metadata(target) {
        Ok(meta) if meta.is_dir() => target,
        _ => target.parent().unwrap_or(target),
    }
}

/// Returns how many path components separate a scope from a target directory.
fn distance(scope: &Utf8Path, anchor: &Utf8Path) -> Option<usize> {
    let rest = anchor.strip_prefix(scope).ok()?;
    Some(rest.components().count())
}

/// Returns true when `path` is strictly below `home`.
///
/// Shared with skill discovery, which stops its upward walk at the same
/// boundary.
pub(crate) fn below(home: &Utf8Path, path: &Utf8Path) -> bool {
    path != home && path.starts_with(home)
}

/// Returns the largest prefix length of `text` that fits in `max` bytes.
///
/// The cut is taken at a character boundary and then at a line boundary, so a
/// multi-byte character and an individual rule are never shown half written.
fn prefix(text: &str, max: usize) -> usize {
    if text.len() <= max {
        return text.len();
    }
    let mut end = max.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    match text.get(..end).and_then(|head| head.rfind('\n')) {
        Some(newline) if newline > 0 => newline,
        _ => end,
    }
}

/// Escapes the characters that would break out of an attribute value.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(character),
        }
    }
    out
}

/// Reads a candidate file, or returns `None` when it is unusable.
///
/// At most one byte past the per-file cap is read, so a huge or hostile file
/// cannot be loaded into memory merely because it is named `AGENTS.md`. The
/// extra byte is what makes an over-cap file detectable.
///
/// A missing, unreadable, non-regular, or non-UTF-8 file is skipped rather than
/// failing the turn: a broken instruction file must not stop work in a
/// repository that merely contains one.
fn read_candidate(path: &Utf8Path) -> Option<(String, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let bound = resolve_limit(LimitName::ProjectInstructionFileBytes).saturating_add(1);
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    let mut handle = std::io::Read::take(file, u64::try_from(bound).unwrap_or(u64::MAX));
    std::io::Read::read_to_end(&mut handle, &mut bytes).ok()?;
    let content = String::from_utf8(bytes).ok()?;
    Some((content, meta.len()))
}

/// Returns the immediate subdirectories of `directory`, in name order.
///
/// Hidden directories are skipped and symlinked directories are never followed,
/// so the subtree walk cannot wander into a build cache or leave the workspace.
fn child_directories(directory: &Utf8Path) -> Vec<Utf8PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut out: Vec<Utf8PathBuf> = entries
        .flatten()
        .filter_map(|entry| Utf8PathBuf::from_path_buf(entry.path()).ok())
        .filter(|path| {
            path.file_name().is_some_and(|name| !name.starts_with('.'))
                && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Creates a fixture tree and returns its root.
    fn tree() -> (TempDir, Utf8PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 tempdir");
        (dir, root)
    }

    /// Writes a fixture file, creating its parents.
    fn write(path: &Utf8Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, contents).expect("write fixture");
    }

    /// Discovers from an explicit configuration root.
    fn files_of(
        workspace: &Utf8Path,
        home: &Utf8Path,
        config_root: &Utf8Path,
    ) -> Vec<InstructionFile> {
        Options::new(workspace, Some(home), config_root)
            .discover()
            .expect("discover")
    }

    /// Returns the scopes of a resolved chain.
    fn scopes(chain: &[&InstructionFile]) -> Vec<String> {
        chain.iter().map(|file| file.scope.to_string()).collect()
    }

    #[test]
    fn a_nested_package_receives_the_root_and_its_own_instructions() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("proj");
        let config = root.join("config");
        write(&workspace.join("AGENTS.md"), "root rules\n");
        write(&workspace.join("pkg/nested/AGENTS.md"), "nested rules\n");

        let files = files_of(&workspace, &home, &config);
        let chain = resolve_for_target(&files, &workspace.join("pkg/nested/src/lib.rs"));

        assert_eq!(
            scopes(&chain),
            vec![
                workspace.to_string(),
                workspace.join("pkg/nested").to_string()
            ]
        );
        assert!(chain[1].content.contains("nested rules"));
    }

    #[test]
    fn a_call_outside_a_package_receives_only_the_root_instructions() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("proj");
        let config = root.join("config");
        write(&workspace.join("AGENTS.md"), "root rules\n");
        write(&workspace.join("pkg/nested/AGENTS.md"), "nested rules\n");

        let files = files_of(&workspace, &home, &config);
        let chain = resolve_for_target(&files, &workspace.join("other/main.rs"));

        assert_eq!(scopes(&chain), vec![workspace.to_string()]);
        assert_eq!(chain[0].content, "root rules\n");
    }

    #[test]
    fn a_target_directory_inside_a_package_receives_its_instructions() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("proj");
        let config = root.join("config");
        write(&workspace.join("AGENTS.md"), "root rules\n");
        write(&workspace.join("pkg/AGENTS.md"), "package rules\n");
        std::fs::create_dir_all(workspace.join("pkg/src")).expect("create dir");

        let files = files_of(&workspace, &home, &config);
        let chain = resolve_for_target(&files, &workspace.join("pkg/src"));

        assert_eq!(
            scopes(&chain),
            vec![workspace.to_string(), workspace.join("pkg").to_string()]
        );
    }

    #[test]
    fn the_user_file_applies_to_every_target_and_ranks_first() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("proj");
        let config = root.join("config");
        write(&config.join("AGENTS.md"), "user rules\n");
        write(&workspace.join("AGENTS.md"), "root rules\n");

        let files = files_of(&workspace, &home, &config);
        assert_eq!(files[0].path, config.join("AGENTS.md"));

        let chain = resolve_for_target(&files, &workspace.join("src/main.rs"));
        assert_eq!(chain[0].path, config.join("AGENTS.md"));
        assert_eq!(chain[1].path, workspace.join("AGENTS.md"));
        assert!(render(&chain).contains("user rules"));
    }

    #[test]
    fn the_upward_walk_stops_below_the_home_directory() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("work/proj");
        write(&home.join("AGENTS.md"), "home rules\n");
        write(&home.join("work/AGENTS.md"), "work rules\n");
        write(&workspace.join("AGENTS.md"), "root rules\n");

        let files = files_of(&workspace, &home, &root.join("config"));
        let paths: Vec<String> = files.iter().map(|file| file.path.to_string()).collect();

        assert!(paths.contains(&home.join("work/AGENTS.md").to_string()));
        assert!(!paths.contains(&home.join("AGENTS.md").to_string()));
    }

    #[test]
    fn an_ancestor_file_widens_a_target_chain() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("work/proj");
        write(&home.join("work/AGENTS.md"), "work rules\n");
        write(&workspace.join("AGENTS.md"), "root rules\n");

        let files = files_of(&workspace, &home, &root.join("config"));
        let chain = resolve_for_target(&files, &workspace.join("src/main.rs"));

        assert_eq!(
            scopes(&chain),
            vec![home.join("work").to_string(), workspace.to_string()]
        );
    }

    #[test]
    fn an_unresolvable_home_leaves_only_the_workspace_root() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        write(&workspace.join("AGENTS.md"), "root rules\n");
        write(&workspace.join("pkg/AGENTS.md"), "package rules\n");

        let files = Options::new(&workspace, None, &root.join("config"))
            .discover()
            .expect("discover");
        assert_eq!(files.len(), 2);

        let chain = resolve_for_target(&files, &workspace.join("other/main.rs"));
        assert_eq!(scopes(&chain), vec![workspace.to_string()]);
    }

    #[test]
    fn disabling_reading_produces_nothing() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        write(&workspace.join("AGENTS.md"), "root rules\n");

        let files = Options::new(&workspace, Some(&root), &root.join("config"))
            .disabled()
            .discover()
            .expect("discover");
        assert!(files.is_empty());
        assert!(render(&[]).is_empty());
    }

    #[test]
    fn an_oversized_file_is_truncated_with_a_marker_and_an_omission_record() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("proj");
        let path = workspace.join("AGENTS.md");
        let content = "rule line\n".repeat(20_000);
        assert!(content.len() > resolve_limit(LimitName::ProjectInstructionFileBytes));
        write(&path, &content);

        let files = files_of(&workspace, &home, &root.join("config"));
        assert_eq!(files.len(), 1);
        let rendered = render(&[&files[0]]);

        assert!(rendered.contains("<instructions-truncated"));
        assert!(rendered.contains(&format!(
            "reason=\"{}\"",
            LimitName::ProjectInstructionFileBytes.as_str()
        )));
        assert!(rendered.contains(&path.to_string()));
        assert!(rendered.len() <= resolve_limit(LimitName::ProjectInstructionsTotalBytes));
        assert!(!rendered.contains(&content));
    }

    #[test]
    fn an_oversized_file_is_read_within_its_bound_and_reports_its_true_size() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("proj");
        let path = workspace.join("AGENTS.md");
        let declared = resolve_limit(LimitName::ProjectInstructionFileBytes)
            .saturating_mul(4)
            .saturating_add(7);
        std::fs::create_dir_all(&workspace).expect("create dir");
        std::fs::write(&path, "a".repeat(declared)).expect("write");

        let files = files_of(&workspace, &home, &root.join("config"));
        let file = files.first().expect("one file");

        assert!(
            file.content.len()
                <= resolve_limit(LimitName::ProjectInstructionFileBytes).saturating_add(1)
        );
        assert_eq!(file.declared_bytes, declared as u64);

        let rendered = render(&[file]);
        assert!(rendered.contains(&format!("observed_bytes=\"{declared}\"")));
    }

    #[test]
    fn the_combined_cap_drops_wider_files_before_the_narrowest() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("proj");
        let wide = workspace.join("pkg/AGENTS.md");
        let narrow = workspace.join("pkg/nested/AGENTS.md");
        write(&workspace.join("AGENTS.md"), "root rules\n");
        write(&wide, &"wide rule\n".repeat(20_000));
        write(&narrow, &"narrow rule\n".repeat(20_000));

        let files = files_of(&workspace, &home, &root.join("config"));
        let chain = resolve_for_target(&files, &workspace.join("pkg/nested/src/lib.rs"));
        let rendered = render(&chain);

        assert!(rendered.len() <= resolve_limit(LimitName::ProjectInstructionsTotalBytes));
        assert!(rendered.contains("narrow rule"));
        assert!(rendered.contains(&format!(
            "reason=\"{}\"",
            LimitName::ProjectInstructionsTotalBytes.as_str()
        )));
        assert!(rendered.contains(&wide.to_string()));
        assert!(!rendered.contains(&"wide rule".repeat(2)));
    }

    #[test]
    fn sections_are_labelled_with_their_scope() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("proj");
        write(&workspace.join("AGENTS.md"), "root rules\n");

        let files = files_of(&workspace, &home, &root.join("config"));
        let rendered = render(&[&files[0]]);

        assert!(rendered.contains(&format!("scope=\"{workspace}\"")));
        assert!(rendered.contains(&format!("from=\"{}\"", workspace.join("AGENTS.md"))));
        assert!(rendered.contains("root rules"));
    }

    #[test]
    fn the_subtree_walk_is_bounded() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("proj");
        let first = workspace.join("d0000");
        let last = workspace.join("d1499");
        for index in 0..1500 {
            std::fs::create_dir_all(workspace.join(format!("d{index:04}"))).expect("create dir");
        }
        write(&first.join("AGENTS.md"), "first\n");
        write(&last.join("AGENTS.md"), "last\n");

        let files = files_of(&workspace, &home, &root.join("config"));
        let paths: Vec<String> = files.iter().map(|file| file.path.to_string()).collect();

        assert!(paths.contains(&first.join("AGENTS.md").to_string()));
        assert!(!paths.contains(&last.join("AGENTS.md").to_string()));
        assert!(files.len() <= resolve_limit(LimitName::ListEntries));
    }

    #[test]
    fn hidden_directories_are_not_scanned() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("proj");
        write(&workspace.join(".cache/AGENTS.md"), "cache rules\n");
        write(&workspace.join("AGENTS.md"), "root rules\n");

        let files = files_of(&workspace, &home, &root.join("config"));
        let paths: Vec<String> = files.iter().map(|file| file.path.to_string()).collect();

        assert_eq!(paths, vec![workspace.join("AGENTS.md").to_string()]);
    }

    #[test]
    fn an_unreadable_candidate_is_skipped_rather_than_failing() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("proj");
        std::fs::create_dir_all(workspace.join("AGENTS.md")).expect("create dir named like a file");
        write(&workspace.join("pkg/AGENTS.md"), "package rules\n");

        let files = files_of(&workspace, &home, &root.join("config"));
        let paths: Vec<String> = files.iter().map(|file| file.path.to_string()).collect();

        assert_eq!(paths, vec![workspace.join("pkg/AGENTS.md").to_string()]);
    }

    #[test]
    fn discovery_falls_back_to_the_process_configuration_root() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        write(&workspace.join("AGENTS.md"), "root rules\n");

        let files = discover(&workspace, None).expect("discover");
        assert!(
            files
                .iter()
                .any(|file| file.path == workspace.join("AGENTS.md"))
        );
    }
}
