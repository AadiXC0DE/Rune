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
    /// An explicitly selected local provider, without a credential.
    AnonymousLoopback,
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
    /// Small model the provider publishes, used to review actions in `auto`
    /// mode when requests go to `base_url` itself.
    ///
    /// `None` where the endpoint serves models nobody can name in advance, so
    /// the session model reviews instead.
    pub review_model: Option<&'static str>,
    /// Key of this provider in the published models.dev catalog.
    ///
    /// `None` where the endpoint serves whatever its operator installed, so no
    /// published catalog describes it.
    pub models_dev_key: Option<&'static str>,
    /// Group the provider is listed under.
    pub group: ProviderGroup,
}

/// Where a provider's models are served from, which is how the choices are
/// grouped when they are listed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProviderGroup {
    /// The vendor that trains the models serves them.
    Direct,
    /// One key reaches models from many vendors.
    Gateway,
    /// Any endpoint that speaks a dialect, named by the user.
    Compatible,
    /// A model server on this machine.
    Local,
}

impl ProviderGroup {
    /// Returns the heading the group is listed under.
    #[must_use]
    pub const fn heading(self) -> &'static str {
        match self {
            Self::Direct => "Direct from the model vendor",
            Self::Gateway => "Gateways",
            Self::Compatible => "Any compatible endpoint",
            Self::Local => "Local servers",
        }
    }
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
            CredentialKind::AnonymousLoopback => "no credential (loopback only)",
        }
    }
}

/// Providers a fresh install offers, in the order they are shown.
///
/// The position in this table is the number a choice is printed with, and the
/// entries of a group are contiguous: vendors serving their own models first,
/// then gateways, then the generic OpenAI-compatible dialect, which needs an
/// endpoint, and finally local servers, which use anonymous loopback mode.
pub const KNOWN_PROVIDERS: &[KnownProvider] = &[
    KnownProvider {
        name: "anthropic",
        summary: "Anthropic Messages API",
        base_url: Some("https://api.anthropic.com"),
        key_variable: Some("ANTHROPIC_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: Some("claude-haiku-5-5"),
        models_dev_key: Some("anthropic"),
        group: ProviderGroup::Direct,
    },
    KnownProvider {
        name: "openai",
        summary: "OpenAI Responses API",
        base_url: Some("https://api.openai.com/v1"),
        key_variable: Some("OPENAI_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: Some("gpt-6-luna"),
        models_dev_key: Some("openai"),
        group: ProviderGroup::Direct,
    },
    KnownProvider {
        name: "gemini",
        summary: "Google Gemini Generate Content API",
        base_url: Some("https://generativelanguage.googleapis.com/v1beta"),
        key_variable: Some("GEMINI_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: Some("gemini-3.5-flash-lite"),
        models_dev_key: Some("google"),
        group: ProviderGroup::Direct,
    },
    KnownProvider {
        name: "deepseek",
        summary: "DeepSeek API, OpenAI-compatible",
        base_url: Some("https://api.deepseek.com"),
        key_variable: Some("DEEPSEEK_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: Some("deepseek-flash"),
        models_dev_key: Some("deepseek"),
        group: ProviderGroup::Direct,
    },
    KnownProvider {
        name: "zai",
        summary: "Z.ai GLM models, OpenAI-compatible",
        base_url: Some("https://api.z.ai/api/paas/v4"),
        key_variable: Some("ZAI_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: None,
        models_dev_key: Some("zai"),
        group: ProviderGroup::Direct,
    },
    KnownProvider {
        name: "moonshot",
        summary: "Moonshot Kimi models, OpenAI-compatible",
        base_url: Some("https://api.moonshot.ai/v1"),
        key_variable: Some("MOONSHOT_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: None,
        models_dev_key: Some("moonshotai"),
        group: ProviderGroup::Direct,
    },
    KnownProvider {
        name: "minimax",
        summary: "MiniMax models, OpenAI-compatible",
        base_url: Some("https://api.minimax.io/v1"),
        key_variable: Some("MINIMAX_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: None,
        models_dev_key: Some("minimax"),
        group: ProviderGroup::Direct,
    },
    KnownProvider {
        name: "xai",
        summary: "xAI Grok models, OpenAI-compatible",
        base_url: Some("https://api.x.ai/v1"),
        key_variable: Some("XAI_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: None,
        models_dev_key: Some("xai"),
        group: ProviderGroup::Direct,
    },
    KnownProvider {
        name: "groq",
        summary: "Groq hosted open models, OpenAI-compatible",
        base_url: Some("https://api.groq.com/openai/v1"),
        key_variable: Some("GROQ_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: None,
        models_dev_key: Some("groq"),
        group: ProviderGroup::Direct,
    },
    KnownProvider {
        name: "mistral",
        summary: "Mistral models, OpenAI-compatible",
        base_url: Some("https://api.mistral.ai/v1"),
        key_variable: Some("MISTRAL_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: None,
        models_dev_key: Some("mistral"),
        group: ProviderGroup::Direct,
    },
    KnownProvider {
        name: "openrouter",
        summary: "OpenRouter, many vendors behind one key",
        base_url: Some("https://openrouter.ai/api/v1"),
        key_variable: Some("OPENROUTER_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: None,
        models_dev_key: Some("openrouter"),
        group: ProviderGroup::Gateway,
    },
    KnownProvider {
        name: "vercel",
        summary: "Vercel AI Gateway, OpenAI-compatible route",
        base_url: Some("https://ai-gateway.vercel.sh/v1"),
        key_variable: Some("AI_GATEWAY_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: None,
        models_dev_key: Some("vercel"),
        group: ProviderGroup::Gateway,
    },
    KnownProvider {
        name: "opencode",
        summary: "OpenCode Zen, pay-per-use over the OpenAI-compatible route",
        base_url: Some("https://opencode.ai/zen/v1"),
        key_variable: Some("OPENCODE_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[("user-agent", USER_AGENT)],
        review_model: None,
        models_dev_key: Some("opencode"),
        group: ProviderGroup::Gateway,
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
        review_model: None,
        models_dev_key: Some("opencode-go"),
        group: ProviderGroup::Gateway,
    },
    KnownProvider {
        name: "chat_completions",
        summary: "Any OpenAI-compatible Chat Completions endpoint",
        base_url: None,
        key_variable: Some("OPENAI_API_KEY"),
        credential: CredentialKind::ApiKey,
        implemented: true,
        required_headers: &[],
        review_model: None,
        models_dev_key: None,
        group: ProviderGroup::Compatible,
    },
    KnownProvider {
        name: "ollama",
        summary: "Local Ollama models, native discovery and Chat Completions requests",
        base_url: Some("http://127.0.0.1:11434/v1"),
        key_variable: None,
        credential: CredentialKind::AnonymousLoopback,
        implemented: true,
        required_headers: &[],
        review_model: None,
        models_dev_key: None,
        group: ProviderGroup::Local,
    },
    KnownProvider {
        name: "lmstudio",
        summary: "Local LM Studio server",
        base_url: Some("http://127.0.0.1:1234/v1"),
        key_variable: None,
        credential: CredentialKind::AnonymousLoopback,
        implemented: true,
        required_headers: &[],
        review_model: None,
        models_dev_key: None,
        group: ProviderGroup::Local,
    },
    KnownProvider {
        name: "llamacpp",
        summary: "Local llama.cpp server",
        base_url: Some("http://127.0.0.1:8080/v1"),
        key_variable: None,
        credential: CredentialKind::AnonymousLoopback,
        implemented: true,
        required_headers: &[],
        review_model: None,
        models_dev_key: None,
        group: ProviderGroup::Local,
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
        "gemini" => &["google"],
        "chat_completions" => &["chat-completions"],
        "opencode" => &["zen", "opencode-zen"],
        "opencode-go" => &["go", "zen-go"],
        "zai" => &["glm", "z.ai"],
        "moonshot" => &["kimi"],
        "xai" => &["grok"],
        "vercel" => &["ai-gateway"],
        "lmstudio" => &["lm-studio"],
        "llamacpp" => &["llama.cpp", "llama-server"],
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
/// already has it exported can skip storing one. Entries are listed under the
/// heading of their group and numbered by their position in the table, which
/// is the number a choice accepts.
#[must_use]
pub fn render_choices() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    // The generic entry's long name runs past the column rather than pushing
    // every other summary to the right.
    let width = KNOWN_PROVIDERS
        .iter()
        .filter(|entry| entry.group != ProviderGroup::Compatible)
        .map(|entry| entry.name.len())
        .max()
        .unwrap_or(0);
    let mut group = None;
    for (position, entry) in KNOWN_PROVIDERS.iter().enumerate() {
        if group != Some(entry.group) {
            group = Some(entry.group);
            let _ = writeln!(out, "{}", entry.group.heading());
        }
        let number = position.saturating_add(1);
        let _ = writeln!(
            out,
            "{number:>3}. {:<width$}  {}",
            entry.name, entry.summary
        );
        let second = if entry.group == ProviderGroup::Local {
            Some("no key needed (loopback only)".to_owned())
        } else {
            let mut needs = Vec::new();
            if entry.base_url.is_none() {
                needs.push("an endpoint");
            }
            if let Some(variable) = entry.key_variable {
                needs.push(variable);
            }
            (!needs.is_empty()).then(|| format!("needs {}", needs.join(", ")))
        };
        if let Some(second) = second {
            let _ = writeln!(out, "     {:<width$}  {second}", "");
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
    fn every_preset_has_a_catalog_key_or_says_none() {
        for entry in KNOWN_PROVIDERS {
            let undescribed =
                entry.name == "chat_completions" || entry.group == ProviderGroup::Local;
            assert_eq!(
                entry.models_dev_key.is_none(),
                undescribed,
                "{} has the wrong catalog key",
                entry.name
            );
        }
    }

    #[test]
    fn the_new_presets_reach_their_published_endpoints() {
        let expect = |name: &str, url: &str, variable: Option<&str>| {
            let entry = lookup(name).expect("known");
            assert_eq!(entry.base_url, Some(url), "{name}");
            assert_eq!(entry.key_variable, variable, "{name}");
            assert_eq!(
                crate::auth::default_variables(name).first().copied(),
                variable,
                "{name}"
            );
        };
        expect(
            "deepseek",
            "https://api.deepseek.com",
            Some("DEEPSEEK_API_KEY"),
        );
        expect("zai", "https://api.z.ai/api/paas/v4", Some("ZAI_API_KEY"));
        expect(
            "moonshot",
            "https://api.moonshot.ai/v1",
            Some("MOONSHOT_API_KEY"),
        );
        expect(
            "minimax",
            "https://api.minimax.io/v1",
            Some("MINIMAX_API_KEY"),
        );
        expect("xai", "https://api.x.ai/v1", Some("XAI_API_KEY"));
        expect(
            "groq",
            "https://api.groq.com/openai/v1",
            Some("GROQ_API_KEY"),
        );
        expect(
            "mistral",
            "https://api.mistral.ai/v1",
            Some("MISTRAL_API_KEY"),
        );
        expect(
            "openrouter",
            "https://openrouter.ai/api/v1",
            Some("OPENROUTER_API_KEY"),
        );
        expect(
            "vercel",
            "https://ai-gateway.vercel.sh/v1",
            Some("AI_GATEWAY_API_KEY"),
        );
        expect("lmstudio", "http://127.0.0.1:1234/v1", None);
        expect("llamacpp", "http://127.0.0.1:8080/v1", None);
        assert_eq!(
            crate::auth::default_variables("zai"),
            &["ZAI_API_KEY", "ZHIPU_API_KEY"]
        );
        for local in ["lmstudio", "llamacpp"] {
            let entry = lookup(local).expect("known");
            assert_eq!(entry.credential, CredentialKind::AnonymousLoopback);
        }
    }

    #[test]
    fn choices_are_grouped_and_numbered() {
        let rendered = render_choices();
        assert_eq!(KNOWN_PROVIDERS.len(), 18);
        let lines: Vec<&str> = rendered.lines().collect();
        let headings = [
            "Direct from the model vendor",
            "Gateways",
            "Any compatible endpoint",
            "Local servers",
        ];
        let at: Vec<usize> = headings
            .iter()
            .map(|heading| {
                lines
                    .iter()
                    .position(|line| line == heading)
                    .unwrap_or_else(|| panic!("no `{heading}` heading: {rendered}"))
            })
            .collect();
        assert!(at.windows(2).all(|pair| pair[0] < pair[1]), "{rendered}");

        assert!(
            rendered.contains("  1. anthropic    Anthropic Messages API\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("\n{}needs ANTHROPIC_API_KEY\n", " ".repeat(18))),
            "{rendered}"
        );
        assert!(rendered.contains("\n  4. deepseek "), "{rendered}");
        assert!(rendered.contains("\n 11. openrouter   "), "{rendered}");
        assert!(
            rendered.contains("\n 15. chat_completions  Any"),
            "{rendered}"
        );
        assert!(rendered.contains("\n 16. ollama       Local"), "{rendered}");
        assert!(rendered.contains("\n 18. llamacpp "), "{rendered}");

        for (position, entry) in KNOWN_PROVIDERS.iter().enumerate() {
            let prefix = format!("{:>3}. {} ", position.saturating_add(1), entry.name);
            let line = lines
                .iter()
                .position(|line| line.starts_with(&prefix))
                .unwrap_or_else(|| panic!("`{prefix}` is not listed: {rendered}"));
            let heading = at
                .iter()
                .rposition(|start| *start < line)
                .expect("under a heading");
            assert_eq!(
                headings[heading],
                entry.group.heading(),
                "{} is under the wrong heading",
                entry.name
            );
            if entry.group == ProviderGroup::Local {
                assert_eq!(
                    lines.get(line.saturating_add(1)).map(|next| next.trim()),
                    Some("no key needed (loopback only)"),
                    "{}",
                    entry.name
                );
            }
        }
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
