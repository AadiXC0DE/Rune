//! A bounded Rust repository map. References are lexical identifier matches,
//! not compiler name resolution. Rebuilding from current bytes avoids stale
//! references after edits, deletions, or ignore-rule changes.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read as _;

use camino::Utf8Path;
use ignore::WalkBuilder;

/// Independent work bounds, including files that contribute no symbols.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum rendered bytes, including the header and truncation notice.
    pub bytes: usize,
    /// Maximum visited filesystem entries.
    pub entries: usize,
    /// Maximum Rust source files read.
    pub files: usize,
    /// Maximum bytes read from one source file. Larger files are skipped.
    pub file_bytes: usize,
    /// Maximum source bytes read in total.
    pub source_bytes: usize,
    /// Maximum declarations retained.
    pub symbols: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes: 4096,
            entries: 4096,
            files: 128,
            file_bytes: 128 * 1024,
            source_bytes: 1024 * 1024,
            symbols: 1024,
        }
    }
}

/// A map generated from one bounded scan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryMap {
    /// Declarations and reference locations, relative to the workspace.
    pub text: String,
    /// Whether a scan, read, symbol, reference, or output bound lost information.
    pub truncated: bool,
}

const HEADER: &str = "Repository symbol map (Rust; lexical references, not resolved types):\n";
const NOTICE: &str = "[repository map incomplete: scan, read, symbol, reference or byte limit]\n";
const REFERENCES: usize = 8;

struct Source {
    path: String,
    text: String,
}

struct Definition<'a> {
    kind: &'a str,
    file: usize,
    line: usize,
    offset: usize,
}

#[derive(Default)]
struct Symbol<'a> {
    definitions: Vec<Definition<'a>>,
    references: Vec<(usize, usize)>,
}

/// Generates a deterministic map for a workspace containing `Cargo.toml`.
///
/// Honors `.gitignore`, `.git/info/exclude`, ancestor ignore rules and `.ignore`,
/// even without a Git checkout. Global Git excludes are disabled for reproducible
/// context. Symlinks and `.git` are never traversed. Unreadable, oversized, or
/// non-UTF-8 sources are skipped and the map is marked incomplete.
#[must_use]
pub fn generate(workspace: &Utf8Path, limits: Limits) -> RepositoryMap {
    if limits.bytes == 0 || !workspace.join("Cargo.toml").is_file() {
        return RepositoryMap::default();
    }
    let mut truncated = false;
    let sources = read_sources(workspace, limits, &mut truncated);
    let mut symbols: BTreeMap<&str, Symbol<'_>> = BTreeMap::new();
    let mut count = 0_usize;
    for (file, source) in sources.iter().enumerate() {
        let mut kind = None;
        let mut depth = 0_usize;
        for token in Lexer::new(&source.text) {
            // Only declarations at module scope orient a reader. A method, an
            // associated constant, or a type nested in a function body is not a
            // landmark, and the map is a per-turn cost, so nesting is skipped.
            match token.text {
                "{" => {
                    depth = depth.saturating_add(1);
                    continue;
                }
                "}" => {
                    depth = depth.saturating_sub(1);
                    continue;
                }
                _ => {}
            }
            if depth == 0
                && !token.raw
                && matches!(
                    token.text,
                    "fn" | "struct" | "enum" | "trait" | "type" | "mod" | "union"
                )
            {
                kind = Some(token.text);
            } else if token.text != "mut"
                && let Some(kind) = kind.take().filter(|_| token.identifier)
            {
                if count >= limits.symbols {
                    truncated = true;
                    break;
                }
                count = count.saturating_add(1);
                symbols
                    .entry(token.text)
                    .or_default()
                    .definitions
                    .push(Definition {
                        kind,
                        file,
                        line: token.line,
                        offset: token.offset,
                    });
            }
        }
        if count >= limits.symbols {
            truncated = true;
            break;
        }
    }
    for (file, source) in sources.iter().enumerate() {
        for token in Lexer::new(&source.text).filter(|token| token.identifier) {
            let Some(symbol) = symbols.get_mut(token.text) else {
                continue;
            };
            if symbol
                .definitions
                .iter()
                .any(|definition| definition.file == file && definition.offset == token.offset)
            {
                continue;
            }
            let location = (file, token.line);
            if symbol.references.last() == Some(&location) {
                continue;
            }
            if symbol.references.len() < REFERENCES {
                symbol.references.push(location);
            } else {
                truncated = true;
            }
        }
    }
    render(&sources, &symbols, limits.bytes, truncated)
}

fn read_sources(workspace: &Utf8Path, limits: Limits, truncated: &mut bool) -> Vec<Source> {
    let mut builder = WalkBuilder::new(workspace);
    builder
        .hidden(false)
        .parents(true)
        .git_global(false)
        .require_git(false)
        .follow_links(false)
        .max_depth(Some(32))
        .sort_by_file_path(std::path::Path::cmp)
        .filter_entry(|entry| {
            let name = entry.file_name();
            // Test-only trees and files orient nobody. A coding agent does not
            // need fixture and mock names to find the production declarations,
            // and the map is a per-turn cost, so the noise is dropped at the
            // walk rather than paid for on every request.
            !matches!(name.to_str(), Some(".git" | "tests" | "benches"))
                && !name.to_string_lossy().ends_with("_tests.rs")
        });
    let mut sources = Vec::new();
    let mut remaining = limits.source_bytes;
    let mut files = 0_usize;
    for (visited, entry) in builder.build().enumerate() {
        if visited >= limits.entries {
            *truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            *truncated = true;
            continue;
        };
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            if entry.depth() == 32 {
                *truncated = true;
            }
            continue;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file())
            || entry.path().extension().is_none_or(|ext| ext != "rs")
        {
            continue;
        }
        if files >= limits.files || remaining == 0 {
            *truncated = true;
            break;
        }
        files = files.saturating_add(1);
        let Ok(path) = entry.path().strip_prefix(workspace) else {
            *truncated = true;
            continue;
        };
        let Some(path) = path.to_str() else {
            *truncated = true;
            continue;
        };
        let Ok(file) = std::fs::File::open(entry.path()) else {
            *truncated = true;
            continue;
        };
        // The extra byte detects growth and oversized files without an unbounded read.
        let cap = limits.file_bytes.saturating_add(1).min(remaining);
        let mut bytes = Vec::new();
        let result = file.take(cap as u64).read_to_end(&mut bytes);
        remaining = remaining.saturating_sub(bytes.len());
        if result.is_err() || bytes.len() > limits.file_bytes || bytes.len() == cap {
            *truncated = true;
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            *truncated = true;
            continue;
        };
        // Test items inside a source file are removed before the scan in the
        // same way a test tree is skipped at the walk, so a `#[cfg(test)]` mock
        // cannot crowd a production declaration out of the byte budget.
        let mut bytes = text.into_bytes();
        blank_test_items(&mut bytes);
        let Ok(text) = String::from_utf8(bytes) else {
            *truncated = true;
            continue;
        };
        sources.push(Source {
            path: path.replace('\\', "/"),
            text,
        });
    }
    sources
}

fn render(
    sources: &[Source],
    symbols: &BTreeMap<&str, Symbol<'_>>,
    cap: usize,
    mut truncated: bool,
) -> RepositoryMap {
    if symbols.is_empty() && !truncated {
        return RepositoryMap::default();
    }
    // Reserve the notice even when a scan initially appears complete. Only whole
    // records are emitted, so no path, identifier, or UTF-8 character is cut.
    let budget = cap.saturating_sub(NOTICE.len());
    let mut text = String::new();
    if HEADER.len() > budget {
        return RepositoryMap {
            text: if NOTICE.len() <= cap {
                NOTICE.to_owned()
            } else {
                String::new()
            },
            truncated: true,
        };
    }
    text.push_str(HEADER);
    'symbols: for (name, symbol) in symbols {
        for definition in &symbol.definitions {
            let mut row = format!(
                "{}:{} {} {}",
                sources[definition.file].path, definition.line, definition.kind, name
            );
            if !symbol.references.is_empty() {
                row.push_str(" <-");
                for (file, line) in &symbol.references {
                    let _ = write!(row, " {}:{line}", sources[*file].path);
                }
            }
            row.push('\n');
            if text.len().saturating_add(row.len()) > budget {
                truncated = true;
                break 'symbols;
            }
            text.push_str(&row);
        }
    }
    if truncated && text.len().saturating_add(NOTICE.len()) <= cap {
        text.push_str(NOTICE);
    }
    RepositoryMap { text, truncated }
}

struct Token<'a> {
    text: &'a str,
    line: usize,
    offset: usize,
    identifier: bool,
    raw: bool,
}

/// Small lexer for identifiers and declaration keywords. Literal and comment
/// contents never become references. It does not expand macros or resolve scopes.
struct Lexer<'a> {
    text: &'a str,
    offset: usize,
    line: usize,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            offset: 0,
            line: 1,
        }
    }

    fn advance(&mut self, bytes: usize) {
        let end = self.offset.saturating_add(bytes).min(self.text.len());
        self.line = self.line.saturating_add(
            self.text[self.offset..end]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count(),
        );
        self.offset = end;
    }

    fn skip_literal(&mut self) -> bool {
        let rest = &self.text[self.offset..];
        let raw = rest
            .strip_prefix("br")
            .or_else(|| rest.strip_prefix("cr"))
            .or_else(|| rest.strip_prefix('r'));
        if let Some(raw) = raw {
            let hashes = raw.bytes().take_while(|byte| *byte == b'#').count();
            if raw.as_bytes().get(hashes) == Some(&b'"') {
                let prefix = rest
                    .len()
                    .saturating_sub(raw.len())
                    .saturating_add(hashes)
                    .saturating_add(1);
                let closing = format!("\"{}", "#".repeat(hashes));
                let length = rest[prefix..].find(&closing).map_or(rest.len(), |at| {
                    prefix.saturating_add(at).saturating_add(closing.len())
                });
                self.advance(length);
                return true;
            }
        }
        let quoted = rest
            .strip_prefix('b')
            .or_else(|| rest.strip_prefix('c'))
            .unwrap_or(rest);
        let Some(quote) = quoted.chars().next().filter(|ch| matches!(ch, '"' | '\'')) else {
            return false;
        };
        let prefix = rest.len().saturating_sub(quoted.len());
        if quote == '\'' {
            // A single character or escape followed by a quote is a character
            // literal. Other apostrophes introduce lifetimes, which are skipped.
            let inner = &quoted[1..];
            let end = if let Some(escaped) = inner.strip_prefix('\\') {
                let end = if escaped.starts_with("u{") {
                    escaped.find('}').map(|at| at.saturating_add(2))
                } else if escaped.starts_with('x') {
                    Some(4)
                } else {
                    escaped
                        .chars()
                        .next()
                        .map(|ch| ch.len_utf8().saturating_add(1))
                };
                end.filter(|at| inner.as_bytes().get(*at) == Some(&b'\''))
                    .map(|at| at.saturating_add(1))
            } else {
                inner
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .filter(|at| inner.as_bytes().get(*at) == Some(&b'\''))
                    .map(|at| at.saturating_add(1))
            };
            if let Some(end) = end {
                self.advance(prefix.saturating_add(1).saturating_add(end));
            } else {
                let length = inner
                    .chars()
                    .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                    .map(char::len_utf8)
                    .sum::<usize>();
                self.advance(prefix.saturating_add(1).saturating_add(length));
            }
            return true;
        }
        let mut escaped = false;
        let mut length = rest.len();
        for (at, ch) in quoted[1..].char_indices() {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                length = prefix.saturating_add(at).saturating_add(2);
                break;
            }
        }
        self.advance(length);
        true
    }
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let rest = self.text.get(self.offset..)?;
            let ch = rest.chars().next()?;
            if ch.is_whitespace() {
                self.advance(ch.len_utf8());
                continue;
            }
            if rest.starts_with("//") {
                self.advance(rest.find('\n').unwrap_or(rest.len()));
                continue;
            }
            if rest.starts_with("/*") {
                self.advance(2);
                let mut depth = 1_usize;
                while depth > 0 && self.offset < self.text.len() {
                    let rest = &self.text[self.offset..];
                    if rest.starts_with("/*") {
                        depth = depth.saturating_add(1);
                        self.advance(2);
                    } else if rest.starts_with("*/") {
                        depth = depth.saturating_sub(1);
                        self.advance(2);
                    } else if let Some(ch) = rest.chars().next() {
                        self.advance(ch.len_utf8());
                    }
                }
                continue;
            }
            if self.skip_literal() {
                continue;
            }
            // Raw identifiers use the same name as their ordinary references.
            let raw_prefix = if rest
                .strip_prefix("r#")
                .and_then(|rest| rest.chars().next())
                .is_some_and(|ch| ch.is_alphabetic() || ch == '_')
            {
                2
            } else {
                0
            };
            let identifier = rest[raw_prefix..]
                .chars()
                .next()
                .is_some_and(|ch| ch.is_alphabetic() || ch == '_');
            let length = if identifier {
                raw_prefix.saturating_add(
                    rest[raw_prefix..]
                        .chars()
                        .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                        .map(char::len_utf8)
                        .sum::<usize>(),
                )
            } else {
                ch.len_utf8()
            };
            let offset = self.offset;
            let line = self.line;
            self.advance(length);
            return Some(Token {
                text: &self.text[offset.saturating_add(raw_prefix)..self.offset],
                line,
                offset,
                identifier,
                raw: raw_prefix > 0,
            });
        }
    }
}

/// Blanks test-only items in a source before the scan.
///
/// The bytes are replaced with spaces rather than removed so that every line
/// number a reference reports still points at the original line. Newlines are
/// kept for the same reason. The text has already been validated as UTF-8, and
/// only ASCII bytes are written, so the result stays valid.
fn blank_test_items(bytes: &mut [u8]) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    let ranges = test_item_ranges(text);
    for range in ranges {
        for byte in &mut bytes[range] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
}

/// Returns the byte ranges of `#[cfg(test)]` items in a source.
fn test_item_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let tokens: Vec<Token<'_>> = Lexer::new(text).collect();
    let mut ranges = Vec::new();
    let mut index = 0_usize;
    while index < tokens.len() {
        if let Some(end) = test_item_end(&tokens, index) {
            ranges.push(tokens[index].offset..end);
        }
        index = index.saturating_add(1);
    }
    ranges
}

/// Returns the offset past the item a test attribute at `index` guards.
///
/// Handles `#[cfg(test)]` and the `#[cfg(all(test, ...))]` / `#[cfg(any(test,
/// ...))]` forms. `#[cfg(not(test))]` is production code and is left alone.
fn test_item_end(tokens: &[Token<'_>], index: usize) -> Option<usize> {
    if tokens.get(index)?.text != "#" {
        return None;
    }
    let bracket = index.saturating_add(1);
    let cfg = index.saturating_add(2);
    let paren = index.saturating_add(3);
    if tokens.get(bracket)?.text != "[" || tokens.get(cfg)?.text != "cfg" {
        return None;
    }
    if tokens.get(paren)?.text != "(" {
        return None;
    }
    let open = paren.saturating_add(1);
    let close = grouped_end(tokens, paren)?;
    let is_test = match tokens.get(open)?.text {
        "test" => true,
        "all" | "any" => tokens
            .get(open..close)?
            .iter()
            .any(|token| token.text == "test"),
        _ => false,
    };
    if !is_test || tokens.get(close.saturating_add(1))?.text != "]" {
        return None;
    }
    item_end(tokens.get(close.saturating_add(2)..)?)
}

/// Returns the index of the `)` matching the `(` at `open`.
fn grouped_end(tokens: &[Token<'_>], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (offset, token) in tokens[open..].iter().enumerate() {
        match token.text {
            "(" => depth = depth.saturating_add(1),
            ")" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open.saturating_add(offset));
                }
            }
            _ => {}
        }
    }
    None
}

/// Returns the offset past the item that follows an attribute.
///
/// A block ends at its matching `}`, and a declaration without a block ends at
/// its `;` or, for a struct field or enum variant, its `,`. Nesting is tracked
/// so a delimiter inside the item does not end it early.
fn item_end(tokens: &[Token<'_>]) -> Option<usize> {
    let mut opened = false;
    let mut braces = 0_usize;
    let mut parens = 0_usize;
    let mut brackets = 0_usize;
    for token in tokens {
        match token.text {
            "{" => {
                opened = true;
                braces = braces.saturating_add(1);
            }
            "}" if opened => {
                braces = braces.saturating_sub(1);
                if braces == 0 {
                    return Some(token.offset.saturating_add(token.text.len()));
                }
            }
            "(" => parens = parens.saturating_add(1),
            ")" => parens = parens.saturating_sub(1),
            "[" => brackets = brackets.saturating_add(1),
            "]" => brackets = brackets.saturating_sub(1),
            ";" | "," if !opened && parens == 0 && brackets == 0 => {
                return Some(token.offset.saturating_add(token.text.len()));
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn fixture() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("fixture");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("UTF-8 root");
        std::fs::create_dir(root.join("src")).expect("source directory");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.1.0'\nedition = '2024'\n",
        )
        .expect("manifest");
        (dir, root)
    }

    fn write(root: &Utf8Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, text).expect("fixture file");
    }

    #[test]
    fn capped_map_refreshes_references_edits_deletions_and_ignore_rules() {
        let (_dir, root) = fixture();
        write(&root, ".gitignore", "ignored.rs\ntarget/\n");
        write(&root, ".ignore", "vendor/\n");
        write(&root, "src/nested/.gitignore", "hidden.rs\n");
        write(&root, ".git/info/exclude", "excluded.rs\n");
        for path in [
            "ignored.rs",
            "target/build.rs",
            "vendor/lib.rs",
            "src/nested/hidden.rs",
            "excluded.rs",
            ".git/private.rs",
        ] {
            write(&root, path, "fn ignored_secret() { alpha(); }\n");
        }
        write(
            &root,
            "src/lib.rs",
            &format!(
                "pub fn alpha() {{}}\npub fn beta() {{}}\n{}",
                (0..100).fold(String::new(), |mut text, index| {
                    writeln!(text, "fn z{index:03}() {{}}").expect("format source");
                    text
                })
            ),
        );
        write(&root, "src/main.rs", "fn main() { alpha(); }\n");
        let limits = Limits {
            bytes: 512,
            ..Limits::default()
        };
        let before = generate(&root, limits);
        assert!(before.truncated);
        assert!(before.text.len() <= limits.bytes);
        assert!(
            before
                .text
                .contains("src/lib.rs:1 fn alpha <- src/main.rs:1"),
            "{}",
            before.text
        );
        assert!(before.text.contains("map incomplete"));
        assert!(!before.text.contains("ignored_secret"));
        assert!(!before.text.contains("excluded.rs"));
        assert_eq!(before, generate(&root, limits));

        // Same-length content changes must refresh without relying on mtime or size.
        write(&root, "src/main.rs", "fn main() { beta();  }\n");
        let after = generate(&root, limits);
        assert!(
            after.text.contains("src/lib.rs:1 fn alpha\n"),
            "{}",
            after.text
        );
        assert!(
            after.text.contains("src/lib.rs:2 fn beta <- src/main.rs:1"),
            "{}",
            after.text
        );
        std::fs::remove_file(root.join("src/main.rs")).expect("delete source");
        assert!(!generate(&root, limits).text.contains("<-"));
        write(&root, ".gitignore", "src/lib.rs\nignored.rs\ntarget/\n");
        assert!(generate(&root, limits).text.is_empty());
    }

    #[test]
    fn lexer_skips_literals_comments_and_lifetimes_and_handles_raw_names() {
        let (_dir, root) = fixture();
        write(
            &root,
            "src/lib.rs",
            r####"pub fn actual() {}
pub struct Thing;
pub fn r#type() {}
// fn comment() { actual(); }
/* fn block() {} /* actual(); */ */
const TEXT: &str = "fn fake() { actual(); }";
const RAW: &str = r###"fn raw_fake() { actual(); }"###;
const BYTE: &[u8] = br#"actual()"#;
fn caller<'actual>() {
    let _ = '\'';
    let _ = '\u{1234}';
    let _ = "escaped \" actual()";
    actual();
    let _ = Thing;
    r#type();
}
fn x41() {}
fn characters() { let _ = b'\x41'; let _ = '\x41'; }
fn c() {}
fn cr() {}
fn c_literals() { let _ = c"actual()"; let _ = cr#"actual()"#; }
"####,
        );
        let map = generate(&root, Limits::default());
        assert!(!map.truncated, "{}", map.text);
        assert!(
            map.text
                .contains("src/lib.rs:1 fn actual <- src/lib.rs:13\n"),
            "{}",
            map.text
        );
        assert!(
            map.text
                .contains("src/lib.rs:2 struct Thing <- src/lib.rs:14\n"),
            "{}",
            map.text
        );
        assert!(
            map.text.contains("src/lib.rs:3 fn type <- src/lib.rs:15\n"),
            "{}",
            map.text
        );
        for declaration in [
            "src/lib.rs:17 fn x41\n",
            "src/lib.rs:19 fn c\n",
            "src/lib.rs:20 fn cr\n",
        ] {
            assert!(map.text.contains(declaration), "{}", map.text);
        }
        for name in ["comment", "block", "fake", "raw_fake"] {
            assert!(!map.text.contains(name), "{}", map.text);
        }
    }

    #[test]
    fn test_trees_files_and_cfg_test_items_stay_out_of_the_map() {
        // A coding agent does not orient itself with fixture or mock names, and
        // the map is a per-turn cost. Integration trees, `*_tests.rs` files, and
        // `#[cfg(test)]` items inside a source file are all test-only.
        let (_dir, root) = fixture();
        write(&root, "src/mock_support_tests.rs", "pub struct MockOnly;\n");
        write(
            &root,
            "tests/integration.rs",
            "pub const FIXTURE: u8 = 1;\n",
        );
        write(
            &root,
            "src/lib.rs",
            "pub fn production() {}\n\n#[cfg(test)]\nmod tests {\n    pub struct MockThing;\n    pub fn mock_helper() { production(); }\n}\npub fn after() {}\n",
        );
        let map = generate(&root, Limits::default());
        // The blanked test module keeps later line numbers where they were.
        assert!(
            map.text.contains("src/lib.rs:1 fn production\n"),
            "{}",
            map.text
        );
        assert!(map.text.contains("src/lib.rs:8 fn after\n"), "{}", map.text);
        for absent in ["MockOnly", "FIXTURE", "MockThing", "mock_helper"] {
            assert!(!map.text.contains(absent), "{absent} leaked: {}", map.text);
        }
    }

    #[test]
    fn only_top_level_declarations_are_landmarks() {
        // Methods, associated constants, and items nested in a module are not
        // landmarks for orientation. Types, functions, traits, and modules at
        // module scope are.
        let (_dir, root) = fixture();
        write(
            &root,
            "src/lib.rs",
            "pub struct Outer;\nimpl Outer {\n    pub fn method(&self) {}\n    pub const VALUE: u32 = 1;\n}\npub fn free() {}\nmod nested {\n    pub fn inner() {}\n}\n",
        );
        let map = generate(&root, Limits::default());
        for kept in [
            "src/lib.rs:1 struct Outer",
            "src/lib.rs:6 fn free",
            "src/lib.rs:7 mod nested",
        ] {
            assert!(map.text.contains(kept), "{kept} missing: {}", map.text);
        }
        for absent in ["method", "VALUE", "inner"] {
            assert!(!map.text.contains(absent), "{absent} leaked: {}", map.text);
        }
    }

    #[test]
    fn the_byte_cap_is_honoured_and_truncation_is_visible() {
        let (_dir, root) = fixture();
        let body: String = (0..200).fold(String::new(), |mut text, index| {
            writeln!(text, "pub fn symbol_{index:03}() {{}}").expect("format source");
            text
        });
        write(&root, "src/lib.rs", &body);
        let limits = Limits {
            bytes: 512,
            ..Limits::default()
        };
        let map = generate(&root, limits);
        assert!(map.text.len() <= limits.bytes, "{}", map.text.len());
        assert!(map.truncated, "{}", map.text);
        assert!(map.text.contains("map incomplete"), "{}", map.text);
    }

    #[test]
    fn each_work_bound_marks_the_map_incomplete_and_tiny_caps_stay_exact() {
        let (_dir, root) = fixture();
        write(&root, "src/a.rs", "pub fn alpha() {}\n");
        write(&root, "src/b.rs", "pub fn beta() { alpha(); }\n");
        for limits in [
            Limits {
                entries: 1,
                ..Limits::default()
            },
            Limits {
                files: 1,
                ..Limits::default()
            },
            Limits {
                file_bytes: 8,
                ..Limits::default()
            },
            Limits {
                source_bytes: 8,
                ..Limits::default()
            },
            Limits {
                symbols: 1,
                ..Limits::default()
            },
        ] {
            let map = generate(&root, limits);
            assert!(map.truncated, "{limits:?}: {map:?}");
            assert!(map.text.contains("map incomplete"));
            assert!(map.text.len() <= limits.bytes);
        }
        for bytes in 0..256 {
            let map = generate(
                &root,
                Limits {
                    bytes,
                    ..Limits::default()
                },
            );
            assert!(map.text.len() <= bytes);
        }
        write(&root, "src/b.rs", &"alpha();\n".repeat(20));
        let map = generate(&root, Limits::default());
        assert!(map.truncated);
        assert!(!map.text.contains("src/b.rs:9"));
    }

    #[test]
    fn non_utf8_sources_and_symlinks_do_not_leak_content() {
        let (_dir, root) = fixture();
        std::fs::write(root.join("src/invalid.rs"), b"fn bad() {}\n\xff").expect("invalid UTF-8");
        let map = generate(&root, Limits::default());
        assert!(map.truncated);
        assert!(!map.text.contains("bad"));
        #[cfg(unix)]
        {
            let (outside_dir, outside) = fixture();
            write(&outside, "secret.rs", "fn outside_secret() {}\n");
            std::os::unix::fs::symlink(outside.join("secret.rs"), root.join("src/link.rs"))
                .expect("file symlink");
            std::os::unix::fs::symlink(outside_dir.path(), root.join("linked"))
                .expect("directory symlink");
            assert!(
                !generate(&root, Limits::default())
                    .text
                    .contains("outside_secret")
            );
        }
    }
}
