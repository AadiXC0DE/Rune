//! The providers a user can connect, and what connecting one needs.
//!
//! One table rather than a list in the command and another in the credential
//! layer. A provider's endpoint, the variable its key is read from, and the
//! dialect that speaks to it are facts about the provider, so they are stated
//! once and both the connection command and the transport read them here.
//!
//! The table is what a fresh install can offer. A provider the user names
//! themselves is not in it, because an endpoint nobody has written down cannot
//! be guessed.

use rune_core::config::Provider;

/// How a connection to a provider is authenticated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CredentialKind {
    /// A key the provider issues, pasted by the user.
    ApiKey,
}

/// One provider a user can connect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KnownProvider {
    /// Name the user types, which is also the configuration value.
    pub name: &'static str,
    /// One line describing what it is.
    pub summary: &'static str,
    /// Endpoint used when the user does not give one.
    ///
    /// `None` where a dialect is served by many hosts, so an endpoint has to be
    /// named rather than guessed.
    pub base_url: Option<&'static str>,
    /// Variable a credential is read from when the user does not store one.
    pub key_variable: Option<&'static str>,
    /// How a connection is authenticated.
    pub credential: CredentialKind,
    /// Whether the provider speaks a dialect this build implements.
    pub implemented: bool,
    /// Headers every request to this provider must carry.
    ///
    /// A gateway that routes by conversation needs one, and refuses a request
    /// without it rather than falling back to something slower, so it is part
    /// of connecting rather than an optimization.
    pub required_headers: &'static [(&'static str, &'static str)],
}

impl KnownProvider {
    /// Returns the provider identity this entry selects.
    #[must_use]
    pub fn provider(self) -> Provider {
        rune_core::config::parse_provider(self.name)
    }

    /// Returns how to name the credential when prompting for it.
    #[must_use]
    pub fn credential_label(self) -> &'static str {
        match self.credential {
            CredentialKind::ApiKey => "API key",
        }
    }
}

/// Providers a fresh install offers, in the order they are shown.
///
/// Ordered with the two that need nothing but a key first, then the generic
/// OpenAI-compatible dialect, which needs an endpoint, and finally the
/// providers that are recognized by name so a configuration written by hand
/// reads back correctly.
pub const KNOWN_PROVIDERS: &[KnownProvider] = &[
    KnownProvider {
        name: "anthropic",
        summary: "Anthropic Messages API",
        base_url: Some("https://api.anthropic.com"),
        key_variable: Some("ANTHROPIC_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
    },
    KnownProvider {
        name: "openai",
        summary: "OpenAI Responses API",
        base_url: Some("https://api.openai.com/v1"),
        key_variable: Some("OPENAI_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
    },
    KnownProvider {
        name: "opencode",
        summary: "OpenCode Zen, pay-per-use over the OpenAI-compatible route",
        base_url: Some("https://opencode.ai/zen/v1"),
        key_variable: Some("OPENCODE_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[("user-agent", USER_AGENT)],
    },
    KnownProvider {
        name: "opencode-go",
        summary: "OpenCode Go, a subscription over the OpenAI-compatible route",
        base_url: Some("https://opencode.ai/zen/go/v1"),
        key_variable: Some("OPENCODE_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        // Go routes by conversation and refuses a request without this rather
        // than degrading, so it is sent with every request rather than on the
        // ones that happen to carry a session.
        required_headers: &[("user-agent", USER_AGENT), ("x-opencode-session", "rune")],
    },
    KnownProvider {
        name: "chat_completions",
        summary: "Any OpenAI-compatible Chat Completions endpoint",
        base_url: None,
        key_variable: Some("OPENAI_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
    },
];

/// Agent name sent when a provider asks its clients to identify themselves.
///
/// A provider that routes or rate-limits by client asks for a name of its own
/// rather than one shared with every HTTP library, so requests from this harness
/// are attributable to it.
pub const USER_AGENT: &str = concat!("rune/", env!("CARGO_PKG_VERSION"));

/// Returns the entry for a provider name as the user wrote it.
///
/// Matching is on the names a provider is known by, so `openai` and
/// `responses` both find the OpenAI entry.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static KnownProvider> {
    let wanted = name.trim().to_ascii_lowercase();
    KNOWN_PROVIDERS
        .iter()
        .find(|entry| entry.name == wanted || aliases(entry.name).contains(&wanted.as_str()))
}

/// Returns the other names a provider answers to.
///
/// Present because the provider identity accepts several spellings, and a user
/// who typed one of them should find the same entry rather than be told the
/// provider is unknown.
#[must_use]
pub fn aliases(name: &str) -> &'static [&'static str] {
    match name {
        "openai" => &["responses"],
        "chat_completions" => &["chat-completions"],
        "opencode" => &["zen", "opencode-zen"],
        "opencode-go" => &["go", "zen-go"],
        _ => &[],
    }
}

/// Returns the endpoint of a known provider, when it has one.
#[must_use]
pub fn base_url(name: &str) -> Option<&'static str> {
    lookup(name).and_then(|entry| entry.base_url)
}

/// Returns the variable a credential for a provider is read from.
///
/// Falls back to the credential layer for a provider that is not in the table,
/// so a name the user made up still reads a variable if one is configured for
/// it.
#[must_use]
pub fn key_variable(name: &str) -> Option<&'static str> {
    lookup(name)
        .and_then(|entry| entry.key_variable)
        .or_else(|| crate::auth::default_variables(name).first().copied())
}

/// Renders the providers a user can connect.
///
/// Each line names the provider and what it needs, so a user can choose without
/// reading documentation. The credential variable is shown because a user who
/// already has it exported can skip storing one.
#[must_use]
pub fn render_choices() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let width = KNOWN_PROVIDERS
        .iter()
        .map(|entry| entry.name.len())
        .max()
        .unwrap_or(0);
    for entry in KNOWN_PROVIDERS {
        let _ = writeln!(out, "  {:<width$}  {}", entry.name, entry.summary);
        let mut needs = Vec::new();
        if entry.base_url.is_none() {
            needs.push("an endpoint");
        }
        if let Some(variable) = entry.key_variable {
            needs.push(variable);
        }
        if !needs.is_empty() {
            let _ = writeln!(out, "  {:<width$}  needs {}", "", needs.join(", "));
        }
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_provider_is_unique_and_implemented() {
        let mut seen = std::collections::HashSet::new();
        for entry in KNOWN_PROVIDERS {
            assert!(seen.insert(entry.name), "duplicate {}", entry.name);
            assert!(
                entry.implemented,
                "{} is offered but not implemented",
                entry.name
            );
            assert!(
                !entry.summary.is_empty(),
                "{} has no summary to show",
                entry.name
            );
        }
    }

    #[test]
    fn a_provider_is_found_by_every_name_it_answers_to() {
        for entry in KNOWN_PROVIDERS {
            assert!(
                lookup(entry.name).is_some_and(|found| found.name == entry.name),
                "{} was not found by its own name",
                entry.name
            );
            for alias in aliases(entry.name) {
                assert!(
                    lookup(alias).is_some_and(|found| found.name == entry.name),
                    "`{alias}` did not find {}",
                    entry.name
                );
            }
        }
    }

    #[test]
    fn an_unknown_name_is_not_invented() {
        assert!(lookup("some-local-server").is_none());
        assert!(base_url("some-local-server").is_none());
    }

    #[test]
    fn a_provider_without_an_endpoint_says_so() {
        let entry = lookup("chat_completions").expect("known");
        assert!(
            entry.base_url.is_none(),
            "a compatible endpoint has no home"
        );
        let rendered = render_choices();
        assert!(
            rendered.contains("needs an endpoint"),
            "the choice does not say an endpoint is required: {rendered}"
        );
    }

    #[test]
    fn the_table_agrees_with_the_transport() {
        // The endpoint and the variable are read from here by the connection
        // command, so a disagreement with the credential layer would prompt for
        // a variable the transport never reads.
        assert_eq!(base_url("anthropic"), Some("https://api.anthropic.com"));
        assert_eq!(key_variable("anthropic"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(key_variable("openai"), Some("OPENAI_API_KEY"));
        assert_eq!(key_variable("chat_completions"), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn the_two_opencode_tiers_are_distinct_endpoints() {
        // One is a pay-per-use gateway and the other a subscription. They share
        // a key but not a base URL, so a connection that selected the wrong one
        // would reach a tier the user had not paid for.
        assert_eq!(base_url("opencode"), Some("https://opencode.ai/zen/v1"));
        assert_eq!(
            base_url("opencode-go"),
            Some("https://opencode.ai/zen/go/v1")
        );
        assert_ne!(base_url("opencode"), base_url("opencode-go"));
    }

    #[test]
    fn a_provider_that_requires_a_header_declares_both_parts() {
        for entry in KNOWN_PROVIDERS {
            for (name, value) in entry.required_headers {
                assert!(!name.is_empty(), "{} has an empty header name", entry.name);
                assert!(
                    !value.is_empty(),
                    "{} sends an empty value for {name}, which a gateway treats as absent",
                    entry.name
                );
            }
        }
    }

    #[test]
    fn the_subscription_tier_asks_to_be_routed_by_conversation() {
        let entry = lookup("opencode-go").expect("known");
        assert!(
            entry
                .required_headers
                .iter()
                .any(|(name, _)| *name == "x-opencode-session"),
            "the subscription tier refuses a request without a session header"
        );
    }

    #[test]
    fn a_client_identifies_itself_rather_than_looking_like_a_library() {
        assert!(USER_AGENT.starts_with("rune/"), "{USER_AGENT}");
        let entry = lookup("opencode").expect("known");
        assert!(
            entry
                .required_headers
                .iter()
                .any(|(name, _)| *name == "user-agent")
        );
    }

    #[test]
    fn every_choice_renders_one_line_per_provider() {
        let rendered = render_choices();
        for entry in KNOWN_PROVIDERS {
            assert!(
                rendered.contains(entry.name),
                "{} is missing from the choices",
                entry.name
            );
        }
    }
}
