//! Colour for code in a fenced block.
//!
//! A small lexer rather than a grammar: comments, strings, numbers, keywords
//! and the names of called functions, which is what makes code readable at a
//! glance. It works one row at a time, so it never holds state across lines,
//! and a construct split by a wrap is simply drawn plainly on the next row.

/// The escapes each kind of token is drawn with.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CodeStyle {
    /// Opens a keyword.
    pub keyword: String,
    /// Opens a string literal.
    pub string: String,
    /// Opens a number.
    pub number: String,
    /// Opens a comment.
    pub comment: String,
    /// Opens the name of a called function.
    pub function: String,
    /// Closes any of the above.
    pub reset: String,
}

impl CodeStyle {
    /// Returns true when no token would be drawn differently.
    #[must_use]
    pub fn is_plain(&self) -> bool {
        self.keyword.is_empty()
            && self.string.is_empty()
            && self.number.is_empty()
            && self.comment.is_empty()
            && self.function.is_empty()
    }
}

/// A language family, chosen from a fence's info string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Language {
    /// Rust.
    Rust,
    /// JavaScript and TypeScript.
    Script,
    /// Python.
    Python,
    /// Go.
    Go,
    /// C, C++, Java, C# and Kotlin.
    CFamily,
    /// Shell scripts.
    Shell,
    /// Anything else: strings, numbers and line comments only.
    Other,
}

impl Language {
    /// Reads the first word of a fence's info string.
    #[must_use]
    pub fn from_info(info: &str) -> Self {
        let name = info
            .trim()
            .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == '{')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match name.as_str() {
            "rust" | "rs" => Self::Rust,
            "js" | "javascript" | "jsx" | "ts" | "typescript" | "tsx" | "mjs" | "cjs" => {
                Self::Script
            }
            "py" | "python" => Self::Python,
            "go" | "golang" => Self::Go,
            "c" | "h" | "cpp" | "c++" | "cc" | "hpp" | "java" | "cs" | "csharp" | "kotlin"
            | "kt" | "swift" => Self::CFamily,
            "sh" | "bash" | "zsh" | "shell" | "console" | "fish" => Self::Shell,
            _ => Self::Other,
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while",
            ],
            Self::Script => &[
                "as",
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "default",
                "delete",
                "do",
                "else",
                "export",
                "extends",
                "false",
                "finally",
                "for",
                "from",
                "function",
                "if",
                "import",
                "in",
                "instanceof",
                "interface",
                "let",
                "new",
                "null",
                "of",
                "return",
                "static",
                "super",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "type",
                "typeof",
                "undefined",
                "var",
                "void",
                "while",
                "yield",
            ],
            Self::Python => &[
                "and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
                "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise",
                "return", "self", "True", "try", "while", "with", "yield",
            ],
            Self::Go => &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "false",
                "for",
                "func",
                "go",
                "goto",
                "if",
                "import",
                "interface",
                "map",
                "nil",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "true",
                "type",
                "var",
            ],
            Self::CFamily => &[
                "auto",
                "bool",
                "break",
                "case",
                "catch",
                "char",
                "class",
                "const",
                "continue",
                "default",
                "delete",
                "do",
                "double",
                "else",
                "enum",
                "extends",
                "false",
                "final",
                "float",
                "for",
                "fun",
                "if",
                "implements",
                "import",
                "int",
                "long",
                "namespace",
                "new",
                "null",
                "nullptr",
                "package",
                "private",
                "protected",
                "public",
                "return",
                "short",
                "static",
                "struct",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "typedef",
                "unsigned",
                "val",
                "var",
                "virtual",
                "void",
                "while",
            ],
            Self::Shell => &[
                "case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function",
                "if", "in", "local", "return", "then", "until", "while",
            ],
            Self::Other => &[],
        }
    }

    /// Returns the marker that opens a comment running to the end of the line.
    fn line_comment(self) -> &'static [&'static str] {
        match self {
            Self::Rust | Self::Script | Self::Go | Self::CFamily => &["//"],
            Self::Python | Self::Shell => &["#"],
            Self::Other => &["//", "#"],
        }
    }
}

/// Colours one row of code.
#[must_use]
pub fn highlight(row: &str, language: Language, style: &CodeStyle) -> String {
    if style.is_plain() {
        return row.to_owned();
    }
    let mut out = String::with_capacity(row.len().saturating_mul(2));
    let mut rest = row;
    let mut previous: Option<char> = None;
    while let Some(ch) = rest.chars().next() {
        if language
            .line_comment()
            .iter()
            .any(|marker| rest.starts_with(marker))
            && previous.is_none_or(|before| !before.is_alphanumeric())
        {
            push_styled(&mut out, &style.comment, rest, &style.reset);
            return out;
        }
        if matches!(ch, '"' | '\'' | '`') && !(language == Language::Rust && ch == '\'') {
            let end = string_end(rest, ch);
            push_styled(&mut out, &style.string, &rest[..end], &style.reset);
            rest = &rest[end..];
            previous = Some(ch);
            continue;
        }
        if ch.is_ascii_digit() && previous.is_none_or(|before| !is_word(before)) {
            let end = rest
                .find(|next: char| !(next.is_ascii_alphanumeric() || next == '_' || next == '.'))
                .unwrap_or(rest.len());
            push_styled(&mut out, &style.number, &rest[..end], &style.reset);
            rest = &rest[end..];
            previous = Some('0');
            continue;
        }
        if is_word_start(ch) {
            let end = rest.find(|next: char| !is_word(next)).unwrap_or(rest.len());
            let word = &rest[..end];
            let called = rest[end..].trim_start().starts_with('(')
                || (language == Language::Rust && rest[end..].starts_with('!'));
            if language.keywords().contains(&word) {
                push_styled(&mut out, &style.keyword, word, &style.reset);
            } else if called {
                push_styled(&mut out, &style.function, word, &style.reset);
            } else {
                out.push_str(word);
            }
            rest = &rest[end..];
            previous = Some('a');
            continue;
        }
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
        previous = Some(ch);
    }
    out
}

/// Returns the byte length of the string literal opened by `quote`.
///
/// An unterminated literal runs to the end of the row.
fn string_end(text: &str, quote: char) -> usize {
    let mut escaped = false;
    for (index, ch) in text.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return index.saturating_add(ch.len_utf8());
        }
    }
    text.len()
}

fn push_styled(out: &mut String, open: &str, text: &str, reset: &str) {
    if open.is_empty() {
        out.push_str(text);
    } else {
        out.push_str(open);
        out.push_str(text);
        out.push_str(reset);
    }
}

fn is_word_start(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_'
}

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked() -> CodeStyle {
        CodeStyle {
            keyword: "<k>".to_owned(),
            string: "<s>".to_owned(),
            number: "<n>".to_owned(),
            comment: "<c>".to_owned(),
            function: "<f>".to_owned(),
            reset: "</>".to_owned(),
        }
    }

    #[test]
    fn rust_tokens_are_classified() {
        assert_eq!(
            highlight(
                "pub fn f(x: u8) -> u8 { g(x + 1) } // note",
                Language::Rust,
                &marked()
            ),
            "<k>pub</> <k>fn</> <f>f</>(x: u8) -> u8 { <f>g</>(x + <n>1</>) } <c>// note</>"
        );
        assert_eq!(
            highlight("println!(\"a \\\" b\");", Language::Rust, &marked()),
            "<f>println</>!(<s>\"a \\\" b\"</>);"
        );
    }

    #[test]
    fn a_lifetime_is_not_a_string_in_rust() {
        assert_eq!(
            highlight("fn f<'a>(x: &'a str)", Language::Rust, &marked()),
            "<k>fn</> f<'a>(x: &'a str)"
        );
    }

    #[test]
    fn script_and_python_use_their_own_keywords_and_comments() {
        assert_eq!(
            highlight("export const n = `t`;", Language::Script, &marked()),
            "<k>export</> <k>const</> n = <s>`t`</>;"
        );
        assert_eq!(
            highlight("def f(): return None  # done", Language::Python, &marked()),
            "<k>def</> <f>f</>(): <k>return</> <k>None</>  <c># done</>"
        );
    }

    #[test]
    fn digits_inside_a_name_are_not_a_number_and_a_hash_inside_a_word_is_not_a_comment() {
        assert_eq!(highlight("x1 = v2", Language::Other, &marked()), "x1 = v2");
        assert_eq!(highlight("a#b", Language::Shell, &marked()), "a#b");
    }

    #[test]
    fn an_unterminated_string_runs_to_the_end_of_the_row() {
        assert_eq!(
            highlight("let s = \"open", Language::Rust, &marked()),
            "<k>let</> s = <s>\"open</>"
        );
    }

    #[test]
    fn the_info_string_picks_the_language() {
        assert_eq!(Language::from_info("rust"), Language::Rust);
        assert_eq!(
            Language::from_info("TypeScript title=a.ts"),
            Language::Script
        );
        assert_eq!(Language::from_info("py,linenos"), Language::Python);
        assert_eq!(Language::from_info(""), Language::Other);
    }

    #[test]
    fn a_plain_style_returns_the_row_unchanged() {
        let row = "fn main() { \"x\" }";
        assert_eq!(highlight(row, Language::Rust, &CodeStyle::default()), row);
    }
}
