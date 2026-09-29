// The project the demo works on: a small word counter with one real bug.
// `count_words` splits on a single space, so two spaces in a row count as an
// extra word, and the test in tests/count.rs says so.

export const WORKSPACE = {
  'README.md': `# tally

Counts the words in standard input.

    echo "one two  three" | tally
    3
`,
  'AGENTS.md': `# Working on tally

- Keep functions small and covered by a test in tests/.
- Run \`cargo test\` before calling a change finished.
`,
  'Cargo.toml': `[package]
name = "tally"
version = "0.1.0"
edition = "2024"

[dependencies]
`,
  'src/lib.rs': `pub mod count;
`,
  'src/main.rs': `use std::io::Read;

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap_or_default();
    println!("{}", tally::count::count_words(&input));
}
`,
  'src/count.rs': `/// Returns how many words a text holds.
pub fn count_words(text: &str) -> usize {
    text.trim().split(' ').count()
}

/// Returns how many lines a text holds.
pub fn count_lines(text: &str) -> usize {
    text.lines().count()
}
`,
  'tests/count.rs': `use tally::count::{count_lines, count_words};

#[test]
fn words_are_counted() {
    assert_eq!(count_words("one two three"), 3);
}

#[test]
fn repeated_spaces_are_one_separator() {
    assert_eq!(count_words("one  two"), 2);
}

#[test]
fn lines_are_counted() {
    assert_eq!(count_lines("a\\nb\\n"), 2);
}
`,
};
