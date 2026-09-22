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
mod permissions;
mod provider_setup;
mod session;
mod session_log;
mod spec;
mod version;

use std::process::ExitCode;

use camino::Utf8PathBuf;
use rune_core::config::{self, EnvironmentOverrides, Layer, Provider, Settings};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;
use rune_session::report::{Period, render_text, summarize, to_json};
use rune_session::usage::{Ledger, now_ms};

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
        // Set on the settings rather than only recorded, so every endpoint a
        // command builds carries the refusal rather than just the label.
        settings.offline = true;
        settings.sources.record("offline", Layer::CommandLine);
    }

    match launch.command {
        Command::Doctor => run_doctor(&settings, &paths, &workspace, &output_flags),
        Command::Status => run_status(&settings, &paths, &workspace, &output_flags),
        Command::Limits => run_limits(&settings, &output_flags),
        Command::Config => run_config(&settings, &output_flags),
        Command::Prompt => run_prompt(&settings, &output_flags),
        Command::Sessions | Command::Tree => run_sessions(&paths, &output_flags),
        Command::Session => run_session(&paths, launch, &output_flags),
        Command::Usage => run_usage(&paths, launch, &output_flags),
        Command::Auth => run_auth(&settings, &paths, launch, &output_flags),
        Command::Connect => run_connect(&settings, &paths, launch, &output_flags),
        Command::Models => run_models(&settings, &output_flags),
        Command::Permissions => run_permissions(&settings, launch, &output_flags),
        Command::Workspace => run_workspace(&settings, &paths, launch, &output_flags),
        Command::Ask => run_ask(&settings, &paths, launch, &output_flags),
        Command::Acp => run_acp(&settings, &paths, &workspace, launch),
        Command::Interactive | Command::Resume => {
            run_interactive(&settings, &paths, &workspace, launch)
        }
        Command::Review => run_review(&settings, &paths, launch, &workspace, &output_flags),
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

/// Reviews the pending changes in the workspace.
///
/// Builds the prompt from the working tree rather than asking the model to go
/// looking, so what is reviewed is what the repository reports as pending and
/// nothing else.
fn run_review(
    settings: &Settings,
    paths: &Paths,
    launch: &Launch,
    workspace: &camino::Utf8Path,
    output: &OutputFlags,
) -> Result<ExitCode> {
    let changes = pending_changes(workspace)?;
    if changes.trim().is_empty() {
        // Reviewing nothing would produce an invented report, so the condition
        // is reported instead.
        if output.json {
            let value = serde_json::json!({
                "output": "",
                "exit_code": 0,
                "usage": serde_json::Value::Null,
                "note": "there are no pending changes to review",
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            println!("there are no pending changes to review");
        }
        return Ok(ExitCode::from(EXIT_OK));
    }

    let context = launch.args.join(" ");
    let mut prompt = String::from(
        "Review the pending changes below. Report defects, risky behavior, and \
         missing tests. Name each finding with the file it is in. Do not change \
         any file.\n\n",
    );
    if !context.trim().is_empty() {
        prompt.push_str("Additional context from the caller:\n");
        prompt.push_str(context.trim());
        prompt.push_str("\n\n");
    }
    prompt.push_str(&changes);

    let options = ask::Options {
        prompt,
        json: output.json,
        no_save: launch.has_flag("--no-save"),
        model: launch.flag("--model").map(str::to_owned),
        effort: launch.flag("--effort").map(str::to_owned),
    };

    match ask::run(settings, paths, &options) {
        Ok(result) => Ok(ExitCode::from(ask::report(&result, &options)?)),
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

/// Reads the pending changes from the working tree.
///
/// Uses `git diff` for tracked files and adds untracked paths by name, so a new
/// file is not silently absent from a review.
fn pending_changes(workspace: &camino::Utf8Path) -> Result<String> {
    let tracked = run_git(workspace, &["diff", "HEAD"])?;
    let untracked = run_git(workspace, &["ls-files", "--others", "--exclude-standard"])?;

    let mut out = tracked;
    let untracked = untracked.trim();
    if !untracked.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("Untracked files:\n");
        out.push_str(untracked);
        out.push('\n');
    }
    Ok(out)
}

/// Runs a git subcommand in the workspace.
///
/// A repository that is absent or has no commits yet yields no changes rather
/// than a failure, because there is nothing to review in either case.
fn run_git(workspace: &camino::Utf8Path, arguments: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(arguments)
        .current_dir(workspace)
        .output()
        .map_err(|err| {
            RuneError::new(
                ErrorCode::TransportFailure,
                format!("git could not be run: {err}"),
            )
            .with_hint("install git, or review the changes another way")
        })?;

    if !output.status.success() {
        // A directory that is not a repository, or one with no commits, has
        // nothing to compare against. Both are an absence of changes rather
        // than a failure, and neither should print git's usage text.
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        let nothing_to_compare = stderr.contains("not a git repository")
            || stderr.contains("unknown revision")
            || stderr.contains("does not have any commits");
        if nothing_to_compare {
            return Ok(String::new());
        }
        return Err(RuneError::new(
            ErrorCode::TransportFailure,
            format!("git reported: {}", first_line(&stderr)),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Returns the first non-empty line of a diagnostic.
///
/// A failing subcommand often prints its usage, and the first line is the part
/// that says what actually went wrong.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no diagnostic")
        .to_owned()
}

/// Runs the interactive session.
fn run_interactive(
    settings: &Settings,
    paths: &Paths,
    workspace: &camino::Utf8Path,
    launch: &Launch,
) -> Result<ExitCode> {
    let resume = match &launch.resume {
        Some(target) => Some(session_log::resolve_target(target, paths)?),
        None => None,
    };
    let config = session::prepare(settings, paths, workspace, resume)?;
    let stdin = std::io::stdin();
    let input = std::io::BufReader::new(stdin.lock());
    let code = session::run(config, input, std::io::stdout())?;
    Ok(ExitCode::from(code))
}

/// Reports the provider connection, or removes a stored credential.
fn run_auth(
    settings: &Settings,
    paths: &Paths,
    launch: &Launch,
    output: &OutputFlags,
) -> Result<ExitCode> {
    let action = launch.args.first().map(String::as_str);
    match action {
        Some("remove" | "logout") => {
            let provider = settings.provider.to_string();
            let removed = provider_setup::disconnect(paths, &provider)?;
            if output.json {
                let value = serde_json::json!({ "provider": provider, "removed": removed });
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else if removed {
                println!("removed the stored credential for {provider}");
            } else {
                println!("no credential was stored for {provider}");
            }
        }
        Some(other) => {
            return Err(RuneError::new(
                ErrorCode::InvalidField,
                format!("`{other}` is not an action for auth"),
            )
            .with_hint("run `rune auth` to inspect, or `rune auth remove` to clear"));
        }
        None => {
            if output.json {
                let value = serde_json::json!({
                    "provider": settings.provider.to_string(),
                    "model": settings.model,
                    "base_url": settings.base_url,
                });
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!("{}", provider_setup::render_connection(settings, paths));
            }
        }
    }
    Ok(ExitCode::from(EXIT_OK))
}

/// Stores a credential for the configured provider.
///
/// The provider is the positional argument. The credential comes from the
/// environment when it is already exported, and from standard input otherwise,
/// so it never has to appear in a shell history or a process listing.
fn run_connect(
    settings: &Settings,
    paths: &Paths,
    launch: &Launch,
    output: &OutputFlags,
) -> Result<ExitCode> {
    let name = launch.args.first().map_or_else(
        || settings.provider.to_string(),
        |value| value.trim().to_owned(),
    );
    if name.is_empty() || name == "unconfigured" {
        return Err(
            RuneError::new(ErrorCode::InvalidConfiguration, "no provider was named")
                .with_hint("name a provider, as in `rune connect anthropic`"),
        );
    }

    let parsed = config::parse_provider(&name);
    let base_url = provider_setup::resolve_endpoint(&parsed, settings.base_url.as_deref())?;

    let from_environment =
        provider_setup::environment_credential(&name, settings.api_key_env.as_deref());
    match from_environment {
        Some(value) => provider_setup::connect(paths, &name, &value)?,
        // Only prompt when no answer can be waiting: a machine caller supplies
        // the variable rather than blocking on a terminal that will not answer.
        None if output.json => {}
        None => {
            let value = ask::read_stdin_prompt()?;
            provider_setup::connect(paths, &name, &value)?;
        }
    }

    let selection = provider_setup::Selection {
        provider: name.clone(),
        model: (!settings.model.trim().is_empty()).then(|| settings.model.clone()),
        base_url: Some(base_url),
    };
    provider_setup::save_selection(paths, &selection)?;

    if output.json {
        let value = serde_json::json!({
            "provider": selection.provider,
            "model": selection.model,
            "base_url": selection.base_url,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("connected {name}");
    }
    Ok(ExitCode::from(EXIT_OK))
}

/// Lists the models the configured provider offers.
fn run_models(settings: &Settings, output: &OutputFlags) -> Result<ExitCode> {
    let catalog = provider_setup::catalog_for(settings);
    if output.json {
        println!("{}", serde_json::to_string_pretty(&catalog.to_json())?);
    } else {
        println!("{}", provider_setup::render_catalog(&catalog));
    }
    Ok(ExitCode::from(EXIT_OK))
}

/// Reads a reporting period from its written name.
fn period_from(raw: Option<&str>) -> Result<Period> {
    match raw {
        None | Some("24h") => Ok(Period::Last24Hours),
        Some("7d") => Ok(Period::Last7Days),
        Some("30d") => Ok(Period::Last30Days),
        Some(other) => Err(RuneError::new(
            ErrorCode::InvalidField,
            format!("`{other}` is not a period"),
        )
        .with_hint("use 24h, 7d, or 30d")),
    }
}

/// Reports one stored session.
fn run_session(paths: &Paths, launch: &Launch, output: &OutputFlags) -> Result<ExitCode> {
    let raw = launch.args.first().ok_or_else(|| {
        RuneError::missing_field("session").with_hint("name a session, or run `rune sessions`")
    })?;
    let id: rune_core::id::SessionId = raw.parse()?;
    let state = session_log::inspect(paths, &id)?;

    if output.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&session_log::detail_json(&state))?
        );
    } else {
        println!(
            "{}",
            session_log::render_detail(&state, &paths.session_dir(&id))
        );
    }
    Ok(ExitCode::from(EXIT_OK))
}

/// Reports token usage over a period.
fn run_usage(paths: &Paths, launch: &Launch, output: &OutputFlags) -> Result<ExitCode> {
    let period = period_from(launch.args.first().map(String::as_str))?;

    let ledger = Ledger::from_paths(paths);
    let read = ledger.read()?;
    let summary = summarize(&read.records, period, now_ms());

    if output.json {
        println!("{}", serde_json::to_string_pretty(&to_json(&summary))?);
    } else {
        println!("{}", render_text(&summary));
    }
    Ok(ExitCode::from(EXIT_OK))
}

/// Reports the permission rules in force.
fn run_permissions(settings: &Settings, launch: &Launch, output: &OutputFlags) -> Result<ExitCode> {
    let rules = permissions::validated(settings)?;

    // An action given as a positional argument is explained rather than listed,
    // because that is the question a user actually has.
    if let Some(action) = launch.args.first() {
        let text = permissions::explain(
            &rules,
            settings.permission_mode,
            action,
            launch.args.get(1).map_or("", String::as_str),
        );
        println!("{text}");
        return Ok(ExitCode::from(EXIT_OK));
    }

    if output.json {
        let value = permissions::to_json(&rules, settings.permission_mode);
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", permissions::render(&rules, settings.permission_mode));
    }
    Ok(ExitCode::from(EXIT_OK))
}

/// Lists the sessions stored for this workspace.
fn run_sessions(paths: &Paths, output: &OutputFlags) -> Result<ExitCode> {
    let rows = session_log::list(paths);
    if output.json {
        let value = serde_json::Value::Array(
            rows.iter()
                .map(|row| {
                    serde_json::json!({
                        "id": row.id,
                        "turns": row.turns,
                        "events": row.events,
                        "updated_at": row.updated_at,
                        "title": row.title,
                    })
                })
                .collect(),
        );
        let rendered = serde_json::to_string_pretty(&value)?;
        println!("{rendered}");
    } else {
        println!("{}", session_log::render_listing(&rows));
    }
    Ok(ExitCode::from(EXIT_OK))
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
        endpoint: rune_net::transport::Endpoint::new(base_url, credential.expose().to_owned())
            .offline(settings.offline),
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
        additional_roots: settings.additional_directories.clone(),
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
fn run_workspace(
    settings: &Settings,
    paths: &Paths,
    launch: &Launch,
    output: &OutputFlags,
) -> Result<ExitCode> {
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
        "add" | "remove" | "clear" => run_workspace_edit(action, launch, paths, output),
        other => Err(RuneError::invalid_field(
            "workspace",
            format!("`{other}` is not a subcommand"),
        )
        .with_hint("use list, add, remove, or clear")),
    }
}

/// Adds, removes, or clears the additional directories in the user config.
///
/// The list is written whole rather than edited in place, because a partial
/// update of a list would have to express which entry moved, and an entry is
/// identified by its path.
fn run_workspace_edit(
    action: &str,
    launch: &Launch,
    paths: &Paths,
    output: &OutputFlags,
) -> Result<ExitCode> {
    let stored = config::load(
        None,
        Some(&paths.config_file(None)),
        &EnvironmentOverrides::default(),
    );
    let mut directories: Vec<String> = stored
        .additional_directories
        .iter()
        .map(ToString::to_string)
        .collect();

    match action {
        "add" => {
            let Some(raw) = launch.args.get(1) else {
                return Err(
                    RuneError::missing_field("directory").with_hint("name the directory to add")
                );
            };
            let resolved = resolve_directory(raw)?;
            if directories.contains(&resolved) {
                // Adding what is already there is not an error; reporting it as
                // one would make a repeated command look broken.
                if !output.json {
                    println!("{resolved} is already listed");
                }
            } else {
                directories.push(resolved.clone());
                directories.sort();
                write_directories(paths, &directories)?;
                if !output.json {
                    println!("added {resolved}");
                }
            }
        }
        "remove" => {
            let Some(raw) = launch.args.get(1) else {
                return Err(
                    RuneError::missing_field("directory").with_hint("name the directory to remove")
                );
            };
            let before = directories.len();
            directories.retain(|entry| entry != raw);
            if directories.len() == before {
                return Err(
                    RuneError::new(ErrorCode::NotFound, format!("`{raw}` is not listed"))
                        .with_hint("run `rune workspace list` to see what is"),
                );
            }
            write_directories(paths, &directories)?;
            if !output.json {
                println!("removed {raw}");
            }
        }
        _ => {
            write_directories(paths, &Vec::new())?;
            if !output.json {
                println!("cleared the additional directories");
            }
        }
    }

    if output.json {
        let value = serde_json::json!({ "directories": directories });
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(ExitCode::from(EXIT_OK))
}

/// Resolves a directory argument to the path that will be stored.
///
/// The result is canonical, which resolves `..` and any symbolic link. Without
/// it the same directory can be listed twice under two spellings, and a check
/// for whether a directory is already present would miss.
fn resolve_directory(raw: &str) -> Result<String> {
    let expanded = Utf8PathBuf::from(expand_tilde(raw));
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        current_workspace()?.join(expanded)
    };

    if !absolute.is_dir() {
        return Err(RuneError::new(
            ErrorCode::NotFound,
            format!("`{absolute}` is not a directory"),
        )
        .with_hint("name a directory that exists"));
    }

    let canonical = absolute.canonicalize_utf8().map_err(|err| {
        RuneError::new(
            ErrorCode::NotFound,
            format!("`{absolute}` could not be resolved: {err}"),
        )
    })?;
    Ok(canonical.to_string())
}

/// Expands a leading tilde to the home directory.
fn expand_tilde(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix('~') else {
        return raw.to_owned();
    };
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return raw.to_owned();
    }
    format!("{home}{rest}")
}

/// Writes the additional-directory list into the user config.
fn write_directories(paths: &Paths, directories: &[String]) -> Result<()> {
    let value = if directories.is_empty() {
        None
    } else {
        Some(toml::Value::Array(
            directories
                .iter()
                .map(|entry| toml::Value::String(entry.clone()))
                .collect(),
        ))
    };
    provider_setup::save_key(paths, "additional_directories", value)
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
    fn pending_changes_are_empty_outside_a_repository() {
        // Nothing to compare against is not an error; it is nothing to review.
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let changes = pending_changes(root).expect("read");
        assert!(changes.trim().is_empty(), "{changes}");
    }

    #[test]
    fn pending_changes_include_a_modified_and_an_untracked_file() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let ok = |arguments: &[&str]| {
            std::process::Command::new("git")
                .args(arguments)
                .current_dir(root)
                .output()
                .expect("git")
        };
        ok(&["init", "-q", "."]);
        std::fs::write(root.join("tracked.txt"), "first\n").expect("write");
        ok(&["add", "tracked.txt"]);
        ok(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "init",
        ]);

        std::fs::write(root.join("tracked.txt"), "first\nsecond\n").expect("write");
        std::fs::write(root.join("untracked.txt"), "new\n").expect("write");

        let changes = pending_changes(root).expect("read");
        assert!(changes.contains("+second"), "{changes}");
        assert!(changes.contains("untracked.txt"), "{changes}");
    }

    #[test]
    fn a_repository_with_no_commits_yields_no_changes() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        std::process::Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(root)
            .output()
            .expect("git");
        // `diff HEAD` fails without a commit, and reporting failure would make
        // a fresh repository look broken.
        let changes = pending_changes(root).expect("read");
        assert!(changes.trim().is_empty(), "{changes}");
    }

    #[test]
    fn a_tilde_is_expanded_against_the_home_directory() {
        let expanded = expand_tilde("~/work");
        assert!(expanded.ends_with("/work"), "{expanded}");
        assert!(!expanded.starts_with('~'), "{expanded}");
    }

    #[test]
    fn a_path_without_a_tilde_is_left_alone() {
        assert_eq!(expand_tilde("/tmp/x"), "/tmp/x");
        assert_eq!(expand_tilde("../x"), "../x");
    }

    #[test]
    fn resolving_a_directory_that_does_not_exist_names_the_problem() {
        let err = resolve_directory("/rune-does-not-exist-1234").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.hint().is_some());
    }

    #[test]
    fn resolving_returns_a_canonical_path() {
        // Canonical is what makes two spellings of one directory compare equal,
        // which is what keeps a list from holding it twice.
        let resolved = resolve_directory(".").expect("resolved");
        assert!(Utf8PathBuf::from(&resolved).is_absolute(), "{resolved}");
        assert!(!resolved.contains("/./"), "{resolved}");
        assert!(!resolved.contains(".."), "{resolved}");
    }

    #[test]
    fn a_usage_period_is_recognized_by_name() {
        assert_eq!(period_from(Some("24h")).expect("24h"), Period::Last24Hours);
        assert_eq!(period_from(Some("7d")).expect("7d"), Period::Last7Days);
        assert_eq!(period_from(Some("30d")).expect("30d"), Period::Last30Days);
        assert_eq!(period_from(None).expect("default"), Period::Last24Hours);
    }

    #[test]
    fn an_unknown_usage_period_names_the_accepted_ones() {
        let err = period_from(Some("99y")).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.hint(), Some("use 24h, 7d, or 30d"));
    }

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
