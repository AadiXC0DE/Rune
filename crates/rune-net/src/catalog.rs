//! The model catalog.
//!
//! What a model can do determines what a request may contain: whether an image
//! can be attached, whether a reasoning effort is accepted, and how much input
//! fits. Capability is reported as unknown when the model is not in a loaded
//! catalog, and unknown is treated as the conservative answer rather than as
//! permission.

use rune_core::config::Effort;
use serde::{Deserialize, Serialize};

/// What a model supports.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Capabilities {
    /// The model accepts function tools.
    pub tools: bool,
    /// The model accepts images natively.
    pub vision: bool,
    /// The model accepts file input.
    pub file_input: bool,
    /// The model accepts a reasoning effort.
    pub reasoning: bool,
    /// The model supports a fast mode.
    pub fast_mode: bool,
    /// The endpoint reports prompt cache usage.
    pub cache_reporting: bool,
}

impl Capabilities {
    /// The answer used for a model that is not in a loaded catalog.
    ///
    /// Everything that changes request shape is false, because assuming a
    /// capability a model lacks produces a rejected request.
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            tools: true,
            vision: false,
            file_input: false,
            reasoning: false,
            fast_mode: false,
            cache_reporting: false,
        }
    }

    /// Returns true when an image can be sent inline rather than described.
    #[must_use]
    pub const fn native_vision(self) -> bool {
        self.vision && self.file_input
    }
}

/// One entry in the catalog.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ModelMetadata {
    /// Identifier sent to the endpoint.
    pub id: String,
    /// Name shown in a picker. Falls back to the identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Input capacity, when the catalog reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Output ceiling, when the catalog reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Reasoning efforts the model accepts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_efforts: Vec<String>,
    /// What the model supports.
    #[serde(default)]
    pub capabilities: Capabilities,
}

impl ModelMetadata {
    /// Builds an entry with capabilities that change nothing about a request.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            reasoning_efforts: Vec::new(),
            capabilities: Capabilities::unknown(),
        }
    }

    /// Returns the name to show for this model.
    #[must_use]
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id)
    }

    /// Returns true when the given effort is accepted.
    #[must_use]
    pub fn accepts_effort(&self, effort: Effort) -> bool {
        if !self.capabilities.reasoning {
            return false;
        }
        if effort == Effort::Auto {
            return true;
        }
        self.reasoning_efforts.is_empty()
            || self
                .reasoning_efforts
                .iter()
                .any(|name| name == effort.as_str())
    }

    /// Returns the effort to request, downgrading one the model does not accept.
    ///
    /// A model without reasoning support receives no effort at all, and a model
    /// that lists efforts receives the nearest one it does support rather than
    /// an error.
    #[must_use]
    pub fn resolve_effort(&self, requested: Effort) -> Option<Effort> {
        if requested == Effort::Auto || !self.capabilities.reasoning {
            return None;
        }
        if self.accepts_effort(requested) {
            return Some(requested);
        }
        // Pick the largest listed effort not exceeding the request.
        let order = [
            Effort::None,
            Effort::Minimal,
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::Xhigh,
            Effort::Max,
        ];
        let target = order.iter().position(|candidate| *candidate == requested)?;
        order
            .iter()
            .take(target.saturating_add(1))
            .rev()
            .find(|candidate| self.accepts_effort(**candidate))
            .copied()
    }

    /// Returns the input capacity, falling back to a conservative default.
    #[must_use]
    pub fn usable_context(&self) -> u64 {
        self.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW)
    }
}

/// Input capacity assumed when a catalog does not report one.
///
/// Re-exported from the configuration crate so the number a report prints and
/// the number a session budgets against cannot drift apart.
pub use rune_core::config::DEFAULT_CONTEXT_WINDOW;

/// A loaded catalog for one provider.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    /// Provider the catalog describes.
    pub provider: String,
    /// Entries, in the order the endpoint reported them.
    pub models: Vec<ModelMetadata>,
    /// Whether the entries came from the endpoint or a compiled fallback.
    pub source: CatalogSource,
}

/// Where a catalog came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    /// Fetched from the endpoint.
    #[default]
    Endpoint,
    /// The endpoint could not be reached, so a local table was used.
    LocalFallback,
    /// Derived from the models configured for a named connection.
    Configured,
}

impl Catalog {
    /// Builds an empty catalog.
    #[must_use]
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            models: Vec::new(),
            source: CatalogSource::Endpoint,
        }
    }

    /// Looks up a model by exact identifier.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ModelMetadata> {
        self.models.iter().find(|model| model.id == id)
    }

    /// Returns the metadata for a model, or a conservative placeholder.
    ///
    /// A model the catalog does not describe is still usable: the user may know
    /// about a model released after this build. It is treated as having only
    /// the capabilities that cannot break a request.
    #[must_use]
    pub fn metadata_or_default(&self, id: &str) -> ModelMetadata {
        self.get(id)
            .cloned()
            .unwrap_or_else(|| ModelMetadata::new(id))
    }

    /// Resolves a partial identifier to the entries it matches.
    ///
    /// Used by a picker, where an unambiguous prefix is a convenience and an
    /// ambiguous one must be reported rather than guessed.
    #[must_use]
    pub fn resolve(&self, query: &str) -> Vec<&ModelMetadata> {
        let query = query.trim().to_ascii_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        if let Some(exact) = self.models.iter().find(|model| model.id == query) {
            return vec![exact];
        }
        self.models
            .iter()
            .filter(|model| {
                let id = model.id.to_ascii_lowercase();
                let label = model.label().to_ascii_lowercase();
                id.contains(&query) || label.contains(&query)
            })
            .collect()
    }

    /// Returns the catalog as the value printed by `models --json`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "provider": self.provider,
            "source": self.source,
            "models": self.models,
        })
    }
}

/// Result of resolving a model name a user typed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ModelChoice {
    /// Exactly one model matched.
    One(String),
    /// Several matched, so the caller must disambiguate.
    Ambiguous(Vec<String>),
    /// Nothing matched.
    None,
}

impl Catalog {
    /// Resolves a user-provided model name.
    #[must_use]
    pub fn choose(&self, query: &str) -> ModelChoice {
        // An exact identifier always wins, including one the catalog lacks.
        if self.get(query).is_some() {
            return ModelChoice::One(query.to_owned());
        }
        let matches = self.resolve(query);
        match matches.len() {
            0 => {
                // A well-formed identifier is accepted even when unknown, so a
                // model released after this build remains usable.
                if looks_like_model_id(query) {
                    ModelChoice::One(query.to_owned())
                } else {
                    ModelChoice::None
                }
            }
            1 => matches.first().map_or(ModelChoice::None, |model| {
                ModelChoice::One(model.id.clone())
            }),
            _ => {
                let mut ids: Vec<String> = matches
                    .iter()
                    .map(|model| model.id.clone())
                    .take(10)
                    .collect();
                ids.sort();
                ModelChoice::Ambiguous(ids)
            }
        }
    }
}

/// Returns true when a string is plausibly an endpoint model identifier.
///
/// Accepts the letters, digits, and separators that appear in real identifiers,
/// and rejects anything with whitespace or a shell metacharacter.
fn looks_like_model_id(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 {
        return false;
    }
    if !value.contains('/') {
        // A bare name is accepted only when it has no spaces.
        return value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    }
    value.split('/').all(|segment| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    })
}

/// Builds the catalog for a named connection from its declared model list.
///
/// A named endpoint does not expose a catalog, so the models it may serve are
/// exactly the ones the connection declares.
#[must_use]
pub fn from_configured_models(
    provider: &str,
    models: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Catalog {
    let mut catalog = Catalog::new(provider);
    catalog.source = CatalogSource::Configured;
    for (id, metadata) in models {
        let mut entry = ModelMetadata::new(id);
        if let Some(window) = metadata
            .get("context_window")
            .and_then(serde_json::Value::as_u64)
        {
            entry.context_window = Some(window);
        }
        if let Some(max) = metadata
            .get("max_output_tokens")
            .and_then(serde_json::Value::as_u64)
        {
            entry.max_output_tokens = Some(max);
        }
        if let Some(tools) = metadata
            .get("supports_tool_use")
            .and_then(serde_json::Value::as_bool)
        {
            entry.capabilities.tools = tools;
        }
        if let Some(vision) = metadata
            .get("supports_vision")
            .and_then(serde_json::Value::as_bool)
        {
            entry.capabilities.vision = vision;
            entry.capabilities.file_input = vision;
        }
        catalog.models.push(entry);
    }
    catalog
}

/// Builds a catalog from an endpoint's model listing.
///
/// The OpenAI-shaped listing, which the compatible-server ecosystem shares, is
/// an object with a `data` array whose entries carry an `id`. Only the
/// identifier is taken: the rest of an entry is vendor decoration, and reading
/// a capability out of it that the vendor did not state would be a guess
/// presented as a fact. An entry without an identifier is skipped rather than
/// listed as an empty name.
#[must_use]
pub fn from_endpoint_listing(provider: &str, body: &str) -> Option<Catalog> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let entries = parsed
        .get("data")
        .or_else(|| parsed.get("models"))
        .and_then(serde_json::Value::as_array)?;

    let mut catalog = Catalog::new(provider);
    catalog.source = CatalogSource::Endpoint;
    for entry in entries {
        let id = entry
            .get("id")
            .or_else(|| entry.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim();
        if id.is_empty() {
            continue;
        }
        catalog.models.push(ModelMetadata::new(id));
    }
    Some(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_with(entries: Vec<ModelMetadata>) -> Catalog {
        Catalog {
            provider: "test".to_owned(),
            models: entries,
            source: CatalogSource::Endpoint,
        }
    }

    fn capable() -> ModelMetadata {
        ModelMetadata {
            id: "vendor/full".to_owned(),
            display_name: Some("Full".to_owned()),
            context_window: Some(200_000),
            max_output_tokens: Some(8192),
            reasoning_efforts: vec!["low".to_owned(), "medium".to_owned(), "high".to_owned()],
            capabilities: Capabilities {
                tools: true,
                vision: true,
                file_input: true,
                reasoning: true,
                fast_mode: true,
                cache_reporting: true,
            },
        }
    }

    #[test]
    fn an_endpoint_listing_becomes_a_catalog() {
        let body = r#"{"object":"list","data":[
            {"id":"alpha","object":"model","owned_by":"vendor"},
            {"id":"beta","object":"model","owned_by":"vendor"}
        ]}"#;
        let catalog = from_endpoint_listing("zen", body).expect("parsed");
        assert_eq!(catalog.provider, "zen");
        assert_eq!(catalog.source, CatalogSource::Endpoint);
        assert_eq!(
            catalog
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
    }

    #[test]
    fn an_entry_without_an_identifier_is_skipped() {
        // A nameless entry would be listed as a blank model, which a user
        // cannot select and which would be sent as an empty identifier.
        let body = r#"{"data":[{"id":"real"},{"object":"model"},{"id":"  "}]}"#;
        let catalog = from_endpoint_listing("p", body).expect("parsed");
        assert_eq!(
            catalog
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["real"]
        );
    }

    #[test]
    fn a_listing_under_models_is_also_read() {
        // Some compatible servers name the array `models` rather than `data`.
        let body = r#"{"models":[{"name":"gamma"}]}"#;
        let catalog = from_endpoint_listing("p", body).expect("parsed");
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "gamma");
    }

    #[test]
    fn an_unrecognized_body_is_not_a_catalog() {
        // A failure and an empty list are different answers, so a body that is
        // not a listing returns nothing rather than an empty catalog.
        assert!(from_endpoint_listing("p", "<html>error</html>").is_none());
        assert!(from_endpoint_listing("p", "{}").is_none());
        assert!(from_endpoint_listing("p", "").is_none());
    }

    #[test]
    fn an_unknown_model_is_conservative_about_request_shape() {
        let capabilities = Capabilities::unknown();
        assert!(!capabilities.native_vision());
        assert!(!capabilities.reasoning);
        assert!(!capabilities.fast_mode);
        // Tools are assumed present because every usable coding model has them,
        // and the tool list is filtered by the policy layer anyway.
        assert!(capabilities.tools);
    }

    #[test]
    fn native_vision_requires_both_vision_and_file_input() {
        let mut capabilities = Capabilities::unknown();
        capabilities.vision = true;
        assert!(!capabilities.native_vision());
        capabilities.file_input = true;
        assert!(capabilities.native_vision());
    }

    #[test]
    fn an_absent_model_falls_back_to_the_identifier() {
        let catalog = Catalog::new("test");
        let metadata = catalog.metadata_or_default("vendor/new-model");
        assert_eq!(metadata.id, "vendor/new-model");
        assert_eq!(metadata.label(), "vendor/new-model");
        assert!(!metadata.capabilities.native_vision());
    }

    #[test]
    fn an_absent_model_uses_a_conservative_context_window() {
        let catalog = Catalog::new("test");
        assert_eq!(
            catalog.metadata_or_default("unknown").usable_context(),
            DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn a_reported_context_window_is_used() {
        let catalog = catalog_with(vec![capable()]);
        assert_eq!(
            catalog.metadata_or_default("vendor/full").usable_context(),
            200_000
        );
    }

    #[test]
    fn an_exact_model_identifier_is_found() {
        let catalog = catalog_with(vec![capable()]);
        assert!(catalog.get("vendor/full").is_some());
        assert!(catalog.get("vendor/other").is_none());
    }

    #[test]
    fn resolution_matches_a_substring_case_insensitively() {
        let catalog = catalog_with(vec![capable()]);
        let matches = catalog.resolve("FULL");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "vendor/full");
    }

    #[test]
    fn resolution_prefers_an_exact_match() {
        let catalog = catalog_with(vec![ModelMetadata::new("a/full"), capable()]);
        let matches = catalog.resolve("vendor/full");
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let catalog = catalog_with(vec![capable()]);
        assert!(catalog.resolve("").is_empty());
        assert_eq!(catalog.choose(""), ModelChoice::None);
    }

    #[test]
    fn a_well_formed_unknown_identifier_is_accepted() {
        let catalog = Catalog::new("test");
        assert_eq!(
            catalog.choose("vendor/released-later"),
            ModelChoice::One("vendor/released-later".to_owned())
        );
    }

    #[test]
    fn a_bare_unknown_name_is_accepted_when_it_is_clean() {
        let catalog = Catalog::new("test");
        assert_eq!(
            catalog.choose("local-llama"),
            ModelChoice::One("local-llama".to_owned())
        );
    }

    #[test]
    fn a_name_with_spaces_is_not_a_model_identifier() {
        let catalog = Catalog::new("test");
        assert_eq!(catalog.choose("not a model"), ModelChoice::None);
    }

    #[test]
    fn a_name_with_a_shell_metacharacter_is_not_a_model_identifier() {
        let catalog = Catalog::new("test");
        for query in ["vendor/x;rm -rf /", "vendor/$(whoami)", "vendor/`id`"] {
            assert_eq!(catalog.choose(query), ModelChoice::None, "{query}");
        }
    }

    #[test]
    fn an_empty_segment_is_not_a_model_identifier() {
        let catalog = Catalog::new("test");
        assert_eq!(catalog.choose("vendor//x"), ModelChoice::None);
        assert_eq!(catalog.choose("/leading"), ModelChoice::None);
    }

    #[test]
    fn an_ambiguous_query_lists_the_candidates() {
        let catalog = catalog_with(vec![
            ModelMetadata::new("vendor/alpha-one"),
            ModelMetadata::new("vendor/alpha-two"),
        ]);
        match catalog.choose("alpha") {
            ModelChoice::Ambiguous(ids) => {
                assert_eq!(ids.len(), 2);
                assert!(ids.contains(&"vendor/alpha-one".to_owned()));
            }
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn an_exact_identifier_beats_an_ambiguous_substring() {
        let catalog = catalog_with(vec![
            ModelMetadata::new("vendor/alpha-one"),
            ModelMetadata::new("vendor/alpha"),
        ]);
        assert_eq!(
            catalog.choose("vendor/alpha"),
            ModelChoice::One("vendor/alpha".to_owned())
        );
    }

    #[test]
    fn effort_is_absent_for_a_model_without_reasoning() {
        let metadata = ModelMetadata::new("plain");
        assert_eq!(metadata.resolve_effort(Effort::High), None);
        assert!(!metadata.accepts_effort(Effort::High));
    }

    #[test]
    fn effort_is_absent_when_auto_is_requested() {
        let metadata = capable();
        assert_eq!(metadata.resolve_effort(Effort::Auto), None);
    }

    #[test]
    fn a_supported_effort_is_passed_through() {
        let metadata = capable();
        assert_eq!(metadata.resolve_effort(Effort::High), Some(Effort::High));
    }

    #[test]
    fn an_unsupported_effort_is_downgraded_to_the_nearest_listed_one() {
        let metadata = capable();
        // `xhigh` is not listed, so the largest listed value below it is used.
        assert_eq!(metadata.resolve_effort(Effort::Xhigh), Some(Effort::High));
        assert_eq!(metadata.resolve_effort(Effort::Max), Some(Effort::High));
    }

    #[test]
    fn a_model_listing_no_efforts_accepts_every_effort() {
        let mut metadata = capable();
        metadata.reasoning_efforts = Vec::new();
        assert_eq!(metadata.resolve_effort(Effort::Max), Some(Effort::Max));
    }

    #[test]
    fn a_request_below_the_lowest_listed_effort_finds_nothing() {
        let mut metadata = capable();
        metadata.reasoning_efforts = vec!["high".to_owned()];
        // Nothing at or below `low` is listed.
        assert_eq!(metadata.resolve_effort(Effort::Low), None);
        assert_eq!(metadata.resolve_effort(Effort::High), Some(Effort::High));
    }

    #[test]
    fn a_configured_connection_builds_a_catalog_from_its_declared_models() {
        let models = std::collections::BTreeMap::from([(
            "local/llama".to_owned(),
            serde_json::json!({
                "context_window": 32768,
                "supports_tool_use": true,
                "supports_vision": false,
            }),
        )]);
        let catalog = from_configured_models("my-endpoint", &models);
        assert_eq!(catalog.source, CatalogSource::Configured);
        let entry = catalog.get("local/llama").expect("present");
        assert_eq!(entry.context_window, Some(32768));
        assert!(entry.capabilities.tools);
        assert!(!entry.capabilities.vision);
    }

    #[test]
    fn vision_implies_file_input_for_a_configured_model() {
        let models = std::collections::BTreeMap::from([(
            "m".to_owned(),
            serde_json::json!({ "supports_vision": true }),
        )]);
        let catalog = from_configured_models("p", &models);
        let entry = catalog.get("m").expect("present");
        assert!(entry.capabilities.native_vision());
    }

    #[test]
    fn the_json_view_carries_the_source() {
        let catalog = catalog_with(vec![capable()]);
        let json = catalog.to_json();
        assert_eq!(json["provider"], "test");
        assert_eq!(json["source"], "endpoint");
        assert_eq!(json["models"].as_array().expect("array").len(), 1);
    }

    #[test]
    fn a_catalog_round_trips_through_json() {
        let catalog = catalog_with(vec![capable()]);
        let text = serde_json::to_string(&catalog).expect("serialize");
        let parsed: Catalog = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(parsed.models.len(), 1);
        assert_eq!(parsed.models[0], capable());
    }
}
