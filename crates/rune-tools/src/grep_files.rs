//! Literal text search with pagination, counts, and explicit truncation.

use std::io::{BufRead, BufReader};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{Result, RuneError};

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput};
use crate::workspace::{
    FileLimits, MAX_MATCH_LINE_BYTES, Walker, bool_arg, compile_glob, default_root, display_in,
    join_capped, resolve, roots, string_arg, truncate_line, usize_arg, walk_notes,
};

/// How often a long search checks for cancellation.
const CANCEL_CHECK_INTERVAL: usize = 256;

/// Bytes read to decide whether a file is binary.
const PROBE_BYTES: usize = 8 * 1024;

/// Characters that make a pattern look like a regular expression.
///
/// Only a hint is produced: the match is literal whatever the pattern contains.
const REGEX_HINTS: &[&str] = &[
    r"\d", r"\w", r"\s", r"\b", ".*", ".+", "[^", "(?", "^", "$", "|", "(", ")", "[", "]",
];

/// Searches file contents for literal text.
#[derive(Clone, Copy, Debug)]
pub struct GrepFiles {
    limits: FileLimits,
}

impl Default for GrepFiles {
    fn default() -> Self {
        Self {
            limits: FileLimits::default(),
        }
    }
}

impl GrepFiles {
    /// Builds a search with the configured caps.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a search with explicit caps.
    #[must_use]
    pub const fn with_limits(limits: FileLimits) -> Self {
        Self { limits }
    }
}

impl Tool for GrepFiles {
    fn name(&self) -> &'static str {
        "grep_files"
    }

    fn description(&self) -> &'static str {
        "Search file contents for literal text. The pattern is not a regular expression: \
         metacharacters match themselves. Supports an include glob, case-insensitive matching, \
         context lines, and pagination with head_limit and offset. Binary files are skipped and \
         reported."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Literal text to find. Regex metacharacters match literally.",
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search. Defaults to the workspace root.",
                },
                "include": {
                    "type": "string",
                    "description": "Glob filter for file paths, for example `**/*.rs`.",
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Match without regard to case. Defaults to false.",
                },
                "mode": {
                    "type": "string",
                    "enum": ["matches", "files_with_matches", "count"],
                    "description": "List matching lines, matching files, or exact counts.",
                },
                "head_limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Entries this page returns. Defaults to the list_entries limit.",
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Entries to skip before this page, for paging.",
                },
                "context_lines": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Lines shown before and after each match, in matches mode.",
                },
            },
            "required": ["pattern"],
            "additionalProperties": false,
        })
    }

    fn activity(&self) -> Activity {
        Activity::Search
    }

    fn permission_target(&self, arguments: &serde_json::Value) -> Option<String> {
        arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .or_else(|| arguments.get("pattern").and_then(serde_json::Value::as_str))
            .map(str::to_owned)
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        let query = parse(self, arguments, context)?;
        let include = match query.include.as_deref() {
            Some(pattern) => Some(compile_glob(pattern)?),
            None => None,
        };

        let normalised = roots(context);
        let label = display_in(&normalised, &query.root);
        let mut walker = Walker::new(&query.root, self.limits.walk_files)?;
        let mut report = Report::default();

        loop {
            let Some(entry) = walker.next() else { break };
            if report.scanned % CANCEL_CHECK_INTERVAL == 0 {
                context.check_cancelled()?;
            }
            report.scanned = report.scanned.saturating_add(1);
            if let Some(include) = &include
                && !include.is_match(&entry.relative)
            {
                continue;
            }
            let display = display_in(&normalised, &entry.path);
            if let Err(err) = scan(&entry.path, &display, &query, &mut report) {
                match err {
                    ScanError::Binary => report.binary.push(display),
                    ScanError::Unreadable(reason) => report.unreadable.push((display, reason)),
                }
            }
        }

        Ok(ToolOutput::success(render(
            &query,
            &label,
            &report,
            &walker,
            self.limits.output_cap(context),
        )))
    }
}

/// Everything a call asked for, already validated.
struct Query {
    pattern: String,
    root: Utf8PathBuf,
    include: Option<String>,
    case_insensitive: bool,
    mode: Mode,
    head_limit: usize,
    offset: usize,
    context_lines: usize,
    /// True when the pattern carries regular-expression syntax.
    looks_like_regex: bool,
}

/// Parses and validates the arguments.
fn parse(
    tool: &GrepFiles,
    arguments: &serde_json::Value,
    context: &ExecutionContext,
) -> Result<Query> {
    let pattern = string_arg(arguments, "pattern")?
        .ok_or_else(|| RuneError::missing_field("pattern"))?
        .to_owned();
    if pattern.is_empty() {
        return Err(RuneError::invalid_field("pattern", "pattern is empty"));
    }
    if pattern.contains('\n') || pattern.contains('\r') {
        return Err(RuneError::invalid_field(
            "pattern",
            "pattern spans lines, and this search is line-oriented",
        ));
    }

    let root = match string_arg(arguments, "path")? {
        Some(raw) => resolve(context, raw)?.path,
        None => default_root(context)?.path,
    };
    let head_limit = match usize_arg(arguments, "head_limit")? {
        Some(0) => {
            return Err(RuneError::invalid_field(
                "head_limit",
                "head_limit must be at least 1; omit it to use the list_entries limit",
            ));
        }
        Some(value) => value.min(tool.limits.list_entries),
        None => tool.limits.list_entries,
    };
    Ok(Query {
        looks_like_regex: REGEX_HINTS.iter().any(|hint| pattern.contains(hint)),
        pattern,
        root,
        include: string_arg(arguments, "include")?.map(str::to_owned),
        case_insensitive: bool_arg(arguments, "case_insensitive")?.unwrap_or(false),
        mode: Mode::of(arguments)?,
        head_limit,
        offset: usize_arg(arguments, "offset")?.unwrap_or(0),
        context_lines: usize_arg(arguments, "context_lines")?.unwrap_or(0),
    })
}

/// The shape of the result a call asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Matches,
    FilesWithMatches,
    Count,
}

impl Mode {
    /// Reads the mode argument.
    fn of(arguments: &serde_json::Value) -> Result<Self> {
        match string_arg(arguments, "mode")? {
            None | Some("matches") => Ok(Self::Matches),
            Some("files_with_matches") => Ok(Self::FilesWithMatches),
            Some("count") => Ok(Self::Count),
            Some(other) => Err(RuneError::invalid_field(
                "mode",
                format!(
                    "`{other}` is not a mode; expected `matches`, `files_with_matches`, or `count`"
                ),
            )),
        }
    }
}

/// One matching line, with the lines shown around it.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Hit {
    /// File the line belongs to, as the model should see it.
    file: String,
    /// 1-based line number.
    number: usize,
    /// Line text, already cut to the display cap.
    text: String,
    /// Lines shown before the match, rendered as `number-text`.
    leading: Vec<String>,
    /// Lines shown after the match, rendered as `number-text`.
    trailing: Vec<String>,
}

/// What a search found.
#[derive(Default)]
struct Report {
    /// Total matching lines across every file.
    matches: usize,
    /// Files that contained at least one match.
    files: usize,
    /// Matching file names, limited to the entries this call can return.
    named: Vec<String>,
    /// The page of hits this call can return.
    hits: Vec<Hit>,
    /// Files skipped because they are binary.
    binary: Vec<String>,
    /// Files that could not be read, with the reason.
    unreadable: Vec<(String, String)>,
    /// Files visited, whether or not they matched.
    scanned: usize,
}

/// Why a file was not searched.
enum ScanError {
    /// The file is binary and was skipped.
    Binary,
    /// The file could not be read.
    Unreadable(String),
}

/// Searches one file, folding whatever it found into the report.
///
/// Every line is examined so the counts stay exact, but only the entries inside
/// the requested page are retained. That keeps memory proportional to the page
/// rather than to the result, whatever `offset` is.
fn scan(
    path: &Utf8Path,
    display: &str,
    query: &Query,
    report: &mut Report,
) -> std::result::Result<(), ScanError> {
    let file = std::fs::File::open(path.as_std_path())
        .map_err(|err| ScanError::Unreadable(format!("`{display}` could not be read: {err}")))?;
    let mut reader = BufReader::with_capacity(PROBE_BYTES, file);
    if reader.fill_buf().unwrap_or_default().contains(&0) {
        return Err(ScanError::Binary);
    }

    let needle = if query.case_insensitive {
        query.pattern.to_lowercase()
    } else {
        query.pattern.clone()
    };

    let mut pending: Vec<String> = Vec::new();
    let mut trailing = 0_usize;
    let mut number = 0_usize;
    let mut file_matched = false;
    let mut buffer = Vec::new();

    loop {
        buffer.clear();
        let read = reader.read_until(b'\n', &mut buffer).map_err(|err| {
            ScanError::Unreadable(format!("`{display}` could not be read: {err}"))
        })?;
        if read == 0 {
            break;
        }
        number = number.saturating_add(1);
        let text = String::from_utf8_lossy(&buffer);
        let text = text.trim_end_matches(['\n', '\r']);
        let shown = truncate_line(text, MAX_MATCH_LINE_BYTES);
        let haystack = if query.case_insensitive {
            shown.to_lowercase()
        } else {
            shown.clone()
        };

        if haystack.contains(&needle) {
            // The index before the increment is this match's place in the
            // result, which is what offset and head_limit select on.
            let index = report.matches;
            report.matches = report.matches.saturating_add(1);
            if !file_matched {
                file_matched = true;
                let file_index = report.files;
                report.files = report.files.saturating_add(1);
                // Only the page's file names are retained, so memory follows
                // head_limit rather than the size of the result.
                if query.mode == Mode::FilesWithMatches
                    && file_index >= query.offset
                    && report.named.len() < query.head_limit
                {
                    report.named.push(display.to_owned());
                }
            }
            if query.mode == Mode::Matches
                && index >= query.offset
                && report.hits.len() < query.head_limit
            {
                report.hits.push(Hit {
                    file: display.to_owned(),
                    number,
                    text: shown,
                    leading: pending.clone(),
                    trailing: Vec::new(),
                });
            }
            trailing = query.context_lines;
            continue;
        }

        if trailing > 0 {
            if let Some(hit) = report.hits.last_mut()
                && hit.file == display
            {
                hit.trailing.push(format!("{number:>6}-{shown}"));
            }
            trailing = trailing.saturating_sub(1);
        }
        if query.context_lines > 0 {
            pending.push(format!("{number:>6}-{shown}"));
            if pending.len() > query.context_lines {
                pending.remove(0);
            }
        }
    }
    Ok(())
}

/// Renders the report for the model.
fn render(query: &Query, label: &str, report: &Report, walker: &Walker, cap: usize) -> String {
    let mut body = String::new();
    let mut footer = String::new();

    match query.mode {
        Mode::Count => {}
        Mode::FilesWithMatches => {
            for file in &report.named {
                body.push_str(file);
                body.push('\n');
            }
        }
        Mode::Matches => {
            for hit in &report.hits {
                for line in &hit.leading {
                    body.push_str(&format!("{}:{line}\n", hit.file));
                }
                body.push_str(&format!("{}:{:>6}:{}\n", hit.file, hit.number, hit.text));
                for line in &hit.trailing {
                    body.push_str(&format!("{}:{line}\n", hit.file));
                }
            }
        }
    }

    let summary = format!(
        "[{} matching lines in {} files, searched {} files under `{label}`]",
        report.matches, report.files, report.scanned
    );
    footer.push_str(&summary);
    footer.push('\n');

    let listed = match query.mode {
        Mode::Count => 0,
        Mode::FilesWithMatches => report.named.len(),
        Mode::Matches => report.hits.len(),
    };
    let total = match query.mode {
        Mode::Count => 0,
        Mode::FilesWithMatches => report.files,
        Mode::Matches => report.matches,
    };
    if query.mode != Mode::Count {
        let start = query.offset.min(total);
        let next = start.saturating_add(listed);
        if listed < total || query.offset > 0 {
            footer.push_str(&format!(
                "[page: entries {} to {next} of {total}; pass offset={next} for the next page]\n",
                if listed == 0 {
                    0
                } else {
                    start.saturating_add(1)
                }
            ));
        }
    }
    if query.context_lines > 0 && query.mode != Mode::Matches {
        footer.push_str("[context_lines applies only to matches mode]\n");
    }
    if query.looks_like_regex {
        footer.push_str(&format!(
            "[the pattern `{}` contains regex metacharacters, which were matched literally]\n",
            query.pattern
        ));
    }
    if !report.binary.is_empty() {
        footer.push_str(&format!(
            "[{} binary files skipped: {}]\n",
            report.binary.len(),
            report.binary.join(", ")
        ));
    }
    if !report.unreadable.is_empty() {
        footer.push_str(&format!(
            "[{} files could not be read: {}]\n",
            report.unreadable.len(),
            report
                .unreadable
                .iter()
                .map(|(file, reason)| format!("{file} ({reason})"))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    footer.push_str(&walk_notes(walker));

    if body.len().saturating_add(footer.len()) > cap {
        footer.push_str(
            "[the byte cap cut this page, so the summary above counts matches that are not \
             listed; lower head_limit or narrow the pattern]\n",
        );
    }
    join_capped(body, &footer, cap)
}

#[cfg(test)]
mod tests {
    use rune_core::error::ErrorCode;

    use super::*;
    use crate::workspace::fixture::{MATCH_FILES, MATCH_LINES, Repo, budget_with_list_entries};

    /// Builds a search with a lowered listing cap.
    fn search(entries: usize) -> GrepFiles {
        GrepFiles::with_limits(FileLimits::from_budget(&budget_with_list_entries(entries)))
    }

    /// Returns the summary line of a result.
    fn summary(text: &str) -> String {
        text.lines()
            .find(|line| line.starts_with("["))
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn literal_matches_are_found_with_line_numbers() {
        let repo = Repo::new();
        let output = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "needle" }), &repo.context())
            .expect("call");
        assert!(!output.is_error);
        assert!(
            output.text.contains("README.md:     2:needle one"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("src/main.rs:     2:let needle = 1;"),
            "{}",
            output.text
        );
    }

    #[test]
    fn the_pattern_is_literal_and_the_result_says_so() {
        let repo = Repo::new();
        repo.write("literal/a.txt", "value .+ here\nvalue xyz here\n");
        let output = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": ".+" }), &repo.context())
            .expect("call");
        assert!(output.text.contains("literal/a.txt"), "{}", output.text);
        assert!(
            output
                .text
                .contains("contains regex metacharacters, which were matched literally"),
            "{}",
            output.text
        );
        // A regex would have matched both lines.
        assert!(!output.text.contains("value xyz here"), "{}", output.text);
    }

    #[test]
    fn a_pattern_without_metacharacters_carries_no_note() {
        let repo = Repo::new();
        let output = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "needle" }), &repo.context())
            .expect("call");
        assert!(!output.text.contains("metacharacters"), "{}", output.text);
    }

    #[test]
    fn case_insensitive_matching_finds_other_cases() {
        let repo = Repo::new();
        repo.write("case/upper.txt", "NEEDLE upper\n");
        let insensitive = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle", "case_insensitive": true }),
                &repo.context(),
            )
            .expect("call");
        assert!(
            insensitive.text.contains("case/upper.txt"),
            "{}",
            insensitive.text
        );

        let sensitive = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "needle" }), &repo.context())
            .expect("call");
        assert!(
            !sensitive.text.contains("case/upper.txt"),
            "{}",
            sensitive.text
        );
    }

    #[test]
    fn count_is_exact_regardless_of_the_page_size() {
        let repo = Repo::new();
        let output = search(2)
            .call(
                &serde_json::json!({ "pattern": "needle", "mode": "count" }),
                &repo.context(),
            )
            .expect("call");
        assert!(
            summary(&output.text).contains(&format!(
                "{MATCH_LINES} matching lines in {MATCH_FILES} files"
            )),
            "{}",
            output.text
        );
    }

    #[test]
    fn every_mode_agrees_on_the_counts() {
        let repo = Repo::new();
        let context = repo.context();
        let mut counts = Vec::new();
        for mode in ["matches", "files_with_matches", "count"] {
            let output = GrepFiles::default()
                .call(
                    &serde_json::json!({ "pattern": "needle", "mode": mode }),
                    &context,
                )
                .expect("call");
            counts.push(summary(&output.text));
        }
        assert_eq!(counts[0], counts[1]);
        assert_eq!(counts[1], counts[2]);
        assert!(
            counts[0].contains(&format!("{MATCH_LINES} matching lines")),
            "{counts:?}"
        );
        assert!(
            counts[0].contains(&format!("{MATCH_FILES} files")),
            "{counts:?}"
        );
    }

    #[test]
    fn pagination_returns_disjoint_pages_covering_everything() {
        let repo = Repo::new();
        let context = repo.context();
        let page = |offset: usize| {
            GrepFiles::default()
                .call(
                    &serde_json::json!({
                        "pattern": "needle",
                        "head_limit": 3,
                        "offset": offset,
                    }),
                    &context,
                )
                .expect("call")
                .text
        };
        let pages = [page(0), page(3), page(6)];
        let entries = |text: &str| {
            text.lines()
                .filter(|line| !line.starts_with('['))
                .filter(|line| line.contains("needle"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let first = entries(&pages[0]);
        let second = entries(&pages[1]);
        let third = entries(&pages[2]);
        assert_eq!(first.len(), 3, "{}", pages[0]);
        assert_eq!(second.len(), 3, "{}", pages[1]);
        assert_eq!(third.len(), 3, "{}", pages[2]);
        assert!(pages[0].contains("pass offset=3"), "{}", pages[0]);
        assert!(pages[1].contains("pass offset=6"), "{}", pages[1]);

        let mut union = first
            .into_iter()
            .chain(second)
            .chain(third)
            .collect::<Vec<_>>();
        let count = union.len();
        union.sort();
        union.dedup();
        assert_eq!(union.len(), count, "pages overlap: {union:?}");
        assert_eq!(union.len(), MATCH_LINES);
    }

    #[test]
    fn every_page_reports_the_same_totals() {
        let repo = Repo::new();
        let context = repo.context();
        let mut totals = Vec::new();
        for offset in [0, 4, 8] {
            let output = GrepFiles::default()
                .call(
                    &serde_json::json!({
                        "pattern": "needle",
                        "head_limit": 4,
                        "offset": offset,
                    }),
                    &context,
                )
                .expect("call");
            totals.push(summary(&output.text));
        }
        assert_eq!(totals[0], totals[1]);
        assert_eq!(totals[1], totals[2]);
        assert!(
            totals[0].contains(&format!("{MATCH_LINES} matching lines")),
            "{totals:?}"
        );
    }

    #[test]
    fn pagination_in_file_mode_is_disjoint_too() {
        let repo = Repo::new();
        let context = repo.context();
        let page = |offset: usize| {
            GrepFiles::default()
                .call(
                    &serde_json::json!({
                        "pattern": "needle",
                        "mode": "files_with_matches",
                        "head_limit": 4,
                        "offset": offset,
                    }),
                    &context,
                )
                .expect("call")
                .text
        };
        let names = |text: &str| {
            text.lines()
                .filter(|line| !line.starts_with('['))
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let first = names(&page(0));
        let second = names(&page(4));
        assert_eq!(first.len(), 4, "{first:?}");
        assert_eq!(first.len() + second.len(), MATCH_FILES);
        for name in &first {
            assert!(!second.contains(name), "overlap on {name}");
        }
        assert!(page(4).contains("pass offset=8"), "{}", page(4));
    }

    #[test]
    fn an_offset_past_the_end_returns_an_empty_page_and_the_true_totals() {
        let repo = Repo::new();
        let output = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle", "head_limit": 5, "offset": 500 }),
                &repo.context(),
            )
            .expect("call");
        assert!(!output.is_error);
        assert!(output.text.contains("entries 0 to 0 of"), "{}", output.text);
        assert!(
            output
                .text
                .contains(&format!("{MATCH_LINES} matching lines")),
            "{}",
            output.text
        );
    }

    #[test]
    fn context_lines_are_shown_around_a_match() {
        let repo = Repo::new();
        repo.write("ctx/block.txt", "before\nneedle here\nafter\n");
        let output = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle here", "context_lines": 1 }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.text.contains("-before"), "{}", output.text);
        assert!(output.text.contains("-after"), "{}", output.text);
        assert!(output.text.contains(":needle here"), "{}", output.text);
    }

    #[test]
    fn context_is_noted_as_ignored_outside_matches_mode() {
        let repo = Repo::new();
        let output = GrepFiles::default()
            .call(
                &serde_json::json!({
                    "pattern": "needle",
                    "mode": "count",
                    "context_lines": 2,
                }),
                &repo.context(),
            )
            .expect("call");
        assert!(
            output
                .text
                .contains("context_lines applies only to matches mode"),
            "{}",
            output.text
        );
    }

    #[test]
    fn the_include_filter_limits_which_files_are_searched() {
        let repo = Repo::new();
        let only_rust = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle", "include": "**/*.rs" }),
                &repo.context(),
            )
            .expect("call");
        assert!(only_rust.text.contains("src/main.rs"), "{}", only_rust.text);
        assert!(!only_rust.text.contains("README.md"), "{}", only_rust.text);

        let by_suffix = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle", "include": "*.rs" }),
                &repo.context(),
            )
            .expect("call");
        assert!(
            by_suffix.text.contains("src/nested/deep/mod.rs"),
            "{}",
            by_suffix.text
        );
    }

    #[test]
    fn an_ignored_directory_is_never_searched() {
        let repo = Repo::new();
        let output = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "needle" }), &repo.context())
            .expect("call");
        assert!(
            !output.text.contains("build/ignored.txt"),
            "{}",
            output.text
        );
        assert!(!output.text.contains("target/"), "{}", output.text);
        assert!(!output.text.contains(".git/"), "{}", output.text);
    }

    #[test]
    fn a_binary_file_is_skipped_and_reported() {
        let repo = Repo::new();
        let output = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "needle" }), &repo.context())
            .expect("call");
        assert!(
            output.text.contains("binary files skipped"),
            "{}",
            output.text
        );
        assert!(output.text.contains("data/blob.bin"), "{}", output.text);
        assert!(
            summary(&output.text).contains(&format!("{MATCH_LINES} matching lines")),
            "{}",
            output.text
        );
    }

    #[test]
    fn no_match_is_reported_with_zero_counts() {
        let repo = Repo::new();
        let output = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "nothinghere" }),
                &repo.context(),
            )
            .expect("call");
        assert!(!output.is_error);
        assert!(
            output.text.contains("0 matching lines in 0 files"),
            "{}",
            output.text
        );
    }

    #[test]
    fn an_empty_pattern_is_refused() {
        let repo = Repo::new();
        let err = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "" }), &repo.context())
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("pattern"));
    }

    #[test]
    fn a_multiline_pattern_is_refused() {
        let repo = Repo::new();
        let err = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "a\nb" }), &repo.context())
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn an_invalid_mode_is_refused() {
        let repo = Repo::new();
        let err = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle", "mode": "everything" }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("mode"));
    }

    #[test]
    fn a_zero_head_limit_is_refused() {
        let repo = Repo::new();
        let err = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle", "head_limit": 0 }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("head_limit"));
    }

    #[test]
    fn an_invalid_include_glob_is_refused() {
        let repo = Repo::new();
        let err = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle", "include": "[oops" }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("pattern"));
    }

    #[test]
    fn an_escaping_search_root_is_refused() {
        let repo = Repo::new();
        let err = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle", "path": "../elsewhere" }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PathOutsideWorkspace);
    }

    #[test]
    fn output_never_exceeds_the_byte_cap_and_the_true_count_survives() {
        let repo = Repo::new();
        let body = (1..=500)
            .map(|n| format!("needle on line {n} {}\n", "z".repeat(40)))
            .collect::<String>();
        repo.write("wide/many.txt", &body);

        let context = repo.context().with_output_cap(4096);
        let output = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "needle" }), &context)
            .expect("call");
        assert!(output.text.len() <= 4096, "{}", output.text.len());
        assert!(
            output.text.contains("500 matching lines"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("byte cap cut this page"),
            "{}",
            output.text
        );
        assert_eq!(output.produced_bytes, output.text.len() as u64);
    }

    #[test]
    fn a_lowered_head_limit_keeps_the_counts_exact() {
        let repo = Repo::new();
        let output = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle", "head_limit": 1 }),
                &repo.context(),
            )
            .expect("call");
        assert!(
            output.text.contains("entries 1 to 1 of 9"),
            "{}",
            output.text
        );
        assert!(
            summary(&output.text).contains("9 matching lines in 7 files"),
            "{}",
            output.text
        );
    }

    #[test]
    fn a_very_long_line_is_cut_before_it_can_flood_the_result() {
        let repo = Repo::new();
        let mut line = String::from("needle ");
        line.push_str(&"q".repeat(200_000));
        line.push('\n');
        repo.write("wide/one.txt", &line);

        let context = repo.context().with_output_cap(64 * 1024);
        let output = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "needle" }), &context)
            .expect("call");
        assert!(output.text.len() <= 64 * 1024, "{}", output.text.len());
        assert!(output.text.contains("1 matching lines"), "{}", output.text);
    }

    #[test]
    fn a_crlf_line_matches_without_its_carriage_return() {
        let repo = Repo::new();
        let output = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle crlf" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.text.contains("crlf/win.txt"), "{}", output.text);
        assert!(!output.text.contains('\r'), "{:?}", output.text);
    }

    #[test]
    fn a_named_file_is_searched_directly() {
        let repo = Repo::new();
        let output = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle", "path": "nested/untracked.txt" }),
                &repo.context(),
            )
            .expect("call");
        assert!(
            output.text.contains("nested/untracked.txt"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("1 matching lines in 1 files"),
            "{}",
            output.text
        );
    }

    #[test]
    fn a_cancelled_call_stops_with_a_typed_error() {
        let repo = Repo::new();
        let context = repo.context();
        context.cancellation().cancel();
        let err = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "needle" }), &context)
            .expect_err("cancelled");
        assert_eq!(err.code(), ErrorCode::Cancelled);
    }

    #[test]
    fn the_declared_schema_names_only_the_accepted_arguments() {
        let schema = GrepFiles::default().input_schema();
        assert_eq!(schema["required"][0], "pattern");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["mode"]["enum"],
            serde_json::json!(["matches", "files_with_matches", "count"])
        );
    }

    #[test]
    fn the_regex_hint_recognises_the_common_escapes() {
        let hints = |pattern: &str| REGEX_HINTS.iter().any(|hint| pattern.contains(hint));
        assert!(hints(r"\d+"));
        assert!(hints("a.*b"));
        assert!(hints("[abc]"));
        assert!(!hints("needle"));
        assert!(!hints("fn main"));
    }
}
