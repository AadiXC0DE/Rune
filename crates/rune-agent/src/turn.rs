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
//!   exhausted retry budget stops it.
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
use crate::steering::{Boundary, Cancellation, SteeringQueue};

/// How a turn ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopReason {
    /// The model finished naturally.
    Completed,
    /// The model stopped asking for tools at the step limit.
    StepLimit,
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
    /// A tool call was refused by policy.
    ToolDenied {
        /// The call.
        call: PreparedCall,
        /// The rule that decided it.
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

    /// Reports an event.
    fn emit(&self, event: Event);

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
    /// Hosts running web tools must pass this context to their implementation.
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
        if step_limit > 0 && u64::from(steps) >= step_limit {
            let outcome = TurnOutcome {
                stop_reason: StopReason::StepLimit,
                text,
                reasoning,
                usage,
                last_request,
                steps,
                calls,
            };
            host.emit(Event::Finished {
                reason: StopReason::StepLimit,
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
            if !steering.is_empty() {
                apply_steering(history, steering, Boundary::Finalizing, host);
                continue;
            }
            let reason = match finish {
                FinishReason::Stop => StopReason::Completed,
                FinishReason::MaxTokens => StopReason::OutputLimit,
                FinishReason::Refused => StopReason::Refused,
                FinishReason::ContentFilter => StopReason::ContentFilter,
                FinishReason::Cancelled => StopReason::Cancelled,
                FinishReason::ToolCalls => StopReason::Completed,
                FinishReason::MaxModelTurns => StopReason::StepLimit,
                FinishReason::ProviderError => StopReason::ProviderFailure,
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
        let results = execute_batch(&pending_calls, host, history, &mut result_bytes, &limits);

        let mut result_parts = Vec::new();
        for result in &results {
            result_parts.push(ContentPart::ToolResult {
                id: rune_core::id::ToolCallId::new(result.call.id.clone())?,
                name: result.call.name.clone(),
                content: result.output.render(),
                is_error: result.output.is_error,
            });
        }
        history.push_tool_results(result_parts);
        calls.extend(results);

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

        // Drain steering that arrived during the request, then continue so the
        // next attempt or the next step sees it.
        if steering_arrived {
            apply_steering(history, steering, Boundary::AfterFailure, host);
        }
    }
}

/// Streams a request, retrying a transient failure within a bounded budget.
///
/// Returns the outcome together with a flag saying whether steering arrived
/// while the request was failing. The caller applies it, because the history is
/// borrowed here to build each attempt.
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

    loop {
        cancellation.check()?;

        if attempt >= max_attempts {
            return Err(last.unwrap_or_else(|| {
                RuneError::new(ErrorCode::TransportFailure, "the request did not succeed")
            }));
        }
        if attempt > 0 {
            host.emit(Event::StepRestarted { step });
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
        plan.effort = host.effort();
        plan.fast_mode = host.fast_mode();
        plan.provider_order = host.provider_order();
        plan.provider_strict = host.provider_strict();
        host.prepare_request(history, &mut plan, client)?;
        cancellation.check()?;

        // Each event is handed to the host as it is decoded, so text appears
        // while it is being produced rather than once the response has ended.
        // A retry re-sends the request, so the host is told the step restarted
        // and the partial text of the failed attempt is dropped.
        let mut observe = |event: &rune_net::stream::ProviderEvent| match event {
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
        };

        match transport::stream_completion_observed(
            client,
            host.endpoint(),
            host.dialect(),
            &plan,
            timeouts,
            &|| cancellation.is_cancelled(),
            &mut observe,
        ) {
            Ok(outcome) => return Ok((outcome, !steering.is_empty())),
            Err(err) => {
                if !err.is_retryable() {
                    return Err(err.to_rune_error());
                }

                // Steering may have arrived while the request was failing, and
                // it can change the request enough that retrying is worth it.
                let delay = err
                    .retry_after_ms()
                    .map_or_else(|| backoff(attempt), Duration::from_millis);
                last = Some(err.to_rune_error());

                // Sleep in small slices so a cancellation is noticed promptly
                // rather than after the whole delay.
                let started = std::time::Instant::now();
                while started.elapsed() < delay {
                    cancellation.check()?;
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }
}

/// Returns the delay before an attempt.
///
/// Doubles from a small base and caps, so a short outage is retried promptly and
/// a long one does not hammer the endpoint.
fn backoff(attempt: usize) -> Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1)).unwrap_or(0).min(5);
    let factor = 1_u64.checked_shl(exponent).unwrap_or(u64::MAX);
    let millis = 250_u64.saturating_mul(factor);
    Duration::from_millis(millis.min(8_000))
}

/// Resolves and executes a batch of tool calls.
///
/// Read-only calls at the front of the batch run concurrently; everything after
/// the first mutating call runs in order, so a write can never race a read of
/// the same file.
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
) -> Vec<CallResult> {
    let mut results = Vec::with_capacity(calls.len());
    let mut cancelled = false;
    let offered: Vec<String> = host.tools().into_iter().map(|spec| spec.name).collect();
    let turn_cap = limits.get_usize(LimitName::MaxTurnResultBytes);
    let tool_cap = limits.get_usize(LimitName::MaxToolResultBytes);
    for call in calls {
        let cap = turn_cap.saturating_sub(*result_bytes).min(tool_cap);
        let mut result = execute_call(call, host, &offered, &mut cancelled, history, cap);
        // Charge each answer before the next call, so a reader in this batch
        // receives the remaining budget and can read a preceding spilled result.
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
    results
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
    if *cancelled {
        return CallResult {
            call: call.clone(),
            output: ToolOutput::failure("not run: the turn was cancelled"),
            executed: false,
        };
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
        return CallResult {
            call: call.clone(),
            output: ToolOutput::failure(reason),
            executed: false,
        };
    }

    let activity = infer_activity(&call.name);
    let arguments: serde_json::Value = match serde_json::from_str(&call.arguments) {
        Ok(value) => value,
        Err(err) => {
            // A malformed argument string is the model's error to correct.
            let output = ToolOutput::failure(format!(
                "the arguments for `{}` are not valid JSON: {err}",
                call.name
            ));
            return CallResult {
                call: call.clone(),
                output,
                executed: false,
            };
        }
    };

    let target = infer_target(host, &call.name, &arguments);
    let (outcome, reason) = host.decide(&call.name, target.as_deref());

    match outcome {
        Outcome::Deny => {
            host.emit(Event::ToolDenied {
                call: call.clone(),
                reason: reason.clone(),
            });
            return CallResult {
                call: call.clone(),
                output: ToolOutput::failure(format!(
                    "`{}` was refused by policy: {reason}",
                    call.name
                )),
                executed: false,
            };
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
            return CallResult {
                call: call.clone(),
                output: ToolOutput::failure(format!(
                    "`{}` was not run: no rule allows `{action}`, and this session has no \
                     way to ask for approval. Do not retry it or a variation of it. Continue \
                     without it, or tell the user what to run or which rule to add.",
                    call.name
                )),
                executed: false,
            };
        }
        Outcome::Allow => {}
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
            return CallResult {
                call: call.clone(),
                output: ToolOutput::failure(format!(
                    "private-network access was not approved: {reason}"
                )),
                executed: false,
            };
        }
    }
    let context = host.context().with_private_network_access(private_access);

    host.emit(Event::ToolStarted {
        call: call.clone(),
        activity,
    });

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
        host.execute_with_context(&call.name, &arguments, &context)
    };
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
        // The hint is part of the answer: it is what tells the model how
        // to correct the call.
        Err(err) => ToolOutput::failure(match err.hint() {
            Some(hint) => format!("{}; {hint}", err.message()),
            None => err.message().to_owned(),
        }),
    };

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

/// Returns the activity for a tool name.
///
/// A name the registry does not know is treated as an execution, which is the
/// conservative answer for presentation purposes.
fn infer_activity(name: &str) -> Activity {
    match name {
        "read_file" | "read_tool_result" => Activity::Read,
        "glob_files" => Activity::List,
        "grep_files" => Activity::Search,
        "write_file" => Activity::Write,
        "edit_file" => Activity::Edit,
        "shell" => Activity::Execute,
        "web_fetch" | "web_search" => Activity::Network,
        "subagent" => Activity::Delegate,
        "ask_user_question" => Activity::Interact,
        _ => Activity::Execute,
    }
}

/// Returns the permission target for a call, when the tool names one.
///
/// Each tool declares its own target, so the loop asks the registry rather than
/// keeping a second table that can drift from it. The host does not hand the
/// loop its registry, so the built-in one answers, and it gives the same
/// target: a tool's target depends only on its arguments and on state every
/// instance reads, such as the command a shell session runs.
fn infer_target(host: &dyn Host, name: &str, arguments: &serde_json::Value) -> Option<String> {
    static BUILTIN: std::sync::OnceLock<Option<Registry>> = std::sync::OnceLock::new();
    let _ = host;
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
/// in the loop and another outside it.
#[must_use]
pub fn permission_target_for(
    registry: &Registry,
    name: &str,
    arguments: &serde_json::Value,
) -> Option<String> {
    registry
        .permission_target(name, arguments)
        .or_else(|| infer_target_standalone(name, arguments))
}

/// Returns the permission target without a host.
fn infer_target_standalone(name: &str, arguments: &serde_json::Value) -> Option<String> {
    match name {
        "read_file" | "write_file" | "edit_file" | "glob_files" | "grep_files" => arguments
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
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(1), Duration::from_millis(250));
        assert_eq!(backoff(2), Duration::from_millis(500));
        assert_eq!(backoff(3), Duration::from_millis(1000));
        assert_eq!(backoff(4), Duration::from_millis(2000));
        assert_eq!(backoff(5), Duration::from_millis(4000));
        assert_eq!(backoff(6), Duration::from_millis(8000));
        assert_eq!(backoff(20), Duration::from_millis(8000));
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
