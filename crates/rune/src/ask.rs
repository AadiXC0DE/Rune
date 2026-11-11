//! The one-shot runner.
//!
//! Runs a bounded turn, prints the assistant output to standard output and
//! every diagnostic to standard error, and exits. With `--json` it prints one
//! object instead; `--jsonl` streams bounded event records. This path supports
//! host tools under the configured permission policy without interactive approval.
//! Completed exchanges are saved unless `--no-save` is selected.
//!
//! The key set and its order are fixed. `output` is the text produced during the
//! request; `final_output` is the completed final response. A usage count the
//! provider did not report is `null`, and a reported zero stays `0`, because the
//! difference matters to a caller summing them.

use std::io::Write;

use rune_agent::history::History;
use rune_agent::retry::RetryPolicy;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{self, Event, Host, StopReason};
use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::config::{Layer, Settings};
use rune_core::error::{ErrorCode, ErrorDetail, Result, RuneError};
use rune_core::id::SessionId;
use rune_core::paths::Paths;
use rune_net::message::{ContentPart, ToolSpec};
use rune_net::provider::Provider;
use rune_net::stream::{FinishReason, Usage};
use rune_net::transport::{Endpoint, StreamOutcome};
use rune_policy::{Outcome, RuleSet};
use rune_session::{SessionEvent, SessionStore};
use rune_tools::{ExecutionContext, Registry, ToolOutput, inventory};
use serde::Serialize;

/// Exit code for a successful run.
pub const EXIT_OK: u8 = 0;

/// Exit code for a failed run.
pub const EXIT_FAILURE: u8 = 1;

/// Exit status when a limit ended the run and its output is usable.
pub const EXIT_LIMIT: u8 = 3;

/// Exit status when SIGINT stopped the run.
pub const EXIT_SIGINT: u8 = 130;

/// Exit status when SIGTERM or SIGHUP stopped the run.
pub const EXIT_SIGTERM: u8 = 143;

/// The exit code for a finished run.
///
/// A script reads the outcome from it alone: 0 finished, 1 failed, 3 a limit
/// ended the run with a usable result, 130 and 143 a signal stopped it.
#[must_use]
pub fn exit_code(stop: StopReason, error: Option<&RuneError>, signal: Option<i32>) -> u8 {
    if let Some(signal) = signal {
        return if signal == crate::shutdown::SIGINT {
            EXIT_SIGINT
        } else {
            EXIT_SIGTERM
        };
    }
    match stop {
        StopReason::Completed if error.is_none() => EXIT_OK,
        StopReason::StepLimit
        | StopReason::OutputLimit
        | StopReason::TimeLimit
        | StopReason::SpendLimit
        | StopReason::ObjectiveBudget => EXIT_LIMIT,
        StopReason::Completed
        | StopReason::ProviderFailure
        | StopReason::Refused
        | StopReason::ContentFilter
        | StopReason::Stalled
        | StopReason::Cancelled => EXIT_FAILURE,
    }
}

/// Largest prompt accepted from standard input.
pub const MAX_STDIN_PROMPT_BYTES: usize = 8 * 1024 * 1024;

/// Options for one run.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The prompt text.
    pub prompt: String,
    /// Explicit local image paths to attach to the user prompt.
    pub images: Vec<std::path::PathBuf>,
    /// Emit one JSON object instead of Markdown.
    pub json: bool,
    /// Stream bounded JSONL records instead of Markdown or one JSON result.
    pub jsonl: bool,
    /// Do not create or update a session.
    pub no_save: bool,
    /// Execute host tools in a bounded loop.
    pub tool_loop: bool,
    /// Refuse tools that do not declare themselves read-only, before policy grants.
    pub read_only: bool,
    /// Step cap from the command line: `Some(0)` removes any cap.
    pub max_steps: Option<u64>,
    /// Wall-clock limit for the whole run.
    pub timeout: Option<std::time::Duration>,
    /// Spending limit from `--max-cost`, in cents.
    pub max_cost_cents: Option<u64>,
    /// Id to save the run under, chosen before the run so the JSONL `start`
    /// record can name it; one is generated when absent.
    pub session_id: Option<SessionId>,
    /// The objective added to the instructions.
    pub goal: Option<String>,
    /// Check the result against the objective before finishing.
    pub judge: bool,
}

/// Appended to the prompt of a run nobody can answer.
pub const RUN_CONTEXT: &str = "<run_context>This is a non-interactive run. Nobody can answer questions or approve actions. When a choice is yours to make, choose and continue. When it is truly the user's, pick the most reasonable option, say which and why, and continue. End with what changed, how it was checked, and what remains.</run_context>";

/// The same note for a read-only review, which changes nothing.
pub const REVIEW_RUN_CONTEXT: &str = "<run_context>This is a non-interactive run. Nobody can answer questions or approve actions. When a choice is yours to make, choose and continue. When it is truly the user's, pick the most reasonable option, say which and why, and continue. End with what you found, how it was checked, and what remains.</run_context>";

/// The text of a run's first user message: the prompt, then, when the run
/// uses tools, the note that nobody can answer it.
///
/// The note is in the user turn rather than the system prompt so the system
/// prompt stays the same as an interactive session's, cache prefix included.
fn first_message(prompt: &str, tool_loop: bool, read_only: bool) -> String {
    if !tool_loop {
        return prompt.to_owned();
    }
    let note = if read_only {
        REVIEW_RUN_CONTEXT
    } else {
        RUN_CONTEXT
    };
    format!("{prompt}\n\n{note}")
}

/// The limits instructions are built with, after the `map` experiment: `off`
/// turns the repository map off, `on` turns it on at its default size when the
/// configured size is zero.
pub(crate) fn map_limits(
    limits: &BudgetSet,
    experiments: rune_core::experiment::Experiments,
) -> std::borrow::Cow<'_, BudgetSet> {
    let bytes = match experiments.map {
        None => return std::borrow::Cow::Borrowed(limits),
        Some(false) => Budget::Bounded(0),
        Some(true) => {
            if limits
                .get(LimitName::RepositoryMapBytes)
                .value()
                .unwrap_or(0)
                > 0
            {
                return std::borrow::Cow::Borrowed(limits);
            }
            LimitName::RepositoryMapBytes.default_value()
        }
    };
    let mut forced = limits.clone();
    if forced
        .set(LimitName::RepositoryMapBytes, bytes, Layer::Environment)
        .is_err()
    {
        return std::borrow::Cow::Borrowed(limits);
    }
    std::borrow::Cow::Owned(forced)
}

/// Reads a dollar amount with at most two decimals, such as `2.50`, as cents.
#[must_use]
pub fn parse_cents(raw: &str) -> Option<u64> {
    let (whole, fraction) = raw.split_once('.').unwrap_or((raw, ""));
    let digits = |text: &str| text.bytes().all(|byte| byte.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || fraction.len() > 2 || !digits(fraction) {
        return None;
    }
    if raw.contains('.') && fraction.is_empty() {
        return None;
    }
    let cents = format!("{fraction:0<2}").parse::<u64>().ok()?;
    whole
        .parse::<u64>()
        .ok()?
        .checked_mul(100)?
        .checked_add(cents)
}

/// Hard cap for one JSONL record, including its terminating newline.
pub const MAX_JSONL_EVENT_BYTES: usize = 1024 * 1024;

/// Raw delta chunk cap, allowing even six-byte JSON escapes within the record cap.
const MAX_JSONL_DELTA_BYTES: usize = 8 * 1024;

/// Version of the JSONL record set, in the `start` record.
const JSONL_SCHEMA: u32 = 2;

/// Longest prompt text in the `prompt` record.
const MAX_JSONL_PROMPT_BYTES: usize = 512 * 1024;

/// Longest reasoning text in a `reasoning` record.
const MAX_JSONL_REASONING_BYTES: usize = 256 * 1024;

/// Longest tool output kept in a `tool_result` record's preview.
const MAX_JSONL_PREVIEW_BYTES: usize = 2048;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JsonlEvent<'a> {
    Start {
        schema: u32,
        session_id: String,
        model: &'a str,
        provider: String,
        cwd: String,
        version: &'static str,
        ts: String,
    },
    Prompt {
        text: &'a str,
        ts: String,
    },
    StepStart {
        step: u32,
        ts: String,
    },
    StepRestart {
        step: u32,
    },
    TextDelta {
        step: u32,
        delta: &'a str,
    },
    Reasoning {
        step: u32,
        text: &'a str,
        truncated: bool,
    },
    StepFinish {
        step: u32,
        finish: &'static str,
        usage: StepUsage,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<String>,
        ts: String,
    },
    ToolCall {
        step: u32,
        id: &'a str,
        name: &'a str,
        arguments: serde_json::Value,
        ts: String,
    },
    ToolResult {
        step: u32,
        id: &'a str,
        ok: bool,
        executed: bool,
        elapsed_ms: u64,
        bytes: usize,
        preview: &'a str,
        ts: String,
    },
    Retry {
        step: u32,
        attempt: usize,
        wait_ms: u64,
        retry_at: String,
        kind: &'static str,
        ts: String,
    },
    ContextPruned {
        results: usize,
        tokens_freed: u64,
    },
    Notice {
        text: &'a str,
    },
    Compaction {
        before_tokens: u64,
        after_tokens: u64,
        ts: String,
    },
    PlanUpdated {
        update: &'a rune_tools::PlanUpdate,
    },
    Final {
        result: &'a JsonResult,
    },
}

/// What a run's helper requests used, kept apart from its own steps.
#[derive(Debug, Default)]
struct HelperSpend {
    /// Usage of each helper, by its name in the JSON result.
    by_helper: std::collections::BTreeMap<String, Usage>,
    /// Helper usage served by the run's own model, priced with its steps.
    same_model: Usage,
    /// Priced helper requests served by another model, in millionths of a dollar.
    other_models_cost: u64,
}

impl HelperSpend {
    /// Adds one request.
    fn add(&mut self, label: &str, usage: &Usage, cost: Option<u64>, same_model: bool) {
        let helper = self.by_helper.entry(label.replace(' ', "_")).or_default();
        *helper = helper.merge_sum(*usage);
        if same_model {
            self.same_model = self.same_model.merge_sum(*usage);
        } else if let Some(cost) = cost {
            self.other_models_cost = self.other_models_cost.saturating_add(cost);
        }
    }

    /// Returns what every helper used.
    fn total(&self) -> Usage {
        self.by_helper
            .values()
            .fold(Usage::default(), |sum, usage| sum.merge_sum(*usage))
    }

    /// Returns the per-helper object of the JSON result.
    fn report(&self) -> std::collections::BTreeMap<String, UsageReport> {
        self.by_helper
            .iter()
            .map(|(name, usage)| (name.clone(), UsageReport::from(usage)))
            .collect()
    }
}

/// One step's usage as the provider reported it; an unreported count is null.
///
/// The field names are the record's wire names, shared with the `usage` of the
/// final result, so the repeated suffix stays.
#[allow(clippy::struct_field_names)]
#[derive(Serialize)]
struct StepUsage {
    input_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
}

impl From<&Usage> for StepUsage {
    fn from(usage: &Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            cached_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }
}

/// The current UTC time with millisecond precision, such as
/// `2026-11-10T09:12:03.123Z`.
fn jsonl_timestamp() -> String {
    rfc3339(std::time::SystemTime::now())
}

/// Formats a time as RFC 3339 UTC with milliseconds.
fn rfc3339(time: std::time::SystemTime) -> String {
    let millis = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        });
    jiff::Timestamp::from_millisecond(millis)
        .map(|timestamp| format!("{timestamp:.3}"))
        .unwrap_or_default()
}

/// The longest prefix of `text` within `limit` bytes that ends on a character.
fn cut_at_boundary(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &text[..end]
}

/// Stages one complete record without ever growing beyond the wire cap.
#[derive(Default)]
struct JsonlBuffer {
    bytes: Vec<u8>,
    overflow: bool,
}

impl Write for JsonlBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > (MAX_JSONL_EVENT_BYTES - 1).saturating_sub(self.bytes.len()) {
            self.overflow = true;
            return Err(std::io::Error::other("JSONL event exceeds its byte cap"));
        }
        let required = self.bytes.len().saturating_add(bytes.len());
        if self.bytes.capacity() < required {
            let capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(required)
                .min(MAX_JSONL_EVENT_BYTES);
            self.bytes
                .reserve_exact(capacity.saturating_sub(self.bytes.len()));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_jsonl(writer: &mut impl Write, event: &JsonlEvent<'_>) -> Result<()> {
    let mut buffer = JsonlBuffer::default();
    if let Err(error) = serde_json::to_writer(&mut buffer, event) {
        return Err(if buffer.overflow {
            RuneError::too_large(
                "jsonl_event",
                MAX_JSONL_EVENT_BYTES + 1,
                MAX_JSONL_EVENT_BYTES,
            )
        } else {
            RuneError::new(ErrorCode::Internal, error.to_string())
        });
    }
    buffer.bytes.reserve_exact(1);
    buffer.bytes.push(b'\n');
    writer
        .write_all(&buffer.bytes)
        .and_then(|()| writer.flush())
        .map_err(|error| RuneError::new(ErrorCode::IoFailure, error.to_string()))?;
    Ok(())
}

fn write_text_deltas(writer: &mut impl Write, step: u32, mut delta: &str) -> Result<()> {
    while !delta.is_empty() {
        let mut end = delta.len().min(MAX_JSONL_DELTA_BYTES);
        while !delta.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        write_jsonl(
            writer,
            &JsonlEvent::TextDelta {
                step,
                delta: &delta[..end],
            },
        )?;
        delta = &delta[end..];
    }
    Ok(())
}

/// Starts the JSONL stream before prompt validation or provider setup.
///
/// `session_id` is the id the run will be saved under, or `None` with
/// `--no-save`.
pub fn start_jsonl(settings: &Settings, session_id: Option<&SessionId>) -> Result<()> {
    write_jsonl(
        &mut std::io::stdout().lock(),
        &JsonlEvent::Start {
            schema: JSONL_SCHEMA,
            session_id: session_id.map(ToString::to_string).unwrap_or_default(),
            model: &settings.model,
            provider: settings.provider.to_string(),
            cwd: crate::current_workspace()
                .map(|workspace| workspace.to_string())
                .unwrap_or_default(),
            version: crate::version::VERSION,
            ts: jsonl_timestamp(),
        },
    )
}

/// Emits a final record, replacing an oversized result with a bounded failure.
/// Returns the effective exit code and prints any size diagnostic on stderr.
pub fn final_jsonl(result: &JsonResult) -> Result<u8> {
    match write_jsonl(&mut std::io::stdout().lock(), &JsonlEvent::Final { result }) {
        Ok(()) => Ok(u8::try_from(result.exit_code).unwrap_or(EXIT_FAILURE)),
        Err(error) if error.code() == ErrorCode::TooLarge => {
            let failure = JsonResult::failure("", result.effort, &error, i32::from(EXIT_FAILURE));
            write_jsonl(
                &mut std::io::stdout().lock(),
                &JsonlEvent::Final { result: &failure },
            )?;
            let _ = writeln!(std::io::stderr().lock(), "rune: {error}");
            Ok(EXIT_FAILURE)
        }
        Err(error) => Err(error),
    }
}

/// One tool call as reported in the JSON result.
#[derive(Clone, Debug, Serialize)]
pub struct ToolCallReport {
    /// Tool name.
    pub name: String,
    /// `success` or `error`.
    pub status: &'static str,
}

/// One entry of the run's narration: `{"text": ...}` or `{"tool": name, "status": ...}`.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum NarrationReport {
    /// Text the model wrote.
    Text {
        /// The text.
        text: String,
    },
    /// A tool call.
    Tool {
        /// Tool name.
        tool: String,
        /// `success`, `error`, or `not_run`.
        status: &'static str,
    },
}

/// Serde skip test for a count that is left out when zero.
fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// The JSON result object.
///
/// Field order here is the field order on the wire, because the struct is
/// serialized in declaration order.
#[derive(Clone, Debug, Serialize)]
pub struct JsonResult {
    /// Assistant text produced during the request.
    pub output: String,
    /// The completed final response, or an empty string.
    pub final_output: String,
    /// Process exit code this run will use.
    pub exit_code: i32,
    /// Why the run stopped, by its wire name; null when no model step started.
    pub stop_reason: Option<&'static str>,
    /// Model identifier that served the request.
    pub model: String,
    /// Reasoning effort sent with the requests, after the model's default and clamping.
    pub effort: &'static str,
    /// Upstream provider that served it, when the endpoint reported one.
    pub resolved_provider: Option<String>,
    /// Session identifier, empty when the run was not saved.
    pub session_id: String,
    /// Model steps taken.
    pub steps: u32,
    /// Token counts. A count the provider did not report is absent.
    pub usage: UsageReport,
    /// The part of `usage` each helper accounts for, by name. Absent when none ran.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub helper_usage: std::collections::BTreeMap<String, UsageReport>,
    /// Tool calls requested by the provider, in order.
    pub tool_calls: Vec<ToolCallReport>,
    /// Tool calls that ran after their name or arguments were repaired.
    pub repaired_calls: u32,
    /// What the run said and which tools it called, in the order they
    /// happened, so text written between calls keeps its place.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub narration: Vec<NarrationReport>,
    /// Estimated cost in US dollars as a decimal string, from the published
    /// prices of a cached model catalog. Absent when no price is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<String>,
    /// Experiment keys that differ from the default, as `key=value` pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experiment: Option<String>,
    /// Whether a check ran after the last change to a file and how it ended:
    /// `verified`, `checked`, `failed`, `unverified`, or `not_applicable` when
    /// nothing changed. Absent when the run failed before finishing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<&'static str>,
    /// Whether any changed path looks like a test file. Absent when `verification` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests_modified: Option<bool>,
    /// Files changed during the run, by the edit tools or by commands. Absent when `verification` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<u32>,
    /// Notes the harness added because a check kept failing. Absent when zero.
    #[serde(skip_serializing_if = "is_zero")]
    pub stall_notes: u32,
    /// The goal judge's rounds, last verdict and token use, when a judge was on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeJson>,
    /// Failure detail, present only when the run failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Stable failure code, present only when the run failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ErrorCode>,
    /// Structured failure context, present only when nonempty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<ErrorDetail>,
}

/// What the goal judge did during a run.
#[derive(Clone, Debug, Serialize)]
pub struct JudgeJson {
    /// Judge requests made.
    pub rounds: u32,
    /// The last verdict: `done`, `continue` or `blocked`.
    pub status: String,
    /// The reason given with the last verdict.
    pub reason: String,
    /// Input tokens of the judge requests.
    pub input_tokens: u64,
    /// Output tokens of the judge requests.
    pub output_tokens: u64,
}

/// Token counts as reported.
// The field names are the keys of the JSON contract, so they keep the suffix.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageReport {
    /// Input tokens, or absent when unreported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens, or absent when unreported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Input tokens read from a prompt cache, part of the input count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Input tokens written to a prompt cache, part of the input count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

impl From<&Usage> for UsageReport {
    fn from(usage: &Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        }
    }
}

impl JsonResult {
    /// Builds a failing result, used so `--json` always emits one object.
    #[must_use]
    pub fn failure(model: &str, effort: &'static str, error: &RuneError, exit_code: i32) -> Self {
        Self {
            output: String::new(),
            final_output: String::new(),
            exit_code,
            stop_reason: None,
            model: model.to_owned(),
            effort,
            resolved_provider: None,
            session_id: String::new(),
            steps: 0,
            usage: UsageReport::default(),
            helper_usage: std::collections::BTreeMap::new(),
            tool_calls: Vec::new(),
            repaired_calls: 0,
            narration: Vec::new(),
            cost_usd: None,
            experiment: None,
            verification: None,
            tests_modified: None,
            changed_files: None,
            stall_notes: 0,
            judge: None,
            error: Some(error.message().to_owned()),
            error_code: Some(error.code()),
            error_detail: (!error.detail().is_empty()).then(|| error.detail().clone()),
        }
    }

    /// Renders the object as one line of JSON.
    pub fn render(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|err| RuneError::new(ErrorCode::Internal, err.to_string()))
    }
}

/// Reads the prompt from standard input.
///
/// Bounded, because a prompt is attacker-controlled in a pipeline and an
/// unbounded read would let it exhaust memory.
pub fn read_stdin_prompt() -> Result<String> {
    use std::io::Read;

    let mut buffer = Vec::new();
    let stdin = std::io::stdin();
    let mut handle = stdin.lock().take((MAX_STDIN_PROMPT_BYTES + 1) as u64);
    handle.read_to_end(&mut buffer)?;

    if buffer.len() > MAX_STDIN_PROMPT_BYTES {
        return Err(RuneError::too_large(
            "prompt",
            buffer.len(),
            MAX_STDIN_PROMPT_BYTES,
        ));
    }

    Ok(String::from_utf8_lossy(&buffer).trim_end().to_owned())
}

/// Noninteractive composition of the shared agent loop and built-in tools.
struct AskHost<'a> {
    /// The reasoning effort sent, after the model's default and clamping.
    effort: rune_core::config::Effort,
    /// The cache routing key of every request of this run, taken from the
    /// first request's prefix.
    cache_key: std::sync::OnceLock<String>,
    settings: &'a Settings,
    /// State paths, where an overflow's bound is saved for later runs.
    paths: &'a Paths,
    instructions: String,
    /// Leading bytes of the instructions that are the same in every workspace.
    stable_instructions_len: usize,
    /// The tools offered, fixed for the run so the prefix never moves.
    tools: Vec<ToolSpec>,
    /// The instructions with the repository map, fixed for the run.
    mapped: rune_context::prompt::MappedInstructions,
    dialect: Box<dyn Provider>,
    endpoint: Endpoint,
    /// How requests to the endpoint are shaped, per model.
    compat: crate::provider_setup::CompatSource,
    registry: Registry,
    /// The connected MCP servers whose tools are advertised one by one.
    mcp: Option<rune_context::mcp::Client>,
    /// Runs `subagent` calls; none for a review or a run without tools.
    subagent: Option<std::sync::Arc<crate::subagent::Bridge>>,
    read_only: bool,
    rules: RuleSet,
    context: ExecutionContext,
    limits: BudgetSet,
    steering: SteeringQueue,
    cancellation: Cancellation,
    incomplete_tool_call: std::sync::atomic::AtomicBool,
    jsonl: bool,
    step: std::sync::atomic::AtomicU32,
    emit_error: std::sync::Mutex<Option<RuneError>>,
    /// The model's context window, which decides when the run compacts; zero when unknown.
    window: rune_agent::compaction::Window,
    /// Largest answer the model produces, when known.
    max_output: Option<u64>,
    /// Requests made outside the main loop, waiting to be counted.
    helper_usage: crate::helper_usage::HelperUsage,
    /// What helper requests used, which the turn's own usage does not count.
    helper_spend: std::sync::Mutex<HelperSpend>,
    /// Tokens the run's children used, reported with the run's own.
    child_usage: std::sync::Mutex<Usage>,
    /// The run's step limit, when it is bounded.
    step_cap: Option<u64>,
    /// The request `prepare_request` serialized, for the loop to send.
    encoded_request: std::sync::Mutex<Option<String>>,
    /// Prices of the model, when known, to estimate what the run spends.
    prices: Option<rune_net::models_dev::Prices>,
    /// Whether the cached catalog says the model takes images.
    vision: bool,
    /// Images tools returned, carried into every request.
    tool_images: crate::attachments::ToolImages,
    /// Estimated spend so far in millionths of a dollar; unknown until a
    /// priced response arrives.
    spent: std::sync::Mutex<Option<u64>>,
    /// Spending limit in millionths of a dollar.
    spend_limit: Option<u64>,
    /// Calls of the current step that have a `tool_call` record.
    announced: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// The session, written as the run goes; absent with `--no-save`.
    log: Option<LiveLog>,
    /// The first failure to write the session, which stops the run.
    log_error: std::sync::Mutex<Option<RuneError>>,
    /// When `--timeout` ends the run, so a retry never waits past it.
    deadline: Option<std::time::Instant>,
    /// The objective added to the instructions.
    goal: Option<String>,
    /// Whether the result is checked against the objective.
    judge: bool,
}

impl AskHost<'_> {
    /// Writes to the session log; the first failure stops the run, because a
    /// run whose record is incomplete should not go on spending.
    fn log(&self, write: impl FnOnce(&LiveLog) -> Result<()>) {
        let Some(log) = &self.log else {
            return;
        };
        if lock(&self.log_error).is_some() {
            return;
        }
        if let Err(error) = write(log) {
            *lock(&self.log_error) = Some(error);
            self.cancellation.cancel();
        }
    }

    /// Writes one JSONL record; a failure to write cancels the run once.
    fn record(&self, write: impl FnOnce(&mut std::io::StdoutLock<'_>) -> Result<()>) {
        if !self.jsonl || lock(&self.emit_error).is_some() {
            return;
        }
        if let Err(error) = write(&mut std::io::stdout().lock()) {
            *lock(&self.emit_error) = Some(error);
            self.cancellation.cancel();
        }
    }

    /// Writes the `tool_call` record for a call, once per call.
    fn announce(&self, out: &mut std::io::StdoutLock<'_>, call: &turn::PreparedCall) -> Result<()> {
        if !lock(&self.announced).insert(call.id.clone()) {
            return Ok(());
        }
        let arguments = serde_json::from_str(&call.arguments)
            .unwrap_or_else(|_| serde_json::Value::String(call.arguments.clone()));
        write_jsonl(
            out,
            &JsonlEvent::ToolCall {
                step: self.step.load(std::sync::atomic::Ordering::Relaxed),
                id: &call.id,
                name: &call.name,
                arguments,
                ts: jsonl_timestamp(),
            },
        )
    }

    /// Moves finished helper requests into the run's totals and the ledger.
    ///
    /// A ledger write must not end a run, so a failure is only reported.
    fn absorb_helper_usage(&self) {
        for (kind, model, usage) in self.helper_usage.drain() {
            let Some(label) = crate::helper_usage::label(kind) else {
                continue;
            };
            let cost = crate::helper_usage::cost(self.settings, self.paths, &model, &usage);
            lock(&self.helper_spend).add(label, &usage, cost, model == self.settings.model);
            if let Some(cost) = cost {
                let mut spent = lock(&self.spent);
                *spent = Some(spent.unwrap_or(0).saturating_add(cost));
            }
            self.log(|log| log.usage(&usage));
            if let Err(err) =
                crate::helper_usage::append_to_ledger(self.paths, kind, &model, &usage, cost)
            {
                let _ = writeln!(std::io::stderr(), "usage was not recorded: {err}");
            }
        }
    }

    /// Adds one request's estimated cost to the run's spend.
    fn add_spend(&self, usage: &Usage) {
        if let Some(cost) = self
            .prices
            .and_then(|prices| prices.cost_at(usage, rune_net::transport::now_timestamp()))
        {
            let mut spent = lock(&self.spent);
            *spent = Some(spent.unwrap_or(0).saturating_add(cost));
        }
    }
}

impl Host for AskHost<'_> {
    fn dialect(&self) -> &dyn Provider {
        self.dialect.as_ref()
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn model(&self) -> String {
        self.settings.model.clone()
    }

    fn compat(
        &self,
        model: &str,
    ) -> (
        rune_net::compat::EndpointCompat,
        rune_net::compat::ModelTraits,
    ) {
        self.compat.resolve(model)
    }

    fn instructions(&self) -> String {
        let mut instructions = self.mapped.get(
            &self.instructions,
            self.context.workspace(),
            &map_limits(&self.limits, self.settings.experiments),
        );
        if let Some(goal) = &self.goal {
            instructions.push_str("\n\nActive session objective:\n");
            instructions.push_str(goal);
        }
        instructions
    }

    fn goal_judge(&self) -> Option<String> {
        self.goal.clone().filter(|_| self.judge)
    }

    fn lowest_effort(&self) -> rune_core::config::Effort {
        turn::lowest_accepted_effort(self)
    }

    fn record_judge_usage(&self, usage: &Usage) {
        self.add_spend(usage);
        self.log(|log| log.usage(usage));
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }

    fn stable_instructions_len(&self) -> usize {
        self.stable_instructions_len
    }

    fn tool_activity(&self, name: &str) -> Option<rune_tools::Activity> {
        self.registry.activity(name)
    }

    fn tool_permission_target(&self, name: &str, arguments: &serde_json::Value) -> Option<String> {
        turn::permission_target_for(&self.registry, name, arguments)
    }

    fn effort(&self) -> rune_core::config::Effort {
        self.effort
    }

    fn max_output_tokens(&self) -> Option<u64> {
        self.max_output
    }

    fn context_window(&self) -> Option<u64> {
        Some(self.window.0).filter(|window| *window > 0)
    }

    fn fast_mode(&self) -> bool {
        self.settings.fast_mode
    }

    fn provider_order(&self) -> Vec<String> {
        self.settings.provider_order.clone()
    }

    fn provider_strict(&self) -> bool {
        self.settings.provider_strict
    }

    fn compaction_model(&self) -> Option<String> {
        self.settings.compaction_model.clone()
    }

    fn compaction_window(&self) -> Option<u64> {
        let model = self.compaction_model()?;
        crate::model_limits::resolve(self.settings, self.paths, &model, None).context_window
    }

    fn prepare_request(
        &self,
        history: &mut History,
        plan: &mut rune_net::provider::RequestPlan,
        client: &dyn rune_net::fetch::Fetch,
    ) -> Result<()> {
        self.tool_images.hydrate(plan);
        self.absorb_helper_usage();
        // Checked before compacting, whose summary request costs money too.
        crate::spend::check(*lock(&self.spent), self.spend_limit, "run").map_err(|err| {
            err.with_hint("raise it with --max-cost, or with session_spend_cents")
        })?;
        let (_, encoded) =
            rune_agent::compaction::auto_compact_encoded(history, plan, self, client, self.window)?;
        *lock(&self.encoded_request) = Some(encoded);
        Ok(())
    }

    fn compact_now(
        &self,
        history: &mut History,
        client: &dyn rune_net::fetch::Fetch,
        reason: turn::CompactReason,
    ) -> Result<bool> {
        let turn::CompactReason::Overflow { target_tokens, .. } = reason;
        rune_agent::compaction::compact_now(history, self, client, target_tokens)
    }

    fn observed_overflow(&self, model: &str, tokens: u64) {
        crate::model_limits::record_observed(
            self.paths,
            &rune_core::config::provider_key(&self.settings.provider),
            model,
            tokens,
        );
    }

    #[cfg(not(target_family = "wasm"))]
    fn concurrent_tool_host(&self, name: &str) -> Option<&(dyn Host + Sync)> {
        // Independent reads in one reply run together, as in the session, and
        // so do read-only children.
        ((name == "subagent" && self.subagent.is_some()) || self.registry.get(name)?.is_read_only())
            .then_some(self)
    }

    fn remaining_steps(&self) -> Option<u64> {
        let taken = u64::from(self.step.load(std::sync::atomic::Ordering::Relaxed));
        self.step_cap.map(|cap| cap.saturating_sub(taken))
    }

    fn record_child_usage(&self, _model: &str, usage: &Usage) {
        let mut total = lock(&self.child_usage);
        *total = total.merge_sum(*usage);
        drop(total);
        self.add_spend(usage);
    }

    fn tool_attachments(&self, result: &turn::CallResult) -> Vec<ContentPart> {
        self.tool_images.attach(result)
    }

    fn accepts_images(&self) -> bool {
        self.dialect().capabilities().images && self.vision
    }

    fn provider_head_timeout_ms(&self) -> Option<u64> {
        crate::provider_setup::provider_head_timeout_ms(self.settings)
    }

    fn temperature(&self) -> Option<f64> {
        self.settings.temperature
    }

    fn nudges_questions(&self) -> bool {
        self.settings.experiments.question_nudge
    }

    fn cache_key(&self) -> Option<String> {
        Some(
            self.cache_key
                .get_or_init(|| {
                    rune_net::provider::prefix_cache_key(&self.instructions(), &self.tools())
                })
                .clone(),
        )
    }

    fn take_encoded_request(&self) -> Option<String> {
        lock(&self.encoded_request).take()
    }

    fn context_replaced(&self, report: &rune_agent::compaction::CompactionReport) {
        self.mapped.refresh();
        self.helper_usage.record(
            rune_session::usage::HelperKind::Compaction,
            &self.settings.model,
            report.usage,
        );
        self.absorb_helper_usage();
    }

    fn compaction_abandoned(&self, usage: &Usage) {
        self.helper_usage.record(
            rune_session::usage::HelperKind::Compaction,
            &self.settings.model,
            *usage,
        );
        self.absorb_helper_usage();
    }

    fn settles_cancellation(&self) -> bool {
        // A run stopped by its time limit reports what it did.
        true
    }

    fn fetch(&self) -> Box<dyn rune_net::fetch::Fetch> {
        Box::new(CancellableFetch {
            inner: std::sync::Arc::new(rune_net::transport::UreqFetch::new()),
            cancellation: self.cancellation.clone(),
        })
    }

    fn observe_response(&self, outcome: &StreamOutcome) {
        self.add_spend(&outcome.usage);
        self.log(|log| log.response(outcome));
        let step = self.step.load(std::sync::atomic::Ordering::Relaxed);
        self.record(|out| {
            let reasoning = outcome.reasoning();
            if !reasoning.trim().is_empty() {
                let text = cut_at_boundary(&reasoning, MAX_JSONL_REASONING_BYTES);
                write_jsonl(
                    out,
                    &JsonlEvent::Reasoning {
                        step,
                        text,
                        truncated: text.len() < reasoning.len(),
                    },
                )?;
            }
            write_jsonl(
                out,
                &JsonlEvent::StepFinish {
                    step,
                    finish: outcome.finish.map_or("stop", FinishReason::as_str),
                    usage: StepUsage::from(&outcome.usage),
                    cost_usd: self
                        .prices
                        .and_then(|prices| {
                            prices.cost_at(&outcome.usage, rune_net::transport::now_timestamp())
                        })
                        .map(rune_net::models_dev::dollars),
                    ts: jsonl_timestamp(),
                },
            )
        });
        self.incomplete_tool_call.store(
            outcome.finish == Some(FinishReason::ToolCalls) && outcome.tool_calls().is_empty(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn emit(&self, event: Event) {
        if let Event::TurnStarted { step } = event {
            self.step.store(step, std::sync::atomic::Ordering::Relaxed);
            lock(&self.announced).clear();
        }
        if let Event::Notice { text } = &event
            && !self.jsonl
        {
            eprintln!("rune: {text}");
        }
        let step = self.step.load(std::sync::atomic::Ordering::Relaxed);
        self.record(|out| match &event {
            Event::TurnStarted { step } => write_jsonl(
                out,
                &JsonlEvent::StepStart {
                    step: *step,
                    ts: jsonl_timestamp(),
                },
            ),
            Event::StepRestarted { step } => {
                write_jsonl(out, &JsonlEvent::StepRestart { step: *step })
            }
            Event::PlanUpdated { update } => write_jsonl(out, &JsonlEvent::PlanUpdated { update }),
            Event::TextDelta { delta } => write_text_deltas(out, step, delta),
            Event::ToolStarted { call, .. } => self.announce(out, call),
            Event::ToolFinished {
                call,
                output,
                elapsed,
            } => {
                self.announce(out, call)?;
                write_jsonl(
                    out,
                    &JsonlEvent::ToolResult {
                        step,
                        id: &call.id,
                        ok: !output.is_error,
                        executed: !output.text.starts_with(turn::REPEAT_MARKER),
                        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                        bytes: output.text.len(),
                        preview: cut_at_boundary(&output.text, MAX_JSONL_PREVIEW_BYTES),
                        ts: jsonl_timestamp(),
                    },
                )
            }
            Event::ToolDenied { call, reason } => {
                self.announce(out, call)?;
                write_jsonl(
                    out,
                    &JsonlEvent::ToolResult {
                        step,
                        id: &call.id,
                        ok: false,
                        executed: false,
                        elapsed_ms: 0,
                        bytes: reason.len(),
                        preview: cut_at_boundary(reason, MAX_JSONL_PREVIEW_BYTES),
                        ts: jsonl_timestamp(),
                    },
                )
            }
            Event::ProviderRetry {
                step,
                next_attempt,
                delay,
                retry_at,
                kind,
                ..
            } => write_jsonl(
                out,
                &JsonlEvent::Retry {
                    step: *step,
                    attempt: *next_attempt,
                    wait_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    retry_at: rfc3339(*retry_at),
                    kind,
                    ts: jsonl_timestamp(),
                },
            ),
            Event::Notice { text } => write_jsonl(out, &JsonlEvent::Notice { text }),
            Event::ContextPruned {
                results,
                tokens_freed,
            } => write_jsonl(
                out,
                &JsonlEvent::ContextPruned {
                    results: *results,
                    tokens_freed: *tokens_freed,
                },
            ),
            Event::ContextCompacted {
                before_tokens,
                after_tokens,
                ..
            } => write_jsonl(
                out,
                &JsonlEvent::Compaction {
                    before_tokens: *before_tokens,
                    after_tokens: *after_tokens,
                    ts: jsonl_timestamp(),
                },
            ),
            _ => Ok(()),
        });
    }

    fn pre_tool(&self, call: &turn::PreparedCall) -> Result<Option<turn::HookVeto>> {
        crate::pre_tool_hook::run(
            self.settings.pre_tool_hook.as_ref(),
            call,
            &self.context,
            &self.cancellation,
        )
    }

    fn post_tool(&self, result: &turn::CallResult) {
        self.log(|log| log.result(result));
        if let Some(message) = crate::post_tool_hook::notify(
            self.settings.post_tool_hook.as_ref(),
            result,
            &self.context,
            &self.cancellation,
            &self.endpoint.credential,
        ) {
            let _ = writeln!(std::io::stderr(), "warning: {message}");
        }
    }

    fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        self.execute_with_context(name, arguments, &self.context)
    }

    fn execute_with_context(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        if let Some(client) = &self.mcp
            && client.tools().iter().any(|tool| tool.name == name)
        {
            return crate::mcp::call_tool(client, name, arguments, &self.cancellation, context);
        }
        if name == "subagent"
            && let Some(subagent) = &self.subagent
        {
            return subagent.call(self, &self.registry, arguments, context);
        }
        self.registry.call(name, arguments, context)
    }

    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        if self.read_only
            && !self
                .registry
                .get(name)
                .is_some_and(rune_tools::Tool::is_read_only)
        {
            let decision = rune_policy::decision::Decision::matched(
                Outcome::Deny,
                rune_policy::decision::Layer::Session,
                "read-only review",
            );
            return (decision.outcome, decision.explain());
        }
        crate::mcp::decide_call(
            &self.rules,
            self.settings.permission_mode,
            name,
            target,
            self.context.workspace(),
        )
    }

    fn decide_quietly(&self, name: &str, target: Option<&str>) -> Outcome {
        // `decide` here evaluates rules only; it never asks anyone.
        self.decide(name, target).0
    }

    fn context(&self) -> ExecutionContext {
        self.context.fork()
    }

    fn limits(&self) -> BudgetSet {
        self.limits.clone()
    }

    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    fn steering(&self) -> &SteeringQueue {
        &self.steering
    }

    fn retry_policy(&self, max_attempts: usize) -> RetryPolicy {
        let remaining = self
            .deadline
            .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()));
        RetryPolicy::headless(max_attempts, remaining)
    }
}

/// Runs one bounded turn and returns the result to report.
///
/// With JSONL enabled, emits deltas synchronously with bounded staging and
/// stdout backpressure. The caller emits the start and final records.
pub fn run(
    settings: &Settings,
    paths: &Paths,
    options: &Options,
) -> std::result::Result<JsonResult, RuneError> {
    // Validate before touching the network so a missing credential or an
    // unselected model is reported immediately and names the remedy.
    crate::provider_setup::validate_selection(settings)?;

    // Configuration is validated before a credential is looked up, so a user
    // who has not set an endpoint is told that rather than being told about a
    // credential for an endpoint that does not exist.
    let endpoint = crate::provider_setup::configured_endpoint(settings, paths)?;
    let compat = crate::provider_setup::CompatSource::from_settings(settings);
    let effort = rune_net::compat::effective_effort(
        settings.effort,
        settings.source_of("effort") != Layer::Default,
        &compat.resolve(&settings.model).1,
    );

    crate::provider_setup::refresh_catalog_in_background(settings, paths);
    // Read from configuration and the cache only, so a run never waits on the
    // network for its limits.
    let model_limits = crate::model_limits::resolve(settings, paths, &settings.model, None);

    let attachments = crate::attachments::Attachments::read(&options.images)?;
    let dialect = attachments.provider(crate::provider_setup::dialect_for(&settings.provider));

    let mut limits = settings.limits.clone();
    if let Some(cents) = options.max_cost_cents {
        limits.set(
            LimitName::SessionSpendCents,
            Budget::Bounded(cents),
            Layer::CommandLine,
        )?;
    }
    let prices = crate::provider_setup::cached_prices(settings, paths);
    if options.max_cost_cents.is_some() && prices.is_none() {
        return Err(RuneError::invalid_field(
            "--max-cost",
            format!(
                "`--max-cost` needs prices for `{}`, and none are known",
                settings.model
            ),
        )
        .with_hint("declare prices for the model under [models.<provider>] in config.toml, or run rune models once while online to cache the catalog"));
    }
    let spend_limit = limits
        .get(LimitName::SessionSpendCents)
        .value()
        .map(|cents| cents.saturating_mul(10_000));
    if !options.tool_loop {
        limits.set(
            LimitName::MaxAgentSteps,
            Budget::Bounded(1),
            Layer::CommandLine,
        )?;
    } else if let Some(requested) = options.max_steps {
        let budget = if requested > 0 {
            Budget::Bounded(requested)
        } else {
            Budget::Unbounded
        };
        limits.set(LimitName::MaxAgentSteps, budget, Layer::CommandLine)?;
    }
    let step_cap: Option<u64> = limits
        .get(LimitName::MaxAgentSteps)
        .value()
        .filter(|cap| *cap > 0);
    let rules = crate::permissions::validated(settings)?;
    let helper_usage = crate::helper_usage::HelperUsage::default();
    let cancellation = Cancellation::new();
    let (registry, subagent, mcp) = if options.tool_loop {
        let catalog: std::sync::Arc<dyn rune_tools::skill::Catalog> = std::sync::Arc::new(
            rune_tools::skill::Fixed::new(rune_context::skills::discover(
                &crate::current_workspace()?,
                rune_core::paths::home_directory().as_deref(),
                &paths.config_root,
            )?),
        );
        let mut registry = inventory::builtin_with_vision(
            &rune_tools::FileLimits::from_budget(&limits),
            &limits,
            &paths.managed_skills_dir(),
            crate::web_client::backends(settings),
            std::sync::Arc::new(rune_tools::ask_user::Unavailable),
            std::sync::Arc::clone(&catalog),
            crate::vision_client::backend(settings, paths, Some(helper_usage.clone()))?,
        )?;
        let workspace = crate::current_workspace()?;
        inventory::tailor(
            &mut registry,
            &inventory::Tailoring {
                workspace: &workspace,
                interactive: false,
                edit_tool: rune_net::compat::edit_tool(settings.edit_tool, &settings.model),
            },
        );
        // A review stays read-only and asks no children.
        let subagent = if options.read_only {
            None
        } else {
            Some(crate::subagent::register(
                &mut registry,
                settings,
                paths,
                &workspace,
                &rules,
            )?)
        };
        if let Some(text) = rune_tools::tool_text::from_env()? {
            registry.set_tool_text(text)?;
        }
        // A review stays read-only, so it starts no server whose every call
        // would be refused.
        let connection = if options.read_only {
            None
        } else {
            crate::mcp::connect_interactive(settings, paths, &workspace)?
        };
        let mcp = match connection {
            Some(connection) => {
                for warning in &connection.warnings {
                    let _ = writeln!(std::io::stderr(), "warning: {warning}");
                }
                crate::mcp::attach(
                    connection.client,
                    settings,
                    &mut registry,
                    catalog,
                    cancellation.clone(),
                )?
            }
            None => None,
        };
        (registry, subagent, mcp)
    } else {
        (Registry::new(), None, None)
    };
    let mut history = History::new();
    let mut parts = vec![ContentPart::Text {
        text: first_message(
            &options.prompt,
            options.tool_loop && settings.experiments.run_note,
            options.read_only,
        ),
    }];
    parts.extend(attachments.parts());
    if options.tool_loop
        && settings.experiments.snapshot
        && let Some(block) = crate::workspace_snapshot::render(&crate::current_workspace()?)
    {
        parts.push(ContentPart::Text {
            text: format!("\n\n{block}"),
        });
    }
    history.prepend(rune_net::message::Role::User, parts);
    let mut context = ExecutionContext::new(crate::current_workspace()?)
        .with_offline(settings.offline)
        .with_allow_unsandboxed(settings.allow_unsandboxed)
        .with_environment(crate::provider_setup::command_environment(settings))
        .with_network(settings.shell_network)
        .with_cache_roots(crate::sandbox::cache_paths(settings));
    context
        .additional_roots
        .clone_from(&settings.additional_directories);
    let (instructions, stable_instructions_len) = rune_context::prompt::base_instructions_split(
        context.workspace(),
        &paths.config_root,
        &settings.limits,
    );
    let mut tools = inventory::advertisement(&registry);
    if let Some(client) = &mcp {
        crate::mcp::append_tools(&mut tools, client)?;
    }
    let (goal, judge) = match &options.goal {
        Some(goal) => (Some(goal.clone()), options.judge),
        None if settings.experiments.goal_judge && options.tool_loop && !options.read_only => {
            (Some(options.prompt.clone()), true)
        }
        None => (None, false),
    };
    let host = AskHost {
        goal,
        judge,
        effort,
        cache_key: std::sync::OnceLock::new(),
        settings,
        paths,
        instructions,
        stable_instructions_len,
        tools,
        mapped: rune_context::prompt::MappedInstructions::default(),
        dialect,
        endpoint,
        compat,
        registry,
        mcp,
        subagent,
        read_only: options.read_only,
        rules,
        context,
        steering: SteeringQueue::from_limits(&limits),
        cancellation,
        incomplete_tool_call: std::sync::atomic::AtomicBool::new(false),
        jsonl: options.jsonl,
        step: std::sync::atomic::AtomicU32::new(0),
        emit_error: std::sync::Mutex::new(None),
        window: rune_agent::compaction::Window(model_limits.context_window.unwrap_or_default()),
        max_output: model_limits.max_output,
        helper_usage,
        helper_spend: std::sync::Mutex::new(HelperSpend::default()),
        child_usage: std::sync::Mutex::new(Usage::default()),
        step_cap,
        encoded_request: std::sync::Mutex::new(None),
        prices,
        vision: crate::provider_setup::cached_vision(settings, paths).unwrap_or(false),
        tool_images: crate::attachments::ToolImages::default(),
        spent: std::sync::Mutex::new(None),
        spend_limit,
        announced: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        log: if options.no_save {
            None
        } else {
            Some(LiveLog::open(
                paths,
                options.session_id.unwrap_or_else(SessionId::generate),
                &options.prompt,
            )?)
        },
        log_error: std::sync::Mutex::new(None),
        deadline: options
            .timeout
            .and_then(|limit| std::time::Instant::now().checked_add(limit)),
        limits,
    };
    if let Some(log) = &host.log
        && let Some(notice) = crate::session::attach_results(&mut history, paths, &log.id)
    {
        let _ = writeln!(std::io::stderr(), "warning: {notice}");
    }
    let watchdog = options.timeout.map(|limit| {
        Watchdog::start(
            limit,
            host.cancellation.clone(),
            host.context.cancellation(),
        )
    });
    host.record(|out| {
        write_jsonl(
            out,
            &JsonlEvent::Prompt {
                text: cut_at_boundary(&options.prompt, MAX_JSONL_PROMPT_BYTES),
                ts: jsonl_timestamp(),
            },
        )
    });
    // A termination signal stops the run the way its limits do, so a runner
    // that ends it on a timeout still gets its record and no stray commands.
    let shutdown = {
        let run = host.cancellation.clone();
        let tools = host.context.cancellation();
        crate::shutdown::Shutdown::install(move || {
            run.cancel();
            tools.cancel();
        })?
    };
    // Files a command changes are seen by comparing the repository status
    // before and after the run; outside a repository only tool changes count.
    let workspace = host.context.workspace().to_owned();
    let status_before = options
        .tool_loop
        .then(|| rune_tools::git_diff::status_entries(&workspace, STATUS_LIMIT))
        .flatten();
    let outcome = turn::run_turn(&mut history, &host);
    // Whatever ended the run, nothing it started keeps running.
    host.registry.shutdown();
    let command_changes = status_before
        .map(|before| changed_by_commands(&workspace, &before))
        .unwrap_or_default();
    let timed_out = watchdog.is_some_and(Watchdog::finish);
    let signal = shutdown.received();
    let outcome = match (signal, outcome) {
        // The cancellation was the signal's; a run that finished as it
        // arrived keeps its own ending.
        (Some(signal), Ok(mut outcome)) if outcome.stop_reason == StopReason::Cancelled => {
            outcome.error = Some(crate::shutdown::stopped_by(signal));
            Ok(outcome)
        }
        (Some(signal), Err(err)) if err.code() == ErrorCode::Cancelled => {
            Err(crate::shutdown::stopped_by(signal))
        }
        (_, outcome) => outcome,
    };
    // The signal that stopped the run, which decides its exit code.
    let signalled = signal.filter(|_| {
        matches!(&outcome, Ok(outcome) if outcome
            .error
            .as_ref()
            .is_some_and(|error| error.code() == ErrorCode::Cancelled))
    });
    let outcome = match (timed_out && signalled.is_none(), outcome) {
        // The run settled when it was cancelled; the cancellation was the
        // time limit, so that is the reason it stopped.
        // A run that finished as the deadline passed keeps its own ending.
        (true, Ok(mut outcome)) if outcome.stop_reason == StopReason::Cancelled => {
            let secs = options.timeout.map_or(0, |limit| limit.as_secs());
            outcome.stop_reason = StopReason::TimeLimit;
            outcome.error = Some(
                RuneError::new(
                    ErrorCode::LimitExceeded,
                    format!("the run reached its time limit of {secs} seconds"),
                )
                .with_hint("raise it with --timeout")
                .with_invariant("run_timeout"),
            );
            Ok(outcome)
        }
        (_, outcome) => outcome,
    };
    host.absorb_helper_usage();
    if let Some(error) = lock(&host.emit_error).take() {
        return Err(error);
    }
    if let Some(error) = lock(&host.log_error).take() {
        return Err(error);
    }
    let outcome = outcome?;
    // Helper requests are billed too, so they are part of what the run used.
    let helpers = lock(&host.helper_spend);
    let child_usage = *lock(&host.child_usage);
    let usage = outcome
        .usage
        .merge_sum(helpers.total())
        .merge_sum(child_usage);
    let priced_at_run_rates = outcome
        .usage
        .merge_sum(helpers.same_model)
        .merge_sum(child_usage);
    // A turn that failed after a completed request still reports its usage,
    // steps and calls, with the failure as the error.
    let error = if let Some(error) = outcome.error.clone() {
        Some(error)
    } else if host
        .incomplete_tool_call
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        Some(RuneError::new(
            ErrorCode::UnsupportedToolCall,
            "the provider requested tools without a completed tool call",
        ))
    } else if !options.tool_loop && !outcome.calls.is_empty() {
        let names = outcome
            .calls
            .iter()
            .map(|call| call.call.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Some(RuneError::new(
            ErrorCode::UnsupportedToolCall,
            format!("tool calls are unsupported when the tool loop is disabled: {names}"),
        ))
    } else if outcome.stop_reason == StopReason::StepLimit {
        Some(RuneError::new(
            ErrorCode::LimitExceeded,
            turn::step_limit_notice(step_cap.unwrap_or_default()),
        ))
    } else {
        None
    };
    let exit_code = exit_code(outcome.stop_reason, error.as_ref(), signalled);
    let tool_calls = outcome
        .calls
        .iter()
        .map(|call| ToolCallReport {
            name: call.call.name.clone(),
            status: if call.output.is_error {
                "error"
            } else {
                "success"
            },
        })
        .collect();
    let narration = outcome
        .narration
        .iter()
        .filter_map(|entry| match entry {
            turn::Narration::Text(text) if text.trim().is_empty() => None,
            turn::Narration::Text(text) => Some(NarrationReport::Text { text: text.clone() }),
            turn::Narration::Call(id) => {
                let call = outcome.calls.iter().find(|call| &call.call.id == id)?;
                Some(NarrationReport::Tool {
                    tool: call.call.name.clone(),
                    status: if !call.executed {
                        "not_run"
                    } else if call.output.is_error {
                        "error"
                    } else {
                        "success"
                    },
                })
            }
        })
        .collect();
    let session_id = match &host.log {
        Some(log) => {
            log.finish(error.as_ref())?;
            log.id.to_string()
        }
        None => String::new(),
    };

    Ok(JsonResult {
        // A run stopped by one of its bounds has a usable partial answer.
        // The answer is usable when the run finished, a limit ended it, or a
        // signal stopped it; after a failure it is only the raw `output`.
        final_output: if matches!(exit_code, EXIT_OK | EXIT_LIMIT | EXIT_SIGINT | EXIT_SIGTERM) {
            outcome.text.clone()
        } else {
            String::new()
        },
        output: outcome.text,
        exit_code: i32::from(exit_code),
        stop_reason: Some(outcome.stop_reason.as_str()),
        model: settings.model.clone(),
        effort: host.effort.as_str(),
        resolved_provider: None,
        session_id,
        steps: outcome.steps,
        usage: UsageReport::from(&usage),
        helper_usage: helpers.report(),
        tool_calls,
        repaired_calls: outcome.repaired_calls,
        narration,
        cost_usd: crate::provider_setup::cached_prices(settings, paths)
            .and_then(|prices| {
                prices.cost_at(&priced_at_run_rates, rune_net::transport::now_timestamp())
            })
            .map(|cost| cost.saturating_add(helpers.other_models_cost))
            .map(rune_net::models_dev::dollars),
        experiment: Some(settings.experiments.render()).filter(|keys| !keys.is_empty()),
        verification: error.is_none().then(|| outcome.verification.as_str()),
        tests_modified: error.is_none().then(|| {
            outcome.checks.tests_modified
                || command_changes
                    .iter()
                    .any(|path| rune_agent::verify::is_test_path(path))
        }),
        changed_files: error.is_none().then(|| {
            let mut changed: std::collections::BTreeSet<String> = outcome
                .checks
                .changed_files
                .iter()
                .map(|path| workspace_relative(&workspace, path))
                .collect();
            changed.extend(command_changes.iter().cloned());
            u32::try_from(changed.len()).unwrap_or(u32::MAX)
        }),
        stall_notes: outcome.checks.stall_notes,
        judge: outcome.judge.as_ref().map(|report| JudgeJson {
            rounds: report.rounds,
            status: report.last_status.clone(),
            reason: report.last_reason.clone(),
            input_tokens: report.usage.input_tokens.unwrap_or(0),
            output_tokens: report.usage.output_tokens.unwrap_or(0),
        }),
        error: error.as_ref().map(|error| error.message().to_owned()),
        error_code: error.as_ref().map(RuneError::code),
        error_detail: error
            .as_ref()
            .filter(|error| !error.detail().is_empty())
            .map(|error| error.detail().clone()),
    })
}

/// How long one repository status snapshot may take before it is given up.
const STATUS_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Returns the workspace-relative paths whose repository status entry is new
/// or different since `before`, or nothing when a snapshot cannot be taken.
fn changed_by_commands(
    workspace: &camino::Utf8Path,
    before: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let Some(after) = rune_tools::git_diff::status_entries(workspace, STATUS_LIMIT) else {
        return Vec::new();
    };
    // Status paths are relative to the repository root.
    let Some(prefix) =
        rune_tools::git_diff::git_output(workspace, &["rev-parse", "--show-prefix"], STATUS_LIMIT)
    else {
        return Vec::new();
    };
    let prefix = prefix.trim_end_matches('\n');
    after
        .into_iter()
        .filter(|(path, status)| before.get(path) != Some(status))
        .filter_map(|(path, _)| path.strip_prefix(prefix).map(str::to_owned))
        .collect()
}

/// Returns a path an edit tool named, relative to the workspace.
fn workspace_relative(workspace: &camino::Utf8Path, path: &str) -> String {
    let path = camino::Utf8Path::new(path);
    let relative = path.strip_prefix(workspace).unwrap_or(path).as_str();
    relative.trim_start_matches("./").to_owned()
}

/// Sends each request on a helper thread, so a cancellation is noticed while
/// the endpoint has not answered yet.
///
/// Reading a response stream is already interruptible; waiting for its first
/// bytes is not, and a time limit must stop a run whose provider is silent.
/// A request abandoned this way finishes or times out on its own thread.
struct CancellableFetch {
    inner: std::sync::Arc<rune_net::transport::UreqFetch>,
    cancellation: Cancellation,
}

impl rune_net::fetch::Fetch for CancellableFetch {
    fn send(
        &self,
        request: rune_net::fetch::FetchRequest,
    ) -> rune_net::error::NetResult<rune_net::fetch::FetchResponse> {
        use rune_net::error::{FailureKind, NetError};
        let cancelled = || NetError::new(FailureKind::Cancelled, "the request was cancelled");
        if self.cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let inner = std::sync::Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("rune-ask-request".to_owned())
            .spawn(move || {
                let _ = sender.send(rune_net::fetch::Fetch::send(inner.as_ref(), request));
            })
            .map_err(|err| NetError::new(FailureKind::Network, err.to_string()))?;
        loop {
            match receiver.recv_timeout(std::time::Duration::from_millis(25)) {
                Ok(response) => return response,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if self.cancellation.is_cancelled() {
                        return Err(cancelled());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(NetError::new(
                        FailureKind::Network,
                        "the request thread ended without a response",
                    ));
                }
            }
        }
    }
}

/// Locks a host field, recovering it if a panicking holder poisoned it.
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Cancels a run at its deadline, from a thread of its own.
struct Watchdog {
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    fired: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Watchdog {
    /// Starts counting down `limit`; at the deadline both cancellations are
    /// set, which stops the provider stream and any running tool.
    fn start(
        limit: std::time::Duration,
        run: Cancellation,
        tools: rune_tools::contract::Cancellation,
    ) -> Self {
        use std::sync::atomic::Ordering;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started = std::time::Instant::now();
        let thread = {
            let done = std::sync::Arc::clone(&done);
            let fired = std::sync::Arc::clone(&fired);
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    let elapsed = started.elapsed();
                    if elapsed >= limit {
                        fired.store(true, Ordering::Relaxed);
                        run.cancel();
                        tools.cancel();
                        return;
                    }
                    std::thread::sleep(
                        limit
                            .saturating_sub(elapsed)
                            .min(std::time::Duration::from_millis(25)),
                    );
                }
            })
        };
        Self {
            done,
            fired,
            thread,
        }
    }

    /// Stops the countdown and returns whether the deadline was reached.
    fn finish(self) -> bool {
        use std::sync::atomic::Ordering;
        self.done.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
        self.fired.load(Ordering::Relaxed)
    }
}

/// The session of a run, written while it happens.
///
/// Each response and each tool result is appended as it arrives, so a run
/// that is killed or times out keeps every step it completed. Appends are not
/// synced one by one, which would cost more than the steps; `finish` makes the
/// whole log durable once.
struct LiveLog {
    store: std::sync::Mutex<SessionStore>,
    id: SessionId,
    warning: std::sync::Mutex<Option<RuneError>>,
}

impl LiveLog {
    /// Creates the session and records what the run starts from.
    fn open(paths: &Paths, id: SessionId, prompt: &str) -> Result<Self> {
        let workspace = crate::current_workspace()?;
        let store = SessionStore::create(paths, &id)?;
        store.append_unsynced([
            SessionEvent::WorkspaceSet {
                workspace: rune_policy::trust::canonical_workspace(&workspace).to_string(),
            },
            SessionEvent::TitleSet {
                title: crate::session_log::derive_title(prompt),
            },
            SessionEvent::UserMessage {
                text: prompt.to_owned(),
            },
            SessionEvent::TurnStarted { turn: 1 },
        ])?;
        Ok(Self {
            store: std::sync::Mutex::new(store),
            id,
            warning: std::sync::Mutex::new(None),
        })
    }

    fn append(&self, events: Vec<SessionEvent>) -> Result<()> {
        lock(&self.store).append_unsynced(events).map(|_| ())
    }

    /// Records one turn of the conversation: an assistant message with its
    /// calls, or the results of those calls.
    fn message(&self, turn: &rune_agent::history::Turn) -> Result<()> {
        self.append(crate::session_log::message_events(turn, 1)?)
    }

    /// Records a completed response and its usage.
    fn response(&self, outcome: &StreamOutcome) -> Result<()> {
        let mut parts = Vec::new();
        let text = outcome.text();
        if !text.is_empty() {
            parts.push(ContentPart::Text { text });
        }
        let reasoning = outcome.reasoning();
        if !reasoning.is_empty() {
            parts.push(ContentPart::Reasoning { text: reasoning });
        }
        for (id, name, arguments) in outcome.tool_calls() {
            parts.push(ContentPart::ToolCall {
                id: rune_core::id::ToolCallId::new(id)?,
                name,
                arguments,
            });
        }
        let mut message = rune_agent::history::Turn::assistant(0, parts);
        message.replay.clone_from(&outcome.replay);
        if !message.parts.is_empty() {
            self.message(&message)?;
        }
        self.usage(&outcome.usage)
    }

    /// Records the usage of one request; a request with no counts adds none.
    fn usage(&self, usage: &Usage) -> Result<()> {
        if let (Some(input_tokens), Some(output_tokens)) = (usage.input_tokens, usage.output_tokens)
        {
            self.append(vec![SessionEvent::UsageRecorded {
                input_tokens,
                output_tokens,
            }])?;
        }
        Ok(())
    }

    /// Records a call's result.
    fn result(&self, result: &turn::CallResult) -> Result<()> {
        self.append(vec![SessionEvent::ToolResult {
            call_id: result.call.id.clone(),
            ok: !result.output.is_error,
            output: result.output.text.clone(),
        }])
    }

    /// Records how the run failed, when it did, and makes the log durable.
    fn finish(&self, error: Option<&RuneError>) -> Result<()> {
        let store = lock(&self.store);
        if let Some(error) = error {
            store.append_unsynced([SessionEvent::TurnFailed {
                turn: 1,
                code: error.code(),
                message: error.message().to_owned(),
            }])?;
        }
        let report = store.sync()?;
        if let Some(warning) = report
            .metadata_warning
            .or_else(|| lock(&self.warning).take())
        {
            let _ = writeln!(std::io::stderr(), "warning: {warning}");
        }
        Ok(())
    }
}

/// Writes a result to the two output streams.
///
/// Markdown goes to standard output and diagnostics to standard error, so a
/// pipeline can consume the answer while a human still sees progress.
pub fn report(result: &JsonResult, options: &Options) -> Result<u8> {
    let code = if options.jsonl {
        final_jsonl(result)?
    } else {
        u8::try_from(result.exit_code).unwrap_or(EXIT_FAILURE)
    };
    if !options.jsonl && options.json {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{}", result.render()?)
            .map_err(|err| RuneError::new(ErrorCode::Internal, err.to_string()))?;
    } else if !options.jsonl && !result.final_output.is_empty() {
        let mut stdout = std::io::stdout().lock();
        write!(stdout, "{}", result.final_output)
            .map_err(|err| RuneError::new(ErrorCode::Internal, err.to_string()))?;
        if !result.final_output.ends_with('\n') {
            let _ = writeln!(stdout);
        }
    }

    if let Some(error) = &result.error {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "rune: {error}");
    }

    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::config::{EnvironmentOverrides, Provider};

    #[test]
    fn exit_codes_follow_the_stop_reason() {
        let error = RuneError::new(ErrorCode::UnsupportedToolCall, "incomplete call");
        let table = [
            (StopReason::Completed, 0, 1),
            (StopReason::StepLimit, 3, 3),
            (StopReason::OutputLimit, 3, 3),
            (StopReason::TimeLimit, 3, 3),
            (StopReason::SpendLimit, 3, 3),
            (StopReason::ObjectiveBudget, 3, 3),
            (StopReason::ProviderFailure, 1, 1),
            (StopReason::Refused, 1, 1),
            (StopReason::ContentFilter, 1, 1),
            (StopReason::Stalled, 1, 1),
            (StopReason::Cancelled, 1, 1),
        ];
        for (stop, clean, with_error) in table {
            assert_eq!(exit_code(stop, None, None), clean, "{stop:?}");
            assert_eq!(exit_code(stop, Some(&error), None), with_error, "{stop:?}");
            assert_eq!(
                exit_code(stop, None, Some(crate::shutdown::SIGINT)),
                EXIT_SIGINT
            );
            assert_eq!(
                exit_code(stop, Some(&error), Some(crate::shutdown::SIGTERM)),
                EXIT_SIGTERM
            );
            assert_eq!(
                exit_code(stop, None, Some(crate::shutdown::SIGHUP)),
                EXIT_SIGTERM
            );
        }
    }

    #[test]
    fn review_uses_the_read_only_note() {
        assert_eq!(
            first_message("check it", true, true),
            format!("check it\n\n{REVIEW_RUN_CONTEXT}")
        );
        assert_eq!(
            first_message("fix it", true, false),
            format!("fix it\n\n{RUN_CONTEXT}")
        );
        assert_eq!(first_message("plain", false, false), "plain");
    }

    #[test]
    fn ask_host_is_sync() {
        // Parallel reads hand the host to worker threads.
        fn assert_sync<T: Sync>() {}
        assert_sync::<AskHost<'static>>();
    }

    #[test]
    fn tool_result_preview_cuts_at_a_character_boundary() {
        // "é" is two bytes; one starting at byte 2,047 does not fit in 2,048.
        let text = format!("{}é tail", "a".repeat(MAX_JSONL_PREVIEW_BYTES - 1));
        let preview = cut_at_boundary(&text, MAX_JSONL_PREVIEW_BYTES);
        assert_eq!(preview.len(), MAX_JSONL_PREVIEW_BYTES - 1);
        assert!(preview.chars().all(|c| c == 'a'));
        assert_eq!(cut_at_boundary("short", MAX_JSONL_PREVIEW_BYTES), "short");
        let big = "x".repeat(5 * 1024 * 1024);
        assert_eq!(cut_at_boundary(&big, MAX_JSONL_PREVIEW_BYTES).len(), 2048);
    }

    #[test]
    fn jsonl_timestamps_are_utc_with_milliseconds() {
        let ts = jsonl_timestamp();
        assert_eq!(ts.len(), "2026-11-10T09:12:03.123Z".len(), "{ts}");
        assert!(ts.ends_with('Z') && ts.as_bytes()[19] == b'.', "{ts}");
    }

    #[test]
    fn dollar_amounts_are_read_as_cents() {
        assert_eq!(parse_cents("0.10"), Some(10));
        assert_eq!(parse_cents("2.5"), Some(250));
        assert_eq!(parse_cents("3"), Some(300));
        assert_eq!(parse_cents("0"), Some(0));
        for bad in ["", "1.234", "-1", "1.", ".5", "abc", "1,50", "1.5x", " 1"] {
            assert_eq!(parse_cents(bad), None, "{bad:?}");
        }
    }

    fn settings_with(provider: Provider, model: &str) -> Settings {
        Settings {
            provider,
            model: model.to_owned(),
            base_url: Some("https://api.example.com".to_owned()),
            ..Settings::default()
        }
    }

    #[test]
    fn saved_ask_preserves_native_assistant_replay() {
        use rune_net::provider::{Provider as _, RequestPlan};
        let temp = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(temp.path()).expect("utf8");
        let paths = Paths {
            config_root: root.join("config"),
            state_root: root.join("state"),
            data_root: root.join("data"),
        };
        let mut history = History::new();
        history.push_user("lookup");
        let id = rune_core::id::ToolCallId::new("local").expect("id");
        let parts = serde_json::json!([{ "functionCall": { "id": "native", "name": "lookup", "args": {} }, "thoughtSignature": "signature" }]);
        history.push_assistant_with_replay(
            vec![ContentPart::ToolCall {
                id: id.clone(),
                name: "lookup".into(),
                arguments: "{}".into(),
            }],
            Some(serde_json::json!({"gemini_parts": parts}).to_string()),
            None,
        );
        history.push_tool_results(vec![ContentPart::ToolResult {
            id,
            name: "lookup".into(),
            content: "found".into(),
            is_error: false,
        }]);
        let saved = SessionId::generate();
        let log = LiveLog::open(&paths, saved, "lookup").expect("open");
        for turn in history.turns().iter().skip(1) {
            log.message(turn).expect("message");
        }
        log.finish(None).expect("finish");
        drop(log);
        let (_, restored) = crate::session_log::load(&paths, &saved).expect("resume");
        restored.validate().expect("history");
        let mut plan = RequestPlan::new("gemini-fixture");
        plan.messages = restored.to_messages();
        let body = rune_net::gemini::Gemini
            .build_request(&plan)
            .expect("request");
        assert_eq!(body["contents"][1]["parts"], parts);
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["name"],
            "lookup"
        );
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["id"],
            "native"
        );
    }

    #[test]
    fn jsonl_splits_escaped_unicode_deltas_and_flushes_each_record() {
        #[derive(Default)]
        struct Output {
            bytes: Vec<u8>,
            flushes: usize,
        }
        impl Write for Output {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }
        let delta = format!(
            "{}{}",
            "a".repeat(MAX_JSONL_DELTA_BYTES - 1),
            "🌱\n\0\"".repeat(5000)
        );
        let mut output = Output::default();
        write_text_deltas(&mut output, 2, &delta).expect("write deltas");
        let lines = std::str::from_utf8(&output.bytes).expect("UTF-8");
        let mut joined = String::new();
        for line in lines.lines() {
            assert!(line.len() < MAX_JSONL_EVENT_BYTES);
            let event: serde_json::Value = serde_json::from_str(line).expect("JSON event");
            assert_eq!(event["type"], "text_delta");
            assert_eq!(event["step"], 2);
            let chunk = event["delta"].as_str().expect("delta");
            assert!(chunk.len() <= MAX_JSONL_DELTA_BYTES);
            joined.push_str(chunk);
        }
        assert_eq!(joined, delta);
        assert_eq!(output.flushes, lines.lines().count());
        assert!(output.flushes > 1);
    }

    #[test]
    fn jsonl_rejects_oversized_serialized_records_before_writing_stdout() {
        let mut result =
            JsonResult::failure("m", "auto", &RuneError::new(ErrorCode::Internal, "x"), 1);
        result.output = "\0".repeat(MAX_JSONL_EVENT_BYTES / 6);
        let mut stdout = Vec::new();
        let error = write_jsonl(&mut stdout, &JsonlEvent::Final { result: &result })
            .expect_err("escaped record exceeds cap");
        assert_eq!(error.code(), ErrorCode::TooLarge);
        assert!(stdout.is_empty());
        let mut buffer = JsonlBuffer::default();
        buffer
            .write_all(&vec![b'a'; MAX_JSONL_EVENT_BYTES - 1])
            .expect("at cap");
        assert!(buffer.write_all(b"b").is_err());
        assert_eq!(buffer.bytes.len(), MAX_JSONL_EVENT_BYTES - 1);
        buffer.bytes.reserve_exact(1);
        buffer.bytes.push(b'\n');
        assert!(buffer.bytes.capacity() <= MAX_JSONL_EVENT_BYTES);
    }

    #[test]
    fn jsonl_propagates_output_failure() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error = write_text_deltas(&mut Closed, 1, "hello").expect_err("closed pipe");
        assert_eq!(error.code(), ErrorCode::IoFailure);
    }

    #[test]
    fn a_missing_provider_is_reported_before_any_request() {
        let settings = Settings::default();
        let paths = Paths::resolve(Some("/tmp"), None, None, None, Some("/tmp/rune-state"));
        let options = Options {
            prompt: "hi".to_owned(),
            ..Options::default()
        };
        let err = run(&settings, &paths, &options).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::AuthenticationRequired);
    }

    #[test]
    fn a_missing_model_is_reported_before_any_request() {
        let settings = settings_with(Provider::Anthropic, "");
        let paths = Paths::resolve(Some("/tmp"), None, None, None, Some("/tmp/rune-state"));
        let options = Options {
            prompt: "hi".to_owned(),
            ..Options::default()
        };
        let err = run(&settings, &paths, &options).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidConfiguration);
    }

    #[test]
    fn a_missing_credential_names_every_source_tried() {
        let settings = settings_with(Provider::Anthropic, "claude-test");
        let paths = Paths::resolve(Some("/tmp"), None, None, None, Some("/tmp/rune-state"));
        let options = Options {
            prompt: "hi".to_owned(),
            ..Options::default()
        };
        let err = run(&settings, &paths, &options).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::AuthenticationRequired);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn a_missing_endpoint_is_reported_with_a_remedy() {
        // A compatible endpoint has no published address, so it is the one
        // provider that still has to be told where to send the request.
        let mut settings = settings_with(Provider::ChatCompletions, "claude-test");
        settings.base_url = None;
        let dir = tempfile::TempDir::new().expect("tempdir");
        let paths = Paths::resolve(
            Some(dir.path().to_str().unwrap_or("/tmp")),
            None,
            None,
            None,
            Some(dir.path().join("state").to_str().unwrap_or("/tmp/s")),
        );
        let options = Options {
            prompt: "hi".to_owned(),
            ..Options::default()
        };
        let err = run(&settings, &paths, &options).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidConfiguration);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn a_failure_result_serializes_with_the_stable_code() {
        let err = RuneError::new(ErrorCode::RateLimited, "slow down");
        let result = JsonResult::failure("m", "auto", &err, 1);
        let json = result.render().expect("render");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(value["error_code"], "rate_limited");
        assert_eq!(value["exit_code"], 1);
        assert_eq!(value["output"], "");
        assert_eq!(value["final_output"], "");
        assert!(value.get("error_detail").is_none());
    }

    #[test]
    fn the_json_object_has_the_documented_key_set() {
        let result = JsonResult::failure("m", "auto", &RuneError::new(ErrorCode::Internal, "x"), 1);
        let json = result.render().expect("render");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let object = value.as_object().expect("object");
        for key in [
            "output",
            "final_output",
            "exit_code",
            "model",
            "resolved_provider",
            "session_id",
            "steps",
            "usage",
            "tool_calls",
            "repaired_calls",
        ] {
            assert!(object.contains_key(key), "missing key `{key}`");
        }
    }

    #[test]
    fn the_json_key_order_is_stable() {
        let result = JsonResult::failure("m", "auto", &RuneError::new(ErrorCode::Internal, "x"), 1);
        let json = result.render().expect("render");
        let output = json.find("\"output\"").expect("output");
        let final_output = json.find("\"final_output\"").expect("final_output");
        let exit_code = json.find("\"exit_code\"").expect("exit_code");
        assert!(output < final_output);
        assert!(final_output < exit_code);
    }

    #[test]
    fn an_unreported_usage_count_is_absent_rather_than_zero() {
        let usage = UsageReport::default();
        let json = serde_json::to_value(&usage).expect("serialize");
        assert_eq!(json, serde_json::json!({}));
    }

    #[test]
    fn a_reported_zero_usage_count_is_present() {
        let usage = UsageReport {
            input_tokens: Some(0),
            ..UsageReport::default()
        };
        let json = serde_json::to_value(&usage).expect("serialize");
        assert_eq!(json["input_tokens"], 0);
        assert!(json.get("output_tokens").is_none());
    }

    #[test]
    fn a_success_result_has_no_error_fields() {
        let result = JsonResult {
            output: "hi".to_owned(),
            final_output: "hi".to_owned(),
            exit_code: 0,
            stop_reason: Some("end_turn"),
            model: "m".to_owned(),
            effort: "auto",
            resolved_provider: None,
            session_id: String::new(),
            steps: 1,
            usage: UsageReport::default(),
            helper_usage: std::collections::BTreeMap::new(),
            tool_calls: Vec::new(),
            repaired_calls: 0,
            narration: Vec::new(),
            cost_usd: None,
            experiment: None,
            verification: Some("not_applicable"),
            tests_modified: Some(false),
            changed_files: Some(0),
            stall_notes: 0,
            judge: None,
            error: None,
            error_code: None,
            error_detail: None,
        };
        let json = result.render().expect("render");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert!(value.get("error").is_none());
        assert!(value.get("error_code").is_none());
        assert!(value.get("error_detail").is_none());
    }

    #[test]
    fn helper_usage_is_reported_in_json() {
        let usage = |input| Usage {
            input_tokens: Some(input),
            output_tokens: Some(1),
            ..Usage::default()
        };
        let mut spend = HelperSpend::default();
        spend.add("compaction", &usage(100), None, true);
        spend.add("image analysis", &usage(20), Some(7), false);
        spend.add("image analysis", &usage(30), Some(5), false);
        assert_eq!(spend.total().input_tokens, Some(150));
        assert_eq!(spend.same_model.input_tokens, Some(100));
        assert_eq!(spend.other_models_cost, 12);

        let mut result =
            JsonResult::failure("m", "auto", &RuneError::new(ErrorCode::Internal, "x"), 1);
        let empty = serde_json::to_value(&result).expect("serialize");
        assert!(empty.get("helper_usage").is_none(), "{empty}");

        result.helper_usage = spend.report();
        let json = serde_json::to_value(&result).expect("serialize");
        assert_eq!(
            json["helper_usage"],
            serde_json::json!({
                "compaction": {"input_tokens": 100, "output_tokens": 1},
                "image_analysis": {"input_tokens": 50, "output_tokens": 2},
            })
        );
    }

    #[test]
    fn a_tool_call_report_carries_a_name_and_status() {
        let report = ToolCallReport {
            name: "read_file".to_owned(),
            status: "success",
        };
        let json = serde_json::to_value(&report).expect("serialize");
        assert_eq!(json["name"], "read_file");
        assert_eq!(json["status"], "success");
    }

    #[test]
    fn the_environment_override_reaches_the_settings() {
        let vars = std::collections::HashMap::from([
            ("RUNE_PROVIDER", "anthropic"),
            ("RUNE_MODEL", "claude-test"),
        ]);
        let env = EnvironmentOverrides::from_lookup(|key| vars.get(key).map(|v| (*v).to_owned()));
        let settings = rune_core::config::load(None, None, &env);
        assert_eq!(settings.provider, Provider::Anthropic);
        assert_eq!(settings.model, "claude-test");
    }
}
