//! Splitting a command line into the commands it runs.
//!
//! A permission rule names one command, but one line can run several: `ls;
//! rm -rf .git` runs two, `ls $(curl x)` runs two, and `bash -c 'rm x'` runs
//! the script it is handed. Matching a rule against the whole string lets the
//! first command vouch for everything after it, so a line is split into the
//! commands it runs and each one is matched on its own.
//!
//! The reading is conservative in one direction only. Quoting is honoured, so
//! a separator inside quotes does not split a command, but syntax this module
//! does not follow turns into one more command a rule has to cover, or into a
//! command only an exact rule can allow. A misreading can make a line harder to
//! allow; it cannot make one easier.

use crate::command::{SHELLS, is_assignment, program_name, script_argument};

/// Nesting deeper than this is not followed, and the line is then only allowed
/// by a rule that names it exactly.
const MAX_DEPTH: usize = 32;

/// Words that open or continue a compound command rather than name a program.
const LEADING_KEYWORDS: &[&str] = &[
    "!", "{", "do", "elif", "else", "if", "then", "until", "while",
];

/// Words that close a compound command and run nothing themselves.
const CLOSING_KEYWORDS: &[&str] = &["}", "done", "esac", "fi"];

/// Programs that run the command after their own options, each with the
/// options that consume the word after them.
///
/// Only a refusal looks through these. An allow does not, because a wrapper can
/// change which program runs: `env PATH=/tmp ls` is not the `ls` a rule for
/// `ls` was written about.
const WRAPPERS: &[(&str, &[&str])] = &[
    ("builtin", &[]),
    ("command", &[]),
    ("doas", &["-C", "-u"]),
    (
        "env",
        &["-C", "-S", "-u", "--chdir", "--split-string", "--unset"],
    ),
    ("exec", &["-a"]),
    ("nice", &["-n", "--adjustment"]),
    ("nohup", &[]),
    (
        "sudo",
        &[
            "-C", "-D", "-R", "-T", "-U", "-g", "-h", "-p", "-r", "-t", "-u",
        ],
    ),
    ("time", &["-f", "-o", "--format", "--output"]),
];

/// One simple command a line runs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Command {
    /// The command as written, trimmed.
    pub raw: String,
    /// The words after quoting is removed, redirections left out.
    pub words: Vec<String>,
    /// True when only a rule naming the command exactly can allow it: it
    /// writes a file through a redirection, or its text could not be read to
    /// the end.
    pub needs_exact: bool,
}

impl Command {
    /// Returns the words an allow rule reads, from the program on.
    ///
    /// Only spellings that cannot change which program runs are passed over:
    /// the keywords of a compound command, and `command` or `time` in front of
    /// the program.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        let mut words = self.words.as_slice();
        while let Some((first, rest)) = words.split_first() {
            if LEADING_KEYWORDS.contains(&first.as_str()) {
                words = rest;
            } else if first == "command" || first == "time" {
                words = match rest.split_first() {
                    Some((flag, after)) if flag == "-p" => after,
                    _ => rest,
                };
            } else {
                break;
            }
        }
        words
    }

    /// Returns the form an allow rule is matched against, with a program named
    /// from a system directory reduced to its name.
    #[must_use]
    pub fn allow_form(&self) -> String {
        join(self.argv(), program_name)
    }

    /// Returns every form a deny or ask rule is matched against.
    ///
    /// A refusal has to hold however one program is spelled, so beyond the
    /// allow form this looks through assignments and wrappers and reduces a
    /// program named by any path to its file name.
    #[must_use]
    pub fn deny_forms(&self) -> Vec<String> {
        let mut forms = vec![self.raw.clone(), self.allow_form()];
        let unwrapped = unwrap(&self.words, &mut Vec::new());
        forms.push(join(unwrapped, base_name));
        let mut unique = Vec::with_capacity(forms.len());
        for form in forms {
            if !unique.contains(&form) {
                unique.push(form);
            }
        }
        unique
    }

    /// Returns the command lines this command hands to another interpreter.
    fn scripts(&self) -> Vec<String> {
        let mut scripts = Vec::new();
        let unwrapped = unwrap(&self.words, &mut scripts);
        if let Some((program, arguments)) = unwrapped.split_first() {
            let name = base_name(program);
            if SHELLS.contains(&name) {
                if let Some(script) = script_argument(arguments) {
                    scripts.push(script.to_owned());
                }
            } else if name == "eval" {
                scripts.push(arguments.join(" "));
            }
        }
        scripts
    }
}

/// Splits a command line into the commands it runs, nested ones included.
///
/// A substitution, a subshell, and a script handed to a shell or to
/// `env -S` each contribute their own commands, so a rule has to cover every
/// one of them.
#[must_use]
pub fn split(line: &str) -> Vec<Command> {
    let mut parser = Parser::new(line, 0);
    let _ = parser.list(None);
    parser.into_commands()
}

/// Joins words back into a line, with the program rewritten by `name`.
fn join(words: &[String], name: fn(&str) -> &str) -> String {
    let mut out = String::new();
    for (index, word) in words.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(if index == 0 { name(word) } else { word });
    }
    out
}

/// Returns the file name a program is named by, whatever path precedes it.
fn base_name(program: &str) -> &str {
    program.rsplit(['/', '\\']).next().unwrap_or(program)
}

/// Removes what runs before the program: keywords, assignments, and wrappers.
///
/// A string handed to `env -S` is a command line of its own, so it is
/// collected into `scripts` as it is passed over.
fn unwrap<'a>(mut words: &'a [String], scripts: &mut Vec<String>) -> &'a [String] {
    while let Some((first, rest)) = words.split_first() {
        if LEADING_KEYWORDS.contains(&first.as_str()) || is_assignment(first) {
            words = rest;
            continue;
        }
        let name = base_name(first);
        let Some((_, takes_value)) = WRAPPERS.iter().find(|(wrapper, _)| *wrapper == name) else {
            break;
        };
        let mut index = 0_usize;
        while let Some(option) = rest.get(index) {
            if option == "--" {
                index = index.saturating_add(1);
                break;
            }
            if !option.starts_with('-') {
                break;
            }
            let (consumes, value) =
                option_value(option, takes_value, rest.get(index.saturating_add(1)));
            if name == "env"
                && let Some(value) = value
                && is_split_string(option)
            {
                scripts.push(value.to_owned());
            }
            index = index.saturating_add(if consumes { 2 } else { 1 });
        }
        words = rest.get(index..).unwrap_or_default();
    }
    words
}

/// Returns whether an option consumes the next word, and the value it takes.
///
/// A short cluster such as `-iS` takes its value from the rest of the cluster
/// or, when the value-taking letter is last, from the next word.
fn option_value<'a>(
    option: &'a str,
    takes_value: &[&str],
    next: Option<&'a String>,
) -> (bool, Option<&'a str>) {
    if takes_value.contains(&option) {
        return (true, next.map(String::as_str));
    }
    if let Some((flag, value)) = option.split_once('=')
        && flag.starts_with("--")
    {
        return (false, takes_value.contains(&flag).then_some(value));
    }
    if option.starts_with("--") {
        return (false, None);
    }
    for (offset, letter) in option.char_indices().skip(1) {
        let short = format!("-{letter}");
        if takes_value.contains(&short.as_str()) {
            let rest = &option[offset.saturating_add(letter.len_utf8())..];
            return if rest.is_empty() {
                (true, next.map(String::as_str))
            } else {
                (false, Some(rest))
            };
        }
    }
    (false, None)
}

/// Returns true for the `env` option that splits its value into a command.
fn is_split_string(option: &str) -> bool {
    option.starts_with("--split-string")
        || (option.starts_with('-') && !option.starts_with("--") && option.contains('S'))
}

/// How a redirection treats its target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Redirect {
    /// Writes the target, unless it is `/dev/null`.
    Output,
    /// Duplicates a descriptor when the target is one, and writes a file
    /// otherwise.
    Duplicate,
    /// Reads the target, which changes nothing.
    Input,
    /// A here-document, whose body this reader does not follow.
    HereDocument,
}

impl Redirect {
    /// Returns true when redirecting to `target` writes a file.
    fn writes_to(self, target: &str) -> bool {
        match self {
            Self::Output => target != "/dev/null",
            Self::Duplicate => {
                target != "-" && !target.chars().all(|character| character.is_ascii_digit())
            }
            Self::Input => false,
            Self::HereDocument => true,
        }
    }
}

/// The command being read, until a separator ends it.
#[derive(Debug)]
struct Pending {
    start: usize,
    words: Vec<String>,
    word: String,
    in_word: bool,
    redirect: Option<Redirect>,
    needs_exact: bool,
}

impl Pending {
    fn at(start: usize) -> Self {
        Self {
            start,
            words: Vec::new(),
            word: String::new(),
            in_word: false,
            redirect: None,
            needs_exact: false,
        }
    }

    fn push(&mut self, character: char) {
        self.word.push(character);
        self.in_word = true;
    }

    fn end_word(&mut self) {
        if !self.in_word {
            return;
        }
        let word = std::mem::take(&mut self.word);
        self.in_word = false;
        match self.redirect.take() {
            Some(redirect) => {
                if redirect.writes_to(&word) {
                    self.needs_exact = true;
                }
            }
            None => self.words.push(word),
        }
    }
}

/// A reader over one command line.
struct Parser {
    chars: Vec<char>,
    pos: usize,
    depth: usize,
    commands: Vec<Command>,
    too_deep: bool,
}

impl Parser {
    fn new(line: &str, depth: usize) -> Self {
        Self {
            chars: line.chars().collect(),
            pos: 0,
            depth,
            commands: Vec::new(),
            too_deep: false,
        }
    }

    fn into_commands(mut self) -> Vec<Command> {
        if self.too_deep {
            for command in &mut self.commands {
                command.needs_exact = true;
            }
        }
        self.commands
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos.saturating_add(ahead)).copied()
    }

    fn advance(&mut self, count: usize) {
        self.pos = self.pos.saturating_add(count).min(self.chars.len());
    }

    /// Reads commands until `close` or the end of the line, returning true
    /// when the list ended where it was expected to.
    fn list(&mut self, close: Option<char>) -> bool {
        let mut pending = Pending::at(self.pos);
        while let Some(character) = self.peek(0) {
            if close == Some(character) {
                self.finish(pending, self.pos);
                self.advance(1);
                return true;
            }
            match character {
                '\\' => self.escape(&mut pending),
                '\'' => self.single_quoted(&mut pending, false),
                '"' => self.double_quoted(&mut pending),
                '$' if self.peek(1) == Some('\'') => {
                    self.advance(1);
                    self.single_quoted(&mut pending, true);
                }
                '$' if self.peek(1) == Some('"') => {
                    self.advance(1);
                    self.double_quoted(&mut pending);
                }
                '$' if self.peek(1) == Some('(') => self.substitution(&mut pending, 2, ')'),
                '`' => self.substitution(&mut pending, 1, '`'),
                '<' | '>' if self.peek(1) == Some('(') => self.substitution(&mut pending, 2, ')'),
                '<' | '>' => self.redirect(&mut pending),
                '&' if self.peek(1) == Some('>') => self.redirect(&mut pending),
                '(' => {
                    self.finish(pending, self.pos);
                    self.advance(1);
                    let _ = self.nest(Some(')'));
                    pending = Pending::at(self.pos);
                }
                ')' | ';' | '&' | '|' | '\n' => {
                    self.finish(pending, self.pos);
                    self.advance(1);
                    pending = Pending::at(self.pos);
                }
                '#' if !pending.in_word => self.comment(),
                ' ' | '\t' | '\r' => {
                    pending.end_word();
                    self.advance(1);
                }
                other => {
                    pending.push(other);
                    self.advance(1);
                }
            }
        }
        if close.is_some() {
            pending.needs_exact = true;
        }
        self.finish(pending, self.pos);
        close.is_none()
    }

    /// Reads a nested list, unless the line is nested too deeply to follow.
    fn nest(&mut self, close: Option<char>) -> bool {
        if self.depth >= MAX_DEPTH {
            self.too_deep = true;
            self.pos = self.chars.len();
            return false;
        }
        self.depth = self.depth.saturating_add(1);
        let closed = self.list(close);
        self.depth = self.depth.saturating_sub(1);
        closed
    }

    /// Reads a command line handed to another interpreter.
    fn nest_line(&mut self, line: &str) {
        if self.depth >= MAX_DEPTH {
            self.too_deep = true;
            return;
        }
        let mut nested = Parser::new(line, self.depth.saturating_add(1));
        let _ = nested.list(None);
        self.too_deep |= nested.too_deep;
        self.commands.append(&mut nested.commands);
    }

    /// Records the command read so far, and any command line it hands on.
    fn finish(&mut self, mut pending: Pending, end: usize) {
        pending.end_word();
        if pending.redirect.is_some() {
            pending.needs_exact = true;
        }
        let closing_only = pending
            .words
            .iter()
            .all(|word| CLOSING_KEYWORDS.contains(&word.as_str()));
        if closing_only && !pending.needs_exact {
            return;
        }
        let start = pending.start.min(end);
        let raw = self.chars[start..end]
            .iter()
            .collect::<String>()
            .trim()
            .to_owned();
        let command = Command {
            raw,
            words: pending.words,
            needs_exact: pending.needs_exact,
        };
        for script in command.scripts() {
            self.nest_line(&script);
        }
        self.commands.push(command);
    }

    fn escape(&mut self, pending: &mut Pending) {
        match self.peek(1) {
            // A line continuation joins the two lines into one word.
            Some('\n') => self.advance(2),
            Some(next) => {
                pending.push(next);
                self.advance(2);
            }
            None => self.advance(1),
        }
    }

    fn single_quoted(&mut self, pending: &mut Pending, escapes: bool) {
        self.advance(1);
        pending.in_word = true;
        loop {
            match self.peek(0) {
                Some('\'') => {
                    self.advance(1);
                    return;
                }
                Some('\\') if escapes => {
                    if let Some(next) = self.peek(1) {
                        pending.word.push(next);
                    }
                    self.advance(2);
                }
                Some(character) => {
                    pending.word.push(character);
                    self.advance(1);
                }
                None => {
                    pending.needs_exact = true;
                    return;
                }
            }
        }
    }

    fn double_quoted(&mut self, pending: &mut Pending) {
        self.advance(1);
        pending.in_word = true;
        loop {
            match self.peek(0) {
                Some('"') => {
                    self.advance(1);
                    return;
                }
                Some('\\') => match self.peek(1) {
                    Some('\n') => self.advance(2),
                    Some(next @ ('"' | '\\' | '$' | '`')) => {
                        pending.word.push(next);
                        self.advance(2);
                    }
                    _ => {
                        pending.word.push('\\');
                        self.advance(1);
                    }
                },
                Some('$') if self.peek(1) == Some('(') => self.substitution(pending, 2, ')'),
                Some('`') => self.substitution(pending, 1, '`'),
                Some(character) => {
                    pending.word.push(character);
                    self.advance(1);
                }
                None => {
                    pending.needs_exact = true;
                    return;
                }
            }
        }
    }

    /// Reads a substitution, whose commands run before the one around it.
    fn substitution(&mut self, pending: &mut Pending, open: usize, close: char) {
        let start = self.pos;
        self.advance(open);
        if !self.nest(Some(close)) {
            pending.needs_exact = true;
        }
        let end = self.pos.min(self.chars.len());
        pending.word.extend(&self.chars[start..end]);
        pending.in_word = true;
    }

    fn redirect(&mut self, pending: &mut Pending) {
        // A descriptor number written against the operator belongs to it.
        if pending.in_word
            && !pending.word.is_empty()
            && pending
                .word
                .chars()
                .all(|character| character.is_ascii_digit())
        {
            pending.word.clear();
            pending.in_word = false;
        } else {
            pending.end_word();
        }
        let (redirect, length) = match (self.peek(0), self.peek(1), self.peek(2)) {
            (Some('<'), Some('<'), Some('<')) => (Redirect::Input, 3),
            (Some('<'), Some('<'), Some('-')) => (Redirect::HereDocument, 3),
            (Some('<'), Some('<'), _) => (Redirect::HereDocument, 2),
            (Some('<'), Some('&'), _) => (Redirect::Input, 2),
            (Some('<'), Some('>'), _) => (Redirect::Output, 2),
            (Some('<'), _, _) => (Redirect::Input, 1),
            (Some('>'), Some('&'), _) => (Redirect::Duplicate, 2),
            (Some('>'), Some('>' | '|'), _) => (Redirect::Output, 2),
            (Some('&'), Some('>'), Some('>')) => (Redirect::Output, 3),
            (Some('&'), Some('>'), _) => (Redirect::Output, 2),
            _ => (Redirect::Output, 1),
        };
        self.advance(length);
        if redirect == Redirect::HereDocument {
            pending.needs_exact = true;
        }
        pending.redirect = Some(redirect);
    }

    fn comment(&mut self) {
        while let Some(character) = self.peek(0) {
            if character == '\n' {
                return;
            }
            self.advance(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raws(line: &str) -> Vec<String> {
        split(line).into_iter().map(|command| command.raw).collect()
    }

    fn allow_forms(line: &str) -> Vec<String> {
        split(line)
            .iter()
            .map(Command::allow_form)
            .collect::<Vec<_>>()
    }

    #[test]
    fn a_plain_command_is_one_command() {
        assert_eq!(raws("  git status --short "), vec!["git status --short"]);
        assert_eq!(allow_forms("git  status"), vec!["git status"]);
    }

    #[test]
    fn every_separator_starts_another_command() {
        for (line, count) in [
            ("ls; rm -rf .git", 2),
            ("ls && rm x", 2),
            ("ls || rm x", 2),
            ("ls | sh", 2),
            ("ls & rm x", 2),
            ("ls\nrm x", 2),
            ("ls |& tee", 2),
            ("ls && curl x | sh", 3),
        ] {
            assert_eq!(split(line).len(), count, "{line}");
        }
    }

    #[test]
    fn a_quoted_separator_does_not_split() {
        assert_eq!(
            allow_forms("echo \"a;b\" 'c|d' e\\&f"),
            vec!["echo a;b c|d e&f"]
        );
    }

    #[test]
    fn substitutions_and_subshells_contribute_their_commands() {
        for line in [
            "echo $(rm -rf x)",
            "echo `rm -rf x`",
            "echo \"$(rm -rf x)\"",
            "(rm -rf x)",
            "cat <(rm -rf x)",
            "bash -c 'rm -rf x'",
            "sh -lc \"rm -rf x\"",
            "env -S 'rm -rf x'",
            "env --split-string='rm -rf x'",
            "eval rm -rf x",
        ] {
            let commands = split(line);
            assert!(
                commands
                    .iter()
                    .any(|command| command.allow_form() == "rm -rf x"),
                "{line}: {commands:?}"
            );
        }
    }

    #[test]
    fn a_comment_ends_at_the_newline() {
        let commands = split("ls # it's fine\nrm -rf x");
        let forms = commands.iter().map(Command::allow_form).collect::<Vec<_>>();
        assert_eq!(forms, vec!["ls", "rm -rf x"]);
    }

    #[test]
    fn a_redirection_that_writes_needs_an_exact_rule() {
        for line in [
            "ls > out.txt",
            "ls >> out",
            "ls &> out",
            "ls >&out",
            "> out",
            "cat <<EOF",
        ] {
            assert!(
                split(line).iter().any(|command| command.needs_exact),
                "{line}"
            );
        }
        for line in [
            "ls 2>&1",
            "ls >&2",
            "ls 2>/dev/null",
            "ls < in.txt",
            "ls 2>&-",
        ] {
            let commands = split(line);
            assert_eq!(commands.len(), 1, "{line}");
            assert!(!commands[0].needs_exact, "{line}");
            assert_eq!(commands[0].allow_form(), "ls", "{line}");
        }
    }

    #[test]
    fn an_unterminated_quote_needs_an_exact_rule() {
        assert!(
            split("ls 'unclosed")
                .iter()
                .all(|command| command.needs_exact)
        );
        assert!(split("ls $(pwd").iter().any(|command| command.needs_exact));
    }

    #[test]
    fn the_allow_form_drops_only_what_cannot_change_the_program() {
        assert_eq!(allow_forms("/bin/ls -la"), vec!["ls -la"]);
        assert_eq!(allow_forms("command ls"), vec!["ls"]);
        assert_eq!(allow_forms("if ls; then pwd; fi"), vec!["ls", "pwd"]);
        assert_eq!(allow_forms("PATH=/tmp ls"), vec!["PATH=/tmp ls"]);
        assert_eq!(allow_forms("env FOO=1 ls"), vec!["env FOO=1 ls"]);
        assert_eq!(allow_forms("./ls"), vec!["./ls"]);
    }

    #[test]
    fn the_deny_forms_see_through_every_spelling_of_a_program() {
        for line in [
            " rm -rf x",
            "/bin/rm -rf x",
            "/usr/bin/rm -rf x",
            "./rm -rf x",
            "command rm -rf x",
            "env FOO=1 rm -rf x",
            "env -i rm -rf x",
            "FOO=1 rm -rf x",
            "sudo -u root rm -rf x",
            "nice -n 5 rm -rf x",
            "'rm' -rf x",
            "r\\m -rf x",
        ] {
            let forms = split(line)
                .iter()
                .flat_map(Command::deny_forms)
                .collect::<Vec<_>>();
            assert!(forms.contains(&"rm -rf x".to_owned()), "{line}: {forms:?}");
        }
    }

    #[test]
    fn closing_keywords_run_nothing() {
        assert_eq!(
            allow_forms("for f in a; do rm $f; done"),
            vec!["for f in a", "rm $f"]
        );
        assert_eq!(allow_forms("{ ls; }"), vec!["ls"]);
    }

    #[test]
    fn deep_nesting_is_bounded_and_needs_an_exact_rule() {
        let line = format!("{}ls{}", "$(".repeat(10_000), ")".repeat(10_000));
        let commands = split(&line);
        assert!(!commands.is_empty());
        assert!(commands.iter().all(|command| command.needs_exact));
    }
}
