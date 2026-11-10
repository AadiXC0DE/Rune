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
    let mut out = std::io::stdout();
    let _ = write!(out, "{prompt}");
    let _ = out.flush();
    let read = rune_term::input::read_secret();
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
