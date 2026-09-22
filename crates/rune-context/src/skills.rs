//! Skill discovery and parsing.
//!
//! A skill is a directory holding a `SKILL.md` file. The file opens with a YAML
//! frontmatter block carrying a `name` and an optional `description`; everything
//! after the block is the skill body, which is loaded only when the skill is
//! used. Only those two fields are read, so a skill written for another tool
//! loads here even when its frontmatter carries fields this build does not know.
//!
//! Skills are discovered from the primary workspace upward to the home
//! directory, then from fixed user roots. A candidate that cannot be parsed is
//! skipped with a warning rather than failing the scan, so one broken skill
//! never hides its siblings. Names are not deduplicated: two roots may hold
//! different skills that share a name, and both are offered.
//!
//! A skill file that is a symlink is followed only when its resolved target
//! stays inside the root that declared it. A root is a directory the user
//! controls or the workspace contains; a link that leaves it would let content
//! outside the scanned tree enter the prompt under the root's authority.

use std::io::Read;

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

use rune_core::budget::LimitName;
use rune_core::error::Result;

use crate::instructions::below;
use crate::resolve_limit;

/// Name of the file that makes a directory a skill.
pub const SKILL_FILE: &str = "SKILL.md";

/// Largest `SKILL.md` prefix read while looking for frontmatter.
///
/// The metadata is at the top of the file, so a skill with a large body still
/// parses without reading the body. A block that does not close inside this
/// prefix is malformed.
const FRONTMATTER_MAX_BYTES: usize = 64 * 1024;

/// Workspace-relative directories holding skills, in precedence order.
pub const WORKSPACE_ROOTS: &[&str] = &[
    "skills",
    ".opencode/skills",
    ".codex/skills",
    ".claude/skills",
    ".agents/skills",
];

/// User-relative directories holding skills, in precedence order.
pub const USER_ROOTS: &[&str] = &[
    ".config/opencode/skills",
    ".codex/skills",
    ".claude/skills",
    ".agents/skills",
];

/// One discovered skill.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Skill {
    /// Name from the frontmatter, or the directory name when there is none.
    pub name: String,
    /// Description from the frontmatter, absent when it declares none.
    pub description: Option<String>,
    /// Path of the `SKILL.md` file.
    pub location: Utf8PathBuf,
    /// Root that declared this skill.
    pub root: Utf8PathBuf,
}

impl Skill {
    /// Returns the directory holding the skill.
    #[must_use]
    pub fn directory(&self) -> &Utf8Path {
        self.location.parent().unwrap_or(&self.location)
    }
}

/// A candidate that was refused, with enough detail to act on it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Warning {
    /// The file could not be read or parsed.
    Malformed {
        /// Path of the offending file.
        path: Utf8PathBuf,
        /// Why it was refused.
        detail: String,
    },
    /// A symlink resolved outside the root that declared it.
    EscapingSymlink {
        /// Path of the symlink.
        path: Utf8PathBuf,
        /// Resolved target, absent when it could not be resolved.
        target: Option<Utf8PathBuf>,
        /// Root the file had to stay inside.
        root: Utf8PathBuf,
    },
}

impl Warning {
    /// Returns the path the warning is about.
    #[must_use]
    pub fn path(&self) -> &Utf8Path {
        match self {
            Self::Malformed { path, .. } | Self::EscapingSymlink { path, .. } => path,
        }
    }
}

/// Outcome of one discovery pass.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Discovery {
    /// Skills that loaded, in discovery order.
    pub skills: Vec<Skill>,
    /// Candidates that were refused.
    pub warnings: Vec<Warning>,
}

/// Discovers every visible skill.
///
/// Warnings are reported by [`scan`]. This entry point is for callers that only
/// need the catalog, such as a listing command.
pub fn discover(
    workspace: &Utf8Path,
    home: Option<&Utf8Path>,
    config_root: &Utf8Path,
) -> Result<Vec<Skill>> {
    Ok(scan(workspace, home, config_root)?.skills)
}

/// Discovers every visible skill, reporting refused candidates.
pub fn scan(
    workspace: &Utf8Path,
    home: Option<&Utf8Path>,
    config_root: &Utf8Path,
) -> Result<Discovery> {
    let mut out = Discovery::default();
    for directory in walked_up(workspace, home) {
        for relative in WORKSPACE_ROOTS {
            scan_root(&directory.join(relative), &mut out);
        }
    }
    scan_root(&config_root.join("skills"), &mut out);
    if let Some(home) = home {
        for relative in USER_ROOTS {
            scan_root(&home.join(relative), &mut out);
        }
    }
    Ok(out)
}

/// Returns the workspace and its ancestors, stopping below the home directory.
fn walked_up(workspace: &Utf8Path, home: Option<&Utf8Path>) -> Vec<Utf8PathBuf> {
    let mut out = vec![workspace.to_owned()];
    let Some(home) = home else {
        return out;
    };
    let mut current = workspace.parent();
    while let Some(directory) = current {
        if !below(home, directory) {
            break;
        }
        out.push(directory.to_owned());
        current = directory.parent();
    }
    out
}

/// Scans one root for skill directories.
fn scan_root(root: &Utf8Path, out: &mut Discovery) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let cap = resolve_limit(LimitName::ListEntries);
    let mut candidates: Vec<Utf8PathBuf> = entries
        .flatten()
        .filter_map(|entry| Utf8PathBuf::from_path_buf(entry.path()).ok())
        .filter(|path| std::fs::metadata(path).is_ok_and(|meta| meta.is_dir()))
        .collect();
    candidates.sort();
    for directory in candidates.into_iter().take(cap) {
        let location = directory.join(SKILL_FILE);
        // A directory without a skill file is not a candidate at all, so it is
        // passed over silently rather than reported.
        if std::fs::symlink_metadata(&location).is_err() {
            continue;
        }
        match load(&directory, root) {
            Ok(skill) => out.skills.push(skill),
            Err(warning) => out.warnings.push(warning),
        }
    }
}

/// Loads one skill directory, refusing an unsafe or unparsable candidate.
///
/// Containment is checked on the directory and on the file, because either may
/// be the symlink: a linked directory escapes just as effectively as a linked
/// file, and the check must catch both.
fn load(directory: &Utf8Path, root: &Utf8Path) -> std::result::Result<Skill, Warning> {
    let location = directory.join(SKILL_FILE);
    let malformed = |detail: String| Warning::Malformed {
        path: location.clone(),
        detail,
    };

    let escape = |path: &Utf8Path| Warning::EscapingSymlink {
        path: path.to_owned(),
        target: std::fs::canonicalize(path)
            .ok()
            .as_deref()
            .and_then(Utf8Path::from_path)
            .map(Utf8Path::to_owned),
        root: root.to_owned(),
    };

    if is_symlink(directory) && !resolves_inside(directory, root) {
        return Err(escape(directory));
    }
    let link = std::fs::symlink_metadata(&location).map_err(|err| malformed(err.to_string()))?;
    if !link.is_file() && !link.is_symlink() {
        return Err(malformed("not a regular file".to_owned()));
    }
    if link.is_symlink() && !resolves_inside(&location, root) {
        return Err(escape(&location));
    }

    let text = read_prefix(&location).map_err(malformed)?;
    let frontmatter = Frontmatter::parse(&text).map_err(malformed)?;
    let fallback = directory.file_name().unwrap_or_default();
    Ok(Skill {
        name: frontmatter.name.unwrap_or_else(|| fallback.to_owned()),
        description: frontmatter.description,
        location,
        root: root.to_owned(),
    })
}

/// Returns true when a path is itself a symbolic link.
fn is_symlink(path: &Utf8Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink())
}

/// Returns true when a link resolves to a location inside `root`.
fn resolves_inside(path: &Utf8Path, root: &Utf8Path) -> bool {
    let Ok(resolved) = std::fs::canonicalize(path) else {
        return false;
    };
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    let (Some(resolved), Some(root)) = (Utf8Path::from_path(&resolved), Utf8Path::from_path(&root))
    else {
        return false;
    };
    resolved.starts_with(root)
}

/// Returns the `name` and `description` declared by a skill file.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct Frontmatter {
    /// Declared name, absent when the file has no frontmatter.
    name: Option<String>,
    /// Declared description.
    description: Option<String>,
}

impl Frontmatter {
    /// Parses the opening block, falling back to the directory name when absent.
    ///
    /// Recognized keys are read; every other key is skipped, including any
    /// indented block it introduces, so a skill carrying fields another tool
    /// defines still loads here.
    fn parse(text: &str) -> std::result::Result<Self, String> {
        let mut lines = text.lines().map(strip_carriage_return).peekable();
        if lines.next() != Some("---") {
            return Ok(Self::default());
        }
        let mut out = Self::default();
        loop {
            let Some(line) = lines.next() else {
                return Err("frontmatter is not closed".to_owned());
            };
            if line == "---" {
                break;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            let block = indicator(value);
            if !matches!(key, "name" | "description") {
                // An unrecognized key is skipped, including the indented block
                // it may introduce.
                if block.is_some() {
                    drain_block(&mut lines);
                }
                continue;
            }
            let value = match block {
                Some(style) => read_block(&mut lines, style),
                None => unquote(value),
            };
            match key {
                "name" => {
                    if out.name.is_some() {
                        return Err("name is declared twice".to_owned());
                    }
                    if value.is_empty() {
                        return Err("name is empty".to_owned());
                    }
                    out.name = Some(value);
                }
                _ => {
                    if !value.is_empty() {
                        out.description = Some(value);
                    }
                }
            }
        }
        if out.name.is_none() {
            return Err("frontmatter declares no name".to_owned());
        }
        Ok(out)
    }
}

/// How a block scalar's line breaks are rendered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Block {
    /// Lines joined with a space, keeping the trailing newline.
    Folded,
    /// Lines joined with a space, dropping the trailing newline.
    FoldedStrip,
    /// Lines kept as written, keeping the trailing newline.
    Literal,
    /// Lines kept as written, dropping the trailing newline.
    LiteralStrip,
}

impl Block {
    /// Renders the collected body lines.
    fn render(self, lines: &[&str]) -> String {
        let indent = lines
            .iter()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.len().saturating_sub(line.trim_start().len()))
            .min()
            .unwrap_or(0);
        let stripped: Vec<&str> = lines
            .iter()
            .map(|line| line.get(indent..).unwrap_or_default())
            .collect();
        let separator = match self {
            Self::Folded | Self::FoldedStrip => " ",
            Self::Literal | Self::LiteralStrip => "\n",
        };
        let mut joined = stripped.join(separator);
        while joined.ends_with('\n') || joined.ends_with(' ') {
            joined.pop();
        }
        match self {
            Self::FoldedStrip | Self::LiteralStrip => {}
            Self::Folded | Self::Literal => joined.push('\n'),
        }
        joined
    }
}

/// Returns the block style a value introduces, if it introduces one.
fn indicator(value: &str) -> Option<Block> {
    match value {
        ">" => Some(Block::Folded),
        ">-" => Some(Block::FoldedStrip),
        "|" => Some(Block::Literal),
        "|-" => Some(Block::LiteralStrip),
        _ => None,
    }
}

/// Drains the indented lines that belong to a block scalar.
///
/// The line that ends the block is left in place, so the caller still sees it.
fn drain_block<'a, I>(lines: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = &'a str>,
{
    while lines.peek().is_some_and(|line| is_continuation(line)) {
        lines.next();
    }
}

/// Reads a block scalar body and renders it.
fn read_block<'a, I>(lines: &mut std::iter::Peekable<I>, style: Block) -> String
where
    I: Iterator<Item = &'a str>,
{
    let mut body = Vec::new();
    while lines.peek().is_some_and(|line| is_continuation(line)) {
        if let Some(line) = lines.next() {
            body.push(line);
        }
    }
    style.render(&body)
}

/// Returns true when a line belongs to a block scalar's body.
///
/// A scalar body is indented; the delimiter that closes the frontmatter is not,
/// so the block always stops before it.
fn is_continuation(line: &str) -> bool {
    line.starts_with(' ') || line.is_empty()
}

/// Removes one trailing carriage return, so CRLF files parse as LF files.
fn strip_carriage_return(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// Removes one matching pair of outer quotes.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    let quoted = bytes.len() >= 2
        && matches!(bytes.first(), Some(b'"' | b'\''))
        && bytes.first() == bytes.last();
    match (quoted, value.get(1..value.len().saturating_sub(1))) {
        (true, Some(inner)) => inner.to_owned(),
        _ => value.to_owned(),
    }
}

/// Reads a bounded prefix of a file as text.
///
/// A file longer than the bound is cut, and a trailing partial character is
/// dropped rather than reported: the bound exists to keep the read cheap, not to
/// reject the file. Short files are decoded strictly, so invalid UTF-8 is an
/// error rather than silently shortened text.
fn read_prefix(path: &Utf8Path) -> std::result::Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|err| err.to_string())?;
    let bound = u64::try_from(FRONTMATTER_MAX_BYTES).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    file.by_ref()
        .take(bound.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|err| err.to_string())?;
    if bytes.len() <= FRONTMATTER_MAX_BYTES {
        return String::from_utf8(bytes).map_err(|_| "file is not utf-8".to_owned());
    }
    let valid = match std::str::from_utf8(&bytes) {
        Ok(text) => text.len(),
        Err(err) => err.valid_up_to(),
    };
    bytes
        .get(..valid)
        .and_then(|head| std::str::from_utf8(head).ok())
        .map(str::to_owned)
        .ok_or_else(|| "file is not utf-8".to_owned())
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

    /// Adds a skill directory under a root.
    fn skill(root: &Utf8Path, name: &str, frontmatter: &str) {
        write(&root.join(name).join(SKILL_FILE), frontmatter);
    }

    /// Scans a workspace with an explicit home and configuration root.
    fn scan_of(workspace: &Utf8Path, home: &Utf8Path, config_root: &Utf8Path) -> Discovery {
        scan(workspace, Some(home), config_root).expect("scan")
    }

    /// Returns the names of every discovered skill.
    fn names(discovery: &Discovery) -> Vec<String> {
        discovery
            .skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect()
    }

    #[test]
    fn frontmatter_name_and_scalar_description_are_read() {
        let parsed = Frontmatter::parse("---\nname: alpha\ndescription: does things\n---\nbody\n")
            .expect("parse");
        assert_eq!(parsed.name.as_deref(), Some("alpha"));
        assert_eq!(parsed.description.as_deref(), Some("does things"));
    }

    #[test]
    fn a_folded_description_is_one_line() {
        let parsed = Frontmatter::parse(
            "---\nname: alpha\ndescription: >-\n  first part\n  second part\n---\n",
        )
        .expect("parse");
        assert_eq!(
            parsed.description.as_deref(),
            Some("first part second part")
        );
    }

    #[test]
    fn a_folded_description_keeps_its_trailing_newline_by_default() {
        let parsed =
            Frontmatter::parse("---\nname: alpha\ndescription: >\n  text\n---\n").expect("parse");
        assert_eq!(parsed.description.as_deref(), Some("text\n"));
    }

    #[test]
    fn a_literal_description_keeps_its_lines() {
        let parsed = Frontmatter::parse(
            "---\nname: alpha\ndescription: |\n  first\n    indented\n  last\n---\n",
        )
        .expect("parse");
        assert_eq!(
            parsed.description.as_deref(),
            Some("first\n  indented\nlast\n")
        );
    }

    #[test]
    fn quoted_values_lose_one_pair_of_quotes() {
        let parsed =
            Frontmatter::parse("---\nname: \"alpha\"\ndescription: 'plain'\n---\n").expect("parse");
        assert_eq!(parsed.name.as_deref(), Some("alpha"));
        assert_eq!(parsed.description.as_deref(), Some("plain"));
    }

    #[test]
    fn extra_frontmatter_fields_are_ignored() {
        let parsed = Frontmatter::parse(
            "---\nname: alpha\nallowed-tools: Read, Grep\nmetadata: >\n  folded text\n  more text\ndescription: kept\n---\n",
        )
        .expect("parse");
        assert_eq!(parsed.name.as_deref(), Some("alpha"));
        assert_eq!(parsed.description.as_deref(), Some("kept"));
    }

    #[test]
    fn a_file_without_frontmatter_falls_back_to_the_directory_name() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        let home = root.join("home");
        skill(&workspace.join("skills"), "pdf-tools", "just a body\n");

        let discovery = scan_of(&workspace, &home, &root.join("config"));
        assert_eq!(names(&discovery), vec!["pdf-tools"]);
        assert_eq!(discovery.skills[0].description, None);
    }

    #[test]
    fn frontmatter_without_a_name_is_refused() {
        assert!(Frontmatter::parse("---\ndescription: text\n---\n").is_err());
        assert!(Frontmatter::parse("---\nname:\n---\n").is_err());
        assert!(Frontmatter::parse("---\nname: alpha\n").is_err());
    }

    #[test]
    fn a_malformed_sibling_does_not_stop_the_other_skills() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        let home = root.join("home");
        let skills = workspace.join("skills");
        skill(&skills, "alpha", "---\nname: alpha\n---\n");
        skill(&skills, "broken", "---\ndescription: no name\n---\n");
        skill(&skills, "beta", "---\nname: beta\n---\n");

        let discovery = scan_of(&workspace, &home, &root.join("config"));
        assert_eq!(names(&discovery), vec!["alpha", "beta"]);
        assert_eq!(discovery.warnings.len(), 1);
        assert_eq!(
            discovery.warnings[0].path(),
            skills.join("broken").join(SKILL_FILE)
        );
    }

    #[test]
    fn every_workspace_root_is_scanned_in_order() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        let home = root.join("home");
        for (index, relative) in WORKSPACE_ROOTS.iter().enumerate() {
            skill(
                &workspace.join(relative),
                &format!("s{index}"),
                &format!("---\nname: s{index}\n---\n"),
            );
        }

        let discovery = scan_of(&workspace, &home, &root.join("config"));
        assert_eq!(
            names(&discovery),
            vec!["s0", "s1", "s2", "s3", "s4"],
            "roots are scanned in declared order"
        );
    }

    #[test]
    fn an_ancestor_of_the_workspace_contributes_below_the_home_directory() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("work/proj");
        skill(
            &home.join("work/skills"),
            "outer",
            "---\nname: outer\n---\n",
        );
        skill(&home.join("skills"), "at-home", "---\nname: at-home\n---\n");
        skill(
            &workspace.join("skills"),
            "inner",
            "---\nname: inner\n---\n",
        );

        let discovery = scan_of(&workspace, &home, &root.join("config"));
        assert_eq!(names(&discovery), vec!["inner", "outer"]);
    }

    #[test]
    fn user_roots_contribute_after_the_workspace() {
        let (_dir, root) = tree();
        let home = root.join("home");
        let workspace = home.join("proj");
        let config = root.join("config");
        skill(
            &workspace.join("skills"),
            "project",
            "---\nname: project\n---\n",
        );
        skill(
            &config.join("skills"),
            "managed",
            "---\nname: managed\n---\n",
        );
        skill(
            &home.join(".config/opencode/skills"),
            "opencode",
            "---\nname: opencode\n---\n",
        );
        skill(
            &home.join(".claude/skills"),
            "claude",
            "---\nname: claude\n---\n",
        );

        let discovery = scan_of(&workspace, &home, &config);
        assert_eq!(
            names(&discovery),
            vec!["project", "managed", "opencode", "claude"]
        );
        assert_eq!(discovery.skills[1].root, config.join("skills"));
    }

    #[test]
    fn duplicate_names_are_preserved() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        let home = root.join("home");
        skill(&workspace.join("skills"), "one", "---\nname: shared\n---\n");
        skill(
            &workspace.join(".claude/skills"),
            "two",
            "---\nname: shared\n---\n",
        );

        let discovery = scan_of(&workspace, &home, &root.join("config"));
        assert_eq!(names(&discovery), vec!["shared", "shared"]);
        assert_ne!(
            discovery.skills[0].location, discovery.skills[1].location,
            "the entries stay distinct"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_skill_file_inside_its_root_loads() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        let home = root.join("home");
        let skills = workspace.join("skills");
        write(&skills.join("shared/SKILL.md"), "---\nname: shared\n---\n");
        std::fs::create_dir_all(skills.join("link")).expect("create dir");
        std::os::unix::fs::symlink(skills.join("shared/SKILL.md"), skills.join("link/SKILL.md"))
            .expect("symlink");

        let discovery = scan_of(&workspace, &home, &root.join("config"));
        assert_eq!(names(&discovery), vec!["shared", "shared"]);
        assert!(discovery.warnings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_escaping_its_root_is_refused_with_a_warning() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        let home = root.join("home");
        let skills = workspace.join("skills");
        let outside = root.join("outside/SKILL.md");
        write(&outside, "---\nname: outside\n---\n");
        std::fs::create_dir_all(skills.join("leak")).expect("create dir");
        std::os::unix::fs::symlink(&outside, skills.join("leak/SKILL.md")).expect("symlink");
        skill(&skills, "alpha", "---\nname: alpha\n---\n");

        let discovery = scan_of(&workspace, &home, &root.join("config"));
        assert_eq!(names(&discovery), vec!["alpha"]);
        assert_eq!(discovery.warnings.len(), 1);
        match &discovery.warnings[0] {
            Warning::EscapingSymlink { path, target, root } => {
                assert_eq!(path, &skills.join("leak/SKILL.md"));
                assert_eq!(
                    target.as_deref(),
                    std::fs::canonicalize(&outside)
                        .ok()
                        .as_deref()
                        .and_then(Utf8Path::from_path),
                    "the warning names where the link actually points"
                );
                assert!(!target.as_ref().is_some_and(|t| t.starts_with(&skills)));
                assert_eq!(root, &skills);
            }
            other @ Warning::Malformed { .. } => {
                panic!("expected an escaping symlink warning, got {other:?}")
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_symlink_escaping_the_root_is_refused() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        let home = root.join("home");
        let outside = root.join("outside/my-skill");
        write(&outside.join(SKILL_FILE), "---\nname: outside\n---\n");
        let skills = workspace.join("skills");
        std::fs::create_dir_all(&skills).expect("create dir");
        std::os::unix::fs::symlink(&outside, skills.join("leak")).expect("symlink");

        let discovery = scan_of(&workspace, &home, &root.join("config"));
        assert!(discovery.skills.is_empty());
        assert!(matches!(
            discovery.warnings.first(),
            Some(Warning::EscapingSymlink { .. })
        ));
    }

    #[test]
    fn a_missing_root_is_skipped() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        std::fs::create_dir_all(&workspace).expect("create dir");

        let discovery = scan_of(&workspace, &root.join("home"), &root.join("config"));
        assert!(discovery.skills.is_empty());
        assert!(discovery.warnings.is_empty());
    }

    #[test]
    fn a_directory_without_a_skill_file_is_not_a_candidate() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        write(&workspace.join("skills/notes/README.md"), "notes\n");

        let discovery = scan_of(&workspace, &root.join("home"), &root.join("config"));
        assert!(discovery.skills.is_empty());
        assert!(discovery.warnings.is_empty());
    }

    #[test]
    fn non_utf8_content_is_refused_with_a_warning() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        let path = workspace.join("skills/raw").join(SKILL_FILE);
        std::fs::create_dir_all(workspace.join("skills/raw")).expect("create dir");
        std::fs::write(&path, [0x2d, 0x2d, 0x2d, 0xff, 0xfe, 0x0a]).expect("write");

        let discovery = scan_of(&workspace, &root.join("home"), &root.join("config"));
        assert!(discovery.skills.is_empty());
        assert!(matches!(
            discovery.warnings.first(),
            Some(Warning::Malformed { .. })
        ));
    }

    #[test]
    fn a_large_body_still_parses_its_frontmatter() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        let body = "x".repeat(FRONTMATTER_MAX_BYTES.saturating_mul(2));
        skill(
            &workspace.join("skills"),
            "big",
            &format!("---\nname: big\ndescription: large\n---\n{body}\n"),
        );

        let discovery = scan_of(&workspace, &root.join("home"), &root.join("config"));
        assert_eq!(names(&discovery), vec!["big"]);
        assert_eq!(discovery.skills[0].description.as_deref(), Some("large"));
    }

    #[test]
    fn carriage_returns_are_accepted() {
        let (_dir, root) = tree();
        let workspace = root.join("proj");
        skill(
            &workspace.join("skills"),
            "windows",
            "---\r\nname: windows\r\ndescription: from windows\r\n---\r\nbody\r\n",
        );

        let discovery = scan_of(&workspace, &root.join("home"), &root.join("config"));
        assert_eq!(names(&discovery), vec!["windows"]);
        assert_eq!(
            discovery.skills[0].description.as_deref(),
            Some("from windows")
        );
    }
}
