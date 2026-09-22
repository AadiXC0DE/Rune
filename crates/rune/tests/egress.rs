//! Egress auditing.
//!
//! Two properties are asserted here. Offline mode produces no request at all,
//! and every outbound call in the workspace goes through one module, so reading
//! that module answers what the product can send.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rune_core::config::{Provider, Settings};
use rune_core::tool::ToolSpec;
use rune_net::provider::{Provider as Dialect, RequestPlan};
use rune_net::transport::Endpoint;

/// A listener that counts the connections it accepts.
struct Counter {
    port: u16,
    accepts: Arc<AtomicUsize>,
    _handle: std::thread::JoinHandle<()>,
}

impl Counter {
    /// Starts a listener on a free port that counts connections and answers
    /// nothing, so a client that reaches it fails rather than hangs.
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&accepts);
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stream.is_err() {
                    break;
                }
                counted.fetch_add(1, Ordering::SeqCst);
                // Close immediately; the count is the measurement.
                drop(stream);
            }
        });
        Self {
            port,
            accepts,
            _handle: handle,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn count(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }
}

/// Sends one request through the transport and reports whether it got through.
fn send(endpoint: &Endpoint, dialect: &dyn Dialect) -> Result<(), rune_core::error::RuneError> {
    let plan = RequestPlan {
        model: "test/model".to_owned(),
        instructions: String::new(),
        messages: rune_net::transport::one_shot_messages("hello"),
        tools: Vec::<ToolSpec>::new(),
        tool_choice: rune_net::provider::ToolChoice::Auto,
        parallel_tool_calls: false,
        effort: rune_core::config::Effort::Auto,
        fast_mode: false,
        max_output_tokens: None,
        provider_order: Vec::new(),
        provider_strict: false,
    };
    rune_net::transport::stream_completion(
        &rune_net::transport::agent(),
        endpoint,
        dialect,
        &plan,
        std::time::Duration::from_secs(2),
        &|| false,
    )
    .map(|_| ())
    .map_err(|err| {
        rune_core::error::RuneError::new(
            rune_core::error::ErrorCode::TransportFailure,
            err.message().to_owned(),
        )
    })
}

/// Waits briefly so a connection that was going to happen has happened.
fn settle() {
    std::thread::sleep(std::time::Duration::from_millis(150));
}

#[test]
fn offline_mode_produces_no_connection() {
    let counter = Counter::start();
    let endpoint = Endpoint::new(counter.url(), "test-key").offline(true);
    let err =
        send(&endpoint, &rune_net::chat_completions::ChatCompletions).expect_err("offline refuses");
    assert_eq!(err.code(), rune_core::error::ErrorCode::TransportFailure);
    settle();
    assert_eq!(
        counter.count(),
        0,
        "offline mode opened {} connection(s)",
        counter.count()
    );
}

#[test]
fn an_online_transport_reaches_its_endpoint() {
    // The control: without it, a transport that never connects at all would
    // pass the test above.
    let counter = Counter::start();
    let endpoint = Endpoint::new(counter.url(), "test-key");
    let _ = send(&endpoint, &rune_net::chat_completions::ChatCompletions);
    settle();
    assert!(
        counter.count() >= 1,
        "an online request opened no connection, so the offline assertion proves nothing"
    );
}

#[test]
fn a_settings_value_carries_offline_into_the_endpoint() {
    // The flag is only useful if it reaches the transport, so this asserts the
    // path from configuration to the endpoint the caller builds.
    let settings = Settings {
        provider: Provider::ChatCompletions,
        model: "test/model".to_owned(),
        base_url: Some("http://127.0.0.1:1/v1".to_owned()),
        offline: true,
        ..Settings::default()
    };
    assert!(settings.offline);

    let endpoint = Endpoint::new(
        settings.base_url.clone().expect("url"),
        "test-key".to_owned(),
    )
    .offline(settings.offline);
    assert!(
        endpoint.offline,
        "the offline setting did not reach the endpoint"
    );
}

#[test]
fn no_crate_constructs_a_network_client_outside_the_transport() {
    // One module owns egress, which is what makes an audit a single read rather
    // than a search. A client built anywhere else would be invisible to it.
    let root = workspace_root();
    let transport = root.join("crates/rune-net/src/transport.rs");
    let mut offenders = Vec::new();

    for crate_entry in std::fs::read_dir(root.join("crates"))
        .expect("crates")
        .flatten()
    {
        let name = crate_entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("rune") {
            continue;
        }
        let src = crate_entry.path().join("src");
        let Ok(entries) = std::fs::read_dir(&src) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            if path == transport {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let shipped = text.split("#[cfg(test)]").next().unwrap_or(&text);
            for needle in [
                "ureq::Agent::config_builder",
                "ureq::agent()",
                "std::net::TcpStream",
            ] {
                if shipped.contains(needle) {
                    offenders.push(format!("{}: {needle}", path.display()));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these files construct a client outside the transport module: {offenders:#?}"
    );
}

#[test]
fn the_transport_module_is_where_egress_lives() {
    // A guard on the guard: if the audited module were emptied or renamed, the
    // test above would pass vacuously.
    let transport = workspace_root().join("crates/rune-net/src/transport.rs");
    let text = std::fs::read_to_string(&transport).expect("transport");
    assert!(
        text.contains("ureq::"),
        "the audited transport module no longer holds the client, so the egress \
         audit is checking nothing"
    );
}

/// Resolves the workspace root from this test's manifest directory.
fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn a_settings_default_is_online() {
    // Offline is opt in; a product that refused every request by default would
    // be unusable and the flag would say nothing.
    assert!(!Settings::default().offline);
}
