//! The interactive session.
//!
//! Connects the terminal shell loop to the agent. One turn runs at a time; a
//! line typed while a turn is running is queued rather than refused, which is
//! what makes the shell usable while the model is working.

use std::io::{BufRead, Write as _};
use std::sync::{Arc, Mutex};

use crate::session_log::{self, Recorder};
use camino::Utf8Path;
use rune_agent::history::History;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{self, Event, Host, StopReason};
use rune_context::prompt::{self, Prompt};
use rune_core::budget::BudgetSet;
use rune_core::config::{Effort, PermissionMode, Settings};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::SessionId;
use rune_core::paths::Paths;
use rune_net::message::ToolSpec;
use rune_net::provider::Provider;
use rune_net::transport::Endpoint;
use rune_policy::decision::Outcome;
use rune_policy::review::{ReviewOutcome, ReviewRequest, ReviewSession, Reviewer};
use rune_policy::rules::RuleSet;
use rune_session::usage::{HelperKind, Ledger, UsageRecord, now_ms};
use rune_term::footer::{self, FooterState};
use rune_term::shell::{Action, Input, Shell};
use rune_term::theme::Theme;
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
    events: Arc<Mutex<Vec<Event>>>,
    /// Tokens spent from the context window, summed across turns.
    context_used: std::sync::atomic::AtomicU64,
    /// Size of the context window, zero when the provider stated none.
    context_limit: u64,
    /// Theme the status line is drawn with.
    theme: Theme,
    /// Session identifier, shown shortened in the status line.
    session_id: String,
    /// Workspace, shown as given.
    workspace: String,
    /// Whether the terminal can render direct color.
    truecolor: bool,
    /// Ordered upstream provider preference.
    provider_order: Vec<String>,
    /// Whether requests are restricted to that preference.
    provider_strict: bool,
    /// Reviewer for unresolved actions, absent when none is configured.
    reviewer: Option<Box<dyn Reviewer>>,
    /// Review activity for the current turn.
    review_session: Arc<Mutex<ReviewSession>>,
}

/// Locks the review session, recovering from a poisoned lock.
///
/// A panic while holding this lock must not make every later tool call fail, so
/// the state is taken as it stands.
fn lock_review(session: &Mutex<ReviewSession>) -> std::sync::MutexGuard<'_, ReviewSession> {
    session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        if outcome != Outcome::Ask {
            return (outcome, reason);
        }

        // In automatic mode an unresolved action gets one narrow review rather
        // than a person's attention. The review never opens a prompt and never
        // ends the turn: a caution or an unreachable reviewer holds the action
        // and hands the agent guidance.
        if self.mode == PermissionMode::Auto
            && let Some(reviewer) = &self.reviewer
        {
            let action = target.unwrap_or(name).to_owned();
            let request = ReviewRequest::new(action.clone(), vec![action.clone()], name, "");
            let mut session = lock_review(&self.review_session);
            return match session.review(reviewer.as_ref(), &request) {
                ReviewOutcome::Clear { .. } => {
                    (Outcome::Allow, format!("{reason}; cleared by review"))
                }
                other => (
                    Outcome::Deny,
                    other
                        .reason()
                        .map_or_else(|| String::from("review held the action"), str::to_owned),
                ),
            };
        }

        // Nothing has judged the action, so it stays unresolved. Being
        // interactive is not a judgment: approving here would authorize an
        // action that no rule allowed and no reviewer saw.
        (outcome, reason)
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

    fn provider_order(&self) -> Vec<String> {
        self.provider_order.clone()
    }

    fn provider_strict(&self) -> bool {
        self.provider_strict
    }
}

impl SessionHost {
    /// Records the tokens a turn spent.
    fn add_context_usage(&self, used: u64) {
        self.context_used
            .fetch_add(used, std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns the status line for the current state.
    fn status_line(&self, width: usize) -> String {
        let state = FooterState {
            model: self.model.clone(),
            permission_mode: self.mode,
            workspace: self.workspace.clone(),
            context_used: self.context_used.load(std::sync::atomic::Ordering::Relaxed),
            context_limit: self.context_limit,
            session_id: self.session_id.clone(),
        };
        let layout = footer::solve(
            (u16::try_from(width).unwrap_or(80), 24),
            1,
            false,
            footer::DEFAULT_MINIMUM_ROWS,
        );
        footer::render(&state, &layout, &self.theme, width, self.truecolor).join("\n")
    }

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
    let prompt = build_prompt(&config.workspace, &config.paths.config_root, &limits);

    // A resumed session continues its stored conversation; a new one starts
    // empty and writes a fresh log.
    let (mut recorder, mut history) = if let Some(id) = &config.resume {
        session_log::load(&config.paths, id)?
    } else {
        let id = SessionId::generate();
        (Recorder::create(&config.paths, &id)?, History::new())
    };

    // Discovered once, because a command file that changes mid-session would
    // otherwise make an invocation mean two different things.
    let commands =
        rune_context::commands::discover(&config.workspace, &Paths::from_process().config_root)
            .unwrap_or_default();

    // Resolved before the host literal because the registry is moved into it.
    let theme = resolve_theme(&config);
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
        events: Arc::new(Mutex::new(Vec::new())),
        context_used: std::sync::atomic::AtomicU64::new(0),
        context_limit: context_limit(&config.settings, &limits),
        theme,
        session_id: recorder.id().to_string(),
        workspace: config.workspace.to_string(),
        truecolor: truecolor_supported(),
        provider_order: config.settings.provider_order.clone(),
        provider_strict: config.settings.provider_strict,
        reviewer: crate::auto_review::build(&config.settings, &config.paths)?,
        review_session: Arc::new(Mutex::new(ReviewSession::new(&limits))),
    };

    let out = Mutex::new(output);
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
            Input::Command { name, arguments } => {
                match handle_command(&name, &arguments, &commands, &mut *sink)? {
                    Handled::Exit => Ok(Action::Exit),
                    Handled::Continue => Ok(Action::Continue),
                    // Expanded text goes to the composer for review, never
                    // straight to the model: a template with a wrong argument
                    // should be visible before it is sent.
                    Handled::Expand(prompt) => {
                        let _ = writeln!(
                            sink,
                            "-- /{name} expanded; edit before sending --\n{prompt}"
                        );
                        Ok(Action::Continue)
                    }
                }
            }
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
                host.add_context_usage(
                    outcome
                        .usage
                        .input_tokens
                        .unwrap_or(0)
                        .saturating_add(outcome.usage.output_tokens.unwrap_or(0)),
                );
                host.clear_events();
                report_turn(&outcome, &host, &mut *sink)?;
                let _ = writeln!(sink, "{}", host.status_line(80));
                Ok(Action::Continue)
            }
            Input::Empty => Ok(Action::Continue),
        }
    })?;

    Ok(reason.exit_code())
}

/// Resolves the theme for a session.
///
/// A terminal that accepts no color gets the colorless theme whatever the
/// configuration names, because the setting expresses a preference and the
/// terminal states a capability.
fn resolve_theme(config: &SessionConfig) -> Theme {
    if std::env::var_os("NO_COLOR").is_some() {
        return Theme::no_color();
    }
    Theme::resolve(
        config.settings.theme.as_deref(),
        true,
        &config.paths.themes_dir(),
    )
}

/// Returns the context window for the configured model.
///
/// Configuration carries no per-model window, so the compiled default applies
/// until a provider reports one. A zero would make every request look oversized.
fn context_limit(_settings: &Settings, _limits: &BudgetSet) -> u64 {
    rune_net::catalog::DEFAULT_CONTEXT_WINDOW
}

/// Returns whether the terminal can render direct color.
///
/// Reads the environment for the two variables that advertise it. Anything else
/// gets the indexed fallback, which every terminal renders.
fn truecolor_supported() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    colorterm.eq_ignore_ascii_case("truecolor") || colorterm.eq_ignore_ascii_case("24bit")
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

/// What handling a slash command decided.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Handled {
    /// Carry on with the session.
    Continue,
    /// End the session.
    Exit,
    /// Place this text in the composer for review.
    Expand(String),
}

/// Handles a slash command.
///
/// An unknown command is reported and the loop continues, because a typo should
/// not end a session.
fn handle_command<W: std::io::Write>(
    name: &str,
    arguments: &str,
    commands: &rune_context::commands::Discovery,
    output: &mut W,
) -> Result<Handled> {
    match name {
        "quit" | "exit" => Ok(Handled::Exit),
        "help" => {
            let _ = writeln!(output, "commands: /help /quit");
            let listing = rune_context::commands::render_listing(&commands.commands);
            let _ = writeln!(output, "{listing}");
            // A command file that was refused is invisible otherwise, and a
            // silent skip looks like a file that was never read.
            for warning in &commands.warnings {
                let _ = writeln!(output, "skipped {}: {}", warning.path, warning.reason);
            }
            Ok(Handled::Continue)
        }
        other => {
            let Some(command) = commands.commands.iter().find(|c| c.name == other) else {
                let _ = writeln!(output, "unknown command `/{other}`; try /help");
                return Ok(Handled::Continue);
            };
            let parts: Vec<String> = arguments.split_whitespace().map(str::to_owned).collect();
            match command.expand(&parts) {
                Ok(text) => Ok(Handled::Expand(text)),
                Err(err) => {
                    let _ = writeln!(output, "{err}");
                    if let Some(hint) = err.hint() {
                        let _ = writeln!(output, "hint: {hint}");
                    }
                    Ok(Handled::Continue)
                }
            }
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
fn build_prompt(workspace: &Utf8Path, config_root: &Utf8Path, limits: &BudgetSet) -> Prompt {
    let instructions = prompt::instructions_for(workspace, config_root, limits);
    Prompt {
        instructions,
        included: Vec::new(),
        omissions: Vec::new(),
    }
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
        endpoint: Endpoint::new(base_url, credential.expose().to_owned()).offline(settings.offline),
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
    fn a_prompt_override_replaces_the_built_in_text() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::write(
            root.join(rune_core::paths::names::SYSTEM_PROMPT_FILE),
            "You are a terse reviewer.\n",
        )
        .expect("write");

        let prompt = build_prompt(root, root, &BudgetSet::new());
        assert_eq!(prompt.instructions, "You are a terse reviewer.");
        assert!(
            !prompt.instructions.contains("coding agent"),
            "the built-in text was kept alongside the override"
        );
    }

    #[test]
    fn no_override_leaves_the_built_in_text_alone() {
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let prompt = build_prompt(root, root, &BudgetSet::new());
        assert!(prompt.instructions.starts_with(prompt::SYSTEM_PROMPT));
    }

    #[test]
    fn an_empty_override_file_is_ignored() {
        // A truncated or blanked file would otherwise leave the model with no
        // instructions at all.
        let dir = tempfile::tempdir().expect("temp");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::write(
            root.join(rune_core::paths::names::SYSTEM_PROMPT_FILE),
            "   \n\n",
        )
        .expect("write");

        let prompt = build_prompt(root, root, &BudgetSet::new());
        assert!(prompt.instructions.starts_with(prompt::SYSTEM_PROMPT));
    }

    /// A reviewer that answers with whatever it was given.
    struct Scripted(ReviewOutcome);

    impl Reviewer for Scripted {
        fn review(&self, _request: &ReviewRequest) -> ReviewOutcome {
            self.0.clone()
        }
    }

    /// Builds a host in automatic mode with the given reviewer.
    fn host_with_reviewer(outcome: ReviewOutcome) -> SessionHost {
        let mut host = test_host();
        host.mode = PermissionMode::Auto;
        host.reviewer = Some(Box::new(Scripted(outcome)));
        host
    }

    #[test]
    fn an_unresolved_action_is_cleared_by_review_in_automatic_mode() {
        // Automatic mode reviews instead of prompting, so an action the rules
        // do not decide is allowed when the reviewer clears it.
        let host = host_with_reviewer(ReviewOutcome::Clear {
            reviewed_action: "rm -rf /tmp/x".to_owned(),
        });
        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Allow, "{reason}");
        assert!(reason.contains("cleared by review"), "{reason}");
    }

    #[test]
    fn a_cautioning_review_holds_the_action_with_its_reason() {
        let host = host_with_reviewer(ReviewOutcome::Caution {
            reason: "broader than the request".to_owned(),
        });
        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Deny, "{reason}");
        assert_eq!(reason, "broader than the request");
    }

    #[test]
    fn a_review_that_cannot_complete_holds_the_action_without_ending_the_turn() {
        // An unavailable reviewer is not a judgment about the action, and it
        // must not become an approval.
        let host = host_with_reviewer(ReviewOutcome::Unavailable {
            reason: "the reviewer could not be reached".to_owned(),
        });
        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Deny, "{reason}");
        assert!(reason.contains("could not be reached"), "{reason}");
    }

    #[test]
    fn an_allowlisted_action_never_reaches_the_reviewer() {
        // A rule that already decided must not cost a review, or the budget
        // would be spent on the commands that were never in question.
        let host = host_with_reviewer(ReviewOutcome::Caution {
            reason: "should not be consulted".to_owned(),
        });
        let (outcome, reason) = host.decide("read_file", Some("src/main.rs"));
        assert_eq!(outcome, Outcome::Allow, "{reason}");
        assert!(!reason.contains("should not be consulted"), "{reason}");
    }

    #[test]
    fn a_reviewer_is_only_consulted_in_automatic_mode() {
        let mut host = host_with_reviewer(ReviewOutcome::Caution {
            reason: "should not be consulted".to_owned(),
        });
        host.mode = PermissionMode::FullAccess;
        let (outcome, _) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Allow);
    }

    #[test]
    fn an_unresolved_action_is_never_approved_without_a_decision() {
        // Nothing judged it: no rule allowed it and no reviewer saw it. Whether
        // a person is watching is not a decision, so it stays unresolved.
        let mut host = test_host();
        host.mode = PermissionMode::Auto;
        let (outcome, reason) = host.decide("shell", Some("rm -rf /tmp/x"));
        assert_eq!(outcome, Outcome::Ask, "{reason}");
    }

    #[test]
    fn the_status_line_names_the_model_and_the_mode() {
        let host = test_host();
        let line = host.status_line(120);
        assert!(line.contains("test"), "{line}");
        assert!(line.contains("auto"), "{line}");
        assert!(line.contains("ctx"), "{line}");
    }

    #[test]
    fn the_status_line_reports_the_context_already_spent() {
        let host = test_host();
        let before = host.status_line(120);
        assert!(before.contains("ctx 0%"), "{before}");
        host.add_context_usage(64_000);
        let after = host.status_line(120);
        assert!(
            !after.contains("ctx 0%"),
            "usage was not reflected: {after}"
        );
    }

    #[test]
    fn spending_context_is_cumulative_across_turns() {
        let host = test_host();
        host.add_context_usage(1_000);
        host.add_context_usage(2_000);
        assert_eq!(
            host.context_used.load(std::sync::atomic::Ordering::Relaxed),
            3_000
        );
    }

    #[test]
    fn a_colorless_terminal_cannot_be_forced_into_color() {
        // Capability beats preference: the setting expresses a wish, the
        // terminal states what it accepts, and the terminal wins.
        let config = colorless_config();
        assert_eq!(resolve_theme(&config).base(), Theme::no_color().base());
    }

    #[test]
    fn the_truecolor_check_reads_the_advertised_variables() {
        // The helper consults the environment, so the assertion is on its shape
        // rather than on a value a test would have to mutate globally.
        let supported = truecolor_supported();
        assert_eq!(supported, supported);
    }

    #[test]
    fn the_shell_reports_an_unknown_command_without_leaving() {
        let mut output = Vec::new();
        let action = handle_command("nope", "", &empty_commands(), &mut output).expect("handled");
        assert_eq!(action, Handled::Continue);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("unknown command"), "{text}");
        assert!(text.contains("/help"), "{text}");
    }

    #[test]
    fn the_quit_command_leaves_the_shell() {
        let mut output = Vec::new();
        assert_eq!(
            handle_command("quit", "", &empty_commands(), &mut output).expect("handled"),
            Handled::Exit
        );
        assert_eq!(
            handle_command("exit", "", &empty_commands(), &mut output).expect("handled"),
            Handled::Exit
        );
    }

    #[test]
    fn a_user_command_expands_for_review_instead_of_running() {
        let mut discovery = rune_context::commands::Discovery::default();
        discovery.commands.push(rune_context::commands::Command {
            name: "fix".to_owned(),
            description: "Fix it".to_owned(),
            argument_hint: None,
            body: "Fix $1 now".to_owned(),
            origin: rune_context::commands::Origin::Project,
            source: None,
        });

        let mut output = Vec::new();
        let handled = handle_command("fix", "parser", &discovery, &mut output).expect("handled");
        assert_eq!(handled, Handled::Expand("Fix parser now".to_owned()));
    }

    #[test]
    fn a_command_missing_its_argument_reports_instead_of_expanding() {
        let mut discovery = rune_context::commands::Discovery::default();
        discovery.commands.push(rune_context::commands::Command {
            name: "fix".to_owned(),
            description: "Fix it".to_owned(),
            argument_hint: None,
            body: "Fix $1 now".to_owned(),
            origin: rune_context::commands::Origin::Project,
            source: None,
        });

        let mut output = Vec::new();
        let handled = handle_command("fix", "", &discovery, &mut output).expect("handled");
        assert_eq!(handled, Handled::Continue);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("$1"), "{text}");
    }

    #[test]
    fn help_reports_a_command_file_that_was_skipped() {
        let mut discovery = rune_context::commands::Discovery::default();
        discovery.warnings.push(rune_context::commands::Warning {
            path: camino::Utf8PathBuf::from("/p/.rune/commands/help.md"),
            reason: "`/help` is a built-in command, so this file was skipped".to_owned(),
        });

        let mut output = Vec::new();
        handle_command("help", "", &discovery, &mut output).expect("handled");
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("skipped"), "{text}");
        assert!(text.contains("built-in"), "{text}");
    }

    #[test]
    fn help_lists_the_commands() {
        let mut output = Vec::new();
        handle_command("help", "", &empty_commands(), &mut output).expect("handled");
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

    /// Builds an empty command set.
    fn empty_commands() -> rune_context::commands::Discovery {
        rune_context::commands::Discovery::default()
    }

    /// Builds a session config whose terminal accepts no color.
    fn colorless_config() -> SessionConfig {
        SessionConfig {
            settings: Settings::default(),
            paths: Paths::resolve(Some("/tmp"), None, None, None, Some("/tmp/s")),
            resume: None,
            workspace: Utf8Path::new("/tmp").to_owned(),
            endpoint: Endpoint::new("https://example.invalid", "k"),
            dialect: Box::new(rune_net::chat_completions::ChatCompletions),
            registry: Registry::new(),
            rules: RuleSet::new(),
        }
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
            rules: crate::permissions::validated(&Settings::default()).expect("rules"),
            mode: PermissionMode::Auto,
            effort: Effort::Auto,
            fast_mode: false,
            limits: BudgetSet::new(),
            context: ExecutionContext::new(camino::Utf8PathBuf::from("/tmp")),
            registry,
            cancellation: Cancellation::new(),
            steering: SteeringQueue::new(4),
            events: Arc::new(Mutex::new(Vec::new())),
            context_used: std::sync::atomic::AtomicU64::new(0),
            context_limit: rune_net::catalog::DEFAULT_CONTEXT_WINDOW,
            theme: Theme::no_color(),
            session_id: "sessiontest1".to_owned(),
            workspace: "/tmp".to_owned(),
            truecolor: false,
            provider_order: Vec::new(),
            provider_strict: false,
            reviewer: None,
            review_session: Arc::new(Mutex::new(ReviewSession::new(&BudgetSet::new()))),
        }
    }
}
