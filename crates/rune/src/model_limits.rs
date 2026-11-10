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
    /// The smallest request the model rejected as too long in an earlier run.
    Observed,
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
    if context.is_none() {
        context = pick(
            observed(paths, &provider_key(&settings.provider), model),
            LimitSource::Observed,
        );
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

/// File under the state root holding the observed window bounds.
const OBSERVED_LIMITS_FILE: &str = "observed-limits.json";

/// Keys the observed limits file holds; past this, the oldest is dropped.
const MAX_OBSERVED_LIMITS: usize = 256;

/// The observed limits file.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ObservedLimits {
    version: u32,
    limits: std::collections::BTreeMap<String, ObservedLimit>,
}

/// One model's observed bound.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct ObservedLimit {
    window_upper_bound: u64,
    observed_at_ms: u64,
}

fn observed_key(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

fn read_observed(paths: &Paths) -> Option<ObservedLimits> {
    let text = std::fs::read_to_string(paths.state_root.join(OBSERVED_LIMITS_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Returns the observed window bound of `model` on `provider`, when one is saved.
fn observed(paths: &Paths, provider: &str, model: &str) -> Option<u64> {
    read_observed(paths)?
        .limits
        .get(&observed_key(provider, model))
        .map(|limit| limit.window_upper_bound)
}

/// Saves that `model` on `provider` rejected a request of `tokens` as too long.
///
/// The smaller bound is kept for a model. A failure to write is ignored: the
/// bound is a hint for later runs, and this run already knows it.
pub fn record_observed(paths: &Paths, provider: &str, model: &str, tokens: u64) {
    if tokens == 0 {
        return;
    }
    let mut file = read_observed(paths).unwrap_or_default();
    file.version = 1;
    let observed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let key = observed_key(provider, model);
    let window_upper_bound = file
        .limits
        .get(&key)
        .map_or(tokens, |known| known.window_upper_bound.min(tokens));
    file.limits.insert(
        key,
        ObservedLimit {
            window_upper_bound,
            observed_at_ms,
        },
    );
    while file.limits.len() > MAX_OBSERVED_LIMITS {
        let Some(oldest) = file
            .limits
            .iter()
            .min_by_key(|(_, limit)| limit.observed_at_ms)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        file.limits.remove(&oldest);
    }
    let Ok(text) = serde_json::to_string(&file) else {
        return;
    };
    let _ = rune_core::paths::create_dir_private(&paths.state_root);
    let _ =
        rune_core::paths::write_private_atomic(&paths.state_root.join(OBSERVED_LIMITS_FILE), &text);
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
    fn an_observed_bound_is_used_only_without_a_catalog_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let paths = temp_paths(&root);
        write_catalog(&paths, CATALOG);
        record_observed(&paths, "anthropic", "m", 90_000);
        record_observed(&paths, "anthropic", "absent", 50_000);
        let settings = Settings {
            provider: Provider::Anthropic,
            model: "m".to_owned(),
            ..Settings::default()
        };
        let mut listed = rune_net::catalog::ModelMetadata::new("absent");
        listed.context_window = Some(400_000);

        let cataloged = resolve(&settings, &paths, "m", None);
        assert_eq!(cataloged.context_window, Some(300_000));
        assert_eq!(cataloged.context_source, LimitSource::Catalog);

        let observed = resolve(&settings, &paths, "absent", Some(&listed));
        assert_eq!(observed.context_window, Some(50_000));
        assert_eq!(observed.context_source, LimitSource::Observed);
        assert_eq!(
            serde_json::to_value(LimitSource::Observed).expect("json"),
            "observed"
        );

        let mut configured = settings.clone();
        configured.context_window_override = Some(64_000);
        let set = resolve(&configured, &paths, "absent", None);
        assert_eq!(set.context_window, Some(64_000));
        assert_eq!(set.context_source, LimitSource::Setting);

        // Another provider's bound for the same model name does not apply.
        let other = Settings {
            provider: Provider::ChatCompletions,
            model: "absent".to_owned(),
            ..Settings::default()
        };
        assert_eq!(resolve(&other, &paths, "absent", None).context_window, None);
    }

    #[test]
    fn observed_limits_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let paths = temp_paths(&root);
        record_observed(&paths, "local", "m", 100_000);
        record_observed(&paths, "local", "m", 120_000);
        assert_eq!(observed(&paths, "local", "m"), Some(100_000));
        record_observed(&paths, "local", "m", 85_000);
        assert_eq!(observed(&paths, "local", "m"), Some(85_000));
        assert_eq!(observed(&paths, "local", "other"), None);

        let text =
            std::fs::read_to_string(paths.state_root.join(OBSERVED_LIMITS_FILE)).expect("file");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["version"], 1);
        assert_eq!(value["limits"]["local/m"]["window_upper_bound"], 85_000);
        assert!(value["limits"]["local/m"]["observed_at_ms"].as_u64() > Some(0));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.state_root.join(OBSERVED_LIMITS_FILE))
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }

        // Past the key limit the oldest observation is dropped.
        let mut file = read_observed(&paths).expect("file");
        for n in 0..MAX_OBSERVED_LIMITS {
            file.limits.insert(
                format!("old/{n}"),
                ObservedLimit {
                    window_upper_bound: 1_000,
                    observed_at_ms: n as u64,
                },
            );
        }
        rune_core::paths::write_private_atomic(
            &paths.state_root.join(OBSERVED_LIMITS_FILE),
            &serde_json::to_string(&file).expect("json"),
        )
        .expect("write");
        record_observed(&paths, "local", "new", 70_000);
        let kept = read_observed(&paths).expect("file");
        assert_eq!(kept.limits.len(), MAX_OBSERVED_LIMITS);
        assert!(!kept.limits.contains_key("old/0"));
        assert!(!kept.limits.contains_key("old/1"));
        assert!(kept.limits.contains_key("old/2"));
        assert_eq!(observed(&paths, "local", "new"), Some(70_000));
        assert_eq!(observed(&paths, "local", "m"), Some(85_000));
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
