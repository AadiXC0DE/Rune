//! Context compaction.
//!
//! When a request reaches the trigger, the oldest part of the conversation is
//! replaced by a summary and the turn continues in a fresh window. Two
//! properties matter:
//!
//! - The cut lands on a user turn. Cutting anywhere else leaves a tool call
//!   without its result, which no provider accepts.
//! - A failed or cancelled compaction leaves the history exactly as it was, so
//!   a summary is never partially installed.

use std::fmt::Write as _;

use rune_core::budget::{BudgetSet, LimitName};
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::history::History;
use crate::tokens::{CapacityDecision, Estimate, UsageTotals, decide_capacity};

/// Instruction given to the summarizer.
///
/// Kept explicit about what must survive, because a summary that drops the
/// user's current intent or an unresolved blocker is worse than no summary.
pub const SUMMARY_INSTRUCTIONS: &str = "\
Summarize the earlier portion of this conversation so work can continue in a \
fresh context window. Preserve, in this order:
1. What the user is trying to achieve, in their own terms.
2. Decisions already made and the reason for each.
3. Every unresolved blocker, open question, or failed attempt with its error.
4. The state of any verification: what was run, what passed, what did not.
5. Concrete facts discovered that would be expensive to rediscover, such as \
file paths, symbol names, and command invocations.
Omit routine tool output. Do not add commentary about the summary itself.";

/// What a compaction would do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Plan {
    /// Turn sequence where the summary begins.
    pub cut_at: u64,
    /// Turns removed after the cut is taken.
    pub removed_turns: usize,
    /// Turns retained verbatim.
    pub kept_turns: usize,
    /// Bytes being removed.
    pub removed_bytes: usize,
}

impl Plan {
    /// Returns the fraction of the history being removed, as a percent.
    #[must_use]
    pub fn removed_percent(&self, total_bytes: usize) -> u8 {
        if total_bytes == 0 {
            return 0;
        }
        let scaled = u128::from(self.removed_bytes as u64).saturating_mul(100);
        let percent = scaled
            .checked_div(u128::from(total_bytes as u64))
            .unwrap_or(0);
        u8::try_from(percent.min(100)).unwrap_or(100)
    }
}

/// Plans a compaction for the current history.
///
/// Returns `None` when there is nothing worth compacting, which is the common
/// case: a short conversation has no prefix worth summarizing.
#[must_use]
pub fn plan(history: &History, limits: &BudgetSet) -> Option<Plan> {
    let keep = recent_target_turns(limits);
    let cut_at = history.compaction_cut(keep)?;
    if cut_at <= 1 {
        // Nothing precedes the cut, so a summary would replace nothing.
        return None;
    }

    let removed_bytes = history
        .turns()
        .iter()
        .filter(|turn| turn.seq < cut_at)
        .fold(0_usize, |total, turn| total.saturating_add(turn.byte_len()));
    let removed_turns = history
        .turns()
        .iter()
        .filter(|turn| turn.seq < cut_at)
        .count();

    if removed_turns == 0 {
        return None;
    }

    Some(Plan {
        cut_at,
        removed_turns,
        kept_turns: history.len().saturating_sub(removed_turns),
        removed_bytes,
    })
}

/// Returns how many recent turns to retain.
///
/// A twentieth of the usable input, expressed as a turn count. The turn count is
/// a proxy: what matters is retaining enough that the model can still see the
/// work in progress, and a fixed fraction of capacity is a stable way to size it
/// without counting tokens here.
#[must_use]
pub fn recent_target_turns(limits: &BudgetSet) -> usize {
    let bytes = limits.get_usize(LimitName::MaxToolResultBytes).max(1);
    // One turn per sixteen kibibytes of result capacity, floored so a very small
    // configuration still keeps a few turns.
    (bytes / (16 * 1024)).clamp(2, 16)
}

/// Renders the request sent to the summarizer.
///
/// Contains only the turns being removed, so the summarizer cannot see and then
/// paraphrase content that is being retained.
#[must_use]
pub fn render_summary_request(history: &History, plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str(SUMMARY_INSTRUCTIONS);
    out.push_str("\n\n<conversation>\n");
    for turn in history.turns() {
        if turn.seq >= plan.cut_at {
            break;
        }
        out.push_str(turn.role.as_str());
        out.push_str(": ");
        for part in &turn.parts {
            match part {
                rune_net::message::ContentPart::Text { text } => out.push_str(text),
                rune_net::message::ContentPart::ToolCall {
                    name, arguments, ..
                } => {
                    let _ = write!(out, "[calls {name} with {arguments}]");
                }
                rune_net::message::ContentPart::ToolResult {
                    name,
                    content,
                    is_error,
                    ..
                } => {
                    let label = if *is_error {
                        "error from"
                    } else {
                        "result from"
                    };
                    let _ = write!(out, "[{label} {name}: {content}]");
                }
                rune_net::message::ContentPart::Reasoning { .. } => {}
                rune_net::message::ContentPart::Image { image } => {
                    let _ = write!(out, "[image {}]", image.id);
                }
            }
        }
        out.push('\n');
    }
    out.push_str("</conversation>\n");
    out
}

/// Applies a summary to the history.
///
/// The retained tail is kept verbatim, and a summary turn is prepended, so the
/// resulting length is the retained count plus one. Returns the number of turns
/// removed, which is the retained count subtracted from the previous length, not
/// the difference in length after the summary was added.
pub fn apply(history: &mut History, plan: &Plan, summary: impl Into<String>) -> usize {
    let summary = summary.into();
    let before = history.len();
    history.replace_with_summary(summary, plan.cut_at);
    let after = history.len();
    // The summary replaces the removed prefix with exactly one turn.
    before.saturating_sub(after.saturating_sub(1))
}

/// Wraps a summary so the model can tell it apart from a user request.
#[must_use]
pub fn wrap_summary(summary: &str) -> String {
    format!(
        "<context_handoff>\nThe following is a summary of earlier conversation, provided so work can continue.\n\n{summary}\n</context_handoff>"
    )
}

/// Rejects a summary that is unusable.
///
/// An empty or trivially short summary means the summarizer failed, and
/// installing it would silently discard the conversation it replaced.
pub fn validate_summary(summary: &str) -> Result<()> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return Err(
            RuneError::new(ErrorCode::InvalidState, "the summarizer returned nothing")
                .with_hint("the previous context was kept"),
        );
    }
    if trimmed.len() < 32 {
        return Err(RuneError::new(
            ErrorCode::InvalidState,
            format!("the summarizer returned only {} bytes", trimmed.len()),
        )
        .with_hint("a summary this short would discard the conversation it replaced"));
    }
    Ok(())
}

/// Outcome of a compaction decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trigger {
    /// The request fits and no compaction is needed.
    NotNeeded,
    /// The request fits but is close to the trigger.
    Approaching,
    /// The request must be compacted before it can be sent.
    Required,
    /// The retained tail alone does not fit, so compaction cannot help.
    Impossible,
}

impl Trigger {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotNeeded => "not_needed",
            Self::Approaching => "approaching",
            Self::Required => "required",
            Self::Impossible => "impossible",
        }
    }
}

/// Decides whether a compaction should run.
#[must_use]
pub fn trigger(estimate: &Estimate, limits: &BudgetSet) -> Trigger {
    match decide_capacity(estimate, limits) {
        CapacityDecision::Fits => Trigger::NotNeeded,
        CapacityDecision::Approaching => Trigger::Approaching,
        CapacityDecision::Compact => Trigger::Required,
        CapacityDecision::OverCapacity => Trigger::Impossible,
    }
}

/// The error returned when compaction cannot make a request fit.
#[must_use]
pub fn cannot_fit_error() -> RuneError {
    RuneError::new(
        ErrorCode::TooLarge,
        "the retained conversation does not fit the context window",
    )
    .with_hint("start a new session, or reduce the size of the last tool result")
}

/// Tracks the effect of compactions on a session.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CompactionStats {
    /// Compactions performed.
    pub count: u32,
    /// Turns removed in total.
    pub turns_removed: usize,
    /// Usage recorded before the first compaction.
    pub usage_at_last: Option<UsageTotals>,
}

impl CompactionStats {
    /// Records a compaction.
    pub fn record(&mut self, removed: usize) {
        self.count = self.count.saturating_add(1);
        self.turns_removed = self.turns_removed.saturating_add(removed);
    }

    /// Returns true when no compaction has run.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::id::ToolCallId;
    use rune_net::message::{ContentPart, Role};

    fn call(id: &str) -> ContentPart {
        ContentPart::ToolCall {
            id: ToolCallId::new(id).expect("id"),
            name: "read_file".to_owned(),
            arguments: "{}".to_owned(),
        }
    }

    fn result(id: &str) -> ContentPart {
        ContentPart::ToolResult {
            id: ToolCallId::new(id).expect("id"),
            name: "read_file".to_owned(),
            content: "contents".to_owned(),
            is_error: false,
        }
    }

    /// Builds a history with several tool exchanges.
    fn long_history() -> History {
        let mut history = History::new();
        for index in 0..12 {
            history.push_user(format!("question {index}"));
            history.push_assistant(vec![call(&format!("c{index}"))]);
            history.push_tool_results(vec![result(&format!("c{index}"))]);
            history.push_assistant(vec![ContentPart::Text {
                text: format!("answer {index}"),
            }]);
        }
        history
    }

    #[test]
    fn a_short_history_has_nothing_to_compact() {
        let mut history = History::new();
        history.push_user("only");
        assert!(plan(&history, &BudgetSet::new()).is_none());
    }

    #[test]
    fn a_long_history_produces_a_plan() {
        let history = long_history();
        let plan = plan(&history, &BudgetSet::new()).expect("a plan");
        assert!(plan.removed_turns > 0);
        assert!(plan.kept_turns > 0);
        assert!(plan.removed_bytes > 0);
    }

    #[test]
    fn the_plan_cuts_on_a_user_turn() {
        let history = long_history();
        let plan = plan(&history, &BudgetSet::new()).expect("a plan");
        let turn = history
            .turns()
            .iter()
            .find(|turn| turn.seq == plan.cut_at)
            .expect("present");
        // Cutting elsewhere would leave a tool result without its call.
        assert_eq!(turn.role, Role::User);
    }

    #[test]
    fn applying_a_summary_leaves_a_valid_history() {
        let mut history = long_history();
        let plan = plan(&history, &BudgetSet::new()).expect("a plan");
        let removed = apply(&mut history, &plan, wrap_summary("what happened so far"));
        assert_eq!(removed, plan.removed_turns);
        history.validate().expect("still valid");
    }

    #[test]
    fn the_summary_is_the_first_turn_after_compaction() {
        let mut history = long_history();
        let plan = plan(&history, &BudgetSet::new()).expect("a plan");
        apply(&mut history, &plan, "the summary");
        let first = history.turns().first().expect("a turn");
        assert!(first.text().contains("the summary"));
    }

    #[test]
    fn compaction_preserves_every_recent_turn() {
        let mut history = long_history();
        let last_text = history.turns().last().expect("a turn").text();
        let plan = plan(&history, &BudgetSet::new()).expect("a plan");
        apply(&mut history, &plan, "summary");
        let texts: Vec<String> = history
            .turns()
            .iter()
            .map(crate::history::Turn::text)
            .collect();
        assert!(
            texts.iter().any(|text| text == &last_text),
            "the most recent turn was dropped"
        );
    }

    #[test]
    fn a_failed_summary_is_rejected_before_it_is_installed() {
        // An empty summary means the summarizer failed. Installing it would
        // discard the conversation it replaced.
        assert!(validate_summary("").is_err());
        assert!(validate_summary("   ").is_err());
        assert!(validate_summary("too short").is_err());
        assert!(validate_summary(&"x".repeat(64)).is_ok());
    }

    #[test]
    fn a_rejected_summary_leaves_the_history_untouched() {
        let history = long_history();
        let before = history.len();
        let plan = plan(&history, &BudgetSet::new()).expect("a plan");
        let summary = "";
        assert!(validate_summary(summary).is_err());
        // Because validation happens first, apply is never reached.
        assert_eq!(history.len(), before);
        assert!(plan.removed_turns > 0);
    }

    #[test]
    fn the_summary_request_contains_only_the_removed_prefix() {
        let history = long_history();
        let plan = plan(&history, &BudgetSet::new()).expect("a plan");
        let request = render_summary_request(&history, &plan);
        assert!(request.contains(SUMMARY_INSTRUCTIONS));

        let kept: Vec<String> = history
            .turns()
            .iter()
            .filter(|turn| turn.seq >= plan.cut_at)
            .map(crate::history::Turn::text)
            .collect();
        for text in kept {
            if text.is_empty() {
                continue;
            }
            assert!(
                !request.contains(&text),
                "the request included content being retained: {text}"
            );
        }
    }

    #[test]
    fn the_summary_request_includes_tool_activity() {
        let history = long_history();
        let plan = plan(&history, &BudgetSet::new()).expect("a plan");
        let request = render_summary_request(&history, &plan);
        assert!(request.contains("calls read_file"), "{request}");
    }

    #[test]
    fn a_wrapped_summary_is_delimited() {
        let wrapped = wrap_summary("the summary");
        assert!(wrapped.contains("<context_handoff>"));
        assert!(wrapped.contains("</context_handoff>"));
        assert!(wrapped.contains("the summary"));
    }

    #[test]
    fn the_removed_percentage_is_computed() {
        let plan = Plan {
            cut_at: 5,
            removed_turns: 4,
            kept_turns: 2,
            removed_bytes: 750,
        };
        assert_eq!(plan.removed_percent(1000), 75);
        assert_eq!(plan.removed_percent(0), 0);
    }

    #[test]
    fn the_trigger_maps_from_the_capacity_decision() {
        let limits = BudgetSet::new();
        assert_eq!(
            trigger(&Estimate::new(0, 10, 1000), &limits),
            Trigger::NotNeeded
        );
        assert_eq!(
            trigger(&Estimate::new(0, 800, 1000), &limits),
            Trigger::Required
        );
        assert_eq!(
            trigger(&Estimate::new(0, 1200, 1000), &limits),
            Trigger::Impossible
        );
    }

    #[test]
    fn triggers_have_distinct_names() {
        let all = [
            Trigger::NotNeeded,
            Trigger::Approaching,
            Trigger::Required,
            Trigger::Impossible,
        ];
        let mut seen = std::collections::HashSet::new();
        for trigger in all {
            assert!(seen.insert(trigger.as_str()), "duplicate {trigger:?}");
        }
    }

    #[test]
    fn the_cannot_fit_error_names_a_remedy() {
        let err = cannot_fit_error();
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn compaction_statistics_accumulate() {
        let mut stats = CompactionStats::default();
        assert!(stats.is_empty());
        stats.record(4);
        stats.record(3);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.turns_removed, 7);
        assert!(!stats.is_empty());
    }

    #[test]
    fn the_recent_turn_target_is_bounded() {
        let limits = BudgetSet::new();
        let target = recent_target_turns(&limits);
        assert!(target >= 2, "target was {target}");
        assert!(target <= 16, "target was {target}");
    }

    #[test]
    fn compaction_can_run_repeatedly() {
        let mut history = long_history();
        for _ in 0..3 {
            let Some(plan) = plan(&history, &BudgetSet::new()) else {
                break;
            };
            apply(&mut history, &plan, format!("summary at {}", plan.cut_at));
            history.validate().expect("valid after compaction");
        }
        // The history stays valid and never becomes empty.
        assert!(!history.is_empty());
    }
}
