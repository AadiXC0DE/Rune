//! Outbound clients for the web tools.
//!
//! The tools crate defines what a fetch and a search are and refuses to depend
//! on the transport, so the clients that actually reach the network are built
//! here, where both crates are visible. Every request goes through
//! `rune_net::transport`, which is the one place that opens a connection and
//! where the address, scheme, and credential refusals live.

use std::sync::Arc;
use std::time::Duration;

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_tools::inventory::WebBackends;
use rune_tools::web::{FetchBackend, Fetched, SearchBackend, SearchFilters, SearchResult};

/// Longest a search may take.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

/// The endpoint used for a keyless search.
///
/// A hosted search API needs an account and a key before the tool can be used at
/// all, which is a tool most people never enable. This endpoint answers an HTML
/// results page and needs neither.
const SEARCH_ENDPOINT: &str = "https://lite.duckduckgo.com/lite/";

/// Fetches a URL over the network.
#[derive(Debug, Default)]
pub struct NetworkFetch;

impl FetchBackend for NetworkFetch {
    fn get(&self, url: &str, timeout: Duration) -> Result<Fetched> {
        let fetched = rune_net::transport::fetch_url(
            url,
            "text/html,application/json,text/plain;q=0.9,*/*;q=0.8",
            timeout,
        )
        .map_err(|err| err.to_rune_error())?;
        Ok(Fetched {
            status: fetched.status,
            content_type: fetched.content_type,
            body: fetched.body,
            // The transport follows redirects itself, so the chain is not
            // observable from here and is reported as empty rather than made up.
            redirects: Vec::new(),
        })
    }
}

/// Searches the web through an HTML results endpoint.
#[derive(Debug)]
pub struct NetworkSearch {
    endpoint: String,
    timeout: Duration,
}

impl Default for NetworkSearch {
    fn default() -> Self {
        Self {
            endpoint: SEARCH_ENDPOINT.to_owned(),
            timeout: SEARCH_TIMEOUT,
        }
    }
}

impl SearchBackend for NetworkSearch {
    fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        max: usize,
    ) -> Result<Vec<SearchResult>> {
        let page = rune_net::transport::search_html(query, &self.endpoint, self.timeout)
            .map_err(|err| err.to_rune_error())?;
        let results = rune_tools::web::parse_results(&page, filters, max);
        if results.is_empty() {
            // An empty page and a refused request look the same to a caller, so
            // the distinction is stated rather than reported as success.
            return Err(RuneError::new(
                ErrorCode::NotFound,
                format!("no results were found for `{query}`"),
            )
            .with_hint("try fewer or more general terms"));
        }
        Ok(results)
    }
}

/// Builds the clients the web tools run on, when the run may reach the network.
///
/// Returns `None` when the run is offline or the web tools are not enabled, so
/// the registry keeps both tools refusing every call.
#[must_use]
pub fn backends(settings: &rune_core::config::Settings) -> Option<WebBackends> {
    if settings.offline || !settings.web_tools {
        return None;
    }
    Some(WebBackends {
        fetch: Arc::new(NetworkFetch),
        search: Arc::new(NetworkSearch::default()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::config::Settings;

    #[test]
    fn an_offline_run_gets_no_clients() {
        let settings = Settings {
            web_tools: true,
            offline: true,
            ..Settings::default()
        };
        assert!(backends(&settings).is_none(), "offline reached the network");
    }

    #[test]
    fn a_run_without_the_web_tools_gets_no_clients() {
        // The setting is the switch: a default run must not be able to fetch,
        // whatever the policy layer would have said.
        let settings = Settings {
            web_tools: false,
            offline: false,
            ..Settings::default()
        };
        assert!(backends(&settings).is_none());
    }

    #[test]
    fn an_enabled_run_gets_both_clients() {
        let settings = Settings {
            web_tools: true,
            offline: false,
            ..Settings::default()
        };
        let built = backends(&settings).expect("clients");
        // Both are present, so neither tool is left refusing.
        let _ = &built.fetch;
        let _ = &built.search;
    }

    #[test]
    fn a_search_query_is_escaped() {
        assert_eq!(
            rune_net::transport::percent_encode_query("a b&c#d"),
            "a+b%26c%23d"
        );
    }

    #[test]
    #[ignore = "reaches the network; run with --ignored to check the real endpoint"]
    fn a_live_search_returns_results() {
        // The parser is tested against a recorded page in the tools crate, which
        // cannot catch the endpoint changing its markup. This reaches the real
        // one, and is ignored by default because the suite must not need the
        // network.
        let backend = NetworkSearch::default();
        let results = backend
            .search("rust programming language", &SearchFilters::default(), 3)
            .expect("the live search ran");
        assert!(!results.is_empty());
        for result in &results {
            println!("{} | {}", result.title, result.url);
        }
    }
}
