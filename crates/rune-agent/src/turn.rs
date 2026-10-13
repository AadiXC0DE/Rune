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

use std::time::Duration;

use camino::Utf8Path;
use rune_core::budget::{BudgetSet, LimitName};
use rune_core::config::{Effort, PermissionMode};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_net::fetch::Fetch;
use rune_net::message::{ContentPart, ToolSpec};
use rune_net::provider::{Provider, RequestPlan, ToolChoice};
use rune_net::stream::{FinishReason, Usage};
use rune_net::transport::{self, Endpoint, StreamOutcome};
use rune_policy::decision::Outcome;
use rune_policy::rules::RuleSet;
use rune_tools::contract::{Activity, ExecutionContext, ToolOutput};
use rune_tools::{Registry, Tool};

use crate::history::History;
use crate::retry::{self, RetryPolicy};
use crate::steering::{Boundary, Cancellation, SteeringQueue};

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
    /// Arguments as a JSON string, exactly as the model produced them.
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
        /// Whether the tool reported a failure.
        is_error: bool,
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
    /// Earlier context was summarized before a model request.
    ContextCompacted {
        /// Original turns replaced by the summary.
        removed_turns: usize,
        /// Turns remaining, including the summary.
        remaining_turns: usize,
    },
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
    fn tools(&self) -> Vec<ToolSpec>;

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
}

/// Outcome of a completed turn.
#[derive(Clone, Debug)]
pub struct TurnOutcome {
    /// How the turn ended.
    pub stop_reason: StopReason,
    /// Assistant text produced across every step.
    pub text: String,
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
}

/// Runs one turn to completion.
///
/// Returns the outcome; a failure to reach a terminal state is an error, not a
/// partial success.
pub fn run_turn(history: &mut History, host: &dyn Host) -> Result<TurnOutcome> {
    let limits = host.limits();
    let cancellation = host.cancellation();
    let steering = host.steering();
    let step_limit = limits.get(LimitName::MaxAgentSteps).value().unwrap_or(0);
    let max_attempts = limits.get_usize(LimitName::ProviderMaxAttempts).max(1);
    let timeouts = transport::RequestTimeouts::from_limits(&limits);

    let instructions = host.instructions();
    history.set_instructions(instructions);
    history.validate()?;

    let client = host.fetch();
    let mut usage = Usage::default();
    let mut last_request = Usage::default();
    let mut calls = Vec::new();
    // Reasoning accumulated across steps, so a multi-step turn keeps all of it.
    let mut reasoning = String::new();
    let mut steps: u32 = 0;
    // Tool-result bytes this turn has retained, shared across its steps.
    let mut result_bytes: usize = 0;
    // The latest step's answer, kept so a turn stopped at the step limit still
    // returns what the model said in its last step.
    let mut text = String::new();

    loop {
        cancellation.check()?;

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
                reasoning,
                usage,
                last_request,
                steps,
                calls,
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

        steps = steps.saturating_add(1);
        host.emit(Event::TurnStarted { step: steps });

        let (outcome, steering_arrived) = stream_with_retry(
            client.as_ref(),
            host,
            history,
            steps,
            timeouts,
            max_attempts,
            &cancellation,
        )?;

        host.observe_response(&outcome);
        usage = usage.merge_sum(outcome.usage);
        last_request = outcome.usage;

        text = outcome.text();
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

        // Record the assistant turn, including any replay state the provider
        // requires on the next request.
        let mut parts: Vec<ContentPart> = Vec::new();
        let mut pending_calls = Vec::new();
        for event in &outcome.events {
            match event {
                // The host already received this delta as it arrived; here it is
                // only assembled into the turn's record.
                rune_net::stream::ProviderEvent::TextDelta { delta } => {
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
        if !parts.is_empty() || !pending_calls.is_empty() {
            // The replay value travels with the turn that produced it, because
            // a later request must send it back unchanged and must not inherit
            // it from an earlier turn.
            history.push_assistant_with_replay(parts, outcome.replay.clone());
        }

        if pending_calls.is_empty() {
            // Steering that arrived while the last request was streaming gets a
            // response in this turn rather than the next one. Ending here would
            // drop it: the loop returns without reaching another drain, and a
            // correction typed against the answer being written would never be
            // seen. Continuing is what makes the correction feel delivered.
            //
            // It is drained here rather than left for the next model boundary,
            // so it is recorded and reported once, at the boundary it actually
            // arrived at.
            if !steering.is_empty() && host.objective_tokens_remaining() != Some(0) {
                apply_steering(history, steering, Boundary::Finalizing, host);
                continue;
            }
            let reason = if host.objective_tokens_remaining() == Some(0) {
                StopReason::ObjectiveBudget
            } else {
                match finish {
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
            host.emit(Event::Finished {
                reason,
                usage,
                last_request,
                steps,
            });
            return Ok(TurnOutcome {
                stop_reason: reason,
                text,
                reasoning,
                usage,
                last_request,
                steps,
                calls,
            });
        }

        // Execute the batch. Policy is resolved per call before anything runs,
        // so a denial never reaches a tool.
        let (results, failure_limit_reached) = execute_batch(
            &pending_calls,
            host,
            history,
            &mut result_bytes,
            &limits,
            &calls,
        );

        let mut result_parts = Vec::new();
        for result in &results {
            host.post_tool(result);
            result_parts.push(ContentPart::ToolResult {
                id: rune_core::id::ToolCallId::new(result.call.id.clone())?,
                name: result.call.name.clone(),
                content: result.output.render(),
                is_error: result.output.is_error,
            });
        }
        history.push_tool_results(result_parts);
        calls.extend(results);

        if failure_limit_reached {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                "stopped before repeating a tool call at max_consecutive_tool_failures",
            )
            .with_invariant(LimitName::MaxConsecutiveToolFailures.as_str()));
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
                reasoning,
                usage,
                last_request,
                steps,
                calls,
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
        if host.endpoint().offline {
            return Ok(());
        }
        let Some(input_estimate) = self.input_estimate else {
            return Ok(());
        };
        let usage = self.usage.merge_max(reported.unwrap_or_default());
        let input = usage.input_tokens.map_or(input_estimate, |input| {
            if host.dialect().name() == rune_net::anthropic::NAME {
                input
                    .saturating_add(usage.cache_read_tokens.unwrap_or(0))
                    .saturating_add(usage.cache_write_tokens.unwrap_or(0))
            } else {
                input
            }
        });
        let output = usage.output_tokens.map_or_else(
            || {
                self.output_bytes
                    .div_ceil(crate::tokens::DEFAULT_BYTES_PER_TOKEN)
            },
            |output| {
                // Gemini reports thinking separately from candidate output. Other
                // dialects already include reasoning in their output totals.
                if host.dialect().name() == "gemini" {
                    output.saturating_add(usage.reasoning_tokens.unwrap_or(0))
                } else {
                    output
                }
            },
        );
        host.account_objective_tokens(input.saturating_add(output))
    }
}

/// Streams a request, retrying a transient failure within a bounded budget.
///
/// Applies queued steering before rebuilding a retry request. Returns the
/// outcome together with a flag saying whether steering is queued after the
/// successful attempt, for the caller to apply at the next boundary.
fn stream_with_retry(
    client: &dyn Fetch,
    host: &dyn Host,
    history: &mut History,
    step: u32,
    timeouts: transport::RequestTimeouts,
    max_attempts: usize,
    cancellation: &Cancellation,
) -> Result<(StreamOutcome, bool)> {
    let steering = host.steering();
    let mut attempt = 0_usize;
    let mut last: Option<RuneError> = None;
    let retry_policy = RetryPolicy::new(max_attempts);

    loop {
        cancellation.check()?;

        check_objective_budget(host)?;
        if attempt >= max_attempts {
            return Err(last.unwrap_or_else(|| {
                RuneError::new(ErrorCode::TransportFailure, "the request did not succeed")
            }));
        }
        if attempt > 0 {
            host.emit(Event::StepRestarted { step });
            // Include corrections from the failed stream or retry delay before
            // constructing the plan and preparing its context.
            apply_steering(history, steering, Boundary::AfterFailure, host);
        }
        attempt = attempt.saturating_add(1);

        let mut plan = RequestPlan::new(host.model());
        plan.instructions = host.instructions();
        plan.messages = history.to_messages();
        plan.tools = host.tools();
        plan.tool_choice = if plan.tools.is_empty() {
            ToolChoice::None
        } else {
            ToolChoice::Auto
        };
        plan.parallel_tool_calls =
            host.limits().get(LimitName::ParallelToolCalls).value() != Some(1);
        plan.effort = host.effort();
        plan.fast_mode = host.fast_mode();
        plan.provider_order = host.provider_order();
        plan.provider_strict = host.provider_strict();
        host.prepare_request(history, &mut plan, client)?;
        cancellation.check()?;
        let mut accounting = ObjectiveAccounting::begin(host, &mut plan)?;

        // Each event is handed to the host as it is decoded, so text appears
        // while it is being produced rather than once the response has ended.
        // A retry re-sends the request, so the host is told the step restarted
        // and the partial text of the failed attempt is dropped.
        let mut observe = |event: &rune_net::stream::ProviderEvent| {
            accounting.observe(event);
            match event {
                rune_net::stream::ProviderEvent::TextDelta { delta } => {
                    host.emit(Event::TextDelta {
                        delta: delta.clone(),
                    });
                }
                rune_net::stream::ProviderEvent::ReasoningDelta { delta } => {
                    host.emit(Event::ReasoningDelta {
                        delta: delta.clone(),
                    });
                }
                _ => {}
            }
        };

        let response = transport::stream_completion_observed(
            client,
            host.endpoint(),
            host.dialect(),
            &plan,
            timeouts,
            &|| cancellation.is_cancelled(),
            &mut observe,
        );
        accounting.finish(host, response.as_ref().ok().map(|outcome| outcome.usage))?;
        match response {
            Ok(outcome) => {
                if attempt > 1 {
                    host.emit(Event::ProviderRetryFinished { step });
                }
                return Ok((outcome, !steering.is_empty()));
            }
            Err(err) => {
                let Some(delay) = retry_policy.delay(attempt, err.kind(), err.retry_after_ms())
                else {
                    return Err(err.to_rune_error());
                };
                check_objective_budget(host)?;
                last = Some(err.to_rune_error());
                host.emit(Event::ProviderRetry {
                    step,
                    next_attempt: attempt.saturating_add(1),
                    max_attempts,
                    delay,
                });
                retry::wait(delay, cancellation)?;
            }
        }
    }
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
fn execute_batch(
    calls: &[PreparedCall],
    host: &dyn Host,
    history: &mut History,
    result_bytes: &mut usize,
    limits: &BudgetSet,
    previous: &[CallResult],
) -> (Vec<CallResult>, bool) {
    let mut results = Vec::with_capacity(calls.len());
    let mut cancelled = false;
    let mut stopped = false;
    let failure_limit = limits.get(LimitName::MaxConsecutiveToolFailures).value();
    let offered: Vec<String> = host.tools().into_iter().map(|spec| spec.name).collect();
    let turn_cap = limits.get_usize(LimitName::MaxTurnResultBytes);
    let tool_cap = limits.get_usize(LimitName::MaxToolResultBytes);
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
    #[cfg(not(target_family = "wasm"))]
    let parallel_prefix = if failure_limit.is_some() {
        calls[..parallel_prefix]
            .windows(2)
            .position(|pair| identical_call(&pair[0], &pair[1]))
            .map_or(parallel_prefix, |index| index.saturating_add(1))
    } else {
        parallel_prefix
    };
    #[cfg(target_family = "wasm")]
    let parallel_prefix = 0;

    #[cfg(not(target_family = "wasm"))]
    for group in calls[..parallel_prefix].chunks(width) {
        let group_results = std::thread::scope(|scope| {
            let mut pending = Vec::with_capacity(group.len());
            for (index, call) in group.iter().enumerate() {
                // Later calls in this group differ from their predecessor,
                // so they cannot extend an existing failure streak.
                let blocked = if index == 0 || stopped {
                    failure_guard(
                        call,
                        host,
                        previous,
                        &results,
                        failure_limit,
                        cancelled,
                        &mut stopped,
                    )
                } else {
                    None
                };
                let prepared = match blocked {
                    Some(result) => Err(result),
                    None => prepare_call(call, host, &offered, &mut cancelled),
                };
                let execution = match prepared {
                    Ok((arguments, context)) => {
                        let executor = host.concurrent_tool_host(&call.name);
                        scope.spawn(move || {
                            let executor = executor.ok_or_else(|| {
                                RuneError::new(ErrorCode::Internal, "read executor disappeared")
                            })?;
                            executor.cancellation().check()?;
                            context.check_cancelled()?;
                            executor.execute_with_context(&call.name, &arguments, &context)
                        })
                    }
                    Err(result) => {
                        pending.push((call, Err(result)));
                        continue;
                    }
                };
                pending.push((call, Ok(execution)));
            }
            pending
                .into_iter()
                .map(|(call, pending)| match pending {
                    Err(result) => result,
                    Ok(worker) => {
                        let execution = match worker.join() {
                            Ok(output) => output,
                            Err(payload) => std::panic::resume_unwind(payload),
                        };
                        finish_call(call, host, execution, &mut cancelled)
                    }
                })
                .collect::<Vec<_>>()
        });
        for result in group_results {
            retain_result(
                result,
                history,
                result_bytes,
                turn_cap,
                tool_cap,
                &mut results,
            );
        }
    }
    for call in &calls[parallel_prefix..] {
        let cap = turn_cap.saturating_sub(*result_bytes).min(tool_cap);
        let result = failure_guard(
            call,
            host,
            previous,
            &results,
            failure_limit,
            cancelled,
            &mut stopped,
        )
        .unwrap_or_else(|| execute_call(call, host, &offered, &mut cancelled, history, cap));
        retain_result(
            result,
            history,
            result_bytes,
            turn_cap,
            tool_cap,
            &mut results,
        );
    }
    (results, stopped)
}

/// Call IDs identify requests, not the operation being repeated.
fn identical_call(left: &PreparedCall, right: &PreparedCall) -> bool {
    left.name == right.name && left.arguments == right.arguments
}

/// Checks failures in request order, across batches but only within this turn.
fn failure_guard(
    call: &PreparedCall,
    host: &dyn Host,
    previous: &[CallResult],
    results: &[CallResult],
    limit: Option<u64>,
    cancelled: bool,
    stopped: &mut bool,
) -> Option<CallResult> {
    // Cancellation retains its existing outcome and answers pending calls.
    if cancelled || host.cancellation().is_cancelled() {
        return None;
    }
    if !*stopped {
        let limit = limit?;
        let failures = previous
            .iter()
            .chain(results)
            .rev()
            .take_while(|result| result.output.is_error && identical_call(&result.call, call))
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
            .count();
        *stopped = u64::try_from(failures).unwrap_or(u64::MAX) >= limit;
    }
    if !*stopped {
        return None;
    }
    let reason = "not run: the turn reached max_consecutive_tool_failures";
    host.emit(Event::ToolDenied {
        call: call.clone(),
        reason: reason.to_owned(),
    });
    Some(CallResult {
        call: call.clone(),
        output: ToolOutput::failure(reason),
        executed: false,
    })
}

/// Retains and charges answers in request order, regardless of execution order.
fn retain_result(
    mut result: CallResult,
    history: &mut History,
    result_bytes: &mut usize,
    turn_cap: usize,
    tool_cap: usize,
    results: &mut Vec<CallResult>,
) {
    let cap = turn_cap.saturating_sub(*result_bytes).min(tool_cap);
    let text = std::mem::take(&mut result.output.text);
    let bounded = if text.len() > cap {
        spill_result(history, &result, text, cap)
    } else {
        text
    };
    *result_bytes = result_bytes.saturating_add(bounded.len());
    result.output.text = bounded;
    results.push(result);
}

/// Resolves advertisement, arguments and authority on the turn thread.
fn prepare_call(
    call: &PreparedCall,
    host: &dyn Host,
    offered: &[String],
    cancelled: &mut bool,
) -> std::result::Result<(serde_json::Value, ExecutionContext), CallResult> {
    if *cancelled || host.cancellation().is_cancelled() {
        *cancelled = true;
        return Err(CallResult {
            call: call.clone(),
            output: ToolOutput::failure("not run: the turn was cancelled"),
            executed: false,
        });
    }

    if !offered.iter().any(|name| name == &call.name) {
        let reason = format!(
            "there is no tool named `{}`; the tools are {}",
            call.name,
            offered.join(", ")
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

    let activity = host
        .tool_activity(&call.name)
        .unwrap_or_else(|| infer_activity(&call.name));
    let arguments: serde_json::Value = match serde_json::from_str(&call.arguments) {
        Ok(value) => value,
        Err(err) => {
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

    let target = host.tool_permission_target(&call.name, &arguments);
    let (outcome, reason) = host.decide(&call.name, target.as_deref());

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
    let context = host.context().with_private_network_access(private_access);

    host.emit(Event::ToolStarted {
        call: call.clone(),
        activity,
    });
    Ok((arguments, context))
}

/// Executes one offered, policy-approved call and preserves batch cancellation.
fn execute_call(
    call: &PreparedCall,
    host: &dyn Host,
    offered: &[String],
    cancelled: &mut bool,
    history: &History,
    output_cap: usize,
) -> CallResult {
    let (arguments, context) = match prepare_call(call, host, offered, cancelled) {
        Ok(prepared) => prepared,
        Err(result) => return result,
    };
    // Retained output is owned by the history rather than the host. Supply
    // that store only after advertisement and policy checks have passed.
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
    finish_call(call, host, execution, cancelled)
}

/// Reports execution and preserves a cancellation for calls still pending.
fn finish_call(
    call: &PreparedCall,
    host: &dyn Host,
    execution: Result<ToolOutput>,
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
        is_error: output.is_error,
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
        "edit_file" => Activity::Edit,
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
