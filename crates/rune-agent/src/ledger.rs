//! What the turn loop knows exactly about a conversation's work: the files it
//! changed and read, the last result of each check command and the shell
//! sessions still running.
//!
//! A summary written by a model can drop or misstate these, so they are
//! recorded from the tool results themselves and written out as they are.

use std::fmt::Write as _;

use rune_tools::PlanUpdate;
use rune_tools::update_plan::Status;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::turn::CallResult;

/// Most changed files kept.
const MAX_CHANGED: usize = 30;
/// Most read files kept.
const MAX_READ: usize = 30;
/// Most check commands kept.
const MAX_CHECKS: usize = 20;
/// Most failing test names kept for one check.
const MAX_FAILING: usize = 10;
/// Largest rendering in bytes.
const MAX_RENDERED_BYTES: usize = 4096;
/// Longest rendered line in characters.
const MAX_LINE_CHARS: usize = 200;

/// Files, checks and shells recorded from tool results.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    /// Changed files, most recent first, one entry per path.
    pub changed: Vec<FileChange>,
    /// Paths read and not changed, most recent first.
    pub read: Vec<String>,
    /// The last result of each distinct check command, most recent first.
    pub checks: Vec<CheckResult>,
    /// Background shell sessions still running: (id, command).
    pub sessions: Vec<(String, String)>,
    /// Set when a check ran after the last change; cleared by the next change.
    pub settled: bool,
}

/// One changed file and the lines its changes added and removed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// The path as the model wrote it.
    pub path: String,
    /// Lines added, summed over every change to the path.
    pub added: u64,
    /// Lines removed, summed over every change to the path.
    pub removed: u64,
    /// How the path was last changed.
    pub how: ChangeKind,
}

/// The tool that changed a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// `edit_file`.
    Edit,
    /// `write_file`.
    Write,
    /// `apply_patch`.
    Patch,
    /// `rename_file`.
    Rename,
}

impl ChangeKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Patch => "patch",
            Self::Rename => "rename",
        }
    }
}

/// The last result of one check command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    /// The command as the model wrote it.
    pub command: String,
    /// Exit status, when the command exited with one.
    pub exit: Option<i32>,
    /// Tests that passed, when the output has a test summary.
    pub passed: Option<u64>,
    /// Tests that failed, when the output has a test summary.
    pub failed: Option<u64>,
    /// The first failing test names the output lists.
    pub failing: Vec<String>,
}

impl Ledger {
    /// Records what one resolved tool call did. A call that did not run
    /// records nothing.
    pub fn record(&mut self, result: &CallResult) {
        if !result.executed {
            return;
        }
        let arguments: Value = serde_json::from_str(&result.call.arguments).unwrap_or(Value::Null);
        let text_arg = |name: &str| {
            arguments
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let output = &result.output;
        let succeeded = !output.is_error;
        match result.call.name.as_str() {
            "edit_file" | "write_file" if succeeded => {
                let how = if result.call.name == "write_file" {
                    ChangeKind::Write
                } else {
                    ChangeKind::Edit
                };
                if let Some(path) = text_arg("path") {
                    let (added, removed) = diff_counts(output.diff.as_deref());
                    self.change(path, how, added, removed);
                }
            }
            "apply_patch" if succeeded => {
                let input = text_arg("input").unwrap_or_default();
                for (path, added, removed) in patch_counts(&input, output.diff.as_deref()) {
                    self.change(path, ChangeKind::Patch, added, removed);
                }
            }
            "rename_file" if succeeded => {
                if let Some(new_path) = text_arg("new_path") {
                    if let Some(old_path) = text_arg("path") {
                        self.changed.retain(|entry| entry.path != old_path);
                    }
                    self.change(new_path, ChangeKind::Rename, 0, 0);
                }
            }
            "read_file" if succeeded => {
                if let Some(path) = text_arg("path")
                    && !self.changed.iter().any(|entry| entry.path == path)
                {
                    self.read.retain(|known| *known != path);
                    self.read.insert(0, path);
                    self.read.truncate(MAX_READ);
                }
            }
            "shell" => self.record_shell(&arguments, result),
            _ => {}
        }
    }

    /// Notes a change to `path`, summing it into an earlier entry.
    fn change(&mut self, path: String, how: ChangeKind, added: u64, removed: u64) {
        let (mut added, mut removed) = (added, removed);
        if let Some(index) = self.changed.iter().position(|entry| entry.path == path) {
            let earlier = self.changed.remove(index);
            added = added.saturating_add(earlier.added);
            removed = removed.saturating_add(earlier.removed);
        }
        self.read.retain(|known| *known != path);
        self.changed.insert(
            0,
            FileChange {
                path,
                added,
                removed,
                how,
            },
        );
        self.changed.truncate(MAX_CHANGED);
        self.settled = false;
    }

    fn record_shell(&mut self, arguments: &Value, result: &CallResult) {
        let action = arguments.get("action").and_then(Value::as_str);
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let text = &result.output.text;
        let mentions = session_mentions(text);
        let mut started = false;
        for (id, running) in &mentions {
            if *running {
                if !self.sessions.iter().any(|(known, _)| known == id) {
                    let command = if action == Some("run") { command } else { "" };
                    self.sessions.push((id.clone(), command.to_owned()));
                }
                started |= action == Some("run");
            } else {
                self.sessions.retain(|(known, _)| known != id);
            }
        }
        // A session an interaction or stop did not report as running is gone,
        // whether it ended, was killed by its deadline or was never known.
        if matches!(action, Some("interact" | "stop"))
            && let Some(id) = arguments.get("session_id").and_then(Value::as_str)
            && !mentions
                .iter()
                .any(|(known, running)| known == id && *running)
        {
            self.sessions.retain(|(known, _)| known != id);
        }
        // A command still running has no result yet.
        if started || !crate::turn::is_check_command(&result.call.arguments) {
            return;
        }
        let full = result.output.full_text.as_deref().unwrap_or(text);
        let counts = rune_tools::test_report::parse(full);
        let mut failing = rune_tools::test_report::failures(full);
        failing.truncate(MAX_FAILING);
        self.checks.retain(|check| check.command != command);
        self.checks.insert(
            0,
            CheckResult {
                command: command.to_owned(),
                exit: exit_status(text),
                passed: counts.map(|counts| counts.passed),
                failed: counts.map(|counts| counts.failed),
                failing,
            },
        );
        self.checks.truncate(MAX_CHECKS);
        if !self.changed.is_empty() {
            self.settled = true;
        }
    }

    /// Renders the facts for a checkpoint in at most 4,096 bytes, with the
    /// plan and the pinned constraints.
    ///
    /// When the text is too long the oldest entry of the longest of the read,
    /// changed and check lists is dropped first; plan steps and pinned
    /// constraints go last.
    #[must_use]
    pub fn render(&self, plan: &PlanUpdate, pinned: &[String]) -> String {
        let mut kept = Kept {
            read: self.read.len(),
            changed: self.changed.len(),
            checks: self.checks.len(),
            sessions: 0,
            plan: plan.plan.len(),
            pinned: pinned.len(),
        };
        loop {
            let text = self.compose(plan, pinned, &kept);
            if text.len() <= MAX_RENDERED_BYTES {
                return text;
            }
            let longest = kept.read.max(kept.changed).max(kept.checks);
            if longest > 0 {
                if kept.read == longest {
                    kept.read = kept.read.saturating_sub(1);
                } else if kept.changed == longest {
                    kept.changed = kept.changed.saturating_sub(1);
                } else {
                    kept.checks = kept.checks.saturating_sub(1);
                }
            } else if kept.sessions < self.sessions.len() {
                kept.sessions = kept.sessions.saturating_add(1);
            } else if kept.plan > 0 {
                kept.plan = kept.plan.saturating_sub(1);
            } else if kept.pinned > 0 {
                kept.pinned = kept.pinned.saturating_sub(1);
            } else {
                return text;
            }
        }
    }

    fn compose(&self, plan: &PlanUpdate, pinned: &[String], kept: &Kept) -> String {
        let mut sections: Vec<(&str, Vec<String>)> = Vec::new();
        sections.push((
            "files changed (newest first):",
            self.changed[..kept.changed]
                .iter()
                .map(|entry| {
                    format!(
                        "- {} +{} -{} ({})",
                        entry.path,
                        entry.added,
                        entry.removed,
                        entry.how.label()
                    )
                })
                .collect(),
        ));
        sections.push((
            "files read:",
            self.read[..kept.read]
                .iter()
                .map(|path| format!("- {path}"))
                .collect(),
        ));
        sections.push((
            "checks (newest first):",
            self.checks[..kept.checks].iter().map(check_line).collect(),
        ));
        sections.push((
            "plan:",
            plan.plan[..kept.plan]
                .iter()
                .map(|step| format!("- [{}] {}", status_label(step.status), step.step))
                .collect(),
        ));
        sections.push((
            "pinned constraints:",
            pinned[..kept.pinned]
                .iter()
                .map(|constraint| format!("- {constraint}"))
                .collect(),
        ));
        sections.push((
            "background shells:",
            self.sessions[kept.sessions..]
                .iter()
                .map(|(id, command)| {
                    if command.is_empty() {
                        format!("- session {id}")
                    } else {
                        format!("- session {id}: {command}")
                    }
                })
                .collect(),
        ));
        let mut lines: Vec<String> = Vec::new();
        for (heading, entries) in sections {
            if entries.is_empty() {
                continue;
            }
            lines.push(heading.to_owned());
            lines.extend(entries.into_iter().map(clip));
        }
        lines.join("\n")
    }
}

/// How many entries of each list a rendering keeps: the newest `read`,
/// `changed` and `checks`, every session after the first `sessions`, and the
/// first `plan` steps and `pinned` constraints.
struct Kept {
    read: usize,
    changed: usize,
    checks: usize,
    sessions: usize,
    plan: usize,
    pinned: usize,
}

fn check_line(check: &CheckResult) -> String {
    let mut line = format!("- `{}`", check.command);
    if let Some(exit) = check.exit {
        let _ = write!(line, " exit {exit}");
    }
    if let (Some(passed), Some(failed)) = (check.passed, check.failed) {
        let _ = write!(line, ": {passed} passed, {failed} failed");
    }
    if !check.failing.is_empty() {
        let _ = write!(line, "; failing: {}", check.failing.join(", "));
    }
    line
}

const fn status_label(status: Status) -> &'static str {
    match status {
        Status::Pending => "pending",
        Status::InProgress => "in_progress",
        Status::Completed => "completed",
    }
}

/// Cuts a line to its first 200 characters.
fn clip(line: String) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((at, _)) => line[..at].to_owned(),
        None => line,
    }
}

/// Counts the `+ ` and `- ` lines of a shown diff.
fn diff_counts(diff: Option<&str>) -> (u64, u64) {
    let Some(diff) = diff else {
        return (0, 0);
    };
    diff.lines().fold((0, 0), |(added, removed), line| {
        if line.starts_with("+ ") {
            (added.saturating_add(1), removed)
        } else if line.starts_with("- ") {
            (added, removed.saturating_add(1))
        } else {
            (added, removed)
        }
    })
}

/// The paths a patch changed with the lines added and removed in each.
///
/// The shown diff covers the whole patch, so it gives the counts only when
/// one path changed; for several, each section's own lines are counted.
fn patch_counts(input: &str, diff: Option<&str>) -> Vec<(String, u64, u64)> {
    use rune_tools::apply_patch::{LineKind, Section};
    let Ok(patch) = rune_tools::apply_patch::parse(input) else {
        return Vec::new();
    };
    let mut counts: Vec<(String, u64, u64)> = Vec::new();
    let mut add = |path: &str, added: u64, removed: u64| {
        if let Some(entry) = counts.iter_mut().find(|(known, _, _)| known == path) {
            entry.1 = entry.1.saturating_add(added);
            entry.2 = entry.2.saturating_add(removed);
        } else {
            counts.push((path.to_owned(), added, removed));
        }
    };
    for section in &patch.sections {
        match section {
            Section::Add { path, contents } => {
                add(
                    path,
                    u64::try_from(contents.lines().count()).unwrap_or(0),
                    0,
                );
            }
            Section::Delete { path } => add(path, 0, 0),
            Section::Update {
                path,
                move_to,
                chunks,
            } => {
                let lines = chunks.iter().flat_map(|chunk| chunk.lines.iter());
                let (added, removed) =
                    lines.fold((0u64, 0u64), |(added, removed), (kind, _)| match kind {
                        LineKind::New => (added.saturating_add(1), removed),
                        LineKind::Old => (added, removed.saturating_add(1)),
                        LineKind::Context => (added, removed),
                    });
                match move_to {
                    Some(dest) => {
                        add(path, 0, 0);
                        add(dest, added, removed);
                    }
                    None => add(path, added, removed),
                }
            }
        }
    }
    if let [(_, added, removed)] = counts.as_mut_slice() {
        (*added, *removed) = diff_counts(diff);
    }
    counts
}

/// Sessions a shell result names, in order: true for one still running
/// (`as session {id} (`, `session {id} running`, `session {id} survived`),
/// false for one that ended (`session {id} ended`).
fn session_mentions(text: &str) -> Vec<(String, bool)> {
    const WORD: &str = "session ";
    let mut mentions = Vec::new();
    let mut rest = text;
    while let Some((head, tail)) = rest.split_once(WORD) {
        rest = tail;
        let Some(end) = tail.find(char::is_whitespace) else {
            break;
        };
        let (id, after) = tail.split_at(end);
        let started = head.ends_with("as ") && after.starts_with(" (");
        let running = started || after.starts_with(" running") || after.starts_with(" survived");
        let ended = after.starts_with(" ended");
        if !id.is_empty() && (running || ended) {
            mentions.push((id.to_owned(), running));
        }
    }
    mentions
}

/// The status in the last `exited with status {n}` of a shell result.
fn exit_status(text: &str) -> Option<i32> {
    const PHRASE: &str = "exited with status ";
    let (_, after) = text.rsplit_once(PHRASE)?;
    let digits: String = after
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '-')
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use rune_tools::ToolOutput;
    use rune_tools::update_plan::Step;

    use super::*;
    use crate::turn::PreparedCall;

    fn result(name: &str, arguments: &Value, output: ToolOutput) -> CallResult {
        CallResult {
            call: PreparedCall {
                id: format!("{name}-call"),
                name: name.to_owned(),
                arguments: arguments.to_string(),
            },
            output,
            executed: true,
        }
    }

    fn edit(path: &str, diff: &str) -> CallResult {
        let mut output = ToolOutput::success("Edited");
        output.diff = Some(diff.to_owned());
        result("edit_file", &serde_json::json!({"path": path}), output)
    }

    fn run(command: &str, output: ToolOutput) -> CallResult {
        result(
            "shell",
            &serde_json::json!({"action": "run", "command": command}),
            output,
        )
    }

    #[test]
    fn edits_to_one_file_are_summed_in_one_entry() {
        let mut ledger = Ledger::default();
        ledger.record(&edit("src/lib.rs", "- a\n+ b\n+ c"));
        ledger.record(&edit("src/main.rs", "+ x"));
        ledger.record(&edit("src/lib.rs", "- d"));
        ledger.record(&edit("src/lib.rs", "+ e\n+ f"));
        assert_eq!(
            ledger.changed,
            vec![
                FileChange {
                    path: "src/lib.rs".to_owned(),
                    added: 4,
                    removed: 2,
                    how: ChangeKind::Edit,
                },
                FileChange {
                    path: "src/main.rs".to_owned(),
                    added: 1,
                    removed: 0,
                    how: ChangeKind::Edit,
                },
            ]
        );
    }

    #[test]
    fn a_patch_records_each_path_it_changed() {
        let patch = "*** Begin Patch\n*** Add File: new.txt\n+one\n+two\n\
                     *** Update File: src/lib.rs\n@@\n-old\n+new\n+more\n*** End Patch";
        let mut output = ToolOutput::success("A new.txt (+2)\nM src/lib.rs");
        output.diff = Some("+ one\n+ two\n\n- old\n+ new\n+ more".to_owned());
        let mut ledger = Ledger::default();
        ledger.record(&result(
            "apply_patch",
            &serde_json::json!({"input": patch}),
            output,
        ));
        let counts: Vec<(&str, u64, u64)> = ledger
            .changed
            .iter()
            .map(|entry| (entry.path.as_str(), entry.added, entry.removed))
            .collect();
        assert_eq!(counts, [("src/lib.rs", 2, 1), ("new.txt", 2, 0)]);
        assert!(
            ledger
                .changed
                .iter()
                .all(|entry| entry.how == ChangeKind::Patch)
        );
    }

    #[test]
    fn a_read_file_that_is_later_changed_moves_to_changed() {
        let mut ledger = Ledger::default();
        let read = |path: &str| {
            result(
                "read_file",
                &serde_json::json!({"path": path}),
                ToolOutput::success("1\tfn main() {}"),
            )
        };
        ledger.record(&read("src/lib.rs"));
        ledger.record(&read("src/main.rs"));
        assert_eq!(ledger.read, ["src/main.rs", "src/lib.rs"]);
        ledger.record(&edit("src/lib.rs", "+ x"));
        assert_eq!(ledger.read, ["src/main.rs"]);
        assert_eq!(ledger.changed.len(), 1);
        // Reading a changed file again leaves it under the changes.
        ledger.record(&read("src/lib.rs"));
        assert_eq!(ledger.read, ["src/main.rs"]);
        // A call that did not run records nothing.
        let mut refused = edit("src/other.rs", "+ y");
        refused.executed = false;
        ledger.record(&refused);
        assert_eq!(ledger.changed.len(), 1);
    }

    #[test]
    fn the_last_run_of_a_check_replaces_the_earlier() {
        let mut ledger = Ledger::default();
        ledger.record(&edit("src/lib.rs", "+ x"));
        ledger.record(&run(
            "cargo test",
            ToolOutput::failure(
                "$ cargo test\ntest result: FAILED. 3 passed; 1 failed; 0 ignored\n\
                 [exited with status 101]",
            ),
        ));
        assert!(ledger.settled);
        ledger.record(&run("grep -n foo src/lib.rs", ToolOutput::success("1:foo")));
        ledger.record(&run(
            "cargo test",
            ToolOutput::success(
                "$ cargo test\ntest result: ok. 4 passed; 0 failed; 0 ignored\n\
                 [exited with status 0]",
            ),
        ));
        assert_eq!(
            ledger.checks,
            vec![CheckResult {
                command: "cargo test".to_owned(),
                exit: Some(0),
                passed: Some(4),
                failed: Some(0),
                failing: Vec::new(),
            }]
        );
        ledger.record(&edit("src/lib.rs", "+ y"));
        assert!(!ledger.settled);
    }

    #[test]
    fn failing_test_names_are_kept_up_to_ten() {
        let mut output = String::from("$ cargo test\n");
        for index in 0..12 {
            let _ = writeln!(output, "test tests::case_{index} ... FAILED");
        }
        output.push_str("test result: FAILED. 0 passed; 12 failed; 0 ignored\n");
        output.push_str("[exited with status 101]");
        let mut ledger = Ledger::default();
        ledger.record(&run("cargo test", ToolOutput::failure(output)));
        let check = &ledger.checks[0];
        assert_eq!(check.failed, Some(12));
        assert_eq!(check.failing.len(), 10);
        assert_eq!(check.failing[0], "tests::case_0");
        assert_eq!(check.failing[9], "tests::case_9");
    }

    #[test]
    fn background_sessions_are_tracked_until_they_end() {
        let mut ledger = Ledger::default();
        ledger.record(&run(
            "npm run dev",
            ToolOutput::success(
                "$ npm run dev\n[still running after 10 s as session shell-1-3 (process group \
                 42). Do not run the command again.]",
            ),
        ));
        assert_eq!(
            ledger.sessions,
            [("shell-1-3".to_owned(), "npm run dev".to_owned())]
        );
        // A command still running has no result to record as a check.
        assert!(ledger.checks.is_empty());
        let interact = |text: &str| {
            result(
                "shell",
                &serde_json::json!({"action": "interact", "session_id": "shell-1-3"}),
                ToolOutput::success(text),
            )
        };
        ledger.record(&interact(
            "$ npm run dev\nready\n[session shell-1-3 running, process group 42]",
        ));
        assert_eq!(ledger.sessions.len(), 1);
        ledger.record(&interact(
            "$ npm run dev\n[session shell-1-3 ended: exited with status 0]",
        ));
        assert!(ledger.sessions.is_empty());
    }

    #[test]
    fn the_rendering_has_the_fixed_shape() {
        let ledger = Ledger {
            changed: vec![FileChange {
                path: "src/lib.rs".to_owned(),
                added: 12,
                removed: 3,
                how: ChangeKind::Edit,
            }],
            read: vec!["src/main.rs".to_owned()],
            checks: vec![CheckResult {
                command: "cargo test".to_owned(),
                exit: Some(101),
                passed: Some(14),
                failed: Some(2),
                failing: vec![
                    "parser::tests::empty".to_owned(),
                    "parser::tests::nested".to_owned(),
                ],
            }],
            sessions: vec![("3".to_owned(), "npm run dev".to_owned())],
            settled: true,
        };
        let plan = PlanUpdate {
            version: 2,
            explanation: None,
            plan: vec![
                Step {
                    step: "Read the parser".to_owned(),
                    status: Status::Completed,
                },
                Step {
                    step: "Fix empty input".to_owned(),
                    status: Status::InProgress,
                },
            ],
        };
        assert_eq!(
            ledger.render(&plan, &["Never commit.".to_owned()]),
            "files changed (newest first):\n\
             - src/lib.rs +12 -3 (edit)\n\
             files read:\n\
             - src/main.rs\n\
             checks (newest first):\n\
             - `cargo test` exit 101: 14 passed, 2 failed; failing: parser::tests::empty, \
             parser::tests::nested\n\
             plan:\n\
             - [completed] Read the parser\n\
             - [in_progress] Fix empty input\n\
             pinned constraints:\n\
             - Never commit.\n\
             background shells:\n\
             - session 3: npm run dev"
        );
        let lint = Ledger {
            checks: vec![CheckResult {
                command: "make lint".to_owned(),
                exit: Some(0),
                passed: None,
                failed: None,
                failing: Vec::new(),
            }],
            ..Ledger::default()
        };
        assert_eq!(
            lint.render(&PlanUpdate::default(), &[]),
            "checks (newest first):\n- `make lint` exit 0"
        );
        assert_eq!(Ledger::default().render(&PlanUpdate::default(), &[]), "");
    }

    #[test]
    fn the_rendering_stays_under_four_kibibytes() {
        let mut ledger = Ledger::default();
        let long = "d".repeat(150);
        for index in 0..40 {
            ledger.record(&result(
                "read_file",
                &serde_json::json!({"path": format!("{long}/read_{index}.rs")}),
                ToolOutput::success("text"),
            ));
        }
        for index in 0..40 {
            ledger.record(&edit(&format!("{long}/changed_{index}.rs"), "+ x"));
        }
        assert_eq!(ledger.changed.len(), 30);
        assert_eq!(ledger.changed[0].path, format!("{long}/changed_39.rs"));
        assert_eq!(ledger.read.len(), 30);
        let plan = PlanUpdate {
            version: 1,
            explanation: None,
            plan: vec![Step {
                step: "s".repeat(300),
                status: Status::Pending,
            }],
        };
        let rendered = ledger.render(&plan, &["Never commit.".to_owned()]);
        assert!(rendered.len() <= 4096, "{}", rendered.len());
        // The overflow drops the oldest entries, read files first on a tie.
        let count = |marker: &str| rendered.matches(marker).count();
        assert!(count("/read_") <= count("/changed_"), "{rendered}");
        assert!(rendered.contains("read_39.rs"), "{rendered}");
        assert!(rendered.contains("changed_39.rs"), "{rendered}");
        assert!(!rendered.contains("read_10.rs"), "{rendered}");
        assert!(!rendered.contains("changed_10.rs"), "{rendered}");
        assert!(rendered.contains("- Never commit."), "{rendered}");
        let step = rendered
            .lines()
            .find(|line| line.starts_with("- [pending]"))
            .expect("plan step");
        assert_eq!(step.chars().count(), 200);
        for line in rendered.lines() {
            assert!(line.chars().count() <= 200, "{line}");
        }
    }
}
