//! The context window and output ceiling of a model, from the best source known.
//!
//! The same answer is needed by the interactive session, `ask`, the model
//! picker and delegated children, however Rune was configured, including with
//! environment variables alone. Each limit is taken from the first source that
//! states it, and a limit no source states is unknown rather than guessed: a
//! guessed window either truncates a conversation that would have fit or lets
//! one grow past what the model accepts.

use rune_core::config::{ModelEntry, Settings, provider_key};
use rune_core::paths::Paths;

/// Where a limit came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    /// `RUNE_CONTEXT_WINDOW` or `RUNE_MAX_OUTPUT_TOKENS`.
    Setting,
    /// The `[models.<provider>]` entry for the model.
    ConfiguredModel,
    /// The cached published catalog.
    Catalog,
    /// The endpoint's own model listing.
    ProviderListing,
    /// No source states it.
    Unknown,
}

/// A model's limits and where each came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModelLimits {
    /// Input capacity in tokens, when known.
    pub context_window: Option<u64>,
    /// Where `context_window` came from.
    pub context_source: LimitSource,
    /// Largest answer in tokens, when known.
    pub max_output: Option<u64>,
    /// Where `max_output` came from.
    pub output_source: LimitSource,
}

/// Resolves the limits of `model` on the configured provider.
///
/// Never reaches the network: the catalog is read from the cache only, and an
/// endpoint listing is used only when the caller already holds one.
pub fn resolve(
    settings: &Settings,
    paths: &Paths,
    model: &str,
    listing: Option<&rune_net::catalog::ModelMetadata>,
) -> ModelLimits {
    let configured = configured_entry(settings, model);
    let mut context = pick(settings.context_window_override, LimitSource::Setting).or_else(|| {
        let declared = configured.and_then(ModelEntry::context_window).or_else(|| {
            (model == settings.model)
                .then_some(settings.context_window)
                .flatten()
        });
        pick(declared, LimitSource::ConfiguredModel)
    });
    let mut output = pick(settings.max_output_override, LimitSource::Setting).or_else(|| {
        let declared = configured
            .and_then(ModelEntry::max_output_tokens)
            .or_else(|| {
                (model == settings.model)
                    .then(|| settings.model_output_limit())
                    .flatten()
            });
        pick(declared, LimitSource::ConfiguredModel)
    });

    if (context.is_none() || output.is_none())
        && let Some(cached) = cached(settings, paths, model)
    {
        context = context.or_else(|| pick(cached.context, LimitSource::Catalog));
        output = output.or_else(|| pick(cached.output, LimitSource::Catalog));
    }
    if let Some(listed) = listing {
        context = context.or_else(|| pick(listed.context_window, LimitSource::ProviderListing));
        output = output.or_else(|| pick(listed.max_output_tokens, LimitSource::ProviderListing));
    }

    let (context_window, context_source) = context
        .map_or((None, LimitSource::Unknown), |(value, source)| {
            (Some(value), source)
        });
    let (max_output, output_source) = output
        .map_or((None, LimitSource::Unknown), |(value, source)| {
            (Some(value), source)
        });
    ModelLimits {
        context_window,
        context_source,
        max_output,
        output_source,
    }
}

/// Keeps a value only when it is positive, with its source.
fn pick(value: Option<u64>, source: LimitSource) -> Option<(u64, LimitSource)> {
    value
        .filter(|value| *value > 0)
        .map(|value| (value, source))
}

/// Returns the configuration entry that describes `model` on this provider.
///
/// The provider's own entry is consulted, then the fallback entry, as when
/// the model is selected; an entry for another provider describes a model that
/// endpoint serves, not this one.
fn configured_entry<'a>(settings: &'a Settings, model: &str) -> Option<&'a ModelEntry> {
    settings
        .configured_models
        .get(&provider_key(&settings.provider))
        .or_else(|| settings.configured_models.get("default"))
        .filter(|entry| entry.id() == model)
}

/// Returns the cached catalog record for `model` on this provider.
fn cached(settings: &Settings, paths: &Paths, model: &str) -> Option<rune_net::models_dev::Limits> {
    let key = rune_net::models_dev::catalog_key(&settings.provider.to_string())?;
    crate::provider_setup::read_cached_limits(&paths.models_cache_file(), key)?.get(model)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use rune_core::config::{ModelSettings, Provider};

    fn temp_paths(root: &camino::Utf8Path) -> Paths {
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

    fn write_catalog(paths: &Paths, body: &str) {
        let path = paths.models_cache_file();
        if let Some(parent) = path.parent() {
            rune_core::paths::create_dir_private(parent).expect("cache directory");
        }
        rune_core::paths::write_private_atomic(&path, body).expect("catalog");
    }

    const CATALOG: &str = r#"{
        "anthropic": { "models": {
            "claude-sonnet-5-5": { "limit": { "context": 1000000, "output": 128000 } },
            "m": { "limit": { "context": 300000, "output": 30000 } },
            "zero": { "limit": { "context": 0, "output": 0 } }
        } }
    }"#;

    fn entry(context: Option<u64>, output: Option<u64>) -> ModelEntry {
        ModelEntry::Detailed(Box::new(ModelSettings {
            id: "m".to_owned(),
            context_window: context,
            max_output_tokens: output,
            prices: None,
        }))
    }

    #[test]
    fn the_first_known_source_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let with_catalog = temp_paths(&root.join("cached"));
        write_catalog(&with_catalog, CATALOG);
        let without_catalog = temp_paths(&root.join("empty"));
        let mut listed = rune_net::catalog::ModelMetadata::new("m");
        listed.context_window = Some(400_000);
        listed.max_output_tokens = Some(40_000);

        // (setting, configured, catalog, listing) => expected window and source.
        let table: [(bool, bool, bool, bool, Option<u64>, LimitSource); 5] = [
            (true, true, true, true, Some(64_000), LimitSource::Setting),
            (
                false,
                true,
                true,
                true,
                Some(200_000),
                LimitSource::ConfiguredModel,
            ),
            (
                false,
                false,
                true,
                true,
                Some(300_000),
                LimitSource::Catalog,
            ),
            (
                false,
                false,
                false,
                true,
                Some(400_000),
                LimitSource::ProviderListing,
            ),
            (false, false, false, false, None, LimitSource::Unknown),
        ];
        for (setting, configured, catalog, listing, window, source) in table {
            let mut settings = Settings {
                provider: Provider::Anthropic,
                model: "m".to_owned(),
                ..Settings::default()
            };
            if setting {
                settings.context_window_override = Some(64_000);
                settings.max_output_override = Some(6_400);
            }
            if configured {
                settings
                    .configured_models
                    .insert("anthropic".to_owned(), entry(Some(200_000), Some(20_000)));
            }
            let paths = if catalog {
                &with_catalog
            } else {
                &without_catalog
            };
            let limits = resolve(&settings, paths, "m", listing.then_some(&listed));
            assert_eq!(limits.context_window, window, "{limits:?}");
            assert_eq!(limits.context_source, source, "{limits:?}");
            assert_eq!(limits.max_output, window.map(|w| w / 10), "{limits:?}");
            assert_eq!(limits.output_source, source, "{limits:?}");
        }
    }

    #[test]
    fn each_limit_takes_its_own_first_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let paths = temp_paths(&root);
        write_catalog(&paths, CATALOG);
        let mut settings = Settings {
            provider: Provider::Anthropic,
            model: "m".to_owned(),
            context_window_override: Some(64_000),
            ..Settings::default()
        };
        settings
            .configured_models
            .insert("anthropic".to_owned(), entry(Some(200_000), None));
        let limits = resolve(&settings, &paths, "m", None);
        assert_eq!(limits.context_window, Some(64_000));
        assert_eq!(limits.context_source, LimitSource::Setting);
        assert_eq!(limits.max_output, Some(30_000));
        assert_eq!(limits.output_source, LimitSource::Catalog);
    }

    #[test]
    fn an_env_only_setup_reads_the_cached_catalog() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let paths = temp_paths(&root);
        write_catalog(&paths, CATALOG);
        let env = rune_core::config::EnvironmentOverrides::from_lookup(|key| match key {
            "RUNE_PROVIDER" => Some("anthropic".to_owned()),
            "RUNE_MODEL" => Some("claude-sonnet-5-5".to_owned()),
            _ => None,
        });
        let settings = rune_core::config::load(None, None, &env);
        let limits = resolve(&settings, &paths, &settings.model, None);
        assert_eq!(limits.context_window, Some(1_000_000));
        assert_eq!(limits.context_source, LimitSource::Catalog);
        assert_eq!(limits.max_output, Some(128_000));
        assert_eq!(limits.output_source, LimitSource::Catalog);
    }

    #[test]
    fn an_unknown_model_is_unknown_not_guessed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let paths = temp_paths(&root);
        write_catalog(&paths, CATALOG);
        let unknown = ModelLimits {
            context_window: None,
            context_source: LimitSource::Unknown,
            max_output: None,
            output_source: LimitSource::Unknown,
        };
        let anthropic = Settings {
            provider: Provider::Anthropic,
            model: "absent".to_owned(),
            ..Settings::default()
        };
        assert_eq!(resolve(&anthropic, &paths, "absent", None), unknown);
        // A zero in the catalog states nothing.
        assert_eq!(resolve(&anthropic, &paths, "zero", None), unknown);
        // A compatible endpoint is described by no catalog.
        let local = Settings {
            provider: Provider::ChatCompletions,
            model: "scripted".to_owned(),
            ..Settings::default()
        };
        assert_eq!(resolve(&local, &paths, "scripted", None), unknown);
    }
}
