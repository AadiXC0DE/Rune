//! The commands that check a project: its tests, build, type checker and linter.
//!
//! One list serves the default permission rules, the verification state of a
//! turn and the guard that answers a repeated check without running it again.

use crate::command::{is_assignment, program_name, script_argument};
use crate::command_line::{Command, split};

/// Commands that run a project's tests, build, type checker or linter, as leading words.
pub const CHECK_COMMANDS: &[&[&str]] = &[
    &["cargo", "test"],
    &["cargo", "build"],
    &["cargo", "check"],
    &["cargo", "clippy"],
    &["cargo", "nextest"],
    &["npm", "test"],
    &["npm", "run", "test"],
    &["npm", "run", "build"],
    &["npm", "run", "lint"],
    &["npm", "run", "typecheck"],
    &["npm", "run", "check"],
    &["pnpm", "test"],
    &["pnpm", "run", "test"],
    &["pnpm", "run", "build"],
    &["pnpm", "run", "lint"],
    &["pnpm", "run", "typecheck"],
    &["pnpm", "run", "check"],
    &["yarn", "test"],
    &["yarn", "run", "test"],
    &["yarn", "run", "build"],
    &["yarn", "run", "lint"],
    &["yarn", "run", "typecheck"],
    &["yarn", "run", "check"],
    &["bun", "test"],
    &["bun", "run", "test"],
    &["bun", "run", "build"],
    &["bun", "run", "lint"],
    &["bun", "run", "typecheck"],
    &["bun", "run", "check"],
    &["deno", "test"],
    &["node", "--test"],
    &["npx", "vitest"],
    &["npx", "jest"],
    &["npx", "tsc"],
    &["npx", "eslint"],
    &["npx", "mocha"],
    &["pnpx", "vitest"],
    &["pnpx", "jest"],
    &["pnpx", "tsc"],
    &["pnpx", "eslint"],
    &["pnpx", "mocha"],
    &["bunx", "vitest"],
    &["bunx", "jest"],
    &["bunx", "tsc"],
    &["bunx", "eslint"],
    &["bunx", "mocha"],
    &["vitest"],
    &["jest"],
    &["tsc"],
    &["eslint"],
    &["mocha"],
    &["pytest"],
    &["python", "-m", "pytest"],
    &["python3", "-m", "pytest"],
    &["python", "-m", "unittest"],
    &["python3", "-m", "unittest"],
    &["python", "-m", "mypy"],
    &["python3", "-m", "mypy"],
    &["mypy"],
    &["uv", "run", "pytest"],
    &["tox"],
    &["ruff", "check"],
    &["go", "test"],
    &["go", "build"],
    &["go", "vet"],
    &["make", "test"],
    &["make", "check"],
    &["make", "lint"],
    &["just", "test"],
    &["mvn", "test"],
    &["gradle", "test"],
    &["./gradlew", "test"],
    &["dotnet", "test"],
    &["ctest"],
    &["bazel", "test"],
    &["phpunit"],
    &["vendor/bin/phpunit"],
    &["rspec"],
    &["bundle", "exec", "rspec"],
    &["mix", "test"],
];

/// Programs a check's output may be piped into without the line doing more.
const READERS: &[&str] = &[
    "cat", "column", "cut", "egrep", "fgrep", "grep", "head", "less", "more", "nl", "rg", "sort",
    "tail", "tr", "uniq", "wc",
];

/// Shells that run a script file named as their first argument.
const SCRIPT_SHELLS: &[&str] = &["sh", "bash", "zsh"];

/// True when any command the line runs starts with a CHECK_COMMANDS entry,
/// after leading `NAME=value` assignments.
#[must_use]
pub fn runs_check(line: &str) -> bool {
    split(line).iter().any(is_check)
}

/// True when the line runs a workspace script whose file name contains `test` or `check`:
/// `sh X`, `bash X`, `zsh X` or `./X`. Never used for permissions.
#[must_use]
pub fn runs_check_script(line: &str) -> bool {
    split(line).iter().any(|command| {
        let words = program_words(command);
        let Some((program, arguments)) = words.split_first() else {
            return false;
        };
        let script = if program.starts_with("./") {
            Some(program.as_str())
        } else if SCRIPT_SHELLS.contains(&program.as_str()) && script_argument(arguments).is_none()
        {
            arguments
                .iter()
                .find(|argument| !argument.starts_with('-'))
                .map(String::as_str)
        } else {
            None
        };
        script.is_some_and(|script| {
            let name = script.rsplit('/').next().unwrap_or(script).to_lowercase();
            name.contains("test") || name.contains("check")
        })
    })
}

/// True for a line that is exactly one check, optionally piped into readers, with no
/// `&&`, `||`, `;`, `>`, backtick, `$(` or ` tee `: running it again with nothing changed
/// cannot have another effect.
#[must_use]
pub fn is_repeatable_check(line: &str) -> bool {
    let plain = line.replace("2>&1", "");
    if ["&&", "||", ";", ">", "`", "$(", " tee "]
        .iter()
        .any(|operator| plain.contains(operator))
    {
        return false;
    }
    let commands = split(line);
    let Some((first, rest)) = commands.split_first() else {
        return false;
    };
    is_check(first)
        && rest.iter().all(|command| {
            program_words(command)
                .first()
                .is_some_and(|program| READERS.contains(&program.as_str()))
        })
}

/// The allow patterns for the default rules, `"{words joined by spaces}*"`, in table order.
#[must_use]
pub fn allow_patterns() -> Vec<String> {
    CHECK_COMMANDS
        .iter()
        .map(|words| format!("{}*", words.join(" ")))
        .collect()
}

/// Returns true when a command starts with a CHECK_COMMANDS entry.
fn is_check(command: &Command) -> bool {
    let words = program_words(command);
    CHECK_COMMANDS.iter().any(|entry| {
        words.len() >= entry.len()
            && entry
                .iter()
                .zip(&words)
                .all(|(expected, word)| expected == word)
    })
}

/// Returns a command's allow form as words, from the program on, with leading
/// assignments passed over and a program named from a system directory
/// reduced to its name.
fn program_words(command: &Command) -> Vec<String> {
    let form = command.allow_form();
    let mut words: Vec<String> = form
        .split_whitespace()
        .skip_while(|word| is_assignment(word))
        .map(str::to_owned)
        .collect();
    if let Some(program) = words.first_mut() {
        *program = program_name(program).to_owned();
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_matches_its_own_words() {
        for entry in CHECK_COMMANDS {
            let line = entry.join(" ");
            assert!(runs_check(&line), "{line}");
            assert!(is_repeatable_check(&line), "{line}");
            assert!(runs_check(&format!("{line} --verbose")), "{line}");
        }
        assert_eq!(allow_patterns().len(), CHECK_COMMANDS.len());
        assert_eq!(
            allow_patterns().first().map(String::as_str),
            Some("cargo test*")
        );
    }

    #[test]
    fn a_chained_line_is_a_check_but_not_repeatable() {
        for line in [
            "cd sub && cargo test",
            "cargo fmt; cargo test",
            "cargo test || true",
            "cargo test > out.txt",
            "bash -c 'cargo test'",
        ] {
            assert!(runs_check(line), "{line}");
            assert!(!is_repeatable_check(line), "{line}");
        }
    }

    #[test]
    fn assignments_before_the_program_are_skipped() {
        assert!(runs_check("RUST_BACKTRACE=1 cargo test"));
        assert!(runs_check("CI=1 NODE_ENV=test npm test"));
        assert!(is_repeatable_check("RUST_BACKTRACE=1 cargo test -p core"));
        assert!(runs_check("/usr/bin/make test"));
    }

    #[test]
    fn word_boundaries_are_respected() {
        for line in [
            "cargo testing",
            "cargo",
            "echo cargo test",
            "jester",
            "npm run tests",
            "gotest",
            "python -m pytester",
            "",
        ] {
            assert!(!runs_check(line), "{line}");
        }
    }

    #[test]
    fn check_scripts_are_named_for_tests_or_checks() {
        for yes in [
            "sh scripts/run_tests.sh",
            "bash -e ci/check.sh",
            "zsh Test.zsh",
            "./run-tests",
            "cd sub && ./scripts/check_all.sh",
            "/bin/sh check.sh",
        ] {
            assert!(runs_check_script(yes), "{yes}");
        }
        for no in [
            "sh setup.sh",
            "./build.sh",
            "bash -c 'echo test'",
            "python test_x.py",
            "sh",
            "cat check.sh",
        ] {
            assert!(!runs_check_script(no), "{no}");
        }
    }

    #[test]
    fn plain_test_build_and_lint_commands_are_repeatable() {
        for yes in [
            "cargo test -p core",
            "cargo test 2>&1 | tail -40",
            "npm test",
            "npm run lint",
            "npx vitest run",
            "python -m pytest -q",
            "go test ./...",
            "node --test test/",
            "npm run check",
            "pnpm run build",
            "yarn run test",
            "bun run test",
            "npx eslint .",
            "uv run pytest -x",
            "ruff check src",
            "make lint",
            "just test",
            "ctest --output-on-failure",
            "bazel test //...",
            "vendor/bin/phpunit",
            "rspec spec/a_spec.rb",
            "tox -e py39",
            "python3 -m mypy src",
            "cargo test | grep FAILED | head",
        ] {
            assert!(is_repeatable_check(yes), "{yes}");
        }
        for no in [
            "sed -i 's/a/b/' x",
            "cargo fmt && cargo test",
            "cargo test > out.txt",
            "npm install",
            "npm run deploy",
            "cargo test | tee log",
            "make install",
            "cargo test | sh",
            "echo $(cargo test)",
        ] {
            assert!(!is_repeatable_check(no), "{no}");
        }
    }
}
