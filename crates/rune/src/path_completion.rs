//! On-demand completion of one prompt token within the configured roots.

use std::ops::Range;

use camino::Utf8Path;
use rune_tools::contract::ExecutionContext;
use rune_tools::workspace::{self, FileLimits};

/// Filesystem work and retained choices are bounded independently of menu size.
const MAX_SCANNED: usize = 4096;
const MAX_MATCHES: usize = 256;

#[derive(Debug)]
pub struct Paths {
    pub range: Range<usize>,
    pub matches: Vec<String>,
}

impl Paths {
    /// Reads directory names only after the shared resolver checks containment.
    pub fn collect(
        line: &str,
        caret: usize,
        context: &ExecutionContext,
        limits: &FileLimits,
    ) -> Self {
        Self::collect_with(line, caret, context, limits, |path| std::fs::read_dir(path))
    }

    fn collect_with(
        line: &str,
        caret: usize,
        context: &ExecutionContext,
        limits: &FileLimits,
        mut read_dir: impl FnMut(&Utf8Path) -> std::io::Result<std::fs::ReadDir>,
    ) -> Self {
        let (range, prefix) = token_at(line, caret);
        let mut result = Self {
            range,
            matches: Vec::new(),
        };
        if limits.list_entries == 0 {
            return result;
        }
        // Completion never inherits a tool's permission to access external files.
        let context = context.clone().with_external_access(false);
        let (directory, name) = prefix
            .rsplit_once('/')
            .map_or((".", prefix.as_str()), |(parent, name)| {
                (if parent.is_empty() { "/" } else { parent }, name)
            });
        let Ok(directory) = workspace::resolve(&context, directory) else {
            return result;
        };
        let Ok(entries) = read_dir(&directory.path) else {
            return result;
        };
        let head = prefix
            .get(..prefix.len().saturating_sub(name.len()))
            .unwrap_or_default();
        for entry in entries.take(limits.walk_files.min(MAX_SCANNED)).flatten() {
            let Ok(filename) = entry.file_name().into_string() else {
                continue;
            };
            if !filename.starts_with(name) || filename.chars().any(char::is_control) {
                continue;
            }
            // An offered link must also lead inside a permitted root. No file
            // contents are opened, including when deciding whether to append '/'.
            let candidate = format!("{head}{filename}");
            let Ok(resolved) = workspace::resolve(&context, &candidate) else {
                continue;
            };
            let candidate = if resolved.path.is_dir() {
                format!("{candidate}/")
            } else {
                candidate
            };
            result.matches.push(quote(&candidate));
            if result.matches.len() >= limits.list_entries.min(MAX_MATCHES) {
                break;
            }
        }
        result.matches.sort_unstable();
        result.matches.dedup();
        result
    }
}

/// Recognises quoted and escaped spaces, replacing the whole token at the caret.
fn token_at(line: &str, caret: usize) -> (Range<usize>, String) {
    let caret = caret.min(line.len());
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut prefix = String::new();
    let mut chars = line.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        if escaped {
            if at < caret {
                prefix.push(ch);
            }
            escaped = false;
        } else if ch == '\\'
            && (quote.is_none()
                || (quote == Some('"')
                    && chars
                        .peek()
                        .is_some_and(|(_, next)| matches!(next, '$' | '`' | '"' | '\\' | '\n'))))
        {
            escaped = true;
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if quote.is_none() && ch.is_whitespace() {
            if at >= caret {
                return (start..at, prefix);
            }
            start = at.saturating_add(ch.len_utf8());
            prefix.clear();
        } else if at < caret {
            prefix.push(ch);
        }
    }
    (start..line.len(), prefix)
}

/// Shell quoting keeps spaces, quotes, and metacharacters in one literal path.
fn quote(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("~/{}", quote(rest));
    }
    if path
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        path.to_owned()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn fixture() -> (tempfile::TempDir, ExecutionContext) {
        let temp = tempfile::tempdir().expect("fixture");
        let root = Utf8PathBuf::from_path_buf(temp.path().join("workspace")).expect("UTF-8");
        std::fs::create_dir(&root).expect("workspace");
        (temp, ExecutionContext::new(root))
    }

    #[test]
    fn quoted_and_escaped_tokens_are_completed_as_one_path_at_the_caret() {
        for (line, caret, expected_range, prefix) in [
            ("read ./fi", 9, 5..9, "./fi"),
            ("read 'a space/fi' please", 16, 5..17, "a space/fi"),
            ("read \"a space/fi", 16, 5..16, "a space/fi"),
            ("read a\\ space/fi", 16, 5..16, "a space/fi"),
            ("read \"a\\ space/fi\"", 18, 5..18, "a\\ space/fi"),
            ("read 'it'\\''s/fi'", 17, 5..17, "it's/fi"),
            ("read 界/fi please", 11, 5..11, "界/fi"),
            ("read fiOLD please", 7, 5..10, "fi"),
            ("read ", 5, 5..5, ""),
        ] {
            assert_eq!(
                token_at(line, caret),
                (expected_range, prefix.to_owned()),
                "{line}"
            );
        }
    }

    #[test]
    fn completion_quotes_spaces_quotes_and_shell_metacharacters() {
        assert_eq!(quote("~/a space/fi"), "~/'a space/fi'");
        let (_temp, context) = fixture();
        for name in [
            "fixture space.txt",
            "fixture's.txt",
            "fixture$(echo).txt",
            "fixture.txt",
        ] {
            std::fs::write(context.workspace().join(name), "content is never read").expect("file");
        }
        let line = "read ./fixture";
        let choices = Paths::collect(line, line.len(), &context, &FileLimits::default());
        assert_eq!(choices.range, 5..line.len());
        assert_eq!(
            choices.matches,
            [
                "'./fixture space.txt'",
                "'./fixture$(echo).txt'",
                "'./fixture'\\''s.txt'",
                "./fixture.txt",
            ]
        );
    }

    #[test]
    fn directories_with_spaces_can_be_completed_again() {
        let (_temp, context) = fixture();
        let directory = context.workspace().join("a space");
        std::fs::create_dir(&directory).expect("directory");
        std::fs::write(directory.join("file space.txt"), "fixture").expect("file");
        let line = "read a";
        assert_eq!(
            Paths::collect(line, line.len(), &context, &FileLimits::default()).matches,
            ["'a space/'"]
        );
        let line = "read 'a space/'";
        assert_eq!(
            Paths::collect(line, line.len(), &context, &FileLimits::default()).matches,
            ["'a space/file space.txt'"]
        );
    }

    #[test]
    fn external_paths_never_reach_directory_read_even_with_external_tool_access() {
        let (temp, context) = fixture();
        let context = context.with_external_access(true);
        let outside = Utf8Path::from_path(temp.path())
            .expect("UTF-8")
            .join("outside");
        std::fs::create_dir(&outside).expect("outside");
        std::fs::write(outside.join("secret.txt"), "secret").expect("secret");
        for line in ["read ../outside/".to_owned(), format!("read {outside}/")] {
            let choices =
                Paths::collect_with(&line, line.len(), &context, &FileLimits::default(), |_| {
                    panic!("an outside directory must never be listed");
                });
            assert!(choices.matches.is_empty());
        }
    }

    #[test]
    fn configured_additional_roots_are_permitted() {
        let (temp, context) = fixture();
        let extra = Utf8Path::from_path(temp.path())
            .expect("UTF-8")
            .join("extra root");
        std::fs::create_dir(&extra).expect("extra");
        std::fs::write(extra.join("fixture space.txt"), "fixture").expect("file");
        let context = context.with_root(extra.clone());
        let line = format!("read '{extra}/fi'");
        let choices = Paths::collect(&line, line.len(), &context, &FileLimits::default());
        assert_eq!(choices.matches, [format!("'{extra}/fixture space.txt'")]);
    }

    #[cfg(unix)]
    #[test]
    fn external_and_dangling_symlinks_are_not_offered_or_traversed() {
        let (temp, context) = fixture();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).expect("outside");
        std::fs::write(outside.join("secret.txt"), "secret").expect("secret");
        std::os::unix::fs::symlink(&outside, context.workspace().join("escape")).expect("link");
        std::os::unix::fs::symlink(
            outside.join("secret.txt"),
            context.workspace().join("secret-link"),
        )
        .expect("file link");
        std::os::unix::fs::symlink(
            outside.join("missing"),
            context.workspace().join("dangling"),
        )
        .expect("dangling");
        for name in ["escape", "secret-link", "dangling"] {
            let line = format!("read {name}");
            assert!(
                Paths::collect(&line, line.len(), &context, &FileLimits::default())
                    .matches
                    .is_empty()
            );
            let line = format!("read {name}/");
            let choices =
                Paths::collect_with(&line, line.len(), &context, &FileLimits::default(), |_| {
                    panic!("an external or dangling link must never be listed");
                });
            assert!(choices.matches.is_empty());
        }
    }

    #[test]
    fn listing_is_bounded_and_control_characters_are_not_offered() {
        let (_temp, context) = fixture();
        for name in ["fixture-a", "fixture-b", "fixture-c"] {
            std::fs::write(context.workspace().join(name), "fixture").expect("file");
        }
        #[cfg(unix)]
        std::fs::write(context.workspace().join("control\nname"), "fixture")
            .expect("control filename");
        let limits = FileLimits {
            list_entries: 2,
            ..FileLimits::default()
        };
        assert_eq!(
            Paths::collect("read fixture", 12, &context, &limits)
                .matches
                .len(),
            2
        );
        assert!(
            Paths::collect("read control", 12, &context, &limits)
                .matches
                .is_empty()
        );
        let limits = FileLimits {
            walk_files: 0,
            ..limits
        };
        assert!(
            Paths::collect("read fixture", 12, &context, &limits)
                .matches
                .is_empty()
        );
        let limits = FileLimits {
            list_entries: 0,
            ..limits
        };
        assert!(
            Paths::collect_with("read fixture", 12, &context, &limits, |_| {
                panic!("a zero cap must not read a directory");
            })
            .matches
            .is_empty()
        );
    }
}
