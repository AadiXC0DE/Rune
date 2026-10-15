//! Provider preflight diagnostics, without a completion or conversation.

use std::fmt::Write as _;
use std::time::Duration;

use rune_core::config::Settings;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;
use rune_net::catalog::DEFAULT_CONTEXT_WINDOW;
use rune_net::provider::{ProviderCapabilities, Routing, ToolStreaming};
use serde::Serialize;

use crate::{cli::Launch, provider_setup};

/// Reported capabilities describe the dialect, not undisclosed model restrictions.
#[derive(Debug, Serialize)]
pub struct Report {
    provider: String,
    model: String,
    dialect: &'static str,
    scope: &'static str,
    capabilities: ProviderCapabilities,
    routing: bool,
    context_window: u64,
    context_source: String,
    valid: bool,
    selection_error: Option<String>,
}

/// Keeps a failed selection reviewable on stdout while reporting failure on stderr.
pub struct Check {
    pub report: Report,
    pub error: Option<RuneError>,
}

/// Checks controls first, then resolves the effective context window and its source.
pub fn check(settings: &Settings, paths: &Paths, launch: &Launch) -> Result<Check> {
    settings.require_model()?;
    let dialect = provider_setup::dialect_for(&settings.provider);
    let capabilities = dialect.capabilities();
    let error = dialect
        .validate(&provider_setup::selection_plan(settings))
        .and_then(|()| {
            for (required, supported, field) in [
                (
                    launch.has_flag("--require-images"),
                    capabilities.images,
                    "images",
                ),
                (
                    launch.has_flag("--require-tool-streaming"),
                    capabilities.tool_streaming == ToolStreaming::Incremental,
                    "incremental tool streaming",
                ),
            ] {
                if required && !supported {
                    return Err(RuneError::new(
                        ErrorCode::Unsupported,
                        format!("the `{}` dialect does not support {field}", dialect.name()),
                    ));
                }
            }
            Ok(())
        })
        .err();
    // Invalid controls never need a network lookup to be rejected.
    let (context_window, context_source) = context(settings, paths, error.is_none());
    Ok(Check {
        report: Report {
            provider: settings.provider.to_string(),
            model: settings.model.clone(),
            dialect: dialect.name(),
            scope: "dialect",
            capabilities,
            routing: dialect.routing() == Routing::Supported,
            context_window,
            context_source,
            valid: error.is_none(),
            selection_error: error.as_ref().map(ToString::to_string),
        },
        error,
    })
}

fn context(settings: &Settings, paths: &Paths, allow_lookup: bool) -> (u64, String) {
    if let Some(window) = settings.context_window {
        return (
            window,
            format!("configured:{}", settings.source_of("context_window")),
        );
    }
    // Match session discovery: endpoint metadata wins over published capacity.
    // Offline diagnostics can still consult the private published catalog cache.
    let catalog = if !settings.offline && allow_lookup {
        provider_setup::fetch_catalog(settings, paths, Duration::from_secs(3)).ok()
    } else {
        None
    }
    .unwrap_or_else(|| {
        let mut offline = settings.clone();
        offline.offline = true;
        let mut catalog = provider_setup::catalog_for(&offline);
        provider_setup::enrich_with_capacity(&offline, paths, &mut catalog);
        catalog
    });
    if let Some(model) = catalog.get(&settings.model)
        && let Some(window) = model.context_window.filter(|window| *window > 0)
    {
        return (
            window,
            model
                .context_source
                .clone()
                .unwrap_or_else(|| "endpoint:context_window".to_owned()),
        );
    }
    (DEFAULT_CONTEXT_WINDOW, "compiled_default".to_owned())
}

/// Renders the same fields available in the JSON report.
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "provider       {}", report.provider);
    let _ = writeln!(out, "model          {}", report.model);
    let _ = writeln!(
        out,
        "dialect        {} (dialect capabilities)",
        report.dialect
    );
    let efforts: Vec<_> = report
        .capabilities
        .efforts
        .iter()
        .map(|effort| effort.as_str())
        .collect();
    let _ = writeln!(out, "effort         {}", efforts.join(", "));
    let _ = writeln!(out, "images         {}", report.capabilities.images);
    let streaming = match report.capabilities.tool_streaming {
        ToolStreaming::Unsupported => "unsupported",
        ToolStreaming::Incremental => "incremental",
        ToolStreaming::CompleteCalls => "complete calls in stream frames",
    };
    let _ = writeln!(out, "tool streaming {streaming}");
    let _ = writeln!(out, "fast mode      {}", report.capabilities.fast_mode);
    let _ = writeln!(out, "routing        {}", report.routing);
    let _ = writeln!(out, "context        {} tokens", report.context_window);
    let _ = writeln!(out, "context source {}", report.context_source);
    let _ = writeln!(
        out,
        "selection      {}",
        if report.valid { "valid" } else { "rejected" }
    );
    out
}
