//! Argument parsing.
//!
//! Leading global flags are parsed first, then the subcommand, then whatever
//! follows it. A global flag means the same thing on either side of the command,
//! and a flag after the command must be one the command declares. Parsing is
//! done with a small hand-rolled parser rather than a derive-based framework so
//! that the argument path adds no measurable startup cost; the specification
//! table in [`crate::spec`] is the single source of truth for what exists.

use std::ffi::OsString;

use rune_core::config::Layer;
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::spec;

/// A subcommand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    /// Run one request without an interactive session.
    Ask,
    /// Serve the Agent Client Protocol.
    Acp,
    /// Review the pending changes in the workspace.
    Review,
    /// Connect a model provider.
    Connect,
    /// List sessions.
    Sessions,
    /// Inspect, migrate, or recover one session.
    Session,
    /// Show the session branch structure.
    Tree,
    /// Report token usage.
    Usage,
    /// Show or manage credentials.
    Auth,
    /// List models.
    Models,
    /// Show permission state.
    Permissions,
    /// Inspect and change workspace trust.
    Projects,
    /// Show resolved configuration.
    Config,
    /// List limits.
    Limits,
    /// Manage additional workspace directories.
    Workspace,
    /// Show the assembled system prompt.
    Prompt,
    /// Show runtime status.
    Status,
    /// Check the local setup.
    Doctor,
    /// Upgrade the installed binary.
    Upgrade,
    /// Remove the installed binary.
    Uninstall,
    /// Print the generated command reference.
    Reference,
    /// Print help.
    Help,
    /// Print the version.
    Version,
    /// Start an interactive session.
    Interactive,
    /// Resume a saved session.
    Resume,
}

impl Command {
    /// Returns the canonical name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Acp => "acp",
            Self::Review => "review",
            Self::Connect => "connect",
            Self::Sessions => "sessions",
            Self::Session => "session",
            Self::Tree => "tree",
            Self::Usage => "usage",
            Self::Auth => "auth",
            Self::Models => "models",
            Self::Permissions => "permissions",
            Self::Projects => "projects",
            Self::Config => "config",
            Self::Limits => "limits",
            Self::Workspace => "workspace",
            Self::Prompt => "prompt",
            Self::Status => "status",
            Self::Doctor => "doctor",
            Self::Upgrade => "upgrade",
            Self::Uninstall => "uninstall",
            Self::Reference => "reference",
            Self::Help => "help",
            Self::Version => "version",
            Self::Interactive => "interactive",
            Self::Resume => "resume",
        }
    }
}

/// How a session is resumed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ResumeTarget {
    /// The latest session in the current workspace.
    Latest,
    /// An exact session identifier.
    Exact(String),
    /// Open the interactive picker.
    Picker,
}

/// Everything parsed from the command line.
#[derive(Clone, Debug)]
pub struct Launch {
    /// Command to run.
    pub command: Command,
    /// One limit override per entry, as `name=value`.
    pub limit_overrides: Vec<(String, String)>,
    /// Additional directories for this process.
    pub add_dirs: Vec<String>,
    /// Whether saved additional directories are ignored.
    pub no_additional_dirs: bool,
    /// Model override for this process.
    pub model: Option<String>,
    /// Provider override for this process.
    pub provider: Option<String>,
    /// Reasoning effort override.
    pub effort: Option<String>,
    /// Fast mode override.
    pub fast_mode: Option<bool>,
    /// Permission mode override.
    pub permission_mode: Option<String>,
    /// Theme override.
    pub theme: Option<String>,
    /// Provider order override.
    pub provider_order: Option<String>,
    /// Whether requests are restricted to the listed providers.
    pub provider_strict: Option<bool>,
    /// Whether every outbound request is refused.
    pub offline: bool,
    /// Whether a command may run where the host has no sandbox backend.
    pub allow_unsandboxed: bool,
    /// Whether output is machine readable.
    pub json: bool,
    /// Positional arguments after the command.
    pub args: Vec<String>,
    /// Flags the command declares, given after it, with their values.
    pub flags: Vec<(String, Option<String>)>,
    /// Session resume target, when the launch resumes.
    pub resume: Option<ResumeTarget>,
    /// Whether a benchmark run should exit after dispatch.
    pub benchmark: bool,
}

impl Launch {
    /// Returns a flag value given after the command.
    #[must_use]
    #[allow(dead_code)]
    pub fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(key, _)| key == name)
            .and_then(|(_, value)| value.as_deref())
    }

    /// Returns true when a boolean flag was given after the command.
    #[must_use]
    pub fn has_flag(&self, name: &str) -> bool {
        self.flags.iter().any(|(key, _)| key == name)
    }

    /// Returns limit overrides as typed pairs.
    #[allow(dead_code)]
    pub fn parsed_limits(&self) -> Result<Vec<(rune_core::LimitName, rune_core::Budget)>> {
        let mut out = Vec::new();
        for (name, value) in &self.limit_overrides {
            let key: rune_core::LimitName = name.parse()?;
            let budget: rune_core::Budget = value.parse()?;
            if let Some(raw) = budget.value().filter(|raw| !key.range().contains(*raw)) {
                return Err(RuneError::invalid_field(
                    name.clone(),
                    format!("{raw} is outside the accepted range"),
                ));
            }
            out.push((key, budget));
        }
        Ok(out)
    }
}

/// Parses the command line from the process environment.
///
/// The benchmark variable short-circuits everything after dispatch so startup
/// can be measured without constructing a runtime or touching the terminal.
pub fn parse_process() -> Result<Launch> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let benchmark = std::env::var_os("RUNE_BENCH").is_some();
    let mut launch = parse(args, benchmark)?;
    launch.benchmark = benchmark;
    Ok(launch)
}

/// Parses a command line.
///
/// Splits the input at the first non-flag token: everything before it is a
/// leading global flag, and everything after belongs to the command, apart from
/// global flags, which apply wherever they are written.
#[allow(clippy::too_many_lines)]
pub fn parse(args: Vec<OsString>, benchmark: bool) -> Result<Launch> {
    let tokens = to_strings(args)?;

    let mut launch = Launch {
        command: Command::Interactive,
        limit_overrides: Vec::new(),
        add_dirs: Vec::new(),
        no_additional_dirs: false,
        model: None,
        provider: None,
        effort: None,
        fast_mode: None,
        permission_mode: None,
        theme: None,
        provider_order: None,
        provider_strict: None,
        offline: false,
        allow_unsandboxed: false,
        json: false,
        args: Vec::new(),
        flags: Vec::new(),
        resume: None,
        benchmark,
    };

    #[allow(unused_mut)]
    let mut index = 0;
    let mut resume_requested = false;

    // Leading global flags.
    while index < tokens.len() {
        let token = tokens.get(index).cloned().unwrap_or_default();
        if !token.starts_with('-') || token == "-" {
            break;
        }
        if take_global(&mut launch, &tokens, &mut index)? {
            continue;
        }

        match token.as_str() {
            "-r" | "--resume-picker" => {
                launch.resume = Some(ResumeTarget::Picker);
                resume_requested = true;
                index = index.saturating_add(1);
            }
            "-c" | "--continue" | "--resume-last" => {
                launch.resume = Some(ResumeTarget::Latest);
                resume_requested = true;
                index = index.saturating_add(1);
            }
            "--resume" => {
                // `--resume` without a value means the latest session in the
                // current workspace, which is what `-c` already does.
                let next = tokens
                    .get(index.saturating_add(1))
                    .cloned()
                    .unwrap_or_default();
                if next.is_empty() || next.starts_with('-') {
                    launch.resume = Some(ResumeTarget::Latest);
                    index = index.saturating_add(1);
                } else {
                    launch.resume = Some(classify_resume(&next));
                    index = index.saturating_add(2);
                }
                resume_requested = true;
            }
            "-h" | "--help" => {
                launch.command = Command::Help;
                index = index.saturating_add(1);
                return finish(launch, &tokens, index);
            }
            "-v" | "--version" => {
                launch.command = Command::Version;
                index = index.saturating_add(1);
                return finish(launch, &tokens, index);
            }
            other if other.starts_with("--resume-") => {
                let id = other.trim_start_matches("--resume-");
                launch.resume = Some(classify_resume(id));
                resume_requested = true;
                index = index.saturating_add(1);
            }
            other => {
                return Err(unknown_flag(other));
            }
        }
    }

    // Subcommand.
    if let Some(token) = tokens.get(index).filter(|token| !token.starts_with('-')) {
        {
            launch.command = match token.as_str() {
                "ask" => Command::Ask,
                "acp" => Command::Acp,
                "review" | "pr" | "issue" => Command::Review,
                "connect" | "login" | "setup" | "provider" => Command::Connect,
                "sessions" => Command::Sessions,
                "session" => Command::Session,
                "tree" => Command::Tree,
                "usage" | "cost" => Command::Usage,
                "auth" | "logout" => Command::Auth,
                "models" => Command::Models,
                "permissions" => Command::Permissions,
                "projects" => Command::Projects,
                "config" | "settings" => Command::Config,
                "limits" => Command::Limits,
                "workspace" => Command::Workspace,
                "prompt" => Command::Prompt,
                "status" => Command::Status,
                "doctor" => Command::Doctor,
                "upgrade" => Command::Upgrade,
                "uninstall" => Command::Uninstall,
                "reference" => Command::Reference,
                "help" => Command::Help,
                "version" => Command::Version,
                "resume" => {
                    launch.resume = Some(ResumeTarget::Latest);
                    resume_requested = true;
                    Command::Resume
                }
                other => {
                    return Err(RuneError::new(
                        ErrorCode::InvalidField,
                        format!("`{other}` is not a command"),
                    )
                    .with_hint("run `rune help` to list the commands"));
                }
            };
            index = index.saturating_add(1);
        }
    }

    // `resume` may carry a target as its first positional argument.
    if launch.command == Command::Resume
        && let Some(token) = tokens.get(index).filter(|token| !token.starts_with('-'))
    {
        launch.resume = Some(classify_resume(token));
    }

    // `resume_requested` distinguishes an explicit resume from the default
    // interactive launch, which is used by the resume handling in a later phase.
    let _ = resume_requested;
    finish(launch, &tokens, index)
}

/// Applies one flag that changes a setting for the whole process.
///
/// Shared by the flags before the command and the flags after it, so a flag
/// means the same thing wherever it is written. Returns false, consuming
/// nothing, when the token is not one of these flags. A value may follow as
/// the next token or after an equals sign.
fn take_global(launch: &mut Launch, tokens: &[String], index: &mut usize) -> Result<bool> {
    let token = tokens.get(*index).cloned().unwrap_or_default();
    let (name, inline) = match token.split_once('=') {
        Some((name, value)) if name.starts_with("--") => (name.to_owned(), Some(value.to_owned())),
        _ => (token.clone(), None),
    };
    let canonical = match name.as_str() {
        "--permissions" => "--permission-mode",
        "--context-limit" => "--limit",
        other => other,
    };

    if matches!(
        canonical,
        "--fast"
            | "--no-fast"
            | "--no-additional-dirs"
            | "--provider-strict"
            | "--no-provider-strict"
            | "--offline"
            | "--allow-unsandboxed"
            | "--json"
    ) {
        if inline.is_some() {
            return Err(RuneError::invalid_field(
                canonical,
                format!("`{canonical}` takes no value"),
            ));
        }
        match canonical {
            "--fast" => launch.fast_mode = Some(true),
            "--no-fast" => launch.fast_mode = Some(false),
            "--no-additional-dirs" => launch.no_additional_dirs = true,
            "--provider-strict" => launch.provider_strict = Some(true),
            "--no-provider-strict" => launch.provider_strict = Some(false),
            "--offline" => launch.offline = true,
            "--allow-unsandboxed" => launch.allow_unsandboxed = true,
            _ => launch.json = true,
        }
        *index = (*index).saturating_add(1);
        return Ok(true);
    }

    if !matches!(
        canonical,
        "--model"
            | "--provider"
            | "--effort"
            | "--permission-mode"
            | "--limit"
            | "--add-dir"
            | "--theme"
            | "--provider-order"
    ) {
        return Ok(false);
    }
    let value = match inline {
        Some(value) => {
            *index = (*index).saturating_add(1);
            value
        }
        None => take_value(tokens, index, canonical)?,
    };
    match canonical {
        "--model" => launch.model = Some(value),
        "--provider" => launch.provider = Some(value),
        "--effort" => launch.effort = Some(value),
        "--permission-mode" => launch.permission_mode = Some(value),
        "--add-dir" => launch.add_dirs.push(value),
        "--theme" => launch.theme = Some(value),
        "--provider-order" => launch.provider_order = Some(value),
        _ => match value.split_once('=') {
            Some((key, limit)) if !key.is_empty() => {
                launch
                    .limit_overrides
                    .push((key.to_owned(), limit.to_owned()));
            }
            _ => {
                return Err(RuneError::invalid_field(
                    "--limit",
                    format!("`{value}` is not in name=value form"),
                )
                .with_hint("for example `--limit list_entries=50`"));
            }
        },
    }
    Ok(true)
}

/// Finishes parsing, collecting the flags and arguments after the command.
///
/// A flag the command declares is kept for it, and a flag that changes a
/// setting is applied as if it had been written before the command. Anything
/// else is refused, because a flag that is silently ignored reads as one that
/// worked. A bare `--` ends the flags.
///
/// The prompt of `ask` and the context of `review` are free text. Once that
/// text has begun, a word that is not a flag either of them takes is part of
/// it, so `rune ask why does ls -la fail` keeps `-la`.
fn finish(mut launch: Launch, tokens: &[String], mut index: usize) -> Result<Launch> {
    // Help and version print and exit, and whatever follows them is only read
    // as the name of a command to describe.
    let lenient = matches!(launch.command, Command::Help | Command::Version);
    let free_text = matches!(launch.command, Command::Ask | Command::Review);
    let declared: &[spec::FlagSpec] = spec::spec_for(launch.command).map_or(&[], |spec| spec.flags);
    let mut text_started = false;

    while index < tokens.len() {
        let token = tokens.get(index).cloned().unwrap_or_default();
        if token == "--" {
            // Everything after a bare double dash is a positional argument,
            // even when it looks like a flag.
            launch
                .args
                .extend(tokens.iter().skip(index.saturating_add(1)).cloned());
            break;
        }
        if !token.starts_with('-') || token.len() == 1 {
            launch.args.push(token);
            text_started = free_text;
            index = index.saturating_add(1);
            continue;
        }

        let (name, inline) = match token.split_once('=') {
            Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
            None => (token.clone(), None),
        };
        if let Some(flag) = declared.iter().find(|flag| flag.name == name) {
            let mut value = inline;
            if flag.value.is_some() && value.is_none() {
                // The next token is the value whatever it looks like, because
                // an identifier may itself begin with a dash.
                index = index.saturating_add(1);
                value = Some(tokens.get(index).cloned().ok_or_else(|| {
                    RuneError::new(
                        ErrorCode::MissingField,
                        format!("`{name}` requires a value"),
                    )
                })?);
            }
            launch.flags.push((name, value));
            index = index.saturating_add(1);
            continue;
        }
        if take_global(&mut launch, tokens, &mut index)? {
            continue;
        }
        if text_started || lenient {
            launch.args.push(token);
            index = index.saturating_add(1);
            continue;
        }
        if matches!(name.as_str(), "-h" | "--help") {
            launch.args = vec![launch.command.as_str().to_owned()];
            launch.command = Command::Help;
            return Ok(launch);
        }
        return Err(undeclared_flag(&token, launch.command));
    }
    Ok(launch)
}

/// Builds the error for a flag the command does not take.
fn undeclared_flag(raw: &str, command: Command) -> RuneError {
    let Some(spec) = spec::spec_for(command) else {
        return unknown_flag(raw);
    };
    let name = spec.name;
    let err = RuneError::new(
        ErrorCode::InvalidField,
        format!("`{raw}` is not a flag of `rune {name}`"),
    );
    if matches!(command, Command::Ask | Command::Review) {
        err.with_hint(format!(
            "run `rune help {name}` for its flags, or put `--` before text that starts with a dash"
        ))
    } else {
        err.with_hint(format!("run `rune help {name}` for its flags"))
    }
}

/// Classifies a resume argument as the latest session or an exact identifier.
fn classify_resume(raw: &str) -> ResumeTarget {
    if raw == "last" || raw.is_empty() {
        ResumeTarget::Latest
    } else {
        ResumeTarget::Exact(raw.to_owned())
    }
}

/// Builds an unknown-flag error naming the closest known flag.
fn unknown_flag(raw: &str) -> RuneError {
    let known: Vec<&str> = spec::GLOBAL_FLAGS.iter().map(|f| f.name).collect();
    RuneError::new(ErrorCode::InvalidField, format!("`{raw}` is not a flag"))
        .with_hint(format!("global flags are {}", known.join(", ")))
}

/// Consumes the value following a flag.
fn take_value(tokens: &[String], index: &mut usize, flag: &str) -> Result<String> {
    let value = tokens.get(index.saturating_add(1)).cloned();
    match value {
        Some(value) if !value.starts_with('-') || value == "-" => {
            *index = (*index).saturating_add(2);
            Ok(value)
        }
        _ => Err(RuneError::new(
            ErrorCode::MissingField,
            format!("`{flag}` requires a value"),
        )),
    }
}

/// Converts operating system strings to UTF-8, rejecting invalid input.
fn to_strings(args: Vec<OsString>) -> Result<Vec<String>> {
    args.into_iter()
        .map(|arg| {
            arg.into_string().map_err(|_| {
                RuneError::new(ErrorCode::InvalidField, "an argument is not valid UTF-8")
            })
        })
        .collect()
}

/// Applies parsed flags to settings, recording the command line as the source.
pub fn apply_to_settings(launch: &Launch, settings: &mut rune_core::config::Settings) {
    let layer = Layer::CommandLine;

    if let Some(model) = &launch.model {
        settings.model.clone_from(model);
        settings.sources.record("model", layer);
    }
    if let Some(provider) = &launch.provider {
        settings.provider = rune_core::config::parse_provider(provider);
        settings.sources.record("provider", layer);
    }
    if let Some(effort) = launch.effort.as_deref().and_then(parse_effort) {
        settings.effort = effort;
        settings.sources.record("effort", layer);
    }
    if let Some(fast) = launch.fast_mode {
        settings.fast_mode = fast;
        settings.sources.record("fast_mode", layer);
    }
    if let Some(parsed) = launch
        .permission_mode
        .as_deref()
        .and_then(rune_core::config::PermissionMode::from_name)
    {
        settings.permission_mode = parsed;
        settings.sources.record("permission_mode", layer);
    }
    if let Some(theme) = &launch.theme {
        settings.theme = Some(theme.clone());
        settings.sources.record("theme", layer);
    }
    if let Some(order) = &launch.provider_order {
        settings.provider_order = order
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect();
        settings.sources.record("provider_order", layer);
    }
    if let Some(strict) = launch.provider_strict {
        settings.provider_strict = strict;
        settings.sources.record("provider_strict", layer);
    }

    for directory in &launch.add_dirs {
        settings
            .additional_directories
            .push(camino::Utf8PathBuf::from(directory));
    }
    if !launch.add_dirs.is_empty() {
        settings.sources.record("additional_directories", layer);
    }
}

/// Parses an effort name given on the command line.
fn parse_effort(raw: &str) -> Option<rune_core::config::Effort> {
    use rune_core::config::Effort;
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(Effort::Auto),
        "none" => Some(Effort::None),
        "minimal" => Some(Effort::Minimal),
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" => Some(Effort::Xhigh),
        "max" => Some(Effort::Max),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn parse_list(list: &[&str]) -> Result<Launch> {
        parse(args(list), false)
    }

    #[test]
    fn no_arguments_starts_an_interactive_session() {
        let launch = parse_list(&[]).expect("parse");
        assert_eq!(launch.command, Command::Interactive);
        assert!(launch.args.is_empty());
    }

    #[test]
    fn subcommand_is_recognized() {
        assert_eq!(parse_list(&["ask"]).expect("parse").command, Command::Ask);
        assert_eq!(
            parse_list(&["doctor"]).expect("parse").command,
            Command::Doctor
        );
        assert_eq!(
            parse_list(&["sessions"]).expect("parse").command,
            Command::Sessions
        );
    }

    #[test]
    fn aliases_map_to_their_canonical_command() {
        assert_eq!(
            parse_list(&["cost"]).expect("parse").command,
            Command::Usage
        );
        assert_eq!(
            parse_list(&["settings"]).expect("parse").command,
            Command::Config
        );
        assert_eq!(parse_list(&["pr"]).expect("parse").command, Command::Review);
    }

    #[test]
    fn unknown_command_names_the_help_command() {
        let err = parse_list(&["frobnicate"]).expect_err("unknown");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(
            err.detail()
                .hint
                .as_deref()
                .expect("hint")
                .contains("rune help")
        );
    }

    #[test]
    fn global_flags_before_the_command_are_parsed() {
        let launch =
            parse_list(&["--model", "m", "--effort", "high", "ask", "hello"]).expect("parse");
        assert_eq!(launch.command, Command::Ask);
        assert_eq!(launch.model.as_deref(), Some("m"));
        assert_eq!(launch.effort.as_deref(), Some("high"));
        assert_eq!(launch.args, vec!["hello".to_owned()]);
    }

    #[test]
    fn flags_after_the_command_belong_to_the_command() {
        let launch = parse_list(&["ask", "--json", "--no-save", "hi"]).expect("parse");
        assert!(launch.has_flag("--json"));
        assert!(launch.has_flag("--no-save"));
        assert_eq!(launch.args, vec!["hi".to_owned()]);
    }

    #[test]
    fn command_flag_with_separate_value_is_captured() {
        let launch = parse_list(&["sessions", "--limit", "20", "--json"]).expect("parse");
        assert_eq!(launch.flag("--limit"), Some("20"));
        assert!(launch.has_flag("--json"));
    }

    #[test]
    fn command_flag_with_equals_value_is_captured() {
        let launch = parse_list(&["session", "last", "--id=abc123"]).expect("parse");
        assert_eq!(launch.flag("--id"), Some("abc123"));
    }

    #[test]
    fn limit_overrides_are_collected_and_validated() {
        let launch = parse_list(&[
            "--limit",
            "list_entries=50",
            "--limit",
            "read_file_lines=off",
            "ask",
        ])
        .expect("parse");
        assert_eq!(launch.limit_overrides.len(), 2);
        let parsed = launch.parsed_limits().expect("valid");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, rune_core::LimitName::ListEntries);
        assert_eq!(parsed[1].1, rune_core::Budget::Unbounded);
    }

    #[test]
    fn limit_without_equals_is_rejected() {
        let err = parse_list(&["--limit", "list_entries", "ask"]).expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn limit_out_of_range_is_rejected_when_resolved() {
        let launch =
            parse_list(&["--limit", "compaction_trigger_percent=5", "ask"]).expect("parse");
        let err = launch.parsed_limits().expect_err("out of range");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn unknown_limit_name_is_rejected_when_resolved() {
        let launch = parse_list(&["--limit", "nonsense=5", "ask"]).expect("parse");
        assert!(launch.parsed_limits().is_err());
    }

    #[test]
    fn flag_without_its_value_is_rejected() {
        let err = parse_list(&["--model"]).expect_err("missing value");
        assert_eq!(err.code(), ErrorCode::MissingField);
    }

    #[test]
    fn unknown_global_flag_lists_the_supported_set() {
        let err = parse_list(&["--nonsense"]).expect_err("unknown");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(
            err.detail()
                .hint
                .as_deref()
                .expect("hint")
                .contains("--model")
        );
    }

    #[test]
    fn help_and_version_short_circuit_before_a_command() {
        assert_eq!(
            parse_list(&["--help"]).expect("parse").command,
            Command::Help
        );
        assert_eq!(
            parse_list(&["-v"]).expect("parse").command,
            Command::Version
        );
        assert_eq!(
            parse_list(&["--version"]).expect("parse").command,
            Command::Version
        );
    }

    #[test]
    fn resume_flags_are_classified() {
        assert_eq!(
            parse_list(&["-c"]).expect("parse").resume,
            Some(ResumeTarget::Latest)
        );
        assert_eq!(
            parse_list(&["--resume"]).expect("parse").resume,
            Some(ResumeTarget::Latest)
        );
        assert_eq!(
            parse_list(&["--resume", "last"]).expect("parse").resume,
            Some(ResumeTarget::Latest)
        );
        assert_eq!(
            parse_list(&["--resume", "AbCdEf123456"])
                .expect("parse")
                .resume,
            Some(ResumeTarget::Exact("AbCdEf123456".to_owned()))
        );
        assert_eq!(
            parse_list(&["-r"]).expect("parse").resume,
            Some(ResumeTarget::Picker)
        );
        assert_eq!(
            parse_list(&["--resume-AbCdEf123456"])
                .expect("parse")
                .resume,
            Some(ResumeTarget::Exact("AbCdEf123456".to_owned()))
        );
        assert_eq!(
            parse_list(&["resume", "abc"]).expect("parse").resume,
            Some(ResumeTarget::Exact("abc".to_owned()))
        );
    }

    #[test]
    fn fast_and_no_fast_set_the_override_both_ways() {
        assert_eq!(
            parse_list(&["--fast"]).expect("parse").fast_mode,
            Some(true)
        );
        assert_eq!(
            parse_list(&["--no-fast"]).expect("parse").fast_mode,
            Some(false)
        );
    }

    #[test]
    fn repeatable_add_dir_collects_every_value() {
        let launch = parse_list(&["--add-dir", "/a", "--add-dir", "/b"]).expect("parse");
        assert_eq!(launch.add_dirs, vec!["/a".to_owned(), "/b".to_owned()]);
    }

    #[test]
    fn double_dash_stops_flag_parsing() {
        let launch = parse_list(&["ask", "--", "--not-a-flag"]).expect("parse");
        assert_eq!(launch.args, vec!["--not-a-flag".to_owned()]);
    }

    #[test]
    fn global_flags_after_the_command_are_applied() {
        // These used to be collected for the command and never read, so
        // `rune ask --offline hi` went to the network and `--model` after the
        // command was reported as no model selected.
        let launch = parse_list(&["ask", "--offline", "--model", "m", "--effort=high", "hi"])
            .expect("parse");
        assert_eq!(launch.command, Command::Ask);
        assert!(launch.offline);
        assert_eq!(launch.model.as_deref(), Some("m"));
        assert_eq!(launch.effort.as_deref(), Some("high"));
        assert_eq!(launch.args, vec!["hi".to_owned()]);

        let launch = parse_list(&["limits", "--limit", "max_agent_steps=5"]).expect("parse");
        assert_eq!(
            launch.limit_overrides,
            vec![("max_agent_steps".to_owned(), "5".to_owned())]
        );
        assert!(parse_list(&["models", "--offline"]).expect("parse").offline);
    }

    #[test]
    fn every_global_setting_flag_is_accepted_after_a_command() {
        for flag in spec::GLOBAL_FLAGS {
            if flag.name.starts_with("-h") || flag.name.starts_with("-v") {
                continue;
            }
            let mut list = vec!["status", flag.name];
            if flag.value.is_some() {
                list.push(if flag.name == "--limit" {
                    "list_entries=5"
                } else {
                    "value"
                });
            }
            let parsed = parse_list(&list);
            assert!(parsed.is_ok(), "{} was refused: {parsed:?}", flag.name);
        }
    }

    #[test]
    fn a_flag_the_command_does_not_take_is_refused() {
        let err = parse_list(&["models", "--nonsense"]).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.message().contains("--nonsense"), "{}", err.message());
        assert!(err.message().contains("rune models"), "{}", err.message());
        assert!(
            err.hint()
                .is_some_and(|hint| hint.contains("rune help models")),
            "{:?}",
            err.hint()
        );
    }

    #[test]
    fn a_command_flag_is_not_taken_for_a_global_one() {
        // `sessions` has its own `--limit`, a page size, which is not the
        // global limit override.
        let launch = parse_list(&["sessions", "--limit", "20"]).expect("parse");
        assert_eq!(launch.flag("--limit"), Some("20"));
        assert!(launch.limit_overrides.is_empty());
    }

    #[test]
    fn a_prompt_keeps_words_that_start_with_a_dash() {
        let launch = parse_list(&["ask", "why", "does", "ls", "-la", "fail"]).expect("parse");
        assert_eq!(launch.args.join(" "), "why does ls -la fail");
        // A flag written after the prompt is still a flag.
        let launch = parse_list(&["ask", "hi", "--offline"]).expect("parse");
        assert!(launch.offline);
        assert_eq!(launch.args, vec!["hi".to_owned()]);
    }

    #[test]
    fn a_dash_before_the_prompt_is_refused_unless_flags_are_ended() {
        let err = parse_list(&["ask", "-la", "is", "a", "flag"]).expect_err("refused");
        assert!(
            err.hint().is_some_and(|hint| hint.contains("--")),
            "{:?}",
            err.hint()
        );
        let launch = parse_list(&["ask", "--", "-la", "is", "a", "flag"]).expect("parse");
        assert_eq!(launch.args.join(" "), "-la is a flag");
    }

    #[test]
    fn help_after_a_command_describes_that_command() {
        let launch = parse_list(&["ask", "--help"]).expect("parse");
        assert_eq!(launch.command, Command::Help);
        assert_eq!(launch.args, vec!["ask".to_owned()]);
    }

    #[test]
    fn command_line_recorded_as_the_source_layer() {
        use rune_core::config::Settings;
        let launch = parse_list(&["--model", "m"]).expect("parse");
        let mut settings = Settings::default();
        apply_to_settings(&launch, &mut settings);
        assert_eq!(settings.model, "m");
        assert_eq!(settings.source_of("model"), Layer::CommandLine);
    }

    #[test]
    fn invalid_effort_on_the_command_line_is_ignored_rather_than_applied() {
        use rune_core::config::{Effort, Settings};
        let launch = parse_list(&["--effort", "sideways"]).expect("parse");
        let mut settings = Settings::default();
        apply_to_settings(&launch, &mut settings);
        assert_eq!(settings.effort, Effort::Auto);
        assert_eq!(settings.source_of("effort"), Layer::Default);
    }

    #[test]
    fn benchmark_flag_is_recorded() {
        let launch = parse(args(&["--help"]), true).expect("parse");
        assert!(launch.benchmark);
    }
}
