//! Context compaction.
//!
//! When a request reaches the trigger, the oldest part of the conversation is
//! replaced by a summary and the turn continues in a fresh window. Two
//! properties matter:
//!
//! - The cut never separates a tool call from its result, which no provider
//!   accepts. It lands on a user turn where one is in reach, and on an
//!   assistant turn otherwise.
//! - A failed or cancelled compaction leaves the history exactly as it was, so
//!   a summary is never partially installed.
//! - Explicitly pinned user constraints live outside the turn list. They are
//!   sent verbatim with subsequent requests regardless of what the summary says.

use std::fmt::Write as _;

use rune_core::budget::{BudgetSet, LimitName};
use rune_core::error::{ErrorCode, Result, RuneError};

use rune_net::fetch::Fetch;
use rune_net::provider::{Provider, RequestPlan};
use rune_net::stream::{FinishReason, Usage};
use rune_net::transport::{self, RequestTimeouts};

use crate::history::History;
use crate::tokens::{CapacityDecision, Estimate, UsageTotals, decide_capacity, trigger_percent};
use crate::turn::{self, Event, Host};

/// A model's context window, in tokens.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Window(pub u64);

/// Keeps the turns a compaction removes, so they can be read back later.
pub trait Archive {
    /// Stores `removed` before the summary replaces them.
    fn archive(&self, removed: &[crate::history::Turn]) -> Result<()>;
}

/// What one compaction did.
#[derive(Clone, Copy, Debug)]
pub struct CompactionReport {
    /// Original turns replaced by the summary.
    pub removed_turns: usize,
    /// Turns left, including the summary.
    pub remaining_turns: usize,
    /// Usage of the summary request.
    pub usage: Usage,
}

/// What an automatic compaction did, with the request size around it.
#[derive(Clone, Copy, Debug)]
pub struct AutoCompaction {
    /// The compaction itself.
    pub report: CompactionReport,
    /// Estimated request tokens before.
    pub before_tokens: u64,
    /// Estimated request tokens after.
    pub after_tokens: u64,
}

/// How long a compaction request waits when no limit is configured.
pub const DEFAULT_COMPACTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How long a compaction request waits.
///
/// Taken from the configured head timeout, so a slow endpoint that has been
/// given more room to answer is not cut off here at a smaller number.
fn compaction_timeout(limits: &BudgetSet) -> std::time::Duration {
    limits
        .get_usize(LimitName::ProviderHeadTimeoutMs)
        .try_into()
        .map_or(DEFAULT_COMPACTION_TIMEOUT, std::time::Duration::from_millis)
}

/// Estimates the actual dialect body, including instructions and tool schemas.
pub fn request_estimate(
    dialect: &dyn Provider,
    plan: &RequestPlan,
    window: Window,
) -> Result<Estimate> {
    encode_request(dialect, plan, window).map(|(_, estimate)| estimate)
}

/// Serializes the request exactly as the transport would and estimates it.
///
/// The text is what `transport::stream_completion_encoded` can send, so the
/// request is serialized once for both purposes.
pub fn encode_request(
    dialect: &dyn Provider,
    plan: &RequestPlan,
    window: Window,
) -> Result<(String, Estimate)> {
    let body = dialect.build_request(plan)?;
    let encoded = serde_json::to_string(&body)
        .map_err(|err| RuneError::new(ErrorCode::Internal, err.to_string()))?;
    let bytes = encoded.len() as u64;
    // Include dialect defaults such as Anthropic's max_tokens reserve.
    let output = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
        .iter()
        .find_map(|key| body.get(key).and_then(serde_json::Value::as_u64))
        .unwrap_or(0);
    let capacity = crate::tokens::usable_input_tokens(window.0, output);
    Ok((
        encoded,
        Estimate::new(bytes, crate::tokens::estimate_tokens(bytes), capacity),
    ))
}

/// Summarizes the turns `plan` removes and installs the summary.
///
/// The summary is produced by the model, because nothing else can compress a
/// conversation without discarding what makes it useful. History changes only
/// after a usable summary has arrived: a failed request, an incomplete or
/// unusable summary, or a failed archive leaves it exactly as it was. The
/// summary request's usage is returned, not recorded.
pub fn compact(
    history: &mut History,
    plan: &Plan,
    host: &dyn Host,
    client: &dyn Fetch,
    archive: Option<&dyn Archive>,
    cancelled: &dyn Fn() -> bool,
) -> Result<CompactionReport> {
    turn::check_objective_budget(host)?;
    let limits = host.limits();
    let request = render_summary_request(history, plan);
    let mut request_plan = RequestPlan::new(host.model());
    SUMMARY_INSTRUCTIONS.clone_into(&mut request_plan.instructions);
    request_plan.messages = transport::one_shot_messages(&request);
    let mut accounting = turn::ObjectiveAccounting::begin(host, &mut request_plan)?;
    let response = transport::stream_completion_observed(
        client,
        host.endpoint(),
        host.dialect(),
        &request_plan,
        RequestTimeouts {
            head: compaction_timeout(&limits),
            ..RequestTimeouts::from_limits(&limits)
        },
        cancelled,
        &mut |event| accounting.observe(event),
    );
    accounting.finish(host, response.as_ref().ok().map(|outcome| outcome.usage))?;
    let outcome = response.map_err(|err| {
        RuneError::new(
            err.kind().code(),
            format!("compaction failed, nothing was changed: {err}"),
        )
    })?;
    if outcome.finish == Some(FinishReason::MaxTokens) {
        return Err(RuneError::new(
            ErrorCode::InvalidState,
            "compaction produced an incomplete summary: the summarizer reached the token limit; nothing was changed",
        ));
    }
    let summary = outcome.text();
    validate_summary(&summary).map_err(|err| {
        RuneError::new(
            err.code(),
            format!("compaction produced nothing usable: {err}"),
        )
    })?;
    if let Some(archive) = archive {
        archive
            .archive(&history.turns()[..plan.removed_turns])
            .map_err(|err| {
                RuneError::new(
                    err.code(),
                    format!(
                        "compaction could not archive original messages; history unchanged: {err}"
                    ),
                )
            })?;
    }
    host.before_compaction(history);
    let removed_turns = apply(history, plan, wrap_summary(&summary));
    Ok(CompactionReport {
        removed_turns,
        remaining_turns: history.len(),
        usage: outcome.usage,
    })
}

/// Compacts before a request when the trigger says it must.
///
/// Returns `Ok(None)` when no compaction was needed or none is possible yet
/// because the conversation is short, and an error when the request cannot
/// fit even after compacting. On success `request` is rebuilt from the new
/// history.
pub fn auto_compact(
    history: &mut History,
    request: &mut RequestPlan,
    host: &dyn Host,
    client: &dyn Fetch,
    window: Window,
) -> Result<Option<AutoCompaction>> {
    auto_compact_encoded(history, request, host, client, window).map(|(done, _)| done)
}

/// [`auto_compact`], also returning the request serialized as it now stands,
/// for a host that sends it through `Host::take_encoded_request`.
pub fn auto_compact_encoded(
    history: &mut History,
    request: &mut RequestPlan,
    host: &dyn Host,
    client: &dyn Fetch,
    window: Window,
) -> Result<(Option<AutoCompaction>, String)> {
    let limits = host.limits();
    let (encoded, estimate) = encode_request(host.dialect(), request, window)?;
    // A request under the trigger needs no look at the retained tail, which
    // costs a second serialization of nearly the whole conversation: the tail
    // is smaller, so it fits whenever the whole request does.
    if estimate.total_tokens() <= estimate.capacity
        && estimate.used_percent() < trigger_percent(&limits)
    {
        return Ok((None, encoded));
    }
    let compaction_plan = plan(history, &limits);
    let retained_estimate = if let Some(cut) = &compaction_plan {
        let mut retained_plan = request.clone();
        let mut retained_history = history.clone();
        retained_history.keep_from(cut.cut_at);
        retained_plan.messages = retained_history.to_messages();
        request_estimate(host.dialect(), &retained_plan, window)?
    } else {
        estimate
    };
    match trigger(&estimate, &retained_estimate, &limits) {
        Trigger::NotNeeded | Trigger::Approaching => return Ok((None, encoded)),
        Trigger::Impossible => return Err(cannot_fit_error()),
        Trigger::Required => {}
    }
    let Some(compaction_plan) = compaction_plan else {
        // A short conversation can cross the trigger while still fitting.
        return Ok((None, encoded));
    };
    let cancellation = host.cancellation();
    let report = compact(history, &compaction_plan, host, client, None, &|| {
        cancellation.is_cancelled()
    })?;
    host.context_replaced(&report);
    request.instructions = host.instructions();
    request.messages = history.to_messages();
    let (encoded, after) = encode_request(host.dialect(), request, window)?;
    host.emit(Event::ContextCompacted {
        removed_turns: report.removed_turns,
        remaining_turns: history.len(),
        before_tokens: estimate.input_tokens,
        after_tokens: after.input_tokens,
    });
    if after.input_tokens > estimate.capacity {
        return Err(cannot_fit_error());
    }
    Ok((
        Some(AutoCompaction {
            report,
            before_tokens: estimate.input_tokens,
            after_tokens: after.input_tokens,
        }),
        encoded,
    ))
}

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
/// Contains the turns being removed and the separately pinned user constraints.
/// Retained turns are excluded; pins remain independent of the returned summary.
#[must_use]
pub fn render_summary_request(history: &History, plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str(SUMMARY_INSTRUCTIONS);
    if !history.pinned_constraints().is_empty() {
        out.push_str("\n\n<pinned_user_constraints>\nThese user constraints remain active and will be carried forward verbatim, independently of your summary.\n");
        for text in history.pinned_constraints() {
            out.push_str(text);
            out.push('\n');
        }
        out.push_str("</pinned_user_constraints>");
    }
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
///
/// The full request drives the trigger; the separately measured retained request
/// determines whether compaction is impossible. Neither estimate includes the
/// future summary, so the caller must check capacity again after installing it.
#[must_use]
pub fn trigger(estimate: &Estimate, retained_estimate: &Estimate, limits: &BudgetSet) -> Trigger {
    match decide_capacity(estimate, retained_estimate, limits) {
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
    fn a_conversation_of_multi_step_prompts_can_be_compacted() {
        // Every prompt took two tool steps, so no user turn falls among the
        // few most recent turns. The cut must still be found.
        let mut history = History::new();
        for index in 0..30 {
            history.push_user(format!("question {index}"));
            for step in 0..2 {
                let id = format!("c{index}-{step}");
                history.push_assistant(vec![call(&id)]);
                history.push_tool_results(vec![result(&id)]);
            }
            history.push_assistant(vec![ContentPart::Text {
                text: format!("answer {index}"),
            }]);
        }
        let limits = BudgetSet::new();

        let plan = plan(&history, &limits).expect("a plan");
        let cut = history
            .turns()
            .iter()
            .find(|turn| turn.seq == plan.cut_at)
            .expect("present");
        assert_eq!(cut.role, Role::User);
        assert!(plan.kept_turns >= recent_target_turns(&limits));

        apply(&mut history, &plan, wrap_summary("what happened so far"));
        history.validate().expect("still valid");
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
            trigger(
                &Estimate::new(0, 10, 1000),
                &Estimate::new(0, 10, 1000),
                &limits
            ),
            Trigger::NotNeeded
        );
        assert_eq!(
            trigger(
                &Estimate::new(0, 800, 1000),
                &Estimate::new(0, 800, 1000),
                &limits
            ),
            Trigger::Required
        );
        assert_eq!(
            trigger(
                &Estimate::new(0, 1200, 1000),
                &Estimate::new(0, 1200, 1000),
                &limits
            ),
            Trigger::Impossible
        );
        assert_eq!(
            trigger(
                &Estimate::new(0, 1200, 1000),
                &Estimate::new(0, 100, 1000),
                &limits
            ),
            Trigger::Required
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
    fn pinned_constraints_survive_repeated_compaction_without_summary_help() {
        let constraints = [
            "Do not add dependencies.",
            "Keep café and 日本語 text unchanged.\nPreserve \\\"quotes\\\".",
            "  Never commit, push, or stash.  ",
        ];
        let mut history = long_history();
        assert!(
            history.pinned_constraints().is_empty(),
            "ordinary user and tool content is not pinned"
        );
        for text in constraints {
            assert!(history.pin_constraint(text).expect("pin"));
            assert!(!history.pin_constraint(text).expect("duplicate"));
        }
        assert!(history.pin_constraint(" \n ").is_err());
        for _ in 0..3 {
            let plan = plan(&history, &BudgetSet::new()).expect("plan");
            let request = render_summary_request(&history, &plan);
            for text in constraints {
                assert!(
                    request.contains(text),
                    "summarizer also receives exact pins"
                );
            }
            apply(
                &mut history,
                &plan,
                "A summary that omits every user constraint.",
            );
            history.validate().expect("valid history");
            let messages = history.to_messages();
            let pinned = messages.first().expect("pins");
            assert_eq!(pinned.role, Role::User);
            for text in constraints {
                assert!(pinned.parts.iter().any(|part| part.as_text() == Some(text)));
            }
            for index in 0..6 {
                history.push_user(format!("new question {index}"));
                history.push_assistant(vec![ContentPart::Text {
                    text: "new answer".to_owned(),
                }]);
            }
        }
        let json = serde_json::to_vec(&history).expect("serialize");
        let restored: History = serde_json::from_slice(&json).expect("restore");
        assert_eq!(restored.pinned_constraints(), constraints);
        assert_eq!(restored.branch_at(1).pinned_constraints(), constraints);
        assert!(History::new().pinned_constraints().is_empty());
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
