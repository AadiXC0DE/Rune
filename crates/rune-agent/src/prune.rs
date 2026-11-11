//! The prune stage: old tool results become one-line receipts.
//!
//! A receipt says what the result was and how to get it back, so the facts
//! stay one call away while their bytes leave the request. A signed history is
//! never edited, because its replay state is valid only after the exact turns
//! that produced it.

use rune_net::message::{ContentPart, Role};
use rune_net::provider::Provider;
use rune_tools::result_store::{Handle, Store};

use crate::history::{History, Turn};
use crate::tokens::estimate_tokens;

/// Newest history kept whole by the prune stage, in estimated tokens.
pub const PROTECTED_TOKENS: u64 = 30_000;
/// Results smaller than this are never pruned.
pub const MIN_PRUNED_BYTES: usize = 1024;

/// Receipt for a successful `read_file` result with numbered lines.
pub const READ_LINES_RECEIPT: &str =
    "[earlier read of {path} lines {a}-{b} removed to save context; read it again if needed]";
/// Receipt for a successful `read_file` result without numbered lines.
pub const READ_RECEIPT: &str =
    "[earlier read of {path} removed to save context; read it again if needed]";
/// Receipt for a `shell` result.
pub const SHELL_RECEIPT: &str = "[output of `{command}` removed to save context: exit {code}{tests}; read_tool_result handle {handle}]";
/// Receipt for any other result.
pub const GENERIC_RECEIPT: &str =
    "[{tool} result removed to save context ({n} bytes); read_tool_result handle {handle}]";
/// The part of a receipt that names the stored copy.
const HANDLE_ACCESS: &str = "read_tool_result handle {handle}";
/// Takes the place of [`HANDLE_ACCESS`] when no stored copy exists.
const NO_HANDLE: &str = "no longer available";
/// Characters of a shell command a receipt keeps.
const MAX_COMMAND_CHARS: usize = 80;
/// Tools whose results from the current request are always kept.
const CURRENT_TURN_TOOLS: [&str; 2] = ["update_plan", "skill"];

/// What one prune pass changed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PruneReport {
    /// Results replaced by receipts.
    pub results: usize,
    /// Bytes of those results before the pass.
    pub bytes_before: usize,
    /// Bytes of their receipts.
    pub bytes_after: usize,
}

/// A result chosen for pruning, with the call that produced it.
struct Candidate {
    id: String,
    tool: String,
    arguments: String,
    content: String,
    is_error: bool,
}

/// Replaces old tool results with receipts. Does nothing on a signed history.
pub fn prune(history: &mut History, dialect: &dyn Provider) -> PruneReport {
    let mut report = PruneReport::default();
    if history.is_signed(dialect) {
        return report;
    }
    for candidate in candidates(history) {
        let receipt = receipt(history, &candidate);
        let bytes_after = receipt.len();
        if history.replace_result(&candidate.id, receipt) {
            report.results = report.results.saturating_add(1);
            report.bytes_before = report.bytes_before.saturating_add(candidate.content.len());
            report.bytes_after = report.bytes_after.saturating_add(bytes_after);
        }
    }
    report
}

/// Collects the results outside every protection rule.
fn candidates(history: &History) -> Vec<Candidate> {
    let turns = history.turns();
    let boundary = protected_boundary(turns);
    let users: Vec<usize> = turns
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, turn)| turn.role == Role::User && !is_harness_text(turn))
        .map(|(index, _)| index)
        .take(2)
        .collect();
    let newest_user = users.first().copied();

    let mut found = Vec::new();
    for (index, turn) in turns.iter().enumerate().take(boundary) {
        if users.contains(&index) {
            continue;
        }
        for part in &turn.parts {
            let ContentPart::ToolResult {
                id,
                name,
                content,
                is_error,
            } = part
            else {
                continue;
            };
            if content.len() < MIN_PRUNED_BYTES || history.is_pruned(id.as_str()) {
                continue;
            }
            let (tool, arguments) = producing_call(turns, index, id.as_str())
                .unwrap_or_else(|| (name.clone(), String::new()));
            if CURRENT_TURN_TOOLS.contains(&tool.as_str())
                && newest_user.is_none_or(|user| index > user)
            {
                continue;
            }
            found.push(Candidate {
                id: id.to_string(),
                tool,
                arguments,
                content: content.clone(),
                is_error: *is_error,
            });
        }
    }
    found
}

/// Returns the index of the oldest protected turn: the turn at which the
/// newest turns reach [`PROTECTED_TOKENS`]. Zero protects everything.
fn protected_boundary(turns: &[Turn]) -> usize {
    let mut sum: u64 = 0;
    for (index, turn) in turns.iter().enumerate().rev() {
        let bytes = u64::try_from(turn.byte_len()).unwrap_or(u64::MAX);
        sum = sum.saturating_add(estimate_tokens(bytes));
        if sum >= PROTECTED_TOKENS {
            return index;
        }
    }
    0
}

/// Returns true for a user turn the harness wrote.
fn is_harness_text(turn: &Turn) -> bool {
    let text = turn.text();
    text.starts_with("<harness_check>") || text.starts_with("<conversation-checkpoint>")
}

/// Finds the name and arguments of the call a result answers, in the nearest
/// assistant turn before it.
fn producing_call(turns: &[Turn], index: usize, id: &str) -> Option<(String, String)> {
    let assistant = turns
        .get(..index)?
        .iter()
        .rev()
        .find(|turn| turn.role == Role::Assistant)?;
    assistant
        .tool_calls()
        .into_iter()
        .find(|(held, _, _)| held.as_str() == id)
        .map(|(_, name, arguments)| (name.to_owned(), arguments.to_owned()))
}

/// Builds the receipt for one result, storing its text when needed.
fn receipt(history: &mut History, candidate: &Candidate) -> String {
    let arguments: serde_json::Value =
        serde_json::from_str(&candidate.arguments).unwrap_or(serde_json::Value::Null);
    let argument = |key: &str| arguments.get(key).and_then(serde_json::Value::as_str);

    if candidate.tool == "read_file"
        && !candidate.is_error
        && let Some(path) = argument("path")
    {
        return match line_range(&candidate.content) {
            Some((first, last)) => fill(
                READ_LINES_RECEIPT,
                &[
                    ("path", path),
                    ("a", &first.to_string()),
                    ("b", &last.to_string()),
                ],
            ),
            None => fill(READ_RECEIPT, &[("path", path)]),
        };
    }

    let handle = stored_handle(history, candidate);
    let access = |template: &str| match &handle {
        Some(_) => template.to_owned(),
        None => template.replace(HANDLE_ACCESS, NO_HANDLE),
    };
    let handle_text = handle
        .as_ref()
        .map(|handle| handle.as_str().to_owned())
        .unwrap_or_default();

    if candidate.tool == "shell"
        && let Some(command) = argument("command")
    {
        let tests = rune_tools::test_report::parse(&candidate.content)
            .map(|counts| {
                format!(
                    ", tests: {} passed, {} failed",
                    counts.passed, counts.failed
                )
            })
            .unwrap_or_default();
        return fill(
            &access(SHELL_RECEIPT),
            &[
                ("command", &short_command(command)),
                ("code", &exit_text(&candidate.content)),
                ("tests", &tests),
                ("handle", &handle_text),
            ],
        );
    }

    fill(
        &access(GENERIC_RECEIPT),
        &[
            ("tool", &candidate.tool),
            ("n", &candidate.content.len().to_string()),
            ("handle", &handle_text),
        ],
    )
}

/// Returns a readable handle for the result's full text: one the content
/// already names, else a new stored copy.
fn stored_handle(history: &mut History, candidate: &Candidate) -> Option<Handle> {
    if let Some(handle) = named_handle(&candidate.content, history.result_store()) {
        return Some(handle);
    }
    history
        .result_store_mut()
        .spill(
            &candidate.tool,
            &candidate.id,
            candidate.content.clone(),
            candidate.is_error,
            0,
        )
        .ok()
        .map(|preview| preview.handle)
}

/// Finds a `handle result-` marker in the content whose result is still stored.
fn named_handle(content: &str, store: &Store) -> Option<Handle> {
    content
        .rmatch_indices("handle result-")
        .find_map(|(at, _)| {
            let start = at.saturating_add("handle ".len());
            let raw = content.get(start..start.saturating_add(31))?;
            let digits = raw.strip_prefix("result-")?;
            if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            let handle = Handle::parse(raw).ok()?;
            store.is_available(&handle).then_some(handle)
        })
}

/// Returns the first and last line numbers of a numbered read.
fn line_range(content: &str) -> Option<(u64, u64)> {
    let mut numbers = content.lines().filter_map(line_number);
    let first = numbers.next()?;
    let last = numbers.next_back().unwrap_or(first);
    Some((first, last))
}

/// Reads the number of a line shaped like `   12\ttext`.
fn line_number(line: &str) -> Option<u64> {
    let (head, _) = line.split_once('\t')?;
    let digits = head.trim_start();
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Puts a command on one line and cuts it to [`MAX_COMMAND_CHARS`].
fn short_command(command: &str) -> String {
    let line = command.replace('\n', " ");
    if line.chars().count() <= MAX_COMMAND_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(MAX_COMMAND_CHARS).collect();
    cut.push_str("...");
    cut
}

/// States how a shell command ended, from its result text.
fn exit_text(content: &str) -> String {
    if let Some(code) = number_after(content, "exited with status ") {
        return code;
    }
    if let Some(signal) = number_after(content, "killed by signal ") {
        return format!("signal {signal}");
    }
    let running = content.split("session ").skip(1).any(|rest| {
        let id_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        rest.get(id_end..)
            .is_some_and(|tail| id_end > 0 && tail.starts_with(" running"))
    });
    if running {
        return "still running".to_owned();
    }
    "unknown".to_owned()
}

/// Returns the integer that follows the last occurrence of `phrase`.
fn number_after(content: &str, phrase: &str) -> Option<String> {
    let (_, rest) = content.rsplit_once(phrase)?;
    let sign = usize::from(rest.starts_with('-'));
    let digits = rest
        .get(sign..)?
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    if digits == 0 {
        return None;
    }
    rest.get(..sign.saturating_add(digits)).map(str::to_owned)
}

/// Fills `{name}` placeholders in one pass, so a value is never read as a
/// placeholder itself.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some((before, after)) = rest.split_once('{') {
        out.push_str(before);
        let value = after.split_once('}').and_then(|(name, tail)| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value, tail))
        });
        if let Some((value, tail)) = value {
            out.push_str(value);
            rest = tail;
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// The receipt that replaces a skill body once its turn is over.
pub const SKILL_RECEIPT: &str = "[skill {name} was loaded here and removed to save context; call the skill tool again if it is needed]";

/// Skill results at or under this many bytes stay where they are.
pub const MAX_KEPT_SKILL_BYTES: usize = 2048;

/// Replaces every large `skill` result from before the newest user turn with
/// a receipt, and returns how many it replaced. A signed history is left as
/// it is; its next checkpoint drops the bodies.
pub fn unload_skills(history: &mut History, dialect: &dyn Provider) -> usize {
    if history.is_signed(dialect) {
        return 0;
    }
    let turns = history.turns();
    let Some(newest_user) = turns
        .iter()
        .rposition(|turn| turn.role == Role::User && !is_harness_text(turn))
    else {
        return 0;
    };
    let mut replacements = Vec::new();
    for (index, turn) in turns.iter().enumerate().take(newest_user) {
        for part in &turn.parts {
            let ContentPart::ToolResult {
                id, name, content, ..
            } = part
            else {
                continue;
            };
            if name != "skill"
                || content.len() <= MAX_KEPT_SKILL_BYTES
                || history.is_pruned(id.as_str())
            {
                continue;
            }
            let arguments =
                producing_call(turns, index, id.as_str()).map(|(_, arguments)| arguments);
            let receipt = fill(
                SKILL_RECEIPT,
                &[("name", &skill_name(content, arguments.as_deref()))],
            );
            replacements.push((id.as_str().to_owned(), receipt));
        }
    }
    replacements
        .into_iter()
        .filter(|(id, receipt)| history.replace_result(id, receipt.clone()))
        .count()
}

/// The skill a result loaded: the `name` of a leading `<skill_content>`, else
/// the last component of the call's `location` without `/SKILL.md`.
pub fn skill_name(content: &str, arguments: Option<&str>) -> String {
    if let Some(rest) = content.strip_prefix("<skill_content name=\"")
        && let Some((name, _)) = rest.split_once('"')
        && !name.is_empty()
    {
        return name.to_owned();
    }
    arguments
        .and_then(|arguments| serde_json::from_str::<serde_json::Value>(arguments).ok())
        .and_then(|value| value.get("location")?.as_str().map(str::to_owned))
        .map(|location| {
            let trimmed = location.trim_end_matches('/');
            let trimmed = trimmed.strip_suffix("/SKILL.md").unwrap_or(trimmed);
            trimmed.rsplit('/').next().unwrap_or(trimmed).to_owned()
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "skill".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::id::ToolCallId;
    use rune_net::chat_completions::ChatCompletions;
    use rune_net::provider::RequestPlan;
    use rune_net::stream::StreamReducer;

    /// A dialect whose every replay is signed.
    struct Signed;

    impl Provider for Signed {
        fn name(&self) -> &'static str {
            "signed"
        }

        fn build_request(
            &self,
            _plan: &RequestPlan,
        ) -> rune_core::error::Result<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }

        fn request_path(&self) -> &'static str {
            "/"
        }

        fn reducer(&self) -> Box<dyn StreamReducer> {
            ChatCompletions.reducer()
        }

        fn replay_is_signed(&self, _replay: &str) -> bool {
            true
        }
    }

    fn call(id: &str, name: &str, arguments: &str) -> ContentPart {
        ContentPart::ToolCall {
            id: ToolCallId::new(id).expect("id"),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        }
    }

    fn result(id: &str, name: &str, content: &str, is_error: bool) -> ContentPart {
        ContentPart::ToolResult {
            id: ToolCallId::new(id).expect("id"),
            name: name.to_owned(),
            content: content.to_owned(),
            is_error,
        }
    }

    /// Appends one call and its result.
    fn exchange(history: &mut History, id: &str, name: &str, arguments: &str, content: &str) {
        history.push_assistant(vec![call(id, name, arguments)]);
        history.push_tool_results(vec![result(id, name, content, false)]);
    }

    /// Appends an assistant text of `bytes` bytes.
    fn text_turn(history: &mut History, bytes: usize) {
        history.push_assistant(vec![ContentPart::Text {
            text: "f".repeat(bytes),
        }]);
    }

    /// Appends enough newest history to protect nothing older.
    fn filler(history: &mut History) {
        text_turn(history, 120_000);
    }

    fn content_of(history: &History, id: &str) -> String {
        history
            .turns()
            .iter()
            .flat_map(|turn| &turn.parts)
            .find_map(|part| match part {
                ContentPart::ToolResult {
                    id: held, content, ..
                } if held.as_str() == id => Some(content.clone()),
                _ => None,
            })
            .expect("result")
    }

    fn numbered(lines: u64) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for line in 1..=lines {
            let _ = writeln!(out, "{line:>6}\t{}", "y".repeat(40));
        }
        out
    }

    fn handle_in(receipt: &str) -> Handle {
        let raw = receipt
            .split("handle ")
            .nth(1)
            .and_then(|rest| rest.get(..31))
            .expect("handle");
        Handle::parse(raw).expect("parse")
    }

    #[test]
    fn old_results_become_receipts_and_new_ones_stay() {
        let mut history = History::new();
        history.push_user("run the checks");
        let body = format!("{}\n[exited with status 0]\n", "z".repeat(8 * 1024));
        for index in 0..10 {
            exchange(
                &mut history,
                &format!("call-{index}"),
                "shell",
                r#"{"action":"run","command":"make check"}"#,
                &body,
            );
        }
        text_turn(&mut history, 64 * 1024);
        let boundary = protected_boundary(history.turns());
        let protected_tokens = history.turns()[boundary..]
            .iter()
            .map(|turn| estimate_tokens(u64::try_from(turn.byte_len()).expect("bytes")))
            .fold(0_u64, u64::saturating_add);
        assert!(protected_tokens >= PROTECTED_TOKENS);

        let report = prune(&mut history, &ChatCompletions);
        assert!(report.results > 0);
        assert!(report.results < 10);
        assert_eq!(
            report.bytes_before,
            body.len().saturating_mul(report.results)
        );
        assert!(report.bytes_after < report.bytes_before);
        for (index, turn) in history.turns().iter().enumerate() {
            for part in &turn.parts {
                if let ContentPart::ToolResult { id, content, .. } = part {
                    if index < boundary {
                        assert!(history.is_pruned(id.as_str()), "{id}");
                        assert!(content.starts_with("[output of `make check` removed"));
                    } else {
                        assert!(!history.is_pruned(id.as_str()), "{id}");
                        assert_eq!(content, &body);
                    }
                }
            }
        }
        assert_eq!(history.anchor(), None);
    }

    #[test]
    fn a_read_receipt_names_the_path_and_lines() {
        let mut history = History::new();
        history.push_user("read it");
        exchange(
            &mut history,
            "read",
            "read_file",
            r#"{"path":"src/lib.rs"}"#,
            &numbered(120),
        );
        filler(&mut history);
        let report = prune(&mut history, &ChatCompletions);
        assert_eq!(report.results, 1);
        assert_eq!(
            content_of(&history, "read"),
            "[earlier read of src/lib.rs lines 1-120 removed to save context; read it again if needed]"
        );
        assert!(history.result_store().is_empty());
    }

    #[test]
    fn an_empty_read_uses_the_short_form() {
        let mut history = History::new();
        let candidate = Candidate {
            id: "read".to_owned(),
            tool: "read_file".to_owned(),
            arguments: r#"{"path":"empty.txt"}"#.to_owned(),
            content: String::new(),
            is_error: false,
        };
        assert_eq!(
            receipt(&mut history, &candidate),
            "[earlier read of empty.txt removed to save context; read it again if needed]"
        );
    }

    #[test]
    fn a_shell_receipt_names_the_command_exit_and_counts() {
        let mut history = History::new();
        history.push_user("test it");
        let content = format!(
            "$ cargo test\n{}\ntest result: FAILED. 12 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out\n[exited with status 101]\n",
            "running 14 tests\n".repeat(80)
        );
        exchange(
            &mut history,
            "test",
            "shell",
            r#"{"action":"run","command":"cargo test"}"#,
            &content,
        );
        filler(&mut history);
        prune(&mut history, &ChatCompletions);
        let receipt = content_of(&history, "test");
        let handle = handle_in(&receipt);
        assert_eq!(
            receipt,
            format!(
                "[output of `cargo test` removed to save context: exit 101, tests: 12 passed, 2 failed; read_tool_result handle {handle}]"
            )
        );
    }

    #[test]
    fn a_long_shell_command_is_cut_to_one_line() {
        let command = format!("{}\n{}", "a".repeat(100), "b".repeat(99));
        assert_eq!(command.chars().count(), 200);
        let short = short_command(&command);
        assert_eq!(short, format!("{}...", "a".repeat(80)));
        assert!(!short.contains('\n'));
        assert_eq!(short_command("echo a\necho b"), "echo a echo b");
    }

    #[test]
    fn a_shell_exit_is_read_from_the_result() {
        assert_eq!(exit_text("[exited with status 2]"), "2");
        assert_eq!(exit_text("[killed by signal 9]"), "signal 9");
        assert_eq!(
            exit_text("[session 4 running, process group 77]"),
            "still running"
        );
        assert_eq!(exit_text("no status here"), "unknown");
    }

    #[test]
    fn a_pruned_result_reads_back_whole() {
        let mut history = History::new();
        history.push_user("look");
        let original = "line of output\n".repeat(200);
        exchange(
            &mut history,
            "grep",
            "grep_files",
            r#"{"pattern":"x"}"#,
            &original,
        );
        filler(&mut history);
        prune(&mut history, &ChatCompletions);
        let receipt = content_of(&history, "grep");
        assert_eq!(
            receipt,
            format!(
                "[grep_files result removed to save context ({} bytes); read_tool_result handle {}]",
                original.len(),
                handle_in(&receipt)
            )
        );
        let read = history
            .result_store()
            .read(&handle_in(&receipt), 0, 65536)
            .expect("read");
        assert_eq!(read.text, original);
    }

    #[test]
    fn a_spilled_preview_keeps_its_handle() {
        let mut history = History::new();
        history.push_user("look");
        let full = "w".repeat(40_000);
        let preview = history
            .result_store_mut()
            .spill("shell", "big", full, false, 2048)
            .expect("spill");
        let shown = preview.render();
        exchange(
            &mut history,
            "big",
            "shell",
            r#"{"action":"run","command":"cat big"}"#,
            &shown,
        );
        filler(&mut history);
        prune(&mut history, &ChatCompletions);
        assert_eq!(history.result_store().len(), 1);
        assert_eq!(handle_in(&content_of(&history, "big")), preview.handle);
    }

    #[test]
    fn a_signed_history_is_never_pruned() {
        let mut history = History::new();
        history.push_user("look");
        history.push_assistant_with_replay(
            vec![call("old", "grep_files", "{}")],
            Some("[]".to_owned()),
            None,
        );
        history.push_tool_results(vec![result("old", "grep_files", &"q".repeat(4096), false)]);
        filler(&mut history);
        let before = history.turns().to_vec();
        assert!(history.is_signed(&Signed));
        assert!(!history.is_signed(&ChatCompletions));
        assert_eq!(prune(&mut history, &Signed), PruneReport::default());
        assert_eq!(history.turns(), before.as_slice());
        assert!(!history.is_pruned("old"));
    }

    #[test]
    fn results_of_the_plan_and_skill_tools_in_the_current_turn_are_kept() {
        let mut history = History::new();
        history.push_user("first request");
        let large = "p".repeat(2048);
        exchange(&mut history, "old-plan", "update_plan", "{}", &large);
        history.push_assistant(vec![ContentPart::Text {
            text: "done".to_owned(),
        }]);
        history.push_user("second request");
        exchange(&mut history, "plan", "update_plan", "{}", &large);
        exchange(&mut history, "skill", "skill", r#"{"name":"x"}"#, &large);
        exchange(&mut history, "other", "grep_files", "{}", &large);
        filler(&mut history);
        let report = prune(&mut history, &ChatCompletions);
        assert_eq!(report.results, 2);
        assert!(history.is_pruned("old-plan"));
        assert!(history.is_pruned("other"));
        assert_eq!(content_of(&history, "plan"), large);
        assert_eq!(content_of(&history, "skill"), large);
    }

    #[test]
    fn the_size_threshold_is_one_kibibyte() {
        let mut history = History::new();
        history.push_user("look");
        exchange(&mut history, "small", "grep_files", "{}", &"s".repeat(1023));
        exchange(&mut history, "large", "grep_files", "{}", &"l".repeat(1024));
        filler(&mut history);
        let report = prune(&mut history, &ChatCompletions);
        assert_eq!(report.results, 1);
        assert!(!history.is_pruned("small"));
        assert!(history.is_pruned("large"));
    }

    #[test]
    fn pruning_twice_leaves_receipts_unchanged() {
        let mut history = History::new();
        history.push_user("look");
        exchange(&mut history, "one", "grep_files", "{}", &"o".repeat(4096));
        filler(&mut history);
        assert_eq!(prune(&mut history, &ChatCompletions).results, 1);
        let receipt = content_of(&history, "one");
        assert_eq!(
            prune(&mut history, &ChatCompletions),
            PruneReport::default()
        );
        assert_eq!(content_of(&history, "one"), receipt);
        assert_eq!(history.result_store().len(), 1);
    }

    #[test]
    fn a_short_history_is_protected_whole() {
        let mut history = History::new();
        history.push_user("look");
        exchange(&mut history, "one", "grep_files", "{}", &"o".repeat(4096));
        assert_eq!(
            prune(&mut history, &ChatCompletions),
            PruneReport::default()
        );
    }

    #[test]
    fn a_skill_body_from_an_earlier_turn_becomes_a_receipt() {
        let mut history = History::new();
        history.push_user("use the alpha skill");
        let body = format!(
            "<skill_content name=\"alpha\">\n{}\n</skill_content>\n",
            "a".repeat(4096)
        );
        exchange(
            &mut history,
            "load",
            "skill",
            r#"{"location":"skills/alpha/SKILL.md"}"#,
            &body,
        );
        history.push_assistant(vec![ContentPart::Text {
            text: "done".to_owned(),
        }]);
        history.push_user("next task");
        assert_eq!(unload_skills(&mut history, &ChatCompletions), 1);
        assert_eq!(
            content_of(&history, "load"),
            "[skill alpha was loaded here and removed to save context; call the skill tool again if it is needed]"
        );
        assert!(history.is_pruned("load"));
        assert_eq!(unload_skills(&mut history, &ChatCompletions), 0);
        assert_eq!(
            skill_name("plain body", Some(r#"{"location":"skills/beta/SKILL.md"}"#)),
            "beta"
        );
    }

    #[test]
    fn a_skill_body_in_the_current_turn_stays() {
        let mut history = History::new();
        history.push_user("use the alpha skill");
        let body = "b".repeat(4096);
        exchange(&mut history, "load", "skill", "{}", &body);
        exchange(&mut history, "small", "skill", "{}", &"s".repeat(1500));
        assert_eq!(unload_skills(&mut history, &ChatCompletions), 0);
        assert_eq!(content_of(&history, "load"), body);
        history.push_user("next task");
        assert_eq!(unload_skills(&mut history, &ChatCompletions), 1);
        assert_eq!(content_of(&history, "small"), "s".repeat(1500));
    }

    #[test]
    fn a_signed_history_keeps_skill_bodies() {
        let mut history = History::new();
        history.push_user("use the alpha skill");
        history.push_assistant_with_replay(
            vec![call("load", "skill", "{}")],
            Some("[]".to_owned()),
            None,
        );
        let body = "c".repeat(4096);
        history.push_tool_results(vec![result("load", "skill", &body, false)]);
        history.push_user("next task");
        assert_eq!(unload_skills(&mut history, &Signed), 0);
        assert_eq!(content_of(&history, "load"), body);
    }
}
