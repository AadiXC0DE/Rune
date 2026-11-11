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

use rune_net::message::{ContentPart, Message, Role};

use crate::history::{History, Turn};
use crate::tokens::{
    Anchor, CapacityDecision, Estimate, UsageTotals, decide_capacity, estimate_input, measure_body,
};
use crate::turn::{self, Event, Host};

/// A model's context window, in tokens.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Window(pub u64);

/// Keeps the turns a compaction removes, so they can be read back later.
pub trait Archive {
    /// Stores `removed` before the summary replaces them.
    fn archive(&self, removed: &[Turn]) -> Result<()>;
}

/// How a compaction made room.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompactionKind {
    /// Old tool results became receipts, and that was enough.
    Pruned,
    /// Earlier turns were replaced by a summary.
    Checkpoint,
}

/// What one compaction did.
#[derive(Clone, Copy, Debug)]
pub struct CompactionReport {
    /// How it made room.
    pub kind: CompactionKind,
    /// Tool results replaced by receipts before any summary.
    pub pruned_results: usize,
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
    /// Whether it ran early, at a settled point, rather than at the ceiling.
    pub settled: bool,
}

/// Share of the ceiling, in percent, past which a settled point compacts early.
pub const SETTLED_POINT_PERCENT: u64 = 70;

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
    anchor: Option<&Anchor>,
) -> Result<Estimate> {
    encode_request(dialect, plan, window, anchor).map(|(_, estimate)| estimate)
}

/// Serializes the request exactly as the transport would and estimates it.
///
/// The text is what `transport::stream_completion_encoded` can send, so the
/// request is serialized once for both purposes.
pub fn encode_request(
    dialect: &dyn Provider,
    plan: &RequestPlan,
    window: Window,
    anchor: Option<&Anchor>,
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
    let (input_tokens, anchored) = estimate_input(
        measure_body(bytes, plan),
        anchor,
        dialect.name(),
        &plan.model,
    );
    let mut estimate = Estimate::new(bytes, input_tokens, capacity);
    estimate.calibrated = anchored;
    Ok((encoded, estimate))
}

/// Summarizes the turns `plan` removes and installs the summary.
///
/// The summary is produced by the model, because nothing else can compress a
/// conversation without discarding what makes it useful. History changes only
/// after a usable summary has arrived: a failed request, an incomplete or
/// unusable summary, or a failed archive leaves it exactly as it was.
///
/// A reply without the checkpoint headings gets one nudge asking for them; if
/// that reply still lacks them, a complete free-form reply is kept. The usage
/// of the summary requests is returned, not recorded.
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
    request_plan.instructions = checkpoint_instruction(starts_with_checkpoint(history));
    request_plan.messages = transport::one_shot_messages(&request);
    request_plan.cache_key = host.cache_key();
    let first = summarize(host, client, &limits, &mut request_plan, cancelled)?;
    let mut usage = first.usage;
    let summary = if is_checkpoint_reply(&first) {
        first.text()
    } else {
        let reply = first.text();
        if !reply.is_empty() {
            request_plan
                .messages
                .push(Message::assistant(vec![ContentPart::Text { text: reply }]));
        }
        request_plan.messages.push(Message::user(CHECKPOINT_NUDGE));
        let second = summarize(host, client, &limits, &mut request_plan, cancelled)?;
        usage = usage.merge_sum(second.usage);
        let free_form =
            second.finish == Some(FinishReason::Stop) && validate_summary(&second.text()).is_ok();
        if is_checkpoint_reply(&second) || free_form {
            second.text()
        } else if second.finish == Some(FinishReason::MaxTokens) {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                "compaction produced an incomplete summary: the summarizer reached the token limit; nothing was changed",
            ));
        } else {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                "compaction produced nothing usable: the summarizer did not return a checkpoint",
            )
            .with_hint("the previous context was kept"));
        }
    };
    if let Some(archive) = archive {
        // A signed plan keeps no turn as a message, so every turn is archived.
        let removed = if plan.signed {
            history.len()
        } else {
            plan.removed_turns
        };
        archive
            .archive(&history.turns()[..removed])
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
    let removed_turns = apply(history, plan, &summary);
    Ok(CompactionReport {
        kind: CompactionKind::Checkpoint,
        pruned_results: 0,
        removed_turns,
        remaining_turns: history.len(),
        usage,
    })
}

/// Sends one summarizer request, charging it to the objective.
fn summarize(
    host: &dyn Host,
    client: &dyn Fetch,
    limits: &BudgetSet,
    request_plan: &mut RequestPlan,
    cancelled: &dyn Fn() -> bool,
) -> Result<transport::StreamOutcome> {
    let mut accounting = turn::ObjectiveAccounting::begin(host, request_plan)?;
    let response = transport::stream_completion_observed(
        client,
        host.endpoint(),
        host.dialect(),
        request_plan,
        RequestTimeouts {
            head: compaction_timeout(limits),
            ..RequestTimeouts::from_settings(limits, host.provider_head_timeout_ms())
        },
        cancelled,
        &mut |event| accounting.observe(event),
    );
    accounting.finish(host, response.as_ref().ok().map(|outcome| outcome.usage))?;
    response.map_err(|err| {
        RuneError::new(
            err.kind().code(),
            format!("compaction failed, nothing was changed: {err}"),
        )
    })
}

/// Returns true for a complete reply that has the checkpoint headings.
///
/// Only the text is judged; a reply cut at the output limit never counts.
fn is_checkpoint_reply(outcome: &transport::StreamOutcome) -> bool {
    outcome.finish != Some(FinishReason::MaxTokens) && validate_checkpoint(&outcome.text())
}

/// Returns `window` lowered to the bound learned for `model`, if any.
///
/// A known bound also makes an unknown window, stated as zero, known.
#[must_use]
pub fn bounded_window(window: Window, history: &History, model: &str) -> Window {
    match history.window_bound(model) {
        Some(bound) if window.0 == 0 => Window(bound),
        Some(bound) => Window(window.0.min(bound)),
        None => window,
    }
}

/// Compacts now, whatever the trigger says, for a request the provider
/// refused as too long, aiming at `target_tokens` for the next request.
///
/// An unsigned history first has its old tool results replaced by receipts;
/// when that brings the request to the target, no summary is asked for.
/// Otherwise the summary keeps a verbatim tail of at most a quarter of the
/// target. Returns `Ok(true)` when anything changed, even if the request is
/// still above the target, and `Ok(false)` when nothing could be cut.
pub fn compact_now(
    history: &mut History,
    host: &dyn Host,
    client: &dyn Fetch,
    target_tokens: u64,
) -> Result<bool> {
    let size = |history: &History| -> Result<u64> {
        let bytes = serde_json::to_vec(&history.to_messages())
            .map_err(|err| RuneError::new(ErrorCode::Internal, err.to_string()))?
            .len() as u64;
        Ok(crate::tokens::estimate_tokens(bytes))
    };
    let mut pruned = false;
    if !history.is_signed(host.dialect()) {
        let before = request_tokens(history, host)?;
        let report = crate::prune::prune(history, host.dialect());
        if report.results > 0 {
            pruned = true;
            let after = request_tokens(history, host)?;
            host.emit(Event::ContextPruned {
                results: report.results,
                tokens_freed: before.saturating_sub(after),
            });
            if after <= target_tokens {
                return Ok(true);
            }
        }
    }
    let limits = host.limits();
    let keep = limits
        .get(LimitName::CompactionKeepTokens)
        .effective(u64::MAX)
        .min(target_tokens / 4);
    let Some(compaction_plan) = plan_keeping(history, keep, host.dialect()) else {
        return Ok(pruned);
    };
    let before_tokens = size(history)?;
    let cancellation = host.cancellation();
    let report = compact(history, &compaction_plan, host, client, None, &|| {
        cancellation.is_cancelled()
    })?;
    host.context_replaced(&report);
    let after_tokens = size(history)?;
    host.emit(Event::ContextCompacted {
        removed_turns: report.removed_turns,
        remaining_turns: history.len(),
        before_tokens,
        after_tokens,
        settled: false,
    });
    Ok(true)
}

/// Estimates the next request built from `history`, as the loop would build it.
fn request_tokens(history: &History, host: &dyn Host) -> Result<u64> {
    let mut plan = RequestPlan::new(host.model());
    plan.instructions = host.instructions();
    plan.messages = history.to_messages();
    plan.tools = host.tools();
    let measure = crate::tokens::measure(host.dialect(), &plan)?;
    Ok(estimate_input(
        measure,
        history.anchor(),
        host.dialect().name(),
        &plan.model,
    )
    .0)
}

/// Compacts before a request when the estimate reaches the ceiling, or early
/// at a settled point once it is past seventy percent of the ceiling.
///
/// At the ceiling, an unsigned history first has its old tool results
/// replaced by receipts. When that brings the request to sixty percent of the
/// ceiling or under, no summary is asked for; otherwise the summary is made
/// from the pruned history.
///
/// Returns `Ok(None)` when no compaction was needed or none is possible yet:
/// the window is unknown, a tool call is still waiting for its result, or the
/// conversation is short. Returns an error when the request cannot fit even
/// after compacting. On success `request` is rebuilt from the new history.
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
    let window = bounded_window(window, history, &request.model);
    let ceiling = crate::tokens::ceiling(window.0, &limits);
    let window = Window(crate::tokens::effective_window(window.0, &limits));
    let (mut encoded, estimate) =
        encode_request(host.dialect(), request, window, history.anchor())?;
    // An unknown window gives no threshold to compact against. The request is
    // sent as it is, and an overflow the provider reports is recovered from.
    let Some(ceiling) = ceiling else {
        return Ok((None, encoded));
    };
    // Never inside a batch: the cut must not separate a call from its result.
    if !history.pending_tool_calls().is_empty() {
        return Ok((None, encoded));
    }
    // A request under the ceiling needs no look at the retained tail, which
    // costs a second serialization of nearly the whole conversation: the tail
    // is smaller, so it fits whenever the whole request does.
    let required = estimate.total_tokens() > estimate.capacity || estimate.input_tokens >= ceiling;
    let settled = !required
        && estimate.input_tokens.saturating_mul(100)
            > ceiling.saturating_mul(SETTLED_POINT_PERCENT)
        && history.ledger().settled
        && history.settled_turn().is_some()
        && history.settled_turn() == history.turns().last().map(|turn| turn.seq);
    if !required && !settled {
        return Ok((None, encoded));
    }
    let mut pruned = None;
    if required && !history.is_signed(host.dialect()) {
        let report = crate::prune::prune(history, host.dialect());
        if report.results > 0 {
            request.messages = history.to_messages();
            let (pruned_encoded, after) =
                encode_request(host.dialect(), request, window, history.anchor())?;
            encoded = pruned_encoded;
            host.emit(Event::ContextPruned {
                results: report.results,
                tokens_freed: estimate.input_tokens.saturating_sub(after.input_tokens),
            });
            let done = AutoCompaction {
                report: CompactionReport {
                    kind: CompactionKind::Pruned,
                    pruned_results: report.results,
                    removed_turns: 0,
                    remaining_turns: history.len(),
                    usage: Usage::default(),
                },
                before_tokens: estimate.input_tokens,
                after_tokens: after.input_tokens,
                settled: false,
            };
            if after.input_tokens <= ceiling.saturating_mul(3) / 5 {
                return Ok((Some(done), encoded));
            }
            pruned = Some(done);
        }
    }
    let compaction_plan = plan_for(history, &limits, host.dialect());
    if required {
        let retained_estimate = if let Some(cut) = &compaction_plan {
            let mut retained_plan = request.clone();
            let mut retained_history = history.clone();
            retained_history.keep_from(cut.cut_at);
            retained_plan.messages = retained_history.to_messages();
            request_estimate(
                host.dialect(),
                &retained_plan,
                window,
                retained_history.anchor(),
            )?
        } else {
            estimate
        };
        match trigger(&estimate, &retained_estimate, ceiling) {
            Trigger::NotNeeded | Trigger::Approaching => return Ok((pruned, encoded)),
            Trigger::Impossible => return Err(cannot_fit_error()),
            Trigger::Required => {}
        }
    }
    let Some(compaction_plan) = compaction_plan else {
        // A short conversation can cross the ceiling while still fitting.
        return Ok((pruned, encoded));
    };
    let cancellation = host.cancellation();
    let mut report = compact(history, &compaction_plan, host, client, None, &|| {
        cancellation.is_cancelled()
    })?;
    report.pruned_results = pruned.map_or(0, |done| done.report.pruned_results);
    host.context_replaced(&report);
    request.instructions = host.instructions();
    request.messages = history.to_messages();
    let (encoded, after) = encode_request(host.dialect(), request, window, history.anchor())?;
    if settled {
        history.ledger_mut().settled = false;
    }
    host.emit(Event::ContextCompacted {
        removed_turns: report.removed_turns,
        remaining_turns: history.len(),
        before_tokens: estimate.input_tokens,
        after_tokens: after.input_tokens,
        settled,
    });
    if after.input_tokens > estimate.capacity {
        return Err(cannot_fit_error());
    }
    Ok((
        Some(AutoCompaction {
            report,
            before_tokens: estimate.input_tokens,
            after_tokens: after.input_tokens,
            settled,
        }),
        encoded,
    ))
}

/// Opening of the summarizer instruction for a first checkpoint.
pub const CHECKPOINT_OPENING: &str = "Write a checkpoint of the conversation above so the work can continue after older messages are removed. Reply with text only and call no tools. Use exactly these headings, in this order:";

/// Opening of the summarizer instruction when the conversation starts with an
/// earlier checkpoint.
pub const CHECKPOINT_UPDATE_OPENING: &str = "The conversation above starts with an earlier checkpoint. Write one updated checkpoint that replaces it, so the work can continue after older messages are removed: keep what still holds, let newer messages win where they disagree, move finished work from \"In progress\" to \"Done\", and drop blockers that were resolved. Do not copy the checkpoint's tags. Reply with text only and call no tools. Use exactly these headings, in this order:";

/// Headings the checkpoint must use, and what goes under each.
///
/// Explicit about what must survive, because a summary that drops the user's
/// current intent or an unresolved blocker is worse than no summary.
pub const CHECKPOINT_TEMPLATE: &str = "## Goal\n## Constraints\n## Decisions\n## State\n### Done\n### In progress\n### Blocked\n## Next steps\n## Files that matter\n## Facts to keep\n\nUnder each heading write short one-line bullets, or \"- none\". Goal: what the user wants, in their words. Constraints: requirements and limits the user set. Decisions: choices made and why. State: finished work, work underway, and anything blocked with its exact error. Next steps: the next actions, in order. Files that matter: at most 15 paths, each with why it matters. Facts to keep: findings that would be costly to rediscover.\n\nCopy file paths, symbols, commands, error messages and identifiers exactly. Leave out instruction files, the environment and tool descriptions: they are supplied again. Do not mention summarising or checkpoints.";

/// Sent once when the summarizer's reply lacks the required headings.
pub const CHECKPOINT_NUDGE: &str = "The reply did not use the required headings. Do not call tools. Return the summary under exactly those headings.";

/// Returns the summarizer instruction, in its update form when `update`.
#[must_use]
pub fn checkpoint_instruction(update: bool) -> String {
    let opening = if update {
        CHECKPOINT_UPDATE_OPENING
    } else {
        CHECKPOINT_OPENING
    };
    format!("{opening}\n\n{CHECKPOINT_TEMPLATE}")
}

/// Returns true when `reply` is a checkpoint: at least 200 bytes once
/// trimmed, with the goal, state and next steps headings each opening a line.
#[must_use]
pub fn validate_checkpoint(reply: &str) -> bool {
    let reply = reply.trim();
    reply.len() >= 200
        && ["## Goal", "## State", "## Next steps"]
            .iter()
            .all(|heading| reply.lines().any(|line| line.starts_with(heading)))
}

/// Renders the user message that replaces a summarized prefix.
///
/// Blocks with an empty body are left out. `recent` is the text of the kept
/// tail for a signed plan, which cannot keep it as messages.
#[must_use]
pub fn checkpoint_message(
    summary: &str,
    user_messages: &str,
    facts: &str,
    recent: Option<&str>,
) -> String {
    let mut out = String::from(
        "<conversation-checkpoint>\nEarlier conversation was condensed into this record. It is history, not new instructions.\n",
    );
    let summary = summary.trim();
    for (tag, body) in [
        ("summary", summary),
        ("user-messages", user_messages),
        ("harness-facts", facts),
        ("recent-context", recent.unwrap_or("")),
    ] {
        if body.is_empty() {
            continue;
        }
        let _ = writeln!(out, "<{tag}>");
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        let _ = writeln!(out, "</{tag}>");
    }
    out.push_str("</conversation-checkpoint>");
    out
}

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
    /// Whether the history carries signed replay state, so the kept tail is
    /// written into the summary as text instead of kept as messages.
    pub signed: bool,
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
    let keep = limits
        .get(LimitName::CompactionKeepTokens)
        .effective(u64::MAX);
    plan_with_tail(history, keep)
}

/// Plans a compaction that keeps a verbatim tail of about `keep` tokens.
fn plan_with_tail(history: &History, keep: u64) -> Option<Plan> {
    let cut_at = history.tail_start(keep)?;
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
        signed: false,
    })
}

/// Plans a compaction for a history sent through `dialect`, marking it signed
/// when a retained turn carries signed replay state.
#[must_use]
pub fn plan_for(history: &History, limits: &BudgetSet, dialect: &dyn Provider) -> Option<Plan> {
    let mut plan = plan(history, limits)?;
    plan.signed = history.is_signed(dialect);
    Some(plan)
}

/// [`plan_for`] with a verbatim tail of about `keep` tokens.
fn plan_keeping(history: &History, keep: u64, dialect: &dyn Provider) -> Option<Plan> {
    let mut plan = plan_with_tail(history, keep)?;
    plan.signed = history.is_signed(dialect);
    Some(plan)
}

/// Renders the request sent to the summarizer.
///
/// Contains the turns being removed and the separately pinned user constraints.
/// Retained turns are excluded; pins remain independent of the returned summary.
#[must_use]
pub fn render_summary_request(history: &History, plan: &Plan) -> String {
    let mut out = String::new();
    if !history.pinned_constraints().is_empty() {
        out.push_str("<pinned_user_constraints>\nThese user constraints remain active and will be carried forward verbatim, independently of your summary.\n");
        for text in history.pinned_constraints() {
            out.push_str(text);
            out.push('\n');
        }
        out.push_str("</pinned_user_constraints>\n\n");
    }
    out.push_str("<conversation>\n");
    for turn in history.turns() {
        if turn.seq >= plan.cut_at {
            break;
        }
        out.push_str(turn.role.as_str());
        out.push_str(": ");
        for part in &turn.parts {
            match part {
                ContentPart::Text { text } => out.push_str(text),
                ContentPart::ToolCall {
                    name, arguments, ..
                } => {
                    let _ = write!(out, "[calls {name} with {arguments}]");
                }
                ContentPart::ToolResult {
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
                ContentPart::Reasoning { .. } => {}
                ContentPart::Image { image } => {
                    let _ = write!(out, "[image {}]", image.id);
                }
            }
        }
        out.push('\n');
    }
    out.push_str("</conversation>\n\n");
    out.push_str(&checkpoint_instruction(starts_with_checkpoint(history)));
    out
}

/// Applies a summary to the history as a checkpoint message.
///
/// The user's own messages from the removed prefix follow the summary word for
/// word, then the facts the harness recorded. An unsigned plan keeps the retained tail verbatim after the summary
/// turn, so the resulting length is the retained count plus one. A signed plan
/// keeps the summary turn alone, with the tail written into it as text, because
/// signed replay state cannot follow a rewritten prefix. Returns the number of
/// turns removed, which is the retained count subtracted from the previous
/// length, not the difference in length after the summary was added.
pub fn apply(history: &mut History, plan: &Plan, summary: &str) -> usize {
    let carried = carried_user_messages(history, plan.cut_at);
    let facts = {
        let live_plan = history.shared_live_plan();
        let live_plan = live_plan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        history
            .ledger()
            .render(&live_plan, history.pinned_constraints())
    };
    let (recent, keep_from) = if plan.signed {
        let start = history
            .turns()
            .iter()
            .position(|turn| turn.seq >= plan.cut_at)
            .unwrap_or(history.len());
        (
            Some(render_recent_context(&history.turns()[start..])),
            history.next_seq(),
        )
    } else {
        (None, plan.cut_at)
    };
    let message = checkpoint_message(summary, &carried, &facts, recent.as_deref());
    let before = history.len();
    history.replace_with_summary(message, keep_from);
    let after = history.len();
    // The summary replaces the removed prefix with exactly one turn.
    before.saturating_sub(after.saturating_sub(1))
}

/// Characters of one carried user message.
const CARRIED_MESSAGE_CHARS: usize = 2000;

/// Bytes of all carried user messages together.
const CARRIED_MESSAGES_BYTES: usize = 16 * 1024;

/// Characters of a tool result in the recent context.
const RECENT_RESULT_CHARS: usize = 2000;

/// Characters of tool call arguments in the recent context.
const RECENT_ARGUMENT_CHARS: usize = 300;

/// Returns true for text that opens a summary installed by compaction.
fn is_checkpoint(text: &str) -> bool {
    text.starts_with("<conversation-checkpoint>")
}

/// Returns true when the first turn is a user turn holding a checkpoint from
/// an earlier compaction.
///
/// Only a leading tag counts; text that quotes the tag later does not.
#[must_use]
pub fn starts_with_checkpoint(history: &History) -> bool {
    history
        .turns()
        .first()
        .is_some_and(|turn| turn.role == Role::User && is_checkpoint(&turn.text()))
}

/// Returns the user messages a summary at the front of the history carries.
fn checkpoint_messages(text: &str) -> Vec<String> {
    const OPEN: &str = "\n<user-messages>\n";
    const ENTRY_OPEN: &str = "<message>\n";
    const ENTRY_CLOSE: &str = "\n</message>\n";
    let Some(start) = text.rfind(OPEN) else {
        return Vec::new();
    };
    let (_, body) = text.split_at(start);
    let body = body.strip_prefix(OPEN).unwrap_or(body);
    let body = body
        .split_once("</user-messages>")
        .map_or(body, |(inside, _)| inside);
    let mut messages = Vec::new();
    let mut rest = body;
    while let Some((_, after)) = rest.split_once(ENTRY_OPEN) {
        let Some((message, next)) = after.split_once(ENTRY_CLOSE) else {
            break;
        };
        messages.push(message.to_owned());
        rest = next;
    }
    messages
}

/// Removes a leading plan mode note and every context update from a user
/// message.
fn strip_harness_blocks(text: &str) -> String {
    let text = text
        .strip_prefix("<plan_mode>")
        .and_then(|rest| rest.split_once("</plan_mode>"))
        .map_or(text, |(_, after)| after);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once("<context-update>") {
        let Some((_, next)) = after.split_once("</context-update>") else {
            break;
        };
        out.push_str(before);
        rest = next;
    }
    out.push_str(rest);
    out.trim().to_owned()
}

/// Cuts a carried message after 2,000 characters, noting how many were left
/// out.
fn cut_message(text: &str) -> String {
    match text.char_indices().nth(CARRIED_MESSAGE_CHARS) {
        Some((end, _)) => {
            let more = text[end..].chars().count();
            format!("{}\n[{more} more characters]", &text[..end])
        }
        None => text.to_owned(),
    }
}

/// Returns the user's own messages from before `cut_at`, as the body of a
/// `<user-messages>` block.
///
/// Messages a previous summary carried come first, then each user turn before
/// the cut, oldest first. Harness-written text and earlier summaries are not
/// user messages. Each message is cut at 2,000 characters, and the oldest are
/// left out when the whole block would pass 16 KiB.
#[must_use]
pub fn carried_user_messages(history: &History, cut_at: u64) -> String {
    let mut entries: Vec<String> = Vec::new();
    for (index, turn) in history.turns().iter().enumerate() {
        if turn.seq >= cut_at {
            break;
        }
        if turn.role != Role::User {
            continue;
        }
        let text = turn.text();
        if is_checkpoint(&text) {
            if index == 0 {
                entries.extend(checkpoint_messages(&text));
            }
            continue;
        }
        if text.starts_with("<harness_check>") {
            continue;
        }
        let text = strip_harness_blocks(&text);
        if !text.is_empty() {
            entries.push(cut_message(&text));
        }
    }

    let mut kept: Vec<String> = Vec::new();
    let mut total = 0_usize;
    for entry in entries.iter().rev() {
        let rendered = format!("<message>\n{entry}\n</message>\n");
        if total.saturating_add(rendered.len()) > CARRIED_MESSAGES_BYTES {
            break;
        }
        total = total.saturating_add(rendered.len());
        kept.push(rendered);
    }
    let omitted = entries.len().saturating_sub(kept.len());
    let mut out = String::with_capacity(total.saturating_add(48));
    if omitted > 0 {
        let _ = writeln!(out, "[{omitted} earlier user messages omitted]");
    }
    for rendered in kept.iter().rev() {
        out.push_str(rendered);
    }
    out
}

/// Keeps the first and last halves of `keep` characters of `text`, with a
/// marker naming how many characters were left out between them.
#[must_use]
pub fn clip_middle(text: &str, keep: usize) -> String {
    let total = text.chars().count();
    if total <= keep {
        return text.to_owned();
    }
    let head = keep / 2;
    let tail = keep.saturating_sub(head);
    let byte_at = |chars: usize| {
        text.char_indices()
            .nth(chars)
            .map_or(text.len(), |(at, _)| at)
    };
    format!(
        "{}\n[{} characters left out]\n{}",
        &text[..byte_at(head)],
        total.saturating_sub(keep),
        &text[byte_at(total.saturating_sub(tail))..]
    )
}

/// Renders turns as text, one item per part, for a summary that cannot keep
/// them as messages.
///
/// Reasoning is left out; tool results are clipped to their first and last
/// thousand characters and call arguments to 300 characters.
#[must_use]
pub fn render_recent_context(turns: &[Turn]) -> String {
    let mut out = String::new();
    for turn in turns {
        let mut tool = "";
        for part in &turn.parts {
            match part {
                ContentPart::Text { text } => {
                    let _ = writeln!(out, "{}: {text}", turn.role.as_str());
                }
                ContentPart::ToolCall {
                    name, arguments, ..
                } => {
                    let arguments = match arguments.char_indices().nth(RECENT_ARGUMENT_CHARS) {
                        Some((end, _)) => format!("{}...", &arguments[..end]),
                        None => arguments.clone(),
                    };
                    let _ = writeln!(out, "call {name} {arguments}");
                }
                ContentPart::ToolResult {
                    name,
                    content,
                    is_error,
                    ..
                } => {
                    tool = name;
                    let label = if *is_error { "error" } else { "result" };
                    if name == "skill" && content.len() > crate::prune::MAX_KEPT_SKILL_BYTES {
                        let receipt = crate::prune::SKILL_RECEIPT
                            .replace("{name}", &crate::prune::skill_name(content, None));
                        let _ = writeln!(out, "{label} skill: {receipt}");
                        continue;
                    }
                    let content = clip_middle(content, RECENT_RESULT_CHARS);
                    let _ = writeln!(out, "{label} {name}: {content}");
                }
                ContentPart::Image { .. } => {
                    if turn.role == Role::Tool {
                        let _ = writeln!(out, "result {tool}: [image]");
                    } else {
                        let _ = writeln!(out, "{}: [image]", turn.role.as_str());
                    }
                }
                ContentPart::Reasoning { .. } => {}
            }
        }
    }
    out
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
pub fn trigger(estimate: &Estimate, retained_estimate: &Estimate, ceiling: u64) -> Trigger {
    match decide_capacity(estimate, retained_estimate, ceiling) {
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

    use rune_core::budget::Budget;
    use rune_core::config::Layer;
    use rune_core::id::ToolCallId;

    #[test]
    fn a_learned_bound_lowers_or_supplies_the_window_of_its_model() {
        let mut history = History::new();
        assert_eq!(
            bounded_window(Window(200_000), &history, "m"),
            Window(200_000)
        );
        history.learn_window_bound("m", 8_000);
        assert_eq!(
            bounded_window(Window(200_000), &history, "m"),
            Window(8_000)
        );
        assert_eq!(bounded_window(Window(4_000), &history, "m"), Window(4_000));
        assert_eq!(bounded_window(Window(0), &history, "m"), Window(8_000));
        assert_eq!(bounded_window(Window(0), &history, "other"), Window(0));
    }

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

    /// Limits that keep only the newest exchange.
    fn keep_none() -> BudgetSet {
        let mut limits = BudgetSet::new();
        limits
            .set(
                LimitName::CompactionKeepTokens,
                Budget::Bounded(0),
                Layer::CommandLine,
            )
            .expect("keep");
        limits
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
        let plan = plan(&history, &keep_none()).expect("a plan");
        let cut = history
            .turns()
            .iter()
            .find(|turn| turn.seq == plan.cut_at)
            .expect("present");
        assert_eq!(cut.role, Role::User);
        // The newest prompt: its question, two calls with results, the answer.
        assert_eq!(plan.kept_turns, 6);

        apply(&mut history, &plan, "what happened so far");
        history.validate().expect("still valid");
    }

    #[test]
    fn a_short_history_has_nothing_to_compact() {
        let mut history = History::new();
        history.push_user("only");
        assert!(plan(&history, &keep_none()).is_none());
    }

    #[test]
    fn a_long_history_produces_a_plan() {
        let history = long_history();
        let plan = plan(&history, &keep_none()).expect("a plan");
        assert!(plan.removed_turns > 0);
        assert!(plan.kept_turns > 0);
        assert!(plan.removed_bytes > 0);
    }

    #[test]
    fn the_plan_cuts_on_a_user_turn() {
        let history = long_history();
        let plan = plan(&history, &keep_none()).expect("a plan");
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
        let plan = plan(&history, &keep_none()).expect("a plan");
        let removed = apply(&mut history, &plan, "what happened so far");
        assert_eq!(removed, plan.removed_turns);
        history.validate().expect("still valid");
    }

    #[test]
    fn the_summary_is_the_first_turn_after_compaction() {
        let mut history = long_history();
        let plan = plan(&history, &keep_none()).expect("a plan");
        apply(&mut history, &plan, "the summary");
        let first = history.turns().first().expect("a turn");
        assert!(first.text().contains("the summary"));
    }

    #[test]
    fn compaction_preserves_every_recent_turn() {
        let mut history = long_history();
        let last_text = history.turns().last().expect("a turn").text();
        let plan = plan(&history, &keep_none()).expect("a plan");
        apply(&mut history, &plan, "summary");
        let texts: Vec<String> = history.turns().iter().map(Turn::text).collect();
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
        let plan = plan(&history, &keep_none()).expect("a plan");
        let summary = "";
        assert!(validate_summary(summary).is_err());
        // Because validation happens first, apply is never reached.
        assert_eq!(history.len(), before);
        assert!(plan.removed_turns > 0);
    }

    #[test]
    fn the_summary_request_contains_only_the_removed_prefix() {
        let history = long_history();
        let plan = plan(&history, &keep_none()).expect("a plan");
        let request = render_summary_request(&history, &plan);
        assert!(request.contains(CHECKPOINT_OPENING));

        let kept: Vec<String> = history
            .turns()
            .iter()
            .filter(|turn| turn.seq >= plan.cut_at)
            .map(Turn::text)
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
        let plan = plan(&history, &keep_none()).expect("a plan");
        let request = render_summary_request(&history, &plan);
        assert!(request.contains("calls read_file"), "{request}");
    }

    #[test]
    fn the_removed_percentage_is_computed() {
        let plan = Plan {
            cut_at: 5,
            removed_turns: 4,
            kept_turns: 2,
            removed_bytes: 750,
            signed: false,
        };
        assert_eq!(plan.removed_percent(1000), 75);
        assert_eq!(plan.removed_percent(0), 0);
    }

    #[test]
    fn the_trigger_maps_from_the_capacity_decision() {
        let ceiling = 800;
        assert_eq!(
            trigger(
                &Estimate::new(0, 10, 1000),
                &Estimate::new(0, 10, 1000),
                ceiling
            ),
            Trigger::NotNeeded
        );
        assert_eq!(
            trigger(
                &Estimate::new(0, 800, 1000),
                &Estimate::new(0, 800, 1000),
                ceiling
            ),
            Trigger::Required
        );
        assert_eq!(
            trigger(
                &Estimate::new(0, 1200, 1000),
                &Estimate::new(0, 1200, 1000),
                ceiling
            ),
            Trigger::Impossible
        );
        assert_eq!(
            trigger(
                &Estimate::new(0, 1200, 1000),
                &Estimate::new(0, 100, 1000),
                ceiling
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
            let plan = plan(&history, &keep_none()).expect("plan");
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
            let Some(plan) = plan(&history, &keep_none()) else {
                break;
            };
            apply(&mut history, &plan, &format!("summary at {}", plan.cut_at));
            history.validate().expect("valid after compaction");
        }
        // The history stays valid and never becomes empty.
        assert!(!history.is_empty());
    }

    /// A reply in the checkpoint shape.
    const CHECKPOINT_REPLY: &str = "## Goal\n- answer the questions\n## Constraints\n- none\n## Decisions\n- none\n## State\n### Done\n- the earlier questions were answered from file contents\n### In progress\n- none\n### Blocked\n- none\n## Next steps\n- answer the newest question\n## Files that matter\n- none\n## Facts to keep\n- none";

    /// Answers requests in order from `replies`, each a text and a finish
    /// reason, and keeps every request body.
    struct ReplyFetch {
        replies: std::sync::Mutex<std::collections::VecDeque<(String, &'static str)>>,
        bodies: std::sync::Mutex<Vec<serde_json::Value>>,
    }

    impl ReplyFetch {
        fn new(replies: &[(&str, &'static str)]) -> Self {
            Self {
                replies: std::sync::Mutex::new(
                    replies
                        .iter()
                        .map(|(text, finish)| ((*text).to_owned(), *finish))
                        .collect(),
                ),
                bodies: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn bodies(&self) -> Vec<serde_json::Value> {
            self.bodies.lock().expect("bodies").clone()
        }
    }

    impl Fetch for ReplyFetch {
        fn send(
            &self,
            request: rune_net::fetch::FetchRequest,
        ) -> rune_net::error::NetResult<rune_net::fetch::FetchResponse> {
            self.bodies
                .lock()
                .expect("bodies")
                .push(serde_json::from_slice(&request.body).expect("request JSON"));
            let (text, finish) = self
                .replies
                .lock()
                .expect("replies")
                .pop_front()
                .expect("a scripted reply");
            let chunk = serde_json::json!({
                "choices": [{"delta": {"content": text}, "finish_reason": finish}]
            });
            let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            Ok(rune_net::fetch::FetchResponse {
                status: 200,
                content_type: "text/event-stream".to_owned(),
                retry_after: None,
                retry_after_ms: None,
                should_retry: None,
                location: None,
                body: Box::new(std::io::Cursor::new(body.into_bytes())),
            })
        }
    }

    fn short_history() -> History {
        let mut history = History::new();
        for index in 0..3 {
            history.push_user(format!("question {index}"));
            history.push_assistant(vec![ContentPart::Text {
                text: format!("answer {index}"),
            }]);
        }
        history
    }

    /// Compacts a short history with `fetch` as the summarizer.
    fn compact_with(fetch: &ReplyFetch) -> (History, History, Result<CompactionReport>) {
        let host = SummaryHost::new();
        let mut history = short_history();
        let before = history.clone();
        let cut = plan(&history, &host.limits()).expect("a plan");
        let result = compact(&mut history, &cut, &host, fetch, None, &|| false);
        (before, history, result)
    }

    const NO_HEADINGS: &str = "Here is what happened so far, without any headings.";

    #[test]
    fn an_invalid_reply_gets_one_nudge() {
        let fetch = ReplyFetch::new(&[(NO_HEADINGS, "stop"), (CHECKPOINT_REPLY, "stop")]);
        let (_, history, result) = compact_with(&fetch);
        result.expect("the second reply is a checkpoint");
        let bodies = fetch.bodies();
        assert_eq!(bodies.len(), 2);
        let first = bodies[0]["messages"].as_array().expect("messages");
        let second = bodies[1]["messages"].as_array().expect("messages");
        assert_eq!(second.len(), first.len() + 2);
        assert_eq!(second[..first.len()], first[..]);
        assert_eq!(second[first.len()]["role"], "assistant");
        assert_eq!(second[first.len()]["content"], NO_HEADINGS);
        assert_eq!(second[first.len() + 1]["role"], "user");
        assert_eq!(second[first.len() + 1]["content"], CHECKPOINT_NUDGE);
        let text = history.turns()[0].text();
        assert!(text.contains("<summary>\n## Goal\n"), "{text}");
    }

    #[test]
    fn a_free_form_reply_is_kept_after_a_failed_nudge() {
        let free_form = "The questions were answered; continue with the newest one.";
        let fetch = ReplyFetch::new(&[(NO_HEADINGS, "stop"), (free_form, "stop")]);
        let (_, history, result) = compact_with(&fetch);
        result.expect("a usable free-form reply");
        assert_eq!(fetch.bodies().len(), 2);
        let text = history.turns()[0].text();
        assert!(
            text.contains(&format!("<summary>\n{free_form}\n</summary>")),
            "{text}"
        );
    }

    #[test]
    fn a_short_reply_after_the_nudge_leaves_the_history_unchanged() {
        let fetch = ReplyFetch::new(&[("no headings here", "stop"), ("still none", "stop")]);
        let (before, history, result) = compact_with(&fetch);
        let err = result.expect_err("nothing usable");
        assert_eq!(
            err.message(),
            "compaction produced nothing usable: the summarizer did not return a checkpoint"
        );
        assert_eq!(fetch.bodies().len(), 2);
        assert_eq!(history.turns(), before.turns());
    }

    #[test]
    fn a_reply_cut_at_the_output_limit_is_never_kept() {
        let fetch = ReplyFetch::new(&[(CHECKPOINT_REPLY, "length"), (CHECKPOINT_REPLY, "length")]);
        let (before, history, result) = compact_with(&fetch);
        let err = result.expect_err("a cut reply is refused");
        assert!(err.message().contains("token limit"), "{}", err.message());
        assert_eq!(fetch.bodies().len(), 2);
        assert_eq!(history.turns(), before.turns());
    }

    /// Returns the text of the last user message of a summarizer request body.
    fn last_user_text(body: &serde_json::Value) -> String {
        body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .and_then(|message| message["content"].as_str())
            .expect("a user message")
            .to_owned()
    }

    #[test]
    fn a_second_compaction_asks_for_an_update() {
        let host = SummaryHost::new();
        let fetch = ReplyFetch::new(&[(CHECKPOINT_REPLY, "stop"), (CHECKPOINT_REPLY, "stop")]);
        let mut history = short_history();
        assert!(!starts_with_checkpoint(&history));
        let first = plan(&history, &host.limits()).expect("a plan");
        compact(&mut history, &first, &host, &fetch, None, &|| false).expect("first compaction");
        assert!(starts_with_checkpoint(&history));

        history.push_user("question 3");
        history.push_assistant(vec![ContentPart::Text {
            text: "answer 3".to_owned(),
        }]);
        let second = plan(&history, &host.limits()).expect("a second plan");
        compact(&mut history, &second, &host, &fetch, None, &|| false).expect("second compaction");

        let bodies = fetch.bodies();
        assert_eq!(bodies.len(), 2);
        let opening = |body: &serde_json::Value| {
            let request = last_user_text(body);
            let (_, instruction) = request
                .split_once("</conversation>\n\n")
                .expect("an instruction after the conversation");
            instruction.to_owned()
        };
        assert!(opening(&bodies[0]).starts_with(CHECKPOINT_OPENING));
        assert!(opening(&bodies[1]).starts_with(CHECKPOINT_UPDATE_OPENING));
        assert_eq!(bodies[1]["messages"][0]["role"], "system");
        assert!(
            bodies[1]["messages"][0]["content"]
                .as_str()
                .expect("system text")
                .starts_with(CHECKPOINT_UPDATE_OPENING)
        );

        let mut quoted = History::new();
        quoted.push_user("what does <conversation-checkpoint> mean");
        assert!(!starts_with_checkpoint(&quoted));
        let mut assistant_first = History::new();
        assistant_first.push_assistant(vec![ContentPart::Text {
            text: "<conversation-checkpoint>".to_owned(),
        }]);
        assert!(!starts_with_checkpoint(&assistant_first));
    }

    #[test]
    fn user_messages_survive_two_compactions() {
        let host = SummaryHost::new();
        let fetch = ReplyFetch::new(&[(CHECKPOINT_REPLY, "stop"), (CHECKPOINT_REPLY, "stop")]);
        let mut history = short_history();
        let first = plan(&history, &host.limits()).expect("a plan");
        compact(&mut history, &first, &host, &fetch, None, &|| false).expect("first compaction");

        history.push_user("question 3");
        history.push_assistant(vec![ContentPart::Text {
            text: "answer 3".to_owned(),
        }]);
        let second = plan(&history, &host.limits()).expect("a second plan");
        compact(&mut history, &second, &host, &fetch, None, &|| false).expect("second compaction");

        let text = user_text(&history);
        assert_eq!(
            text.matches("<conversation-checkpoint>").count(),
            1,
            "{text}"
        );
        assert!(
            text.contains(
                "<user-messages>\n<message>\nquestion 0\n</message>\n<message>\nquestion 1\n</message>\n<message>\nquestion 2\n</message>\n</user-messages>\n"
            ),
            "{text}"
        );
        assert_eq!(history.turns()[1].text(), "question 3");
        history.validate().expect("valid after two compactions");
    }

    #[test]
    fn the_instructions_stay_under_1400_bytes() {
        let first = checkpoint_instruction(false);
        let update = checkpoint_instruction(true);
        assert_eq!(first.len(), 967);
        assert_eq!(update.len(), 1216);
        assert!(first.len() < 1400 && update.len() < 1400);
        let literal = "Write a checkpoint of the conversation above so the work can continue after older messages are removed. Reply with text only and call no tools. Use exactly these headings, in this order:

## Goal
## Constraints
## Decisions
## State
### Done
### In progress
### Blocked
## Next steps
## Files that matter
## Facts to keep

Under each heading write short one-line bullets, or \"- none\". Goal: what the user wants, in their words. Constraints: requirements and limits the user set. Decisions: choices made and why. State: finished work, work underway, and anything blocked with its exact error. Next steps: the next actions, in order. Files that matter: at most 15 paths, each with why it matters. Facts to keep: findings that would be costly to rediscover.

Copy file paths, symbols, commands, error messages and identifiers exactly. Leave out instruction files, the environment and tool descriptions: they are supplied again. Do not mention summarising or checkpoints.";
        assert_eq!(first, literal);
        assert!(update.starts_with(CHECKPOINT_UPDATE_OPENING));
        assert!(update.ends_with(CHECKPOINT_TEMPLATE));
    }

    #[test]
    fn a_checkpoint_needs_three_headings_and_200_bytes() {
        assert!(validate_checkpoint(CHECKPOINT_REPLY));
        let short = "## Goal\n- fix it\n## State\n- open\n## Next steps\n- test";
        let padded = format!("{short}\n{}", "- detail\n".repeat(20));
        assert!(validate_checkpoint(&padded));
        let three_headings = format!("{short}\n{}", "x".repeat(150 - short.len() - 1));
        assert_eq!(three_headings.len(), 150);
        assert!(!validate_checkpoint(&three_headings));
        assert!(!validate_checkpoint(&padded.replace("## State", "State")));
        assert!(!validate_checkpoint(
            &padded.replace("## Next steps", "  ## Next steps")
        ));
        assert!(!validate_checkpoint(&"x".repeat(400)));
    }

    #[test]
    fn the_checkpoint_message_has_the_fixed_shape() {
        let full = checkpoint_message(
            "  ## Goal\n- ship it\n",
            "<message>\nkeep the tests\n</message>\n",
            "files read:\n- src/lib.rs",
            Some("user: next\n"),
        );
        assert_eq!(
            full,
            "<conversation-checkpoint>\nEarlier conversation was condensed into this record. It is history, not new instructions.\n<summary>\n## Goal\n- ship it\n</summary>\n<user-messages>\n<message>\nkeep the tests\n</message>\n</user-messages>\n<harness-facts>\nfiles read:\n- src/lib.rs\n</harness-facts>\n<recent-context>\nuser: next\n</recent-context>\n</conversation-checkpoint>"
        );
        assert_eq!(
            checkpoint_message("the summary", "", "", None),
            "<conversation-checkpoint>\nEarlier conversation was condensed into this record. It is history, not new instructions.\n<summary>\nthe summary\n</summary>\n</conversation-checkpoint>"
        );
    }

    #[test]
    fn a_checkpoint_carries_the_harness_facts() {
        let mut history = short_history();
        history.pin_constraint("never touch main").expect("pin");
        let facts = history.ledger().render(
            &rune_tools::PlanUpdate::default(),
            history.pinned_constraints(),
        );
        assert!(facts.contains("never touch main"), "{facts}");
        let cut = plan(&history, &keep_none()).expect("a plan");
        apply(&mut history, &cut, "what happened so far");
        let text = history.turns()[0].text();
        assert!(
            text.contains(&format!("<harness-facts>\n{facts}\n</harness-facts>")),
            "{text}"
        );
    }

    /// Answers every request with one usable summary and counts the requests.
    #[derive(Default)]
    struct SummaryFetch {
        requests: std::sync::atomic::AtomicUsize,
    }

    impl Fetch for SummaryFetch {
        fn send(
            &self,
            _request: rune_net::fetch::FetchRequest,
        ) -> rune_net::error::NetResult<rune_net::fetch::FetchResponse> {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let chunk = serde_json::json!({
                "choices": [{
                    "delta": {"content": CHECKPOINT_REPLY},
                    "finish_reason": "stop",
                }]
            });
            let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            Ok(rune_net::fetch::FetchResponse {
                status: 200,
                content_type: "text/event-stream".to_owned(),
                retry_after: None,
                retry_after_ms: None,
                should_retry: None,
                location: None,
                body: Box::new(std::io::Cursor::new(body.into_bytes())),
            })
        }
    }

    impl SummaryFetch {
        fn requests(&self) -> usize {
            self.requests.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    /// A host whose only model traffic is the summary request.
    struct SummaryHost {
        endpoint: transport::Endpoint,
        events: std::sync::Mutex<Vec<Event>>,
        cancellation: crate::steering::Cancellation,
        steering: crate::steering::SteeringQueue,
    }

    impl SummaryHost {
        fn new() -> Self {
            Self {
                endpoint: transport::Endpoint::new("https://api.example.com", "k"),
                events: std::sync::Mutex::new(Vec::new()),
                cancellation: crate::steering::Cancellation::new(),
                steering: crate::steering::SteeringQueue::new(8),
            }
        }

        fn settled_events(&self) -> Vec<bool> {
            self.events
                .lock()
                .expect("events")
                .iter()
                .filter_map(|event| match event {
                    Event::ContextCompacted { settled, .. } => Some(*settled),
                    _ => None,
                })
                .collect()
        }
    }

    impl Host for SummaryHost {
        // The fixture history is short, so only the newest exchange is kept.
        fn limits(&self) -> BudgetSet {
            let mut limits = BudgetSet::new();
            limits
                .set(
                    LimitName::CompactionKeepTokens,
                    Budget::Bounded(0),
                    Layer::User,
                )
                .expect("tail budget");
            limits
        }

        fn dialect(&self) -> &dyn Provider {
            &rune_net::chat_completions::ChatCompletions
        }

        fn endpoint(&self) -> &transport::Endpoint {
            &self.endpoint
        }

        fn model(&self) -> String {
            "test/model".to_owned()
        }

        fn instructions(&self) -> String {
            "You are a test assistant.".to_owned()
        }

        fn tools(&self) -> Vec<rune_net::message::ToolSpec> {
            Vec::new()
        }

        fn fetch(&self) -> Box<dyn Fetch> {
            Box::new(SummaryFetch::default())
        }

        fn emit(&self, event: Event) {
            self.events.lock().expect("events").push(event);
        }

        fn execute(
            &self,
            _name: &str,
            _arguments: &serde_json::Value,
        ) -> Result<rune_tools::contract::ToolOutput> {
            Ok(rune_tools::contract::ToolOutput::success("ok"))
        }

        fn decide(
            &self,
            _name: &str,
            _target: Option<&str>,
        ) -> (rune_policy::decision::Outcome, String) {
            (rune_policy::decision::Outcome::Allow, "allowed".to_owned())
        }

        fn context(&self) -> rune_tools::contract::ExecutionContext {
            rune_tools::contract::ExecutionContext::new(camino::Utf8PathBuf::from("/tmp/rune-test"))
        }

        fn cancellation(&self) -> crate::steering::Cancellation {
            self.cancellation.clone()
        }

        fn steering(&self) -> &crate::steering::SteeringQueue {
            &self.steering
        }
    }

    /// The request the host would send for `history`.
    fn request_for(host: &SummaryHost, history: &History) -> RequestPlan {
        let mut request = RequestPlan::new(host.model());
        request.instructions = host.instructions();
        request.messages = history.to_messages();
        request
    }

    /// Estimated input tokens of the request for `history`.
    fn input_tokens(host: &SummaryHost, history: &History) -> u64 {
        request_estimate(
            host.dialect(),
            &request_for(host, history),
            Window(u64::MAX),
            None,
        )
        .expect("estimate")
        .input_tokens
    }

    /// The smallest window whose ceiling puts `tokens` at `percent` of it or below.
    fn window_at(tokens: u64, percent: u64) -> Window {
        let limits = BudgetSet::new();
        let mut window = tokens;
        while crate::tokens::ceiling(window, &limits)
            .expect("ceiling")
            .saturating_mul(percent)
            < tokens.saturating_mul(100)
        {
            window = window.saturating_add(1);
        }
        Window(window)
    }

    /// A long history whose newest turn holds a check run after an edit.
    fn settled_history() -> History {
        let mut history = long_history();
        history.push_user("fix the parser and run the tests");
        history.push_assistant(vec![call("check")]);
        let seq = history.push_tool_results(vec![result("check")]);
        history.ledger_mut().settled = true;
        history.set_settled_turn(seq);
        history
    }

    #[test]
    fn a_settled_point_compacts_early() {
        let host = SummaryHost::new();
        let fetch = SummaryFetch::default();
        let mut history = settled_history();
        let window = window_at(input_tokens(&host, &history), 75);
        let mut request = request_for(&host, &history);

        let done = auto_compact(&mut history, &mut request, &host, &fetch, window)
            .expect("compaction")
            .expect("compacted at the settled point");

        assert!(done.settled);
        assert_eq!(fetch.requests(), 1);
        assert_eq!(host.settled_events(), vec![true]);
        assert!(!history.ledger().settled);
        assert_eq!(request.messages, history.to_messages());
        history.validate().expect("valid after compaction");
    }

    #[test]
    fn a_settled_point_is_used_once() {
        let host = SummaryHost::new();
        let fetch = SummaryFetch::default();
        let mut history = settled_history();
        history.push_assistant(vec![call("next")]);
        history.push_tool_results(vec![result("next")]);
        assert!(history.ledger().settled);
        let window = window_at(input_tokens(&host, &history), 75);
        let mut request = request_for(&host, &history);

        let done =
            auto_compact(&mut history, &mut request, &host, &fetch, window).expect("no compaction");

        assert!(done.is_none());
        assert_eq!(fetch.requests(), 0);
        assert!(host.settled_events().is_empty());

        let mut history = settled_history();
        let window = window_at(input_tokens(&host, &history), 75);
        let mut request = request_for(&host, &history);
        auto_compact(&mut history, &mut request, &host, &fetch, window)
            .expect("compaction")
            .expect("compacted at the settled point");
        let mut request = request_for(&host, &history);
        let again = auto_compact(&mut history, &mut request, &host, &fetch, window)
            .expect("no second compaction");
        assert!(again.is_none());
        assert_eq!(fetch.requests(), 1);
    }

    #[test]
    fn no_compaction_while_a_call_waits_for_its_result() {
        let host = SummaryHost::new();
        let fetch = SummaryFetch::default();
        let mut history = long_history();
        history.push_user("read one more file");
        history.push_assistant(vec![call("waiting")]);
        assert_eq!(history.pending_tool_calls().len(), 1);
        let window = window_at(input_tokens(&host, &history), 105);
        let limits = BudgetSet::new();
        assert!(
            input_tokens(&host, &history)
                >= crate::tokens::ceiling(window.0, &limits).expect("ceiling")
        );
        let mut request = request_for(&host, &history);

        let done =
            auto_compact(&mut history, &mut request, &host, &fetch, window).expect("no compaction");

        assert!(done.is_none());
        assert_eq!(fetch.requests(), 0);

        history.push_tool_results(vec![result("waiting")]);
        let mut request = request_for(&host, &history);
        let done = auto_compact(&mut history, &mut request, &host, &fetch, window)
            .expect("compaction")
            .expect("compacted once the result is in");
        assert!(!done.settled);
        assert_eq!(fetch.requests(), 1);
        assert_eq!(host.settled_events(), vec![false]);
    }

    /// Fails the test on any request, for a run that must not summarize.
    struct NoSummaryFetch;

    impl Fetch for NoSummaryFetch {
        fn send(
            &self,
            _request: rune_net::fetch::FetchRequest,
        ) -> rune_net::error::NetResult<rune_net::fetch::FetchResponse> {
            panic!("no summary request was expected");
        }
    }

    /// Exchanges whose `grep` results are `result_bytes` long, each after a
    /// question padded by `question_bytes`.
    fn bulky_history(count: usize, result_bytes: usize, question_bytes: usize) -> History {
        let mut history = History::new();
        for index in 0..count {
            let id = ToolCallId::new(format!("g{index}")).expect("id");
            history.push_user(format!("question {index} {}", "q".repeat(question_bytes)));
            history.push_assistant(vec![ContentPart::ToolCall {
                id: id.clone(),
                name: "grep".to_owned(),
                arguments: "{}".to_owned(),
            }]);
            history.push_tool_results(vec![ContentPart::ToolResult {
                id,
                name: "grep".to_owned(),
                content: "r".repeat(result_bytes),
                is_error: false,
            }]);
            history.push_assistant(vec![ContentPart::Text {
                text: format!("answer {index}"),
            }]);
        }
        history
    }

    fn pruned_events(host: &SummaryHost) -> Vec<usize> {
        host.events
            .lock()
            .expect("events")
            .iter()
            .filter_map(|event| match event {
                Event::ContextPruned { results, .. } => Some(*results),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn pruning_that_reaches_sixty_percent_skips_the_summary() {
        let host = SummaryHost::new();
        let mut history = bulky_history(24, 16 * 1024, 0);
        let before = input_tokens(&host, &history);
        let window = window_at(before, 105);
        let ceiling = crate::tokens::ceiling(window.0, &BudgetSet::new()).expect("ceiling");
        assert!(before >= ceiling);
        let mut request = request_for(&host, &history);

        let done = auto_compact(&mut history, &mut request, &host, &NoSummaryFetch, window)
            .expect("pruned")
            .expect("room was made");

        assert_eq!(done.report.kind, CompactionKind::Pruned);
        assert!(done.report.pruned_results > 0);
        assert_eq!(done.report.removed_turns, 0);
        assert_eq!(done.report.remaining_turns, history.len());
        assert!(
            done.after_tokens <= ceiling * 3 / 5,
            "{}",
            done.after_tokens
        );
        assert_eq!(pruned_events(&host), vec![done.report.pruned_results]);
        assert!(host.settled_events().is_empty());
        assert_eq!(request.messages, history.to_messages());
        assert!(history.is_pruned("g0"));
        history.validate().expect("valid after pruning");
    }

    #[test]
    fn pruning_that_frees_too_little_goes_on_to_the_summary() {
        let host = SummaryHost::new();
        let fetch = SummaryFetch::default();
        let mut history = bulky_history(24, 2048, 8192);
        let window = window_at(input_tokens(&host, &history), 105);
        let mut request = request_for(&host, &history);

        let done = auto_compact(&mut history, &mut request, &host, &fetch, window)
            .expect("compaction")
            .expect("compacted");

        assert_eq!(done.report.kind, CompactionKind::Checkpoint);
        assert!(done.report.pruned_results > 0);
        assert_eq!(pruned_events(&host), vec![done.report.pruned_results]);
        assert_eq!(fetch.requests(), 1);
        assert_eq!(host.settled_events(), vec![false]);
        assert_eq!(request.messages, history.to_messages());
        history.validate().expect("valid after compaction");
    }

    fn user_text(history: &History) -> String {
        history.turns().first().expect("a turn").text()
    }

    #[test]
    fn user_messages_are_carried_word_for_word() {
        let mut history = long_history();
        history.push_user("Keep café and 日本語 exactly.\nSecond line.");
        history.push_assistant(vec![ContentPart::Text {
            text: "noted".to_owned(),
        }]);
        history.push_user("latest");
        let plan = plan(&history, &keep_none()).expect("a plan");

        let carried = carried_user_messages(&history, plan.cut_at);
        let mut expected = String::new();
        for index in 0..12 {
            let _ = write!(expected, "<message>\nquestion {index}\n</message>\n");
        }
        expected.push_str("<message>\nKeep café and 日本語 exactly.\nSecond line.\n</message>\n");
        assert_eq!(carried, expected);

        apply(&mut history, &plan, "what happened so far");
        let text = user_text(&history);
        assert!(text.ends_with(&format!(
            "</summary>\n<user-messages>\n{expected}</user-messages>\n</conversation-checkpoint>"
        )));
        assert_eq!(history.len(), 2);

        // A second compaction carries the same messages out of the summary.
        history.push_assistant(vec![ContentPart::Text {
            text: "done".to_owned(),
        }]);
        history.push_user("after");
        let again = super::plan(&history, &keep_none()).expect("a plan");
        let carried = carried_user_messages(&history, again.cut_at);
        assert_eq!(
            carried,
            format!("{expected}<message>\nlatest\n</message>\n")
        );
    }

    #[test]
    fn long_user_messages_are_cut_with_a_marker() {
        let mut history = History::new();
        history.push_user(format!("{}{}", "é".repeat(1999), "x".repeat(3001)));
        history.push_assistant(vec![ContentPart::Text {
            text: "ok".to_owned(),
        }]);
        history.push_user("latest");

        let carried = carried_user_messages(&history, 3);
        assert_eq!(
            carried,
            format!(
                "<message>\n{}x\n[3000 more characters]\n</message>\n",
                "é".repeat(1999)
            )
        );
    }

    #[test]
    fn user_messages_past_sixteen_kibibytes_drop_the_oldest() {
        let mut history = History::new();
        for index in 0..20 {
            history.push_user(format!("{index:02}{}", "m".repeat(1498)));
            history.push_assistant(vec![ContentPart::Text {
                text: "ok".to_owned(),
            }]);
        }
        history.push_user("latest");

        let carried = carried_user_messages(&history, 41);
        assert!(carried.starts_with("[10 earlier user messages omitted]\n<message>\n10m"));
        assert_eq!(carried.matches("<message>").count(), 10);
        assert!(carried.ends_with(&format!("<message>\n19{}\n</message>\n", "m".repeat(1498))));
        assert!(!carried.contains("\n09m"));
    }

    #[test]
    fn harness_text_is_not_carried_as_a_user_message() {
        let mut history = History::new();
        history.push_user("<conversation-checkpoint>\nan old summary\n</conversation-checkpoint>");
        history.push_assistant(vec![ContentPart::Text {
            text: "ok".to_owned(),
        }]);
        history.push_user("<plan_mode>The session is in plan mode.</plan_mode>\n\nplan the change");
        history.push_assistant(vec![ContentPart::Text {
            text: "ok".to_owned(),
        }]);
        history.push_user("<harness_check>Files were changed.</harness_check>");
        history.push_assistant(vec![ContentPart::Text {
            text: "ok".to_owned(),
        }]);
        history.push_user("before <context-update>\nnew file\n</context-update> after");
        history.push_assistant(vec![ContentPart::Text {
            text: "ok".to_owned(),
        }]);
        history.push_user("latest");

        let carried = carried_user_messages(&history, 9);
        assert_eq!(
            carried,
            "<message>\nplan the change\n</message>\n<message>\nbefore  after\n</message>\n"
        );
    }

    #[test]
    fn recent_context_drops_skill_bodies() {
        let mut history = History::new();
        history.push_user("use alpha");
        history.push_assistant(vec![ContentPart::ToolCall {
            id: ToolCallId::new("s1").expect("id"),
            name: "skill".to_owned(),
            arguments: "{}".to_owned(),
        }]);
        let body = format!(
            "<skill_content name=\"alpha\">\n{}\n</skill_content>\n",
            "x".repeat(3000)
        );
        history.push_tool_results(vec![ContentPart::ToolResult {
            id: ToolCallId::new("s1").expect("id"),
            name: "skill".to_owned(),
            content: body,
            is_error: false,
        }]);
        let text = render_recent_context(history.turns());
        assert!(
            text.contains(
                "result skill: [skill alpha was loaded here and removed to save context; call the \
                 skill tool again if it is needed]"
            ),
            "{text}"
        );
        assert!(!text.contains("xxxx"), "{text}");
    }

    #[test]
    fn recent_context_renders_one_item_per_part_without_reasoning() {
        let mut history = History::new();
        history.push_user("fix the test");
        history.push_assistant(vec![
            ContentPart::Reasoning {
                text: "private thought".to_owned(),
            },
            ContentPart::Text {
                text: "Looking.".to_owned(),
            },
            ContentPart::ToolCall {
                id: ToolCallId::new("c1").expect("id"),
                name: "read_file".to_owned(),
                arguments: format!("{{\"path\":\"{}\"}}", "a".repeat(300)),
            },
            ContentPart::ToolCall {
                id: ToolCallId::new("c2").expect("id"),
                name: "run".to_owned(),
                arguments: "{\"command\":\"cargo test\"}".to_owned(),
            },
        ]);
        history.push_tool_results(vec![
            ContentPart::ToolResult {
                id: ToolCallId::new("c1").expect("id"),
                name: "read_file".to_owned(),
                content: format!("{}{}", "h".repeat(1000), "t".repeat(1500)),
                is_error: false,
            },
            ContentPart::ToolResult {
                id: ToolCallId::new("c2").expect("id"),
                name: "run".to_owned(),
                content: "1 failed".to_owned(),
                is_error: true,
            },
        ]);

        let rendered = render_recent_context(history.turns());
        let expected = format!(
            "user: fix the test\n\
             assistant: Looking.\n\
             call read_file {{\"path\":\"{}...\n\
             call run {{\"command\":\"cargo test\"}}\n\
             result read_file: {}\n[500 characters left out]\n{}\n\
             error run: 1 failed\n",
            "a".repeat(291),
            "h".repeat(1000),
            "t".repeat(1000),
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn a_signed_plan_keeps_no_real_messages() {
        let dialect = rune_net::responses::Responses;
        let mut history = History::new();
        history.push_user("first question");
        history.push_assistant_with_replay(
            vec![ContentPart::Text {
                text: "first answer".to_owned(),
            }],
            Some(r#"[{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"}]"#.to_owned()),
        );
        history.push_user("second question");
        history.push_assistant(vec![ContentPart::Text {
            text: "second answer".to_owned(),
        }]);

        assert!(!plan(&history, &keep_none()).expect("a plan").signed);
        let plan = plan_for(&history, &keep_none(), &dialect).expect("a plan");
        assert!(plan.signed);
        assert_eq!(plan.cut_at, 3);

        let removed = apply(&mut history, &plan, "what happened so far");
        assert_eq!(removed, 4);
        assert_eq!(history.len(), 1);
        assert!(!history.is_signed(&dialect));
        history.validate().expect("still valid");
        let text = user_text(&history);
        assert!(text.ends_with(
            "<user-messages>\n<message>\nfirst question\n</message>\n</user-messages>\n\
             <recent-context>\nuser: second question\nassistant: second answer\n</recent-context>\n\
             </conversation-checkpoint>"
        ));
    }
}
