//! The file patch format: `*** Begin Patch`, file sections, `*** End Patch`.

use rune_core::error::{Result, RuneError};

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
        assert_eq!(error.code(), rune_core::error::ErrorCode::InvalidField);
    }
}
