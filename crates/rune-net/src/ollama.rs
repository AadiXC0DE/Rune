//! Local Ollama discovery. Inference uses the existing Chat Completions dialect.
//!
//! `/api/tags` supplies identifiers and `/api/show` supplies model windows.
//! Only the selected endpoint is contacted; published catalogs are not used.

use std::io::Read as _;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::catalog::{Catalog, ModelMetadata};
use crate::error::{FailureKind, NetError, NetResult};
use crate::fetch::{Fetch, FetchRequest};
use crate::transport::Endpoint;

const MAX_BODY_BYTES: u64 = 1024 * 1024;

/// Imports a catalog from an Ollama endpoint whose base URL ends in `/v1`.
///
/// The listing and all detail requests share one timeout budget. Missing
/// capacity stays unknown and carries an explicit compiled-default source.
pub fn fetch_catalog(
    client: &dyn Fetch,
    endpoint: &Endpoint,
    timeout: Duration,
) -> NetResult<Catalog> {
    if endpoint.offline {
        return Err(NetError::new(
            FailureKind::PolicyDenied,
            "outbound requests are disabled",
        ));
    }
    endpoint
        .validate()
        .map_err(|err| NetError::new(FailureKind::InvalidRequest, err.message().to_owned()))?;
    let base = endpoint.base_url.trim_end_matches('/');
    let root = base.strip_suffix("/v1").ok_or_else(|| {
        NetError::new(
            FailureKind::InvalidRequest,
            "the Ollama endpoint must end in `/v1`",
        )
    })?;
    let started = Instant::now();
    let tags = request_json(
        client,
        endpoint,
        FetchRequest::get(format!("{root}/api/tags")),
        started,
        timeout,
    )?;
    let entries = tags
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            NetError::new(
                FailureKind::ProtocolViolation,
                "Ollama's model list must contain a `models` array",
            )
        })?;
    let mut catalog = Catalog::new("ollama");
    catalog.endpoint = Some(endpoint.base_url.clone());
    for entry in entries {
        let id = entry
            .get("name")
            .or_else(|| entry.get("model"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                NetError::new(
                    FailureKind::ProtocolViolation,
                    "an Ollama model has no identifier",
                )
            })?;
        let body = serde_json::to_vec(&serde_json::json!({"model": id})).map_err(|err| {
            NetError::new(
                FailureKind::InvalidRequest,
                format!("Ollama request: {err}"),
            )
        })?;
        let show = request_json(
            client,
            endpoint,
            FetchRequest::post(format!("{root}/api/show"), body)
                .with_header("content-type", "application/json"),
            started,
            timeout,
        )?;
        if !show.is_object() {
            return Err(NetError::new(
                FailureKind::ProtocolViolation,
                "Ollama's model details must be an object",
            ));
        }
        let mut model = ModelMetadata::new(id);
        let (window, source) = context_window(&show);
        model.context_window = window;
        model.context_source = Some(source);
        catalog.models.push(model);
    }
    Ok(catalog)
}

fn request_json(
    client: &dyn Fetch,
    endpoint: &Endpoint,
    request: FetchRequest,
    started: Instant,
    timeout: Duration,
) -> NetResult<Value> {
    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(NetError::new(
            FailureKind::Timeout,
            "Ollama discovery timed out",
        ));
    }
    let mut request = endpoint.authenticate(
        request
            .with_header("accept", "application/json")
            .with_timeout(Some(remaining))
            .with_redirects(false),
    )?;
    for (name, value) in &endpoint.headers {
        request = request.with_header(name, value);
    }
    let response = client.send(request)?;
    let mut body = String::new();
    response
        .body
        .take(MAX_BODY_BYTES.saturating_add(1))
        .read_to_string(&mut body)
        .map_err(|err| NetError::new(FailureKind::Decode, format!("Ollama response: {err}")))?;
    if body.len() as u64 > MAX_BODY_BYTES {
        return Err(NetError::new(
            FailureKind::ProtocolViolation,
            "Ollama discovery response exceeds one MiB",
        ));
    }
    if response.status >= 400 {
        return Err(NetError::classify_status(
            response.status,
            &crate::redact::redact(&body),
        ));
    }
    if !(200..300).contains(&response.status) {
        return Err(NetError::new(
            FailureKind::ProtocolViolation,
            "Ollama discovery requires a successful response without redirects",
        ));
    }
    serde_json::from_str(&body).map_err(|err| {
        NetError::new(
            FailureKind::ProtocolViolation,
            format!("Ollama JSON: {err}"),
        )
    })
}

fn context_window(show: &Value) -> (Option<u64>, String) {
    // A model's explicit num_ctx takes precedence over its architecture limit.
    if let Some(parameters) = show.get("parameters").and_then(Value::as_str) {
        for line in parameters.lines() {
            let mut fields = line.split_whitespace();
            if fields.next() == Some("num_ctx")
                && let Some(window) = fields.next().and_then(|value| value.parse::<u64>().ok())
                && window > 0
                && fields.next().is_none()
            {
                return (
                    Some(window),
                    "ollama:/api/show:parameters.num_ctx".to_owned(),
                );
            }
        }
    }
    if let Some(info) = show.get("model_info").and_then(Value::as_object)
        && let Some(architecture) = info.get("general.architecture").and_then(Value::as_str)
    {
        let key = format!("{architecture}.context_length");
        if let Some(window) = info.get(&key).and_then(Value::as_u64).filter(|n| *n > 0) {
            return (Some(window), format!("ollama:/api/show:model_info.{key}"));
        }
    }
    (
        None,
        "compiled_default (Ollama did not report a window)".to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::fetch::{FetchResponse, Method};

    struct Fixture {
        responses: Mutex<std::collections::VecDeque<(u16, String)>>,
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl Fixture {
        fn new(responses: &[(u16, &str)]) -> Self {
            Self {
                responses: Mutex::new(
                    responses
                        .iter()
                        .map(|(status, body)| (*status, (*body).to_owned()))
                        .collect(),
                ),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl Fetch for Fixture {
        fn send(&self, request: FetchRequest) -> NetResult<FetchResponse> {
            // The only allowed destination is the explicitly selected server.
            assert!(request.url.starts_with("http://127.0.0.1:11434/proxy/api/"));
            assert!(!request.follow_redirects);
            assert!(
                request
                    .timeout
                    .is_some_and(|timeout| timeout <= Duration::from_secs(5))
            );
            assert!(
                !request
                    .headers
                    .iter()
                    .any(|(name, _)| name == "authorization")
            );
            self.requests.lock().expect("requests").push(request);
            let (status, body) = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .expect("unexpected request");
            Ok(FetchResponse {
                status,
                content_type: "application/json".to_owned(),
                retry_after: None,
                location: None,
                body: Box::new(std::io::Cursor::new(body.into_bytes())),
            })
        }
    }

    fn endpoint() -> Endpoint {
        Endpoint::anonymous("http://127.0.0.1:11434/proxy/v1/").expect("local endpoint")
    }

    #[test]
    fn imports_two_tagged_models_with_endpoint_and_context_provenance() {
        let fixture = Fixture::new(&[
            (
                200,
                r#"{"models":[{"name":"llama:latest"},{"model":"team/qwen:8b"}]}"#,
            ),
            (
                200,
                r#"{"model_info":{"general.architecture":"llama","llama.context_length":8192}}"#,
            ),
            (
                200,
                r#"{"parameters":"stop \"num_ctx 3\"\nnum_ctx 32768\n", "model_info":{"general.architecture":"qwen","qwen.context_length":131072}}"#,
            ),
        ]);
        let catalog = fetch_catalog(&fixture, &endpoint(), Duration::from_secs(5)).expect("import");
        assert_eq!(catalog.source, crate::catalog::CatalogSource::Endpoint);
        assert_eq!(
            catalog.endpoint.as_deref(),
            Some(endpoint().base_url.as_str())
        );
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(catalog.models[0].id, "llama:latest");
        assert_eq!(catalog.models[0].usable_context(), 8192);
        assert_eq!(
            catalog.models[0].context_source.as_deref(),
            Some("ollama:/api/show:model_info.llama.context_length")
        );
        assert_eq!(catalog.models[1].id, "team/qwen:8b");
        assert_eq!(catalog.models[1].usable_context(), 32768);
        assert_eq!(
            catalog.models[1].context_source.as_deref(),
            Some("ollama:/api/show:parameters.num_ctx")
        );
        assert!(crate::models_dev::catalog_key("ollama").is_none());
        let requests = fixture.requests.lock().expect("requests");
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].method, Method::Get);
        assert!(requests[0].url.ends_with("/proxy/api/tags"));
        for (request, model) in requests
            .iter()
            .skip(1)
            .zip(["llama:latest", "team/qwen:8b"])
        {
            assert_eq!(request.method, Method::Post);
            assert!(request.url.ends_with("/proxy/api/show"));
            assert_eq!(
                serde_json::from_slice::<Value>(&request.body).expect("body"),
                serde_json::json!({"model": model})
            );
        }
        let json = catalog.to_json();
        assert_eq!(json["endpoint"], endpoint().base_url);
        assert_eq!(json["models"][1]["context_window"], 32768);
        let roundtrip: Catalog = serde_json::from_value(json).expect("catalog JSON");
        assert_eq!(roundtrip.models, catalog.models);
    }

    #[test]
    fn missing_invalid_and_zero_windows_remain_unknown() {
        for show in [
            "{}",
            r#"{"parameters":"num_ctx 0\nnum_ctx -1\nnum_ctx invalid\nnum_ctx 18446744073709551616"}"#,
            r#"{"model_info":{"general.architecture":"llama","llama.context_length":0}}"#,
            r#"{"model_info":{"general.architecture":"llama","llama.context_length":"8192"}}"#,
            r#"{"model_info":{"general.architecture":"llama","other.context_length":8192}}"#,
        ] {
            let fixture = Fixture::new(&[
                (200, r#"{"models":[{"name":"unknown:latest"}]}"#),
                (200, show),
            ]);
            let catalog =
                fetch_catalog(&fixture, &endpoint(), Duration::from_secs(5)).expect("import");
            let model = &catalog.models[0];
            assert_eq!(model.context_window, None, "{show}");
            assert_eq!(
                model.usable_context(),
                crate::catalog::DEFAULT_CONTEXT_WINDOW
            );
            assert!(
                model
                    .context_source
                    .as_deref()
                    .expect("source")
                    .starts_with("compiled_default")
            );
        }
    }

    #[test]
    fn offline_invalid_endpoint_and_expired_budget_make_no_requests() {
        let fixture = Fixture::new(&[]);
        let error = fetch_catalog(&fixture, &endpoint().offline(true), Duration::from_secs(5))
            .expect_err("offline");
        assert_eq!(error.kind(), FailureKind::PolicyDenied);
        let invalid = Endpoint::new("http://127.0.0.1:11434", "key");
        assert_eq!(
            fetch_catalog(&fixture, &invalid, Duration::from_secs(5))
                .expect_err("path")
                .kind(),
            FailureKind::InvalidRequest
        );
        assert_eq!(
            fetch_catalog(&fixture, &endpoint(), Duration::ZERO)
                .expect_err("timeout")
                .kind(),
            FailureKind::Timeout
        );
        assert!(fixture.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn failures_and_malformed_details_are_reported_without_a_catalog() {
        for (status, body, expected) in [
            (404, "not found", FailureKind::ModelNotFound),
            (302, "redirect", FailureKind::ProtocolViolation),
            (200, "not JSON", FailureKind::ProtocolViolation),
            (200, "[]", FailureKind::ProtocolViolation),
        ] {
            let fixture =
                Fixture::new(&[(200, r#"{"models":[{"name":"missing"}]}"#), (status, body)]);
            assert_eq!(
                fetch_catalog(&fixture, &endpoint(), Duration::from_secs(5))
                    .expect_err("bad detail")
                    .kind(),
                expected
            );
        }
        for body in ["{}", r#"{"models":[{}]}"#] {
            let fixture = Fixture::new(&[(200, body)]);
            assert_eq!(
                fetch_catalog(&fixture, &endpoint(), Duration::from_secs(5))
                    .expect_err("bad listing")
                    .kind(),
                FailureKind::ProtocolViolation
            );
        }
        let oversized = " ".repeat(MAX_BODY_BYTES as usize + 1);
        let fixture = Fixture::new(&[(200, &oversized)]);
        assert_eq!(
            fetch_catalog(&fixture, &endpoint(), Duration::from_secs(5))
                .expect_err("oversized")
                .kind(),
            FailureKind::ProtocolViolation
        );
    }
}
