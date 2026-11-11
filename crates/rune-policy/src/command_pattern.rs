//! The rule saved when a command is allowed for a project.
//!
//! A saved rule keeps the program and the words that name what it does, so
//! `git status -s` saves `git status*` and covers later `git status` calls
//! without covering `git push`. Anything that could hide a second program is
//! never saved.

use crate::command::{CommandKind, classify, is_assignment, program_name};
use crate::command_line::split;

/// Program words and how many leading words a saved pattern keeps for them.
/// The longest matching prefix wins; a program not listed keeps one word.
pub const ARITY: &[(&[&str], usize)] = &[
    (&["git"], 2),
    (&["npm"], 2),
    (&["npm", "run"], 3),
    (&["pnpm"], 2),
    (&["pnpm", "run"], 3),
    (&["yarn"], 2),
    (&["yarn", "run"], 3),
    (&["bun"], 2),
    (&["bun", "run"], 3),
    (&["npx"], 2),
    (&["pnpx"], 2),
    (&["bunx"], 2),
    (&["cargo"], 2),
    (&["go"], 2),
    (&["make"], 2),
    (&["just"], 2),
    (&["uv"], 2),
    (&["uv", "run"], 3),
    (&["poetry"], 2),
    (&["poetry", "run"], 3),
    (&["python", "-m"], 3),
    (&["python3", "-m"], 3),
    (&["node", "--test"], 2),
    (&["pip"], 2),
    (&["docker"], 2),
    (&["docker", "compose"], 3),
    (&["kubectl"], 2),
    (&["dotnet"], 2),
    (&["mvn"], 2),
    (&["gradle"], 2),
    (&["./gradlew"], 2),
    (&["bundle", "exec"], 3),
    (&["mix"], 2),
    (&["deno"], 2),
    (&["rustup"], 2),
    (&["brew"], 2),
    (&["gh"], 3),
    (&["ruff"], 2),
    (&["bazel"], 2),
];

/// Programs that run code handed to them as an argument.
const INTERPRETERS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "fish", "python", "python3", "node", "perl", "ruby", "php",
];

/// Arguments that hand an interpreter code to run.
const CODE_FLAGS: &[&str] = &["-c", "-e", "--eval", "-p"];

/// Returns the pattern to save for a command line, or None when no rule may be saved
/// for it.
#[must_use]
pub fn pattern_for(line: &str) -> Option<String> {
    let commands = split(line);
    let [command] = commands.as_slice() else {
        return None;
    };
    if command.needs_exact {
        return None;
    }
    let argv = command.argv();
    let (first, _) = argv.split_first()?;
    if is_assignment(first) {
        return None;
    }
    let mut words: Vec<&str> = argv.iter().map(String::as_str).collect();
    words[0] = program_name(first);
    let base = words[0].rsplit(['/', '\\']).next().unwrap_or(words[0]);
    if INTERPRETERS.contains(&base) && words.iter().any(|word| CODE_FLAGS.contains(word)) {
        return None;
    }
    if classify(argv).kind == CommandKind::Consequential {
        return None;
    }
    let (prefix, arity) = ARITY
        .iter()
        .filter(|(prefix, _)| words.starts_with(prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map_or((1, 1), |(prefix, arity)| (prefix.len(), *arity));
    let kept = &words[..arity.min(words.len())];
    // A flag before the subcommand changes what the kept words mean, and a
    // wildcard in a kept word would widen the rule past what was approved.
    if kept.iter().skip(prefix).any(|word| word.starts_with('-'))
        || kept.iter().any(|word| word.contains(['*', '?']))
    {
        return None;
    }
    let joined = kept.join(" ");
    if words.len() < arity {
        Some(joined)
    } else {
        Some(format!("{joined}*"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(line: &str) -> Option<String> {
        pattern_for(line)
    }

    #[test]
    fn a_subcommand_is_kept_with_its_program() {
        assert_eq!(pattern("git status -s").as_deref(), Some("git status*"));
    }

    #[test]
    fn a_flag_before_the_subcommand_saves_nothing() {
        assert_eq!(pattern("git -C x status"), None);
    }

    #[test]
    fn a_script_name_is_kept_after_run() {
        assert_eq!(pattern("npm run test").as_deref(), Some("npm run test*"));
        assert_eq!(
            pattern("npm run test -- --watch").as_deref(),
            Some("npm run test*")
        );
    }

    #[test]
    fn an_unlisted_program_keeps_one_word() {
        assert_eq!(pattern("pytest tests/unit").as_deref(), Some("pytest*"));
    }

    #[test]
    fn code_handed_to_an_interpreter_saves_nothing() {
        assert_eq!(pattern("sh -c 'npm test'"), None);
        assert_eq!(pattern("python3 -c 'print(1)'"), None);
        assert_eq!(pattern("node -e 'process.exit(0)'"), None);
    }

    #[test]
    fn a_chained_line_saves_nothing() {
        assert_eq!(pattern("npm test && rm -rf x"), None);
        assert_eq!(pattern("npm test | tee log"), None);
        assert_eq!(pattern("echo $(git status)"), None);
    }

    #[test]
    fn a_redirection_saves_nothing() {
        assert_eq!(pattern("cargo test > out.txt"), None);
    }

    #[test]
    fn a_consequential_command_saves_nothing() {
        assert_eq!(pattern("rm -rf build"), None);
        assert_eq!(pattern("git push"), None);
    }

    #[test]
    fn fewer_words_than_the_arity_save_the_exact_line() {
        assert_eq!(pattern("make").as_deref(), Some("make"));
    }

    #[test]
    fn a_wildcard_or_assignment_saves_nothing() {
        assert_eq!(pattern("cargo t?st"), None);
        assert_eq!(pattern("FOO=1 npm test"), None);
    }

    #[test]
    fn check_patterns_agree_with_pattern_for() {
        for (words, allow) in crate::checks::CHECK_COMMANDS
            .iter()
            .zip(crate::checks::allow_patterns())
        {
            assert_eq!(
                pattern_for(&words.join(" ")).as_deref(),
                Some(allow.as_str()),
                "{words:?}"
            );
        }
    }
}
