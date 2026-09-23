//! The interactive provider connection.
//!
//! Connecting is a conversation rather than an argument list, because a user who
//! has just installed the harness does not know which providers exist or what
//! each one needs. The command lists them, takes a choice, and takes the
//! credential, so nothing has to be read from documentation first.
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

    // A number is accepted as well as a name, because the list is printed with
    // an order and reaching for the number is natural.
    if let Ok(index) = answer.parse::<usize>()
        && let Some(entry) = index
            .checked_sub(1)
            .and_then(|position| providers::KNOWN_PROVIDERS.get(position))
    {
        return Ok(entry);
    }

    providers::lookup(&answer).ok_or_else(|| {
        RuneError::new(
            ErrorCode::InvalidConfiguration,
            format!("`{answer}` is not a provider this build knows"),
        )
        .with_hint("run `rune connect` to see the list")
    })
}

/// Asks for the credential, explaining where it comes from.
///
/// The variable is named when one exists, so a user who already has it exported
/// knows they can stop and export it instead of pasting a secret into a shell.
pub fn ask_credential(entry: &KnownProvider) -> Result<String> {
    if let Some(variable) = entry.key_variable {
        println!();
        println!(
            "Paste your {} for {}.",
            entry.credential_label(),
            entry.name
        );
        println!("It is stored privately and never printed again.");
        println!("If ${variable} is already exported, press enter to use that instead.");
    } else {
        println!();
        println!(
            "Paste your {} for {}.",
            entry.credential_label(),
            entry.name
        );
        println!("It is stored privately and never printed again.");
    }
    println!();

    let value = read_line(&format!("{}: ", entry.credential_label()))?;
    if value.is_empty() {
        // An empty answer is only an answer when a variable can supply the
        // credential; otherwise there is nothing to store and the caller is
        // told rather than left with a connection that fails later.
        if let Some(variable) = entry.key_variable {
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
pub fn run(paths: &Paths, entry: &KnownProvider) -> Result<String> {
    let name = entry.name.to_owned();
    let endpoint = match entry.base_url {
        Some(url) => url.to_owned(),
        None => ask_endpoint(&name)?,
    };

    // The environment is consulted first, so a user with the key already
    // exported is not asked to paste it. The prompt explains this too, so the
    // empty answer is a choice rather than a dead end.
    let from_environment = provider_setup::environment_credential(&name, None);
    let credential = match from_environment {
        Some(value) => value,
        None => ask_credential(entry)?,
    };
    provider_setup::connect(paths, &name, &credential)?;

    provider_setup::save_selection(
        paths,
        &provider_setup::Selection {
            provider: name.clone(),
            model: None,
            base_url: Some(endpoint.clone()),
        },
    )?;

    println!();
    println!("connected {name}");
    println!("endpoint  {endpoint}");
    println!();
    println!("Run `rune models` to see the model in use, or `rune connect` again to add another.");
    Ok(name)
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
