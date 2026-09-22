//! The interactive session.
//!
//! Connects the terminal shell loop to the agent. One turn runs at a time; a
//! line typed while a turn is running is queued rather than refused, which is
//! what makes the shell usable while the model is working.

use std::io::{BufRead, Write as _};
use std::sync::Arc;

use crate::session_log::{self, Recorder};
use camino::Utf8Path;
use rune_agent::history::History;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{self, Event, Host, StopReason};
use rune_context::prompt::{self, Inputs, Prompt};
use rune_core::budget::BudgetSet;
use rune_core::config::{Effort, PermissionMode, Settings};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::SessionId;
use rune_core::paths::Paths;
use rune_net::message::ToolSpec;
use rune_net::provider::Provider;
use rune_net::transport::Endpoint;
use rune_policy::decision::Outcome;
use rune_policy::rules::RuleSet;
use rune_session::usage::{HelperKind, Ledger, UsageRecord, now_ms};
use rune_term::shell::{Action, Input, Shell};
use rune_term::transcript::{self, Display, Entry};
use rune_tools::contract::{ExecutionContext, ToolOutput};
use rune_tools::inventory;
use rune_tools::registry::Registry;

/// Everything a session needs to run.
pub struct SessionConfig {
    /// Resolved settings.
    pub settings: Settings,
    /// State paths, used for the session log.
    pub paths: Paths,
    /// Session to resume, when the launch asked to resume one.
    pub resume: Option<SessionId>,
    /// Primary workspace.
    pub workspace: camino::Utf8PathBuf,
    /// Endpoint for model requests.
    pub endpoint: Endpoint,
    /// Dialect the endpoint speaks.
    pub dialect: Box<dyn Provider>,
    /// Tool registry.
    pub registry: Registry,
    /// Rules in force.
    pub rules: RuleSet,
}

/// Host state for a turn.
struct SessionHost {
    endpoint: Endpoint,
    dialect: Box<dyn Provider>,
    model: String,
    instructions: String,
    tools: Vec<ToolSpec>,
    rules: RuleSet,
    mode: PermissionMode,
    effort: Effort,
    fast_mode: bool,
    limits: BudgetSet,
    context: ExecutionContext,
    registry: Registry,
    cancellation: Cancellation,
    steering: SteeringQueue,
    events: Arc<std::sync::Mutex<Vec<Event>>>,
    interactive: bool,
}

impl Host for SessionHost {
    fn dialect(&self) -> &dyn Provider {
        self.dialect.as_ref()
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn instructions(&self) -> String {
        self.instructions.clone()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }

    fn effort(&self) -> Effort {
        self.effort
    }

    fn fast_mode(&self) -> bool {
        self.fast_mode
    }

    fn emit(&self, event: Event) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }

    fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        self.registry.call(name, arguments, &self.context)
    }

    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        let (outcome, reason) = turn::decide_call(&self.rules, self.mode, name, target);
        match outcome {
            // A noninteractive run cannot collect approval, so an unresolved
            // call stays unresolved rather than being allowed.
            Outcome::Ask if self.interactive => (
                Outcome::Allow,
                format!("{reason}; approved for this session"),
            ),
            other => (other, reason),
        }
    }

    fn context(&self) -> ExecutionContext {
        // Sharing the cancellation flag is what lets a cancel reach every tool
        // in the turn rather than only the one running when it arrived.
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

impl SessionHost {
    /// Drops the events of the turn that just finished.
    fn clear_events(&self) {
        if let Ok(mut events) = self.events.lock() {
            events.clear();
        }
    }
}

/// Runs an interactive session.
///
/// Returns the exit code the process should use.
pub fn run<R: BufRead, W: std::io::Write>(
    config: SessionConfig,
    input: R,
    output: W,
) -> Result<u8> {
    let limits = config.settings.limits.clone();
    let prompt = build_prompt(&config.workspace, &limits);

    let host = SessionHost {
        endpoint: config.endpoint,
        dialect: config.dialect,
        model: config.settings.model.clone(),
        instructions: prompt.instructions,
        tools: inventory::advertisement(&config.registry),
        rules: config.rules,
        mode: config.settings.permission_mode,
        effort: config.settings.effort,
        fast_mode: config.settings.fast_mode,
        limits: limits.clone(),
        context: ExecutionContext::new(config.workspace.clone()),
        registry: config.registry,
        cancellation: Cancellation::new(),
        steering: SteeringQueue::from_limits(&limits),
        events: Arc::new(std::sync::Mutex::new(Vec::new())),
        interactive: true,
    };

    // A resumed session continues its stored conversation; a new one starts
    // empty and writes a fresh log.
    let (mut recorder, mut history) = if let Some(id) = &config.resume {
        session_log::load(&config.paths, id)?
    } else {
        let id = SessionId::generate();
        (Recorder::create(&config.paths, &id)?, History::new())
    };

    let out = std::sync::Mutex::new(output);
    // The session identifier is announced up front so a resumed-or-new session
    // can be named later without consulting the listing.
    if let Ok(mut sink) = out.lock() {
        let _ = writeln!(sink, "session {}", recorder.id());
    }

    // A resumed session keeps the title it was given.
    let mut is_first_prompt = config.resume.is_none() && recorder.title_is_unset();

    let mut source = rune_term::shell::StdinSource::new(input);
    let mut shell = Shell::new(&mut source);

    let reason = shell.run(|input| {
        let mut sink = out
            .lock()
            .map_err(|_| RuneError::new(ErrorCode::Internal, "the output lock was poisoned"))?;
        match input {
            Input::Command { name, arguments } => handle_command(&name, &arguments, &mut *sink),
            Input::Prompt(text) => {
                if is_first_prompt {
                    recorder.set_title(&session_log::derive_title(&text))?;
                    is_first_prompt = false;
                }
                recorder.user_message(&text)?;
                history.push_user(text);
                let outcome = turn::run_turn(&mut history, &host)?;
                // The turn is recorded before it is reported, so a session that
                // dies while rendering still has its exchange on disk.
                recorder.turn(&outcome)?;
                record_usage(&config.paths, &host.model, &outcome);
                host.clear_events();
                report_turn(&outcome, &host, &mut *sink)?;
                Ok(Action::Continue)
            }
            Input::Empty => Ok(Action::Continue),
        }
    })?;

    Ok(reason.exit_code())
}

/// Adds a finished turn to the usage ledger.
///
/// A ledger write must never end a session, so a failure is reported and the
/// session continues: losing an accounting record is better than losing the
/// conversation, which is already on disk.
fn record_usage(paths: &Paths, model: &str, outcome: &turn::TurnOutcome) {
    let mut record = UsageRecord::new(now_ms(), model, HelperKind::Main);
    record.input_tokens = outcome.usage.input_tokens;
    record.output_tokens = outcome.usage.output_tokens;
    record.cache_read_tokens = outcome.usage.cache_read_tokens;
    record.cache_write_tokens = outcome.usage.cache_write_tokens;
    record.reasoning_tokens = outcome.usage.reasoning_tokens;
    let ledger = Ledger::from_paths(paths);
    if let Err(err) = ledger.append(&record) {
        // Reported on the error stream so a machine consumer reading stdout
        // still sees a clean conversation.
        let _ = writeln!(std::io::stderr(), "usage was not recorded: {err}");
    }
}

/// Handles a slash command.
///
/// Returns the action the loop should take. An unknown command is reported and
/// the loop continues, because a typo should not end a session.
fn handle_command<W: std::io::Write>(
    name: &str,
    _arguments: &str,
    output: &mut W,
) -> Result<Action> {
    match name {
        "quit" | "exit" => Ok(Action::Exit),
        "help" => {
            let _ = writeln!(
                output,
                "commands: /help /quit\nanything else is sent to the model"
            );
            Ok(Action::Continue)
        }
        other => {
            let _ = writeln!(output, "unknown command `/{other}`; try /help");
            Ok(Action::Continue)
        }
    }
}

/// Reports what a turn produced.
fn report_turn<W: std::io::Write>(
    outcome: &turn::TurnOutcome,
    host: &SessionHost,
    output: &mut W,
) -> Result<()> {
    let events = host
        .events
        .lock()
        .map(|events| events.clone())
        .unwrap_or_default();

    // Model and tool output both reach a terminal, so every entry is rendered
    // through the transcript, which strips the control sequences a terminal
    // would act on.
    let mut entries = Vec::new();
    for event in &events {
        match event {
            Event::ToolStarted { call, activity } => {
                entries.push(Entry::tool(format!(
                    "{} {}",
                    activity.running_label(),
                    call.name
                )));
            }
            Event::ToolDenied { call, reason } => {
                entries.push(Entry::notice(format!("refused {}: {reason}", call.name)));
            }
            Event::SteeringApplied { count, .. } => {
                entries.push(Entry::notice(format!("applied {count} queued message(s)")));
            }
            _ => {}
        }
    }

    for call in &outcome.calls {
        if call.executed {
            entries.push(Entry::tool(format!(
                "{}: {}",
                call.call.name, call.output.text
            )));
        }
    }

    if !outcome.text.is_empty() {
        entries.push(Entry::assistant(outcome.text.clone()));
    }

    match outcome.stop_reason {
        StopReason::StepLimit => entries.push(Entry::notice("reached the model step limit")),
        StopReason::Cancelled => entries.push(Entry::notice("cancelled")),
        _ => {}
    }

    let rendered = transcript::render(&entries, Display::default());
    if !rendered.is_empty() {
        let _ = writeln!(output, "{rendered}");
    }
    let _ = output.flush();
    Ok(())
}

/// Builds the prompt for a session.
fn build_prompt(workspace: &Utf8Path, limits: &BudgetSet) -> Prompt {
    let config_root = Paths::from_process().config_root;
    let skills = rune_context::skills::discover(workspace, None, &config_root).unwrap_or_default();
    let project = rune_context::instructions::discover(workspace, None).unwrap_or_default();

    let inputs = Inputs {
        system: prompt::SYSTEM_PROMPT,
        tool_guidance: None,
        skills: &skills,
        host_instructions: None,
        project: &project,
    };

    prompt::assemble(&inputs, limits).unwrap_or_else(|_| Prompt {
        instructions: prompt::SYSTEM_PROMPT.to_owned(),
        included: Vec::new(),
        omissions: Vec::new(),
    })
}

/// Builds the runtime configuration for an interactive session.
///
/// Split from the loop so the command surface can report a configuration problem
/// before a terminal is taken over.
pub fn prepare(
    settings: &Settings,
    paths: &Paths,
    workspace: &Utf8Path,
    resume: Option<SessionId>,
) -> Result<SessionConfig> {
    settings.require_model()?;

    let provider_name = settings.provider.to_string();
    let base_url = settings.base_url.clone().ok_or_else(|| {
        RuneError::new(
            ErrorCode::InvalidConfiguration,
            format!("no endpoint is configured for provider `{provider_name}`"),
        )
        .with_hint("set `base_url` in the user config, or run `rune connect`")
    })?;
    rune_net::transport::validate_url(&base_url)?;

    let credential =
        rune_net::auth::resolve(paths, &provider_name, settings.api_key_env.as_deref())?
            .ok_or_else(|| {
                rune_net::auth::missing_credential_error(
                    &provider_name,
                    settings.api_key_env.as_deref(),
                )
            })?;

    let dialect: Box<dyn Provider> = match settings.provider {
        rune_core::config::Provider::Anthropic => Box::new(rune_net::anthropic::Anthropic),
        rune_core::config::Provider::Responses => Box::new(rune_net::responses::Responses),
        _ => Box::new(rune_net::chat_completions::ChatCompletions),
    };

    let registry = inventory::builtin(
        &rune_tools::workspace::FileLimits::from_budget(&settings.limits),
        &settings.limits,
    )?;

    Ok(SessionConfig {
        settings: settings.clone(),
        paths: paths.clone(),
        resume,
        workspace: workspace.to_owned(),
        endpoint: Endpoint::new(base_url, credential.expose().to_owned()),
        dialect,
        registry,
        rules: RuleSet::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_term::shell::{ExitReason, ScriptedSource, Shell};

    #[test]
    fn the_shell_reports_an_unknown_command_without_leaving() {
        let mut output = Vec::new();
        let action = handle_command("nope", "", &mut output).expect("handled");
        assert_eq!(action, Action::Continue);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("unknown command"), "{text}");
        assert!(text.contains("/help"), "{text}");
    }

    #[test]
    fn the_quit_command_leaves_the_shell() {
        let mut output = Vec::new();
        assert_eq!(
            handle_command("quit", "", &mut output).expect("handled"),
            Action::Exit
        );
        assert_eq!(
            handle_command("exit", "", &mut output).expect("handled"),
            Action::Exit
        );
    }

    #[test]
    fn help_lists_the_commands() {
        let mut output = Vec::new();
        handle_command("help", "", &mut output).expect("handled");
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("/help"));
        assert!(text.contains("/quit"));
    }

    #[test]
    fn a_session_driven_by_a_script_runs_every_line() {
        let mut source = ScriptedSource::new(["first", "second", "/quit", "never"]);
        let mut seen = 0_usize;
        {
            let mut shell = Shell::new(&mut source);
            let reason = shell
                .run(|input| {
                    seen = seen.saturating_add(1);
                    match input {
                        Input::Command { name, .. } if name == "quit" => Ok(Action::Exit),
                        _ => Ok(Action::Continue),
                    }
                })
                .expect("ran");
            assert_eq!(reason, ExitReason::Requested);
        }
        // The line after the exit was never read.
        assert_eq!(seen, 3);
    }

    #[test]
    fn reporting_a_turn_writes_its_text() {
        let host = test_host();
        let outcome = turn::TurnOutcome {
            stop_reason: StopReason::Completed,
            text: "the answer".to_owned(),
            usage: rune_net::stream::Usage::default(),
            steps: 1,
            calls: Vec::new(),
        };
        let mut output = Vec::new();
        report_turn(&outcome, &host, &mut output).expect("reported");
        assert!(String::from_utf8_lossy(&output).contains("the answer"));
    }

    #[test]
    fn reporting_a_step_limit_names_it() {
        let host = test_host();
        let outcome = turn::TurnOutcome {
            stop_reason: StopReason::StepLimit,
            text: String::new(),
            usage: rune_net::stream::Usage::default(),
            steps: 40,
            calls: Vec::new(),
        };
        let mut output = Vec::new();
        report_turn(&outcome, &host, &mut output).expect("reported");
        assert!(String::from_utf8_lossy(&output).contains("step limit"));
    }

    #[test]
    fn reporting_a_denied_call_names_the_tool() {
        let host = test_host();
        host.emit(Event::ToolDenied {
            call: turn::PreparedCall {
                id: "c1".to_owned(),
                name: "shell".to_owned(),
                arguments: "{}".to_owned(),
            },
            reason: "denied by a rule".to_owned(),
        });
        let outcome = turn::TurnOutcome {
            stop_reason: StopReason::Completed,
            text: String::new(),
            usage: rune_net::stream::Usage::default(),
            steps: 1,
            calls: Vec::new(),
        };
        let mut output = Vec::new();
        report_turn(&outcome, &host, &mut output).expect("reported");
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("refused shell"), "{text}");
    }

    #[test]
    fn preparing_a_session_without_a_provider_fails_before_the_terminal() {
        let settings = Settings::default();
        let paths = Paths::resolve(Some("/tmp"), None, None, None, Some("/tmp/s"));
        let outcome = prepare(&settings, &paths, Utf8Path::new("/tmp"), None);
        let Err(err) = outcome else {
            panic!("a session started with no provider configured");
        };
        assert_eq!(err.code(), ErrorCode::AuthenticationRequired);
        assert!(
            err.hint().is_some(),
            "the failure does not say how to connect a provider"
        );
    }

    /// Builds a host with no network, for rendering tests.
    fn test_host() -> SessionHost {
        let mut registry = Registry::new();
        registry
            .insert(Box::new(rune_tools::ReadFile::new()))
            .expect("registered");
        SessionHost {
            endpoint: Endpoint::new("https://example.invalid", "k"),
            dialect: Box::new(rune_net::chat_completions::ChatCompletions),
            model: "test".to_owned(),
            instructions: String::new(),
            tools: Vec::new(),
            rules: RuleSet::new(),
            mode: PermissionMode::Auto,
            effort: Effort::Auto,
            fast_mode: false,
            limits: BudgetSet::new(),
            context: ExecutionContext::new(camino::Utf8PathBuf::from("/tmp")),
            registry,
            cancellation: Cancellation::new(),
            steering: SteeringQueue::new(4),
            events: Arc::new(std::sync::Mutex::new(Vec::new())),
            interactive: false,
        }
    }
}
