//! A background index of workspace paths for `@` mentions and Tab completion.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use rune_tools::workspace::Walker;

/// Files walked before the index stops.
pub const INDEX_CAP: usize = 50_000;
/// Age after which opening the menu rebuilds the index.
pub const REFRESH_AFTER: Duration = Duration::from_secs(30);

/// Starts tried per path, so a path full of one letter stays cheap.
const MAX_STARTS: usize = 16;
const MAX_GAP_PENALTY: i64 = 15;

/// Workspace-relative paths with `/` separators; directories end in `/`.
#[derive(Debug)]
pub struct FileIndex {
    paths: Arc<Vec<String>>,
    built: Instant,
    truncated: bool,
}

impl FileIndex {
    fn build(root: &Utf8Path) -> Self {
        let built = Instant::now();
        let Ok(walker) = Walker::new(root, INDEX_CAP) else {
            return Self {
                paths: Arc::new(Vec::new()),
                built,
                truncated: false,
            };
        };
        let mut walker = walker;
        let mut files = Vec::new();
        let mut directories = BTreeSet::new();
        for walked in walker.by_ref() {
            let Some(relative) = relative_form(&walked.relative) else {
                continue;
            };
            let mut end = 0;
            while let Some(slash) = relative.get(end..).and_then(|rest| rest.find('/')) {
                end = end.saturating_add(slash).saturating_add(1);
                if let Some(directory) = relative.get(..end) {
                    directories.insert(directory.to_owned());
                }
            }
            files.push(relative);
        }
        let truncated = walker.stopped();
        let mut paths: Vec<String> = directories.into_iter().chain(files).collect();
        paths.sort_unstable();
        paths.dedup();
        Self {
            paths: Arc::new(paths),
            built,
            truncated,
        }
    }
}

/// A path is offered only when it can be typed back as one prompt token.
fn relative_form(path: &Utf8Path) -> Option<String> {
    let text = path.as_str().replace('\\', "/");
    (!text.is_empty() && !text.chars().any(char::is_control)).then_some(text)
}

/// The index shared between the prompt and the thread that builds it.
///
/// Only one build runs at a time; the previous index stays readable while a
/// rebuild runs. A default value was never started and never builds.
#[derive(Clone, Debug, Default)]
pub struct SharedIndex(Arc<Mutex<Option<FileIndex>>>, Arc<AtomicBool>);

impl SharedIndex {
    /// Spawns the first build.
    pub fn start(root: Utf8PathBuf) -> Self {
        let index = Self::default();
        index.spawn(root);
        index
    }

    /// Spawns a rebuild when the index is older than [`REFRESH_AFTER`].
    pub fn refresh_if_stale(&self, root: &Utf8Path) {
        let stale = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|index| index.built.elapsed() >= REFRESH_AFTER);
        if stale {
            self.spawn(root.to_owned());
        }
    }

    /// The paths of the last finished build, if any.
    pub fn snapshot(&self) -> Option<Arc<Vec<String>>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|index| Arc::clone(&index.paths))
    }

    /// Whether the last finished build stopped at [`INDEX_CAP`].
    pub fn truncated(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|index| index.truncated)
    }

    fn spawn(&self, root: Utf8PathBuf) {
        if self.1.swap(true, Ordering::SeqCst) {
            return;
        }
        let slot = Arc::clone(&self.0);
        let building = Arc::clone(&self.1);
        let spawned = std::thread::Builder::new()
            .name("rune-file-index".to_owned())
            .spawn(move || {
                let index = FileIndex::build(&root);
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(index);
                building.store(false, Ordering::SeqCst);
            });
        if spawned.is_err() {
            self.1.store(false, Ordering::SeqCst);
        }
    }

    /// An index that already holds these paths.
    #[cfg(test)]
    pub fn from_paths(paths: Vec<String>) -> Self {
        let index = Self::default();
        *index.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(FileIndex {
            paths: Arc::new(paths),
            built: Instant::now(),
            truncated: false,
        });
        index
    }

    #[cfg(test)]
    fn wait(&self) {
        while self.1.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Ranks paths against a fuzzy query, best first.
pub fn rank(
    paths: &[String],
    query: &str,
    touched: &BTreeSet<String>,
    limit: usize,
) -> Vec<String> {
    let query: Vec<char> = query.chars().map(fold).collect();
    let dotted = query.first() == Some(&'.');
    let mut original = Vec::new();
    let mut folded = Vec::new();
    let mut scored: Vec<(i64, &String)> = Vec::new();
    for path in paths {
        if !dotted && path.split('/').any(|segment| segment.starts_with('.')) {
            continue;
        }
        if !query.is_empty() && path.chars().map(fold).eq(query.iter().copied()) {
            continue;
        }
        original.clear();
        original.extend(path.chars());
        folded.clear();
        folded.extend(original.iter().copied().map(fold));
        let Some(mut score) = fuzzy_score(&original, &folded, &query) else {
            continue;
        };
        let name = name_start(&original);
        let prefix_of = |from: usize| {
            folded
                .get(from..)
                .is_some_and(|rest| rest.starts_with(&query))
        };
        if prefix_of(name) || (query.contains(&'/') && prefix_of(0)) {
            score = score.saturating_add(10);
        }
        if touched.contains(path) {
            score = score.saturating_add(8);
        }
        scored.push((score, path));
    }
    scored.sort_unstable_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.len().cmp(&right.len()))
            .then_with(|| left.as_bytes().cmp(right.as_bytes()))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, path)| path.clone())
        .collect()
}

fn fold(ch: char) -> char {
    ch.to_lowercase().next().unwrap_or(ch)
}

/// Index of the first char of the last segment, ignoring a trailing `/`.
fn name_start(path: &[char]) -> usize {
    let end = if path.last() == Some(&'/') {
        path.len().saturating_sub(1)
    } else {
        path.len()
    };
    path.get(..end)
        .and_then(|head| head.iter().rposition(|&ch| ch == '/'))
        .map_or(0, |slash| slash.saturating_add(1))
}

fn segment_start(path: &[char], at: usize) -> bool {
    let Some(previous) = at.checked_sub(1).and_then(|before| path.get(before)) else {
        return true;
    };
    matches!(previous, '/' | '_' | '-' | '.')
        || (previous.is_lowercase() && path.get(at).is_some_and(|ch| ch.is_uppercase()))
}

/// The best greedy match over the first [`MAX_STARTS`] starts, or `None`.
fn fuzzy_score(original: &[char], folded: &[char], query: &[char]) -> Option<i64> {
    let Some(&first) = query.first() else {
        return Some(0);
    };
    let name = name_start(original);
    let mut best = None;
    let starts = folded
        .iter()
        .enumerate()
        .filter(|(_, ch)| **ch == first)
        .map(|(at, _)| at)
        .take(MAX_STARTS);
    for start in starts {
        let mut score: i64 = 0;
        let mut previous: Option<usize> = None;
        let mut from = start;
        let mut complete = true;
        for &wanted in query {
            let Some(at) = folded
                .get(from..)
                .and_then(|rest| rest.iter().position(|&ch| ch == wanted))
                .map(|offset| from.saturating_add(offset))
            else {
                complete = false;
                break;
            };
            score = score.saturating_add(1);
            if segment_start(original, at) {
                score = score.saturating_add(5);
            }
            if previous.is_some_and(|before| before.saturating_add(1) == at) {
                score = score.saturating_add(3);
            }
            if at >= name {
                score = score.saturating_add(2);
            }
            previous = Some(at);
            from = at.saturating_add(1);
        }
        if !complete {
            continue;
        }
        let span = previous.map_or(0, |last| last.saturating_sub(start).saturating_add(1));
        let gaps = span.saturating_sub(query.len());
        let penalty = i64::try_from(gaps).unwrap_or(i64::MAX).min(MAX_GAP_PENALTY);
        let score = score.saturating_sub(penalty);
        if best.is_none_or(|known| score > known) {
            best = Some(score);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|&path| path.to_owned()).collect()
    }

    const FIXTURE: [&str; 12] = [
        "Cargo.toml",
        "README.md",
        "crates/",
        "crates/rune/",
        "crates/rune/src/main.rs",
        "crates/rune/src/session.rs",
        "docs/",
        "docs/guide.md",
        "scripts/remain.txt",
        "src/",
        "src/lib.rs",
        "src/main.rs",
    ];

    #[test]
    fn rank_orders_the_fixture_tree() {
        let paths = owned(&FIXTURE);
        let none = BTreeSet::new();
        for (query, expected) in [
            (
                "srmain",
                &[
                    "src/main.rs",
                    "crates/rune/src/main.rs",
                    "scripts/remain.txt",
                ][..],
            ),
            (
                "main",
                &[
                    "src/main.rs",
                    "crates/rune/src/main.rs",
                    "scripts/remain.txt",
                ][..],
            ),
            ("guide", &["docs/guide.md"][..]),
            (
                "src/",
                &[
                    "src/lib.rs",
                    "src/main.rs",
                    "crates/rune/src/main.rs",
                    "crates/rune/src/session.rs",
                ][..],
            ),
            (
                "CT",
                &[
                    "Cargo.toml",
                    "crates/",
                    "crates/rune/",
                    "crates/rune/src/main.rs",
                ][..],
            ),
        ] {
            assert_eq!(rank(&paths, query, &none, 4), expected, "query {query}");
        }
    }

    #[test]
    fn hidden_paths_need_a_dot_query() {
        let paths = owned(&[".github/workflows/ci.yml", "src/cli.rs", ".gitignore"]);
        let none = BTreeSet::new();
        assert_eq!(rank(&paths, "ci", &none, 10), ["src/cli.rs"]);
        assert_eq!(
            rank(&paths, ".git", &none, 10),
            [".gitignore", ".github/workflows/ci.yml"]
        );
    }

    #[test]
    fn touched_files_rank_higher() {
        let paths = owned(&["src/main.rs", "crates/rune/src/main.rs"]);
        let touched = BTreeSet::from(["crates/rune/src/main.rs".to_owned()]);
        assert_eq!(
            rank(&paths, "main", &BTreeSet::new(), 10),
            ["src/main.rs", "crates/rune/src/main.rs"]
        );
        assert_eq!(
            rank(&paths, "main", &touched, 10),
            ["crates/rune/src/main.rs", "src/main.rs"]
        );
    }

    #[test]
    fn empty_query_lists_touched_then_short_paths() {
        let paths = owned(&FIXTURE);
        let touched = BTreeSet::from(["crates/rune/src/session.rs".to_owned()]);
        assert_eq!(
            rank(&paths, "", &touched, 5),
            [
                "crates/rune/src/session.rs",
                "src/",
                "docs/",
                "crates/",
                "README.md"
            ]
        );
    }

    #[test]
    fn an_ignored_file_is_not_indexed() {
        let temp = tempfile::tempdir().expect("fixture");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_owned()).expect("UTF-8");
        std::fs::write(root.join(".gitignore"), "target/\nsecret.txt\n").expect("ignore");
        std::fs::create_dir_all(root.join("target")).expect("target");
        std::fs::write(root.join("target/x"), "built").expect("built");
        std::fs::write(root.join("secret.txt"), "secret").expect("secret");
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(root.join("src/main.rs"), "fn main() {}").expect("main");
        let index = SharedIndex::start(root);
        index.wait();
        let paths = index.snapshot().expect("built");
        assert_eq!(*paths, owned(&[".gitignore", "src/", "src/main.rs"]));
        assert!(!index.truncated());
    }

    #[test]
    fn the_index_stops_at_its_cap_and_says_so() {
        let temp = tempfile::tempdir().expect("fixture");
        let root = Utf8PathBuf::from_path_buf(temp.path().to_owned()).expect("UTF-8");
        for at in 0..=INDEX_CAP {
            std::fs::write(root.join(format!("f{at}")), "").expect("file");
        }
        let index = SharedIndex::start(root);
        index.wait();
        assert_eq!(index.snapshot().expect("built").len(), INDEX_CAP);
        assert!(index.truncated());
    }

    #[test]
    fn a_default_index_never_builds() {
        let index = SharedIndex::default();
        index.refresh_if_stale(Utf8Path::new("."));
        index.wait();
        assert!(index.snapshot().is_none());
    }

    #[test]
    fn ranking_twenty_thousand_paths_is_fast() {
        let paths: Vec<String> = (0..20_000)
            .map(|at| format!("crates/module{}/src/file_{at}_handler.rs", at % 97))
            .collect();
        let touched = BTreeSet::new();
        let started = Instant::now();
        let ranked = rank(&paths, "mod/hand", &touched, 50);
        let elapsed = started.elapsed();
        assert_eq!(ranked.len(), 50);
        let bound = if cfg!(debug_assertions) {
            Duration::from_millis(250)
        } else {
            Duration::from_millis(50)
        };
        assert!(elapsed < bound, "ranking took {elapsed:?}");
    }
}
