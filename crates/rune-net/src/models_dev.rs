//! Model capacity read from the published models.dev catalog.
//!
//! An OpenAI-compatible listing carries an identifier and little else: the
//! opencode endpoint returns `id`, `object`, `created`, and `owned_by`, and no
//! capacity at all. Reading only that listing left every model budgeted against
//! the compiled default, so a model serving a million tokens reported as though
//! it held a hundred and twenty-eight thousand.
//!
//! models.dev is the catalog the compatible clients read for exactly this
//! reason. It is a single JSON document keyed by provider, and within each
//! provider keyed by model identifier, which is the same identifier the
//! endpoint lists. Only the capacity is taken from it: this is a description of
//! a model, not a source of endpoints or credentials, and nothing here is
//! trusted for anything but numbers.

use std::collections::BTreeMap;

/// Largest catalog document accepted.
///
/// The published document is a few megabytes. The ceiling is generous enough to
/// survive it growing and tight enough that a wrong address cannot stream
/// something unbounded into memory.
pub const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;

/// Capacity of one model, as the catalog describes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Input capacity in tokens.
    pub context: Option<u64>,
    /// Output ceiling in tokens.
    pub output: Option<u64>,
}

impl Limits {
    /// Returns whether nothing usable was described.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.context.is_none() && self.output.is_none()
    }
}

/// Capacity for every model of one provider.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct ProviderLimits {
    /// Model identifier to its capacity.
    pub models: BTreeMap<String, Limits>,
}

impl ProviderLimits {
    /// Returns the capacity described for a model, when there is one.
    #[must_use]
    pub fn get(&self, model: &str) -> Option<Limits> {
        self.models.get(model).copied().filter(|l| !l.is_empty())
    }

    /// Returns how many models were described.
    #[must_use]
    pub fn len(&self) -> usize {
        self.models.len()
    }

    /// Returns whether no model was described.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

/// Maps a provider name to its key in the catalog.
///
/// The keys are not always the name this program uses: a compatible endpoint is
/// unknown to the catalog, and a name that is absent yields nothing rather than
/// a guess.
#[must_use]
pub fn catalog_key(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("anthropic"),
        "openai" | "responses" => Some("openai"),
        "opencode" => Some("opencode"),
        "opencode-go" => Some("opencode-go"),
        "google" | "gemini" => Some("google"),
        "groq" => Some("groq"),
        "mistral" => Some("mistral"),
        "xai" => Some("xai"),
        "deepseek" => Some("deepseek"),
        // A self-hosted or unknown endpoint is not described by any published
        // catalog, so nothing is claimed for it.
        _ => None,
    }
}

/// Reads the capacity for one provider out of a catalog document.
///
/// Returns `None` when the document is not the shape this reads or the provider
/// is absent from it. A model the catalog does not describe is simply missing
/// from the result, which leaves the figure already in force rather than
/// inventing one.
#[must_use]
pub fn parse(body: &str, key: &str) -> Option<ProviderLimits> {
    let document: serde_json::Value = serde_json::from_str(body).ok()?;
    let provider = document.get(key)?;
    let models = provider.get("models")?.as_object()?;

    let mut limits = ProviderLimits::default();
    for (id, entry) in models {
        let limit = entry.get("limit");
        let context = limit
            .and_then(|l| l.get("context"))
            .and_then(serde_json::Value::as_u64)
            .filter(|value| *value > 0);
        let output = limit
            .and_then(|l| l.get("output"))
            .and_then(serde_json::Value::as_u64)
            .filter(|value| *value > 0);
        if context.is_none() && output.is_none() {
            continue;
        }
        limits.models.insert(id.clone(), Limits { context, output });
    }
    Some(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A document shaped like the published one, trimmed to what is read.
    const DOC: &str = r#"{
        "opencode-go": {
            "id": "opencode-go",
            "name": "OpenCode Go",
            "api": "https://opencode.ai/zen/go/v1",
            "env": ["OPENCODE_API_KEY"],
            "models": {
                "grok-4.7": { "id": "grok-4.7", "name": "Grok 4.7",
                              "limit": { "context": 500000, "output": 500000 } },
                "glm-5.3-flash": { "id": "glm-5.3-flash", "name": "GLM-5.3-Flash",
                                   "limit": { "context": 1000000, "output": 131072 } }
            }
        },
        "other": { "id": "other", "models": { "m": { "limit": { "context": 4096 } } } }
    }"#;

    #[test]
    fn the_capacity_of_a_listed_model_is_read() {
        let limits = parse(DOC, "opencode-go").expect("parsed");
        assert_eq!(limits.len(), 2);
        let grok = limits.get("grok-4.7").expect("grok");
        assert_eq!(grok.context, Some(500_000));
        assert_eq!(grok.output, Some(500_000));
        assert_eq!(
            limits.get("glm-5.3-flash").and_then(|l| l.context),
            Some(1_000_000)
        );
    }

    #[test]
    fn a_model_the_catalog_does_not_describe_yields_nothing() {
        // A figure that is absent must not become a number here, or a model the
        // catalog has never heard of would be budgeted against a guess.
        let limits = parse(DOC, "opencode-go").expect("parsed");
        assert_eq!(limits.get("never-heard-of-it"), None);
    }

    #[test]
    fn a_provider_that_is_absent_is_not_a_failure() {
        assert!(parse(DOC, "not-a-provider").is_none());
    }

    #[test]
    fn a_body_that_is_not_the_catalog_is_refused() {
        assert!(parse("<html>nope</html>", "opencode-go").is_none());
        assert!(parse("{}", "opencode-go").is_none());
        assert!(parse("", "opencode-go").is_none());
        // The right provider key with no model table is not usable either.
        assert!(parse(r#"{"p":{}}"#, "p").is_none());
        assert!(parse(r#"{"p":{"models":[]}}"#, "p").is_none());
    }

    #[test]
    fn an_entry_with_no_capacity_is_left_out() {
        let body = r#"{"p":{"models":{"a":{"limit":{}},"b":{"limit":{"context":0}},
                                       "c":{"id":"c","limit":{"context":8192}}}}}"#;
        let limits = parse(body, "p").expect("parsed");
        assert_eq!(limits.len(), 1);
        assert_eq!(limits.get("c").and_then(|l| l.context), Some(8192));
    }

    #[test]
    fn a_zero_capacity_is_treated_as_absent() {
        // A window of nothing would make every request look oversized.
        let body = r#"{"p":{"models":{"a":{"limit":{"context":0,"output":0}}}}}"#;
        let limits = parse(body, "p").expect("parsed");
        assert!(limits.is_empty());
    }

    #[test]
    fn only_a_context_or_only_an_output_is_kept() {
        let body = r#"{"p":{"models":{"a":{"limit":{"context":4096}},
                                       "b":{"limit":{"output":512}}}}}"#;
        let limits = parse(body, "p").expect("parsed");
        assert_eq!(limits.get("a").and_then(|l| l.context), Some(4096));
        assert_eq!(limits.get("a").and_then(|l| l.output), None);
        assert_eq!(limits.get("b").and_then(|l| l.output), Some(512));
        assert_eq!(limits.get("b").and_then(|l| l.context), None);
    }

    #[test]
    fn the_keys_this_build_knows_are_the_ones_the_catalog_publishes() {
        for (ours, theirs) in [
            ("anthropic", "anthropic"),
            ("openai", "openai"),
            ("responses", "openai"),
            ("opencode", "opencode"),
            ("opencode-go", "opencode-go"),
        ] {
            assert_eq!(catalog_key(ours), Some(theirs), "{ours}");
        }
        // A compatible endpoint is not described by any published catalog.
        assert_eq!(catalog_key("chat_completions"), None);
        assert_eq!(catalog_key("my-self-hosted"), None);
    }
}
