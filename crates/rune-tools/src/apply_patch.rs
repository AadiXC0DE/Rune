//! The file patch format: `*** Begin Patch`, file sections, `*** End Patch`.

use rune_core::error::{ErrorCode, Result, RuneError};

use crate::matching::{closest_lines, fold};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ENVIRONMENT: &str = "*** Environment ID:";
const ADD: &str = "*** Add File:";
const DELETE: &str = "*** Delete File:";
const UPDATE: &str = "*** Update File:";
const MOVE: &str = "*** Move to:";
const END_OF_FILE: &str = "*** End of File";

/// Most characters of an input line quoted in a parse error.
const MAX_QUOTED_CHARS: usize = 120;

/// A parsed patch.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Patch {
    pub sections: Vec<Section>,
}

/// One file operation of a patch.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Section {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<Chunk>,
    },
}

/// One run of changed lines in an updated file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Chunk {
    /// Text after `@@ `, sought before the chunk's old lines.
    pub context: Option<String>,
    /// The chunk's lines in order, each tagged.
    pub lines: Vec<(LineKind, String)>,
    pub end_of_file: bool,
}

/// What a chunk line contributes: to both versions, the old only, or the new only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    Context,
    Old,
    New,
}

impl Chunk {
    /// Context and old lines, in order.
    #[must_use]
    pub fn old_lines(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter(|(kind, _)| *kind != LineKind::New)
            .map(|(_, text)| text.as_str())
            .collect()
    }

    /// Context and new lines, in order.
    #[must_use]
    pub fn new_lines(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter(|(kind, _)| *kind != LineKind::Old)
            .map(|(_, text)| text.as_str())
            .collect()
    }
}

/// Every path the patch names (sources and move destinations), in order;
/// empty when it does not parse.
#[must_use]
pub fn paths(input: &str) -> Vec<String> {
    let Ok(patch) = parse(input) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for section in patch.sections {
        match section {
            Section::Add { path, .. } | Section::Delete { path } => paths.push(path),
            Section::Update { path, move_to, .. } => {
                paths.push(path);
                paths.extend(move_to);
            }
        }
    }
    paths
}

/// Parses a patch.
///
/// # Errors
///
/// Returns an `invalid_field` error on `input` that names the offending line.
pub fn parse(input: &str) -> Result<Patch> {
    let trimmed = input.trim();
    let leading = input.len().saturating_sub(input.trim_start().len());
    let first_number = input
        .get(..leading)
        .unwrap_or_default()
        .matches('\n')
        .count()
        .saturating_add(1);
    let lines: Vec<&str> = trimmed
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let (body, first_number) = match envelope(&lines) {
        Ok(body) => (body, first_number.saturating_add(1)),
        Err(error) => match unwrap_heredoc(&lines) {
            Some(inner) => (envelope(inner)?, first_number.saturating_add(2)),
            None => return Err(error),
        },
    };
    Parser {
        lines: body,
        first_number,
        index: 0,
    }
    .sections()
}

/// The lines between the markers, or the marker error.
fn envelope<'a>(lines: &'a [&'a str]) -> Result<&'a [&'a str]> {
    let first = "the first line of the patch must be '*** Begin Patch'";
    let last = "the last line of the patch must be '*** End Patch'";
    match lines.split_first() {
        Some((begin, rest)) if begin.trim() == BEGIN => match rest.split_last() {
            Some((end, body)) if end.trim() == END => Ok(body),
            _ => Err(invalid(last)),
        },
        _ => Err(invalid(first)),
    }
}

/// The lines inside a `<<EOF` ... `EOF` wrapper.
fn unwrap_heredoc<'a>(lines: &'a [&'a str]) -> Option<&'a [&'a str]> {
    if lines.len() < 4 {
        return None;
    }
    let (first, rest) = lines.split_first()?;
    let (last, inner) = rest.split_last()?;
    let opener = first
        .strip_prefix("apply_patch ")
        .or_else(|| first.strip_prefix("cat "))
        .unwrap_or(first);
    if !matches!(opener, "<<EOF" | "<<'EOF'" | "<<\"EOF\"") {
        return None;
    }
    last.ends_with("EOF").then_some(inner)
}

struct Parser<'a> {
    lines: &'a [&'a str],
    /// The 1-based input line number of `lines[0]`.
    first_number: usize,
    index: usize,
}

impl Parser<'_> {
    fn number(&self, index: usize) -> usize {
        self.first_number.saturating_add(index)
    }

    fn sections(mut self) -> Result<Patch> {
        if self
            .lines
            .first()
            .is_some_and(|line| line.trim().starts_with(ENVIRONMENT))
        {
            self.index = 1;
        }
        let mut sections = Vec::new();
        while self.index < self.lines.len() {
            let at = self.index;
            let line = self.lines[at].trim();
            self.index = self.index.saturating_add(1);
            if let Some(rest) = line.strip_prefix(ADD) {
                let path = self.path(rest, at)?;
                sections.push(self.add(path)?);
            } else if let Some(rest) = line.strip_prefix(DELETE) {
                let path = self.path(rest, at)?;
                sections.push(Section::Delete { path });
            } else if let Some(rest) = line.strip_prefix(UPDATE) {
                let path = self.path(rest, at)?;
                sections.push(self.update(path, at)?);
            } else {
                return Err(invalid(format!(
                    "line {}: '{}' is not a section header; use '*** Add File: <path>', \
                     '*** Delete File: <path>' or '*** Update File: <path>'",
                    self.number(at),
                    quoted(self.lines[at]),
                )));
            }
        }
        Ok(Patch { sections })
    }

    fn path(&self, rest: &str, at: usize) -> Result<String> {
        let path = rest.trim();
        if path.is_empty() {
            return Err(invalid(format!(
                "line {}: a section header needs a path",
                self.number(at)
            )));
        }
        Ok(path.to_owned())
    }

    /// Whether the next line opens a section. Inside an update only trailing
    /// space is ignored, so an indented context line is never a header.
    fn at_header(&self, trim_start: bool) -> bool {
        self.lines.get(self.index).is_some_and(|line| {
            let line = line.trim_end();
            is_header(if trim_start { line.trim_start() } else { line })
        })
    }

    fn add(&mut self, path: String) -> Result<Section> {
        let mut contents = String::new();
        while self.index < self.lines.len() && !self.at_header(true) {
            let at = self.index;
            let Some(text) = self.lines[at].strip_prefix('+') else {
                return Err(invalid(format!(
                    "line {}: every line of the added file '{path}' starts with '+'",
                    self.number(at)
                )));
            };
            contents.push_str(text);
            contents.push('\n');
            self.index = self.index.saturating_add(1);
        }
        Ok(Section::Add { path, contents })
    }

    fn update(&mut self, path: String, header: usize) -> Result<Section> {
        let mut move_to = None;
        if let Some(rest) = self
            .lines
            .get(self.index)
            .and_then(|line| line.trim().strip_prefix(MOVE))
        {
            move_to = Some(self.path(rest, self.index)?);
            self.index = self.index.saturating_add(1);
        }
        let mut chunks: Vec<Chunk> = Vec::new();
        // The open chunk and the line that opened it.
        let mut open: Option<(Chunk, usize)> = None;
        let mut closed_at_end = false;
        while self.index < self.lines.len() && !self.at_header(false) {
            let at = self.index;
            let line = self.lines[at];
            let marker = line.trim_end();
            self.index = self.index.saturating_add(1);
            if marker.starts_with(MOVE) {
                return Err(invalid(format!(
                    "line {}: '*** Move to:' must directly follow '*** Update File: {path}'",
                    self.number(at)
                )));
            }
            if marker == "@@" || marker.starts_with("@@ ") {
                if let Some(chunk) = self.close(open.take(), &path)? {
                    chunks.push(chunk);
                }
                let context = marker
                    .strip_prefix("@@ ")
                    .filter(|text| !text.trim().is_empty())
                    .map(str::to_owned);
                open = Some((
                    Chunk {
                        context,
                        lines: Vec::new(),
                        end_of_file: false,
                    },
                    at,
                ));
                closed_at_end = false;
                continue;
            }
            if marker == END_OF_FILE {
                let Some((mut chunk, opened)) = open.take() else {
                    return Err(self.stray(&path, at));
                };
                chunk.end_of_file = true;
                if let Some(chunk) = self.close(Some((chunk, opened)), &path)? {
                    chunks.push(chunk);
                }
                closed_at_end = true;
                continue;
            }
            if closed_at_end {
                if line.trim().is_empty() {
                    continue;
                }
                return Err(self.stray(&path, at));
            }
            let (kind, text) = if line.is_empty() {
                (LineKind::Context, "")
            } else if let Some(text) = line.strip_prefix(' ') {
                (LineKind::Context, text)
            } else if let Some(text) = line.strip_prefix('-') {
                (LineKind::Old, text)
            } else if let Some(text) = line.strip_prefix('+') {
                (LineKind::New, text)
            } else {
                return Err(self.stray(&path, at));
            };
            let (chunk, _) = open.get_or_insert_with(|| {
                (
                    Chunk {
                        context: None,
                        lines: Vec::new(),
                        end_of_file: false,
                    },
                    at,
                )
            });
            chunk.lines.push((kind, text.to_owned()));
        }
        if let Some(chunk) = self.close(open.take(), &path)? {
            chunks.push(chunk);
        }
        if chunks.is_empty() && move_to.is_none() {
            return Err(invalid(format!(
                "line {}: the update of '{path}' has no changes",
                self.number(header)
            )));
        }
        Ok(Section::Update {
            path,
            move_to,
            chunks,
        })
    }

    fn close(&self, open: Option<(Chunk, usize)>, path: &str) -> Result<Option<Chunk>> {
        let Some((chunk, opened)) = open else {
            return Ok(None);
        };
        if chunk.lines.is_empty() {
            return Err(invalid(format!(
                "line {}: a chunk of '{path}' has no lines",
                self.number(opened)
            )));
        }
        Ok(Some(chunk))
    }

    fn stray(&self, path: &str, at: usize) -> RuneError {
        invalid(format!(
            "line {}: in the update of '{path}', '{}' does not start with ' ', '-' or '+'",
            self.number(at),
            quoted(self.lines[at]),
        ))
    }
}

/// Finds `pattern` in `lines` at or after `start`: exact, then equal after trim_end, then
/// after trim, then after trim and the punctuation fold. Each pass scans the whole range
/// before the next. With `eof`, the search starts at `lines.len() - pattern.len()`.
#[must_use]
pub fn seek(lines: &[&str], pattern: &[&str], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(if eof { lines.len() } else { start });
    }
    let last = lines.len().checked_sub(pattern.len())?;
    let from = if eof { start.max(last) } else { start };
    if from > last {
        return None;
    }
    (0..4u8).find_map(|pass| {
        (from..=last).find(|&at| {
            lines.get(at..).is_some_and(|window| {
                window
                    .iter()
                    .zip(pattern)
                    .all(|(line, wanted)| equal_in_pass(pass, line, wanted))
            })
        })
    })
}

/// Line equality for one pass of `seek`.
fn equal_in_pass(pass: u8, line: &str, wanted: &str) -> bool {
    match pass {
        0 => line == wanted,
        1 => line.trim_end() == wanted.trim_end(),
        2 => line.trim() == wanted.trim(),
        _ => line
            .trim()
            .chars()
            .map(fold)
            .eq(wanted.trim().chars().map(fold)),
    }
}

/// A file's new state after one section, computed without writing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Change {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        contents: String,
        added: usize,
        removed: usize,
    },
}

/// Computes every section's result in memory, in order; writes nothing.
///
/// `read` returns the current text of a path, `None` when it is missing.
/// Sections that touch a path an earlier section changed see that result.
/// With `quote`, a missed chunk quotes the closest file lines.
///
/// # Errors
///
/// Returns the first section that cannot apply, named by number, or an
/// error from `read`.
pub fn compute(
    patch: &Patch,
    read: &dyn Fn(&str) -> Result<Option<String>>,
    quote: bool,
) -> Result<Vec<Change>> {
    let mut overlay: Vec<(String, Option<String>)> = Vec::new();
    let current = |overlay: &[(String, Option<String>)], path: &str| -> Result<Option<String>> {
        match overlay.iter().rev().find(|(known, _)| known == path) {
            Some((_, text)) => Ok(text.clone()),
            None => read(path),
        }
    };
    let mut changes = Vec::with_capacity(patch.sections.len());
    for (index, section) in patch.sections.iter().enumerate() {
        let k = index.saturating_add(1);
        match section {
            Section::Add { path, contents } => {
                if current(&overlay, path)?.is_some() {
                    return Err(RuneError::new(
                        ErrorCode::AlreadyExists,
                        format!("section {k}: `{path}` already exists; use '*** Update File'"),
                    ));
                }
                overlay.push((path.clone(), Some(contents.clone())));
                changes.push(Change::Add {
                    path: path.clone(),
                    contents: contents.clone(),
                });
            }
            Section::Delete { path } => {
                if current(&overlay, path)?.is_none() {
                    return Err(missing(k, path));
                }
                overlay.push((path.clone(), None));
                changes.push(Change::Delete { path: path.clone() });
            }
            Section::Update {
                path,
                move_to,
                chunks,
            } => {
                let Some(source) = current(&overlay, path)? else {
                    return Err(missing(k, path));
                };
                if let Some(dest) = move_to
                    && dest != path
                    && current(&overlay, dest)?.is_some()
                {
                    return Err(RuneError::new(
                        ErrorCode::AlreadyExists,
                        format!(
                            "section {k}: `{dest}` already exists, so `{path}` cannot be moved there"
                        ),
                    ));
                }
                let (contents, added, removed) = update(&source, chunks, k, path, quote)?;
                match move_to {
                    Some(dest) => {
                        overlay.push((path.clone(), None));
                        overlay.push((dest.clone(), Some(contents.clone())));
                    }
                    None => overlay.push((path.clone(), Some(contents.clone()))),
                }
                changes.push(Change::Update {
                    path: path.clone(),
                    move_to: move_to.clone(),
                    contents,
                    added,
                    removed,
                });
            }
        }
    }
    Ok(changes)
}

fn missing(k: usize, path: &str) -> RuneError {
    RuneError::new(
        ErrorCode::NotFound,
        format!("section {k}: `{path}` does not exist; use '*** Add File' to create it"),
    )
}

/// One file line: its text and its terminator (`\n`, `\r\n` or empty).
#[derive(Clone, Copy)]
struct Line<'a> {
    text: &'a str,
    ending: &'a str,
}

fn split_lines(source: &str) -> Vec<Line<'_>> {
    source
        .split_inclusive('\n')
        .map(|line| {
            let text = line
                .strip_suffix("\r\n")
                .or_else(|| line.strip_suffix('\n'))
                .unwrap_or(line);
            Line {
                text,
                ending: line.get(text.len()..).unwrap_or_default(),
            }
        })
        .collect()
}

/// The terminator for lines written next to line `at`: its own, else the
/// nearest earlier one, else the nearest later one, else `\n`.
fn ending_near<'a>(lines: &[Line<'a>], at: usize) -> &'a str {
    let at = at.min(lines.len().saturating_sub(1));
    let earlier = lines.get(..=at).unwrap_or_default().iter().rev();
    let later = lines.get(at..).unwrap_or_default().iter();
    earlier
        .chain(later)
        .map(|line| line.ending)
        .find(|ending| !ending.is_empty())
        .unwrap_or("\n")
}

/// A run of output lines replacing `len` original lines from `start`.
struct Replacement {
    start: usize,
    len: usize,
    lines: Vec<(String, String)>,
}

/// Applies one Update section's chunks to `source`.
fn update(
    source: &str,
    chunks: &[Chunk],
    k: usize,
    path: &str,
    quote: bool,
) -> Result<(String, usize, usize)> {
    let bom = source.starts_with('\u{FEFF}');
    let lines = split_lines(source);
    let texts: Vec<&str> = lines
        .iter()
        .enumerate()
        .map(|(at, line)| {
            if at == 0 {
                line.text.strip_prefix('\u{FEFF}').unwrap_or(line.text)
            } else {
                line.text
            }
        })
        .collect();
    let mut cursor = 0usize;
    let mut replacements: Vec<Replacement> = Vec::with_capacity(chunks.len());
    let (mut added, mut removed) = (0usize, 0usize);
    for (index, chunk) in chunks.iter().enumerate() {
        let c = index.saturating_add(1);
        if let Some(context) = &chunk.context {
            let Some(at) = seek(&texts, &[context.as_str()], cursor, false) else {
                return Err(RuneError::new(
                    ErrorCode::NotFound,
                    format!(
                        "section {k}, chunk {c}: the line `{context}` was not found in `{path}`"
                    ),
                ));
            };
            cursor = at.saturating_add(1);
        }
        let mut tagged: Vec<(LineKind, &str)> = chunk
            .lines
            .iter()
            .map(|(kind, text)| (*kind, text.as_str()))
            .collect();
        let mut old = old_of(&tagged);
        if old.is_empty() {
            let at = if chunk.context.is_some() {
                cursor
            } else {
                lines.len()
            };
            let ending = ending_near(&lines, at.saturating_sub(1));
            let new: Vec<(String, String)> = tagged
                .iter()
                .map(|(_, text)| ((*text).to_owned(), ending.to_owned()))
                .collect();
            added = added.saturating_add(new.len());
            replacements.push(Replacement {
                start: at,
                len: 0,
                lines: new,
            });
            continue;
        }
        let mut found = seek(&texts, &old, cursor, chunk.end_of_file);
        if found.is_none() && old.last() == Some(&"") {
            drop_trailing_empty(&mut tagged);
            old = old_of(&tagged);
            found = seek(&texts, &old, cursor, chunk.end_of_file);
        }
        let Some(at) = found else {
            return Err(expected_lines(
                source,
                &chunk.old_lines(),
                k,
                c,
                path,
                quote,
            ));
        };
        let ending = ending_near(&lines, at);
        let mut out = Vec::with_capacity(tagged.len());
        let mut original = at;
        for (kind, text) in &tagged {
            match kind {
                LineKind::Context => {
                    if let Some(line) = lines.get(original) {
                        out.push((line.text.to_owned(), line.ending.to_owned()));
                    }
                    original = original.saturating_add(1);
                }
                LineKind::Old => {
                    removed = removed.saturating_add(1);
                    original = original.saturating_add(1);
                }
                LineKind::New => {
                    added = added.saturating_add(1);
                    out.push(((*text).to_owned(), ending.to_owned()));
                }
            }
        }
        replacements.push(Replacement {
            start: at,
            len: old.len(),
            lines: out,
        });
        cursor = at.saturating_add(old.len());
    }
    replacements.sort_by_key(|replacement| replacement.start);
    let mut output: Vec<(String, String)> = Vec::with_capacity(lines.len());
    let mut copied = 0usize;
    for replacement in replacements {
        for line in lines.get(copied..replacement.start).unwrap_or_default() {
            output.push((line.text.to_owned(), line.ending.to_owned()));
        }
        output.extend(replacement.lines);
        copied = copied.max(replacement.start.saturating_add(replacement.len));
    }
    for line in lines.get(copied..).unwrap_or_default() {
        output.push((line.text.to_owned(), line.ending.to_owned()));
    }
    let fallback = ending_near(&lines, 0);
    let last = output.len().saturating_sub(1);
    let mut contents = String::with_capacity(source.len());
    for (at, (text, ending)) in output.iter().enumerate() {
        contents.push_str(text);
        if !ending.is_empty() {
            contents.push_str(ending);
        } else if at < last {
            contents.push_str(fallback);
        }
    }
    if !source.is_empty()
        && !source.ends_with('\n')
        && let Some(stripped) = contents
            .strip_suffix("\r\n")
            .or_else(|| contents.strip_suffix('\n'))
    {
        contents.truncate(stripped.len());
    }
    if bom && !contents.starts_with('\u{FEFF}') {
        contents.insert(0, '\u{FEFF}');
    }
    Ok((contents, added, removed))
}

fn old_of<'a>(tagged: &[(LineKind, &'a str)]) -> Vec<&'a str> {
    tagged
        .iter()
        .filter(|(kind, _)| *kind != LineKind::New)
        .map(|(_, text)| *text)
        .collect()
}

/// Drops the last old line (an empty one) and, when it was only old, a
/// trailing empty new line.
fn drop_trailing_empty(tagged: &mut Vec<(LineKind, &str)>) {
    let Some(at) = tagged.iter().rposition(|(kind, _)| *kind != LineKind::New) else {
        return;
    };
    let (kind, _) = tagged.remove(at);
    if kind == LineKind::Old
        && let Some(new) = tagged.iter().rposition(|(kind, _)| *kind == LineKind::New)
        && tagged.get(new).is_some_and(|(_, text)| text.is_empty())
        && tagged
            .iter()
            .skip(new.saturating_add(1))
            .all(|(kind, _)| *kind == LineKind::New)
    {
        tagged.remove(new);
    }
}

fn expected_lines(
    source: &str,
    old: &[&str],
    k: usize,
    c: usize,
    path: &str,
    quote: bool,
) -> RuneError {
    let missing = format!("section {k}, chunk {c}: the expected lines were not found in `{path}`");
    if quote && let Some(closest) = closest_lines(source, &old.join("\n")) {
        let lines = if closest.first == closest.last {
            format!("is line {}", closest.first)
        } else {
            format!("is lines {}-{}", closest.first, closest.last)
        };
        let mut message = format!("{missing}. The closest text {lines}:\n{}", closest.numbered);
        if let Some(note) = closest.indent_note {
            message.push('\n');
            message.push_str(note);
        }
        return RuneError::new(ErrorCode::NotFound, message)
            .with_hint("copy those lines exactly into the patch; nothing was written");
    }
    RuneError::new(ErrorCode::NotFound, missing)
        .with_hint("read the file again; nothing was written")
}

fn is_header(line: &str) -> bool {
    [ADD, DELETE, UPDATE]
        .iter()
        .any(|header| line.starts_with(header))
}

fn quoted(line: &str) -> String {
    line.chars().take(MAX_QUOTED_CHARS).collect()
}

fn invalid(message: impl Into<String>) -> RuneError {
    RuneError::invalid_field("input", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(input: &str) -> String {
        parse(input).unwrap_err().message().to_owned()
    }

    fn update_chunks(patch: &Patch) -> &[Chunk] {
        match &patch.sections[0] {
            Section::Update { chunks, .. } => chunks,
            other => panic!("expected an update, got {other:?}"),
        }
    }

    const EXAMPLE: &str = "*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print(\"Hi\")
+print(\"Hello, world!\")
*** Delete File: obsolete.txt
*** End Patch";

    #[test]
    fn the_reference_example_parses() {
        let patch = parse(EXAMPLE).unwrap();
        assert_eq!(
            patch.sections,
            vec![
                Section::Add {
                    path: "hello.txt".into(),
                    contents: "Hello world\n".into(),
                },
                Section::Update {
                    path: "src/app.py".into(),
                    move_to: Some("src/main.py".into()),
                    chunks: vec![Chunk {
                        context: Some("def greet():".into()),
                        lines: vec![
                            (LineKind::Old, "print(\"Hi\")".into()),
                            (LineKind::New, "print(\"Hello, world!\")".into()),
                        ],
                        end_of_file: false,
                    }],
                },
                Section::Delete {
                    path: "obsolete.txt".into(),
                },
            ]
        );
    }

    #[test]
    fn a_heredoc_wrapper_is_stripped() {
        for opener in [
            "<<EOF",
            "<<'EOF'",
            "<<\"EOF\"",
            "apply_patch <<'EOF'",
            "cat <<EOF",
        ] {
            let wrapped = format!("{opener}\n{EXAMPLE}\nEOF\n");
            assert_eq!(
                parse(&wrapped).unwrap(),
                parse(EXAMPLE).unwrap(),
                "{opener}"
            );
        }
    }

    #[test]
    fn a_mismatched_heredoc_quote_is_rejected() {
        let wrapped = format!("<<\"EOF'\n{EXAMPLE}\nEOF");
        assert_eq!(
            message(&wrapped),
            "the first line of the patch must be '*** Begin Patch'"
        );
    }

    #[test]
    fn a_heredoc_keeps_input_line_numbers() {
        let wrapped = "<<'EOF'\n*** Begin Patch\nstray\n*** End Patch\nEOF";
        assert!(message(wrapped).starts_with("line 3: 'stray' is not a section header"));
    }

    #[test]
    fn markers_with_surrounding_spaces_are_accepted() {
        let spaced = "\n  *** Begin Patch  \n*** Delete File: a.txt\n   *** End Patch \n\n";
        assert_eq!(
            parse(spaced).unwrap().sections,
            vec![Section::Delete {
                path: "a.txt".into()
            }]
        );
    }

    #[test]
    fn missing_markers_are_named() {
        assert_eq!(
            message("*** Delete File: a\n*** End Patch"),
            "the first line of the patch must be '*** Begin Patch'"
        );
        assert_eq!(
            message("*** Begin Patch\n*** Delete File: a"),
            "the last line of the patch must be '*** End Patch'"
        );
        assert_eq!(
            message("*** Begin Patch"),
            "the last line of the patch must be '*** End Patch'"
        );
    }

    #[test]
    fn an_environment_line_is_ignored() {
        let patch =
            parse("*** Begin Patch\n*** Environment ID: dev\n*** Delete File: a\n*** End Patch")
                .unwrap();
        assert_eq!(patch.sections.len(), 1);
    }

    #[test]
    fn an_implicit_first_chunk_needs_no_marker() {
        let patch = parse(
            "*** Begin Patch\n*** Update File: a.txt\n one\n-two\n+three\n@@ four\n-five\n*** End Patch",
        )
        .unwrap();
        let chunks = update_chunks(&patch);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].context, None);
        assert_eq!(chunks[0].old_lines(), ["one", "two"]);
        assert_eq!(chunks[0].new_lines(), ["one", "three"]);
        assert_eq!(chunks[1].context.as_deref(), Some("four"));
        assert_eq!(chunks[1].old_lines(), ["five"]);
        assert!(chunks[1].new_lines().is_empty());
    }

    #[test]
    fn an_empty_line_is_an_empty_context_line() {
        let patch =
            parse("*** Begin Patch\n*** Update File: a.txt\n@@\n a\n\n-b\n*** End Patch").unwrap();
        assert_eq!(
            update_chunks(&patch)[0].lines,
            vec![
                (LineKind::Context, "a".into()),
                (LineKind::Context, String::new()),
                (LineKind::Old, "b".into()),
            ]
        );
    }

    #[test]
    fn end_of_file_marks_the_chunk() {
        let patch = parse(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-last\n+final\n*** End of File\n\n@@\n-x\n*** End Patch",
        )
        .unwrap();
        let chunks = update_chunks(&patch);
        assert!(chunks[0].end_of_file);
        assert!(!chunks[1].end_of_file);
        assert_eq!(
            message(
                "*** Begin Patch\n*** Update File: a.txt\n-a\n*** End of File\n-b\n*** End Patch"
            ),
            "line 5: in the update of 'a.txt', '-b' does not start with ' ', '-' or '+'"
        );
    }

    #[test]
    fn an_add_with_no_lines_is_an_empty_file() {
        let patch = parse("*** Begin Patch\n*** Add File: pkg/__init__.py\n*** End Patch").unwrap();
        assert_eq!(
            patch.sections,
            vec![Section::Add {
                path: "pkg/__init__.py".into(),
                contents: String::new(),
            }]
        );
    }

    #[test]
    fn an_add_line_without_a_plus_is_named() {
        assert_eq!(
            message("*** Begin Patch\n*** Add File: a.txt\n+one\ntwo\n*** End Patch"),
            "line 4: every line of the added file 'a.txt' starts with '+'"
        );
    }

    #[test]
    fn an_update_with_only_a_move_is_a_rename() {
        let patch =
            parse("*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n*** End Patch")
                .unwrap();
        assert_eq!(
            patch.sections,
            vec![Section::Update {
                path: "a.txt".into(),
                move_to: Some("b.txt".into()),
                chunks: Vec::new(),
            }]
        );
    }

    #[test]
    fn an_update_with_nothing_is_an_error() {
        assert_eq!(
            message("*** Begin Patch\n*** Update File: a.txt\n*** Delete File: b\n*** End Patch"),
            "line 2: the update of 'a.txt' has no changes"
        );
    }

    #[test]
    fn an_empty_chunk_is_an_error() {
        assert_eq!(
            message("*** Begin Patch\n*** Update File: a.txt\n@@ one\n@@ two\n-x\n*** End Patch"),
            "line 3: a chunk of 'a.txt' has no lines"
        );
    }

    #[test]
    fn a_late_move_is_an_error() {
        assert_eq!(
            message(
                "*** Begin Patch\n*** Update File: a.txt\n-x\n*** Move to: b.txt\n*** End Patch"
            ),
            "line 4: '*** Move to:' must directly follow '*** Update File: a.txt'"
        );
    }

    #[test]
    fn a_header_without_a_path_is_an_error() {
        assert_eq!(
            message("*** Begin Patch\n*** Delete File:   \n*** End Patch"),
            "line 2: a section header needs a path"
        );
    }

    #[test]
    fn a_stray_line_names_its_line_number() {
        assert_eq!(
            message("*** Begin Patch\n*** Update File: a.txt\n@@\n a\n?b\n*** End Patch"),
            "line 5: in the update of 'a.txt', '?b' does not start with ' ', '-' or '+'"
        );
        assert_eq!(
            message("\n\n*** Begin Patch\nnot a header\n*** End Patch"),
            "line 4: 'not a header' is not a section header; use '*** Add File: <path>', \
             '*** Delete File: <path>' or '*** Update File: <path>'"
        );
    }

    #[test]
    fn a_long_stray_line_is_cut() {
        let long = "x".repeat(300);
        let text = message(&format!("*** Begin Patch\n{long}\n*** End Patch"));
        assert!(text.contains(&format!("'{}'", "x".repeat(120))));
        assert!(!text.contains(&"x".repeat(121)));
    }

    #[test]
    fn an_indented_header_inside_an_update_is_context() {
        let patch =
            parse("*** Begin Patch\n*** Update File: a.md\n *** Delete File: b\n-x\n*** End Patch")
                .unwrap();
        assert_eq!(patch.sections.len(), 1);
        assert_eq!(
            update_chunks(&patch)[0].old_lines(),
            ["*** Delete File: b", "x"]
        );
    }

    #[test]
    fn a_crlf_patch_parses_like_lf() {
        let crlf = EXAMPLE.replace('\n', "\r\n");
        assert_eq!(parse(&crlf).unwrap(), parse(EXAMPLE).unwrap());
    }

    #[test]
    fn paths_lists_sources_and_move_destinations() {
        assert_eq!(
            paths(EXAMPLE),
            ["hello.txt", "src/app.py", "src/main.py", "obsolete.txt"]
        );
        assert!(paths("not a patch").is_empty());
    }

    #[test]
    fn errors_name_the_input_field() {
        let error = parse("nope").unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidField);
    }

    fn run(files: &[(&str, &str)], patch: &str, quote: bool) -> Result<Vec<Change>> {
        let files: std::collections::HashMap<String, String> = files
            .iter()
            .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
            .collect();
        let read = move |path: &str| -> Result<Option<String>> { Ok(files.get(path).cloned()) };
        compute(&parse(patch).unwrap(), &read, quote)
    }

    fn updated(files: &[(&str, &str)], patch: &str) -> String {
        match run(files, patch, false).unwrap().pop() {
            Some(Change::Update { contents, .. }) => contents,
            other => panic!("expected an update, got {other:?}"),
        }
    }

    fn update_of(path: &str, body: &str) -> String {
        format!("*** Begin Patch\n*** Update File: {path}\n{body}\n*** End Patch")
    }

    #[test]
    fn each_seek_pass_finds_what_the_previous_missed() {
        let lines = ["a ", "  b", "\u{201C}c\u{201D}\u{00A0}", "d"];
        assert_eq!(seek(&lines, &["d"], 0, false), Some(3));
        assert_eq!(seek(&lines, &["a"], 0, false), Some(0));
        assert_eq!(seek(&lines, &["b"], 0, false), Some(1));
        assert_eq!(seek(&lines, &["\"c\""], 0, false), Some(2));
        assert_eq!(seek(&lines, &["e"], 0, false), None);
    }

    #[test]
    fn an_exact_match_later_wins_over_a_loose_match_earlier() {
        let lines = ["x ", "x"];
        assert_eq!(seek(&lines, &["x"], 0, false), Some(1));
    }

    #[test]
    fn seek_handles_empty_and_oversized_patterns() {
        let lines = ["a", "b"];
        assert_eq!(seek(&lines, &[], 1, false), Some(1));
        assert_eq!(seek(&lines, &[], 0, true), Some(2));
        assert_eq!(seek(&lines, &["a", "b", "c"], 0, false), None);
        assert_eq!(seek(&lines, &["a"], 5, false), None);
        assert_eq!(seek(&[], &["a"], 0, true), None);
    }

    #[test]
    fn end_of_file_binds_to_the_last_lines() {
        let source = "end\nmiddle\nend\n";
        let patch = update_of("a.txt", "@@\n-end\n+last\n*** End of File");
        assert_eq!(updated(&[("a.txt", source)], &patch), "end\nmiddle\nlast\n");
    }

    #[test]
    fn a_context_line_moves_past_an_earlier_identical_block() {
        let source = "fn one() {\n    x();\n}\nfn two() {\n    x();\n}\n";
        let patch = update_of("a.rs", "@@ fn two() {\n-    x();\n+    y();");
        assert_eq!(
            updated(&[("a.rs", source)], &patch),
            "fn one() {\n    x();\n}\nfn two() {\n    y();\n}\n"
        );
    }

    #[test]
    fn a_missing_context_line_is_named() {
        let error = run(
            &[("a.txt", "a\n")],
            &update_of("a.txt", "@@ zzz\n-a"),
            false,
        )
        .unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(
            error.message(),
            "section 1, chunk 1: the line `zzz` was not found in `a.txt`"
        );
    }

    #[test]
    fn an_insertion_with_a_context_line_goes_after_it() {
        let patch = update_of("a.txt", "@@ one\n+inserted");
        assert_eq!(
            updated(&[("a.txt", "one\ntwo\n")], &patch),
            "one\ninserted\ntwo\n"
        );
    }

    #[test]
    fn an_insertion_without_a_context_line_goes_at_the_end() {
        let patch = update_of("a.txt", "@@\n+appended");
        assert_eq!(
            updated(&[("a.txt", "one\ntwo\n")], &patch),
            "one\ntwo\nappended\n"
        );
        assert_eq!(updated(&[("a.txt", "")], &patch), "appended\n");
    }

    #[test]
    fn a_trailing_empty_old_line_is_retried_without_it() {
        let patch = update_of("a.txt", "@@\n-two\n+2\n");
        assert_eq!(updated(&[("a.txt", "one\ntwo")], &patch), "one\n2");
        let patch = update_of("a.txt", " one\n-two\n-\n+2");
        assert_eq!(updated(&[("a.txt", "one\ntwo")], &patch), "one\n2");
    }

    #[test]
    fn a_crlf_file_keeps_crlf_on_context_and_new_lines() {
        let patch = update_of("a.txt", " one\n-two\n+2\n+3\n three");
        assert_eq!(
            updated(&[("a.txt", "one\r\ntwo\r\nthree\r\n")], &patch),
            "one\r\n2\r\n3\r\nthree\r\n"
        );
    }

    #[test]
    fn a_mixed_ending_file_keeps_each_untouched_ending() {
        let source = "a\r\nb\nc\r\nd\n";
        let patch = update_of("a.txt", "-c\n+C");
        assert_eq!(updated(&[("a.txt", source)], &patch), "a\r\nb\nC\r\nd\n");
        // New lines take the ending of the first line the chunk matched.
        let patch = update_of("a.txt", " b\n-c\n+C");
        assert_eq!(updated(&[("a.txt", source)], &patch), "a\r\nb\nC\nd\n");
    }

    #[test]
    fn a_file_without_a_final_newline_stays_without_one() {
        let patch = update_of("a.txt", " one\n-two\n+2");
        assert_eq!(updated(&[("a.txt", "one\ntwo")], &patch), "one\n2");
        let patch = update_of("a.txt", "@@\n+three");
        assert_eq!(updated(&[("a.txt", "one\ntwo")], &patch), "one\ntwo\nthree");
    }

    #[test]
    fn a_byte_order_mark_survives_a_change_to_the_first_line() {
        let patch = update_of("a.txt", "-one\n+1");
        assert_eq!(
            updated(&[("a.txt", "\u{FEFF}one\ntwo\n")], &patch),
            "\u{FEFF}1\ntwo\n"
        );
    }

    #[test]
    fn a_loose_match_keeps_the_files_context_bytes() {
        let patch = update_of("a.py", " def f():  \n-    return 1\n+    return 2");
        assert_eq!(
            updated(&[("a.py", "def f():\t\n    return 1\n")], &patch),
            "def f():\t\n    return 2\n"
        );
    }

    #[test]
    fn two_sections_on_one_file_apply_in_order() {
        let patch = "*** Begin Patch\n*** Update File: a.txt\n-one\n+1\n*** Update File: a.txt\n-1\n+uno\n*** End Patch";
        let changes = run(&[("a.txt", "one\n")], patch, false).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(matches!(&changes[1], Change::Update { contents, .. } if contents == "uno\n"));
    }

    #[test]
    fn a_move_with_changes_and_a_pure_rename() {
        let patch =
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n-one\n+1\n*** End Patch";
        assert_eq!(
            run(&[("a.txt", "one\ntwo\n")], patch, false).unwrap(),
            [Change::Update {
                path: "a.txt".into(),
                move_to: Some("b.txt".into()),
                contents: "1\ntwo\n".into(),
                added: 1,
                removed: 1,
            }]
        );
        let rename = "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n*** End Patch";
        assert_eq!(
            run(&[("a.txt", "one\n")], rename, false).unwrap(),
            [Change::Update {
                path: "a.txt".into(),
                move_to: Some("b.txt".into()),
                contents: "one\n".into(),
                added: 0,
                removed: 0,
            }]
        );
        let error = run(&[("a.txt", "one\n"), ("b.txt", "")], rename, false).unwrap_err();
        assert_eq!(error.code(), ErrorCode::AlreadyExists);
        assert_eq!(
            error.message(),
            "section 1: `b.txt` already exists, so `a.txt` cannot be moved there"
        );
    }

    #[test]
    fn a_delete_and_an_update_after_it() {
        let patch = "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch";
        assert_eq!(
            run(&[("a.txt", "x\n")], patch, false).unwrap(),
            [Change::Delete {
                path: "a.txt".into()
            }]
        );
        let twice =
            "*** Begin Patch\n*** Delete File: a.txt\n*** Update File: a.txt\n-x\n*** End Patch";
        assert_eq!(
            run(&[("a.txt", "x\n")], twice, false)
                .unwrap_err()
                .message(),
            "section 2: `a.txt` does not exist; use '*** Add File' to create it"
        );
    }

    #[test]
    fn an_add_over_an_existing_file_is_refused() {
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch";
        let error = run(&[("a.txt", "old\n")], patch, false).unwrap_err();
        assert_eq!(error.code(), ErrorCode::AlreadyExists);
        assert_eq!(
            error.message(),
            "section 1: `a.txt` already exists; use '*** Update File'"
        );
    }

    #[test]
    fn an_update_of_a_missing_file_is_refused() {
        let error = run(&[], &update_of("a.txt", "-x"), false).unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert_eq!(
            error.message(),
            "section 1: `a.txt` does not exist; use '*** Add File' to create it"
        );
    }

    #[test]
    fn a_failing_second_section_returns_only_the_error() {
        let patch = "*** Begin Patch\n*** Add File: new.txt\n+n\n*** Update File: a.txt\n-missing\n+x\n*** End Patch";
        let error = run(&[("a.txt", "one\n")], patch, false).unwrap_err();
        assert_eq!(
            error.message(),
            "section 2, chunk 1: the expected lines were not found in `a.txt`"
        );
        assert_eq!(
            error.hint(),
            Some("read the file again; nothing was written")
        );
    }

    #[test]
    fn a_missed_chunk_quotes_the_closest_lines_when_allowed() {
        let source = "fn main() {\n    let x = compute(1);\n    print(x);\n}\n";
        let patch = update_of(
            "a.rs",
            "-    let x = compute(2);\n-    print(x);\n+    done();",
        );
        let error = run(&[("a.rs", source)], &patch, true).unwrap_err();
        assert!(
            error.message().starts_with(
                "section 1, chunk 1: the expected lines were not found in `a.rs`. The closest text is lines 2-3:\n"
            ),
            "{}",
            error.message()
        );
        assert!(error.message().contains("compute(1)"));
        assert_eq!(
            error.hint(),
            Some("copy those lines exactly into the patch; nothing was written")
        );
        let quiet = run(&[("a.rs", source)], &patch, false).unwrap_err();
        assert!(!quiet.message().contains("compute(1)"));
    }

    #[test]
    fn counts_follow_the_tagged_lines() {
        let patch = update_of("a.txt", " a\n-b\n-c\n+B");
        let changes = run(&[("a.txt", "a\nb\nc\n")], &patch, false).unwrap();
        assert!(matches!(
            &changes[0],
            Change::Update {
                added: 1,
                removed: 2,
                ..
            }
        ));
    }
}
