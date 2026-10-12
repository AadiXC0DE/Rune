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

use std::cell::{Cell, RefCell};
use std::io::Write;

use rune_agent::history::History;
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

/// Largest prompt accepted from standard input.
pub const MAX_STDIN_PROMPT_BYTES: usize = 8 * 1024 * 1024;

/// Default finite bound when the configured step limit is zero or unbounded.
pub const DEFAULT_MAX_STEPS: u64 = 16;

/// Options for one run.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The prompt text.
    pub prompt: String,
    /// Emit one JSON object instead of Markdown.
    pub json: bool,
    /// Stream bounded JSONL records instead of Markdown or one JSON result.
    pub jsonl: bool,
    /// Do not create or update a session.
    pub no_save: bool,
    /// Execute host tools in a bounded loop. The review command stays text-only.
    pub tool_loop: bool,
}

/// Hard cap for one JSONL record, including its terminating newline.
pub const MAX_JSONL_EVENT_BYTES: usize = 1024 * 1024;

/// Raw delta chunk cap, allowing even six-byte JSON escapes within the record cap.
const MAX_JSONL_DELTA_BYTES: usize = 8 * 1024;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JsonlEvent<'a> {
    Start {},
    StepStart { step: u32 },
    StepRestart { step: u32 },
    TextDelta { step: u32, delta: &'a str },
    Final { result: &'a JsonResult },
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
pub fn start_jsonl() -> Result<()> {
    write_jsonl(&mut std::io::stdout().lock(), &JsonlEvent::Start {})
}

/// Emits a final record, replacing an oversized result with a bounded failure.
/// Returns the effective exit code and prints any size diagnostic on stderr.
pub fn final_jsonl(result: &JsonResult) -> Result<u8> {
    match write_jsonl(&mut std::io::stdout().lock(), &JsonlEvent::Final { result }) {
        Ok(()) => Ok(u8::try_from(result.exit_code).unwrap_or(EXIT_FAILURE)),
        Err(error) if error.code() == ErrorCode::TooLarge => {
            let failure = JsonResult::failure("", &error, i32::from(EXIT_FAILURE));
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
    /// Model identifier that served the request.
    pub model: String,
    /// Upstream provider that served it, when the endpoint reported one.
    pub resolved_provider: Option<String>,
    /// Session identifier, empty when the run was not saved.
    pub session_id: String,
    /// Model steps taken.
    pub steps: u32,
    /// Token counts. A count the provider did not report is absent.
    pub usage: UsageReport,
    /// Tool calls requested by the provider, in order.
    pub tool_calls: Vec<ToolCallReport>,
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

/// Token counts as reported.
#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageReport {
    /// Input tokens, or absent when unreported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens, or absent when unreported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

impl JsonResult {
    /// Builds a failing result, used so `--json` always emits one object.
    #[must_use]
    pub fn failure(model: &str, error: &RuneError, exit_code: i32) -> Self {
        Self {
            output: String::new(),
            final_output: String::new(),
            exit_code,
            model: model.to_owned(),
            resolved_provider: None,
            session_id: String::new(),
            steps: 0,
            usage: UsageReport::default(),
            tool_calls: Vec::new(),
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
    settings: &'a Settings,
    instructions: String,
    dialect: Box<dyn Provider>,
    endpoint: Endpoint,
    registry: Registry,
    rules: RuleSet,
    context: ExecutionContext,
    limits: BudgetSet,
    steering: SteeringQueue,
    cancellation: Cancellation,
    incomplete_tool_call: Cell<bool>,
    jsonl: bool,
    step: Cell<u32>,
    emit_error: RefCell<Option<RuneError>>,
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

    fn instructions(&self) -> String {
        rune_context::prompt::with_repository_map(
            &self.instructions,
            self.context.workspace(),
            &self.limits,
        )
    }

    fn tools(&self) -> Vec<ToolSpec> {
        inventory::advertisement(&self.registry)
    }

    fn tool_activity(&self, name: &str) -> Option<rune_tools::Activity> {
        self.registry.activity(name)
    }

    fn tool_permission_target(&self, name: &str, arguments: &serde_json::Value) -> Option<String> {
        turn::permission_target_for(&self.registry, name, arguments)
    }

    fn effort(&self) -> rune_core::config::Effort {
        self.settings.effort
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

    fn observe_response(&self, outcome: &StreamOutcome) {
        self.incomplete_tool_call.set(
            outcome.finish == Some(FinishReason::ToolCalls) && outcome.tool_calls().is_empty(),
        );
    }

    fn emit(&self, event: Event) {
        if !self.jsonl || self.emit_error.borrow().is_some() {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        let result = match event {
            Event::TurnStarted { step } => {
                self.step.set(step);
                write_jsonl(&mut stdout, &JsonlEvent::StepStart { step })
            }
            Event::StepRestarted { step } => {
                write_jsonl(&mut stdout, &JsonlEvent::StepRestart { step })
            }
            Event::TextDelta { delta } => write_text_deltas(&mut stdout, self.step.get(), &delta),
            _ => Ok(()),
        };
        if let Err(error) = result {
            *self.emit_error.borrow_mut() = Some(error);
            self.cancellation.cancel();
        }
    }

    fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        self.registry.call(name, arguments, &self.context)
    }

    fn execute_with_context(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        self.registry.call(name, arguments, context)
    }

    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        turn::decide_call_in_workspace(
            &self.rules,
            self.settings.permission_mode,
            name,
            target,
            self.context.workspace(),
        )
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

    let dialect = crate::provider_setup::dialect_for(&settings.provider);

    let mut limits = settings.limits.clone();
    let step_cap = limits
        .get(LimitName::MaxAgentSteps)
        .value()
        .filter(|cap| *cap > 0)
        .unwrap_or(DEFAULT_MAX_STEPS);
    let step_cap = if options.tool_loop { step_cap } else { 1 };
    limits.set(
        LimitName::MaxAgentSteps,
        Budget::Bounded(step_cap),
        Layer::CommandLine,
    )?;
    let registry = if options.tool_loop {
        let catalog = rune_context::skills::discover(
            &crate::current_workspace()?,
            rune_core::paths::home_directory().as_deref(),
            &paths.config_root,
        )?;
        inventory::builtin_with_vision(
            &rune_tools::FileLimits::from_budget(&limits),
            &limits,
            &paths.managed_skills_dir(),
            crate::web_client::backends(settings),
            std::sync::Arc::new(rune_tools::ask_user::Unavailable),
            std::sync::Arc::new(rune_tools::skill::Fixed::new(catalog)),
            crate::vision_client::backend(settings, paths)?,
        )?
    } else {
        Registry::new()
    };
    let mut history = History::new();
    history.push_user(options.prompt.clone());
    let mut context = ExecutionContext::new(crate::current_workspace()?)
        .with_offline(settings.offline)
        .with_allow_unsandboxed(settings.allow_unsandboxed);
    context
        .additional_roots
        .clone_from(&settings.additional_directories);
    let host = AskHost {
        settings,
        instructions: rune_context::prompt::base_instructions_for(
            context.workspace(),
            &paths.config_root,
            &settings.limits,
        ),
        dialect,
        endpoint,
        registry,
        rules: crate::permissions::validated(settings)?,
        context,
        steering: SteeringQueue::from_limits(&limits),
        cancellation: Cancellation::new(),
        incomplete_tool_call: Cell::new(false),
        jsonl: options.jsonl,
        step: Cell::new(0),
        emit_error: RefCell::new(None),
        limits,
    };
    let outcome = turn::run_turn(&mut history, &host);
    if let Some(error) = host.emit_error.borrow_mut().take() {
        return Err(error);
    }
    let outcome = outcome?;
    let error = if host.incomplete_tool_call.get() {
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
            format!("tool calls are unsupported in the text-only review path: {names}"),
        ))
    } else if outcome.stop_reason == StopReason::StepLimit {
        Some(RuneError::new(
            ErrorCode::LimitExceeded,
            turn::step_limit_notice(step_cap),
        ))
    } else {
        None
    };
    let exit_code = if error.is_none() && outcome.stop_reason == StopReason::Completed {
        i32::from(EXIT_OK)
    } else {
        i32::from(EXIT_FAILURE)
    };
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
    let session_id = if options.no_save {
        String::new()
    } else {
        save_exchange(
            paths,
            &options.prompt,
            &history,
            &outcome.usage,
            error.as_ref(),
        )?
        .to_string()
    };

    Ok(JsonResult {
        final_output: if error.is_none() {
            outcome.text.clone()
        } else {
            String::new()
        },
        output: outcome.text,
        exit_code,
        model: settings.model.clone(),
        resolved_provider: None,
        session_id,
        steps: outcome.steps,
        usage: UsageReport {
            input_tokens: outcome.usage.input_tokens,
            output_tokens: outcome.usage.output_tokens,
        },
        tool_calls,
        error: error.as_ref().map(|error| error.message().to_owned()),
        error_code: error.as_ref().map(RuneError::code),
        error_detail: error
            .as_ref()
            .filter(|error| !error.detail().is_empty())
            .map(|error| error.detail().clone()),
    })
}

/// Saves a completed request before its identifier is reported to the caller.
fn save_exchange(
    paths: &Paths,
    prompt: &str,
    history: &History,
    usage: &Usage,
    error: Option<&RuneError>,
) -> Result<SessionId> {
    let workspace = crate::current_workspace()?;
    let id = SessionId::generate();
    let store = SessionStore::create(paths, &id)?;
    let mut metadata_warning = None;
    let mut append = |event| -> Result<()> {
        let report = store.append(event)?;
        metadata_warning = report.metadata_warning;
        Ok(())
    };
    append(SessionEvent::WorkspaceSet {
        workspace: rune_policy::trust::canonical_workspace(&workspace).to_string(),
    })?;
    append(SessionEvent::TitleSet {
        title: crate::session_log::derive_title(prompt),
    })?;
    append(SessionEvent::UserMessage {
        text: prompt.to_owned(),
    })?;
    append(SessionEvent::TurnStarted { turn: 1 })?;
    for turn in history.turns().iter().skip(1) {
        let text = turn.text();
        if !text.is_empty() {
            append(SessionEvent::AssistantMessage { turn: 1, text })?;
        }
        for part in &turn.parts {
            match part {
                ContentPart::ToolCall {
                    id,
                    name,
                    arguments,
                } => append(SessionEvent::ToolCall {
                    call_id: id.to_string(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                })?,
                ContentPart::ToolResult {
                    id,
                    content,
                    is_error,
                    ..
                } => append(SessionEvent::ToolResult {
                    call_id: id.to_string(),
                    ok: !is_error,
                    output: content.clone(),
                })?,
                _ => {}
            }
        }
    }
    if let (Some(input_tokens), Some(output_tokens)) = (usage.input_tokens, usage.output_tokens) {
        append(SessionEvent::UsageRecorded {
            input_tokens,
            output_tokens,
        })?;
    }
    if let Some(error) = error {
        append(SessionEvent::TurnFailed {
            turn: 1,
            code: error.code(),
            message: error.message().to_owned(),
        })?;
    }
    if let Some(warning) = metadata_warning {
        let _ = writeln!(std::io::stderr(), "warning: {warning}");
    }
    Ok(id)
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
    } else if !options.jsonl && result.error.is_none() {
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

    fn settings_with(provider: Provider, model: &str) -> Settings {
        Settings {
            provider,
            model: model.to_owned(),
            base_url: Some("https://api.example.com".to_owned()),
            ..Settings::default()
        }
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
        let mut result = JsonResult::failure("m", &RuneError::new(ErrorCode::Internal, "x"), 1);
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
        let result = JsonResult::failure("m", &err, 1);
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
        let result = JsonResult::failure("m", &RuneError::new(ErrorCode::Internal, "x"), 1);
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
        ] {
            assert!(object.contains_key(key), "missing key `{key}`");
        }
    }

    #[test]
    fn the_json_key_order_is_stable() {
        let result = JsonResult::failure("m", &RuneError::new(ErrorCode::Internal, "x"), 1);
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
            output_tokens: None,
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
            model: "m".to_owned(),
            resolved_provider: None,
            session_id: String::new(),
            steps: 1,
            usage: UsageReport::default(),
            tool_calls: Vec::new(),
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
