//! The turn loop.
//!
//! Runs a conversation forward: build a request, stream the response, execute any
//! tool calls, and repeat until the model stops asking for tools or a bound is
//! reached. Every host interaction is a callback, so the loop itself owns no
//! product state and can be driven from the command line, from an editor, or
//! from a test.
//!
//! Two properties matter more than the rest:
//!
//! - A malformed tool call or a tool failure is information for the model, not
//!   an error that ends the turn. Only a policy denial, a cancellation, or an
//!   exhausted retry/objective budget or consecutive identical failure cap stops it.
//! - Finalization happens exactly once, so a session cannot record a turn twice.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::budget::{BudgetSet, LimitName, window_result_bytes};
use rune_core::config::{Effort, PermissionMode};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_net::compat::{EndpointCompat, ModelTraits};
use rune_net::fetch::Fetch;
use rune_net::message::{ContentPart, Message, ToolSpec};
use rune_net::provider::{Provider, RequestPlan, ToolChoice};
use rune_net::stream::{FinishReason, Usage};
use rune_net::transport::{self, Endpoint, StreamOutcome};
use rune_policy::decision::Outcome;
use rune_policy::rules::RuleSet;
use rune_tools::contract::{Activity, ExecutionContext, ToolOutput};
use rune_tools::{Registry, Tool};

use crate::history::History;
use crate::repair::{self, Resolved};
use crate::retry::{self, Decision, RetryPolicy, RetryState};
use crate::steering::{Boundary, Cancellation, SteeringQueue};
pub use crate::verify::Verification;
use crate::verify::{self, CheckSummary};

/// How a turn ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopReason {
    /// The model finished naturally.
    Completed,
    /// The model stopped asking for tools at the step limit.
    StepLimit,
    /// The active objective exhausted its accounted token budget.
    ObjectiveBudget,
    /// The response hit the output token limit.
    OutputLimit,
    /// The model declined.
    Refused,
    /// The caller cancelled.
    Cancelled,
    /// A content filter stopped the response.
    ContentFilter,
    /// The provider failed in a way that could not be recovered.
    ProviderFailure,
    /// The model kept sending a call that failed, after it was refused twice.
    Stalled,
    /// The run reached its wall-clock limit.
    TimeLimit,
    /// The run reached its spending limit.
    SpendLimit,
}

impl StopReason {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "end_turn",
            Self::StepLimit => "max_model_turns",
            Self::ObjectiveBudget => "objective_token_budget",
            Self::OutputLimit => "max_output_tokens",
            Self::Refused => "refused",
            Self::Cancelled => "cancelled",
            Self::ContentFilter => "content_filter",
            Self::ProviderFailure => "provider_error",
            Self::Stalled => "stalled",
            Self::TimeLimit => "time_limit",
            Self::SpendLimit => "spend_limit",
        }
    }

    /// Returns true when the turn produced an answer.
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Completed | Self::StepLimit)
    }
}

/// One tool call the model asked for, after decoding.
#[derive(Clone, Debug)]
pub struct PreparedCall {
    /// Identifier assigned by the model.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as a JSON string, exactly as the model produced them unless
    /// the call was repaired.
    pub arguments: String,
}

/// A pre-tool hook's named refusal. Hooks can only narrow existing authority.
#[derive(Clone, Debug)]
pub struct HookVeto {
    /// User-defined hook name.
    pub name: String,
    /// Explanation sent to the model and reported in the denial event.
    pub reason: String,
}

/// How a tool call was resolved.
#[derive(Clone, Debug)]
pub struct CallResult {
    /// The call.
    pub call: PreparedCall,
    /// What the tool returned, or an explanation of why it did not run.
    pub output: ToolOutput,
    /// Whether the call was allowed to run.
    pub executed: bool,
}

/// Everything the loop reports as it runs.
#[derive(Clone, Debug)]
pub enum Event {
    /// A turn began.
    TurnStarted {
        /// Step index, starting at one.
        step: u32,
    },
    /// A retryable provider failure scheduled another request attempt.
    ProviderRetry {
        /// Step index, starting at one.
        step: u32,
        /// Next request attempt, counting the initial request as one.
        next_attempt: usize,
        /// Maximum requests allowed for this step.
        max_attempts: usize,
        /// Wait before the next attempt, including a provider Retry-After.
        delay: Duration,
        /// When the next attempt is sent: now plus `delay`.
        retry_at: std::time::SystemTime,
        /// What failed, as the failure kind's wire name, such as `rate_limited`.
        kind: &'static str,
    },
    /// A retried provider request succeeded, before any tool decisions begin.
    ProviderRetryFinished {
        /// Step index, starting at one.
        step: u32,
    },
    /// A step's request failed and is being sent again.
    ///
    /// The retry streams its answer from the beginning, so the text and
    /// reasoning already reported for this step belong to the failed attempt
    /// and should be dropped.
    StepRestarted {
        /// Step index, starting at one.
        step: u32,
    },
    /// Assistant text arrived.
    TextDelta {
        /// The appended text.
        delta: String,
    },
    /// Reasoning text arrived.
    ReasoningDelta {
        /// The appended text.
        delta: String,
    },
    /// A tool call sent in a near-miss form was resolved to the call it meant.
    CallRepaired {
        /// Identifier assigned by the model.
        id: String,
        /// The tool name the model sent.
        from: String,
        /// The tool the call runs as.
        to: String,
        /// What was changed in the arguments, as short phrases.
        repairs: Vec<String>,
    },
    /// A tool call is about to run.
    ToolStarted {
        /// The call.
        call: PreparedCall,
        /// What the tool does.
        activity: Activity,
    },
    /// A tool call finished.
    ToolFinished {
        /// The call.
        call: PreparedCall,
        /// What the tool returned, before the turn bounds it for the model.
        output: ToolOutput,
        /// Time the tool spent executing.
        elapsed: Duration,
    },
    /// The live plan changed after an allowed tool call.
    PlanUpdated {
        /// Complete replacement snapshot with its monotonically increasing version.
        update: rune_tools::PlanUpdate,
    },
    /// A tool call was refused by policy, a pre-tool hook, or a turn guard.
    ToolDenied {
        /// The call.
        call: PreparedCall,
        /// The deciding rule, hook, or guard and its explanation.
        reason: String,
    },
    /// Steering was drained at a boundary.
    SteeringApplied {
        /// The boundary.
        boundary: Boundary,
        /// How many messages were applied.
        count: usize,
    },
    /// A line for the user about how the run is going, outside the reply.
    Notice {
        /// The text to show.
        text: String,
    },
    /// Old tool results were replaced by receipts before a model request.
    ContextPruned {
        /// Results replaced.
        results: usize,
        /// Estimated request tokens freed.
        tokens_freed: u64,
    },
    /// Earlier context was summarized before a model request.
    ContextCompacted {
        /// Original turns replaced by the summary.
        removed_turns: usize,
        /// Turns remaining, including the summary.
        remaining_turns: usize,
        /// Estimated request tokens before the compaction.
        before_tokens: u64,
        /// Estimated request tokens after it.
        after_tokens: u64,
        /// Whether it ran early, at a settled point, rather than at the ceiling.
        settled: bool,
    },
    /// A turn that changed files without checking them was asked to verify.
    VerificationRequested,
    /// The turn finished.
    Finished {
        /// How it ended.
        reason: StopReason,
        /// Token usage billed across every step of the turn.
        usage: Usage,
        /// Token usage of the turn's last request alone.
        last_request: Usage,
        /// Steps taken.
        steps: u32,
    },
}

/// Largest output ceiling a request asks for when the model's own is known.
///
/// A dialect that requires a ceiling falls back to a small one, which cuts a
/// long file write short. The model's published maximum is often far larger,
/// and the context estimate reserves whatever a request asks for, so the
/// ceiling is capped where it still leaves most of a window for input.
pub const MAX_REQUESTED_OUTPUT_TOKENS: u64 = 32 * 1024;

/// Everything the loop needs from its host.
///
/// The loop owns no product state: sessions, credentials, and presentation all
/// arrive through this trait, which is what lets the same loop serve the command
/// line, an editor, and a test.
pub trait Host {
    /// Returns a provider dialect for the active model.
    fn dialect(&self) -> &dyn Provider;

    /// Returns the endpoint to reach.
    fn endpoint(&self) -> &Endpoint;

    /// Returns the model identifier.
    ///
    /// Owned rather than borrowed because the model can be changed while a
    /// session is running, so it lives behind a lock and cannot be handed out
    /// as a reference. It is read once per request attempt, not per token.
    fn model(&self) -> String;

    /// Returns the system instructions.
    fn instructions(&self) -> String;

    /// Returns the tools available for this turn.
    ///
    /// Must return the same list, in the same order and with the same bytes,
    /// for every request of a session.
    fn tools(&self) -> Vec<ToolSpec>;

    /// Returns how many leading bytes of the instructions are the same in
    /// every workspace; zero when the instructions are not split.
    fn stable_instructions_len(&self) -> usize {
        0
    }

    /// Returns the registered tool's activity for event presentation.
    ///
    /// Hosts with a registry should return its declared activity. When none
    /// is supplied, built-in names retain their usual labels and other names
    /// are presented as executions.
    fn tool_activity(&self, _name: &str) -> Option<Activity> {
        None
    }

    /// Returns the permission target declared by the tool that will execute.
    ///
    /// Hosts with a registry should resolve through that registry, including
    /// returning `None` when the registered tool declares no target. Hosts
    /// without a registry retain the built-in target resolution by default.
    fn tool_permission_target(&self, name: &str, arguments: &serde_json::Value) -> Option<String> {
        infer_target(name, arguments)
    }

    /// Offers a thread-safe executor for a registered read-only tool.
    ///
    /// Returning `Some(self)` opts this call into native concurrent execution.
    /// Hosts must check the tool's read-only contract, rather than its name.
    /// Policy and events stay on the turn thread; only execution is delegated.
    /// Hosts without this capability and WebAssembly hosts execute in order.
    #[cfg(not(target_family = "wasm"))]
    fn concurrent_tool_host(&self, _name: &str) -> Option<&(dyn Host + Sync)> {
        None
    }

    /// Returns the reasoning effort to request.
    fn effort(&self) -> Effort {
        Effort::Auto
    }

    /// Returns the largest answer the model is known to produce, if known.
    fn max_output_tokens(&self) -> Option<u64> {
        None
    }

    /// Returns the model's context window in tokens, if known.
    ///
    /// A known window lowers the per-result cap to a share of it, so a model
    /// with a small window is not handed results that crowd out the rest.
    fn context_window(&self) -> Option<u64> {
        None
    }

    /// Returns the model that summarizes a conversation too large for its
    /// own model, when one is configured.
    fn compaction_model(&self) -> Option<String> {
        None
    }

    /// Returns the context window of the compaction model, if known.
    fn compaction_window(&self) -> Option<u64> {
        None
    }

    /// Returns the client this turn sends through.
    ///
    /// A native build defaults to the workspace transport, one per turn, which
    /// is what gives every step of that turn one connection pool.
    #[cfg(not(target_family = "wasm"))]
    fn fetch(&self) -> Box<dyn Fetch> {
        Box::new(transport::UreqFetch::new())
    }

    /// Returns the client this turn sends through.
    ///
    /// A target where the built-in client cannot exist has no default, so a
    /// host there supplies its own rather than the loop inventing one that
    /// cannot open a socket.
    #[cfg(target_family = "wasm")]
    fn fetch(&self) -> Box<dyn Fetch>;

    /// Returns whether fast mode is requested.
    fn fast_mode(&self) -> bool {
        false
    }

    /// Returns the upstream provider preference.
    fn provider_order(&self) -> Vec<String> {
        Vec::new()
    }

    /// Returns whether requests are restricted to that preference.
    fn provider_strict(&self) -> bool {
        false
    }

    /// Returns how requests for this model are shaped.
    fn compat(&self, _model: &str) -> (EndpointCompat, ModelTraits) {
        (EndpointCompat::default(), ModelTraits::default())
    }

    /// Prepares context immediately before sending a model request.
    ///
    /// A host may compact history and update the plan's messages. The default
    /// leaves both unchanged, so context policy remains the host's decision.
    fn prepare_request(
        &self,
        _history: &mut History,
        _plan: &mut RequestPlan,
        _client: &dyn Fetch,
    ) -> Result<()> {
        Ok(())
    }

    /// Observes a completed provider response before recording or executing it.
    /// Hosts may retain finish metadata needed by their output contract.
    fn observe_response(&self, _outcome: &StreamOutcome) {}

    /// Returns the request body `prepare_request` serialized, when it
    /// serialized the plan exactly as it left it, so the loop sends that text
    /// instead of serializing the whole conversation a second time.
    ///
    /// A host that changes the plan after serializing it returns `None`.
    fn take_encoded_request(&self) -> Option<String> {
        None
    }

    /// Compacts now, returning true when the history was shortened.
    ///
    /// The client is the turn's own, so the summary request goes the same way
    /// the rejected one did.
    fn compact_now(
        &self,
        _history: &mut History,
        _client: &dyn Fetch,
        _reason: CompactReason,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Called when `model` rejected a request of an estimated `tokens` input
    /// tokens as too long, so a host can remember the bound for later runs.
    fn observed_overflow(&self, _model: &str, _tokens: u64) {}

    /// Called after a summary is accepted and before it replaces earlier turns.
    fn before_compaction(&self, _history: &History) {}

    /// Called after earlier turns were replaced, before the request is rebuilt.
    ///
    /// The report carries the summary request's usage, which no other path
    /// records, so a host that counts usage adds it here.
    fn context_replaced(&self, _report: &crate::compaction::CompactionReport) {}

    /// Called with the usage of summary requests whose compaction failed, which
    /// no report carries, so the host can still count what they cost.
    fn compaction_abandoned(&self, _usage: &Usage) {}

    /// Returns what to append to this step's last tool result. Called once per step after
    /// every result is final. The default appends nothing.
    fn step_appendix(&self, _history: &History, _results: &[CallResult]) -> Option<Appendix> {
        None
    }

    /// Records delegated provider usage without adding child conversation events.
    fn record_child_usage(&self, _model: &str, _usage: &Usage) {}

    /// Reports an event.
    fn emit(&self, event: Event);

    /// Runs a user-authorized veto hook after all policy checks, before execution.
    ///
    /// Called once on the turn thread for each eligible call, including concurrent
    /// reads and retained-result reads. Returning a veto or an error prevents
    /// execution and preserves the matching error result. The default allows
    /// the call to continue. Hosts must bound external hook execution themselves.
    fn pre_tool(&self, _call: &PreparedCall) -> Result<Option<HookVeto>> {
        Ok(None)
    }

    /// Observes each finalized result once on the turn thread, in request order.
    ///
    /// Includes refused calls, concurrent reads, and retained-result reads after
    /// result budgets are applied. Hosts must redact data and bound external
    /// execution. This notification cannot change the tool result.
    fn post_tool(&self, _result: &CallResult) {}

    /// Executes a tool call, after policy has allowed it.
    fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput>;

    /// Resolves policy for a tool call.
    ///
    /// Returns the outcome and, for an explanation, the deciding rule.
    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String);

    /// Resolves policy for a call from the rules and mode alone, without asking a
    /// person or running a review. Used where the loop only needs to know whether
    /// something is already allowed.
    fn decide_quietly(&self, _name: &str, _target: Option<&str>) -> Outcome {
        Outcome::Ask
    }

    /// Resolves a call like [`Host::decide`], with its arguments in view, so
    /// an approval can show what the call would do. The arguments never
    /// change the scope that is approved.
    fn decide_with_arguments(
        &self,
        name: &str,
        target: Option<&str>,
        _arguments: &serde_json::Value,
    ) -> (Outcome, String) {
        self.decide(name, target)
    }

    /// Resolves a separate user decision for private-network access, including
    /// this fetch's redirects. A model review or full-access mode is insufficient.
    /// Hosts without a user approval path refuse it by default.
    fn decide_private_network(&self, _target: Option<&str>) -> (Outcome, String) {
        (
            Outcome::Deny,
            "private-network access requires user authority".to_owned(),
        )
    }

    /// Executes with the per-call authority resolved by the loop.
    /// Hosts running web or plan tools must pass this context to their implementation.
    fn execute_with_context(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        _context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        self.execute(name, arguments)
    }

    /// Returns the execution context for this turn.
    fn context(&self) -> ExecutionContext;

    /// Returns the limits in force.
    fn limits(&self) -> BudgetSet {
        BudgetSet::new()
    }

    /// Remaining input plus output tokens for an explicitly budgeted objective.
    /// `None` leaves ordinary conversations and unbudgeted objectives unlimited.
    fn objective_tokens_remaining(&self) -> Option<u64> {
        None
    }

    /// Durably charges one provider attempt, including attempts that failed.
    /// Called before another request can start. Persistence failure stops the turn.
    fn account_objective_tokens(&self, _tokens: u64) -> Result<()> {
        Ok(())
    }

    /// Returns the cancellation flag for this turn.
    fn cancellation(&self) -> Cancellation;

    /// Returns the steering queue for this turn.
    fn steering(&self) -> &SteeringQueue;

    /// Returns the retry policy for a step allowed `max_attempts` requests.
    fn retry_policy(&self, max_attempts: usize) -> RetryPolicy {
        RetryPolicy::interactive(max_attempts)
    }

    /// Returns true when a cancelled turn should end with
    /// [`StopReason::Cancelled`] and the work done so far, rather than with a
    /// `Cancelled` error.
    fn settles_cancellation(&self) -> bool {
        false
    }

    /// Called when one request attempt has ended, successfully or not, so a
    /// host can release what it tied to that attempt, such as a body reader
    /// still waiting on a connection the loop has stopped reading.
    fn attempt_finished(&self) {}

    /// Returns true when an oversized result is kept whole behind a handle
    /// that `read_tool_result` reads. A host that offers no such tool returns
    /// false, and its results are cut in place.
    fn retains_results(&self) -> bool {
        true
    }

    /// Returns parts recorded right after a call's result, such as images a
    /// host tool produced. Called once per result, in request order. The
    /// default has none.
    fn tool_attachments(&self, _result: &CallResult) -> Vec<ContentPart> {
        Vec::new()
    }

    /// Returns true when the model can see an image a tool attaches to its
    /// result. The default is false, so tools describe images instead.
    fn accepts_images(&self) -> bool {
        false
    }

    /// The provider's own time to wait for a reply's first byte, in
    /// milliseconds, over the `provider_head_timeout_ms` limit. None by default.
    fn provider_head_timeout_ms(&self) -> Option<u64> {
        None
    }

    /// The cache routing key every request of the session carries. None by
    /// default, so each request derives one from its own prefix.
    fn cache_key(&self) -> Option<String> {
        None
    }

    /// Whether a reply that ends on a question is told once that nobody can
    /// answer, as in a headless run. False by default.
    fn nudges_questions(&self) -> bool {
        false
    }

    /// The sampling temperature the user chose, over the model's default.
    /// None by default.
    fn temperature(&self) -> Option<f64> {
        None
    }

    /// Steps this run may still take, when its step limit is bounded. None by
    /// default.
    fn remaining_steps(&self) -> Option<u64> {
        None
    }

    /// The objective to check the result against, when a judge is on. None by
    /// default.
    fn goal_judge(&self) -> Option<String> {
        None
    }

    /// The effort a judge request asks for. `Auto` by default.
    fn lowest_effort(&self) -> Effort {
        Effort::Auto
    }

    /// Called with the usage of each judge request, which reaches no other
    /// per-request record. The turn's usage includes it as well.
    fn record_judge_usage(&self, _usage: &Usage) {}
}

/// The lowest effort both the model and the dialect accept, or `Auto`.
pub fn lowest_accepted_effort(host: &dyn Host) -> Effort {
    let (_, traits) = host.compat(&host.model());
    crate::compaction::lowest_effort(traits.efforts, host.dialect())
}

/// Text a host appends to the last tool result of a step, with keys marking what it
/// delivered so the history can forget the delivery when the turn carrying it is summarised.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Appendix {
    /// The text added after the result, following a blank line.
    pub text: String,
    /// What the text delivered, for `History::is_delivered`.
    pub keys: Vec<String>,
}

/// Renders changes for the model as one `<context-update>` block, one line each.
#[must_use]
pub fn context_update(lines: &[String]) -> String {
    let mut block = String::from("<context-update>\n");
    for line in lines {
        block.push_str(line);
        block.push('\n');
    }
    block.push_str("</context-update>");
    block
}

/// One entry of what a turn said and did, in the order it happened.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Narration {
    /// Text the model wrote between tool calls, or as its answer.
    Text(String),
    /// A tool call, by the identifier the model gave it.
    Call(String),
}

/// Outcome of a completed turn.
#[derive(Clone, Debug)]
pub struct TurnOutcome {
    /// How the turn ended.
    pub stop_reason: StopReason,
    /// Assistant text produced across every step.
    pub text: String,
    /// Text and tool calls across every step, in order.
    pub narration: Vec<Narration>,
    /// Reasoning the model produced, kept apart from its answer.
    ///
    /// Held separately so a reader can tell thinking from the reply, and so the
    /// two are never interleaved in one blob.
    pub reasoning: String,
    /// Usage billed across every step, summed because each step is a request
    /// of its own.
    pub usage: Usage,
    /// Usage of the last request alone.
    ///
    /// Its input is the whole conversation as last sent, which is how full the
    /// context window is. The summed usage counts that conversation once per
    /// step, so it cannot answer that question.
    pub last_request: Usage,
    /// Steps taken.
    pub steps: u32,
    /// Tool calls made, in order.
    pub calls: Vec<CallResult>,
    /// Calls that ran in a repaired form, in order.
    pub repairs: Vec<CallRepair>,
    /// How many calls were repaired.
    pub repaired_calls: u32,
    /// Whether a check ran after the turn's last change to a file.
    pub verification: Verification,
    /// The turn's changed files and its latest check after the last change.
    pub checks: CheckSummary,
    /// Why the turn stopped early, when it did. Usage, steps and text still
    /// describe everything that completed before the failure.
    pub error: Option<RuneError>,
    /// The goal judge's rounds and last verdict, when a judge ran.
    pub judge: Option<JudgeReport>,
}

/// What the goal judge did during a turn.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct JudgeReport {
    /// Judge requests made.
    pub rounds: u32,
    /// The last verdict: `done`, `continue` or `blocked`, or `failed` when
    /// the judge request itself failed.
    pub last_status: String,
    /// The reason given with the last verdict.
    pub last_reason: String,
    /// Usage of the judge requests, also counted in the turn's usage.
    pub usage: Usage,
}

/// A call that ran in a repaired form.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CallRepair {
    /// Identifier assigned by the model.
    pub id: String,
    /// The tool name the model sent.
    pub from: String,
    /// The tool the call ran as.
    pub to: String,
    /// What was changed in the arguments, as short phrases.
    pub repairs: Vec<String>,
}

impl CallRepair {
    fn new(id: &str, resolved: &Resolved) -> Self {
        Self {
            id: id.to_owned(),
            from: resolved
                .renamed_from
                .clone()
                .unwrap_or_else(|| resolved.name.clone()),
            to: resolved.name.clone(),
            repairs: resolved.repairs.clone(),
        }
    }
}

impl TurnOutcome {
    /// Returns the error as `Err` for hosts that end the exchange on any failure.
    pub fn into_strict(self) -> Result<Self> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self),
        }
    }
}

/// The stop reason a turn reports when it ends on `error`.
#[must_use]
pub fn stop_reason_for(error: &RuneError) -> StopReason {
    match (error.code(), error.detail().invariant.as_deref()) {
        (ErrorCode::Cancelled, _) => StopReason::Cancelled,
        (_, Some("objective_token_budget")) => StopReason::ObjectiveBudget,
        (_, Some("max_consecutive_tool_failures")) => StopReason::Stalled,
        (_, Some("session_spend_cents")) => StopReason::SpendLimit,
        (_, Some("run_timeout")) => StopReason::TimeLimit,
        _ => StopReason::ProviderFailure,
    }
}

/// Returns the command of a shell `run` call.
fn shell_run_command(arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    if value.get("action").and_then(serde_json::Value::as_str) != Some("run") {
        return None;
    }
    value
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(|command| command.trim().to_owned())
}

/// Asks the model to check a change before the turn ends.
///
/// Sent as a turn from the harness, and only once per configured reminder,
/// so a model that has a reason not to run anything can say so and finish.
const VERIFICATION_REMINDER: &str = "<harness_check>Files were changed and nothing has \
run since the last change to check it. Run the focused test, build, or command that \
exercises the change and report what it shows, or say why it cannot be run.</harness_check>";

/// Tells the model its last check after a change failed, once per turn.
const CHECK_FAILED_NOTE: &str = "<harness_check>The last check after your changes failed: \
`{command}` ({detail}). Fix it, or say why it cannot pass, before finishing.</harness_check>";

/// Asks for competing explanations when the work has stopped making progress.
const STALL_NOTE: &str = "<harness_check>The same check is still failing after {n} {changes}. \
Before changing more code: state two different explanations for the failure, then run one \
command that tells them apart.</harness_check>";

/// Fills the stall note for a number of changes.
fn stall_note(changes: usize) -> String {
    let noun = if changes == 1 { "change" } else { "changes" };
    STALL_NOTE
        .replace("{n}", &changes.to_string())
        .replace("{changes}", noun)
}

/// Appends the stall note to the first result of the batch that completes a
/// stall. Returns the bytes appended, or `None` when no result does.
fn note_stall(calls: &mut Vec<CallResult>, results: &mut Vec<CallResult>) -> Option<usize> {
    let base = calls.len();
    calls.append(results);
    let found = (base..calls.len())
        .find_map(|index| verify::stall_at(calls, index).map(|stall| (index, stall)));
    let appended = found.and_then(|(index, stall)| {
        let changes = match stall {
            verify::Stall::SameFailure { changes } | verify::Stall::SameFile { changes, .. } => {
                changes
            }
        };
        let note = format!("\n\n{}", stall_note(changes));
        let result = calls.get_mut(index)?;
        result.output.text.push_str(&note);
        Some(note.len())
    });
    *results = calls.split_off(base);
    appended
}

/// Longest command, in bytes, the failed-check note repeats.
const NOTE_COMMAND_BYTES: usize = 200;

/// Most failing test names the failed-check note lists.
const NOTE_FAILING_NAMES: usize = 5;

/// Fills the failed-check note for one check.
fn check_failed_note(record: &verify::CheckRecord) -> String {
    let mut command =
        rune_tools::workspace::truncate_to_bytes(&record.command, NOTE_COMMAND_BYTES).to_owned();
    if command.len() < record.command.len() {
        command.push_str("...");
    }
    let detail = if record.command.starts_with("syntax_diagnostics ") && record.exit_code.is_none()
    {
        "diagnostics reported".to_owned()
    } else {
        let mut detail = match record.exit_code {
            Some(code) => format!("exit {code}"),
            None => "no exit status".to_owned(),
        };
        if !record.failing.is_empty() {
            let names: Vec<&str> = record
                .failing
                .iter()
                .take(NOTE_FAILING_NAMES)
                .map(String::as_str)
                .collect();
            let _ = write!(detail, ", failing: {}", names.join(", "));
        }
        detail
    };
    CHECK_FAILED_NOTE
        .replace("{command}", &command)
        .replace("{detail}", &detail)
}

/// Why a host is asked to compact outside its own trigger.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompactReason {
    /// The provider rejected the request as longer than the context window.
    Overflow {
        /// Estimated input tokens of the rejected request.
        rejected_tokens: u64,
        /// Estimated input tokens the next request should stay under.
        target_tokens: u64,
    },
}

/// Share of the rejected request, in percent, that each successive overflow
/// compaction aims for.
const OVERFLOW_TARGETS: [u64; 4] = [85, 70, 50, 35];

/// Hint for a request the provider rejected as too long, once compacting
/// cannot or did not help.
pub const OVERFLOW_HINT: &str = "the conversation no longer fits the model's context window; start a new session or lower the size of tool results";

/// Tells the model the request it is answering is the last one the run allows.
pub const LAST_STEP_NOTE: &str = "<harness_check>This is the last step allowed in this run. \
Do not call tools. Say what is done, what was checked and how, and what remains.</harness_check>";

/// Answer recorded for a call the model made on the last allowed step.
const LAST_STEP_REFUSAL: &str = "not run: the step limit was reached";

/// Asks again after a reply the output limit cut off before it said anything.
const OUTPUT_LIMIT_RETRY: &str = "<harness_check>Your last reply reached the output \
token limit before giving any answer. Answer again more briefly: think less, and split \
large writes into smaller steps.</harness_check>";

/// Asks for the rest of a reply whose stream was cut before it ended.
const INTERRUPTED_CONTINUE: &str = "<harness_check>The previous response was interrupted. \
Continue from where it stopped without repeating what is already there.</harness_check>";

/// Cut streams continued in a row before the next cut is retried in full.
const MAX_CUT_CONTINUATIONS: u8 = 2;

/// Shown when a cut stream is kept and continued rather than sent again.
const CUT_NOTICE: &str = "the response was cut off; continuing from where it stopped";

/// Asks for the rest of a reply the output limit cut off.
const CONTINUE_NOTE: &str = "<harness_check>Your reply was cut at the output limit. Continue \
exactly where it stopped. Do not repeat or apologise. Write large files in smaller pieces.</harness_check>";

/// Answers a tool call whose arguments the output limit cut off.
const CUT_CALL_NOTE: &str = "<harness_check>Your tool call was cut at the output limit before \
its arguments were complete. Send a smaller call: split a large write into several edits.</harness_check>";

/// Consecutive continuations allowed before the turn stops at the output limit.
const MAX_CONTINUATIONS: u32 = 2;

/// Asks for a verdict on the objective from the evidence in the conversation.
const JUDGE_PROMPT: &str = "<goal_judge>\nObjective: {objective}\nHarness record: verification {verdict}{last_check}.\n\
Decide whether the objective is met. Only tool results in this conversation count as evidence; \
the assistant saying the work is done is not evidence. Reply with one JSON object and nothing else:\n\
{\"status\": \"done\" | \"continue\" | \"blocked\", \"reason\": \"one sentence\", \"next\": \"the next concrete step when status is continue\"}\n</goal_judge>";

/// Resumes the turn with the judge's next step.
const JUDGE_CONTINUE: &str =
    "<harness_check>The objective is not met yet: {reason} Next: {next}</harness_check>";

/// Judge requests allowed in one turn.
const JUDGE_MAX_ROUNDS: u32 = 20;

/// Identical reasons in a row after which the judge is no longer followed.
const JUDGE_SAME_REASON: u32 = 3;

/// Longest judge reason or next step carried into the conversation, in bytes.
const JUDGE_TEXT_BYTES: usize = 500;

/// Next step used when the judge asks to continue and names none.
const JUDGE_DEFAULT_NEXT: &str = "decide the next step from the evidence above.";

/// Reason recorded for a judge reply that does not decode.
const JUDGE_UNREADABLE: &str = "the judge reply could not be read";

/// Runs one turn to completion.
///
/// Returns the outcome; a failure to reach a terminal state is an error, not a
/// partial success.
pub fn run_turn(history: &mut History, host: &dyn Host) -> Result<TurnOutcome> {
    let limits = host.limits();
    let cancellation = host.cancellation();
    let steering = host.steering();
    let step_limit = limits.get(LimitName::MaxAgentSteps).value().unwrap_or(0);
    // One retry for a reply the output limit cut off before it said anything.
    let mut limit_retry = true;
    // Cut replies continued in a row, and the cut text the next step continues.
    let mut continuations: u32 = 0;
    let mut carried = String::new();
    // Cut streams kept and continued in a row.
    let mut cut_continuations: u8 = 0;
    let mut reminders = limits
        .get(LimitName::VerificationReminders)
        .value()
        .unwrap_or(0);
    // A failing last check is pointed out once; the harness does not insist.
    let mut failure_noted = false;
    // A stall is pointed out once per turn.
    let mut stall_noted = false;
    // A headless question is answered with the nudge once per turn.
    let mut question_nudged = false;
    let max_attempts = limits.get_usize(LimitName::ProviderMaxAttempts).max(1);
    let timeouts =
        transport::RequestTimeouts::from_settings(&limits, host.provider_head_timeout_ms());

    let instructions = host.instructions();
    history.set_instructions(instructions);
    crate::prune::unload_skills(history, host.dialect());
    history.validate()?;

    let client = host.fetch();
    let mut usage = Usage::default();
    let mut last_request = Usage::default();
    let mut calls = Vec::new();
    let mut repairs: Vec<CallRepair> = Vec::new();
    // Reasoning accumulated across steps, so a multi-step turn keeps all of it.
    let mut reasoning = String::new();
    let mut steps: u32 = 0;
    // Requests that returned a response, which is what a failed turn reports
    // as its steps: a step that failed is not counted.
    let mut completed: u32 = 0;
    // Tool-result bytes this turn has retained, shared across its steps.
    let mut result_bytes: usize = 0;
    // The latest step's answer, kept so a turn stopped at the step limit still
    // returns what the model said in its last step.
    let mut text = String::new();
    let mut narration: Vec<Narration> = Vec::new();
    let mut judge: Option<JudgeReport> = None;
    // The judge's last reason, normalized, and how many times in a row it came.
    let mut judge_reason = String::new();
    let mut judge_same: u32 = 0;

    // Ends the turn with what it has so far. The two-argument form ends it on
    // a failure, counting only the requests that completed.
    macro_rules! settle {
        ($reason:expr) => {{
            settle!(@end $reason, steps, None)
        }};
        ($reason:expr, $error:expr) => {{
            settle!(@end $reason, completed, Some($error))
        }};
        (@end $reason:expr, $steps:expr, $error:expr) => {{
            let stop_reason = $reason;
            let steps = $steps;
            host.emit(Event::Finished {
                reason: stop_reason,
                usage,
                last_request,
                steps,
            });
            return Ok(TurnOutcome {
                stop_reason,
                text,
                narration,
                reasoning,
                usage,
                last_request,
                steps,
                verification: Verification::of(&calls),
                checks: verify::summarize(&calls, stall_noted),
                calls,
                repaired_calls: u32::try_from(repairs.len()).unwrap_or(u32::MAX),
                repairs,
                error: $error,
                judge,
            });
        }};
    }
    // A failure before any request completed is still an error; after one,
    // the turn keeps what it has.
    macro_rules! fail {
        ($error:expr) => {{
            let error = $error;
            if completed == 0 {
                return Err(error);
            }
            settle!(stop_reason_for(&error), error)
        }};
    }

    loop {
        if host.settles_cancellation() && cancellation.is_cancelled() {
            settle!(StopReason::Cancelled);
        }
        if let Err(err) = cancellation.check() {
            fail!(err);
        }

        // A step limit of zero means unlimited, which is the documented default.
        let stop_reason = if host.objective_tokens_remaining() == Some(0) {
            Some(StopReason::ObjectiveBudget)
        } else if step_limit > 0 && u64::from(steps) >= step_limit {
            Some(StopReason::StepLimit)
        } else {
            None
        };
        if let Some(stop_reason) = stop_reason {
            let outcome = TurnOutcome {
                stop_reason,
                text,
                narration,
                reasoning,
                usage,
                last_request,
                steps,
                verification: Verification::of(&calls),
                checks: verify::summarize(&calls, stall_noted),
                calls,
                repaired_calls: u32::try_from(repairs.len()).unwrap_or(u32::MAX),
                repairs,
                error: None,
                judge,
            };
            host.emit(Event::Finished {
                reason: stop_reason,
                usage,
                last_request,
                steps,
            });
            return Ok(outcome);
        }

        // Drain steering before building the request, so the model sees the
        // latest guidance in the same turn rather than the next one.
        apply_steering(history, steering, Boundary::Model, host);

        let last_step = step_limit > 0
            && u64::from(steps).saturating_add(1) == step_limit
            && !host.tools().is_empty();
        if last_step {
            history.push_harness_note(LAST_STEP_NOTE);
        }

        steps = steps.saturating_add(1);
        host.emit(Event::TurnStarted { step: steps });

        let (outcome, steering_arrived, measure) = match stream_with_retry(
            client.as_ref(),
            host,
            history,
            steps,
            last_step,
            timeouts,
            max_attempts,
            &cancellation,
            &mut cut_continuations,
            None,
            None,
            false,
        ) {
            Ok(streamed) => {
                completed = completed.saturating_add(1);
                streamed
            }
            // Once the turn is cancelled, whatever the stream failed with is
            // the cancellation's doing.
            Err(err)
                if host.settles_cancellation()
                    && (err.code() == ErrorCode::Cancelled || cancellation.is_cancelled()) =>
            {
                // The step cut off by the cancellation is not counted, as a
                // failed step is not: steps are requests that completed.
                settle!(@end StopReason::Cancelled, completed, None)
            }
            Err(err) => fail!(err),
        };

        host.observe_response(&outcome);
        usage = usage.merge_sum(outcome.usage);
        last_request = outcome.usage;
        if let Some(input_tokens) = outcome.usage.input_tokens
            && input_tokens > 0
        {
            history.set_anchor(crate::tokens::Anchor {
                dialect: host.dialect().name().to_owned(),
                model: host.model(),
                input_tokens,
                measure,
            });
        }

        text = outcome.text();
        if !carried.is_empty() {
            text.insert_str(0, &carried);
            carried.clear();
        }
        // Reasoning is accumulated across steps, so a turn that used several
        // ends up with all of its thinking rather than only the last step's.
        let step_reasoning = outcome.reasoning();
        if !step_reasoning.trim().is_empty() {
            if !reasoning.is_empty() {
                reasoning.push('\n');
            }
            reasoning.push_str(&step_reasoning);
        }
        let tool_calls = outcome.tool_calls();
        let finish = outcome.finish.unwrap_or(FinishReason::Stop);
        if finish != FinishReason::MaxTokens {
            continuations = 0;
        }
        if !outcome.interrupted {
            cut_continuations = 0;
        }

        // Record the assistant turn, including any replay state the provider
        // requires on the next request.
        let mut parts: Vec<ContentPart> = Vec::new();
        let mut pending_calls = Vec::new();
        // Text from a new step starts a new entry, even with no call between.
        let mut new_step = true;
        for event in &outcome.events {
            match event {
                // The host already received this delta as it arrived; here it is
                // only assembled into the turn's record.
                rune_net::stream::ProviderEvent::TextDelta { delta } => {
                    match narration.last_mut() {
                        Some(Narration::Text(said)) if !new_step => said.push_str(delta),
                        _ => narration.push(Narration::Text(delta.clone())),
                    }
                    new_step = false;
                    parts.push(ContentPart::Text {
                        text: delta.clone(),
                    });
                }
                // Reasoning is assembled too, so it outlives the turn. A
                // provider that requires its own reasoning replayed would
                // otherwise be sent a conversation with the thinking removed.
                rune_net::stream::ProviderEvent::ReasoningDelta { delta } => {
                    parts.push(ContentPart::Reasoning {
                        text: delta.clone(),
                    });
                }
                rune_net::stream::ProviderEvent::ToolCallEnd { id, arguments } => {
                    let name = tool_calls
                        .iter()
                        .find(|(call_id, _, _)| call_id == &id.to_string())
                        .map(|(_, name, _)| name.clone())
                        .unwrap_or_default();
                    parts.push(ContentPart::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    narration.push(Narration::Call(id.to_string()));
                    pending_calls.push(PreparedCall {
                        id: id.to_string(),
                        name,
                        arguments: arguments.clone(),
                    });
                }
                _ => {}
            }
        }

        // Merge adjacent text parts so the transcript has one entry per run.
        let parts = coalesce_text(parts);
        let recorded = !parts.is_empty() || !pending_calls.is_empty();
        if recorded {
            // The replay value travels with the turn that produced it, because
            // a later request must send it back unchanged and must not inherit
            // it from an earlier turn.
            history.push_assistant_with_replay(
                parts,
                outcome.replay.clone(),
                Some(request_origin(host)),
            );
        }

        if pending_calls.is_empty() {
            // A reply whose stream was cut is kept and continued, and the next
            // step's text is joined to it.
            if outcome.interrupted && !last_step && host.objective_tokens_remaining() != Some(0) {
                carried = text.clone();
                history.push_user(INTERRUPTED_CONTINUE);
                continue;
            }
            // Steering that arrived while the last request was streaming gets a
            // response in this turn rather than the next one. Ending here would
            // drop it: the loop returns without reaching another drain, and a
            // correction typed against the answer being written would never be
            // seen. Continuing is what makes the correction feel delivered.
            //
            // It is drained here rather than left for the next model boundary,
            // so it is recorded and reported once, at the boundary it actually
            // arrived at.
            if !last_step && !steering.is_empty() && host.objective_tokens_remaining() != Some(0) {
                apply_steering(history, steering, Boundary::Finalizing, host);
                continue;
            }
            // A reply cut off before any answer is asked for once more, shorter.
            // An assistant turn is recorded first, so the roles still alternate.
            if !last_step
                && finish == FinishReason::MaxTokens
                && limit_retry
                && text.trim().is_empty()
                && host.objective_tokens_remaining() != Some(0)
            {
                limit_retry = false;
                if !recorded {
                    history.push_assistant(vec![ContentPart::Text {
                        text: "[no reply: the output limit was reached]".to_owned(),
                    }]);
                }
                history.push_user(OUTPUT_LIMIT_RETRY);
                continue;
            }
            // A reply cut off after it said something is continued where it
            // stopped, and the next step's text is joined to it.
            if !last_step
                && finish == FinishReason::MaxTokens
                && continuations < MAX_CONTINUATIONS
                && !text.trim().is_empty()
                && host.objective_tokens_remaining() != Some(0)
            {
                continuations = continuations.saturating_add(1);
                carried = text.clone();
                history.push_harness_note(CONTINUE_NOTE);
                continue;
            }
            // A turn that changed files and stopped of its own accord is told
            // once when its last check failed, and asked once to check work
            // nothing checked. No extra request is spent on a turn that
            // checked, changed nothing, or ended for another reason.
            // A headless reply that ends on a question is told once that
            // nobody will answer, before the other finish notes.
            if !last_step
                && finish == FinishReason::Stop
                && !question_nudged
                && host.nudges_questions()
                && text.trim_end().ends_with('?')
                && host.objective_tokens_remaining() != Some(0)
            {
                question_nudged = true;
                history.push_harness_note(QUESTION_NUDGE);
                continue;
            }
            if !last_step
                && finish == FinishReason::Stop
                && host.objective_tokens_remaining() != Some(0)
            {
                let verdict = Verification::of(&calls);
                if verdict == Verification::Failed
                    && !failure_noted
                    && let Some(record) = verify::summarize(&calls, stall_noted).last_check
                {
                    failure_noted = true;
                    history.push_harness_note(&check_failed_note(&record));
                    host.emit(Event::VerificationRequested);
                    continue;
                }
                if verdict == Verification::Unverified && reminders > 0 {
                    reminders = reminders.saturating_sub(1);
                    history.push_user(VERIFICATION_REMINDER);
                    host.emit(Event::VerificationRequested);
                    continue;
                }
            }
            let reason = if host.objective_tokens_remaining() == Some(0) {
                StopReason::ObjectiveBudget
            } else {
                match finish {
                    FinishReason::Stop | FinishReason::ToolCalls if last_step => {
                        StopReason::StepLimit
                    }
                    FinishReason::Stop => StopReason::Completed,
                    FinishReason::MaxTokens => StopReason::OutputLimit,
                    FinishReason::Refused => StopReason::Refused,
                    FinishReason::ContentFilter => StopReason::ContentFilter,
                    FinishReason::Cancelled => StopReason::Cancelled,
                    FinishReason::ToolCalls => StopReason::Completed,
                    FinishReason::MaxModelTurns => StopReason::StepLimit,
                    FinishReason::ProviderError => StopReason::ProviderFailure,
                }
            };
            // The judge sees a turn that is about to end by itself. It never
            // runs on the last step the run allows.
            if reason == StopReason::Completed
                && (step_limit == 0 || u64::from(steps) < step_limit)
                && let Some(objective) = host.goal_judge()
            {
                let prompt = judge_prompt(&objective, &calls, stall_noted);
                let (verdict, spent) = match ask_judge(
                    client.as_ref(),
                    host,
                    history,
                    &prompt,
                    steps,
                    timeouts,
                    max_attempts,
                    &cancellation,
                ) {
                    Ok(judged) => judged,
                    Err(err)
                        if err.code() == ErrorCode::Cancelled || cancellation.is_cancelled() =>
                    {
                        if host.settles_cancellation() {
                            settle!(@end StopReason::Cancelled, completed, None)
                        }
                        fail!(err)
                    }
                    Err(err) => (
                        JudgeVerdict {
                            status: "failed".to_owned(),
                            reason: format!("the judge request failed: {}", err.message()),
                            next: String::new(),
                        },
                        Usage::default(),
                    ),
                };
                usage = usage.merge_sum(spent);
                host.record_judge_usage(&spent);
                let key = verdict.reason.trim().to_lowercase();
                judge_same = if key == judge_reason {
                    judge_same.saturating_add(1)
                } else {
                    1
                };
                judge_reason = key;
                let report = judge.get_or_insert_with(JudgeReport::default);
                report.rounds = report.rounds.saturating_add(1);
                report.usage = report.usage.merge_sum(spent);
                report.last_status.clone_from(&verdict.status);
                report.last_reason.clone_from(&verdict.reason);
                if verdict.status == "continue"
                    && report.rounds < JUDGE_MAX_ROUNDS
                    && judge_same < JUDGE_SAME_REASON
                {
                    history.push_harness_note(&judge_continue_note(&verdict.reason, &verdict.next));
                    continue;
                }
                // Steering that arrived during the judge request would
                // otherwise be dropped with the turn.
                if !steering.is_empty() && host.objective_tokens_remaining() != Some(0) {
                    apply_steering(history, steering, Boundary::Finalizing, host);
                    continue;
                }
            }
            host.emit(Event::Finished {
                reason,
                usage,
                last_request,
                steps,
            });
            return Ok(TurnOutcome {
                stop_reason: reason,
                text,
                narration,
                reasoning,
                usage,
                last_request,
                steps,
                verification: Verification::of(&calls),
                checks: verify::summarize(&calls, stall_noted),
                calls,
                repaired_calls: u32::try_from(repairs.len()).unwrap_or(u32::MAX),
                repairs,
                error: None,
                judge,
            });
        }

        // Execute the batch. Policy is resolved per call before anything runs,
        // so a denial never reaches a tool. On the last step nothing runs: each
        // call is answered as refused so the record stays well formed.
        let mut repaired: Vec<(String, Resolved)> = Vec::new();
        // A call whose arguments the output limit cut off is answered, not run.
        let cut: Vec<String> = if finish == FinishReason::MaxTokens {
            pending_calls
                .iter()
                .filter(|call| {
                    !serde_json::from_str::<serde_json::Value>(&call.arguments)
                        .is_ok_and(|value| value.is_object())
                })
                .map(|call| call.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        let (mut results, stalled) = if last_step {
            let refused = pending_calls
                .iter()
                .map(|call| {
                    host.emit(Event::ToolDenied {
                        call: call.clone(),
                        reason: LAST_STEP_REFUSAL.to_owned(),
                    });
                    CallResult {
                        call: call.clone(),
                        output: ToolOutput::failure(LAST_STEP_REFUSAL),
                        executed: false,
                    }
                })
                .collect();
            (refused, None)
        } else {
            execute_batch(
                &pending_calls,
                &cut,
                host,
                history,
                &mut result_bytes,
                &limits,
                &calls,
                &mut repaired,
            )
        };

        // The next request shows a repaired call as it ran, next to its
        // result. A call beside signed reasoning is left as the model sent
        // it, because changing it can invalidate the signature; its result
        // keeps the recorded name and the note says what ran.
        let signed = matches!(host.dialect().name(), "anthropic" | "responses" | "gemini")
            && outcome
                .replay
                .as_deref()
                .is_some_and(|replay| !replay.is_empty());
        let mut recorded_names = std::collections::HashMap::new();
        for (id, resolved) in &repaired {
            if signed || resolved.intercepted {
                if let Some(from) = &resolved.renamed_from {
                    recorded_names.insert(id.as_str(), from.clone());
                }
            } else {
                history.rewrite_tool_call(id, &resolved.name, &resolved.arguments.to_string());
            }
            if !resolved.intercepted {
                repairs.push(CallRepair::new(id, resolved));
            }
        }

        if !stall_noted && let Some(appended) = note_stall(&mut calls, &mut results) {
            result_bytes = result_bytes.saturating_add(appended);
            stall_noted = true;
        }

        for result in &results {
            host.post_tool(result);
        }
        let appendix = host
            .step_appendix(history, &results)
            .filter(|appendix| !appendix.text.is_empty());
        if let Some(appendix) = &appendix
            && let Some(last) = results.last_mut()
        {
            last.output.text.push_str("\n\n");
            last.output.text.push_str(&appendix.text);
        }
        let mut result_parts = Vec::new();
        for result in &results {
            let id = match rune_core::id::ToolCallId::new(result.call.id.clone()) {
                Ok(id) => id,
                Err(err) => fail!(err),
            };
            result_parts.push(ContentPart::ToolResult {
                id,
                name: recorded_names
                    .get(result.call.id.as_str())
                    .cloned()
                    .unwrap_or_else(|| result.call.name.clone()),
                content: result.output.render(),
                is_error: result.output.is_error,
            });
            result_parts.extend(host.tool_attachments(result));
        }
        let mut settled_now = false;
        for result in &results {
            let was_settled = history.ledger().settled;
            history.ledger_mut().record(result);
            settled_now |= !was_settled && history.ledger().settled;
        }
        let seq = history.push_tool_results(result_parts);
        if let Some(appendix) = appendix {
            for key in appendix.keys {
                history.mark_delivered(key, seq);
            }
        }
        if settled_now {
            history.set_settled_turn(seq);
        }
        calls.extend(results);

        if last_step {
            settle!(StopReason::StepLimit);
        }

        if let Some((_, message)) = stalled {
            settle!(
                StopReason::Stalled,
                RuneError::new(ErrorCode::LimitExceeded, message)
                    .with_invariant(LimitName::MaxConsecutiveToolFailures.as_str())
            );
        }

        if !cut.is_empty() {
            continuations = continuations.saturating_add(1);
            if continuations > MAX_CONTINUATIONS {
                settle!(StopReason::OutputLimit);
            }
        }

        // A provider error with tool calls in flight still ends the turn after
        // the results are recorded, so nothing is lost.
        if finish == FinishReason::ProviderError {
            host.emit(Event::Finished {
                reason: StopReason::ProviderFailure,
                usage,
                last_request,
                steps,
            });
            return Ok(TurnOutcome {
                stop_reason: StopReason::ProviderFailure,
                text,
                narration,
                reasoning,
                usage,
                last_request,
                steps,
                verification: Verification::of(&calls),
                checks: verify::summarize(&calls, stall_noted),
                calls,
                repaired_calls: u32::try_from(repairs.len()).unwrap_or(u32::MAX),
                repairs,
                error: None,
                judge,
            });
        }

        // Drain steering that arrived during the successful request, then
        // continue so the next step sees it.
        if steering_arrived {
            apply_steering(history, steering, Boundary::AfterFailure, host);
        }
    }
}

/// Refuses another provider request once the objective budget is exhausted.
pub fn check_objective_budget(host: &dyn Host) -> Result<()> {
    if host.objective_tokens_remaining() == Some(0) {
        return Err(RuneError::new(
            ErrorCode::LimitExceeded,
            "the active objective exhausted its token budget",
        )
        .with_invariant("objective_token_budget"));
    }
    Ok(())
}

/// Accounting for one provider attempt, shared with context summarization.
/// Missing counts use rounded-up request/output bytes divided by four. Cache
/// details are added only for dialects that exclude them from input totals;
/// reasoning details already included in output are not added again.
#[derive(Debug)]
pub struct ObjectiveAccounting {
    input_estimate: Option<u64>,
    output_bytes: u64,
    argument_bytes: std::collections::BTreeMap<rune_core::id::ToolCallId, u64>,
    usage: Usage,
}

impl ObjectiveAccounting {
    /// Checks the budget and narrows an existing output ceiling when applicable.
    pub fn begin(host: &dyn Host, plan: &mut RequestPlan) -> Result<Self> {
        check_objective_budget(host)?;
        let input_estimate = if let Some(remaining) = host.objective_tokens_remaining() {
            host.dialect().validate(plan)?;
            let body = host.dialect().build_request(plan)?;
            let ceiling = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
                .iter()
                .find_map(|key| body.get(key).and_then(serde_json::Value::as_u64));
            plan.max_output_tokens = Some(
                plan.max_output_tokens
                    .or(ceiling)
                    .map_or(remaining, |cap| cap.min(remaining)),
            );
            let body = host.dialect().build_request(plan)?;
            let bytes = serde_json::to_vec(&body)
                .map_err(|err| RuneError::new(ErrorCode::Internal, err.to_string()))?
                .len() as u64;
            Some(
                bytes
                    .div_ceil(crate::tokens::DEFAULT_BYTES_PER_TOKEN)
                    .max(1),
            )
        } else {
            None
        };
        Ok(Self {
            input_estimate,
            output_bytes: 0,
            argument_bytes: std::collections::BTreeMap::new(),
            usage: Usage::default(),
        })
    }

    /// Tracks partial counts even when the transport later fails or is cancelled.
    pub fn observe(&mut self, event: &rune_net::stream::ProviderEvent) {
        use rune_net::stream::ProviderEvent;
        if self.input_estimate.is_none() {
            return;
        }
        match event {
            ProviderEvent::Usage(usage) => self.usage = self.usage.merge_max(*usage),
            ProviderEvent::TextDelta { delta } | ProviderEvent::ReasoningDelta { delta } => {
                self.output_bytes = self.output_bytes.saturating_add(delta.len() as u64);
            }
            ProviderEvent::ToolCallStart { name, .. } => {
                self.output_bytes = self.output_bytes.saturating_add(name.len() as u64);
            }
            ProviderEvent::ToolCallDelta { id, delta } => {
                let bytes = self.argument_bytes.entry(id.clone()).or_default();
                *bytes = bytes.saturating_add(delta.len() as u64);
                self.output_bytes = self.output_bytes.saturating_add(delta.len() as u64);
            }
            ProviderEvent::ToolCallEnd { id, arguments } => {
                let streamed = self.argument_bytes.remove(id).unwrap_or(0);
                self.output_bytes = self
                    .output_bytes
                    .saturating_add((arguments.len() as u64).saturating_sub(streamed));
            }
            _ => {}
        }
    }

    /// Persists the charge before the next request or tool execution.
    pub fn finish(self, host: &dyn Host, reported: Option<Usage>) -> Result<()> {
        // An offline refusal never reached a provider and consumes no tokens.
        // A loopback server is still reached offline, so its use is counted.
        if host.endpoint().offline && !transport::is_loopback_url(&host.endpoint().base_url) {
            return Ok(());
        }
        let Some(input_estimate) = self.input_estimate else {
            return Ok(());
        };
        let usage = self.usage.merge_max(reported.unwrap_or_default());
        // Every dialect reports the whole prompt as input, cached parts
        // included, so the cache counts are not added again.
        let input = usage.input_tokens.unwrap_or(input_estimate);
        // Every dialect includes reasoning in its output count.
        let output = usage.output_tokens.unwrap_or_else(|| {
            self.output_bytes
                .div_ceil(crate::tokens::DEFAULT_BYTES_PER_TOKEN)
        });
        host.account_objective_tokens(input.saturating_add(output))
    }
}

/// Returns what can be kept from a stream cut before it ended.
///
/// Complete calls are kept and the calls that never ended are dropped. A cut
/// with neither a complete call nor any text keeps nothing, so the request is
/// retried.
fn kept_from_cut(err: &rune_net::error::NetError) -> Option<StreamOutcome> {
    use rune_net::error::FailureKind;
    use rune_net::stream::ProviderEvent;

    if !matches!(
        err.kind(),
        FailureKind::IncompleteStream
            | FailureKind::Network
            | FailureKind::Timeout
            | FailureKind::Unreachable
    ) {
        return None;
    }
    let partial = err.partial()?;
    let ended: Vec<&rune_core::id::ToolCallId> = partial
        .events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ToolCallEnd { id, .. } => Some(id),
            _ => None,
        })
        .collect();
    let events: Vec<ProviderEvent> = partial
        .events
        .iter()
        .filter(|event| match event {
            ProviderEvent::ToolCallStart { id, .. } | ProviderEvent::ToolCallDelta { id, .. } => {
                ended.contains(&id)
            }
            _ => true,
        })
        .cloned()
        .collect();
    let finish = if !ended.is_empty() {
        FinishReason::ToolCalls
    } else if !partial.text().trim().is_empty() {
        FinishReason::Stop
    } else {
        return None;
    };
    Some(StreamOutcome {
        events,
        usage: partial.usage,
        replay: partial.replay.clone(),
        finish: Some(finish),
        interrupted: true,
    })
}

/// The `dialect:model` a request goes to, recorded with the turn it produces.
fn request_origin(host: &dyn Host) -> String {
    format!("{}:{}", host.dialect().name(), host.model())
}

/// Builds the next request from `history` as the loop sends it, before the
/// host prepares it.
pub(crate) fn request_plan(host: &dyn Host, history: &mut History) -> RequestPlan {
    let mut plan = RequestPlan::new(host.model());
    plan.instructions = host.instructions();
    plan.stable_instructions_len = host.stable_instructions_len();
    plan.messages = history.to_messages();
    (plan.compat, plan.traits) = host.compat(&plan.model);
    plan.origin = request_origin(host);
    plan.tools = host.tools();
    plan.tool_choice = if plan.tools.is_empty() {
        ToolChoice::None
    } else {
        ToolChoice::Auto
    };
    plan.parallel_tool_calls = host.limits().get(LimitName::ParallelToolCalls).value() != Some(1);
    plan.effort = host.effort();
    let temperature = host.temperature();
    if rune_net::compat::accepts_sampling(&plan.traits, plan.effort, &plan.compat) {
        plan.sampling = plan.traits.sampling;
        if temperature.is_some() {
            plan.sampling.temperature = temperature;
        }
    } else if temperature.is_some() && history.notice_temperature_once() {
        host.emit(Event::Notice {
            text: format!(
                "temperature is not sent to {}: it does not accept one",
                plan.model
            ),
        });
    }
    if host.dialect().capabilities().requires_output_ceiling {
        plan.max_output_tokens = host
            .max_output_tokens()
            .map(|known| known.min(MAX_REQUESTED_OUTPUT_TOKENS));
    }
    plan.fast_mode = host.fast_mode();
    plan.provider_order = host.provider_order();
    plan.provider_strict = host.provider_strict();
    plan.cache_key = host.cache_key();
    plan.reasoning_summary = !history.reasoning_summary_refused();
    plan
}

/// Streams a request, retrying a transient failure within a bounded budget.
///
/// Applies queued steering before rebuilding a retry request. Returns the
/// outcome together with a flag saying whether steering is queued after the
/// successful attempt, for the caller to apply at the next boundary, and the
/// measure of the request that succeeded.
///
/// `extra_user` is added to the request only, never to `history`, with tools
/// left listed but not callable. `quiet` hides the request from the host's
/// stream: the answer already shown stays, and steering waits for the turn.
#[allow(clippy::too_many_arguments)]
fn stream_with_retry(
    client: &dyn Fetch,
    host: &dyn Host,
    history: &mut History,
    step: u32,
    last_step: bool,
    timeouts: transport::RequestTimeouts,
    max_attempts: usize,
    cancellation: &Cancellation,
    cut_continuations: &mut u8,
    extra_user: Option<&str>,
    effort_override: Option<Effort>,
    quiet: bool,
) -> Result<(StreamOutcome, bool, crate::tokens::Measure)> {
    let steering = host.steering();
    let mut attempt = 0_usize;
    let retry_policy = host.retry_policy(max_attempts);
    let mut retry_state = RetryState::default();
    // Resends after compacting for an overflow, which do not use up the
    // provider attempts: the request changed, it did not fail transiently.
    let mut overflow_compactions = 0_usize;

    loop {
        cancellation.check()?;

        check_objective_budget(host)?;
        if attempt > 0 && !quiet {
            host.emit(Event::StepRestarted { step });
            // Include corrections from the failed stream or retry delay before
            // constructing the plan and preparing its context.
            apply_steering(history, steering, Boundary::AfterFailure, host);
        }
        attempt = attempt.saturating_add(1);

        let mut plan = request_plan(host, history);
        if last_step {
            plan.tool_choice = ToolChoice::None;
        }
        host.prepare_request(history, &mut plan, client)?;
        let encoded = host.take_encoded_request();
        cancellation.check()?;
        // Added after preparation, which can rebuild the messages.
        if let Some(text) = extra_user {
            plan.messages.push(Message::user(text));
            plan.tool_choice = ToolChoice::None;
        }
        if let Some(effort) = effort_override {
            plan.effort = effort;
            if !rune_net::compat::accepts_sampling(&plan.traits, plan.effort, &plan.compat) {
                plan.sampling = rune_net::compat::Sampling::default();
            }
        }
        // An active objective caps the output tokens here, and an extra
        // message changes the request, so the host's serialization no longer
        // matches it.
        let encoded =
            encoded.filter(|_| extra_user.is_none() && host.objective_tokens_remaining().is_none());
        let mut accounting = ObjectiveAccounting::begin(host, &mut plan)?;
        let measure = match &encoded {
            Some(body) => crate::tokens::measure_body(body.len() as u64, &plan),
            None => crate::tokens::measure(host.dialect(), &plan)?,
        };

        // Each event is handed to the host as it is decoded, so text appears
        // while it is being produced rather than once the response has ended.
        // A retry re-sends the request, so the host is told the step restarted
        // and the partial text of the failed attempt is dropped.
        let mut observe = |event: &rune_net::stream::ProviderEvent| {
            accounting.observe(event);
            match event {
                rune_net::stream::ProviderEvent::TextDelta { delta } if !quiet => {
                    host.emit(Event::TextDelta {
                        delta: delta.clone(),
                    });
                }
                rune_net::stream::ProviderEvent::ReasoningDelta { delta } if !quiet => {
                    host.emit(Event::ReasoningDelta {
                        delta: delta.clone(),
                    });
                }
                _ => {}
            }
        };

        let response = transport::stream_completion_encoded(
            client,
            host.endpoint(),
            host.dialect(),
            &plan,
            encoded,
            timeouts,
            &|| cancellation.is_cancelled(),
            &mut observe,
        );
        host.attempt_finished();
        // What a cut stream produced is kept rather than produced again, a
        // limited number of times in a row.
        let response = match response {
            Err(err)
                if *cut_continuations < MAX_CUT_CONTINUATIONS && !cancellation.is_cancelled() =>
            {
                match kept_from_cut(&err) {
                    Some(kept) => {
                        *cut_continuations = cut_continuations.saturating_add(1);
                        host.emit(Event::Notice {
                            text: CUT_NOTICE.to_owned(),
                        });
                        Ok(kept)
                    }
                    None => Err(err),
                }
            }
            other => other,
        };
        accounting.finish(host, response.as_ref().ok().map(|outcome| outcome.usage))?;
        match response {
            Ok(outcome) => {
                if attempt > 1 {
                    host.emit(Event::ProviderRetryFinished { step });
                }
                return Ok((outcome, !steering.is_empty(), measure));
            }
            Err(err) => {
                if plan.reasoning_summary && summary_refused(&err) {
                    // Sent again at once without summaries, which does not
                    // use up a provider attempt.
                    history.refuse_reasoning_summary();
                    host.emit(Event::Notice {
                        text: SUMMARY_REFUSED_NOTICE.to_owned(),
                    });
                    attempt = attempt.saturating_sub(1);
                    continue;
                }
                if err.kind() == rune_net::error::FailureKind::ContextTooLong {
                    if let Some(percent) = OVERFLOW_TARGETS.get(overflow_compactions).copied() {
                        let model = host.model();
                        let (rejected_tokens, _) = crate::tokens::estimate_input(
                            measure,
                            history.anchor(),
                            host.dialect().name(),
                            &plan.model,
                        );
                        let target_tokens = rejected_tokens.saturating_mul(percent) / 100;
                        history.learn_window_bound(&model, rejected_tokens);
                        host.observed_overflow(&model, rejected_tokens);
                        let reason = CompactReason::Overflow {
                            rejected_tokens,
                            target_tokens,
                        };
                        if host.compact_now(history, client, reason)? {
                            // The loop top reports the restart and rebuilds the
                            // request from the shortened history.
                            overflow_compactions = overflow_compactions.saturating_add(1);
                            continue;
                        }
                    }
                    return Err(err.to_rune_error().with_hint(OVERFLOW_HINT));
                }
                let delay = match retry_policy.decide(&mut retry_state, &err) {
                    Decision::Wait(delay) => delay,
                    Decision::Stop => return Err(err.to_rune_error()),
                    Decision::TooLong(requested) => {
                        let seconds = requested.as_millis().div_ceil(1000);
                        return Err(err.to_rune_error().with_hint(format!(
                            "the provider asked to wait {seconds} s, longer than this run allows"
                        )));
                    }
                };
                check_objective_budget(host)?;
                let now = std::time::SystemTime::now();
                host.emit(Event::ProviderRetry {
                    step,
                    next_attempt: attempt.saturating_add(1),
                    max_attempts,
                    delay,
                    retry_at: now.checked_add(delay).unwrap_or(now),
                    kind: err.kind().as_str(),
                });
                retry::wait(delay, cancellation)?;
            }
        }
    }
}

/// One decoded verdict of the goal judge.
#[derive(Clone, PartialEq, Eq, Debug)]
struct JudgeVerdict {
    status: String,
    reason: String,
    next: String,
}

/// Builds the judge's question from the objective and the harness record.
fn judge_prompt(objective: &str, calls: &[CallResult], stall_noted: bool) -> String {
    let last_check = verify::summarize(calls, stall_noted)
        .last_check
        .map_or_else(String::new, |record| {
            let outcome = if record.passed { "passed" } else { "failed" };
            format!("; last check `{}` {outcome}", record.command)
        });
    // Later placeholders go first, so text put in cannot be taken for one.
    JUDGE_PROMPT
        .replacen("{last_check}", &last_check, 1)
        .replacen("{verdict}", Verification::of(calls).as_str(), 1)
        .replacen("{objective}", objective, 1)
}

/// Returns `text` cut to at most `limit` bytes, on a character boundary.
fn cut_bytes(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.get(..end).unwrap_or_default()
}

/// Reads the judge's reply; anything unreadable counts as `done`.
fn parse_judge_reply(text: &str) -> JudgeVerdict {
    let decoded = text
        .find('{')
        .zip(text.rfind('}'))
        .and_then(|(start, end)| text.get(start..=end))
        .and_then(|object| serde_json::from_str::<serde_json::Value>(object).ok())
        .filter(|value| {
            matches!(
                value.get("status").and_then(serde_json::Value::as_str),
                Some("done" | "continue" | "blocked")
            )
        });
    let Some(value) = decoded else {
        return JudgeVerdict {
            status: "done".to_owned(),
            reason: JUDGE_UNREADABLE.to_owned(),
            next: String::new(),
        };
    };
    let field = |name: &str| {
        let text = value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        cut_bytes(text.trim(), JUDGE_TEXT_BYTES).to_owned()
    };
    JudgeVerdict {
        status: field("status"),
        reason: field("reason"),
        next: field("next"),
    }
}

/// Words the note that resumes the turn with the judge's next step.
fn judge_continue_note(reason: &str, next: &str) -> String {
    let next = if next.is_empty() {
        JUDGE_DEFAULT_NEXT
    } else {
        next
    };
    // Later placeholders go first, so text put in cannot be taken for one.
    JUDGE_CONTINUE
        .replacen("{next}", next, 1)
        .replacen("{reason}", reason, 1)
}

/// Returns the effort a judge request overrides, if any.
///
/// Anthropic drops its cached messages when the thinking settings change, so
/// there the judge keeps the session's effort and reads the prefix from cache.
fn judge_effort(dialect: &str, lowest: Effort) -> Option<Effort> {
    (dialect != rune_net::anthropic::NAME).then_some(lowest)
}

/// Sends one judge request on the conversation as it stands.
#[allow(clippy::too_many_arguments)]
fn ask_judge(
    client: &dyn Fetch,
    host: &dyn Host,
    history: &mut History,
    prompt: &str,
    step: u32,
    timeouts: transport::RequestTimeouts,
    max_attempts: usize,
    cancellation: &Cancellation,
) -> Result<(JudgeVerdict, Usage)> {
    // A cut reply cannot be decoded, so it is sent again rather than kept.
    let mut cuts = MAX_CUT_CONTINUATIONS;
    let (outcome, _, _) = stream_with_retry(
        client,
        host,
        history,
        step,
        false,
        timeouts,
        max_attempts,
        cancellation,
        &mut cuts,
        Some(prompt),
        judge_effort(host.dialect().name(), host.lowest_effort()),
        true,
    )?;
    Ok((parse_judge_reply(&outcome.text()), outcome.usage))
}

/// Most `subagent` calls run together in one group.
pub const MAX_PARALLEL_CHILDREN: usize = 4;

/// Splits the concurrent prefix into the groups that run together, in order:
/// each holds at most `width` calls and at most `MAX_PARALLEL_CHILDREN` calls
/// named `subagent`.
#[cfg(not(target_family = "wasm"))]
fn parallel_groups(calls: &[PreparedCall], width: usize) -> Vec<&[PreparedCall]> {
    let mut groups = Vec::new();
    let mut start = 0_usize;
    let mut children = 0_usize;
    for (index, call) in calls.iter().enumerate() {
        let child = call.name == "subagent";
        let full = index.saturating_sub(start) >= width;
        if index > start && (full || (child && children >= MAX_PARALLEL_CHILDREN)) {
            groups.push(&calls[start..index]);
            start = index;
            children = 0;
        }
        if child {
            children = children.saturating_add(1);
        }
    }
    if start < calls.len() {
        groups.push(&calls[start..]);
    }
    groups
}

/// Tells a headless run that a question at the end will not be answered.
const QUESTION_NUDGE: &str = "<harness_check>Nobody can answer in this run. Decide, state the \
assumption in one line, and continue the work.</harness_check>";

/// Shown once when the provider will not return reasoning summaries.
pub const SUMMARY_REFUSED_NOTICE: &str =
    "reasoning summaries are not available for this organization; continuing without them";

/// Whether a refused request was refused for asking for reasoning summaries,
/// which some organizations must be verified to receive.
fn summary_refused(err: &rune_net::error::NetError) -> bool {
    if err.kind() != rune_net::error::FailureKind::InvalidRequest {
        return false;
    }
    let message = err.message().to_ascii_lowercase();
    message.contains("verified") && message.contains("summar")
}

/// Resolves and executes a batch of tool calls.
///
/// Native hosts may run independent read-only calls at the front concurrently,
/// within the configured cap. Retained-result reads and everything after the
/// first mutating call run in order, so writes cannot race reads of the same file.
///
/// Every call is answered, including those left unrun by a cancellation, so
/// the history never holds a call without its result. A conversation in that
/// state cannot form another request.
#[allow(clippy::too_many_arguments)]
fn execute_batch(
    calls: &[PreparedCall],
    cut: &[String],
    host: &dyn Host,
    history: &mut History,
    result_bytes: &mut usize,
    limits: &BudgetSet,
    previous: &[CallResult],
    repaired: &mut Vec<(String, Resolved)>,
) -> (Vec<CallResult>, Option<(PreparedCall, String)>) {
    let mut results = Vec::with_capacity(calls.len());
    let mut cancelled = false;
    // The call that stalled the run and why; everything after it is answered unrun.
    let mut stalled: Option<(PreparedCall, String)> = None;
    let failure_limit = limits.get(LimitName::MaxConsecutiveToolFailures).value();
    let offered = host.tools();
    let turn_cap = limits.get_usize(LimitName::MaxTurnResultBytes);
    let retains = host.retains_results();
    let tool_cap = usize::try_from(window_result_bytes(
        limits.get_bytes(LimitName::MaxToolResultBytes),
        host.context_window(),
    ))
    .unwrap_or(usize::MAX);
    // Only the initial independent reads may overlap. Retained-result reads
    // depend on earlier spills and on the exact remaining budget, so flush the
    // prefix before them and keep the rest of the batch sequential.
    // `off` still respects the documented hard concurrency ceiling.
    #[cfg(not(target_family = "wasm"))]
    let width = limits.get_usize(LimitName::ParallelToolCalls).clamp(1, 64);
    #[cfg(not(target_family = "wasm"))]
    let parallel_prefix = if width > 1 {
        calls
            .iter()
            .take_while(|call| {
                call.name != "read_tool_result" && host.concurrent_tool_host(&call.name).is_some()
            })
            .count()
    } else {
        0
    };
    // Adjacent duplicates must see the preceding result before they can run.
    // Keep those and the remaining calls in order, while other reads overlap.
    // A repeat must see the earlier result before it can be answered from it.
    #[cfg(not(target_family = "wasm"))]
    let parallel_prefix = (1..parallel_prefix)
        .find(|&index| {
            calls[..index]
                .iter()
                .any(|earlier| identical_call(earlier, &calls[index]))
        })
        .unwrap_or(parallel_prefix);
    #[cfg(target_family = "wasm")]
    let parallel_prefix = 0;

    #[cfg(not(target_family = "wasm"))]
    for group in parallel_groups(&calls[..parallel_prefix], width) {
        let group_results = std::thread::scope(|scope| {
            let mut pending = Vec::with_capacity(group.len());
            for (index, call) in group.iter().enumerate() {
                let view = guard_view(call, &offered);
                // Later calls in this group differ from their predecessor,
                // so they cannot extend an existing failure streak.
                let blocked = if let Some(result) = cut_call(call, cut, host) {
                    Some(result)
                } else if stalled.is_some() {
                    Some(stopped_call(call, host))
                } else if index == 0 {
                    guard_result(
                        failure_guard(&view, host, previous, &results, failure_limit, cancelled),
                        &view,
                        &mut stalled,
                    )
                } else {
                    None
                };
                let blocked = blocked
                    .or_else(|| repeat_guard(&view, host, history, previous, &results, cancelled))
                    .or_else(|| {
                        guard_result(
                            loop_guard(&view, host, previous, &results, cancelled),
                            &view,
                            &mut stalled,
                        )
                    });
                let prepared = match blocked {
                    Some(result) => Err(result),
                    None => prepare_call(call, host, &offered, &mut cancelled, tool_cap, repaired),
                };
                let execution = match prepared {
                    Ok(Cleared {
                        call,
                        arguments,
                        context,
                        note,
                    }) => {
                        let executor = host.concurrent_tool_host(&call.name);
                        let name = call.name.clone();
                        let worker = scope.spawn(move || {
                            let started = Instant::now();
                            let execution = (|| {
                                let executor = executor.ok_or_else(|| {
                                    RuneError::new(ErrorCode::Internal, "read executor disappeared")
                                })?;
                                executor.cancellation().check()?;
                                context.check_cancelled()?;
                                executor.execute_with_context(&name, &arguments, &context)
                            })();
                            (execution, started.elapsed())
                        });
                        (call, note, worker)
                    }
                    Err(result) => {
                        pending.push(Err(result));
                        continue;
                    }
                };
                pending.push(Ok(execution));
            }
            pending
                .into_iter()
                .map(|pending| match pending {
                    Err(result) => result,
                    Ok((call, note, worker)) => {
                        let (execution, elapsed) = match worker.join() {
                            Ok(finished) => finished,
                            Err(payload) => std::panic::resume_unwind(payload),
                        };
                        let result = finish_call(&call, host, execution, elapsed, &mut cancelled);
                        with_note(result, note.as_deref())
                    }
                })
                .collect::<Vec<_>>()
        });
        for result in group_results {
            retain_result(
                result,
                host,
                history,
                retains,
                result_bytes,
                turn_cap,
                tool_cap,
                &mut results,
            );
        }
    }
    for call in &calls[parallel_prefix..] {
        let view = guard_view(call, &offered);
        let cap = turn_cap.saturating_sub(*result_bytes).min(tool_cap);
        let blocked = if let Some(result) = cut_call(call, cut, host) {
            Some(result)
        } else if stalled.is_some() {
            Some(stopped_call(call, host))
        } else {
            guard_result(
                failure_guard(&view, host, previous, &results, failure_limit, cancelled),
                &view,
                &mut stalled,
            )
        };
        let result = blocked
            .or_else(|| repeat_guard(&view, host, history, previous, &results, cancelled))
            .or_else(|| {
                guard_result(
                    loop_guard(&view, host, previous, &results, cancelled),
                    &view,
                    &mut stalled,
                )
            })
            .or_else(|| overwrite_guard(&view, host, history))
            .unwrap_or_else(|| {
                execute_call(
                    call,
                    host,
                    &offered,
                    &mut cancelled,
                    history,
                    cap,
                    tool_cap,
                    repaired,
                )
            });
        retain_result(
            result,
            host,
            history,
            retains,
            result_bytes,
            turn_cap,
            tool_cap,
            &mut results,
        );
    }
    (results, stalled)
}

/// Answers a call the output limit cut off inside its arguments.
fn cut_call(call: &PreparedCall, cut: &[String], host: &dyn Host) -> Option<CallResult> {
    if !cut.contains(&call.id) {
        return None;
    }
    host.emit(Event::ToolDenied {
        call: call.clone(),
        reason: "cut at the output limit".to_owned(),
    });
    Some(CallResult {
        call: call.clone(),
        output: ToolOutput::failure(CUT_CALL_NOTE),
        executed: false,
    })
}

/// Call IDs identify requests, not the operation being repeated.
fn identical_call(left: &PreparedCall, right: &PreparedCall) -> bool {
    left.name == right.name && left.arguments == right.arguments
}

/// Opens the answer given in place of a repeated read.
pub const REPEAT_MARKER: &str = "[unchanged since call ";

/// Answers a read-only call that repeats an earlier successful one.
///
/// It applies within this turn, only when no call that could change anything
/// ran since, and only while the earlier result is still in the history. A
/// second repeat runs, so a model that knows of an outside change can still
/// look again. A check is not answered from an earlier result while
/// background work is alive or after any of it started or ended since that
/// result; the background list is shared by every conversation in the
/// process, which errs toward running the check.
fn repeat_guard(
    call: &PreparedCall,
    host: &dyn Host,
    history: &History,
    previous: &[CallResult],
    results: &[CallResult],
    cancelled: bool,
) -> Option<CallResult> {
    if cancelled || host.cancellation().is_cancelled() {
        return None;
    }
    let activity = |name: &str| {
        host.tool_activity(name)
            .unwrap_or_else(|| infer_activity(name))
    };
    let reads =
        |activity: Activity| matches!(activity, Activity::Read | Activity::List | Activity::Search);
    let activity_now = activity(&call.name);
    // A test, build or check command run again with nothing changed since says
    // what it said before, failures included, so it is answered the same way.
    let check = call.name == "shell"
        && shell_run_command(&call.arguments)
            .is_some_and(|command| rune_policy::checks::is_repeatable_check(&command));
    // A retained-result page depends on the remaining budget, not only its arguments.
    if !(reads(activity_now) || check) || call.name == "read_tool_result" {
        return None;
    }
    // A server, watcher or background build can change a check's outcome
    // without any call in between.
    let background = check.then(rune_tools::shell::background);
    if background.is_some_and(|(alive, _)| alive > 0) {
        return None;
    }
    // A file read can be checked directly: an identical read of a file whose
    // content is the same as when the earlier result was kept, in this turn or
    // an earlier one, is answered from that result.
    if call.name == "read_file" {
        let repeated_here = previous
            .iter()
            .chain(results)
            .rev()
            .take_while(|result| !(result.executed && identical_call(&result.call, call)))
            .any(|result| identical_call(&result.call, call));
        if repeated_here {
            return None;
        }
        let digest = file_digest(host, &call.arguments)?;
        let (id, succeeded) = results
            .iter()
            .rev()
            .find(|result| result.executed && identical_call(&result.call, call))
            .map(|result| (result.call.id.clone(), !result.output.is_error))
            .or_else(|| latest_identical_result(history, call))?;
        if !succeeded || history.read_digest(&id) != Some(digest) {
            return None;
        }
        return Some(repeat_pointer(
            call,
            host,
            &id,
            activity_now,
            "the file is unchanged",
        ));
    }
    let earlier = previous.iter().chain(results).rev().find_map(|result| {
        if identical_call(&result.call, call) {
            if !result.executed && result.output.text.starts_with(REPEAT_MARKER) {
                return Some(None);
            }
            if result.executed && (check || !result.output.is_error) {
                return Some(Some(result));
            }
        }
        let changes = result.executed
            && !reads(activity(&result.call.name))
            && activity(&result.call.name) != Activity::Plan;
        changes.then_some(None)
    })??;
    // A compaction inside the turn may have removed the earlier result.
    let in_batch = results
        .iter()
        .any(|result| result.call.id == earlier.call.id);
    if !in_batch && !result_retained(history, &earlier.call.id) {
        return None;
    }
    if let Some((_, generation)) = background
        && history.check_generation(&earlier.call.id) != Some(generation)
    {
        return None;
    }
    Some(repeat_pointer(
        call,
        host,
        &earlier.call.id,
        activity_now,
        "nothing that changes files has run since",
    ))
}

/// Answers a repeated call with a pointer to the earlier result and reports it.
fn repeat_pointer(
    call: &PreparedCall,
    host: &dyn Host,
    earlier: &str,
    activity: Activity,
    why: &str,
) -> CallResult {
    let output = ToolOutput::success(format!(
        "{REPEAT_MARKER}{earlier}] Same `{}` call as `{earlier}` above, and {why}, so its \
         result still holds. Call it once more only if you have reason to think it changed.",
        call.name
    ));
    host.emit(Event::ToolStarted {
        call: call.clone(),
        activity,
    });
    host.emit(Event::ToolFinished {
        call: call.clone(),
        output: output.clone(),
        elapsed: Duration::ZERO,
    });
    CallResult {
        call: call.clone(),
        output,
        executed: false,
    }
}

/// Returns whether the history still holds a call's result. A result that
/// was replaced by a receipt no longer counts.
fn result_retained(history: &History, id: &str) -> bool {
    if history.is_pruned(id) {
        return false;
    }
    history.turns().iter().any(|turn| {
        turn.parts.iter().any(|part| {
            matches!(part, ContentPart::ToolResult { id: held, .. } if held.to_string() == id)
        })
    })
}

/// Finds the newest identical read with a recorded digest in the history,
/// and whether its result, still held there, succeeded.
fn latest_identical_result(history: &History, call: &PreparedCall) -> Option<(String, bool)> {
    let id = history.turns().iter().rev().find_map(|turn| {
        turn.parts.iter().rev().find_map(|part| match part {
            ContentPart::ToolCall {
                id,
                name,
                arguments,
            } if name == &call.name
                && arguments == &call.arguments
                && history.read_digest(&id.to_string()).is_some() =>
            {
                Some(id.to_string())
            }
            _ => None,
        })
    })?;
    if history.is_pruned(&id) {
        return None;
    }
    history.turns().iter().find_map(|turn| {
        turn.parts.iter().find_map(|part| match part {
            ContentPart::ToolResult {
                id: held, is_error, ..
            } if held.to_string() == id => Some((id.clone(), !is_error)),
            _ => None,
        })
    })
}

/// Digests the file a `read_file` call names, as it is now.
fn file_digest(host: &dyn Host, arguments: &str) -> Option<[u8; 32]> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    digest_of(&resolved_path(host, value.get("path")?.as_str()?))
}

/// The path a file tool would use for `raw`: resolved like the tools resolve
/// it, else joined to the workspace when relative.
fn resolved_path(host: &dyn Host, raw: &str) -> Utf8PathBuf {
    let context = host.context();
    rune_tools::resolve(&context, raw).map_or_else(
        |_| {
            let path = Utf8Path::new(raw);
            if path.is_absolute() {
                path.to_owned()
            } else {
                context.workspace().join(path)
            }
        },
        |resolved| resolved.path,
    )
}

/// SHA-256 of a file's bytes, `None` when it cannot be read.
fn digest_of(path: &Utf8Path) -> Option<[u8; 32]> {
    use sha2::{Digest as _, Sha256};
    let bytes = std::fs::read(path).ok()?;
    Some(Sha256::digest(&bytes).into())
}

/// The paths a write, edit or patch changes, as the model wrote them.
fn changed_paths(name: &str, arguments: &serde_json::Value) -> Vec<String> {
    match name {
        "write_file" | "edit_file" => arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .into_iter()
            .collect(),
        "apply_patch" => arguments
            .get("input")
            .and_then(serde_json::Value::as_str)
            .map(rune_tools::apply_patch::paths)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Answers a `write_file` over an existing file the model has not seen, once.
///
/// The identical call sent again runs, so a deliberate overwrite costs one
/// extra step and a blind one is caught.
fn overwrite_guard(
    call: &PreparedCall,
    host: &dyn Host,
    history: &mut History,
) -> Option<CallResult> {
    use sha2::{Digest as _, Sha256};
    if call.name != "write_file" {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    let raw = value.get("path")?.as_str()?;
    let path = resolved_path(host, raw);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 || history.view(&path).is_some() {
        return None;
    }
    // A write policy refuses anyway is left to policy: sending it again would
    // not overwrite anything, so the advice below would be wrong.
    if host.decide_quietly(&call.name, Some(raw)) == Outcome::Deny {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(call.name.as_bytes());
    hasher.update([0]);
    hasher.update(call.arguments.as_bytes());
    if !history.refuse_overwrite(hasher.finalize().into()) {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    // Pieces between newlines, less the empty one after a final newline.
    let lines = bytes
        .split(|&byte| byte == b'\n')
        .count()
        .saturating_sub(usize::from(bytes.ends_with(b"\n")));
    let text = format!(
        "`{raw}` already exists ({lines} lines) and has not been read in this session. Read it \
         first, or send this same call again to overwrite it."
    );
    host.emit(Event::ToolDenied {
        call: call.clone(),
        reason: text.clone(),
    });
    Some(CallResult {
        call: call.clone(),
        output: ToolOutput::failure(text),
        executed: false,
    })
}

/// Answer for a call refused because it already failed the same way.
fn repeated_failure_refusal(failures: usize) -> String {
    format!(
        "{REFUSAL_PREFIX} {failures} times in a row. Do not send it again. Read the error above \
         and change something: different arguments, another tool, or say what is blocking you."
    )
}

/// How every repeated-failure refusal begins.
const REFUSAL_PREFIX: &str = "not run: this exact call has failed";

/// Answer for the call that stalls the run.
const STALLED_CALL: &str =
    "not run: the run stopped because this call was sent again after two refusals";

/// Answer for the calls after a stall in the same batch.
const STOPPED_CALL: &str = "not run: the run stopped";

/// What the failure guard decided for one call.
enum Guard {
    /// The call runs.
    Run,
    /// The call is answered with a nudge and the turn goes on.
    Refuse(CallResult),
    /// The call is answered and the run stops, for the reason given.
    Stall(CallResult, String),
}

/// Checks failures in request order, across batches but only within this turn.
///
/// Looks at the trailing run of earlier results identical to `call` that all
/// failed. A call that already failed enough times is refused with a nudge;
/// one sent again after two refusals stalls the run.
fn failure_guard(
    call: &PreparedCall,
    host: &dyn Host,
    previous: &[CallResult],
    results: &[CallResult],
    limit: Option<u64>,
    cancelled: bool,
) -> Guard {
    // Cancellation retains its existing outcome and answers pending calls.
    if cancelled || host.cancellation().is_cancelled() {
        return Guard::Run;
    }
    let Some(limit) = limit else {
        return Guard::Run;
    };
    // A malformed, unoffered or denied call never runs but still failed, so
    // the run counts every failed answer. Once it holds `limit` of them, each
    // further identical call is answered here instead of running, so the
    // entries past the limit are this guard's own refusals. Counting them
    // this way does not depend on their text, which a result budget may cut.
    let limit = usize::try_from(limit).unwrap_or(usize::MAX).max(1);
    let run = previous
        .iter()
        .chain(results)
        .rev()
        .take_while(|result| result.output.is_error && identical_call(&result.call, call))
        .count();
    let failures = run.min(limit);
    let refusals = run.saturating_sub(limit);
    let deny = |text: String| {
        host.emit(Event::ToolDenied {
            call: call.clone(),
            reason: text.clone(),
        });
        CallResult {
            call: call.clone(),
            output: ToolOutput::failure(text),
            executed: false,
        }
    };
    if refusals >= 2 {
        return Guard::Stall(
            deny(STALLED_CALL.to_owned()),
            format!(
                "the model sent the failing `{}` call again after it was refused twice",
                call.name
            ),
        );
    }
    // The limit is how many identical failures in a row are allowed; the
    // next attempt is refused.
    if failures >= limit {
        return Guard::Refuse(deny(repeated_failure_refusal(failures)));
    }
    Guard::Run
}

/// Turns a guard decision into the answer given in place of running the call,
/// recording the call when it stalled the run.
fn guard_result(
    guard: Guard,
    call: &PreparedCall,
    stalled: &mut Option<(PreparedCall, String)>,
) -> Option<CallResult> {
    match guard {
        Guard::Run => None,
        Guard::Refuse(result) => Some(result),
        Guard::Stall(result, message) => {
            *stalled = Some((call.clone(), message));
            Some(result)
        }
    }
}

/// Executed calls the loop guard looks back over.
const LOOP_WINDOW: usize = 10;
/// Identical results in the window after which the same call is refused.
const LOOP_REPEATS: usize = 4;
/// Answer given in place of a call that keeps returning the same result.
const LOOP_REFUSAL: &str = "not run: this exact call has returned the same result {n} times. \
Do not send it again. Read the result above and change something: different arguments, \
another tool, or say what is blocking you.";
/// How every loop refusal begins.
const LOOP_REFUSAL_PREFIX: &str = "not run: this exact call has returned the same result";

/// Refuses a call that keeps returning the same result with nothing changed
/// in between, within this turn.
///
/// Only executed calls since the last change count, at most [`LOOP_WINDOW`]
/// of them. When the newest [`LOOP_REPEATS`] identical calls among them
/// succeeded with the same text, the call is refused; one sent again after
/// two such refusals stalls the run. Repeated failures are left to the
/// failure guard and its configurable cap. Arguments compare as JSON values, so key
/// order and spacing do not matter.
fn loop_guard(
    call: &PreparedCall,
    host: &dyn Host,
    previous: &[CallResult],
    results: &[CallResult],
    cancelled: bool,
) -> Guard {
    if cancelled || host.cancellation().is_cancelled() || call.name == "read_tool_result" {
        return Guard::Run;
    }
    let canonical = serde_json::from_str::<serde_json::Value>(&call.arguments).ok();
    let interact = call.name == "shell"
        && canonical
            .as_ref()
            .and_then(|value| value.get("action"))
            .and_then(serde_json::Value::as_str)
            == Some("interact");
    if interact {
        return Guard::Run;
    }
    let same = |other: &PreparedCall| {
        other.name == call.name
            && match (
                &canonical,
                serde_json::from_str::<serde_json::Value>(&other.arguments).ok(),
            ) {
                (Some(left), Some(right)) => *left == right,
                _ => other.arguments == call.arguments,
            }
    };
    let mut executed: usize = 0;
    let mut outputs: Vec<&ToolOutput> = Vec::new();
    let mut refusals: usize = 0;
    for result in previous
        .iter()
        .chain(results)
        .rev()
        .take_while(|result| !verify::is_change(result))
    {
        if result.executed {
            executed = executed.saturating_add(1);
            if executed > LOOP_WINDOW {
                break;
            }
            if same(&result.call) {
                outputs.push(&result.output);
            }
        } else if same(&result.call) && result.output.text.starts_with(LOOP_REFUSAL_PREFIX) {
            refusals = refusals.saturating_add(1);
        }
    }
    let looping = outputs.len() >= LOOP_REPEATS
        && outputs[..LOOP_REPEATS]
            .iter()
            .all(|output| !output.is_error && output.text == outputs[0].text);
    if !looping {
        return Guard::Run;
    }
    let deny = |text: String| {
        host.emit(Event::ToolDenied {
            call: call.clone(),
            reason: text.clone(),
        });
        CallResult {
            call: call.clone(),
            output: ToolOutput::failure(text),
            executed: false,
        }
    };
    if refusals >= 2 {
        return Guard::Stall(
            deny(STALLED_CALL.to_owned()),
            format!(
                "`{}` returned the same result {LOOP_REPEATS} times and was sent again after 2 \
                 refusals",
                call.name
            ),
        );
    }
    Guard::Refuse(deny(LOOP_REFUSAL.replace("{n}", &LOOP_REPEATS.to_string())))
}

/// The call as the guards compare it: in its repaired form when repair
/// changes it, the form earlier results are recorded in, so a repeated
/// near-miss is recognised as the call it runs as.
fn guard_view<'a>(
    call: &'a PreparedCall,
    offered: &[ToolSpec],
) -> std::borrow::Cow<'a, PreparedCall> {
    match repair::resolve(&call.name, &call.arguments, offered) {
        Ok(resolved) if repair::note(&resolved).is_some() => {
            std::borrow::Cow::Owned(PreparedCall {
                id: call.id.clone(),
                name: resolved.name,
                arguments: resolved.arguments.to_string(),
            })
        }
        _ => std::borrow::Cow::Borrowed(call),
    }
}

/// Answers a call that comes after a stall in the same batch.
fn stopped_call(call: &PreparedCall, host: &dyn Host) -> CallResult {
    host.emit(Event::ToolDenied {
        call: call.clone(),
        reason: STOPPED_CALL.to_owned(),
    });
    CallResult {
        call: call.clone(),
        output: ToolOutput::failure(STOPPED_CALL),
        executed: false,
    }
}

/// Retains and charges answers in request order, regardless of execution order.
#[allow(clippy::too_many_arguments)]
fn retain_result(
    mut result: CallResult,
    host: &dyn Host,
    history: &mut History,
    retains: bool,
    result_bytes: &mut usize,
    turn_cap: usize,
    tool_cap: usize,
    results: &mut Vec<CallResult>,
) {
    // Taken now, before any later call in the batch can change the file.
    if result.executed
        && !result.output.is_error
        && result.call.name == "read_file"
        && let Some(digest) = file_digest(host, &result.call.arguments)
    {
        history.record_read(&result.call.id, digest);
        if let Some(raw) = serde_json::from_str::<serde_json::Value>(&result.call.arguments)
            .ok()
            .as_ref()
            .and_then(|value| value.get("path"))
            .and_then(serde_json::Value::as_str)
        {
            history.record_view(resolved_path(host, raw), digest);
        }
    }
    if result.executed
        && result.call.name == "shell"
        && shell_run_command(&result.call.arguments)
            .is_some_and(|command| rune_policy::checks::is_repeatable_check(&command))
    {
        history.record_check_generation(&result.call.id, rune_tools::shell::background().1);
    }
    let cap = turn_cap.saturating_sub(*result_bytes).min(tool_cap);
    let mut text = std::mem::take(&mut result.output.text);
    // A shortened view keeps the complete text one read away.
    if let Some(full) = result.output.full_text.take().filter(|_| retains)
        && let Ok(preview) = history.result_store_mut().spill(
            &result.call.name,
            &result.call.id,
            full,
            result.output.is_error,
            0,
        )
    {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let _ = write!(
            text,
            "[full log: read_tool_result handle {}]",
            preview.handle
        );
    }
    let bounded = if text.len() > cap && retains {
        spill_result(history, &result, text, cap)
    } else if text.len() > cap {
        bound_result(text, cap)
    } else {
        text
    };
    *result_bytes = result_bytes.saturating_add(bounded.len());
    result.output.text = bounded;
    results.push(result);
}

/// A call cleared to run, in the form it runs as.
struct Cleared {
    /// The effective call: the model's own unless it was repaired.
    call: PreparedCall,
    arguments: serde_json::Value,
    context: ExecutionContext,
    /// Appended to the result of a repaired call.
    note: Option<String>,
}

/// Resolves advertisement, arguments and authority on the turn thread.
///
/// The call is first resolved against the offered tools, which may repair
/// its name or arguments. Everything after that, from policy and hooks to
/// events, execution and the recorded result, sees the effective call.
fn prepare_call(
    call: &PreparedCall,
    host: &dyn Host,
    offered: &[ToolSpec],
    cancelled: &mut bool,
    tool_cap: usize,
    repaired: &mut Vec<(String, Resolved)>,
) -> std::result::Result<Cleared, CallResult> {
    if *cancelled || host.cancellation().is_cancelled() {
        *cancelled = true;
        return Err(CallResult {
            call: call.clone(),
            output: ToolOutput::failure("not run: the turn was cancelled"),
            executed: false,
        });
    }

    let resolved = match repair::resolve(&call.name, &call.arguments, offered) {
        Ok(resolved) => resolved,
        Err(repair::Unresolved::UnknownTool) => {
            let names: Vec<&str> = offered.iter().map(|spec| spec.name.as_str()).collect();
            let reason = format!(
                "there is no tool named `{}`; the tools are {}",
                call.name,
                names.join(", ")
            );
            host.emit(Event::ToolDenied {
                call: call.clone(),
                reason: reason.clone(),
            });
            return Err(CallResult {
                call: call.clone(),
                output: ToolOutput::failure(reason),
                executed: false,
            });
        }
        Err(repair::Unresolved::InvalidJson(err)) => {
            // A malformed argument string is the model's error to correct.
            let output = ToolOutput::failure(format!(
                "the arguments for `{}` are not valid JSON: {err}",
                call.name
            ));
            return Err(CallResult {
                call: call.clone(),
                output,
                executed: false,
            });
        }
    };
    let note = repair::note(&resolved);
    // A shell command that is only an apply_patch heredoc runs as the patch
    // tool, whether or not that tool is offered. The recorded call stays as
    // the model sent it.
    if let Some(patch) = heredoc_patch(&resolved) {
        let arguments = serde_json::json!({ "input": patch });
        let effective = PreparedCall {
            id: call.id.clone(),
            name: "apply_patch".to_owned(),
            arguments: arguments.to_string(),
        };
        repaired.push((
            call.id.clone(),
            Resolved {
                name: effective.name.clone(),
                arguments: arguments.clone(),
                renamed_from: Some(call.name.clone()),
                repairs: Vec::new(),
                intercepted: true,
            },
        ));
        let note = Some(match note {
            Some(repair) => format!("{repair}\n{HEREDOC_NOTE}"),
            None => HEREDOC_NOTE.to_owned(),
        });
        return match authorize(&effective, arguments, host, cancelled, tool_cap) {
            Ok((arguments, context)) => Ok(Cleared {
                call: effective,
                arguments,
                context,
                note,
            }),
            Err(result) => Err(with_note(result, note.as_deref())),
        };
    }
    // An unrepaired call keeps its argument text exactly as sent, which is
    // what the repeat and failure guards compare.
    let effective = if note.is_some() {
        host.emit(Event::CallRepaired {
            id: call.id.clone(),
            from: call.name.clone(),
            to: resolved.name.clone(),
            repairs: resolved.repairs.clone(),
        });
        repaired.push((call.id.clone(), resolved.clone()));
        PreparedCall {
            id: call.id.clone(),
            name: resolved.name,
            arguments: resolved.arguments.to_string(),
        }
    } else {
        call.clone()
    };
    match authorize(&effective, resolved.arguments, host, cancelled, tool_cap) {
        Ok((arguments, context)) => Ok(Cleared {
            call: effective,
            arguments,
            context,
            note,
        }),
        Err(result) => Err(with_note(result, note.as_deref())),
    }
}

/// The last line of a shell heredoc that ran as the patch tool.
const HEREDOC_NOTE: &str = "[ran as the apply_patch tool]";

/// The patch of a `shell` run whose command is only an apply_patch heredoc.
fn heredoc_patch(resolved: &Resolved) -> Option<String> {
    if resolved.name != "shell"
        || resolved
            .arguments
            .get("action")
            .and_then(serde_json::Value::as_str)
            != Some("run")
    {
        return None;
    }
    let command = resolved.arguments.get("command")?.as_str()?;
    rune_tools::apply_patch::intercept(command).map(|(_, patch)| patch)
}

/// Appends a repaired call's note to the end of its result text.
fn with_note(mut result: CallResult, note: Option<&str>) -> CallResult {
    if let Some(note) = note {
        let text = &mut result.output.text;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(note);
    }
    result
}

/// Resolves policy, hooks and the execution context for an effective call.
fn authorize(
    call: &PreparedCall,
    arguments: serde_json::Value,
    host: &dyn Host,
    cancelled: &mut bool,
    tool_cap: usize,
) -> std::result::Result<(serde_json::Value, ExecutionContext), CallResult> {
    let activity = host
        .tool_activity(&call.name)
        .unwrap_or_else(|| infer_activity(&call.name));

    let target = host.tool_permission_target(&call.name, &arguments);
    // A patch is decided path by path, each as the edit or write it makes.
    // One that does not parse runs, so the tool reports the parse error.
    let patch = if call.name == "apply_patch" {
        arguments
            .get("input")
            .and_then(serde_json::Value::as_str)
            .and_then(|input| rune_tools::apply_patch::parse(input).ok())
    } else {
        None
    };
    let (outcome, reason) = if let Some(patch) = &patch {
        for (name, path) in patch_decisions(patch) {
            let (outcome, reason) =
                host.decide_with_arguments(name, Some(path), &serde_json::json!({ "path": path }));
            if outcome != Outcome::Allow {
                host.emit(Event::ToolDenied {
                    call: call.clone(),
                    reason: reason.clone(),
                });
                return Err(CallResult {
                    call: call.clone(),
                    output: ToolOutput::failure(format!(
                        "patch path `{path}` was not approved: {reason}"
                    )),
                    executed: false,
                });
            }
        }
        (Outcome::Allow, String::new())
    } else if call.name == "apply_patch" {
        (Outcome::Allow, String::new())
    } else {
        host.decide_with_arguments(&call.name, target.as_deref(), &arguments)
    };

    match outcome {
        Outcome::Deny => {
            host.emit(Event::ToolDenied {
                call: call.clone(),
                reason: reason.clone(),
            });
            return Err(CallResult {
                call: call.clone(),
                output: ToolOutput::failure(format!(
                    "`{}` was refused by policy: {reason}",
                    call.name
                )),
                executed: false,
            });
        }
        Outcome::Ask => {
            // The host resolves an ask before reaching this point, so an
            // ask that arrives here means nothing could approve the call.
            // It is reported like a refusal, so the person watching sees
            // it, and the model is told why and not to retry, because the
            // same call will be held the same way every time.
            let action = target.as_deref().unwrap_or(&call.name);
            host.emit(Event::ToolDenied {
                call: call.clone(),
                reason: format!("{reason}; nothing in this session could approve it"),
            });
            return Err(CallResult {
                call: call.clone(),
                output: ToolOutput::failure(format!(
                    "`{}` was not run: no rule allows `{action}`, and this session has no \
                     way to ask for approval. Do not retry it or a variation of it. Continue \
                     without it, or tell the user what to run or which rule to add.",
                    call.name
                )),
                executed: false,
            });
        }
        Outcome::Allow => {}
    }

    // Every image in a vision batch needs its own decision before the backend
    // sees any of them. A grant for the first path cannot cover the rest.
    if call.name == "vision"
        && let Some(paths) = arguments.get("paths").and_then(serde_json::Value::as_array)
    {
        for path in paths.iter().skip(1).filter_map(serde_json::Value::as_str) {
            let path = path.trim();
            let (outcome, reason) = host.decide(&call.name, Some(path));
            if outcome != Outcome::Allow {
                host.emit(Event::ToolDenied {
                    call: call.clone(),
                    reason: reason.clone(),
                });
                return Err(CallResult {
                    call: call.clone(),
                    output: ToolOutput::failure(format!(
                        "vision image `{path}` was not approved: {reason}"
                    )),
                    executed: false,
                });
            }
        }
    }

    // A rename changes both names. Neither path's grant may authorize the
    // other, so resolve the destination through the same host policy as the source.
    if call.name == "rename_file"
        && let Some(destination) = arguments
            .get("new_path")
            .and_then(serde_json::Value::as_str)
    {
        let (outcome, reason) = host.decide(&call.name, Some(destination));
        if outcome != Outcome::Allow {
            host.emit(Event::ToolDenied {
                call: call.clone(),
                reason: reason.clone(),
            });
            return Err(CallResult {
                call: call.clone(),
                output: ToolOutput::failure(format!(
                    "rename destination `{destination}` was not approved: {reason}"
                )),
                executed: false,
            });
        }
    }

    let private_access = call.name == "web_fetch"
        && arguments
            .get("allow_private")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
    if private_access {
        let (outcome, reason) = host.decide_private_network(target.as_deref());
        if outcome != Outcome::Allow {
            host.emit(Event::ToolDenied {
                call: call.clone(),
                reason: reason.clone(),
            });
            return Err(CallResult {
                call: call.clone(),
                output: ToolOutput::failure(format!(
                    "private-network access was not approved: {reason}"
                )),
                executed: false,
            });
        }
    }
    let veto = match host.pre_tool(call) {
        Ok(veto) => {
            veto.map(|veto| format!("pre-tool hook `{}` vetoed: {}", veto.name, veto.reason))
        }
        Err(error) if error.code() == ErrorCode::Cancelled => {
            *cancelled = true;
            return Err(CallResult {
                call: call.clone(),
                output: ToolOutput::failure("not run: the turn was cancelled"),
                executed: false,
            });
        }
        Err(error) => Some(format!("pre-tool hook failed: {error}")),
    };
    if let Some(reason) = veto {
        host.emit(Event::ToolDenied {
            call: call.clone(),
            reason: reason.clone(),
        });
        return Err(CallResult {
            call: call.clone(),
            output: ToolOutput::failure(reason),
            executed: false,
        });
    }
    // A hook may have observed cancellation while deciding to allow the call.
    if host.cancellation().is_cancelled() {
        *cancelled = true;
        return Err(CallResult {
            call: call.clone(),
            output: ToolOutput::failure("not run: the turn was cancelled"),
            executed: false,
        });
    }
    let mut context = host.context().with_private_network_access(private_access);
    // Tools cut their own output to the result cap, so what they keep is what
    // the model is shown.
    let output_cap = context.max_output_bytes.min(tool_cap);
    context = context.with_output_cap(output_cap);
    if call.name == "edit_file" {
        // A missed edit may quote the file's closest lines, which is only
        // fair when reading the file would not need an approval.
        let read_target = host.tool_permission_target("read_file", &arguments);
        context = context.with_quote_files(
            host.decide_quietly("read_file", read_target.as_deref()) == Outcome::Allow,
        );
    }
    if let Some(patch) = &patch {
        context = context.with_quote_files(
            patch_decisions(patch)
                .iter()
                .all(|(_, path)| host.decide_quietly("read_file", Some(path)) == Outcome::Allow),
        );
    }
    if call.name == "read_file" {
        context = context.with_accepts_images(host.accepts_images());
    }

    host.emit(Event::ToolStarted {
        call: call.clone(),
        activity,
    });
    Ok((arguments, context))
}

/// Every path a patch touches with the tool it is decided as, in patch order:
/// sources it changes or deletes as `edit_file`, files it creates as `write_file`.
fn patch_decisions(patch: &rune_tools::apply_patch::Patch) -> Vec<(&'static str, &str)> {
    use rune_tools::apply_patch::Section;
    let mut decisions = Vec::new();
    for section in &patch.sections {
        match section {
            Section::Add { path, .. } => decisions.push(("write_file", path.as_str())),
            Section::Delete { path } => decisions.push(("edit_file", path.as_str())),
            Section::Update { path, move_to, .. } => {
                decisions.push(("edit_file", path.as_str()));
                if let Some(dest) = move_to {
                    decisions.push(("write_file", dest.as_str()));
                }
            }
        }
    }
    decisions
}

/// Executes one offered, policy-approved call and preserves batch cancellation.
#[allow(clippy::too_many_arguments)]
fn execute_call(
    call: &PreparedCall,
    host: &dyn Host,
    offered: &[ToolSpec],
    cancelled: &mut bool,
    history: &mut History,
    output_cap: usize,
    tool_cap: usize,
    repaired: &mut Vec<(String, Resolved)>,
) -> CallResult {
    let Cleared {
        call,
        arguments,
        context,
        note,
    } = match prepare_call(call, host, offered, cancelled, tool_cap, repaired) {
        Ok(cleared) => cleared,
        Err(result) => return result,
    };
    // Each changed path as written, resolved, and as it was before the call.
    let targets: Vec<(String, Utf8PathBuf, Option<[u8; 32]>)> =
        changed_paths(&call.name, &arguments)
            .into_iter()
            .map(|raw| {
                let path = resolved_path(host, &raw);
                let before = digest_of(&path);
                (raw, path, before)
            })
            .collect();
    // Retained output is owned by the history rather than the host. Supply
    // that store only after advertisement and policy checks have passed.
    let started = Instant::now();
    let execution = if call.name == "read_tool_result" {
        let cap = context.max_output_bytes.min(output_cap);
        rune_tools::ReadToolResult::default().call(
            &arguments,
            &context
                .with_output_cap(cap)
                .with_result_store(history.shared_result_store()),
        )
    } else {
        host.execute_with_context(
            &call.name,
            &arguments,
            &context.with_live_plan(history.shared_live_plan()),
        )
    };
    let execution = execution.map(|mut output| {
        if !output.is_error {
            note_changes(&call, host, history, &arguments, &targets, &mut output);
        }
        output
    });
    let result = finish_call(&call, host, execution, started.elapsed(), cancelled);
    with_note(result, note.as_deref())
}

/// After a successful change, records what the model now knows of each path
/// and says when a file had changed on disk since it last saw it.
fn note_changes(
    call: &PreparedCall,
    host: &dyn Host,
    history: &mut History,
    arguments: &serde_json::Value,
    targets: &[(String, Utf8PathBuf, Option<[u8; 32]>)],
    output: &mut ToolOutput,
) {
    if call.name == "rename_file"
        && let (Some(source), Some(destination)) = (
            arguments.get("path").and_then(serde_json::Value::as_str),
            arguments
                .get("new_path")
                .and_then(serde_json::Value::as_str),
        )
        && let Some(digest) = history.forget_view(&resolved_path(host, source))
    {
        history.record_view(resolved_path(host, destination), digest);
    }
    let mut stale: Vec<&str> = Vec::new();
    for (raw, path, before) in targets {
        if let (Some(before), Some(seen)) = (before, history.view(path))
            && *before != seen
            && !stale.contains(&raw.as_str())
        {
            stale.push(raw);
        }
        match digest_of(path) {
            Some(after) => history.record_view(path.clone(), after),
            None => {
                history.forget_view(path);
            }
        }
    }
    let notice = match stale.as_slice() {
        [] => return,
        [_] => "[the file had changed on disk since you last read it]".to_owned(),
        several => format!(
            "[these files had changed on disk since you last read them: {}]",
            several.join(", ")
        ),
    };
    if !output.text.is_empty() && !output.text.ends_with('\n') {
        output.text.push('\n');
    }
    output.text.push_str(&notice);
}

/// Reports execution and preserves a cancellation for calls still pending.
fn finish_call(
    call: &PreparedCall,
    host: &dyn Host,
    execution: Result<ToolOutput>,
    elapsed: Duration,
    cancelled: &mut bool,
) -> CallResult {
    let output = match execution {
        Ok(output) => output,
        Err(err) if matches!(err.code(), ErrorCode::Cancelled) => {
            *cancelled = true;
            return CallResult {
                call: call.clone(),
                output: ToolOutput::failure("the call was cancelled"),
                executed: false,
            };
        }
        // Preserve the diagnostic even when the host bypasses the registry.
        Err(err) => ToolOutput::from_error(&err),
    };
    if !output.is_error
        && let Some(update) = &output.plan_update
    {
        host.emit(Event::PlanUpdated {
            update: update.clone(),
        });
    }
    host.emit(Event::ToolFinished {
        call: call.clone(),
        output: output.clone(),
        elapsed,
    });
    CallResult {
        call: call.clone(),
        output,
        executed: true,
    }
}

/// Returns the fallback activity for a host without registered metadata.
///
/// An unknown name is treated as an execution, which is the conservative
/// answer for presentation purposes.
fn infer_activity(name: &str) -> Activity {
    match name {
        "read_file" | "read_tool_result" | "git_diff" | "syntax_diagnostics" => Activity::Read,
        "glob_files" => Activity::List,
        "grep_files" => Activity::Search,
        "write_file" | "rename_file" | "create_directory" => Activity::Write,
        "edit_file" | "apply_patch" => Activity::Edit,
        "shell" => Activity::Execute,
        "web_fetch" | "web_search" => Activity::Network,
        "subagent" => Activity::Delegate,
        "ask_user_question" => Activity::Interact,
        "update_plan" => Activity::Plan,
        _ => Activity::Execute,
    }
}

/// Resolves built-in targets for hosts without a registry.
fn infer_target(name: &str, arguments: &serde_json::Value) -> Option<String> {
    static BUILTIN: std::sync::OnceLock<Option<Registry>> = std::sync::OnceLock::new();
    match BUILTIN.get_or_init(|| rune_tools::inventory::builtin_default().ok()) {
        Some(registry) => permission_target_for(registry, name, arguments),
        None => infer_target_standalone(name, arguments),
    }
}

/// Marker appended to a result cut by the turn's result budget.
const RESULT_TRUNCATION_MARKER: &str = "\n[tool result truncated at the turn's result limit]";

/// Stores the full body before reducing the model-visible text. Metadata is
/// charged to both result budgets, so a tiny or exhausted budget can still
/// answer the call with an empty body without exceeding its limit.
fn spill_result(history: &mut History, result: &CallResult, text: String, cap: usize) -> String {
    match history.result_store_mut().spill(
        &result.call.name,
        &result.call.id,
        text,
        result.output.is_error,
        cap,
    ) {
        Ok(preview) => {
            let marker = format!(
                "\n[{} bytes retained; handle {}]",
                preview.retained_bytes, preview.handle
            );
            if marker.len() > cap {
                return String::new();
            }
            let mut end = preview.text.len().min(cap.saturating_sub(marker.len()));
            while end > 0 && !preview.text.is_char_boundary(end) {
                end = end.saturating_sub(1);
            }
            let mut output = preview.text.get(..end).unwrap_or_default().to_owned();
            output.push_str(&marker);
            output
        }
        Err(err) => bound_result(format!("[tool result could not be retained: {err}]"), cap),
    }
}

/// Cuts a rendered tool result to the bytes the turn can still retain.
///
/// The retained length is charged against the turn's budget, marker included, so
/// the bytes the turn accumulates across its steps can never exceed the limit. A
/// result is therefore whole, or carries the full marker, or is empty: a
/// fragment with no room for the marker would look like the whole result. An
/// empty body still records the answer a call must have, because an unanswered
/// call is an invalid request.
fn bound_result(content: String, remaining: usize) -> String {
    if content.len() <= remaining {
        return content;
    }
    if remaining <= RESULT_TRUNCATION_MARKER.len() {
        return String::new();
    }
    let mut end = remaining.saturating_sub(RESULT_TRUNCATION_MARKER.len());
    while end > 0 && !content.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut out = content.get(..end).unwrap_or_default().to_owned();
    out.push_str(RESULT_TRUNCATION_MARKER);
    out
}

/// Merges adjacent text parts into one, so a delta-per-character stream does not
/// produce one transcript entry per character.
fn coalesce_text(parts: Vec<ContentPart>) -> Vec<ContentPart> {
    let mut out: Vec<ContentPart> = Vec::with_capacity(parts.len());
    for part in parts {
        match (out.last_mut(), &part) {
            (Some(ContentPart::Text { text }), ContentPart::Text { text: next }) => {
                text.push_str(next);
            }
            // Reasoning arrives one delta at a time like text, so it is merged
            // the same way rather than kept as a part per token.
            (Some(ContentPart::Reasoning { text }), ContentPart::Reasoning { text: next }) => {
                text.push_str(next);
            }
            _ => out.push(part),
        }
    }
    out
}

/// Drains steering and appends it to the history as a user turn.
fn apply_steering(
    history: &mut History,
    queue: &SteeringQueue,
    boundary: Boundary,
    host: &dyn Host,
) {
    let drained = queue.drain(boundary);
    if drained.is_empty() {
        return;
    }
    let text = crate::steering::render_steering(&drained);
    history.push_user(text);
    host.emit(Event::SteeringApplied {
        boundary,
        count: drained.len(),
    });
}

/// Returns the effective outcome for a call given the permission mode.
///
/// Full access resolves everything to allow. The other modes leave the rule
/// evaluation alone, because the rules are the user's expression of intent.
#[must_use]
pub fn effective_outcome(mode: PermissionMode, decided: Outcome) -> Outcome {
    match mode {
        PermissionMode::FullAccess => Outcome::Allow,
        _ => decided,
    }
}

/// Builds a registry lookup helper for a target pattern.
///
/// Present so the loop and the command surface agree on how a target is derived
/// from arguments; a divergence between them would mean a rule matching one path
/// in the loop and another outside it. Registered declarations, including no
/// target, are authoritative. Unregistered built-in names use argument fallback.
#[must_use]
pub fn permission_target_for(
    registry: &Registry,
    name: &str,
    arguments: &serde_json::Value,
) -> Option<String> {
    if registry.contains(name) {
        registry.permission_target(name, arguments)
    } else {
        infer_target_standalone(name, arguments)
    }
}

/// Returns the permission target without a host.
fn infer_target_standalone(name: &str, arguments: &serde_json::Value) -> Option<String> {
    match name {
        "read_file" | "write_file" | "edit_file" | "rename_file" | "create_directory"
        | "glob_files" | "grep_files" | "syntax_diagnostics" => arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        "apply_patch" => arguments
            .get("input")
            .and_then(serde_json::Value::as_str)
            .and_then(|input| rune_tools::apply_patch::paths(input).into_iter().next()),
        "shell" => arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        "web_fetch" => arguments
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

/// Evaluates a rule set for a call, applying the mode.
#[must_use]
pub fn decide_call(
    rules: &RuleSet,
    mode: PermissionMode,
    name: &str,
    target: Option<&str>,
) -> (Outcome, String) {
    let fallback = rune_policy::decision::mode_default(mode);
    let target = target.unwrap_or("");
    let decision = rules.evaluate(name, target, fallback);
    let outcome = effective_outcome(mode, decision.outcome);
    (outcome, decision.explain())
}

/// Evaluates a call using the workspace its file tools resolve paths against.
#[must_use]
pub fn decide_call_in_workspace(
    rules: &RuleSet,
    mode: PermissionMode,
    name: &str,
    target: Option<&str>,
    workspace: &Utf8Path,
) -> (Outcome, String) {
    let fallback = rune_policy::decision::mode_default(mode);
    let decision = rules.evaluate_in_workspace(name, target.unwrap_or(""), fallback, workspace);
    let outcome = effective_outcome(mode, decision.outcome);
    (outcome, decision.explain())
}

/// Signature of a message the loop appends when a step limit is reached.
#[must_use]
pub fn step_limit_notice(limit: u64) -> String {
    format!("The turn reached its limit of {limit} model steps before finishing.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_judge_keeps_the_session_effort_on_anthropic() {
        assert_eq!(judge_effort(rune_net::anthropic::NAME, Effort::Low), None);
        assert_eq!(
            judge_effort(rune_net::chat_completions::NAME, Effort::Low),
            Some(Effort::Low)
        );
    }

    #[test]
    fn a_judge_reply_is_read_from_the_outermost_braces() {
        let verdict = parse_judge_reply(
            "Verdict:\n{\"status\":\"continue\",\"reason\":\" no check ran. \",\"next\":\"run it\",\"extra\":1} thanks",
        );
        assert_eq!(verdict.status, "continue");
        assert_eq!(verdict.reason, "no check ran.");
        assert_eq!(verdict.next, "run it");
    }

    #[test]
    fn an_unreadable_judge_reply_counts_as_done() {
        for reply in [
            "",
            "no json",
            "} backwards {",
            "{not json}",
            "[1]",
            r#"{"reason":"no status"}"#,
            r#"{"status":"maybe","reason":"r"}"#,
            r#"{"status":1}"#,
        ] {
            let verdict = parse_judge_reply(reply);
            assert_eq!(verdict.status, "done", "{reply}");
            assert_eq!(verdict.reason, JUDGE_UNREADABLE, "{reply}");
        }
    }

    #[test]
    fn a_missing_or_null_next_is_empty() {
        let verdict = parse_judge_reply(r#"{"status":"blocked","reason":"r","next":null}"#);
        assert_eq!(verdict.status, "blocked");
        assert_eq!(verdict.next, "");
        assert_eq!(parse_judge_reply(r#"{"status":"done"}"#).reason, "");
    }

    #[test]
    fn a_judge_reason_and_next_are_cut_on_a_character_boundary() {
        let long = "é".repeat(400);
        let reply = serde_json::json!({"status": "continue", "reason": long, "next": long});
        let verdict = parse_judge_reply(&reply.to_string());
        assert_eq!(verdict.reason.len(), 500);
        assert_eq!(verdict.next.len(), 500);
        assert!(verdict.reason.chars().all(|c| c == 'é'));
        assert_eq!(cut_bytes("aé", 2), "a");
        assert_eq!(cut_bytes("ab", 9), "ab");
    }

    #[test]
    fn judge_text_is_not_taken_for_a_placeholder() {
        assert_eq!(
            judge_continue_note("see {next}", "use {reason}"),
            "<harness_check>The objective is not met yet: see {next} Next: use {reason}</harness_check>"
        );
        let prompt = judge_prompt("reach {verdict} and {last_check}", &[], false);
        assert!(
            prompt.contains("Objective: reach {verdict} and {last_check}\n"),
            "{prompt}"
        );
        assert!(
            prompt.contains("Harness record: verification not_applicable.\n"),
            "{prompt}"
        );
    }

    #[test]
    fn an_empty_next_asks_the_model_to_decide() {
        assert_eq!(
            judge_continue_note("r.", ""),
            "<harness_check>The objective is not met yet: r. Next: decide the next step from the evidence above.</harness_check>"
        );
    }

    #[test]
    fn only_a_shell_run_has_a_command() {
        assert_eq!(
            shell_run_command(r#"{"action":"run","command":" cargo test "}"#).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            shell_run_command(r#"{"action":"read","session":"s1"}"#),
            None
        );
        assert_eq!(shell_run_command("not json"), None);
    }

    #[test]
    fn transcript_text_parts_are_coalesced() {
        let parts = vec![
            ContentPart::Text {
                text: "a".to_owned(),
            },
            ContentPart::Text {
                text: "b".to_owned(),
            },
            ContentPart::Reasoning {
                text: "thinking".to_owned(),
            },
            ContentPart::Text {
                text: "c".to_owned(),
            },
        ];
        let merged = coalesce_text(parts);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].as_text(), Some("ab"));
        assert_eq!(merged[2].as_text(), Some("c"));
    }

    #[test]
    fn coalescing_leaves_a_single_part_alone() {
        let parts = vec![ContentPart::Text {
            text: "only".to_owned(),
        }];
        assert_eq!(coalesce_text(parts).len(), 1);
    }

    #[test]
    fn coalescing_an_empty_list_yields_an_empty_list() {
        assert!(coalesce_text(Vec::new()).is_empty());
    }

    #[test]
    fn a_result_inside_its_budget_is_kept_whole() {
        let bounded = bound_result("short".to_owned(), 64);
        assert_eq!(bounded, "short");
    }

    #[test]
    fn a_result_past_its_budget_is_cut_and_charged_in_full() {
        let content = "x".repeat(1024);
        let bounded = bound_result(content, 128);
        assert_eq!(bounded.len(), 128, "the marker was not charged");
        assert!(bounded.ends_with("result limit]"), "{bounded}");
    }

    #[test]
    fn an_exhausted_budget_leaves_no_payload_behind() {
        // Nothing is left to retain, but the call still needs an answer.
        assert!(bound_result("x".repeat(1024), 0).is_empty());
        // A fragment with no room for the marker would look like a whole
        // result, so the body is empty rather than misleading.
        assert!(bound_result("x".repeat(1024), 8).is_empty());
    }

    #[test]
    fn a_cut_result_never_splits_a_character() {
        let content = "\u{1f600}".repeat(64);
        let budget = RESULT_TRUNCATION_MARKER.len() + 6;
        let bounded = bound_result(content, budget);
        assert!(bounded.len() <= budget, "{}", bounded.len());
        assert!(bounded.ends_with(RESULT_TRUNCATION_MARKER), "{bounded}");
    }

    #[test]
    fn full_access_resolves_every_outcome_to_allow() {
        assert_eq!(
            effective_outcome(PermissionMode::FullAccess, Outcome::Deny),
            Outcome::Allow
        );
        assert_eq!(
            effective_outcome(PermissionMode::FullAccess, Outcome::Ask),
            Outcome::Allow
        );
    }

    #[test]
    fn other_modes_leave_the_rule_outcome_alone() {
        for mode in [PermissionMode::Ask, PermissionMode::Auto] {
            assert_eq!(effective_outcome(mode, Outcome::Deny), Outcome::Deny);
            assert_eq!(effective_outcome(mode, Outcome::Allow), Outcome::Allow);
        }
    }

    #[test]
    fn a_call_with_no_rules_falls_back_to_the_mode() {
        let rules = RuleSet::new();
        let (outcome, _) = decide_call(&rules, PermissionMode::Ask, "shell", Some("ls"));
        assert_eq!(outcome, Outcome::Ask);
        let (outcome, _) = decide_call(&rules, PermissionMode::FullAccess, "shell", Some("ls"));
        assert_eq!(outcome, Outcome::Allow);
    }

    #[test]
    fn a_rule_decides_the_call_and_explains_itself() {
        let mut rules = RuleSet::new();
        rules.push(rune_policy::rules::Rule::deny(
            "*",
            "git push*",
            rune_policy::decision::Layer::User,
        ));
        let (outcome, reason) = decide_call(
            &rules,
            PermissionMode::Ask,
            "shell",
            Some("git push origin"),
        );
        assert_eq!(outcome, Outcome::Deny);
        assert!(reason.contains("git push"), "{reason}");
    }

    #[test]
    fn full_access_overrides_a_deny_rule() {
        let mut rules = RuleSet::new();
        rules.push(rune_policy::rules::Rule::deny(
            "*",
            "*",
            rune_policy::decision::Layer::User,
        ));
        let (outcome, _) = decide_call(&rules, PermissionMode::FullAccess, "shell", Some("ls"));
        assert_eq!(outcome, Outcome::Allow);
    }

    #[test]
    fn activity_is_inferred_for_each_builtin_name() {
        assert_eq!(infer_activity("read_file"), Activity::Read);
        assert_eq!(infer_activity("git_diff"), Activity::Read);
        assert_eq!(infer_activity("syntax_diagnostics"), Activity::Read);
        assert_eq!(infer_activity("glob_files"), Activity::List);
        assert_eq!(infer_activity("grep_files"), Activity::Search);
        assert_eq!(infer_activity("write_file"), Activity::Write);
        assert_eq!(infer_activity("edit_file"), Activity::Edit);
        assert_eq!(infer_activity("shell"), Activity::Execute);
        assert_eq!(infer_activity("web_fetch"), Activity::Network);
        assert_eq!(infer_activity("ask_user_question"), Activity::Interact);
    }

    #[test]
    fn an_unknown_tool_name_is_treated_as_an_execution() {
        assert_eq!(infer_activity("something_new"), Activity::Execute);
    }

    #[test]
    fn the_permission_target_is_read_from_the_documented_argument() {
        let arguments = serde_json::json!({ "path": "src/main.rs" });
        assert_eq!(
            infer_target_standalone("read_file", &arguments).as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            infer_target_standalone("edit_file", &arguments).as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            infer_target_standalone("syntax_diagnostics", &arguments).as_deref(),
            Some("src/main.rs")
        );
    }

    #[test]
    fn the_shell_target_is_the_command() {
        let arguments = serde_json::json!({ "command": "git status" });
        assert_eq!(
            infer_target_standalone("shell", &arguments).as_deref(),
            Some("git status")
        );
    }

    #[test]
    fn a_tool_with_no_target_returns_none() {
        let arguments = serde_json::json!({ "name": "x" });
        assert!(infer_target_standalone("subagent", &arguments).is_none());
    }

    #[test]
    fn stop_reasons_render_their_wire_names() {
        assert_eq!(StopReason::Completed.as_str(), "end_turn");
        assert_eq!(StopReason::StepLimit.as_str(), "max_model_turns");
        assert_eq!(StopReason::OutputLimit.as_str(), "max_output_tokens");
        assert_eq!(StopReason::Cancelled.as_str(), "cancelled");
    }

    #[test]
    fn only_completion_and_the_step_limit_are_successes() {
        assert!(StopReason::Completed.is_success());
        assert!(StopReason::StepLimit.is_success());
        assert!(!StopReason::OutputLimit.is_success());
        assert!(!StopReason::Refused.is_success());
        assert!(!StopReason::Cancelled.is_success());
        assert!(!StopReason::ContentFilter.is_success());
        assert!(!StopReason::ProviderFailure.is_success());
    }

    #[test]
    fn the_step_limit_notice_names_the_limit() {
        let notice = step_limit_notice(40);
        assert!(notice.contains("40"), "{notice}");
    }
}
