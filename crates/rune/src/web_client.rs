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
        self.get_with_private_access(url, timeout, false)
    }

    fn get_with_private_access(
        &self,
        url: &str,
        timeout: Duration,
        allow_private: bool,
    ) -> Result<Fetched> {
        // One hop only: the tool follows a redirect itself, after checking
        // where it points. The transport pins the vetted DNS result as well.
        let fetched = rune_net::transport::fetch_hop_checked(
            url,
            "text/html,application/json,text/plain;q=0.9,*/*;q=0.8",
            timeout,
            |address| {
                if !allow_private && rune_tools::web::is_local_host(&address.to_string()) {
                    return Err(RuneError::new(
                        ErrorCode::PermissionDenied,
                        format!(
                            "the destination resolves to `{address}`, a loopback, private, or link-local address"
                        ),
                    )
                    .with_hint("pass allow_private: true to reach an address on the local network"));
                }
                Ok(())
            },
        )?;
        Ok(Fetched {
            status: fetched.status,
            content_type: fetched.content_type,
            body: fetched.body,
            location: fetched.location,
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
    use std::io::{BufRead as _, Write as _};

    use camino::Utf8Path;
    use rune_core::config::{EnvironmentOverrides, Settings, load};
    use rune_policy::decision::Outcome;
    use rune_tools::contract::ExecutionContext;

    #[test]
    fn a_hostname_resolving_to_loopback_is_refused_before_http() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("address").port();
        let error = NetworkFetch
            .get(
                &format!("http://localhost:{port}/fixture"),
                Duration::from_secs(2),
            )
            .expect_err("resolved loopback must be refused");
        assert_eq!(error.code(), ErrorCode::PermissionDenied);
        assert!(error.message().contains("resolves to"), "{error}");
        assert_eq!(
            listener.accept().expect_err("no connection").kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn a_fixture_fetch_requires_an_explicit_web_opt_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Utf8Path::from_path(dir.path()).expect("utf8 path");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        listener.set_nonblocking(true).expect("nonblocking fixture");
        let url = format!("http://{}/fixture", listener.local_addr().expect("address"));
        // The private-address opt-in permits this local fixture in both calls.
        let arguments = serde_json::json!({ "url": url, "allow_private": true });
        let context = ExecutionContext::new(workspace.to_owned());
        let env = EnvironmentOverrides::default();
        let settings = load(None, None, &env);
        let rules = crate::permissions::validated(&settings).expect("rules");
        assert_eq!(
            rules
                .evaluate("web_fetch", "domain:127.0.0.1", Outcome::Allow)
                .outcome,
            Outcome::Deny
        );
        assert!(backends(&settings).is_none());
        let registry = rune_tools::inventory::builtin_with_web(
            &rune_tools::workspace::FileLimits::default(),
            &settings.limits,
            workspace,
            backends(&settings),
        )
        .expect("registry");
        // Even a caller that already permitted the tool cannot bypass the setting.
        let refused = registry
            .call("web_fetch", &arguments, &context)
            .expect("call");
        assert!(refused.is_error, "{refused:?}");
        assert!(
            refused.text.contains("no network access is configured"),
            "{refused:?}"
        );
        assert_eq!(
            listener
                .accept()
                .expect_err("default opened no connection")
                .kind(),
            std::io::ErrorKind::WouldBlock
        );

        let user = workspace.join("config.toml");
        std::fs::write(
            &user,
            "web_tools = true\n[limits]\nweb_fetch_timeout_ms = 2000\n",
        )
        .expect("write opt-in");
        let settings = load(None, Some(&user), &env);
        assert!(
            settings.diagnostics.is_empty(),
            "{:?}",
            settings.diagnostics
        );
        let rules = crate::permissions::validated(&settings).expect("rules");
        assert_eq!(
            rules
                .evaluate("web_fetch", "domain:127.0.0.1", Outcome::Deny)
                .outcome,
            Outcome::Allow
        );
        let registry = rune_tools::inventory::builtin_with_web(
            &rune_tools::workspace::FileLimits::default(),
            &settings.limits,
            workspace,
            backends(&settings),
        )
        .expect("registry");
        let fixture = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "fixture was not called"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .expect("write timeout");
            let mut reader = std::io::BufReader::new(&stream);
            let mut request = String::new();
            reader.read_line(&mut request).expect("request line");
            loop {
                let mut header = String::new();
                assert!(
                    reader.read_line(&mut header).expect("header") > 0,
                    "incomplete request"
                );
                if header == "\r\n" {
                    break;
                }
            }
            let body = "R018_WEB_OK\n";
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                .expect("fixture response");
            request
        });
        let fetched = registry
            .call("web_fetch", &arguments, &context)
            .expect("call");
        let request = fixture.join().expect("fixture thread");
        assert_eq!(request, "GET /fixture HTTP/1.1\r\n");
        assert!(!fetched.is_error, "{fetched:?}");
        assert!(fetched.text.contains("R018_WEB_OK"), "{fetched:?}");
    }

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
