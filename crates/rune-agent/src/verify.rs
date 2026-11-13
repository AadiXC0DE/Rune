//! Whether a turn checked its own change, and what the check showed.
//!
//! The classification reads only the calls the turn made and what they
//! returned, so the same calls always give the same verdict.

use rune_policy::checks;

use crate::turn::CallResult;

/// Whether a turn checked its own change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verification {
    /// The turn changed no file.
    NotApplicable,
    /// A listed check ran after the last change and passed.
    Verified,
    /// No listed check ran after the last change, but another command did and
    /// exited 0.
    Checked,
    /// The latest listed check after the last change failed.
    Failed,
    /// A file changed and nothing checked it afterwards.
    Unverified,
}

impl Verification {
    /// Returns the stable label used in machine output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Verified => "verified",
            Self::Checked => "checked",
            Self::Failed => "failed",
            Self::Unverified => "unverified",
        }
    }

    /// Classifies a turn's calls.
    ///
    /// The latest check after the last change decides. Without one, a command
    /// other than a plain read that exited 0 after the change counts as
    /// checked, though never as verified.
    #[must_use]
    pub fn of(calls: &[CallResult]) -> Self {
        let Some(last_change) = calls.iter().rposition(is_change) else {
            return Self::NotApplicable;
        };
        let after = || {
            calls
                .iter()
                .enumerate()
                .skip(last_change.saturating_add(1))
                .rev()
        };
        if let Some(record) = after().find_map(|(index, call)| check_record(index, call)) {
            return if record.passed {
                Self::Verified
            } else {
                Self::Failed
            };
        }
        let other = after().map(|(_, call)| call).find(|call| {
            call.executed
                && call.call.name == "shell"
                && call.output.check.is_some()
                && is_check_command(&call.call.arguments)
        });
        match other.and_then(|call| call.output.check.as_ref()) {
            Some(check) if check.exit_code == Some(0) => Self::Checked,
            _ => Self::Unverified,
        }
    }
}

/// One check a turn ran, with its outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRecord {
    /// Position in the turn's calls.
    pub index: usize,
    /// Identifier of the call.
    pub call_id: String,
    /// The shell command, or `syntax_diagnostics {path}`.
    pub command: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Exit status, when the command exited.
    pub exit_code: Option<i32>,
    /// Failing test names, in order, at most ten.
    pub failing: Vec<String>,
    /// What identifies this failure among others of the same command.
    pub signature: String,
}

/// What a turn's checks showed, for reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckSummary {
    /// Whether any changed path looks like a test file.
    pub tests_modified: bool,
    /// Distinct paths changed by the edit tools, in first-change order.
    pub changed_files: Vec<String>,
    /// The latest check after the last change.
    pub last_check: Option<CheckRecord>,
    /// Notes the harness added because a check kept failing, 0 or 1.
    pub stall_notes: u32,
}

/// Work that has stopped making progress, read from the turn's calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stall {
    /// A check failed the same way again after at least one change.
    SameFailure {
        /// Changes made since the earliest failure of the run.
        changes: usize,
    },
    /// One file was changed over and over while the latest check failed.
    SameFile {
        /// The path changed most.
        path: String,
        /// Changes to that path in the turn.
        changes: usize,
    },
}

/// Changes to one path, at this count or more, while the latest check fails.
const SAME_FILE_CHANGES: usize = 4;

/// Returns true for a call that changed a file.
#[must_use]
pub fn is_change(call: &CallResult) -> bool {
    call.executed
        && !call.output.is_error
        && matches!(
            call.call.name.as_str(),
            "write_file" | "edit_file" | "rename_file" | "apply_patch"
        )
}

/// Returns the paths a change call names, in the order it names them.
#[must_use]
pub fn changed_paths(call: &CallResult) -> Vec<String> {
    if !is_change(call) {
        return Vec::new();
    }
    let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&call.call.arguments) else {
        return Vec::new();
    };
    let field = |name: &str| {
        arguments
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    match call.call.name.as_str() {
        "apply_patch" => field("input")
            .map(|input| rune_tools::apply_patch::paths(&input))
            .unwrap_or_default(),
        "rename_file" => [field("path"), field("new_path")]
            .into_iter()
            .flatten()
            .collect(),
        _ => field("path").into_iter().collect(),
    }
}

/// Returns the check a call ran, once its outcome is known.
#[must_use]
pub fn check_record(index: usize, call: &CallResult) -> Option<CheckRecord> {
    if !call.executed {
        return None;
    }
    let arguments = serde_json::from_str::<serde_json::Value>(&call.call.arguments).ok()?;
    let field = |name: &str| arguments.get(name).and_then(serde_json::Value::as_str);
    match call.call.name.as_str() {
        "syntax_diagnostics" => Some(CheckRecord {
            index,
            call_id: call.call.id.clone(),
            command: format!("syntax_diagnostics {}", field("path").unwrap_or_default()),
            passed: !call.output.is_error,
            exit_code: None,
            failing: Vec::new(),
            signature: String::new(),
        }),
        "shell" => {
            let check = call.output.check.as_ref()?;
            let command = match field("action")? {
                "run" => field("command")?.trim().to_owned(),
                // A session's output starts with the command it was started with.
                "interact" => call
                    .output
                    .text
                    .lines()
                    .next()?
                    .strip_prefix("$ ")?
                    .trim()
                    .to_owned(),
                _ => return None,
            };
            if !(checks::runs_check(&command) || checks::runs_check_script(&command)) {
                return None;
            }
            let passed = check.failed_tests.unwrap_or(0) == 0 && check.exit_code == Some(0);
            Some(CheckRecord {
                index,
                call_id: call.call.id.clone(),
                command,
                passed,
                exit_code: check.exit_code,
                failing: check.failing.clone(),
                signature: check.signature.clone(),
            })
        }
        _ => None,
    }
}

/// Looks for a stall completed by `calls[index]`, given every call of the turn so far.
#[must_use]
pub fn stall_at(calls: &[CallResult], index: usize) -> Option<Stall> {
    let call = calls.get(index)?;
    let earlier = calls.get(..index)?;
    if let Some(record) = check_record(index, call) {
        if record.passed {
            return None;
        }
        let mut earliest = None;
        for (position, previous) in earlier.iter().enumerate().rev() {
            let Some(previous) = check_record(position, previous) else {
                continue;
            };
            if previous.command != record.command {
                continue;
            }
            if previous.passed || previous.signature != record.signature {
                break;
            }
            earliest = Some(position);
        }
        let start = earliest?.saturating_add(1);
        let changes = earlier
            .iter()
            .skip(start)
            .filter(|call| is_change(call))
            .count();
        return (changes > 0).then_some(Stall::SameFailure { changes });
    }
    let latest_failed = earlier
        .iter()
        .enumerate()
        .rev()
        .find_map(|(position, previous)| check_record(position, previous))
        .is_some_and(|record| !record.passed);
    if !latest_failed {
        return None;
    }
    changed_paths(call).into_iter().find_map(|path| {
        let changes = calls
            .iter()
            .take(index.saturating_add(1))
            .filter(|call| changed_paths(call).contains(&path))
            .count();
        (changes >= SAME_FILE_CHANGES).then_some(Stall::SameFile { path, changes })
    })
}

/// Returns true for a path that looks like a test file.
#[must_use]
pub fn is_test_path(path: &str) -> bool {
    let mut components: Vec<&str> = path.split('/').collect();
    let name = components.pop().unwrap_or_default();
    if components
        .iter()
        .any(|part| matches!(*part, "test" | "tests" | "__tests__" | "spec"))
    {
        return true;
    }
    name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || (name.starts_with("test_") && name.rsplit_once('.').is_some_and(|(_, ext)| ext == "py"))
}

/// Summarizes a turn's changes and its latest check after the last change.
#[must_use]
pub fn summarize(calls: &[CallResult], stall_noted: bool) -> CheckSummary {
    let mut changed_files: Vec<String> = Vec::new();
    for path in calls.iter().flat_map(changed_paths) {
        if !changed_files.contains(&path) {
            changed_files.push(path);
        }
    }
    let start = calls
        .iter()
        .rposition(is_change)
        .map_or(0, |last| last.saturating_add(1));
    let last_check = calls
        .iter()
        .enumerate()
        .skip(start)
        .rev()
        .find_map(|(index, call)| check_record(index, call));
    CheckSummary {
        tests_modified: changed_files.iter().any(|path| is_test_path(path)),
        changed_files,
        last_check,
        stall_notes: u32::from(stall_noted),
    }
}

/// Returns true when a shell call ran something other than a plain read.
pub(crate) fn is_check_command(arguments: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return false;
    };
    if value.get("action").and_then(serde_json::Value::as_str) != Some("run") {
        return false;
    }
    let command = value
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    let program = command.split_whitespace().next().unwrap_or_default();
    let reads = [
        "ls", "cat", "head", "tail", "wc", "pwd", "echo", "grep", "rg", "find", "tree", "file",
        "stat", "which",
    ];
    let git_read = command.starts_with("git ")
        && ["status", "diff", "log", "show", "branch"]
            .iter()
            .any(|sub| command.split_whitespace().nth(1) == Some(sub));
    !command.is_empty() && !reads.contains(&program) && !git_read
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::PreparedCall;
    use rune_tools::{CheckRun, ToolOutput};

    fn call(name: &str, arguments: &serde_json::Value, output: ToolOutput) -> CallResult {
        CallResult {
            call: PreparedCall {
                id: format!("call-{name}"),
                name: name.to_owned(),
                arguments: arguments.to_string(),
            },
            output,
            executed: true,
        }
    }

    fn edit(path: &str) -> CallResult {
        call(
            "edit_file",
            &serde_json::json!({"path": path, "old_string": "a", "new_string": "b"}),
            ToolOutput::success("edited"),
        )
    }

    fn run(command: &str, exit_code: i32, failed_tests: Option<u64>) -> CallResult {
        let mut output = if exit_code == 0 {
            ToolOutput::success(format!("$ {command}\n"))
        } else {
            ToolOutput::failure(format!("$ {command}\n"))
        };
        output.check = Some(CheckRun {
            exit_code: Some(exit_code),
            failed_tests,
            failing: Vec::new(),
            signature: String::new(),
        });
        call(
            "shell",
            &serde_json::json!({"action": "run", "command": command}),
            output,
        )
    }

    fn failing(command: &str, signature: &str) -> CallResult {
        let mut result = run(command, 101, Some(1));
        if let Some(check) = result.output.check.as_mut() {
            check.signature = signature.to_owned();
        }
        result
    }

    /// Whether a stall ends at any call of the sequence.
    fn stalls(calls: &[CallResult]) -> Vec<Stall> {
        (0..calls.len())
            .filter_map(|index| stall_at(calls, index))
            .collect()
    }

    #[test]
    fn the_same_failure_after_a_change_is_a_stall() {
        let calls = [
            failing("cargo test", "a"),
            edit("src/a.rs"),
            failing("cargo test", "a"),
        ];
        assert_eq!(stall_at(&calls, 2), Some(Stall::SameFailure { changes: 1 }));
        assert_eq!(stall_at(&calls, 0), None);
        // An identical rerun with nothing changed is not a stall.
        let rerun = [failing("cargo test", "a"), failing("cargo test", "a")];
        assert_eq!(stalls(&rerun), Vec::new());
        // Checks of another command do not break the run.
        let mixed = [
            failing("cargo test", "a"),
            edit("src/a.rs"),
            run("cargo build", 0, None),
            edit("src/a.rs"),
            failing("cargo test", "a"),
        ];
        assert_eq!(stall_at(&mixed, 4), Some(Stall::SameFailure { changes: 2 }));
    }

    #[test]
    fn a_different_signature_is_not_a_stall() {
        let calls = [
            failing("cargo test", "a"),
            edit("src/a.rs"),
            failing("cargo test", "b"),
        ];
        assert_eq!(stalls(&calls), Vec::new());
        let other_command = [
            failing("cargo test", "a"),
            edit("src/a.rs"),
            failing("cargo test -p a", "a"),
        ];
        assert_eq!(stalls(&other_command), Vec::new());
    }

    #[test]
    fn a_pass_between_failures_resets_the_run() {
        let calls = [
            failing("cargo test", "a"),
            edit("src/a.rs"),
            run("cargo test", 0, Some(0)),
            edit("src/a.rs"),
            failing("cargo test", "a"),
        ];
        assert_eq!(stalls(&calls), Vec::new());
    }

    #[test]
    fn four_changes_to_one_file_while_failing_is_a_stall() {
        let calls = [
            failing("sh check.sh", "a"),
            edit("a.txt"),
            edit("a.txt"),
            edit("b.txt"),
            edit("a.txt"),
            edit("a.txt"),
        ];
        assert_eq!(stall_at(&calls, 4), None);
        assert_eq!(
            stall_at(&calls, 5),
            Some(Stall::SameFile {
                path: "a.txt".to_owned(),
                changes: 4
            })
        );
        assert_eq!(stalls(&calls).len(), 1);
    }

    #[test]
    fn four_changes_without_a_failing_check_is_not_a_stall() {
        let unchecked = [edit("a.txt"), edit("a.txt"), edit("a.txt"), edit("a.txt")];
        assert_eq!(stalls(&unchecked), Vec::new());
        let passing = [
            failing("sh check.sh", "a"),
            edit("a.txt"),
            run("sh check.sh", 0, None),
            edit("a.txt"),
            edit("a.txt"),
            edit("a.txt"),
        ];
        assert_eq!(stalls(&passing), Vec::new());
    }

    #[test]
    fn an_unlisted_command_that_exits_zero_is_checked() {
        let calls = [edit("src/a.rs"), run("python3 script.py", 0, None)];
        assert_eq!(Verification::of(&calls), Verification::Checked);
        assert_eq!(summarize(&calls, false).last_check, None);
    }

    #[test]
    fn an_unlisted_command_that_fails_is_unverified() {
        let calls = [edit("src/a.rs"), run("python3 script.py", 1, None)];
        assert_eq!(Verification::of(&calls), Verification::Unverified);
        let reads = [edit("src/a.rs"), run("cat src/a.rs", 0, None)];
        assert_eq!(Verification::of(&reads), Verification::Unverified);
    }

    #[test]
    fn no_change_is_not_applicable() {
        assert_eq!(Verification::of(&[]), Verification::NotApplicable);
        let calls = [run("cargo test", 1, None)];
        assert_eq!(Verification::of(&calls), Verification::NotApplicable);
        let mut failed = edit("src/a.rs");
        failed.output.is_error = true;
        assert_eq!(Verification::of(&[failed]), Verification::NotApplicable);
    }

    #[test]
    fn a_check_after_the_last_change_decides() {
        let passed = [edit("src/a.rs"), run("cargo test", 0, Some(0))];
        assert_eq!(Verification::of(&passed), Verification::Verified);
        let failed = [edit("src/a.rs"), run("cargo test", 101, Some(1))];
        assert_eq!(Verification::of(&failed), Verification::Failed);
        let fixed = [
            edit("src/a.rs"),
            run("cargo test", 101, Some(1)),
            run("cargo test", 0, Some(0)),
        ];
        assert_eq!(Verification::of(&fixed), Verification::Verified);
        let record = summarize(&fixed, false)
            .last_check
            .expect("a check after the change");
        assert_eq!(record.index, 2);
        assert_eq!(record.command, "cargo test");
        assert!(record.passed);
        let script = [edit("src/a.rs"), run("sh check.sh", 1, None)];
        assert_eq!(Verification::of(&script), Verification::Failed);
        let mut diagnostics = call(
            "syntax_diagnostics",
            &serde_json::json!({"path": "src/a.rs"}),
            ToolOutput::failure("error"),
        );
        let reported = [edit("src/a.rs"), diagnostics.clone()];
        assert_eq!(Verification::of(&reported), Verification::Failed);
        assert_eq!(
            summarize(&reported, false)
                .last_check
                .map(|record| record.command),
            Some("syntax_diagnostics src/a.rs".to_owned())
        );
        diagnostics.output.is_error = false;
        assert_eq!(
            Verification::of(&[edit("src/a.rs"), diagnostics]),
            Verification::Verified
        );
    }

    #[test]
    fn a_failing_report_with_exit_zero_fails() {
        let calls = [edit("src/a.rs"), run("cargo test", 0, Some(1))];
        assert_eq!(Verification::of(&calls), Verification::Failed);
    }

    #[test]
    fn a_check_before_the_last_change_does_not_count() {
        let calls = [
            edit("src/a.rs"),
            run("cargo test", 101, Some(1)),
            edit("src/b.rs"),
        ];
        assert_eq!(Verification::of(&calls), Verification::Unverified);
        let summary = summarize(&calls, false);
        assert_eq!(summary.last_check, None);
        assert_eq!(summary.changed_files, vec!["src/a.rs", "src/b.rs"]);

        // A check still running has no outcome yet.
        let mut running = run("cargo test", 0, None);
        running.output.check = None;
        assert_eq!(
            Verification::of(&[edit("src/a.rs"), running]),
            Verification::Unverified
        );
    }

    #[test]
    fn an_ended_session_is_judged_by_the_command_it_started_with() {
        let mut ended = run("cargo test", 101, Some(1));
        ended.call.arguments =
            serde_json::json!({"action": "interact", "session_id": "s1"}).to_string();
        assert_eq!(
            Verification::of(&[edit("src/a.rs"), ended]),
            Verification::Failed
        );
    }

    #[test]
    fn test_paths_are_recognised() {
        for (path, test) in [
            ("tests/test_parse.py", true),
            ("src/parse.test.ts", true),
            ("src/latest.rs", false),
            ("pkg/parse_test.go", true),
            ("web/__tests__/app.js", true),
            ("spec/models/user_spec.rb", true),
            ("src/app.spec.js", true),
            ("test_util.py", true),
            ("src/contest.rs", false),
            ("crates/a/test/fixture.json", true),
        ] {
            assert_eq!(is_test_path(path), test, "{path}");
        }
        let calls = [edit("tests/test_parse.py")];
        assert!(summarize(&calls, false).tests_modified);
        assert!(!summarize(&[edit("src/latest.rs")], false).tests_modified);
    }

    #[test]
    fn apply_patch_paths_are_read_from_the_envelope() {
        let patch = "*** Begin Patch\n*** Add File: src/new.rs\n+fn a() {}\n\
*** Update File: src/old.rs\n*** Move to: src/moved.rs\n@@\n-a\n+b\n\
*** Delete File: src/gone.rs\n*** End Patch\n";
        let applied = call(
            "apply_patch",
            &serde_json::json!({"input": patch}),
            ToolOutput::success("applied"),
        );
        assert_eq!(
            changed_paths(&applied),
            vec!["src/new.rs", "src/old.rs", "src/moved.rs", "src/gone.rs"]
        );
        let renamed = call(
            "rename_file",
            &serde_json::json!({"path": "a.rs", "new_path": "b.rs"}),
            ToolOutput::success("renamed"),
        );
        assert_eq!(changed_paths(&renamed), vec!["a.rs", "b.rs"]);
    }
}
