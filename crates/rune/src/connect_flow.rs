//! The interactive provider connection.
//!
//! Connecting is a conversation rather than an argument list, because a user who
//! has just installed the harness does not know which providers exist or what
//! each one needs. The command lists them, takes a choice, and takes any
//! required credential, so nothing has to be read from documentation first.
//!
//! Every prompt reads standard input, so the whole flow can be driven by a pipe
//! or a test. Nothing here decides whether to be interactive: the caller does,
//! because a machine caller supplies its answer through the environment and must
//! never block on a terminal that will not answer.

use std::io::Write as _;

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;
use rune_net::providers::{self, KnownProvider};

use crate::provider_setup;

/// Reads one line from standard input, trimmed.
///
/// Returns an empty string at end of input rather than failing, so a caller can
/// treat a closed stream as no answer and say so in its own words.
fn read_line(prompt: &str) -> Result<String> {
    let mut out = std::io::stdout();
    let _ = write!(out, "{prompt}");
    let _ = out.flush();

    let mut line = String::new();
    let read =
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line).map_err(|err| {
            RuneError::new(ErrorCode::Internal, format!("could not read input: {err}"))
        })?;
    if read == 0 {
        return Ok(String::new());
    }
    Ok(line.trim().to_owned())
}

/// Asks which provider to connect, and returns the one chosen.
///
/// An empty answer at the end of input is an error rather than a default,
/// because connecting a provider the user did not choose would send their
/// credential somewhere they never named.
pub fn choose_provider() -> Result<&'static KnownProvider> {
    println!("Which provider would you like to connect?");
    println!();
    println!("{}", providers::render_choices());
    println!();

    let answer = read_line("provider: ")?;
    if answer.is_empty() {
        return Err(
            RuneError::new(ErrorCode::InvalidConfiguration, "no provider was chosen")
                .with_hint("name a provider, as in `rune connect anthropic`"),
        );
    }

    resolve_choice(&answer).ok_or_else(|| {
        RuneError::new(
            ErrorCode::InvalidConfiguration,
            format!("`{answer}` is not a provider this build knows"),
        )
        .with_hint("run `rune connect` to see the list")
    })
}

/// Returns the entry an answer names, by its printed number or by a name it
/// answers to.
///
/// A number is accepted as well as a name, because the list is printed with an
/// order and reaching for the number is natural.
#[must_use]
pub fn resolve_choice(answer: &str) -> Option<&'static KnownProvider> {
    let answer = answer.trim();
    if let Ok(index) = answer.parse::<usize>() {
        return index
            .checked_sub(1)
            .and_then(|position| providers::KNOWN_PROVIDERS.get(position));
    }
    providers::lookup(answer)
}

/// Returns the provider name a connection named on the command line writes.
///
/// A number or an alias of a preset becomes the preset's own name, so
/// `rune connect kimi` stores `moonshot`. A name that selects a dialect keeps
/// its spelling, because rewriting `responses` to the entry it is listed under
/// would select a different dialect, and a name the table does not know is
/// kept as typed.
#[must_use]
pub fn connection_name(named: &str) -> String {
    let named = named.trim();
    match resolve_choice(named) {
        Some(entry)
            if named.parse::<usize>().is_ok()
                || matches!(
                    rune_core::config::parse_provider(named),
                    rune_core::config::Provider::Named(_)
                ) =>
        {
            entry.name.to_owned()
        }
        _ => named.to_owned(),
    }
}

/// Asks for the credential of a provider, explaining where it comes from.
///
/// Only asked when the environment holds no credential, so the variable is
/// named as the other way to connect rather than as something to fall back on.
/// A provider the table does not describe is asked for an API key.
pub fn ask_credential(name: &str) -> Result<String> {
    let entry = providers::lookup(name);
    let label = entry.map_or("API key", |entry| entry.credential_label());
    let variable = entry.and_then(|entry| entry.key_variable);

    println!();
    println!("Paste your {label} for {name}.");
    println!("It is stored privately and never printed again.");
    if let Some(variable) = variable {
        println!("To read it from ${variable} instead, export it and run this again.");
    }
    println!();

    let value = read_credential(&format!("{label}: "))?;
    if value.is_empty() {
        // An empty answer is only an answer when a variable can supply the
        // credential; otherwise there is nothing to store and the caller is
        // told rather than left with a connection that fails later.
        if let Some(variable) = variable {
            return Err(RuneError::new(
                ErrorCode::AuthenticationRequired,
                format!("no credential was given, and ${variable} is not set in this shell"),
            )
            .with_hint(format!(
                "export {variable}, or run this again and paste the key"
            )));
        }
        return Err(
            RuneError::new(ErrorCode::AuthenticationRequired, "no credential was given")
                .with_hint("run this again and paste the key"),
        );
    }
    Ok(value)
}

/// Reads a credential after a prompt, without echo when a terminal is attached.
///
/// An echoed key stays on screen and in the terminal's scrollback. Piped input
/// is read as one line, as every other answer in the flow is.
fn read_credential(prompt: &str) -> Result<String> {
    use std::io::IsTerminal as _;

    if !std::io::stdin().is_terminal() {
        return read_line(prompt);
    }
    let read = rune_term::input::read_secret(prompt);
    // Enter was not echoed either, so the line the prompt is on is ended here.
    println!();
    match read {
        Ok(value) => Ok(value.unwrap_or_default().trim().to_owned()),
        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => Err(RuneError::new(
            ErrorCode::Cancelled,
            "the connection was cancelled",
        )),
        Err(err) => Err(RuneError::new(
            ErrorCode::Internal,
            format!("could not read input: {err}"),
        )),
    }
}

/// Asks for the endpoint a provider needs when it has no default.
///
/// Takes the name rather than a table entry, because a provider the user named
/// themselves has no entry and still needs an endpoint.
pub fn ask_endpoint(name: &str) -> Result<String> {
    println!();
    println!("{name} is served by many hosts, so give the endpoint to use.");
    println!("Include the version path, as in `https://host/v1`.");
    println!();

    let value = read_line("endpoint: ")?;
    if value.is_empty() {
        return Err(
            RuneError::new(ErrorCode::InvalidConfiguration, "no endpoint was given")
                .with_hint("give the base URL of the endpoint, or set `base_url` in the config"),
        );
    }
    // Validated before anything is stored, so a typo is refused while the user
    // is still looking at the prompt rather than at the first request.
    rune_net::transport::validate_url(&value)?;
    Ok(value)
}

/// Connects a provider in one flow and reports what was written.
///
/// Returns the provider name that was connected, so the caller can report it
/// without deciding again which one that was.
pub fn run(
    paths: &Paths,
    settings: &rune_core::config::Settings,
    entry: &KnownProvider,
) -> Result<String> {
    let name = entry.name.to_owned();
    let endpoint = match entry.base_url {
        Some(url) => url.to_owned(),
        None => ask_endpoint(&name)?,
    };

    if entry.credential == providers::CredentialKind::AnonymousLoopback {
        rune_net::transport::Endpoint::anonymous(&endpoint)?;
    } else {
        // An exported key avoids another credential prompt.
        let credential = match provider_setup::environment_credential(&name, None) {
            Some(value) => value,
            None => ask_credential(&name)?,
        };
        provider_setup::connect(paths, &name, &credential)?;
    }

    let mut selection = provider_setup::Selection {
        provider: name.clone(),
        model: settings
            .configured_models
            .get(&name)
            .or_else(|| settings.configured_models.get("default"))
            .map(|model| model.id().to_owned()),
        base_url: Some(endpoint.clone()),
    };
    offer_model(paths, settings, &mut selection)?;
    provider_setup::save_selection(paths, &selection)?;

    println!();
    println!("connected {name}");
    println!("endpoint  {endpoint}");
    if let Some(model) = &selection.model {
        println!("model     {model}");
    } else {
        println!("{}", model_next_step(&name));
    }
    Ok(name)
}

/// Gives a complete, valid command when a connection has no model yet.
pub fn model_next_step(provider: &str) -> String {
    format!(
        "No model is selected. Run `rune models` to list the endpoint's models, then `rune connect {provider} --model <id>` to select one (stored in the `models` table)."
    )
}

/// Offers the endpoint's current models before saving an interactive connection.
pub fn offer_model(
    paths: &Paths,
    settings: &rune_core::config::Settings,
    selection: &mut provider_setup::Selection,
) -> Result<()> {
    if selection.model.is_some() {
        return Ok(());
    }
    let mut settings = settings.clone();
    settings.provider = rune_core::config::parse_provider(&selection.provider);
    settings.base_url.clone_from(&selection.base_url);
    settings.anonymous = providers::lookup(&selection.provider)
        .is_some_and(|entry| entry.credential == providers::CredentialKind::AnonymousLoopback);
    let catalog = match provider_setup::fetch_catalog(&settings, paths, crate::MODELS_TIMEOUT) {
        Ok(catalog) => catalog,
        Err(error) => {
            println!("Could not list the endpoint's models: {error}");
            return Ok(());
        }
    };
    if catalog.models.is_empty() {
        return Ok(());
    }
    println!("Choose a model (or press Enter to select it later):");
    for (index, model) in catalog.models.iter().enumerate() {
        println!("  {}  {}", index.saturating_add(1), model.id);
    }
    let answer = read_line("model: ")?;
    if answer.is_empty() {
        return Ok(());
    }
    let chosen = answer
        .parse::<usize>()
        .ok()
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| catalog.models.get(index))
        .or_else(|| catalog.models.iter().find(|model| model.id == answer))
        .ok_or_else(|| {
            RuneError::invalid_field("model", "choose a listed model by number or identifier")
        })?;
    selection.model = Some(chosen.id.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_listed_order_matches_the_numbering_offered() {
        // A number is accepted as a choice, so the position in the printed list
        // has to be the position in the table.
        let rendered = providers::render_choices();
        for (position, entry) in providers::KNOWN_PROVIDERS.iter().enumerate() {
            assert!(
                rendered.contains(entry.name),
                "{} is not in the list",
                entry.name
            );
            assert!(
                providers::KNOWN_PROVIDERS
                    .get(position)
                    .is_some_and(|found| found.name == entry.name),
                "position {position} does not hold {}",
                entry.name
            );
        }
    }

    #[test]
    fn a_number_selects_the_entry_printed_with_it() {
        let rendered = providers::render_choices();
        assert!(rendered.contains("\n  4. deepseek "), "{rendered}");
        assert_eq!(
            resolve_choice("4").map(|entry| entry.name),
            Some("deepseek")
        );
        assert_eq!(
            resolve_choice(" 18 ").map(|entry| entry.name),
            Some("llamacpp")
        );
        assert!(resolve_choice("0").is_none());
        assert!(resolve_choice("19").is_none());
        assert_eq!(connection_name("4"), "deepseek");
    }

    #[test]
    fn an_alias_connects_under_the_preset_name() {
        assert_eq!(connection_name("kimi"), "moonshot");
        assert_eq!(connection_name("llama.cpp"), "llamacpp");
        assert_eq!(connection_name("deepseek"), "deepseek");
        // A dialect name selects that dialect, so it is not rewritten to the
        // entry it is listed under.
        assert_eq!(connection_name("responses"), "responses");
        assert_eq!(connection_name("my-self-hosted"), "my-self-hosted");
    }

    #[test]
    fn a_provider_without_a_default_asks_for_an_endpoint() {
        let entry = providers::lookup("chat_completions").expect("known");
        assert!(entry.base_url.is_none());
        assert!(entry.key_variable.is_some());
    }

    #[test]
    fn a_provider_with_a_default_does_not_need_one() {
        let entry = providers::lookup("anthropic").expect("known");
        assert_eq!(entry.base_url, Some("https://api.anthropic.com"));
    }
}

/// The rows a first start shows before anything is asked.
pub const WELCOME: [&str; 3] = [
    "rune is a coding agent that works in this directory, in this terminal.",
    "it needs a model provider: choose one below, then paste its key.",
    "esc leaves without changing anything.",
];

/// The provider picker's items, in table order: the group label, then the
/// name and what it is.
#[must_use]
pub fn first_run_provider_items() -> Vec<String> {
    providers::KNOWN_PROVIDERS
        .iter()
        .filter(|entry| entry.implemented)
        .map(|entry| {
            let group = match entry.group {
                providers::ProviderGroup::Direct => "direct",
                providers::ProviderGroup::Gateway => "gateway",
                providers::ProviderGroup::Compatible => "compatible",
                providers::ProviderGroup::Local => "local",
            };
            format!("{group:<10}  {}  {}", entry.name, entry.summary)
        })
        .collect()
}

/// One step of the first-run screen: the frame to draw and the caret.
type Draw<'a> = dyn FnMut(&[String], (u16, u16)) -> Result<()> + 'a;

/// What a picker or field step ended with.
enum Step<T> {
    Chosen(T),
    Left,
}

/// Runs a filtered picker until Enter chooses an entry or Escape leaves.
fn pick_one(
    reader: &mut rune_term::input::KeyReader,
    draw: &mut Draw<'_>,
    title: &str,
    items: Vec<String>,
    above: &[String],
) -> Result<Step<usize>> {
    use rune_term::input::KeyAction;
    let (_, height) = rune_term::shell::terminal_size().unwrap_or((80, 24));
    let window = usize::from(height)
        .saturating_sub(above.len().saturating_add(4))
        .clamp(3, 12);
    let mut picker = rune_term::picker::Picker::new(title, items, window);
    let theme = rune_term::theme::Theme::no_color();
    let depth = rune_term::theme::ColorDepth::Ansi16;
    reader.clear();
    loop {
        let mut frame: Vec<String> = above.to_vec();
        frame.push(picker.title().to_owned());
        frame.push(format!("> {}", reader.line()));
        frame.extend(picker.rows(&theme, depth));
        let row = u16::try_from(above.len().saturating_add(1)).unwrap_or(u16::MAX);
        let column = u16::try_from(reader.column().saturating_add(2)).unwrap_or(u16::MAX);
        draw(&frame, (row, column))?;
        match reader.read_key() {
            KeyAction::Submit => {
                if picker.selected().is_some() {
                    return Ok(Step::Chosen(picker.cursor()));
                }
            }
            KeyAction::Interrupt | KeyAction::Cancel | KeyAction::Escape => return Ok(Step::Left),
            KeyAction::Up => {
                picker.up();
            }
            KeyAction::Down => {
                picker.down();
            }
            KeyAction::Suspend => rune_term::modes::suspend(),
            _ => picker.set_query(reader.line()),
        }
    }
}

/// Runs a one-line field until Enter gives a value `check` accepts or Escape
/// leaves; a refused value shows `check`'s message under the field.
fn field(
    reader: &mut rune_term::input::KeyReader,
    draw: &mut Draw<'_>,
    title: &str,
    above: &[String],
    check: &dyn Fn(&str) -> std::result::Result<String, String>,
) -> Result<Step<String>> {
    use rune_term::input::KeyAction;
    reader.clear();
    let mut problem: Option<String> = None;
    loop {
        let mut frame: Vec<String> = above.to_vec();
        frame.push(title.to_owned());
        frame.push(format!("> {}", reader.line()));
        frame.extend(problem.clone());
        let row = u16::try_from(above.len().saturating_add(1)).unwrap_or(u16::MAX);
        let column = u16::try_from(reader.column().saturating_add(2)).unwrap_or(u16::MAX);
        draw(&frame, (row, column))?;
        match reader.read_key() {
            KeyAction::Submit => match check(reader.line().trim()) {
                Ok(value) => return Ok(Step::Chosen(value)),
                Err(message) => problem = Some(message),
            },
            KeyAction::Interrupt | KeyAction::Cancel | KeyAction::Escape => return Ok(Step::Left),
            KeyAction::Suspend => rune_term::modes::suspend(),
            _ => {}
        }
    }
}

/// Connects a provider on the first start, by choosing in pickers: provider,
/// endpoint when the preset has none, the key (hidden), then a model from the
/// endpoint's live list. Finishes with a check of the connection: listing the
/// models with the new key. Returns `None` when the user left with Escape
/// before anything was saved.
///
/// # Errors
///
/// Fails when the terminal cannot be drawn on or the key cannot be stored.
pub fn run_interactive_first(
    paths: &Paths,
    settings: &rune_core::config::Settings,
) -> Result<Option<String>> {
    let (cols, rows) = rune_term::shell::terminal_size().unwrap_or((80, 24));
    let mut inline = rune_term::inline::Inline::new(cols);
    inline.set_max_rows(rows);
    let mut out = rune_term::inline::TerminalOutput;
    for line in WELCOME {
        println!("{line}");
    }
    println!();
    let mut reader = rune_term::input::KeyReader::new();
    let mut draw = |lines: &[String], caret: (u16, u16)| -> Result<()> {
        if let Some((cols, rows)) = rune_term::shell::terminal_size() {
            inline.set_width(cols);
            inline.set_max_rows(rows);
        }
        let bytes = inline.frame(&rune_term::inline::Frame {
            prompt: lines,
            caret,
            ..rune_term::inline::Frame::default()
        });
        out.write_all(&bytes)?;
        out.flush()?;
        Ok(())
    };
    let entries: Vec<&KnownProvider> = providers::KNOWN_PROVIDERS
        .iter()
        .filter(|entry| entry.implemented)
        .collect();
    let Step::Chosen(index) = pick_one(
        &mut reader,
        &mut draw,
        "choose a provider",
        first_run_provider_items(),
        &[],
    )?
    else {
        return Ok(None);
    };
    let Some(entry) = entries.get(index).copied() else {
        return Ok(None);
    };
    let name = entry.name.to_owned();
    let endpoint = if let Some(url) = entry.base_url {
        url.to_owned()
    } else {
        let check = |value: &str| {
            rune_net::transport::validate_url(value)
                .map(|()| value.trim_end_matches('/').to_owned())
                .map_err(|err| err.message().to_owned())
        };
        let above = [format!("provider  {name}")];
        match field(
            &mut reader,
            &mut draw,
            "endpoint (as in https://host/v1)",
            &above,
            &check,
        )? {
            Step::Chosen(url) => url,
            Step::Left => return Ok(None),
        }
    };
    // The key is read outside the drawn frame, with echo off.
    let _ = draw(
        &[format!("provider  {name}"), format!("endpoint  {endpoint}")],
        (0, 0),
    );
    let cleared = inline.clear();
    let _ = out.write_all(&cleared);
    let _ = out.flush();
    drop(reader);
    println!("provider  {name}");
    println!("endpoint  {endpoint}");
    if entry.credential != providers::CredentialKind::AnonymousLoopback {
        let credential = if let Some(value) = provider_setup::environment_credential(&name, None) {
            if let Some(variable) = entry.key_variable {
                println!("using {variable} from the environment");
            }
            value
        } else {
            let value = read_credential(&format!(
                "paste your {} for {name} (hidden): ",
                entry.credential_label()
            ))?;
            if value.is_empty() {
                return Err(RuneError::new(
                    ErrorCode::AuthenticationRequired,
                    "no credential was given",
                )
                .with_hint("run rune again and paste the key"));
            }
            value
        };
        provider_setup::connect(paths, &name, &credential)?;
    }

    // The model list comes from the endpoint with the new key, which is also
    // the check that the connection works.
    let mut probe = settings.clone();
    probe.provider = rune_core::config::parse_provider(&name);
    probe.base_url = Some(endpoint.clone());
    probe.api_key_env = None;
    probe.anonymous = entry.credential == providers::CredentialKind::AnonymousLoopback;
    println!("checking {name} by listing its models...");
    let catalog = provider_setup::fetch_catalog(&probe, paths, std::time::Duration::from_secs(5));
    let mut selection = provider_setup::Selection {
        provider: name.clone(),
        model: None,
        base_url: Some(endpoint),
    };
    let mut reader = rune_term::input::KeyReader::new();
    let (cols, rows) = rune_term::shell::terminal_size().unwrap_or((80, 24));
    let mut inline = rune_term::inline::Inline::new(cols);
    inline.set_max_rows(rows);
    let mut out = rune_term::inline::TerminalOutput;
    let mut draw = |lines: &[String], caret: (u16, u16)| -> Result<()> {
        let bytes = inline.frame(&rune_term::inline::Frame {
            prompt: lines,
            caret,
            ..rune_term::inline::Frame::default()
        });
        out.write_all(&bytes)?;
        out.flush()?;
        Ok(())
    };
    let report = match &catalog {
        Ok(catalog) => {
            let ids: Vec<String> = catalog
                .models
                .iter()
                .map(|model| model.id.clone())
                .collect();
            if !ids.is_empty()
                && let Step::Chosen(index) = pick_one(
                    &mut reader,
                    &mut draw,
                    &format!("choose a model from {name}"),
                    ids.clone(),
                    &[],
                )?
            {
                selection.model = ids.get(index).cloned();
            }
            None
        }
        Err(err) => {
            let above = [format!("could not list models: {}", err.message())];
            let check = |value: &str| {
                if value.is_empty() {
                    Err("type a model id".to_owned())
                } else {
                    Ok(value.to_owned())
                }
            };
            if let Step::Chosen(model) = field(&mut reader, &mut draw, "model id", &above, &check)?
            {
                selection.model = Some(model);
            }
            let hint = if err.code() == ErrorCode::AuthenticationRequired {
                REFUSED_KEY.to_owned()
            } else {
                err.hint()
                    .unwrap_or("check the endpoint, then run /connect")
                    .to_owned()
            };
            Some(format!("failed: {}\n{hint}", err.message()))
        }
    };
    let cleared = inline.clear();
    let _ = out.write_all(&cleared);
    let _ = out.flush();
    drop(reader);
    provider_setup::save_selection(paths, &selection)?;
    match &selection.model {
        Some(model) => {
            println!("model     {model}");
            println!("checking {name} with one short tool call...");
            let mut checked = probe;
            checked.model.clone_from(model);
            println!("{}", round_trip_row(&checked, paths, &name, model));
        }
        None => {
            if let Some(report) = report {
                println!("{report}");
            }
        }
    }
    Ok(Some(name))
}

/// What the session says when the endpoint refused the pasted key.
const REFUSED_KEY: &str = "the key was refused; run /connect in the session to paste another";

/// One short tool round trip with the new connection, as one row and a hint.
fn round_trip_row(
    settings: &rune_core::config::Settings,
    paths: &Paths,
    name: &str,
    model: &str,
) -> String {
    let workspace = std::env::current_dir()
        .ok()
        .and_then(|dir| camino::Utf8PathBuf::from_path_buf(dir).ok())
        .unwrap_or_else(|| camino::Utf8PathBuf::from("."));
    match crate::diagnostics::provider_check(settings, paths, &workspace) {
        Ok(report) => match report.first_failure() {
            None => format!("ok: {model} answered and called a tool"),
            Some(check) => {
                let hint = if check.name == "authenticated" {
                    REFUSED_KEY.to_owned()
                } else {
                    format!("rune doctor --provider {name} shows every check")
                };
                format!("failed: {}: {}\n{hint}", check.name, check.detail)
            }
        },
        Err(err) => format!("failed: {}", err.message()),
    }
}

#[cfg(test)]
mod first_run_tests {
    use super::*;

    #[test]
    fn first_run_provider_items_carry_group_labels() {
        let items = first_run_provider_items();
        assert!(
            items
                .iter()
                .any(|item| item.starts_with("direct      anthropic  ")),
            "{items:?}"
        );
        assert!(
            items
                .iter()
                .any(|item| item.starts_with("compatible  chat_completions  ")),
            "{items:?}"
        );
        let implemented = providers::KNOWN_PROVIDERS
            .iter()
            .filter(|entry| entry.implemented)
            .count();
        assert_eq!(items.len(), implemented);
        assert_eq!(WELCOME.len(), 3);
    }
}
