//! Literal text search with pagination, counts, and explicit truncation.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput, validate_closed_arguments};
use crate::workspace::{
    FileLimits, MAX_MATCH_LINE_BYTES, Walker, bool_arg, compile_glob_as, default_root, display_in,
    join_capped, read_line_bounded_observed, resolve, roots, string_arg, summarize, truncate_line,
    usize_arg, walk_notes,
};

/// How often traversal checks for cancellation between files.
const CANCEL_CHECK_INTERVAL: usize = 256;

/// Bytes read to decide whether a file is binary.
const PROBE_BYTES: usize = 8 * 1024;

/// Names listed before a footer says how many more there are.
const MAX_LISTED_NAMES: usize = 20;

/// Largest context window a model may request, bounding the ring allocation.
const MAX_CONTEXT_LINES: usize = 1000;

/// Characters that make a pattern look like a regular expression.
///
/// Only a hint is produced: the match is literal whatever the pattern contains.
const REGEX_HINTS: &[&str] = &[
    r"\d", r"\w", r"\s", r"\b", ".*", ".+", "[^", "(?", "^", "$", "|", "(", ")", "[", "]",
];

/// Searches file contents for literal text.
#[derive(Clone, Copy, Debug, Default)]
pub struct GrepFiles {
    limits: FileLimits,
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
                    "maximum": MAX_CONTEXT_LINES,
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
        context.check_cancelled()?;
        validate_closed_arguments(arguments, &self.input_schema())?;
        let query = parse(self, arguments, context)?;
        let include = match query.include.as_deref() {
            Some(pattern) => Some(compile_glob_as("include", pattern)?),
            None => None,
        };

        let normalised = roots(context);
        let label = display_in(&normalised, &query.root);
        let mut walker = Walker::new(&query.root, self.limits.walk_files)?;
        let mut report = Report::default();

        for entry in walker.by_ref() {
            if report.scanned.is_multiple_of(CANCEL_CHECK_INTERVAL) {
                context.check_cancelled()?;
            }
            report.scanned = report.scanned.saturating_add(1);
            if let Some(include) = &include
                && !include.is_match(&entry.relative)
            {
                continue;
            }
            let display = display_in(&normalised, &entry.path);
            if let Err(err) = scan(
                &entry.path,
                &display,
                &query,
                report.matches,
                &mut report,
                context,
            ) {
                match err {
                    ScanError::Binary => report.binary.push(display),
                    ScanError::Unreadable(reason) => report.unreadable.push((display, reason)),
                    ScanError::Cancelled(err) => return Err(err),
                }
            }
        }

        context.check_cancelled()?;
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

    let context_lines = usize_arg(arguments, "context_lines")?.unwrap_or(0);
    if context_lines > MAX_CONTEXT_LINES {
        return Err(RuneError::invalid_field(
            "context_lines",
            format!("`context_lines` must be at most {MAX_CONTEXT_LINES}"),
        )
        .with_observed(context_lines.to_string()));
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
        context_lines,
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
#[derive(Debug)]
enum ScanError {
    /// The file is binary and was skipped.
    Binary,
    /// The file could not be read.
    Unreadable(String),
    /// Cancellation stops the entire search rather than skipping the file.
    Cancelled(RuneError),
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
    base: usize,
    report: &mut Report,
    context: &ExecutionContext,
) -> std::result::Result<(), ScanError> {
    context.check_cancelled().map_err(ScanError::Cancelled)?;
    let file = std::fs::File::open(path.as_std_path())
        .map_err(|err| ScanError::Unreadable(format!("`{display}` could not be read: {err}")))?;
    let mut reader = BufReader::with_capacity(PROBE_BYTES, file);
    scan_reader(&mut reader, display, query, base, report, context)
}

/// Scans buffered chunks with cancellation checks, including oversized lines.
fn scan_reader<R: BufRead>(
    reader: &mut R,
    display: &str,
    query: &Query,
    base: usize,
    report: &mut Report,
    context: &ExecutionContext,
) -> std::result::Result<(), ScanError> {
    context.check_cancelled().map_err(ScanError::Cancelled)?;
    let binary = reader.fill_buf().unwrap_or_default().contains(&0);
    context.check_cancelled().map_err(ScanError::Cancelled)?;
    if binary {
        return Err(ScanError::Binary);
    }

    let needle = if query.case_insensitive {
        query.pattern.to_lowercase()
    } else {
        query.pattern.clone()
    };
    let matcher = LiteralMatcher::new(&needle, query.case_insensitive)?;

    // Findings are staged locally and merged only when the whole file has been
    // read, so a file rejected partway through leaves no trace in the report.
    let context_lines = if query.mode == Mode::Matches {
        query.context_lines
    } else {
        0
    };
    let mut found = Found::default();
    // The context window is a ring, so only context_lines lines are ever held.
    // Every line enters it, a match included, because a match can be the line
    // before the next one.
    let mut recent: VecDeque<(usize, String)> = VecDeque::with_capacity(context_lines);
    // Lines still owed as context after the last hit on this page.
    let mut trailing = 0_usize;
    // The last line already shown, so overlapping context is shown once.
    let mut shown_through = 0_usize;
    let mut number = 0_usize;
    let mut buffer = Vec::new();

    loop {
        let mut matching = matcher.line();
        let line = read_line_bounded_observed(
            reader,
            &mut buffer,
            MAX_MATCH_LINE_BYTES,
            || context.check_cancelled(),
            |chunk| matching.feed(chunk),
        )
        .map_err(|err: RuneError| {
            if err.code() == ErrorCode::Cancelled {
                ScanError::Cancelled(err)
            } else {
                ScanError::Unreadable(format!("`{display}` could not be read: {}", err.message()))
            }
        })?;
        if line.consumed == 0 {
            break;
        }
        // Only the leading bytes were classified, so a NUL anywhere later still
        // skips the file rather than emitting mojibake or a false match.
        if line.has_nul {
            return Err(ScanError::Binary);
        }
        number = number.saturating_add(1);
        let text = String::from_utf8_lossy(&buffer);
        let text = text.trim_end_matches(['\n', '\r']);
        // All source chunks are searched independently of the retained head.
        let matched = matching.finish();
        let shown = truncate_line(text, line.length, MAX_MATCH_LINE_BYTES);
        let mut recorded = false;
        if matched {
            found.matches = found.matches.saturating_add(1);
            // The index is this match's place in the whole result, which is
            // what offset and head_limit select on.
            let index = base.saturating_add(found.matches).saturating_sub(1);
            recorded = query.mode == Mode::Matches
                && index >= query.offset
                && found.hits.len() < query.head_limit;
        }
        if recorded {
            let leading = recent
                .iter()
                .filter(|(line, _)| *line > shown_through)
                .map(|(line, text)| format!("{line:>6}-{text}"))
                .collect();
            found.hits.push(Hit {
                file: display.to_owned(),
                number,
                text: shown.clone(),
                leading,
                trailing: Vec::new(),
            });
            shown_through = number;
            trailing = context_lines;
        } else if trailing > 0 {
            // A line after the last hit is its context whether or not it
            // matches. A match left off the page opens no window of its own,
            // so nothing after it is shown as some earlier hit's context.
            if let Some(hit) = found.hits.last_mut() {
                hit.trailing.push(format!("{number:>6}-{shown}"));
                shown_through = number;
            }
            trailing = trailing.saturating_sub(1);
        }

        if context_lines > 0 {
            recent.push_back((number, shown));
            if recent.len() > context_lines {
                let _ = recent.pop_front();
            }
        }
    }

    report.matches = report.matches.saturating_add(found.matches);
    if found.matches > 0 {
        let file_index = report.files;
        report.files = report.files.saturating_add(1);
        if query.mode == Mode::FilesWithMatches
            && file_index >= query.offset
            && report.named.len() < query.head_limit
        {
            report.named.push(display.to_owned());
        }
    }
    // A page never exceeds head_limit, however many files contribute to it.
    for hit in found.hits {
        if report.hits.len() >= query.head_limit {
            break;
        }
        report.hits.push(hit);
    }
    Ok(())
}

/// What one file contributed, staged until the file has been read in full.
#[derive(Default)]
struct Found {
    /// Matching lines in this file.
    matches: usize,
    /// Hits to report, already limited to the page.
    hits: Vec<Hit>,
}

/// A streaming literal search. The prefix table bounds both memory and work
/// even for patterns longer than the reader's buffer or with repeated prefixes.
struct LiteralMatcher {
    needle: Vec<u8>,
    prefixes: Vec<usize>,
    case_insensitive: bool,
    /// Lowercasing Greek sigma depends on surrounding cased characters, ignoring
    /// case-ignorable characters. Other lowercase mappings are per character.
    sigma_context: Option<(regex::Regex, regex::Regex)>,
}

impl LiteralMatcher {
    fn new(needle: &str, case_insensitive: bool) -> std::result::Result<Self, ScanError> {
        let sigma_context = if case_insensitive && needle.contains(['σ', 'ς']) {
            let compile = |pattern| {
                regex::Regex::new(pattern)
                    .map_err(|err| ScanError::Unreadable(format!("Unicode matcher: {err}")))
            };
            Some((
                compile(r"\A\p{Cased}\z")?,
                compile(r"\A\p{Case_Ignorable}\z")?,
            ))
        } else {
            None
        };
        let needle = needle.as_bytes().to_vec();
        let mut prefixes = vec![0; needle.len()];
        let mut prefix = 0;
        for index in 1..needle.len() {
            while prefix > 0 && needle[index] != needle[prefix] {
                prefix = prefixes[prefix.saturating_sub(1)];
            }
            if needle[index] == needle[prefix] {
                prefix = prefix.saturating_add(1);
            }
            prefixes[index] = prefix;
        }
        Ok(Self {
            needle,
            prefixes,
            case_insensitive,
            sigma_context,
        })
    }

    fn line(&self) -> MatchingLine<'_> {
        MatchingLine {
            matcher: self,
            state: MatchState::default(),
            final_sigma: None,
            preceding_cased: false,
            undecoded: Vec::new(),
        }
    }

    fn advance(&self, state: &mut MatchState, bytes: &[u8]) {
        for byte in bytes {
            if state.matched {
                return;
            }
            while state.prefix > 0 && *byte != self.needle[state.prefix] {
                state.prefix = self.prefixes[state.prefix.saturating_sub(1)];
            }
            if *byte == self.needle[state.prefix] {
                state.prefix = state.prefix.saturating_add(1);
            }
            state.matched = state.prefix == self.needle.len();
        }
    }
}

#[derive(Clone, Copy, Default)]
struct MatchState {
    prefix: usize,
    matched: bool,
}

/// Per-line state holds only the matcher prefix, incomplete UTF-8, and two
/// possible sigma states until its following context decides the lowercase form.
struct MatchingLine<'a> {
    matcher: &'a LiteralMatcher,
    state: MatchState,
    final_sigma: Option<MatchState>,
    preceding_cased: bool,
    undecoded: Vec<u8>,
}

impl MatchingLine<'_> {
    fn feed(&mut self, chunk: &[u8]) {
        if self.state.matched && self.final_sigma.is_none() {
            return;
        }
        // Carry incomplete UTF-8 into the next chunk. Invalid sequences have
        // exactly the same replacement characters as from_utf8_lossy on a line.
        if self.undecoded.is_empty() {
            self.decode(chunk);
        } else {
            let mut bytes = std::mem::take(&mut self.undecoded);
            bytes.extend_from_slice(chunk);
            self.decode(&bytes);
        }
    }

    fn decode(&mut self, mut rest: &[u8]) {
        while !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    self.text(text);
                    rest = &[];
                }
                Err(err) => {
                    let valid = err.valid_up_to();
                    if let Ok(text) = std::str::from_utf8(&rest[..valid]) {
                        self.text(text);
                    }
                    rest = &rest[valid..];
                    if let Some(invalid) = err.error_len() {
                        self.character('�');
                        rest = &rest[invalid..];
                    } else {
                        break;
                    }
                }
            }
        }
        self.undecoded.extend_from_slice(rest);
    }

    fn text(&mut self, text: &str) {
        if self.matcher.case_insensitive {
            for character in text.chars() {
                self.character(character);
                if self.state.matched && self.final_sigma.is_none() {
                    break;
                }
            }
        } else {
            self.matcher.advance(&mut self.state, text.as_bytes());
        }
    }

    fn character(&mut self, character: char) {
        let mut encoded = [0; 4];
        if let Some((cased, ignorable)) = &self.matcher.sigma_context {
            let text = character.encode_utf8(&mut encoded);
            let (is_cased, is_ignorable) = if character.is_ascii_alphabetic() {
                (true, false)
            } else {
                (cased.is_match(text), ignorable.is_match(text))
            };
            if !is_ignorable
                && let Some(final_sigma) = self.final_sigma.take()
                && !is_cased
            {
                self.state = final_sigma;
            }
            if character == 'Σ' && self.preceding_cased {
                let mut alternate = self.state;
                self.matcher.advance(&mut alternate, "ς".as_bytes());
                self.final_sigma = Some(alternate);
                self.matcher.advance(&mut self.state, "σ".as_bytes());
                self.preceding_cased = true;
                return;
            }
            if !is_ignorable {
                self.preceding_cased = is_cased;
            }
        }
        if self.matcher.case_insensitive {
            for lower in character.to_lowercase() {
                self.advance(lower.encode_utf8(&mut encoded).as_bytes());
            }
        } else {
            self.advance(character.encode_utf8(&mut encoded).as_bytes());
        }
    }

    fn advance(&mut self, bytes: &[u8]) {
        self.matcher.advance(&mut self.state, bytes);
        if let Some(alternate) = &mut self.final_sigma {
            self.matcher.advance(alternate, bytes);
        }
    }

    fn finish(mut self) -> bool {
        if !self.undecoded.is_empty() {
            self.character('�');
        }
        self.final_sigma.unwrap_or(self.state).matched
    }
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
                    let _ = writeln!(body, "{}:{line}", hit.file);
                }
                let _ = writeln!(body, "{}:{:>6}:{}", hit.file, hit.number, hit.text);
                for line in &hit.trailing {
                    let _ = writeln!(body, "{}:{line}", hit.file);
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
    if query.mode != Mode::Count && total > 0 {
        let start = query.offset.min(total);
        let next = start.saturating_add(listed);
        if query.offset >= total {
            let _ = writeln!(
                footer,
                "[page is empty: offset {} is past the {total} entries]",
                query.offset
            );
        } else if query.offset > 0 || next < total {
            let first = start.saturating_add(1);
            if next < total {
                let _ = writeln!(
                    footer,
                    "[page: entries {first} to {next} of {total}; pass offset={next} for the next \
                     page]"
                );
            } else {
                let _ = writeln!(footer, "[page: entries {first} to {next} of {total}]");
            }
        }
    }
    if query.context_lines > 0 && query.mode != Mode::Matches {
        footer.push_str("[context_lines applies only to matches mode]\n");
    }
    if query.looks_like_regex {
        let _ = writeln!(
            footer,
            "[the pattern `{}` contains regex metacharacters, which were matched literally]",
            query.pattern
        );
    }
    if !report.binary.is_empty() {
        let _ = writeln!(
            footer,
            "[{} binary files skipped: {}]",
            report.binary.len(),
            summarize(&report.binary, MAX_LISTED_NAMES)
        );
    }
    if !report.unreadable.is_empty() {
        let reasons = report
            .unreadable
            .iter()
            .take(MAX_LISTED_NAMES)
            .map(|(file, reason)| format!("{file} ({reason})"))
            .collect::<Vec<_>>()
            .join("; ");
        let _ = writeln!(
            footer,
            "[{} files could not be read: {reasons}]",
            report.unreadable.len()
        );
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
    use super::*;
    use crate::contract::Cancellation;
    use crate::workspace::fixture::{MATCH_FILES, MATCH_LINES, Repo, budget_with_list_entries};

    /// Cancels at a known byte boundary without relying on thread scheduling.
    struct CancellingReader<R> {
        inner: R,
        cancellation: Cancellation,
        cancel_after: usize,
        consumed: usize,
        fills: usize,
    }

    impl<R: std::io::Read> std::io::Read for CancellingReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buffer)
        }
    }

    impl<R: BufRead> BufRead for CancellingReader<R> {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.fills = self.fills.checked_add(1).expect("buffer fills");
            self.inner.fill_buf()
        }

        fn consume(&mut self, amount: usize) {
            self.inner.consume(amount);
            self.consumed = self.consumed.checked_add(amount).expect("consumed bytes");
            if self.consumed >= self.cancel_after {
                self.cancellation.cancel();
            }
        }
    }

    #[test]
    fn a_cancelled_call_stops_before_accessing_the_file() {
        let repo = Repo::new();
        let context = repo.context();
        context.cancellation().cancel();

        for path in ["README.md", "absent.txt"] {
            let error = GrepFiles::new()
                .call(
                    &serde_json::json!({ "pattern": "needle", "path": path }),
                    &context,
                )
                .expect_err("cancelled before file access");
            assert_eq!(error.code(), ErrorCode::Cancelled);
        }
    }

    #[test]
    fn a_cancelled_scan_does_no_io() {
        let repo = Repo::new();
        let context = repo.context();
        let query = parse(
            &GrepFiles::new(),
            &serde_json::json!({ "pattern": "needle" }),
            &context,
        )
        .expect("query");
        context.cancellation().cancel();
        let mut reader = CancellingReader {
            inner: std::io::Cursor::new(b"needle\n"),
            cancellation: context.cancellation(),
            cancel_after: usize::MAX,
            consumed: 0,
            fills: 0,
        };
        let mut report = Report::default();

        let error = scan_reader(&mut reader, "large.txt", &query, 0, &mut report, &context)
            .expect_err("cancelled before reading");
        assert!(matches!(error, ScanError::Cancelled(err) if err.code() == ErrorCode::Cancelled));
        assert_eq!(reader.consumed, 0);
        assert_eq!(reader.fills, 0);
    }

    #[test]
    fn cancellation_stops_within_one_large_file_in_every_mode() {
        let repo = Repo::new();
        let body = "needle on a short line\n".repeat(64 * 1024);
        repo.write("large.txt", &body);
        for (mode, offset) in [
            ("matches", 0),
            ("matches", 100_000),
            ("files_with_matches", 0),
            ("count", 0),
        ] {
            let context = repo.context();
            let query = parse(
                &GrepFiles::new(),
                &serde_json::json!({
                    "pattern": "NEEDLE",
                    "path": "large.txt",
                    "case_insensitive": true,
                    "mode": mode,
                    "offset": offset,
                    "head_limit": 1,
                    "context_lines": 2,
                }),
                &context,
            )
            .expect("query");
            let mut reader = CancellingReader {
                inner: BufReader::with_capacity(
                    PROBE_BYTES,
                    std::fs::File::open(&query.root).expect("open"),
                ),
                cancellation: context.cancellation(),
                cancel_after: 3 * PROBE_BYTES,
                consumed: 0,
                fills: 0,
            };
            let mut report = Report::default();

            let error = scan_reader(&mut reader, "large.txt", &query, 0, &mut report, &context)
                .expect_err("cancelled during a single-file search");
            assert!(
                matches!(error, ScanError::Cancelled(err) if err.code() == ErrorCode::Cancelled)
            );
            assert!(reader.consumed >= reader.cancel_after);
            assert!(reader.consumed <= reader.cancel_after + PROBE_BYTES);
            assert!(reader.consumed < body.len());
            assert_eq!(report.matches, 0);
            assert_eq!(report.files, 0);
            assert!(report.hits.is_empty());
            assert!(report.named.is_empty());
        }
    }

    #[test]
    fn cancellation_stops_while_discarding_an_oversized_line() {
        let repo = Repo::new();
        let context = repo.context();
        let query = parse(
            &GrepFiles::new(),
            &serde_json::json!({ "pattern": "x" }),
            &context,
        )
        .expect("query");
        let body = "x".repeat(8 * MAX_MATCH_LINE_BYTES);
        let mut reader = CancellingReader {
            inner: BufReader::with_capacity(PROBE_BYTES, body.as_bytes()),
            cancellation: context.cancellation(),
            cancel_after: MAX_MATCH_LINE_BYTES + 3 * PROBE_BYTES,
            consumed: 0,
            fills: 0,
        };
        let mut report = Report::default();

        let error = scan_reader(&mut reader, "large.txt", &query, 0, &mut report, &context)
            .expect_err("cancelled while discarding a long line");
        assert!(matches!(error, ScanError::Cancelled(err) if err.code() == ErrorCode::Cancelled));
        assert_eq!(reader.consumed, reader.cancel_after);
        assert!(reader.consumed < body.len());
        assert_eq!(report.matches, 0);
        assert!(report.hits.is_empty());
    }

    #[test]
    fn cancellation_on_the_final_chunk_does_not_merge_partial_matches() {
        let repo = Repo::new();
        for body in [
            b"needle\nlast line\n".as_slice(),
            b"needle\nlast line".as_slice(),
        ] {
            let context = repo.context();
            let query = parse(
                &GrepFiles::new(),
                &serde_json::json!({ "pattern": "needle" }),
                &context,
            )
            .expect("query");
            let mut reader = CancellingReader {
                inner: BufReader::with_capacity(PROBE_BYTES, body),
                cancellation: context.cancellation(),
                cancel_after: body.len(),
                consumed: 0,
                fills: 0,
            };
            let mut report = Report::default();

            let error = scan_reader(&mut reader, "large.txt", &query, 0, &mut report, &context)
                .expect_err("cancelled at end of file");
            assert!(
                matches!(error, ScanError::Cancelled(err) if err.code() == ErrorCode::Cancelled)
            );
            assert_eq!(reader.consumed, body.len());
            assert_eq!(report.matches, 0);
            assert_eq!(report.files, 0);
            assert!(report.hits.is_empty());
        }
    }

    /// Builds a search with a lowered listing cap.
    fn search(entries: usize) -> GrepFiles {
        GrepFiles::with_limits(FileLimits::from_budget(&budget_with_list_entries(entries)))
    }

    /// Builds many matching lines of a fixed width.
    fn wide_lines(count: usize) -> String {
        let mut body = String::new();
        for n in 1..=count {
            let _ = writeln!(body, "wideword on line {n} {}", "z".repeat(40));
        }
        body
    }

    /// Returns the summary line of a result.
    fn summary(text: &str) -> String {
        text.lines()
            .find(|line| line.starts_with('['))
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

    #[cfg(unix)]
    #[test]
    fn a_search_never_reaches_through_a_link_that_leads_outside() {
        let repo = Repo::new();
        let elsewhere = tempfile::tempdir().expect("temp dir");
        std::fs::write(elsewhere.path().join("secret.txt"), "needle outside\n").expect("write");
        std::os::unix::fs::symlink(elsewhere.path(), repo.path().join("up")).expect("symlink");

        let err = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle", "path": "up" }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PathOutsideWorkspace);

        let whole = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "needle" }), &repo.context())
            .expect("call");
        assert!(!whole.text.contains("needle outside"), "{}", whole.text);
        assert!(whole.text.contains("README.md"), "{}", whole.text);
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
        assert_eq!(second.len(), MATCH_FILES - 4, "{second:?}");
        for name in &first {
            assert!(!second.contains(name), "overlap on {name}");
        }
        assert!(page(0).contains("pass offset=4"), "{}", page(0));
        // The second page is the last, so it offers no further page.
        assert!(!page(4).contains("next page"), "{}", page(4));
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
        assert!(
            output
                .text
                .contains("page is empty: offset 500 is past the"),
            "{}",
            output.text
        );
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
    fn an_extreme_context_argument_returns_a_validation_error() {
        let repo = Repo::new();
        repo.write("fixture.txt", "x\n");
        let arguments = serde_json::from_str(
            r#"{"pattern":"x","path":"fixture.txt","context_lines":18446744073709551615}"#,
        )
        .expect("arguments");
        let err = GrepFiles::default()
            .call(&arguments, &repo.context())
            .expect_err("context must be bounded before allocation");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.detail().field.as_deref(), Some("context_lines"));
    }

    #[test]
    fn context_over_the_maximum_is_rejected_in_every_mode() {
        let repo = Repo::new();
        repo.write("fixture.txt", "x\n");
        for mode in ["matches", "files_with_matches", "count"] {
            let err = GrepFiles::default()
                .call(
                    &serde_json::json!({
                        "pattern": "x",
                        "path": "fixture.txt",
                        "mode": mode,
                        "context_lines": MAX_CONTEXT_LINES.saturating_add(1),
                    }),
                    &repo.context(),
                )
                .expect_err("context exceeds the maximum");
            assert_eq!(err.code(), ErrorCode::InvalidField);
            assert_eq!(err.detail().field.as_deref(), Some("context_lines"));
            assert!(
                err.message().contains(&MAX_CONTEXT_LINES.to_string()),
                "{err}"
            );
        }
    }

    #[test]
    fn zero_and_maximum_context_windows_are_accepted_without_clamping() {
        let repo = Repo::new();
        let before = "before\n".repeat(MAX_CONTEXT_LINES);
        let after = "after\n".repeat(MAX_CONTEXT_LINES);
        repo.write("fixture.txt", &format!("{before}x\n{after}"));
        for context_lines in [0, MAX_CONTEXT_LINES] {
            let output = GrepFiles::default()
                .call(
                    &serde_json::json!({
                        "pattern": "x",
                        "path": "fixture.txt",
                        "context_lines": context_lines,
                    }),
                    &repo.context(),
                )
                .expect("valid context");
            assert!(!output.is_error);
            assert_eq!(
                lines_for(&output.text, "fixture.txt").len(),
                context_lines.saturating_mul(2).saturating_add(1),
                "{}",
                output.text
            );
        }
    }

    /// Returns the lines a search printed for one file, in order.
    fn lines_for(text: &str, file: &str) -> Vec<String> {
        let prefix = format!("{file}:");
        text.lines()
            .filter_map(|line| line.strip_prefix(&prefix))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn overlapping_context_is_shown_once_and_in_order() {
        let repo = Repo::new();
        repo.write("ctx/two.txt", "x\ny\nneedle A\nz\nneedle B\n");
        let output = GrepFiles::default()
            .call(
                &serde_json::json!({
                    "pattern": "needle",
                    "path": "ctx/two.txt",
                    "context_lines": 2,
                }),
                &repo.context(),
            )
            .expect("call");
        assert_eq!(
            lines_for(&output.text, "ctx/two.txt"),
            vec![
                "     1-x",
                "     2-y",
                "     3:needle A",
                "     4-z",
                "     5:needle B",
            ],
            "{}",
            output.text
        );
    }

    #[test]
    fn a_match_left_off_the_page_lends_no_context_to_an_earlier_hit() {
        let repo = Repo::new();
        repo.write("ctx/far.txt", "needle 1\na\nb\nc\nd\nneedle 2\ne\nf\n");
        let output = GrepFiles::default()
            .call(
                &serde_json::json!({
                    "pattern": "needle",
                    "path": "ctx/far.txt",
                    "context_lines": 1,
                    "head_limit": 1,
                }),
                &repo.context(),
            )
            .expect("call");
        assert_eq!(
            lines_for(&output.text, "ctx/far.txt"),
            vec!["     1:needle 1", "     2-a"],
            "{}",
            output.text
        );

        let later = GrepFiles::default()
            .call(
                &serde_json::json!({
                    "pattern": "needle",
                    "path": "ctx/far.txt",
                    "context_lines": 1,
                    "offset": 1,
                }),
                &repo.context(),
            )
            .expect("call");
        assert_eq!(
            lines_for(&later.text, "ctx/far.txt"),
            vec!["     5-d", "     6:needle 2", "     7-e"],
            "{}",
            later.text
        );
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
    fn a_negative_offset_is_refused_rather_than_clamped() {
        let repo = Repo::new();
        let err = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "needle", "offset": -1 }),
                &repo.context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("offset"));
    }

    #[test]
    fn a_wrongly_typed_argument_is_refused() {
        let repo = Repo::new();
        let err = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": 7 }), &repo.context())
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("pattern"));
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
        assert_eq!(err.field(), Some("include"));
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
        let body = wide_lines(500);
        repo.write("wide/many.txt", &body);

        let context = repo.context().with_output_cap(4096);
        let output = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "wideword" }), &context)
            .expect("call");
        assert!(output.text.len() <= 4096, "{}", output.text.len());
        assert!(
            output.text.contains("500 matching lines in 1 files"),
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
        let mut line = String::from("wideword ");
        line.push_str(&"q".repeat(200_000));
        line.push('\n');
        repo.write("wide/one.txt", &line);

        let context = repo.context().with_output_cap(64 * 1024);
        let output = GrepFiles::default()
            .call(&serde_json::json!({ "pattern": "wideword" }), &context)
            .expect("call");
        assert!(output.text.len() <= 64 * 1024, "{}", output.text.len());
        assert!(
            output.text.contains("1 matching lines in 1 files"),
            "{}",
            output.text
        );
    }

    #[test]
    fn streaming_search_preserves_whole_line_text_at_chunk_boundaries() {
        let repo = Repo::new();
        let cases: &[(&[u8], &[&str])] = &[
            (
                "prefix Éİ🦀 ΟΣ ΟΣΑ Σ AΣ' AΣ'B AΣ\u{301} AΣ\u{301}B\r\n".as_bytes(),
                &[
                    "Éİ🦀",
                    "éİ🦀",
                    "ΟΣ",
                    "οσ",
                    "ος",
                    "σ",
                    "ς",
                    "aς'",
                    "aσ'b",
                    "aς\u{301}",
                    "aσ\u{301}b",
                ],
            ),
            (b"a\xffb\xe2\x82c\xf0\x9f", &["a�b", "b�c�", "�", "absent"]),
            (b"ababababac\n", &["ababac", "abababad", "abac"]),
            (b"hidden\nneedle\n", &["hiddenneedle"]),
        ];
        for (bytes, patterns) in cases {
            repo.write_bytes("chunks.txt", bytes);
            for pattern in *patterns {
                for case_insensitive in [false, true] {
                    let source = String::from_utf8_lossy(bytes);
                    let expected = source
                        .lines()
                        .filter(|line| {
                            if case_insensitive {
                                line.to_lowercase().contains(&pattern.to_lowercase())
                            } else {
                                line.contains(pattern)
                            }
                        })
                        .count();
                    let context = repo.context();
                    let query = parse(
                        &GrepFiles::new(),
                        &serde_json::json!({ "pattern": pattern, "case_insensitive": case_insensitive }),
                        &context,
                    ).expect("query");
                    for capacity in [1, 2, 3, 7, PROBE_BYTES] {
                        let mut reader = BufReader::with_capacity(capacity, *bytes);
                        let mut report = Report::default();
                        scan_reader(&mut reader, "chunks.txt", &query, 0, &mut report, &context)
                            .expect("scan");
                        assert_eq!(
                            report.matches, expected,
                            "pattern={pattern:?}, insensitive={case_insensitive}, capacity={capacity}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn long_line_matches_span_chunks_and_the_retained_head_boundary() {
        let repo = Repo::new();
        for start in [
            MAX_MATCH_LINE_BYTES - 5,
            MAX_MATCH_LINE_BYTES + PROBE_BYTES - 5,
        ] {
            for ending in ["\n", "\r\n", ""] {
                repo.write(
                    "long.txt",
                    &format!("{}hiddenneedle{ending}", "a".repeat(start)),
                );
                let output = GrepFiles::new().call(
                    &serde_json::json!({ "pattern": "hiddenneedle", "path": "long.txt", "mode": "count" }),
                    &repo.context(),
                ).expect("call");
                assert!(summary(&output.text).contains("1 matching lines in 1 files"));
            }
        }
        let pattern = format!("{}b", "a".repeat(MAX_MATCH_LINE_BYTES + 1));
        repo.write("long.txt", &format!("aaaa{pattern}\n"));
        let output = GrepFiles::new()
            .call(
                &serde_json::json!({ "pattern": pattern, "path": "long.txt", "mode": "count" }),
                &repo.context(),
            )
            .expect("call");
        assert!(summary(&output.text).contains("1 matching lines in 1 files"));
    }

    #[test]
    fn long_line_tail_matches_keep_pagination_context_and_binary_detection() {
        let repo = Repo::new();
        let line = format!("{}hiddenneedle", "a".repeat(MAX_MATCH_LINE_BYTES + 1));
        repo.write(
            "long.txt",
            &format!("hiddenneedle\nbefore\n{line}\nafter\n"),
        );
        let context = repo.context().with_output_cap(4096);
        let output = GrepFiles::new()
            .call(
                &serde_json::json!({
                    "pattern": "hiddenneedle", "path": "long.txt",
                    "head_limit": 1, "offset": 1, "context_lines": 1,
                }),
                &context,
            )
            .expect("call");
        assert!(summary(&output.text).contains("2 matching lines in 1 files"));
        assert!(output.text.contains("long.txt:     2-before"));
        assert!(output.text.contains("long.txt:     3:"));
        assert!(!output.text.contains("long.txt:     1:"));
        assert!(output.text.len() <= 4096);

        // Finding a match does not stop reading or checking the rest of a file.
        repo.write_bytes("long.txt", format!("{line}\0\n").as_bytes());
        for mode in ["matches", "files_with_matches", "count"] {
            let output = GrepFiles::new().call(
                &serde_json::json!({ "pattern": "hiddenneedle", "path": "long.txt", "mode": mode }),
                &context,
            ).expect("call");
            assert!(summary(&output.text).contains("0 matching lines in 0 files"));
            assert!(output.text.contains("1 binary files skipped"));
        }
    }

    #[test]
    fn matches_past_the_retained_line_head_are_counted_in_every_mode() {
        let repo = Repo::new();
        repo.write(
            "long.txt",
            &format!(
                "{}hiddenneedle hiddenneedle\n",
                "a".repeat(MAX_MATCH_LINE_BYTES + 1)
            ),
        );
        let context = repo.context().with_output_cap(4096);
        for mode in ["matches", "files_with_matches", "count"] {
            for case_insensitive in [false, true] {
                let output = GrepFiles::new()
                    .call(
                        &serde_json::json!({
                            "pattern": if case_insensitive { "HIDDENNEEDLE" } else { "hiddenneedle" },
                            "path": "long.txt",
                            "mode": mode,
                            "case_insensitive": case_insensitive,
                        }),
                        &context,
                    )
                    .expect("call");
                assert!(!output.is_error);
                assert!(summary(&output.text).contains("1 matching lines in 1 files"));
                assert!(output.text.len() <= 4096);
                if mode == "matches" {
                    assert!(output.text.contains("long.txt:     1:"));
                    assert!(output.text.contains("output truncated at the byte cap"));
                }
            }
        }
    }

    #[test]
    fn a_long_line_truncation_annotation_is_not_searched() {
        let repo = Repo::new();
        let line = format!("wideword {}\n", "q".repeat(MAX_MATCH_LINE_BYTES + 1));
        assert!(!line.contains("line truncated"));
        repo.write("wide/one.txt", &line);

        let cap = 2 * MAX_MATCH_LINE_BYTES;
        let context = repo.context().with_output_cap(cap);
        let tool = GrepFiles::with_limits(FileLimits {
            output_bytes: cap,
            ..FileLimits::default()
        });
        let rendered = tool
            .call(
                &serde_json::json!({ "pattern": "wideword", "path": "wide/one.txt" }),
                &context,
            )
            .expect("call");
        assert!(!rendered.is_error);
        assert!(rendered.text.contains("... [line truncated, the line is "));
        assert!(summary(&rendered.text).contains("1 matching lines in 1 files"));
        assert!(rendered.text.len() <= cap);

        for mode in ["matches", "files_with_matches", "count"] {
            for case_insensitive in [false, true] {
                let pattern = if case_insensitive {
                    "LINE TRUNCATED"
                } else {
                    "line truncated"
                };
                let output = tool
                    .call(
                        &serde_json::json!({
                            "pattern": pattern,
                            "path": "wide/one.txt",
                            "mode": mode,
                            "case_insensitive": case_insensitive,
                        }),
                        &context,
                    )
                    .expect("call");
                assert!(!output.is_error);
                assert!(
                    summary(&output.text).contains("0 matching lines in 0 files"),
                    "mode={mode}, case_insensitive={case_insensitive}: {}",
                    summary(&output.text)
                );
            }
        }
    }

    #[test]
    fn a_literal_truncation_phrase_in_a_long_line_still_matches() {
        let repo = Repo::new();
        let line = format!("LINE TRUNCATED {}\n", "q".repeat(MAX_MATCH_LINE_BYTES + 1));
        repo.write("wide/one.txt", &line);

        for mode in ["matches", "files_with_matches", "count"] {
            for case_insensitive in [false, true] {
                let pattern = if case_insensitive {
                    "line truncated"
                } else {
                    "LINE TRUNCATED"
                };
                let output = GrepFiles::default()
                    .call(
                        &serde_json::json!({
                            "pattern": pattern,
                            "path": "wide/one.txt",
                            "mode": mode,
                            "case_insensitive": case_insensitive,
                        }),
                        &repo.context(),
                    )
                    .expect("call");
                assert!(!output.is_error);
                assert!(
                    summary(&output.text).contains("1 matching lines in 1 files"),
                    "mode={mode}, case_insensitive={case_insensitive}: {}",
                    summary(&output.text)
                );
            }
        }
    }

    #[test]
    fn a_walk_cap_is_reported_in_the_result() {
        let repo = Repo::new();
        for index in 0..70 {
            repo.write(&format!("many/file_{index}.txt"), "wideword\n");
        }
        let capped = GrepFiles::with_limits(FileLimits {
            walk_files: 4,
            ..FileLimits::default()
        });
        let output = capped
            .call(
                &serde_json::json!({ "pattern": "wideword" }),
                &repo.context(),
            )
            .expect("call");
        assert!(output.text.contains("searched 4 files"), "{}", output.text);
        assert!(
            output.text.contains("the walk stopped after 4 files"),
            "{}",
            output.text
        );

        // Without a cap the same search sees every seeded file.
        let full = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "wideword" }),
                &repo.context(),
            )
            .expect("call");
        assert!(
            full.text.contains("70 matching lines in 70 files"),
            "{}",
            full.text
        );
    }

    #[test]
    fn binary_content_past_the_probe_is_skipped_without_counting_matches() {
        let repo = Repo::new();
        let mut bytes = "wideword here\n".repeat(2000).into_bytes();
        bytes.extend_from_slice(&[0x00, 0x01]);
        bytes.extend_from_slice(b"wideword later\n");
        repo.write_bytes("data/late.bin", &bytes);

        let output = GrepFiles::default()
            .call(
                &serde_json::json!({ "pattern": "wideword" }),
                &repo.context(),
            )
            .expect("call");
        assert!(!output.is_error);
        assert!(output.text.contains("data/late.bin"), "{}", output.text);
        assert!(
            output.text.contains("binary files skipped"),
            "{}",
            output.text
        );
        assert!(
            output.text.contains("0 matching lines in 0 files"),
            "{}",
            output.text
        );
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
        assert_eq!(schema["properties"]["context_lines"]["minimum"], 0);
        assert_eq!(
            schema["properties"]["context_lines"]["maximum"],
            MAX_CONTEXT_LINES
        );
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
