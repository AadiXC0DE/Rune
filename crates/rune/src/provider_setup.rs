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
use rune_net::catalog::{Catalog, ModelMetadata};

/// Endpoint used when a provider is connected without one given.
///
/// A provider whose dialect is OpenAI-compatible has no single home, so it has
/// no default: requiring the endpoint is what keeps a request from being sent
/// somewhere the user never named.
#[must_use]
pub fn default_base_url(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Anthropic => Some("https://api.anthropic.com"),
        Provider::Responses => Some("https://api.openai.com/v1"),
        // A compatible or named endpoint has no single home, so it must be
        // given rather than guessed.
        Provider::Unconfigured | Provider::ChatCompletions | Provider::Named(_) => None,
    }
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
                "credential {} ({} characters, from {})",
                "set",
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
    if !settings.model.trim().is_empty() {
        catalog
            .models
            .push(ModelMetadata::new(settings.model.clone()));
    }
    catalog
}

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
fn capability_names(model: &rune_net::catalog::ModelMetadata) -> Vec<&'static str> {
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

/// Resolves the endpoint a connection should use.
pub fn resolve_endpoint(provider: Provider, configured: Option<&str>) -> Result<String> {
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
            default_base_url(Provider::Anthropic),
            Some("https://api.anthropic.com")
        );
        assert_eq!(
            default_base_url(Provider::Responses),
            Some("https://api.openai.com/v1")
        );
    }

    #[test]
    fn an_openai_compatible_provider_has_no_default_endpoint() {
        // There is no one home for a compatible dialect, so a request must name
        // one rather than being sent somewhere the user never chose.
        assert_eq!(default_base_url(Provider::ChatCompletions), None);
        assert_eq!(default_base_url(Provider::Unconfigured), None);
    }

    #[test]
    fn resolving_without_a_default_or_a_value_names_the_remedy() {
        let err = resolve_endpoint(Provider::ChatCompletions, None).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidConfiguration);
        assert!(err.hint().is_some(), "no remedy was given");
    }

    #[test]
    fn a_configured_endpoint_wins_over_the_default() {
        let resolved =
            resolve_endpoint(Provider::Anthropic, Some("http://127.0.0.1:9/v1")).expect("resolved");
        assert_eq!(resolved, "http://127.0.0.1:9/v1");
    }

    #[test]
    fn a_malformed_endpoint_is_refused() {
        assert!(resolve_endpoint(Provider::Anthropic, Some("not a url")).is_err());
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
        let mut settings = Settings::default();
        settings.provider = Provider::Anthropic;
        settings.model = "claude-test".to_owned();

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

        let mut settings = Settings::default();
        settings.provider = Provider::Anthropic;
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
        let mut settings = Settings::default();
        settings.provider = Provider::Anthropic;
        settings.model = "claude-test".to_owned();

        let catalog = catalog_for(&settings);
        assert_eq!(catalog.models.len(), 1);
        assert!(catalog.get("claude-test").is_some());
    }

    #[test]
    fn a_catalog_with_no_model_configured_says_so() {
        let mut settings = Settings::default();
        settings.model = String::new();
        let catalog = catalog_for(&settings);
        assert!(catalog.models.is_empty());
        let rendered = render_catalog(&catalog);
        assert!(rendered.contains("no models are declared"), "{rendered}");
    }

    #[test]
    fn the_rendered_catalog_names_the_configured_model() {
        let mut settings = Settings::default();
        settings.provider = Provider::Anthropic;
        settings.model = "claude-test".to_owned();
        let rendered = render_catalog(&catalog_for(&settings));
        assert!(rendered.contains("claude-test"), "{rendered}");
    }

    #[test]
    fn the_catalog_falls_back_to_the_compiled_context_window() {
        let mut settings = Settings::default();
        settings.provider = Provider::Anthropic;
        settings.model = "claude-test".to_owned();
        let catalog = catalog_for(&settings);
        let model = catalog.get("claude-test").expect("present");
        // Nothing declared a window, so the compiled default applies rather
        // than a zero that would make every request look too large.
        assert!(model.usable_context() > 0);
    }
}
