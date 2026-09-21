//! Path resolution and the one ignore policy every file tool walks with.
//!
//! A tool never decides where it may operate. The caller resolves policy and
//! hands the tool a context carrying the roots and whether external access was
//! granted. This module turns a raw argument into the absolute path to use, and
//! reports the single case a tool can detect for itself: a path that lands
//! outside every permitted root.

use std::ffi::OsStr;
use std::fmt::Write as _;

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::{Walk, WalkBuilder};
use rune_core::LimitName;
use rune_core::budget::BudgetSet;
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::contract::ExecutionContext;

/// A path resolved against the permitted roots.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ResolvedPath {
    /// Absolute path with `.` and `..` removed lexically.
    pub path: Utf8PathBuf,
    /// True when the path lies outside every permitted root.
    pub external: bool,
}

/// Files one walk visits for each entry the caller may list.
///
/// A traversal cap has to be larger than a listing cap: an exact count must see
/// every file even when the listing it produces is truncated.
pub const WALK_ENTRY_FACTOR: usize = 64;

/// Largest number of files one walk visits, whatever the listing cap.
pub const MAX_WALK_FILES: usize = 1_000_000;

/// Line length the search tools display before truncating.
pub const MAX_MATCH_LINE_BYTES: usize = 1024 * 1024;

/// Floor applied to a resolved byte cap, so a footer always fits.
pub const MIN_OUTPUT_BYTES: usize = 1024;

/// The caps a file tool applies, resolved once from the limit set.
///
/// Every bound a tool enforces comes from here, so `rune limits` reports the
/// number that actually governs the run rather than a compiled constant.
#[derive(Clone, Copy, Debug)]
pub struct FileLimits {
    /// Lines returned by one read.
    pub read_lines: usize,
    /// Bytes kept from one line.
    pub line_bytes: usize,
    /// Entries returned by one listing.
    pub list_entries: usize,
    /// Files one walk visits.
    pub walk_files: usize,
    /// Bytes one result may occupy.
    pub output_bytes: usize,
}

impl FileLimits {
    /// Resolves every cap from a limit set.
    #[must_use]
    pub fn from_budget(budget: &BudgetSet) -> Self {
        let list_entries = budget.get_usize(LimitName::ListEntries);
        Self {
            read_lines: budget.get_usize(LimitName::ReadFileLines),
            line_bytes: budget.get_usize(LimitName::ReadFileLineBytes),
            list_entries,
            walk_files: list_entries
                .saturating_mul(WALK_ENTRY_FACTOR)
                .min(MAX_WALK_FILES),
            output_bytes: budget.get_usize(LimitName::CommandOutputBytes),
        }
    }

    /// Returns the byte budget for one call.
    ///
    /// The context carries the cap resolved for this call; the limit set holds
    /// the configured cap. The smaller of the two wins, so neither widens the
    /// other, and neither can cut a result below the floor that keeps its
    /// summary readable.
    #[must_use]
    pub fn output_cap(&self, context: &ExecutionContext) -> usize {
        context
            .max_output_bytes
            .min(self.output_bytes)
            .max(MIN_OUTPUT_BYTES)
    }
}

impl Default for FileLimits {
    fn default() -> Self {
        Self::from_budget(&BudgetSet::new())
    }
}

/// Resolves an argument into the absolute path a tool should use.
///
/// Accepts a workspace-relative path, an absolute path, `~` for the home
/// directory, and a relative escape such as `../sibling`. A relative path is
/// resolved against the primary workspace root, never against the process
/// working directory, so a call means the same thing in every process.
pub fn resolve(context: &ExecutionContext, raw: &str) -> Result<ResolvedPath> {
    if raw.is_empty() {
        return Err(RuneError::invalid_field("path", "path is empty"));
    }
    if raw.contains('\0') {
        return Err(RuneError::invalid_field("path", "path contains a NUL byte")
            .with_observed(raw.escape_debug().to_string()));
    }
    let candidate = if let Some(rest) = raw.strip_prefix("~/") {
        home()?.join(rest)
    } else if raw == "~" {
        home()?
    } else if raw.starts_with('~') {
        return Err(RuneError::invalid_field(
            "path",
            "only `~` and `~/` expand to the home directory",
        ));
    } else {
        Utf8PathBuf::from(raw)
    };

    let path = if candidate.is_absolute() {
        normalize(&candidate)
    } else {
        normalize(&absolute(context.workspace()).join(candidate))
    };
    let permitted = roots(context);
    if permitted.iter().any(|root| contains(root, &path)) {
        return Ok(ResolvedPath {
            path,
            external: false,
        });
    }
    if context.external_access {
        return Ok(ResolvedPath {
            path,
            external: true,
        });
    }

    let names = permitted
        .iter()
        .map(|root| root.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(RuneError::new(
        ErrorCode::PathOutsideWorkspace,
        format!("`{raw}` resolves to `{path}`, outside every permitted root"),
    )
    .with_observed(raw)
    .with_invariant("path_inside_roots")
    .with_hint(format!("permitted roots are {names}")))
}

/// Returns the primary workspace root as a resolved path.
pub fn default_root(context: &ExecutionContext) -> Result<ResolvedPath> {
    Ok(ResolvedPath {
        path: absolute(context.workspace()),
        external: false,
    })
}

/// Returns the permitted roots in absolute, normalised form, primary first.
#[must_use]
pub fn roots(context: &ExecutionContext) -> Vec<Utf8PathBuf> {
    context.roots().into_iter().map(absolute).collect()
}

/// Returns a path as the model should see it, given the normalised roots.
///
/// A path inside a root is shown relative to it, which keeps the output of a
/// tool independent of where the workspace happens to live.
#[must_use]
pub fn display_in(roots: &[Utf8PathBuf], path: &Utf8Path) -> String {
    for root in roots {
        if let Ok(relative) = path.strip_prefix(root) {
            return if relative.as_str().is_empty() {
                String::from(".")
            } else {
                relative.as_str().to_owned()
            };
        }
    }
    path.as_str().to_owned()
}

/// Returns a path as the model should see it.
#[must_use]
pub fn display_path(context: &ExecutionContext, path: &Utf8Path) -> String {
    display_in(&roots(context), path)
}

/// Returns an optional string argument.
pub fn string_arg<'a>(arguments: &'a serde_json::Value, name: &str) -> Result<Option<&'a str>> {
    match arguments.get(name) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| RuneError::invalid_field(name, format!("`{name}` must be a string"))),
    }
}

/// Returns an optional non-negative integer argument.
pub fn usize_arg(arguments: &serde_json::Value, name: &str) -> Result<Option<usize>> {
    match arguments.get(name) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|raw| usize::try_from(raw).ok())
            .map(Some)
            .ok_or_else(|| {
                RuneError::invalid_field(name, format!("`{name}` must be a non-negative integer"))
            }),
    }
}

/// Returns an optional boolean argument.
pub fn bool_arg(arguments: &serde_json::Value, name: &str) -> Result<Option<bool>> {
    match arguments.get(name) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| RuneError::invalid_field(name, format!("`{name}` must be a boolean"))),
    }
}

/// Returns the longest prefix of `text` that fits in `limit` bytes.
#[must_use]
pub fn truncate_to_bytes(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &text[..end]
}

/// Returns one line of file content, cut to `limit` bytes with a marker.
#[must_use]
pub fn truncate_line(line: &str, limit: usize) -> String {
    if line.len() <= limit {
        return line.to_owned();
    }
    let head = truncate_to_bytes(line, limit);
    format!(
        "{head}... [line truncated, the line is {bytes} bytes]",
        bytes = line.len()
    )
}

/// Appends a footer to a body, cutting the body when the pair exceeds `cap`.
///
/// The footer carries the true counts, so it survives the cut even when the
/// body does not.
#[must_use]
pub fn join_capped(body: String, footer: &str, cap: usize) -> String {
    let marker = "\n[output truncated at the byte cap]";
    let mut footer = footer.to_owned();
    // The footer carries the true counts, so it is kept whole in preference to
    // the body; only a footer that cannot fit at all is cut.
    if footer.len().saturating_add(marker.len()) >= cap {
        let room = cap.saturating_sub(marker.len());
        let end = truncate_to_bytes(&footer, room).len();
        footer.truncate(end);
    }

    let mut out = body;
    if out.len().saturating_add(footer.len()) > cap {
        let room = cap.saturating_sub(footer.len().saturating_add(marker.len()));
        let end = truncate_to_bytes(&out, room).len();
        out.truncate(end);
        out.push_str(marker);
    }
    out.push_str(&footer);
    out
}

/// Returns at most `max` names, with a count of the rest.
#[must_use]
pub fn summarize(names: &[String], max: usize) -> String {
    if names.len() <= max {
        return names.join(", ");
    }
    let shown = names
        .iter()
        .take(max)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{shown} and {} more", names.len().saturating_sub(max))
}

/// Compiles a glob for matching paths relative to a walk root.
///
/// `*` and `?` never cross a path separator, and a pattern that names no
/// separator also matches a file name at any depth, which is what `*.rs` is
/// read to mean.
pub fn compile_glob(pattern: &str) -> Result<GlobSet> {
    if pattern.is_empty() {
        return Err(RuneError::invalid_field("pattern", "pattern is empty"));
    }
    let mut builder = GlobSetBuilder::new();
    let _ = builder.add(build_glob(pattern)?);
    if !pattern.contains('/') {
        let _ = builder.add(build_glob(&format!("**/{pattern}"))?);
    }
    builder.build().map_err(|err| {
        RuneError::invalid_field("pattern", format!("invalid glob `{pattern}`: {err}"))
    })
}

/// Builds one glob with the path separator treated literally.
fn build_glob(pattern: &str) -> Result<Glob> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|err| {
            RuneError::invalid_field("pattern", format!("invalid glob `{pattern}`: {err}"))
        })
}

/// Notes produced by a finished walk, for the result footer.
#[must_use]
pub fn walk_notes(walker: &Walker) -> String {
    let mut out = String::new();
    if walker.stopped() {
        let _ = writeln!(
            out,
            "[the walk stopped after {} files; later files were not searched]",
            walker.visited()
        );
    }
    let unreadable = walker.unreadable();
    if unreadable > 0 {
        let _ = writeln!(out, "[{unreadable} entries could not be read]");
    }
    out
}

/// One entry produced by a walk.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Walked {
    /// Absolute path.
    pub path: Utf8PathBuf,
    /// Path relative to the walk root, which is what a glob matches against.
    pub relative: Utf8PathBuf,
}

/// The one walker every file tool uses.
///
/// The policy is fixed here rather than at each call site: `.gitignore` files
/// inside the tree are honoured even when the tree is not a git repository,
/// global git configuration is ignored so a result depends only on the
/// workspace, `.git` is skipped at every depth, symbolic links are never
/// followed, and traversal stops after the entry cap instead of quietly
/// returning fewer files.
pub struct Walker {
    root: Utf8PathBuf,
    walk: Option<Walk>,
    single: Option<Utf8PathBuf>,
    is_file: bool,
    limit: usize,
    visited: usize,
    stopped: bool,
    unreadable: usize,
}

impl std::fmt::Debug for Walker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Walker")
            .field("root", &self.root)
            .field("visited", &self.visited)
            .field("stopped", &self.stopped)
            .finish()
    }
}

impl Walker {
    /// Builds a walker over a file or a directory.
    ///
    /// A directory is walked recursively; a file is the only entry, because a
    /// caller that names a file has already said what it wants.
    pub fn new(root: &Utf8Path, limit: usize) -> Result<Self> {
        let metadata = std::fs::metadata(root).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => {
                RuneError::new(ErrorCode::NotFound, format!("`{root}` does not exist"))
            }
            _ => RuneError::new(
                ErrorCode::Unsupported,
                format!("`{root}` could not be opened: {err}"),
            ),
        })?;

        if metadata.is_file() {
            return Ok(Self {
                root: root.to_owned(),
                walk: None,
                single: Some(root.to_owned()),
                is_file: true,
                limit,
                visited: 0,
                stopped: false,
                unreadable: 0,
            });
        }
        if !metadata.is_dir() {
            return Err(RuneError::invalid_field(
                "path",
                format!("`{root}` is neither a file nor a directory"),
            ));
        }

        let mut builder = WalkBuilder::new(root);
        let _ = builder
            .hidden(false)
            .parents(true)
            .git_global(false)
            .require_git(false)
            .follow_links(false)
            .sort_by_file_path(|a, b| a.cmp(b))
            .filter_entry(|entry| entry.file_name() != OsStr::new(".git"));

        Ok(Self {
            root: root.to_owned(),
            walk: Some(builder.build()),
            single: None,
            is_file: false,
            limit,
            visited: 0,
            stopped: false,
            unreadable: 0,
        })
    }

    /// Returns the root the walk started at.
    #[must_use]
    pub fn root(&self) -> &Utf8Path {
        self.root.as_path()
    }

    /// Returns the number of entries produced, whether or not they were used.
    #[must_use]
    pub const fn visited(&self) -> usize {
        self.visited
    }

    /// Returns true when the walk stopped at the entry cap.
    #[must_use]
    pub const fn stopped(&self) -> bool {
        self.stopped
    }

    /// Returns the number of entries that could not be read.
    #[must_use]
    pub const fn unreadable(&self) -> usize {
        self.unreadable
    }

    /// Returns the relative form of a walked path.
    ///
    /// A file named directly has no root to be relative to, so its own name is
    /// what a pattern matches against.
    fn relative(&self, path: &Utf8Path) -> Utf8PathBuf {
        if self.is_file {
            return path
                .file_name()
                .map_or_else(|| path.to_owned(), Utf8PathBuf::from);
        }
        path.strip_prefix(&self.root).unwrap_or(path).to_owned()
    }
}

impl Iterator for Walker {
    type Item = Walked;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(single) = self.single.take() {
            if self.limit == 0 {
                self.stopped = true;
                return None;
            }
            self.visited = self.visited.saturating_add(1);
            return Some(Walked {
                relative: self.relative(&single),
                path: single,
            });
        }

        let walk = self.walk.as_mut()?;
        loop {
            let entry = match walk.next() {
                Some(Ok(entry)) => entry,
                Some(Err(_)) => {
                    self.unreadable = self.unreadable.saturating_add(1);
                    continue;
                }
                None => return None,
            };
            if entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if self.visited >= self.limit {
                self.stopped = true;
                return None;
            }
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.into_path()) else {
                self.unreadable = self.unreadable.saturating_add(1);
                continue;
            };
            self.visited = self.visited.saturating_add(1);
            let relative = self.relative(&path);
            return Some(Walked { path, relative });
        }
    }
}

/// Returns the absolute, normalised form of a path.
fn absolute(path: &Utf8Path) -> Utf8PathBuf {
    if path.is_absolute() {
        return normalize(path);
    }
    match std::env::current_dir() {
        Ok(dir) => match Utf8PathBuf::from_path_buf(dir) {
            Ok(dir) => normalize(&dir.join(path)),
            Err(_) => normalize(path),
        },
        Err(_) => normalize(path),
    }
}

/// Removes `.` and `..` components without touching the filesystem.
///
/// A `..` above the root is dropped, so the result is the path the file system
/// would use and an escape can be detected without a stat call.
fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut out = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::Prefix(prefix) => out.push(prefix.as_str()),
            Utf8Component::RootDir => out.push("/"),
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                let _ = out.pop();
            }
            Utf8Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Returns true when `path` equals `root` or lies below it.
fn contains(root: &Utf8Path, path: &Utf8Path) -> bool {
    path == root || path.starts_with(root)
}

/// Returns the home directory.
fn home() -> Result<Utf8PathBuf> {
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
        {
            return Ok(Utf8PathBuf::from(value));
        }
    }
    Err(RuneError::new(
        ErrorCode::Unsupported,
        "the home directory is not set, so `~` cannot be expanded",
    ))
}

#[cfg(test)]
pub(crate) mod fixture {
    use camino::{Utf8Path, Utf8PathBuf};
    use rune_core::LimitName;
    use rune_core::budget::{Budget, BudgetSet};
    use rune_core::config::Layer;

    use crate::contract::ExecutionContext;

    /// Lines the fixture writes into every counted file.
    pub const MATCH_LINES: usize = 9;
    /// Files the fixture seeds that contain a match.
    pub const MATCH_FILES: usize = 7;

    /// A temporary repository shared by the file tool tests.
    pub struct Repo {
        dir: tempfile::TempDir,
    }

    impl Repo {
        /// Builds the fixture tree.
        pub fn new() -> Self {
            let repo = Self {
                dir: tempfile::tempdir().expect("temp dir"),
            };
            repo.write(".gitignore", "build/\ntarget/\n");
            repo.write(".git/HEAD", "ref: refs/heads/main\n");
            repo.write("README.md", "Rune fixture\nneedle one\n");
            repo.write("src/main.rs", "fn main() {}\nlet needle = 1;\n");
            repo.write("src/nested/deep/mod.rs", "needle nested\n");
            repo.write("nested/untracked.txt", "untracked needle\n");
            repo.write("build/ignored.txt", "needle in an ignored directory\n");
            repo.write(
                "target/ignored/deep.txt",
                "needle in a nested ignored directory\n",
            );
            repo.write("crlf/win.txt", "alpha\r\nbeta\r\nneedle crlf\r\n");
            repo.write("empty.txt", "");
            repo.write(
                "counts/known.txt",
                "alpha needle\nbeta\nneedle needle\ngamma\ndelta needle\n",
            );

            let mut long = String::from("needle ");
            long.push_str(&"x".repeat(5000));
            long.push('\n');
            repo.write("long/line.txt", &long);
            repo.write_bytes("data/blob.bin", &binary());
            repo
        }

        /// Returns the repository root.
        pub fn path(&self) -> &Utf8Path {
            Utf8Path::from_path(self.dir.path()).expect("a UTF-8 temporary path")
        }

        /// Returns an execution context rooted at the repository.
        pub fn context(&self) -> ExecutionContext {
            ExecutionContext::new(self.path().to_owned())
        }

        /// Writes a text file, creating parent directories.
        pub fn write(&self, relative: &str, contents: &str) {
            self.write_bytes(relative, contents.as_bytes());
        }

        /// Writes bytes, creating parent directories.
        pub fn write_bytes(&self, relative: &str, contents: &[u8]) {
            let path = Utf8PathBuf::from(self.path()).join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create a parent directory");
            }
            std::fs::write(path, contents).expect("write a fixture file");
        }
    }

    /// Returns the bytes of a file that is not text.
    pub fn binary() -> Vec<u8> {
        let mut bytes = vec![0x00, 0x01, 0x02, 0x03];
        bytes.extend_from_slice(b"needle in a binary file\n");
        bytes.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x10]);
        bytes
    }

    /// Returns a limit set with a lowered listing cap.
    pub fn budget_with_list_entries(entries: usize) -> BudgetSet {
        let mut budget = BudgetSet::new();
        budget
            .set(
                LimitName::ListEntries,
                Budget::Bounded(entries as u64),
                Layer::CommandLine,
            )
            .expect("a valid limit");
        budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::fixture::Repo;

    #[test]
    fn a_workspace_relative_path_resolves_against_the_root() {
        let repo = Repo::new();
        let context = repo.context();
        let resolved = resolve(&context, "src/main.rs").expect("resolve");
        assert_eq!(resolved.path, repo.path().join("src/main.rs"));
        assert!(!resolved.external);
    }

    #[test]
    fn an_absolute_path_inside_the_root_is_not_external() {
        let repo = Repo::new();
        let context = repo.context();
        let raw = repo.path().join("README.md");
        let resolved = resolve(&context, raw.as_str()).expect("resolve");
        assert_eq!(resolved.path, raw);
        assert!(!resolved.external);
    }

    #[test]
    fn a_path_escaping_every_root_is_refused_and_names_them() {
        let repo = Repo::new();
        let context = repo.context();
        let err = resolve(&context, "../outside.txt").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PathOutsideWorkspace);
        let hint = err.detail().hint.clone().unwrap_or_default();
        assert!(hint.contains(repo.path().as_str()), "{hint}");
        assert!(
            err.message().contains("../outside.txt"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn an_escaped_path_resolves_when_external_access_is_granted() {
        let repo = Repo::new();
        let context = repo.context().with_external_access(true);
        let resolved = resolve(&context, "../outside.txt").expect("resolve");
        assert!(resolved.external);
        assert_eq!(
            resolved.path,
            repo.path().parent().expect("parent").join("outside.txt")
        );
    }

    #[test]
    fn an_additional_root_is_permitted() {
        let repo = Repo::new();
        let extra = repo.path().join("nested");
        let context = repo.context().with_root(extra.clone());
        let resolved = resolve(&context, extra.join("untracked.txt").as_str()).expect("resolve");
        assert!(!resolved.external);
    }

    #[test]
    fn a_tilde_path_expands_to_the_home_directory() {
        let repo = Repo::new();
        let context = repo.context().with_external_access(true);
        let Some(home) = std::env::var("HOME").ok().filter(|home| !home.is_empty()) else {
            return;
        };
        let resolved = resolve(&context, "~").expect("resolve");
        let nested = resolve(&context, "~/child").expect("resolve");
        assert_eq!(resolved.path, Utf8PathBuf::from(home.as_str()));
        assert_eq!(nested.path, Utf8PathBuf::from(home).join("child"));
    }

    #[test]
    fn a_named_user_tilde_is_refused() {
        let repo = Repo::new();
        let err = resolve(&repo.context(), "~other/file").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("path"));
    }

    #[test]
    fn a_nul_byte_in_a_path_is_refused() {
        let repo = Repo::new();
        let err = resolve(&repo.context(), "src/\0main.rs").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("path"));
    }

    #[test]
    fn an_empty_path_is_refused() {
        let repo = Repo::new();
        let err = resolve(&repo.context(), "").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn normalisation_never_rises_above_the_root() {
        assert_eq!(normalize(Utf8Path::new("/a/../b")), Utf8PathBuf::from("/b"));
        assert_eq!(normalize(Utf8Path::new("/../b")), Utf8PathBuf::from("/b"));
        assert_eq!(
            normalize(Utf8Path::new("/a/./b/../c")),
            Utf8PathBuf::from("/a/c")
        );
    }

    #[test]
    fn the_display_form_is_relative_inside_the_workspace() {
        let repo = Repo::new();
        let context = repo.context();
        let inside = repo.path().join("src/main.rs");
        assert_eq!(display_path(&context, &inside), "src/main.rs");
        assert_eq!(display_path(&context, repo.path()), ".");
        assert_eq!(
            display_path(&context, Utf8Path::new("/etc/hosts")),
            "/etc/hosts"
        );
    }

    #[test]
    fn the_walk_finds_nested_untracked_files_and_skips_ignored_ones() {
        let repo = Repo::new();
        let walker = Walker::new(repo.path(), FileLimits::default().walk_files).expect("walker");
        let found = walker
            .map(|entry| entry.relative.as_str().to_owned())
            .collect::<Vec<_>>();

        assert!(
            found.contains(&"nested/untracked.txt".to_owned()),
            "{found:?}"
        );
        assert!(
            found.contains(&"src/nested/deep/mod.rs".to_owned()),
            "{found:?}"
        );
        assert!(
            !found.iter().any(|path| path.starts_with("build/")),
            "{found:?}"
        );
        assert!(
            !found.iter().any(|path| path.starts_with("target/")),
            "{found:?}"
        );
        assert!(
            !found.iter().any(|path| path.starts_with(".git/")),
            "{found:?}"
        );
    }

    #[test]
    fn a_nested_walk_finds_the_same_file() {
        let repo = Repo::new();
        let nested = repo.path().join("nested");
        let walker = Walker::new(&nested, FileLimits::default().walk_files).expect("walker");
        let found = walker
            .map(|entry| entry.relative.as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(found, vec!["untracked.txt".to_owned()]);
    }

    #[test]
    fn a_named_file_walks_as_a_single_entry() {
        let repo = Repo::new();
        let file = repo.path().join("src/main.rs");
        let walker = Walker::new(&file, FileLimits::default().walk_files).expect("walker");
        let found = walker.collect::<Vec<_>>();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].relative, Utf8PathBuf::from("main.rs"));
    }

    #[test]
    fn the_walk_stops_at_the_entry_cap_and_says_so() {
        let repo = Repo::new();
        for index in 0..70 {
            repo.write(&format!("many/file_{index}.txt"), "content\n");
        }
        let mut walker = Walker::new(repo.path(), 8).expect("walker");
        let found = walker.by_ref().count();
        assert_eq!(found, 8);
        assert!(walker.stopped());
        assert_eq!(walker.visited(), 8);
    }

    #[test]
    fn a_walk_that_visits_everything_is_not_reported_as_stopped() {
        let repo = Repo::new();
        let mut walker =
            Walker::new(repo.path(), FileLimits::default().walk_files).expect("walker");
        let found = walker.by_ref().count();
        assert!(found > 8, "{found}");
        assert!(!walker.stopped());
    }

    #[test]
    fn a_missing_root_is_reported() {
        let repo = Repo::new();
        let err = Walker::new(&repo.path().join("absent"), 8).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }

    #[test]
    fn globs_do_not_cross_a_separator_but_match_a_name_at_any_depth() {
        let nested = compile_glob("*.rs").expect("compile");
        assert!(nested.is_match("src/main.rs"));
        assert!(nested.is_match("mod.rs"));

        let rooted = compile_glob("src/*.rs").expect("compile");
        assert!(rooted.is_match("src/main.rs"));
        assert!(!rooted.is_match("src/deep/main.rs"));

        let deep = compile_glob("**/*.rs").expect("compile");
        assert!(deep.is_match("src/nested/deep/mod.rs"));
    }

    #[test]
    fn an_empty_or_invalid_glob_is_refused() {
        let err = compile_glob("").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("pattern"));

        let err = compile_glob("[unclosed").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.message().contains("[unclosed"), "{}", err.message());
    }

    #[test]
    fn a_truncated_line_names_its_true_length() {
        let line = "x".repeat(3000);
        let cut = truncate_line(&line, 2000);
        assert!(cut.len() < line.len());
        assert!(cut.contains("3000 bytes"), "{cut}");
        assert_eq!(truncate_line("short", 2000), "short");
    }

    #[test]
    fn the_footer_survives_a_cut_body() {
        let body = "a".repeat(1000);
        let footer = "\n[footer]";
        let joined = join_capped(body.clone(), footer, 200);
        assert!(joined.ends_with(footer), "{joined}");
        assert!(joined.contains("truncated"), "{joined}");
        assert_eq!(join_capped(body, footer, 2000).ends_with(footer), true);
    }

    #[test]
    fn limits_come_from_the_limit_set() {
        let limits = FileLimits::from_budget(&fixture::budget_with_list_entries(5));
        assert_eq!(limits.list_entries, 5);
        assert_eq!(limits.walk_files, 5 * WALK_ENTRY_FACTOR);
        assert_eq!(
            FileLimits::default().list_entries,
            LimitName::ListEntries
                .default_value()
                .value()
                .expect("bounded") as usize
        );
        assert_eq!(
            FileLimits::default().output_bytes,
            LimitName::CommandOutputBytes
                .default_value()
                .value()
                .expect("bounded") as usize
        );
    }

    #[test]
    fn the_context_cap_and_the_limit_cap_take_the_smaller() {
        let repo = Repo::new();
        let limits = FileLimits::default();
        let narrow = repo.context().with_output_cap(64);
        assert_eq!(limits.output_cap(&narrow), MIN_OUTPUT_BYTES);
        let wide = repo.context().with_output_cap(usize::MAX);
        assert_eq!(limits.output_cap(&wide), limits.output_bytes);
    }
}
