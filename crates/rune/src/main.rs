//! Command entry point.
//!
//! The order here is deliberate. Argument parsing, help, and version run before
//! anything else touches configuration, the filesystem, or a runtime, which is
//! what keeps the startup path free of that cost. A command only loads what its
//! specification declares it needs.

#![forbid(unsafe_code)]
// Tests assert by panicking. The guards that forbid panicking apply to the
// shipped build, where a panic on user input is a defect.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

mod ask;
mod cli;
mod diagnostics;
mod help;
mod session;
mod spec;
mod version;

use std::process::ExitCode;

use camino::Utf8PathBuf;
use rune_core::config::{self, EnvironmentOverrides, Layer, Provider, Settings};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;

use crate::cli::{Command, Launch};

/// Exit code for a successful run.
const EXIT_OK: u8 = 0;

/// Exit code for a failed run.
const EXIT_FAILURE: u8 = 1;

/// Exit code for an interrupt.
const EXIT_INTERRUPTED: u8 = 130;

fn main() -> ExitCode {
    let launch = match cli::parse_process() {
        Ok(launch) => launch,
        Err(err) => {
            report_error(&err);
            return ExitCode::from(EXIT_FAILURE);
        }
    };

    // Commands that need nothing from the runtime return before any load.
    if let Some(code) = handle_runtime_free(&launch) {
        return code;
    }

    // The startup benchmark measures everything up to and including dispatch,
    // then exits without loading configuration or touching the terminal.
    if launch.benchmark {
        return ExitCode::from(EXIT_OK);
    }

    match run(&launch) {
        Ok(code) => code,
        Err(err) => {
            report_error(&err);
            ExitCode::from(exit_code_for(&err))
        }
    }
}

/// Handles commands that must not load configuration or a runtime.
///
/// Returns `None` when the command needs more than this.
fn handle_runtime_free(launch: &Launch) -> Option<ExitCode> {
    match launch.command {
        Command::Help => {
            let text = match launch.args.first() {
                Some(name) => help::render_command(name).unwrap_or_else(|| {
                    format!(
                        "rune: `{name}` is not a command\n{}\n",
                        help::unknown_command_hint()
                    )
                }),
                None => help::render_top_level(),
            };
            print!("{text}");
            Some(ExitCode::from(EXIT_OK))
        }
        Command::Version => {
            println!("{}", version::version_line());
            Some(ExitCode::from(EXIT_OK))
        }
        _ => None,
    }
}

/// Runs a command that needs the resolved environment.
fn run(launch: &Launch) -> Result<ExitCode> {
    let paths = Paths::from_process();
    let workspace = current_workspace()?;

    // A command declares whether it needs configuration. Consulting the
    // specification here is what keeps commands that only need paths from
    // paying for a full five-layer merge.
    let requirements =
        spec::spec_for(launch.command).map_or(spec::Requirements::FULL, |spec| spec.requirements);

    let mut settings = if requirements.config {
        let config_override = std::env::var("RUNE_CONFIG").ok();
        let config_file = paths.config_file(config_override.as_deref());
        let env = EnvironmentOverrides::from_process();
        config::load(
            Some(&workspace.join(".rune.toml")),
            Some(&config_file),
            &env,
        )
    } else {
        Settings::default()
    };

    // Command-line flags sit above every file and environment layer.
    cli::apply_to_settings(launch, &mut settings);
    apply_limit_overrides(launch, &mut settings)?;

    // Output flags are accepted on either side of the command name, because
    // both `rune --json status` and `rune status --json` read naturally.
    let output_flags = OutputFlags {
        json: launch.json || launch.has_flag("--json"),
    };

    if launch.offline {
        // Recorded here so the transport can enforce it.
        settings.sources.record("offline", Layer::CommandLine);
    }

    match launch.command {
        Command::Doctor => run_doctor(&settings, &paths, &workspace, &output_flags),
        Command::Status => run_status(&settings, &paths, &workspace, &output_flags),
        Command::Limits => run_limits(&settings, &output_flags),
        Command::Config => run_config(&settings, &output_flags),
        Command::Prompt => run_prompt(&settings, &output_flags),
        Command::Sessions | Command::Session | Command::Tree => {
            Err(not_yet_available("session storage"))
        }
        Command::Usage => Err(not_yet_available("the usage ledger")),
        Command::Auth | Command::Connect => Err(not_yet_available("provider connection")),
        Command::Models => Err(not_yet_available("the model catalog")),
        Command::Permissions => Err(not_yet_available("the permission engine")),
        Command::Workspace => run_workspace(&settings, launch, &output_flags),
        Command::Ask => run_ask(&settings, &paths, launch, &output_flags),
        Command::Acp => run_acp(&settings, &paths, &workspace, launch),
        Command::Interactive | Command::Resume => run_interactive(&settings, &paths, &workspace),
        Command::Review => Err(not_yet_available("the review command")),
        Command::Upgrade | Command::Uninstall => Err(not_yet_available("the installer")),
        Command::Help | Command::Version => Ok(ExitCode::from(EXIT_OK)),
    }
}

/// Applies command-line limit overrides.
fn apply_limit_overrides(launch: &Launch, settings: &mut Settings) -> Result<()> {
    let mut diagnostics = Vec::new();
    for (name, value) in &launch.limit_overrides {
        config::apply_limit_string(
            &mut settings.limits,
            name,
            value,
            Layer::CommandLine,
            &mut diagnostics,
        );
    }
    if let Some(diagnostic) = diagnostics.into_iter().next() {
        return Err(RuneError::new(diagnostic.code, diagnostic.message));
    }
    Ok(())
}

/// Returns the workspace root for this process.
fn current_workspace() -> Result<Utf8PathBuf> {
    let cwd = std::env::current_dir()?;
    Utf8PathBuf::from_path_buf(cwd).map_err(|path| {
        RuneError::new(
            ErrorCode::InvalidField,
            format!(
                "the working directory is not valid UTF-8: {}",
                path.display()
            ),
        )
    })
}

/// Runs `doctor`.
fn run_doctor(
    settings: &Settings,
    paths: &Paths,
    workspace: &camino::Utf8Path,
    output: &OutputFlags,
) -> Result<ExitCode> {
    let report = diagnostics::run(settings, paths, workspace);
    if output.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        print!("{}", report.render());
    }
    let code = report.exit_code();
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(EXIT_FAILURE)))
}

/// Runs `ask`.
///
/// With `--json` the result object is always printed, including on failure, so a
/// caller parsing standard output never receives an empty stream.
fn run_ask(
    settings: &Settings,
    paths: &Paths,
    launch: &Launch,
    output: &OutputFlags,
) -> Result<ExitCode> {
    let submitted: String = launch.args.join(" ");
    let prompt = if submitted.trim().is_empty() {
        ask::read_stdin_prompt()?
    } else {
        submitted
    };

    if prompt.trim().is_empty() {
        return Err(RuneError::missing_field("prompt")
            .with_hint("pass the prompt as an argument, or pipe it on standard input"));
    }

    let options = ask::Options {
        prompt,
        json: output.json,
        no_save: launch.has_flag("--no-save"),
        model: launch.flag("--model").map(str::to_owned),
        effort: launch.flag("--effort").map(str::to_owned),
    };

    match ask::run(settings, paths, &options) {
        Ok(result) => {
            let code = ask::report(&result, &options)?;
            Ok(ExitCode::from(code))
        }
        Err(err) => {
            if options.json {
                let result =
                    ask::JsonResult::failure(&settings.model, &err, i32::from(EXIT_FAILURE));
                let _ = ask::report(&result, &options);
            }
            Err(err)
        }
    }
}

/// Runs the interactive session.
fn run_interactive(
    settings: &Settings,
    paths: &Paths,
    workspace: &camino::Utf8Path,
) -> Result<ExitCode> {
    let config = session::prepare(settings, paths, workspace)?;
    let stdin = std::io::stdin();
    let input = std::io::BufReader::new(stdin.lock());
    let code = session::run(config, input, std::io::stdout())?;
    Ok(ExitCode::from(code))
}

/// Runs `acp`.
///
/// Fails before touching the transport when the endpoint or credential is
/// missing, so a client gets a usable error rather than a connection that
/// accepts a session and then cannot run a turn.
fn run_acp(
    settings: &Settings,
    paths: &Paths,
    workspace: &camino::Utf8Path,
    launch: &Launch,
) -> Result<ExitCode> {
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

    let dialect = match settings.provider {
        Provider::Anthropic => rune_acp::Dialect::Anthropic,
        Provider::Responses => rune_acp::Dialect::Responses,
        _ => rune_acp::Dialect::ChatCompletions,
    };

    let registry = rune_tools::inventory::builtin(
        &rune_tools::workspace::FileLimits::from_budget(&settings.limits),
        &settings.limits,
    )?;

    let rules = rune_policy::rules::RuleSet::new();

    let log_file = launch.flag("--log-file").map(Utf8PathBuf::from);

    let config = rune_acp::ServerConfig {
        paths: paths.clone(),
        workspace: workspace.to_owned(),
        endpoint: rune_net::transport::Endpoint::new(base_url, credential.expose().to_owned()),
        dialect,
        model: settings.model.clone(),
        instructions: rune_context::prompt::SYSTEM_PROMPT.to_owned(),
        registry,
        rules,
        mode: settings.permission_mode,
        effort: settings.effort,
        limits: settings.limits.clone(),
        log_file,
        context_window: rune_net::catalog::DEFAULT_CONTEXT_WINDOW,
    };

    let server = std::sync::Arc::new(rune_acp::Server::new(config, std::io::stdout())?);
    let input = std::io::BufReader::new(std::io::stdin());
    server.run(input)?;
    Ok(ExitCode::from(EXIT_OK))
}

/// Runs `status`.
fn run_status(
    settings: &Settings,
    paths: &Paths,
    workspace: &camino::Utf8Path,
    output: &OutputFlags,
) -> Result<ExitCode> {
    if output.json {
        let value = diagnostics::status_json(settings, paths, workspace);
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        println!("{}", version::version_line());
        println!("workspace      {workspace}");
        println!("provider       {}", settings.provider);
        println!(
            "model          {}",
            if settings.model.is_empty() {
                "none selected"
            } else {
                settings.model.as_str()
            }
        );
        println!("permissions    {}", settings.permission_mode.label());
        println!("config root    {}", paths.config_root);
        println!("state root     {}", paths.state_root);
        println!("data root      {}", paths.data_root);
    }
    Ok(ExitCode::from(EXIT_OK))
}

/// Runs `limits`.
fn run_limits(settings: &Settings, output: &OutputFlags) -> Result<ExitCode> {
    if output.json {
        let value = diagnostics::limits_json(settings);
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        print!("{}", diagnostics::limits_text(settings));
    }
    Ok(ExitCode::from(EXIT_OK))
}

/// Runs `config`.
fn run_config(settings: &Settings, output: &OutputFlags) -> Result<ExitCode> {
    if output.json {
        let value = diagnostics::config_json(settings);
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        print!("{}", diagnostics::config_text(settings));
    }
    Ok(ExitCode::from(EXIT_OK))
}

/// Runs `prompt`, which reports the prompt source without contacting a model.
fn run_prompt(settings: &Settings, _output: &OutputFlags) -> Result<ExitCode> {
    let paths = Paths::from_process();
    let override_path = paths.system_prompt_file();
    match std::fs::metadata(&override_path) {
        Ok(_) => println!("system prompt  {override_path} (override)"),
        Err(_) => println!("system prompt  built in (no override at {override_path})"),
    }
    println!(
        "context        {}",
        if settings.context {
            "enabled"
        } else {
            "disabled"
        }
    );
    Ok(ExitCode::from(EXIT_OK))
}

/// Runs `workspace`.
fn run_workspace(settings: &Settings, launch: &Launch, output: &OutputFlags) -> Result<ExitCode> {
    let action = launch.args.first().map_or("list", String::as_str);
    match action {
        "list" => {
            if output.json {
                let value = serde_json::json!({
                    "directories": settings.additional_directories,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
                );
            } else if settings.additional_directories.is_empty() {
                println!("no additional directories");
            } else {
                for directory in &settings.additional_directories {
                    println!("{directory}");
                }
            }
            Ok(ExitCode::from(EXIT_OK))
        }
        "add" | "remove" | "clear" => Err(not_yet_available("saving workspace directories")),
        other => Err(RuneError::invalid_field(
            "workspace",
            format!("`{other}` is not a subcommand"),
        )
        .with_hint("use list, add, remove, or clear")),
    }
}

/// Output-related flags, resolved from either side of the command name.
#[derive(Clone, Copy, Debug)]
struct OutputFlags {
    /// Emit machine-readable output.
    json: bool,
}

/// Builds the error used by a surface that a later phase provides.
///
/// Distinct from a generic failure so the message is honest about what exists.
fn not_yet_available(what: &str) -> RuneError {
    RuneError::new(
        ErrorCode::Unsupported,
        format!("{what} is not available in this build"),
    )
    .with_hint("this command lands with a later change")
}

/// Maps an error to a process exit code.
fn exit_code_for(err: &RuneError) -> u8 {
    match err.code() {
        ErrorCode::Cancelled => EXIT_INTERRUPTED,
        _ => EXIT_FAILURE,
    }
}

/// Prints an error to standard error.
fn report_error(err: &RuneError) {
    eprintln!("rune: {err}");
    if let Some(hint) = &err.detail().hint {
        eprintln!("hint: {hint}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_free_commands_are_handled_without_loading() {
        for name in ["help", "version"] {
            let launch = cli::parse(vec![std::ffi::OsString::from(name)], false).expect("parse");
            assert!(
                handle_runtime_free(&launch).is_some(),
                "{name} should not load configuration"
            );
        }
    }

    #[test]
    fn other_commands_need_the_environment() {
        let launch = cli::parse(vec![std::ffi::OsString::from("doctor")], false).expect("parse");
        assert!(handle_runtime_free(&launch).is_none());
    }

    #[test]
    fn interrupt_maps_to_the_documented_exit_code() {
        let err = RuneError::new(ErrorCode::Cancelled, "stopped");
        assert_eq!(exit_code_for(&err), EXIT_INTERRUPTED);
        let other = RuneError::new(ErrorCode::Internal, "bad");
        assert_eq!(exit_code_for(&other), EXIT_FAILURE);
    }

    #[test]
    fn unimplemented_surfaces_say_so_rather_than_failing_generically() {
        let err = not_yet_available("the agent runtime");
        assert_eq!(err.code(), ErrorCode::Unsupported);
        assert!(err.message().contains("agent runtime"));
    }
}
