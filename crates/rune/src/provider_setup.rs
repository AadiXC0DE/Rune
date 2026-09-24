//! Connecting a provider and listing the models it offers.
//!
//! Connecting writes the credential to the profile file with private permissions
//! and never echoes it. The endpoint, the model, and the variable a credential
//! comes from are reported, so a user can see what the harness will use without
//! reading the stored secret.

use std::fmt::Write as _;

use rune_core::config::{Provider, Settings};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;
use rune_net::auth::{self, CredentialSource};
use rune_net::catalog::{Catalog, CatalogSource, ModelMetadata};
use rune_net::providers;

/// Endpoint used when a provider is connected without one given.
///
/// A provider whose dialect is OpenAI-compatible has no single home, so it has
/// no default: requiring the endpoint is what keeps a request from being sent
/// somewhere the user never named.
#[must_use]
pub fn default_base_url(provider: &Provider) -> Option<&'static str> {
    // Read from the provider table rather than restated, so the endpoint the
    // connection command offers and the one the transport uses cannot drift.
    // A compatible or named endpoint has no single home and returns `None`,
    // because guessing one would send a request somewhere never named.
    providers::base_url(provider.as_str())
}

/// Reads a credential from the environment for a provider.
///
/// Only the variables a provider actually uses are consulted, so a stray
/// variable for a different provider is not picked up as this one's credential.
fn credential_from_environment(provider: &str, configured: Option<&str>) -> Option<String> {
    let names = configured
        .into_iter()
        .chain(auth::default_variables(provider).iter().copied());
    for name in names {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            return Some(value);
        }
    }
    None
}

/// Writes one key into the user configuration.
///
/// Reads the file first and replaces only the named key, so a value the user set
/// by hand survives. A value of `None` removes the key.
pub fn save_key(paths: &Paths, key: &str, value: Option<toml::Value>) -> Result<()> {
    let path = paths.config_file(None);
    if let Some(parent) = path.parent() {
        rune_core::paths::create_dir_private(parent)?;
    }

    let existing = rune_core::paths::read_private(&path, MAX_CONFIG_BYTES)?;
    let mut document: toml::Table = match existing.as_deref() {
        Some(text) if !text.trim().is_empty() => toml::from_str(text).map_err(|err| {
            RuneError::new(
                ErrorCode::CorruptRecord,
                format!("the config file could not be parsed: {err}"),
            )
            .with_hint("repair the file, or move it aside")
        })?,
        _ => toml::Table::new(),
    };

    match value {
        Some(value) => {
            document.insert(key.to_owned(), value);
        }
        None => {
            document.remove(key);
        }
    }

    let rendered = toml::to_string_pretty(&document).map_err(|err| {
        RuneError::new(
            ErrorCode::Internal,
            format!("the configuration could not be written: {err}"),
        )
    })?;
    rune_core::paths::write_private(&path, &rendered)
}

/// Writes the provider selection into the user configuration.
///
/// The file is read first and the named keys are replaced, so a value the user
/// set by hand is preserved rather than overwritten by a whole-file write.
pub fn save_selection(paths: &Paths, selection: &Selection) -> Result<()> {
    let path = paths.config_file(None);
    if let Some(parent) = path.parent() {
        rune_core::paths::create_dir_private(parent)?;
    }

    let existing = rune_core::paths::read_private(&path, MAX_CONFIG_BYTES)?;
    let mut document: toml::Table = match existing.as_deref() {
        Some(text) if !text.trim().is_empty() => toml::from_str(text).map_err(|err| {
            RuneError::new(
                ErrorCode::CorruptRecord,
                format!("the config file could not be parsed: {err}"),
            )
            .with_hint("repair the file, or move it aside")
        })?,
        _ => toml::Table::new(),
    };

    document.insert(
        "provider".to_owned(),
        toml::Value::String(selection.provider.clone()),
    );
    // The model is selected by a table keyed on the provider, not by a bare
    // key, so writing it anywhere else produces a file the loader refuses.
    if let Some(model) = &selection.model {
        let table = document
            .entry("models".to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(table) = table.as_table_mut() {
            table.insert(
                selection.provider.clone(),
                toml::Value::String(model.clone()),
            );
        }
    }
    if let Some(base_url) = &selection.base_url {
        document.insert("base_url".to_owned(), toml::Value::String(base_url.clone()));
    }

    let rendered = toml::to_string_pretty(&document).map_err(|err| {
        RuneError::new(
            ErrorCode::Internal,
            format!("the configuration could not be written: {err}"),
        )
    })?;
    rune_core::paths::write_private(&path, &rendered)
}

/// Largest configuration file accepted.
pub const MAX_CONFIG_BYTES: u64 = 256 * 1024;

/// What a connection writes into the configuration.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Selection {
    /// Provider name as the user wrote it.
    pub provider: String,
    /// Model to select, when one was given.
    pub model: Option<String>,
    /// Endpoint to use, when one was given or is known.
    pub base_url: Option<String>,
}

/// Stores a credential for a provider.
///
/// The value is written to the profile file, which is created private. An empty
/// value is refused rather than stored, because a stored empty credential reads
/// back as configured and then fails at the endpoint.
pub fn connect(paths: &Paths, provider: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(
            RuneError::invalid_field("credential", "a credential cannot be empty")
                .with_hint("pass the value the provider issued, or leave it unset"),
        );
    }
    auth::store(paths, provider, value.trim())
}

/// Removes a stored credential.
///
/// Reports whether one was there, so a caller can say which happened rather than
/// always reporting success.
pub fn disconnect(paths: &Paths, provider: &str) -> Result<bool> {
    auth::remove(paths, provider)
}

/// Reports what the harness will use for a provider.
#[must_use]
pub fn render_connection(settings: &Settings, paths: &Paths) -> String {
    let provider = settings.provider.to_string();
    let mut out = String::new();
    let _ = writeln!(out, "provider  {provider}");
    let _ = writeln!(out, "model     {}", settings.model);

    match &settings.base_url {
        Some(url) => {
            let _ = writeln!(out, "endpoint  {url}");
        }
        None => {
            let _ = writeln!(out, "endpoint  not set");
        }
    }

    let resolved = auth::resolve(paths, &provider, settings.api_key_env.as_deref())
        .ok()
        .flatten();
    match resolved {
        Some(credential) => {
            // The length is reported rather than the value, so the output can be
            // pasted into a report without carrying the secret.
            let _ = writeln!(
                out,
                "credential set ({} characters, from {})",
                credential.len(),
                source_name(credential.source()),
            );
        }
        None => {
            let _ = writeln!(out, "credential not set");
        }
    }
    out.trim_end().to_owned()
}

/// Returns the readable name of where a credential came from.
#[must_use]
pub const fn source_name(source: CredentialSource) -> &'static str {
    match source {
        CredentialSource::ConfiguredVariable => "the configured variable",
        CredentialSource::Environment => "the environment",
        CredentialSource::SystemStore => "the system secret store",
        CredentialSource::ProfileFile => "the profile file",
    }
}

/// Returns the catalog for the configured provider.
///
/// Configuration names one model per provider rather than a table of metadata,
/// so the catalog holds the configured identifier described by the compiled
/// defaults. It is not filled in from a request: nothing here reaches a network.
#[must_use]
pub fn catalog_for(settings: &Settings) -> Catalog {
    let provider = settings.provider.to_string();
    let mut catalog = Catalog::new(provider);
    // Nothing here contacts an endpoint, so the source says where the entries
    // actually came from rather than the default the type happens to carry.
    catalog.source = CatalogSource::Configured;
    if !settings.model.trim().is_empty() {
        catalog
            .models
            .push(ModelMetadata::new(settings.model.clone()));
    }
    catalog
}

/// Fetches the models the configured endpoint serves.
///
/// The list comes from the endpoint rather than a compiled table, because the
/// identifier has to be one the endpoint will accept and no table this build
/// ships can name models released after it. A provider whose dialect has no
/// listing path reports that instead of being asked for one.
pub fn fetch_catalog(
    settings: &Settings,
    paths: &Paths,
    timeout: std::time::Duration,
) -> Result<Catalog> {
    let provider_name = settings.provider.to_string();
    let base_url = settings.base_url.clone().ok_or_else(|| {
        RuneError::new(
            ErrorCode::InvalidConfiguration,
            format!("no endpoint is configured for provider `{provider_name}`"),
        )
        .with_hint("set `base_url` in the user config, or run `rune connect`")
    })?;

    let credential = auth::resolve(paths, &provider_name, settings.api_key_env.as_deref())?
        .ok_or_else(|| {
            auth::missing_credential_error(&provider_name, settings.api_key_env.as_deref())
        })?;

    let dialect: Box<dyn rune_net::provider::Provider> = match settings.provider {
        Provider::Anthropic => Box::new(rune_net::anthropic::Anthropic),
        Provider::Responses => Box::new(rune_net::responses::Responses),
        _ => Box::new(rune_net::chat_completions::ChatCompletions),
    };

    let endpoint = endpoint(
        &settings.provider,
        &base_url,
        credential.expose(),
        rune_net::transport::AuthStyle::Bearer,
        settings.offline,
    );

    let body = rune_net::transport::list_models(
        &rune_net::transport::agent(),
        &endpoint,
        dialect.as_ref(),
        timeout,
    )
    .map_err(|err| err.to_rune_error())?;

    let mut catalog =
        rune_net::catalog::from_endpoint_listing(&provider_name, &body).ok_or_else(|| {
            RuneError::new(
                ErrorCode::ProtocolViolation,
                "the endpoint's model list was not in a shape this build reads",
            )
            .with_hint("the configured model is used as given")
        })?;
    // An endpoint listing carries identifiers and little else, so the capacity
    // of each model is filled in from the published catalog. Doing it here means
    // every caller sees the same catalogue, rather than each having to remember.
    enrich_with_capacity(settings, paths, &mut catalog);
    Ok(catalog)
}

/// Fills in the capacity of each model from the published catalog.
///
/// An OpenAI-compatible listing carries an identifier and nothing else, so
/// without this every model is budgeted against the compiled default and a
/// model serving a million tokens reports as though it held a hundred and
/// twenty-eight thousand. A figure the listing already stated is kept, because
/// the endpoint describing its own model is the better authority.
///
/// Nothing here fails a listing: an unreachable catalog, an unreadable cache,
/// and a provider the catalog does not describe all leave the models exactly as
/// the endpoint listed them.
pub fn enrich_with_capacity(settings: &Settings, paths: &Paths, catalog: &mut Catalog) {
    let provider_name = settings.provider.to_string();
    let Some(key) = rune_net::models_dev::catalog_key(&provider_name) else {
        return;
    };
    let Some(limits) = fetch_provider_limits(settings, paths, key) else {
        return;
    };
    for model in &mut catalog.models {
        let Some(limit) = limits.get(&model.id) else {
            continue;
        };
        if model.context_window.is_none() {
            model.context_window = limit.context;
        }
        if model.max_output_tokens.is_none() {
            model.max_output_tokens = limit.output;
        }
    }
}

/// URL the published catalog is served from.
const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// How long the catalog fetch waits.
const MODELS_DEV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Returns the published capacity for one provider.
///
/// The cache is read first, so a session does not pay a multi-megabyte download
/// every time it starts, and the fetch happens only when there is no cache. A
/// cached document is used whatever its age: a model's window does not change,
/// and a stale entry for a model that has been retired costs nothing.
fn fetch_provider_limits(
    settings: &Settings,
    paths: &Paths,
    key: &str,
) -> Option<rune_net::models_dev::ProviderLimits> {
    let cache = paths.models_cache_file();
    if let Some(limits) = read_cached_limits(&cache, key) {
        return Some(limits);
    }
    // Nothing cached and no network permitted means nothing to add, which the
    // caller treats as an unenriched listing rather than a failure.
    if settings.offline {
        return None;
    }

    let fetched =
        rune_net::transport::fetch_url(MODELS_DEV_URL, "application/json", MODELS_DEV_TIMEOUT)
            .ok()?;
    if fetched.status != 200 {
        return None;
    }
    let body = String::from_utf8(fetched.body).ok()?;
    let limits = rune_net::models_dev::parse(&body, key)?;
    // Written after it parses, so a partial or error body is never cached and
    // the next run tries again rather than reading a failure forever.
    let _ = write_cached_catalog(paths, &body);
    Some(limits)
}

/// Reads a provider's capacity out of the cache.
fn read_cached_limits(
    path: &camino::Utf8Path,
    key: &str,
) -> Option<rune_net::models_dev::ProviderLimits> {
    let text = rune_core::paths::read_private(path, MAX_CACHED_CATALOG_BYTES as u64).ok()??;
    rune_net::models_dev::parse(&text, key)
}

/// Writes the catalog to the cache.
fn write_cached_catalog(paths: &Paths, body: &str) -> Result<()> {
    if body.len() > MAX_CACHED_CATALOG_BYTES {
        return Err(RuneError::too_large(
            "models-dev.json",
            body.len(),
            MAX_CACHED_CATALOG_BYTES,
        ));
    }
    let path = paths.models_cache_file();
    if let Some(parent) = path.parent() {
        rune_core::paths::create_dir_private(parent)?;
    }
    rune_core::paths::write_private(&path, body)
}

/// Largest cached catalog accepted.
///
/// The published document is a few megabytes and grows as models are added. The
/// ceiling is generous enough to survive that and tight enough that a wrong
/// address or a corrupt file cannot fill memory on the next start.
const MAX_CACHED_CATALOG_BYTES: usize = rune_net::models_dev::MAX_CATALOG_BYTES;

/// Renders a catalog for a terminal.
#[must_use]
pub fn render_catalog(catalog: &Catalog) -> String {
    if catalog.models.is_empty() {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "no models are declared for provider `{}`",
            catalog.provider
        );
        let _ = writeln!(out, "the configured model is used as given");
        return out;
    }

    let mut out = String::new();
    let _ = writeln!(out, "{}", catalog.provider);
    for model in &catalog.models {
        let _ = write!(out, "  {}", model.id);
        if let Some(window) = model.context_window {
            let _ = write!(out, "  context {window}");
        }
        if let Some(max) = model.max_output_tokens {
            let _ = write!(out, "  max output {max}");
        }
        let flags = capability_names(model);
        if !flags.is_empty() {
            let _ = write!(out, "  [{}]", flags.join(", "));
        }
        out.push('\n');
    }
    out.trim_end().to_owned()
}

/// Names the capabilities a model declares.
fn capability_names(model: &ModelMetadata) -> Vec<&'static str> {
    let mut names = Vec::new();
    if model.capabilities.tools {
        names.push("tools");
    }
    if model.capabilities.vision {
        names.push("vision");
    }
    if model.capabilities.reasoning {
        names.push("reasoning");
    }
    names
}

/// Returns the endpoint already configured for a provider, when there is one.
///
/// The configured endpoint belongs to the provider that was configured, so it
/// is only reused for a connection to that same provider. Reusing it for
/// another one would point the new provider at the old provider's host, which
/// succeeds and then sends every request somewhere the user never named.
#[must_use]
pub fn inherited_endpoint(
    configured_provider: &Provider,
    configured_url: Option<&str>,
    connecting: &str,
) -> Option<String> {
    if configured_provider.as_str() != connecting {
        return None;
    }
    configured_url.map(str::to_owned)
}

/// Builds the endpoint a request is sent to, with the headers its provider needs.
///
/// A provider that routes by conversation or asks its clients to identify
/// themselves states that in the provider table, so the headers are applied here
/// rather than at each call site. A caller that builds its own endpoint would
/// otherwise reach the provider without them and be refused.
#[must_use]
pub fn endpoint(
    provider: &Provider,
    base_url: &str,
    credential: &str,
    auth: rune_net::transport::AuthStyle,
    offline: bool,
) -> rune_net::transport::Endpoint {
    let name = provider.as_str();
    let mut endpoint =
        rune_net::transport::Endpoint::new(base_url.to_owned(), credential.to_owned())
            .with_auth(auth)
            .offline(offline);
    for (header, value) in providers::lookup(name)
        .map(|entry| entry.required_headers)
        .unwrap_or_default()
    {
        endpoint = endpoint.with_header(*header, *value);
    }
    endpoint
}

/// Resolves the endpoint a connection should use.
pub fn resolve_endpoint(provider: &Provider, configured: Option<&str>) -> Result<String> {
    let url = configured
        .map(str::to_owned)
        .or_else(|| default_base_url(provider).map(str::to_owned))
        .ok_or_else(|| {
            RuneError::new(
                ErrorCode::InvalidConfiguration,
                "this provider has no default endpoint",
            )
            .with_hint("pass the endpoint, or set `base_url` in the config")
        })?;
    rune_net::transport::validate_url(&url)?;
    Ok(url)
}

/// Returns a credential from the environment, when one is present.
///
/// Present so a test can exercise the resolution without mutating the real
/// environment of the test runner.
#[must_use]
pub fn environment_credential(provider: &str, configured: Option<&str>) -> Option<String> {
    credential_from_environment(provider, configured)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::config::Provider;

    /// A settings value for a provider, with no network permitted.
    fn offline_settings(provider: &str) -> Settings {
        Settings {
            provider: rune_core::config::parse_provider(provider),
            offline: true,
            ..Settings::default()
        }
    }

    fn temp_paths(root: &camino::Utf8Path) -> Paths {
        // The state root must be private, and a tempdir is created world
        // readable, so it is tightened before anything writes beneath it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                root,
                std::fs::Permissions::from_mode(rune_core::paths::DIR_MODE),
            );
        }
        Paths::resolve(Some(root.as_str()), None, None, None, Some(root.as_str()))
    }

    #[test]
    fn a_cached_catalog_supplies_the_capacity_the_listing_omitted() {
        // An OpenAI-compatible listing carries identifiers and little else, so
        // without this every model is budgeted against the compiled default.
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = temp_paths(root);
        paths.ensure_roots().expect("roots");
        let doc = r#"{"opencode-go":{"models":{
            "grok-4.7":{"limit":{"context":500000,"output":500000}}}}}"#;
        write_cached_catalog(&paths, doc).expect("cached");

        let settings = offline_settings("opencode-go");
        let mut catalog = Catalog::new("opencode-go".to_owned());
        catalog.models.push(ModelMetadata::new("grok-4.7"));
        catalog.models.push(ModelMetadata::new("unknown-model"));
        enrich_with_capacity(&settings, &paths, &mut catalog);

        assert_eq!(catalog.models[0].context_window, Some(500_000));
        assert_eq!(catalog.models[0].max_output_tokens, Some(500_000));
        // A model the catalog does not describe is left as it was rather than
        // given a made-up figure.
        assert_eq!(catalog.models[1].context_window, None);
    }

    #[test]
    fn a_figure_the_endpoint_stated_is_not_replaced() {
        // The endpoint describing its own model is the better authority, so a
        // figure it stated survives enrichment.
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = temp_paths(root);
        paths.ensure_roots().expect("roots");
        let doc = r#"{"opencode-go":{"models":{
            "m":{"limit":{"context":111,"output":222}}}}}"#;
        write_cached_catalog(&paths, doc).expect("cached");

        let settings = offline_settings("opencode-go");
        let mut catalog = Catalog::new("opencode-go".to_owned());
        let mut entry = ModelMetadata::new("m");
        entry.context_window = Some(4096);
        catalog.models.push(entry);
        enrich_with_capacity(&settings, &paths, &mut catalog);

        assert_eq!(catalog.models[0].context_window, Some(4096));
        // The output ceiling was not stated, so it is filled in.
        assert_eq!(catalog.models[0].max_output_tokens, Some(222));
    }

    #[test]
    fn an_offline_run_with_no_cache_adds_nothing_and_does_not_fail() {
        // No network permitted and nothing cached means nothing to add, which
        // is an unenriched listing rather than an error.
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = temp_paths(root);
        paths.ensure_roots().expect("roots");

        let settings = offline_settings("opencode-go");
        let mut catalog = Catalog::new("opencode-go".to_owned());
        catalog.models.push(ModelMetadata::new("grok-4.7"));
        enrich_with_capacity(&settings, &paths, &mut catalog);
        assert_eq!(catalog.models[0].context_window, None);
    }

    #[test]
    fn a_provider_the_catalog_does_not_describe_is_left_alone() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = temp_paths(root);
        paths.ensure_roots().expect("roots");
        let doc = r#"{"opencode-go":{"models":{"m":{"limit":{"context":9}}}}}"#;
        write_cached_catalog(&paths, doc).expect("cached");

        // A self-hosted endpoint is not described by any published catalog.
        let settings = offline_settings("chat_completions");
        let mut catalog = Catalog::new("chat_completions".to_owned());
        catalog.models.push(ModelMetadata::new("m"));
        enrich_with_capacity(&settings, &paths, &mut catalog);
        assert_eq!(catalog.models[0].context_window, None);
    }

    #[test]
    fn a_corrupt_cache_is_ignored_rather_than_fatal() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = temp_paths(root);
        paths.ensure_roots().expect("roots");
        write_cached_catalog(&paths, "not json at all").expect("cached");

        let settings = offline_settings("opencode-go");
        let mut catalog = Catalog::new("opencode-go".to_owned());
        catalog.models.push(ModelMetadata::new("grok-4.7"));
        enrich_with_capacity(&settings, &paths, &mut catalog);
        assert_eq!(catalog.models[0].context_window, None);
    }

    #[test]
    fn a_second_provider_does_not_inherit_the_first_endpoint() {
        // Connecting another provider while one is configured used to carry the
        // configured endpoint over, so the new provider pointed at the old
        // provider's host and every request went somewhere never named.
        let configured = rune_core::config::parse_provider("anthropic");
        let url = Some("https://api.anthropic.com");

        assert_eq!(
            inherited_endpoint(&configured, url, "anthropic").as_deref(),
            Some("https://api.anthropic.com"),
            "the same provider should reuse its endpoint"
        );
        assert_eq!(
            inherited_endpoint(&configured, url, "chat_completions"),
            None,
            "a different provider must not inherit another one's endpoint"
        );
    }

    #[test]
    fn a_provider_with_no_configured_endpoint_inherits_nothing() {
        let configured = rune_core::config::parse_provider("anthropic");
        assert_eq!(inherited_endpoint(&configured, None, "anthropic"), None);
    }

    fn paths(root: &camino::Utf8Path) -> Paths {
        let resolved = Paths::resolve(
            Some(root.as_str()),
            Some(root.as_str()),
            Some(root.as_str()),
            Some(root.as_str()),
            None,
        );
        resolved.ensure_roots().expect("roots");
        resolved
    }

    #[test]
    fn a_provider_with_a_single_home_has_a_default_endpoint() {
        assert_eq!(
            default_base_url(&Provider::Anthropic),
            Some("https://api.anthropic.com")
        );
        assert_eq!(
            default_base_url(&Provider::Responses),
            Some("https://api.openai.com/v1")
        );
    }

    #[test]
    fn an_openai_compatible_provider_has_no_default_endpoint() {
        // There is no one home for a compatible dialect, so a request must name
        // one rather than being sent somewhere the user never chose.
        assert_eq!(default_base_url(&Provider::ChatCompletions), None);
        assert_eq!(default_base_url(&Provider::Unconfigured), None);
    }

    #[test]
    fn resolving_without_a_default_or_a_value_names_the_remedy() {
        let err = resolve_endpoint(&Provider::ChatCompletions, None).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidConfiguration);
        assert!(err.hint().is_some(), "no remedy was given");
    }

    #[test]
    fn a_configured_endpoint_wins_over_the_default() {
        let resolved = resolve_endpoint(&Provider::Anthropic, Some("http://127.0.0.1:9/v1"))
            .expect("resolved");
        assert_eq!(resolved, "http://127.0.0.1:9/v1");
    }

    #[test]
    fn a_malformed_endpoint_is_refused() {
        assert!(resolve_endpoint(&Provider::Anthropic, Some("not a url")).is_err());
    }

    #[test]
    fn an_empty_credential_is_refused_rather_than_stored() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let err = connect(&paths(root), "anthropic", "   ").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn a_credential_round_trips_and_is_removed() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);

        connect(&paths, "anthropic", "sk-test-1234").expect("stored");
        let stored = auth::resolve(&paths, "anthropic", None)
            .expect("resolved")
            .expect("present");
        assert_eq!(stored.expose(), "sk-test-1234");

        assert!(disconnect(&paths, "anthropic").expect("removed"));
        assert!(
            auth::resolve(&paths, "anthropic", None)
                .expect("resolved")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_stored_credential_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        connect(&paths, "anthropic", "sk-test-1234").expect("stored");

        let mode = std::fs::metadata(paths.credentials_file())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the credential file is readable by others");
    }

    #[test]
    fn saving_a_selection_writes_the_provider_and_endpoint() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        save_selection(
            &paths,
            &Selection {
                provider: "anthropic".to_owned(),
                model: Some("claude-test".to_owned()),
                base_url: Some("https://api.anthropic.com".to_owned()),
            },
        )
        .expect("saved");

        let written = std::fs::read_to_string(paths.config_file(None)).expect("read");
        assert!(written.contains(r#"provider = "anthropic""#), "{written}");
        assert!(
            written.contains(r#"base_url = "https://api.anthropic.com""#),
            "{written}"
        );
        // The model lives in a table keyed on the provider, which is where the
        // loader looks for it.
        assert!(written.contains("[models]"), "{written}");
        assert!(
            written.contains(r#"anthropic = "claude-test""#),
            "{written}"
        );
    }

    #[test]
    fn a_saved_selection_is_a_configuration_the_loader_accepts() {
        // The loader rejects an unknown key and selects the model from a table
        // keyed on the provider, so a bare `model` key would make the file
        // unreadable and lose every setting in it.
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        save_selection(
            &paths,
            &Selection {
                provider: "anthropic".to_owned(),
                model: Some("claude-test".to_owned()),
                base_url: Some("https://api.anthropic.com".to_owned()),
            },
        )
        .expect("saved");

        let loaded = rune_core::config::load(
            None,
            Some(&paths.config_file(None)),
            &rune_core::config::EnvironmentOverrides::default(),
        );
        assert!(
            loaded.diagnostics.is_empty(),
            "the written configuration was refused: {:#?}",
            loaded.diagnostics
        );
        assert_eq!(loaded.provider, Provider::Anthropic);
        assert_eq!(loaded.model, "claude-test");
    }

    #[test]
    fn saving_a_selection_preserves_keys_it_does_not_set() {
        // A whole-file write would discard whatever the user configured by hand.
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let file = paths.config_file(None);
        if let Some(parent) = file.parent() {
            rune_core::paths::create_dir_private(parent).expect("dir");
        }
        rune_core::paths::write_private(&file, "permission_mode = \"ask\"\n").expect("seed");

        save_selection(
            &paths,
            &Selection {
                provider: "anthropic".to_owned(),
                model: None,
                base_url: None,
            },
        )
        .expect("saved");

        let written = std::fs::read_to_string(&file).expect("read");
        assert!(written.contains("permission_mode"), "{written}");
        assert!(written.contains("anthropic"), "{written}");
    }

    #[test]
    fn saving_over_a_corrupt_config_refuses_rather_than_discarding_it() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        let file = paths.config_file(None);
        if let Some(parent) = file.parent() {
            rune_core::paths::create_dir_private(parent).expect("dir");
        }
        rune_core::paths::write_private(&file, "this is not = = toml").expect("seed");

        let err = save_selection(
            &paths,
            &Selection {
                provider: "anthropic".to_owned(),
                model: None,
                base_url: None,
            },
        )
        .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::CorruptRecord);
        assert!(
            std::fs::read_to_string(&file)
                .expect("read")
                .contains("not = ="),
            "the corrupt file was overwritten"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_config_file_written_by_a_selection_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        save_selection(
            &paths,
            &Selection {
                provider: "anthropic".to_owned(),
                model: None,
                base_url: None,
            },
        )
        .expect("saved");

        let mode = std::fs::metadata(paths.config_file(None))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the config file is readable by others");
    }

    #[test]
    fn a_surrounding_whitespace_is_trimmed_rather_than_stored() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        connect(&paths, "anthropic", "  sk-padded  ").expect("stored");
        let stored = auth::resolve(&paths, "anthropic", None)
            .expect("resolved")
            .expect("present");
        assert_eq!(stored.expose(), "sk-padded");
    }

    #[test]
    fn removing_a_credential_that_is_not_there_reports_so() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        assert!(!disconnect(&paths(root), "anthropic").expect("checked"));
    }

    #[test]
    fn the_connection_report_names_the_provider_and_model() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let settings = Settings {
            provider: Provider::Anthropic,
            model: "claude-test".to_owned(),
            ..Settings::default()
        };
        let rendered = render_connection(&settings, &paths(root));
        assert!(rendered.contains("anthropic"), "{rendered}");
        assert!(rendered.contains("claude-test"), "{rendered}");
        assert!(rendered.contains("credential not set"), "{rendered}");
    }

    #[test]
    fn the_connection_report_never_shows_the_credential() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let paths = paths(root);
        connect(&paths, "anthropic", "sk-super-secret-9999").expect("stored");

        let settings = Settings {
            provider: Provider::Anthropic,
            ..Settings::default()
        };
        let rendered = render_connection(&settings, &paths);
        assert!(
            !rendered.contains("sk-super-secret-9999"),
            "the report carries the secret: {rendered}"
        );
        assert!(rendered.contains("credential set"), "{rendered}");
        assert!(rendered.contains("20 characters"), "{rendered}");
    }

    #[test]
    fn every_credential_source_has_a_readable_name() {
        for source in [
            CredentialSource::ConfiguredVariable,
            CredentialSource::Environment,
            CredentialSource::SystemStore,
            CredentialSource::ProfileFile,
        ] {
            assert!(!source_name(source).is_empty());
        }
    }

    #[test]
    fn a_declared_model_appears_in_the_catalog() {
        let dir = tempfile::tempdir().expect("temp");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8");
        let _ = paths(root);
        let settings = Settings {
            provider: Provider::Anthropic,
            model: "claude-test".to_owned(),
            ..Settings::default()
        };
        let catalog = catalog_for(&settings);
        assert_eq!(catalog.models.len(), 1);
        assert!(catalog.get("claude-test").is_some());
    }

    #[test]
    fn a_catalog_with_no_model_configured_says_so() {
        let settings = Settings {
            model: String::new(),
            ..Settings::default()
        };
        let catalog = catalog_for(&settings);
        assert!(catalog.models.is_empty());
        let rendered = render_catalog(&catalog);
        assert!(rendered.contains("no models are declared"), "{rendered}");
    }

    #[test]
    fn the_rendered_catalog_names_the_configured_model() {
        let settings = Settings {
            provider: Provider::Anthropic,
            model: "claude-test".to_owned(),
            ..Settings::default()
        };
        let rendered = render_catalog(&catalog_for(&settings));
        assert!(rendered.contains("claude-test"), "{rendered}");
    }

    #[test]
    fn the_catalog_reports_where_its_entries_came_from() {
        // Reporting an endpoint that was never contacted would misdescribe
        // where the metadata came from.
        let settings = Settings {
            provider: Provider::Anthropic,
            model: "claude-test".to_owned(),
            ..Settings::default()
        };
        let catalog = catalog_for(&settings);
        assert_eq!(catalog.source, CatalogSource::Configured);
    }

    #[test]
    fn the_catalog_falls_back_to_the_compiled_context_window() {
        let settings = Settings {
            provider: Provider::Anthropic,
            model: "claude-test".to_owned(),
            ..Settings::default()
        };
        let catalog = catalog_for(&settings);
        let model = catalog.get("claude-test").expect("present");
        // Nothing declared a window, so the compiled default applies rather
        // than a zero that would make every request look too large.
        assert!(model.usable_context() > 0);
    }
}
