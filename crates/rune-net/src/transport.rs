//! HTTP transport and streaming dispatch.
//!
//! One place performs network calls, so an audit of outbound traffic is a read
//! of this module. The client is blocking: a streaming read loop needs no
//! executor, and keeping one out of the process means command dispatch and
//! configuration never link an async runtime.
//!
//! The client itself sits behind [`Fetch`], and every request path here takes
//! `&dyn Fetch` rather than a concrete client. This module is the only one that
//! names the blocking client, so a target where it cannot compile supplies its
//! own implementation of the trait instead of a second request path.
//!
//! Every request is bounded in time, every response body is bounded in bytes,
//! and every failure maps onto the taxonomy the retry policy reads.

use std::io::Read;
use std::time::{Duration, Instant};

use rune_core::error::Result;

use crate::error::{FailureKind, NetError, NetResult};
use crate::fetch::{Fetch, FetchRequest};
use crate::message::Message;
use crate::provider::{Provider, RequestPlan, summarize_plan};
use crate::redact;
use crate::sse::{Decoder, Event};
use crate::stream::ProviderEvent;

/// Longest accepted endpoint URL.
pub const MAX_URL_BYTES: usize = 2048;

/// Largest non-streaming response body accepted.
pub const MAX_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;

/// Connection setup budget.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Time to wait for a response head before failing the attempt.
///
/// Configurable through the `provider_head_timeout_ms` limit. The default is
/// generous because a cold local model can take a long time to produce its
/// first byte, which is a failure mode other harnesses get wrong by timing out
/// too early.
pub const DEFAULT_HEAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Time budgets for one provider request attempt.
#[derive(Clone, Copy, Debug)]
pub struct RequestTimeouts {
    /// Time allowed for the response head and initial stream output.
    pub head: Duration,
    /// Total time allowed, including sending the request and reading its body.
    /// `None` disables the total deadline.
    pub total: Option<Duration>,
}

impl RequestTimeouts {
    /// Resolves the provider time budgets from configuration.
    #[must_use]
    pub fn from_limits(limits: &rune_core::budget::BudgetSet) -> Self {
        use rune_core::budget::LimitName;

        Self {
            head: Duration::from_millis(
                limits
                    .get(LimitName::ProviderHeadTimeoutMs)
                    .value()
                    .unwrap_or(120_000),
            ),
            total: limits
                .get(LimitName::ProviderRequestTimeoutMs)
                .value()
                .map(Duration::from_millis),
        }
    }
}

impl From<Duration> for RequestTimeouts {
    /// Preserves the head-only budget accepted by earlier transport callers.
    fn from(head: Duration) -> Self {
        Self { head, total: None }
    }
}

/// How a request is authenticated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthStyle {
    /// `Authorization: Bearer <value>`.
    Bearer,
    /// `x-api-key: <value>`, used by one dialect.
    ApiKeyHeader,
}

impl AuthStyle {
    /// Returns the header name this style sets.
    #[must_use]
    pub const fn header(self) -> &'static str {
        match self {
            Self::Bearer => "authorization",
            Self::ApiKeyHeader => "x-api-key",
        }
    }

    /// Renders the header value.
    #[must_use]
    pub fn value(self, credential: &str) -> String {
        match self {
            Self::Bearer => format!("Bearer {credential}"),
            Self::ApiKeyHeader => credential.to_owned(),
        }
    }
}

/// Everything needed to reach one endpoint.
#[derive(Clone, Debug)]
pub struct Endpoint {
    /// Base URL without a trailing slash.
    pub base_url: String,
    /// Credential value.
    pub credential: String,
    /// How the credential is presented.
    pub auth: AuthStyle,
    /// Headers sent with every request, after the authentication header.
    ///
    /// A gateway that routes by conversation needs a header naming the
    /// conversation, and one that expects a client to identify itself needs a
    /// user agent. Both are facts about the endpoint rather than about a
    /// dialect, so they live here rather than in the request builder.
    pub headers: Vec<(String, String)>,
    /// Whether outbound requests are refused entirely.
    pub offline: bool,
}

impl Endpoint {
    /// Builds an endpoint.
    #[must_use]
    pub fn new(base_url: impl Into<String>, credential: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            credential: credential.into(),
            auth: AuthStyle::Bearer,
            headers: Vec::new(),
            offline: false,
        }
    }

    /// Adds a header sent with every request.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Adds a header only when a value is present.
    #[must_use]
    pub fn with_header_when(self, name: &str, value: Option<impl Into<String>>) -> Self {
        match value {
            Some(value) => self.with_header(name, value),
            None => self,
        }
    }

    /// Sets the authentication style.
    #[must_use]
    pub const fn with_auth(mut self, auth: AuthStyle) -> Self {
        self.auth = auth;
        self
    }

    /// Refuses every request when set.
    #[must_use]
    pub const fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    /// Validates the base URL.
    pub fn validate(&self) -> Result<()> {
        validate_url(&self.base_url)
    }

    /// Joins a request path onto the base URL.
    #[must_use]
    pub fn url_for(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }
}

/// Validates an endpoint URL.
///
/// Requires HTTPS, except for a loopback address, where plain HTTP is the only
/// thing a local server provides.
pub fn validate_url(url: &str) -> Result<()> {
    use rune_core::error::{ErrorCode, RuneError};

    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(RuneError::missing_field("base_url"));
    }
    if trimmed.len() > MAX_URL_BYTES {
        return Err(RuneError::too_large(
            "base_url",
            trimmed.len(),
            MAX_URL_BYTES,
        ));
    }

    let lower = trimmed.to_ascii_lowercase();
    let rest = if let Some(rest) = lower.strip_prefix("https://") {
        rest
    } else if let Some(rest) = lower.strip_prefix("http://") {
        if !is_loopback_host(host_of(rest)) {
            return Err(RuneError::invalid_field(
                "base_url",
                "plain HTTP is only accepted for a loopback address",
            )
            .with_hint("use an https:// URL, or a local endpoint"));
        }
        rest
    } else {
        return Err(RuneError::invalid_field(
            "base_url",
            "must start with https:// or a loopback http:// URL",
        ));
    };

    if rest.is_empty() {
        return Err(RuneError::invalid_field("base_url", "has no host"));
    }
    if trimmed.contains(' ') {
        return Err(RuneError::invalid_field("base_url", "contains a space"));
    }
    if trimmed.contains('@') {
        return Err(
            RuneError::invalid_field("base_url", "must not embed credentials")
                .with_hint("supply the credential separately"),
        );
    }
    if trimmed.contains('#') {
        return Err(RuneError::invalid_field(
            "base_url",
            "must not have a fragment",
        ));
    }

    let _ = ErrorCode::InvalidField;
    Ok(())
}

/// Extracts the host from the part of a URL after the scheme.
///
/// A bracketed IPv6 literal is returned with its brackets removed, because the
/// colons inside it are not a port separator.
fn host_of(after_scheme: &str) -> &str {
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // Strip any user information, which is rejected separately.
    let authority = authority.rsplit('@').next().unwrap_or(authority);

    if let Some(rest) = authority.strip_prefix('[') {
        // A bracketed literal ends at the closing bracket.
        return rest.split(']').next().unwrap_or(rest);
    }

    authority.split(':').next().unwrap_or(authority)
}

/// Returns true when a host is a loopback address.
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Outcome of one streaming request.
#[derive(Clone, Debug, Default)]
pub struct StreamOutcome {
    /// Every event in arrival order.
    pub events: Vec<ProviderEvent>,
    /// Final usage, with unreported counts absent.
    pub usage: crate::stream::Usage,
    /// Provider state to store with the assistant message.
    pub replay: Option<String>,
    /// Normalized stop reason.
    pub finish: Option<crate::stream::FinishReason>,
}

impl StreamOutcome {
    /// Returns the concatenated assistant text.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for event in &self.events {
            if let ProviderEvent::TextDelta { delta } = event {
                out.push_str(delta);
            }
        }
        out
    }

    /// Returns the concatenated reasoning text.
    #[must_use]
    pub fn reasoning(&self) -> String {
        let mut out = String::new();
        for event in &self.events {
            if let ProviderEvent::ReasoningDelta { delta } = event {
                out.push_str(delta);
            }
        }
        out
    }

    /// Returns the completed tool calls.
    #[must_use]
    pub fn tool_calls(&self) -> Vec<(String, String, String)> {
        self.events
            .iter()
            .filter_map(|event| match event {
                ProviderEvent::ToolCallEnd { id, arguments } => {
                    let name = self
                        .events
                        .iter()
                        .find_map(|candidate| match candidate {
                            ProviderEvent::ToolCallStart { id: start, name } if start == id => {
                                Some(name.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    Some((id.to_string(), name, arguments.clone()))
                }
                _ => None,
            })
            .collect()
    }
}

/// Builds the HTTP agent used for every request.
///
/// One agent per process, so connections are reused rather than re-established.
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        // Callers supply each request's total budget through FetchRequest.
        .timeout_global(None)
        .http_status_as_error(false)
        .build()
        .into()
}

/// Adds headers to a request of either typestate.
///
/// `get` and `post` produce different builder types, so the headers are applied
/// through a generic helper rather than a value both branches could share.
#[cfg(not(target_family = "wasm"))]
fn with_headers<Any>(
    mut builder: ureq::RequestBuilder<Any>,
    headers: &[(String, String)],
) -> ureq::RequestBuilder<Any> {
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    builder
}

/// Applies a request's own bounds and redirect policy to a request of either
/// typestate.
///
/// A whole-request timeout bounds the exchange; leaving it unset keeps a
/// streamed generation open, and the head timeout bounds the wait for the
/// endpoint to start answering instead. Each is applied only when given, so a
/// client that carries its own bound keeps it.
#[cfg(not(target_family = "wasm"))]
fn with_bounds<Any>(
    builder: ureq::RequestBuilder<Any>,
    request: &FetchRequest,
) -> ureq::RequestBuilder<Any> {
    let mut config = builder.config();
    if let Some(timeout) = request.timeout {
        config = config.timeout_global(Some(timeout));
    }
    if let Some(head_timeout) = request.head_timeout {
        config = config.timeout_recv_response(Some(head_timeout));
    }
    if !request.follow_redirects {
        // No redirect is followed, and the redirect response is returned.
        config = config.max_redirects(0);
    }
    config.build()
}

/// Performs one request over a client.
///
/// The single place a request becomes a client call, so the pooled transport
/// and the plain client cannot drift apart in how they set headers, apply a
/// timeout, or read the media type.
#[cfg(not(target_family = "wasm"))]
fn send_over(
    client: &ureq::Agent,
    request: &FetchRequest,
) -> NetResult<crate::fetch::FetchResponse> {
    use crate::fetch::{FetchResponse, Method};

    let response = match request.method {
        Method::Get => with_bounds(
            with_headers(client.get(&request.url), &request.headers),
            request,
        )
        .call(),
        Method::Post => with_bounds(
            with_headers(client.post(&request.url), &request.headers),
            request,
        )
        .send(&request.body),
    }
    .map_err(|err| classify_transport_error(&err))?;

    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let content_type =
        header("content-type").unwrap_or_else(|| "application/octet-stream".to_owned());
    let retry_after = header("retry-after");
    let location = header("location");
    Ok(FetchResponse {
        status,
        content_type,
        retry_after,
        location,
        body: Box::new(response.into_body().into_reader()),
    })
}

/// The built-in [`Fetch`], over a pooled blocking client.
///
/// One instance per process keeps the connection pool alive across requests.
/// Created with [`UreqFetch::new`] and passed by reference, so every request
/// reuses the same pool.
///
/// Absent on a target where the client cannot build. Such a target supplies its
/// own [`Fetch`] and reaches every function here through the trait.
#[cfg(not(target_family = "wasm"))]
#[derive(Debug)]
pub struct UreqFetch {
    client: ureq::Agent,
}

#[cfg(not(target_family = "wasm"))]
impl UreqFetch {
    /// Builds the transport over its own pooled client.
    #[must_use]
    pub fn new() -> Self {
        Self { client: agent() }
    }
}

#[cfg(not(target_family = "wasm"))]
impl Default for UreqFetch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(target_family = "wasm"))]
impl Fetch for UreqFetch {
    fn send(&self, request: FetchRequest) -> NetResult<crate::fetch::FetchResponse> {
        send_over(&self.client, &request)
    }
}

#[cfg(not(target_family = "wasm"))]
impl Fetch for ureq::Agent {
    /// Sends over a client the caller already holds.
    ///
    /// A client built per request reuses no connection, so this is for a caller
    /// holding one already. A caller with none uses [`UreqFetch`].
    fn send(&self, request: FetchRequest) -> NetResult<crate::fetch::FetchResponse> {
        send_over(self, &request)
    }
}

/// Sends a streaming request and reduces the response.
///
/// Returns a retryable error for a transient failure and a permanent one for a
/// rejected request, which is the distinction the caller acts on.
pub fn stream_completion(
    client: &dyn Fetch,
    endpoint: &Endpoint,
    provider: &dyn Provider,
    plan: &RequestPlan,
    timeouts: impl Into<RequestTimeouts>,
    cancel: &dyn Fn() -> bool,
) -> NetResult<StreamOutcome> {
    stream_completion_observed(
        client,
        endpoint,
        provider,
        plan,
        timeouts,
        cancel,
        &mut |_| {},
    )
}

/// Sends a streaming request and reports each event as it is decoded.
///
/// The observer is called while the body is still arriving, which is what makes
/// a response appear as it is produced rather than once it has finished. It sees
/// every event the reducer produces, in order, and its return value is ignored:
/// presenting a delta must never be able to fail a request.
pub fn stream_completion_observed(
    client: &dyn Fetch,
    endpoint: &Endpoint,
    provider: &dyn Provider,
    plan: &RequestPlan,
    timeouts: impl Into<RequestTimeouts>,
    cancel: &dyn Fn() -> bool,
    observe: &mut dyn FnMut(&ProviderEvent),
) -> NetResult<StreamOutcome> {
    if endpoint.offline {
        return Err(
            NetError::new(FailureKind::Network, "outbound requests are disabled")
                .with_hint("remove the offline setting to reach a provider"),
        );
    }

    provider
        .validate(plan)
        .map_err(|err| NetError::new(FailureKind::InvalidRequest, err.message().to_owned()))?;

    let body = provider
        .build_request(plan)
        .map_err(|err| NetError::new(FailureKind::InvalidRequest, err.message().to_owned()))?;

    let url = endpoint.url_for(provider.request_path());
    let encoded = serde_json::to_string(&body)?;
    let timeouts = timeouts.into();

    let mut request = FetchRequest::post(url, encoded.into_bytes())
        .with_head_timeout(Some(timeouts.head))
        .with_timeout(timeouts.total)
        .with_header("content-type", "application/json")
        .with_header("accept", "text/event-stream")
        .with_header(
            endpoint.auth.header(),
            endpoint.auth.value(&endpoint.credential),
        );

    // The endpoint's own headers come after the dialect's, so an endpoint that
    // names a header the dialect also sets wins rather than being overwritten.
    for (name, value) in provider
        .extra_headers()
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .chain(endpoint.headers.iter().cloned())
    {
        request = request.with_header(name, value);
    }

    let started = Instant::now();
    let response = client.send(request)?;
    check_request_timeout(started, timeouts.total)?;

    if response.status >= 400 {
        let retry_after = response
            .retry_after
            .as_deref()
            .and_then(crate::error::parse_retry_after);
        let text = read_bounded_text(response.body, 64 * 1024);
        check_request_timeout(started, timeouts.total)?;
        let sanitized = redact::redact(&text);
        let mut error = NetError::classify_status(response.status, &sanitized).with_hint(format!(
            "provider `{}` rejected the request",
            provider.name()
        ));
        // The endpoint's own delay is used when it named one. Otherwise none is
        // attached, and the caller's backoff decides.
        if let Some(millis) = retry_after {
            error = error.with_retry_after(millis);
        }
        return Err(error);
    }

    read_stream_started(response.body, provider, timeouts, started, cancel, observe)
}

/// One response to an outbound request that is not a model completion.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Fetched {
    /// HTTP status of the final response.
    pub status: u16,
    /// Media type as reported, parameters included.
    pub content_type: String,
    /// The `Location` header, when the response carried one.
    pub location: Option<String>,
    /// Response body.
    pub body: Vec<u8>,
}

/// Largest body any outbound tool request will hold.
pub const MAX_FETCH_BYTES: u64 = 16 * 1024 * 1024;

/// A user agent used by the tool-facing client.
///
/// Names the program and its version so a server that rate limits or blocks can
/// see who is asking, rather than receiving an unnamed scraper.
pub const TOOL_USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; rune/",
    env!("CARGO_PKG_VERSION"),
    "; +https://github.com/AadiXC0DE/Rune)"
);

/// Fetches a URL this build chose, following any redirect.
///
/// Lives here rather than with its callers because every outbound request has
/// to pass through this module: it is the one place where the address, scheme,
/// and credential refusals are enforced, and a second client elsewhere would be
/// a way around them.
///
/// The chain is followed without being seen, so an address a model or a user
/// supplied goes through [`fetch_hop`] instead, whose caller vets every hop.
///
/// Absent on a target where the built-in client cannot build. Such a target
/// reaches the endpoint through [`stream_completion`] and its own [`Fetch`], or
/// not at all.
#[cfg(not(target_family = "wasm"))]
pub fn fetch_url(url: &str, accept: &str, timeout: Duration) -> NetResult<Fetched> {
    fetch(url, accept, timeout, true)
}

/// Fetches one URL for a tool, without following a redirect.
///
/// A redirect comes back as it is, with its status and `Location`. A client
/// that followed the chain itself would have requested every address on it
/// before the tool could check one, so a public page redirecting to a private
/// address would be fetched; the tool follows the chain instead, checking each
/// address before it is requested.
#[cfg(not(target_family = "wasm"))]
pub fn fetch_hop(url: &str, accept: &str, timeout: Duration) -> NetResult<Fetched> {
    fetch(url, accept, timeout, false)
}

/// Performs one bounded GET and holds its body.
#[cfg(not(target_family = "wasm"))]
fn fetch(url: &str, accept: &str, timeout: Duration, follow_redirects: bool) -> NetResult<Fetched> {
    let response = UreqFetch::new().send(
        FetchRequest::get(url)
            .with_header("user-agent", TOOL_USER_AGENT)
            .with_header("accept", accept)
            .with_timeout(Some(timeout))
            .with_redirects(follow_redirects),
    )?;

    let mut body = Vec::new();
    // One byte past the bound, so an oversized body is refused rather than
    // silently truncated into a decode failure.
    let limit = MAX_FETCH_BYTES.saturating_add(1);
    let mut reader = response.body.take(limit);
    reader.read_to_end(&mut body).map_err(NetError::from)?;
    if u64::try_from(body.len()).unwrap_or(u64::MAX) > MAX_FETCH_BYTES {
        return Err(NetError::new(
            FailureKind::InvalidRequest,
            "the response body is too large",
        )
        .with_hint("the tool refused to hold more than its body bound"));
    }

    Ok(Fetched {
        status: response.status,
        content_type: response.content_type,
        location: response.location,
        body,
    })
}

/// Searches the web through an HTML results endpoint.
///
/// Returns the page rather than parsed results, because parsing belongs with the
/// tool that defines what a result is. What lives here is the request, so this
/// module remains the only place that opens a connection.
#[cfg(not(target_family = "wasm"))]
pub fn search_html(query: &str, endpoint: &str, timeout: Duration) -> NetResult<String> {
    let escaped = percent_encode_query(query);
    let separator = if endpoint.contains('?') { '&' } else { '?' };
    let url = format!("{endpoint}{separator}q={escaped}");
    let fetched = fetch_url(
        &url,
        "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8",
        timeout,
    )?;
    if fetched.status >= 400 {
        return Err(NetError::new(
            FailureKind::Network,
            format!("the search endpoint returned HTTP {}", fetched.status),
        )
        .with_status(fetched.status)
        .with_hint("the endpoint may be rate limiting or refusing this client"));
    }
    Ok(String::from_utf8_lossy(&fetched.body).into_owned())
}

/// Escapes a query for a URL.
///
/// A space becomes `+`, which a search endpoint reads as a space, and every
/// other reserved byte is percent-encoded so a term carrying an ampersand or a
/// hash reaches the endpoint as one term rather than as several parameters.
#[must_use]
pub fn percent_encode_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for byte in query.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(*byte));
            }
            b' ' => out.push('+'),
            other => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

/// Fetches the models an endpoint offers.
///
/// This is the one non-streaming request in this module. It exists because a
/// user cannot choose a model they cannot see: the identifier has to come from
/// the endpoint that will serve it, since the same name means different things
/// at different hosts and no compiled table can be complete.
///
/// A failure is returned rather than swallowed, because an empty list and an
/// unreachable endpoint are different answers and only one of them means the
/// provider has no models.
pub fn list_models(
    client: &dyn Fetch,
    endpoint: &Endpoint,
    provider: &dyn Provider,
    timeout: Duration,
) -> NetResult<String> {
    if endpoint.offline {
        return Err(
            NetError::new(FailureKind::Network, "outbound requests are disabled")
                .with_hint("remove the offline setting to reach a provider"),
        );
    }

    let path = provider.models_path().ok_or_else(|| {
        NetError::new(
            FailureKind::InvalidRequest,
            format!(
                "provider `{}` does not expose a model list",
                provider.name()
            ),
        )
        .with_hint("the model is used as given; check the provider's documentation")
    })?;

    let mut request = FetchRequest::get(endpoint.url_for(path))
        .with_header("accept", "application/json")
        .with_header(
            endpoint.auth.header(),
            endpoint.auth.value(&endpoint.credential),
        );
    for (name, value) in provider
        .extra_headers()
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .chain(endpoint.headers.iter().cloned())
    {
        request = request.with_header(name, value);
    }

    // Each request supplies its own total budget. A listing is a small
    // document, so it gets the caller's listing budget here.
    let response = client.send(request.with_timeout(Some(timeout)))?;

    let text = read_bounded_text(response.body, 1024 * 1024);
    if response.status >= 400 {
        return Err(
            NetError::classify_status(response.status, &redact::redact(&text)).with_hint(format!(
                "provider `{}` refused to list its models",
                provider.name()
            )),
        );
    }
    Ok(text)
}

/// Reports events a reducer appended since a recorded length.
///
/// The reducer owns where an event lands in the vec, so what it appended is
/// read back rather than predicted.
fn report_new(events: &[ProviderEvent], before: usize, observe: &mut dyn FnMut(&ProviderEvent)) {
    for event in events.get(before..).unwrap_or_default() {
        observe(event);
    }
}

/// Largest piece of a streamed body read at once.
///
/// The decoder accepts any chunk boundary, so the body is read in pieces of a
/// fixed size rather than by line: a body that never sends a line break then
/// meets the decoder's bounds instead of growing one line without end.
const STREAM_CHUNK_BYTES: usize = 16 * 1024;

/// Chunks read ahead of the decoder before the reading thread waits.
#[cfg(not(target_family = "wasm"))]
const STREAM_CHUNKS_AHEAD: usize = 8;

/// How often a silent stream is checked for cancellation.
#[cfg(not(target_family = "wasm"))]
const STREAM_POLL: Duration = Duration::from_millis(50);

/// Reads and reduces a streaming response body.
///
/// The body is read as it arrives, so each frame is reduced and reported as
/// soon as it is complete. Collecting the body first would hold the whole
/// answer back until the response ended, which is the behavior this path
/// exists to avoid. A cancellation is noticed while the endpoint is silent,
/// not only when it next sends something.
///
/// Public because a host that reaches an endpoint through its own [`Fetch`],
/// including one that runs where the built-in client cannot compile, reduces
/// the body with the same decoder, limits, and time budgets rather than a
/// second implementation that can disagree with this one.
/// The total budget starts here; `stream_completion_observed` also counts the
/// time spent sending the request and waiting for its response head.
pub fn read_stream(
    body: Box<dyn Read + Send>,
    provider: &dyn Provider,
    timeouts: impl Into<RequestTimeouts>,
    cancel: &dyn Fn() -> bool,
    observe: &mut dyn FnMut(&ProviderEvent),
) -> NetResult<StreamOutcome> {
    read_stream_started(
        body,
        provider,
        timeouts.into(),
        Instant::now(),
        cancel,
        observe,
    )
}

fn read_stream_started(
    body: Box<dyn Read + Send>,
    provider: &dyn Provider,
    timeouts: RequestTimeouts,
    started: Instant,
    cancel: &dyn Fn() -> bool,
    observe: &mut dyn FnMut(&ProviderEvent),
) -> NetResult<StreamOutcome> {
    let mut state = StreamState::new(provider, observe, timeouts, started);
    feed(body, &mut state, cancel)?;
    state.finish()
}

/// Feeds a body to the decoder on a thread of its own.
///
/// A blocking read cannot be interrupted, so the body is read on a helper
/// thread and this one waits on the handover in short slices, checking for a
/// cancellation and the total deadline between them. The handover is bounded,
/// so the helper reads only a few chunks ahead of the decoder. Once nothing is
/// listening the helper ends at its next chunk; a read that never returns holds
/// it until the connection closes.
#[cfg(not(target_family = "wasm"))]
fn feed(
    mut body: Box<dyn Read + Send>,
    state: &mut StreamState<'_>,
    cancel: &dyn Fn() -> bool,
) -> NetResult<()> {
    use std::sync::mpsc::{RecvTimeoutError, sync_channel};

    let (sender, chunks) = sync_channel::<std::io::Result<Vec<u8>>>(STREAM_CHUNKS_AHEAD);
    std::thread::Builder::new()
        .name("rune-stream-read".to_owned())
        .spawn(move || {
            let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
            loop {
                let chunk = match body.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => Ok(buffer.get(..count).unwrap_or_default().to_vec()),
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => Err(err),
                };
                let failed = chunk.is_err();
                if sender.send(chunk).is_err() || failed {
                    break;
                }
            }
        })
        .map_err(NetError::from)?;

    loop {
        if cancel() {
            return Err(cancelled());
        }
        state.check_request_timeout()?;
        let chunk = chunks.recv_timeout(STREAM_POLL);
        state.check_request_timeout()?;
        match chunk {
            Ok(chunk) => {
                state.push(&chunk.map_err(NetError::from)?)?;
                if state.is_done() {
                    return Ok(());
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The helper has reached the end of the body.
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// Feeds a body to the decoder where there are no threads to read it on.
///
/// Each read blocks until the endpoint sends something, so a cancellation is
/// noticed between reads rather than during one.
#[cfg(target_family = "wasm")]
fn feed(
    mut body: Box<dyn Read + Send>,
    state: &mut StreamState<'_>,
    cancel: &dyn Fn() -> bool,
) -> NetResult<()> {
    let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
    loop {
        if cancel() {
            return Err(cancelled());
        }
        state.check_request_timeout()?;
        let count = match body.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => count,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(NetError::from(err)),
        };
        state.push(buffer.get(..count).unwrap_or_default())?;
        if state.is_done() {
            return Ok(());
        }
    }
}

/// The error for a request the caller cancelled.
fn cancelled() -> NetError {
    NetError::new(FailureKind::Cancelled, "the request was cancelled")
}

/// Checks a total budget without restarting it when a chunk arrives.
fn check_request_timeout(started: Instant, total: Option<Duration>) -> NetResult<()> {
    if total.is_some_and(|timeout| started.elapsed() >= timeout) {
        return Err(NetError::new(
            FailureKind::Timeout,
            "the provider request exceeded its total timeout",
        )
        .with_hint("raise provider_request_timeout_ms to allow a longer generation"));
    }
    Ok(())
}

/// The decoding half of a streamed read.
///
/// Kept apart from the reading so the threaded and the plain read loops share
/// one set of framing, limits, and termination rules.
struct StreamState<'a> {
    decoder: Decoder,
    reducer: Box<dyn crate::stream::StreamReducer>,
    outcome: StreamOutcome,
    frames: Vec<Event>,
    observe: &'a mut dyn FnMut(&ProviderEvent),
    head_timeout: Duration,
    started: Instant,
    request_timeout: Option<Duration>,
    request_started: Instant,
    /// Whether any event has been decoded, which ends the wait for output.
    saw_event: bool,
}

impl<'a> StreamState<'a> {
    fn new(
        provider: &dyn Provider,
        observe: &'a mut dyn FnMut(&ProviderEvent),
        timeouts: RequestTimeouts,
        request_started: Instant,
    ) -> Self {
        Self {
            decoder: Decoder::new(provider.limits()),
            reducer: provider.reducer(),
            outcome: StreamOutcome::default(),
            frames: Vec::new(),
            observe,
            head_timeout: timeouts.head,
            started: Instant::now(),
            request_timeout: timeouts.total,
            request_started,
            saw_event: false,
        }
    }

    /// Decodes one chunk, reducing and reporting every event it completes.
    fn push(&mut self, chunk: &[u8]) -> NetResult<()> {
        self.check_request_timeout()?;
        self.decoder
            .push(chunk, &mut self.frames)
            .map_err(NetError::from)?;
        self.reduce_frames()?;

        // Judged after the chunk is decoded, because the line that completes
        // the first event is that event arriving, not more waiting. What
        // remains is an endpoint holding the connection open with keep-alive
        // lines and nothing else.
        if !self.saw_event && self.decoder.is_idle() && self.started.elapsed() > self.head_timeout {
            return Err(NetError::new(
                FailureKind::Timeout,
                "the endpoint produced no output within the head timeout",
            )
            .with_hint("raise provider_head_timeout_ms for a slow local model"));
        }
        Ok(())
    }

    /// Returns true once the stream has said it is done.
    fn is_done(&self) -> bool {
        self.decoder.is_done()
    }

    fn check_request_timeout(&self) -> NetResult<()> {
        check_request_timeout(self.request_started, self.request_timeout)
    }

    /// Hands every decoded event to the reducer.
    fn reduce_frames(&mut self) -> NetResult<()> {
        for event in self.frames.drain(..) {
            self.saw_event = true;
            // Every payload reaches the reducer, including the terminator,
            // because the reducer is the authority on what a terminal payload
            // means for its dialect and it is the one that checks the stream
            // ended legally.
            if event.is_empty() && event.name.is_none() {
                continue;
            }
            let before = self.outcome.events.len();
            self.reducer
                .apply(Some(&event.data), &mut self.outcome.events)
                .map_err(NetError::from)?;
            report_new(&self.outcome.events, before, &mut *self.observe);
        }
        Ok(())
    }

    /// Ends the stream and returns what it produced.
    fn finish(mut self) -> NetResult<StreamOutcome> {
        self.check_request_timeout()?;
        self.decoder
            .finish(&mut self.frames)
            .map_err(NetError::from)?;
        self.reduce_frames()?;

        // Signals end of transport. A reducer that never saw a legal terminator
        // reports an incomplete stream, which is retryable, rather than allowing
        // a truncated response to look like a success.
        let before = self.outcome.events.len();
        self.reducer
            .apply(None, &mut self.outcome.events)
            .map_err(NetError::from)?;
        report_new(&self.outcome.events, before, &mut *self.observe);

        self.outcome.finish = Some(self.reducer.finish().map_err(NetError::from)?);
        self.outcome.usage = self.reducer.usage();
        self.outcome.replay = self.reducer.replay();
        Ok(self.outcome)
    }
}

/// Reads a body up to a byte limit, lossily decoding it.
fn read_bounded_text(body: Box<dyn Read + Send>, limit: u64) -> String {
    let mut reader = body.take(limit);
    let mut buffer = Vec::new();
    let _ = reader.read_to_end(&mut buffer);
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Maps a transport failure onto the taxonomy.
#[cfg(not(target_family = "wasm"))]
fn classify_transport_error(err: &ureq::Error) -> NetError {
    let message = redact::redact(&err.to_string());
    let kind = match err {
        ureq::Error::Timeout(_) => FailureKind::Timeout,
        ureq::Error::Io(_) => FailureKind::Network,
        ureq::Error::ConnectionFailed => FailureKind::Network,
        ureq::Error::HostNotFound => FailureKind::Network,
        ureq::Error::RedirectFailed => FailureKind::InvalidRequest,
        _ => FailureKind::Network,
    };
    NetError::new(kind, message)
}

/// Builds a request plan's message list for a one-shot request.
///
/// Kept here so the agent and the one-shot runner construct the same shape and
/// a difference between them is a test failure rather than a surprise.
#[must_use]
pub fn one_shot_messages(prompt: &str) -> Vec<Message> {
    vec![Message::user(prompt)]
}

/// Renders a plan summary for a failed-request diagnostic.
#[must_use]
pub fn diagnostic_for(plan: &RequestPlan, error: &NetError) -> serde_json::Value {
    serde_json::json!({
        "failure": error.kind().as_str(),
        "message": redact::redact(error.message()),
        "status": error.status(),
        "request": summarize_plan(plan),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client that fails the test if a request reaches it.
    ///
    /// Used where the point of the test is that no request is attempted, so a
    /// transport is needed to call the function but none may be touched.
    struct NoFetch;

    impl Fetch for NoFetch {
        fn send(&self, _request: FetchRequest) -> NetResult<crate::fetch::FetchResponse> {
            unreachable!("the request should have been refused before any client call")
        }
    }

    /// A response body that serves scripted parts, each after its own delay.
    struct PacedBody {
        parts: std::collections::VecDeque<(Duration, Vec<u8>)>,
    }

    impl PacedBody {
        fn body(parts: Vec<(Duration, &[u8])>) -> Box<dyn Read + Send> {
            Box::new(Self {
                parts: parts
                    .into_iter()
                    .map(|(delay, bytes)| (delay, bytes.to_vec()))
                    .collect(),
            })
        }
    }

    impl Read for PacedBody {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let Some((delay, bytes)) = self.parts.front_mut() else {
                return Ok(0);
            };
            std::thread::sleep(std::mem::take(delay));
            let count = bytes.len().min(buf.len());
            buf[..count].copy_from_slice(&bytes[..count]);
            bytes.drain(..count);
            if bytes.is_empty() {
                self.parts.pop_front();
            }
            Ok(count)
        }
    }

    /// One complete chat answer, split before the blank line that ends its
    /// first event.
    const ANSWER_HEAD: &[u8] = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late\"},\"finish_reason\":\"stop\"}]}\n";
    const ANSWER_TAIL: &[u8] = b"\ndata: [DONE]\n\n";

    const DELTA: &[u8] =
        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"still streaming\"}}]}\n\n";

    /// A client whose head and body each consume part of the request budget.
    struct PacedFetch {
        expected_timeout: Option<Duration>,
    }

    impl Fetch for PacedFetch {
        fn send(&self, request: FetchRequest) -> NetResult<crate::fetch::FetchResponse> {
            assert_eq!(request.timeout, self.expected_timeout);
            assert_eq!(request.head_timeout, Some(DEFAULT_HEAD_TIMEOUT));
            std::thread::sleep(Duration::from_millis(100));
            Ok(crate::fetch::FetchResponse {
                status: 200,
                content_type: "text/event-stream".to_owned(),
                retry_after: None,
                location: None,
                body: PacedBody::body(vec![
                    (Duration::ZERO, DELTA),
                    (Duration::from_millis(50), DELTA),
                    (Duration::from_millis(100), ANSWER_HEAD),
                    (Duration::ZERO, ANSWER_TAIL),
                ]),
            })
        }
    }

    #[test]
    fn the_total_deadline_includes_the_head_and_does_not_reset_on_output() {
        let mut observed = Vec::new();
        let started = Instant::now();
        let err = stream_completion_observed(
            &PacedFetch {
                expected_timeout: Some(Duration::from_millis(200)),
            },
            &Endpoint::new("https://api.example.com", "k"),
            &crate::chat_completions::ChatCompletions,
            &RequestPlan::new("m"),
            RequestTimeouts {
                head: DEFAULT_HEAD_TIMEOUT,
                total: Some(Duration::from_millis(200)),
            },
            &|| false,
            &mut |event| observed.push(event.clone()),
        )
        .expect_err("the late completion must time out");
        assert_eq!(err.kind(), FailureKind::Timeout, "{err}");
        assert_eq!(
            err.to_rune_error().code(),
            rune_core::error::ErrorCode::Timeout
        );
        assert!(err.message().contains("total timeout"), "{err}");
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, ProviderEvent::TextDelta { .. })),
            "the request must start streaming before timing out"
        );
    }

    #[test]
    fn disabling_the_total_deadline_keeps_a_longer_completion() {
        let mut limits = rune_core::budget::BudgetSet::new();
        limits
            .set(
                rune_core::budget::LimitName::ProviderRequestTimeoutMs,
                rune_core::budget::Budget::Unbounded,
                rune_core::config::Layer::User,
            )
            .expect("disable total deadline");
        let outcome = stream_completion(
            &PacedFetch {
                expected_timeout: None,
            },
            &Endpoint::new("https://api.example.com", "k"),
            &crate::chat_completions::ChatCompletions,
            &RequestPlan::new("m"),
            RequestTimeouts::from_limits(&limits),
            &|| false,
        )
        .expect("completion with no total deadline");
        assert_eq!(outcome.text(), "still streamingstill streaminglate");
    }

    #[test]
    fn a_first_event_completed_after_the_head_timeout_is_kept() {
        // The endpoint answered, and only the line that closes its first
        // event arrived after the head timeout.
        let body = PacedBody::body(vec![
            (Duration::ZERO, ANSWER_HEAD),
            (Duration::from_millis(150), ANSWER_TAIL),
        ]);
        let outcome = read_stream(
            body,
            &crate::chat_completions::ChatCompletions,
            Duration::from_millis(50),
            &|| false,
            &mut |_| {},
        )
        .expect("the delivered answer is kept");
        assert_eq!(outcome.text(), "late");
    }

    #[test]
    fn an_endpoint_sending_only_keep_alives_past_the_head_timeout_times_out() {
        let body = PacedBody::body(vec![
            (Duration::ZERO, b": keep-alive\n\n"),
            (Duration::from_millis(150), b": keep-alive\n\n"),
            (Duration::ZERO, ANSWER_HEAD),
            (Duration::ZERO, ANSWER_TAIL),
        ]);
        let err = read_stream(
            body,
            &crate::chat_completions::ChatCompletions,
            Duration::from_millis(50),
            &|| false,
            &mut |_| {},
        )
        .expect_err("timed out");
        assert_eq!(err.kind(), FailureKind::Timeout);
    }

    /// A response body that sends nothing until the test releases it.
    struct SilentBody(std::sync::mpsc::Receiver<()>);

    impl Read for SilentBody {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    #[test]
    fn the_total_deadline_expires_during_a_blocked_body_read() {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (report, reported) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = read_stream(
                Box::new(SilentBody(released)),
                &crate::chat_completions::ChatCompletions,
                RequestTimeouts {
                    head: DEFAULT_HEAD_TIMEOUT,
                    total: Some(Duration::from_millis(100)),
                },
                &|| false,
                &mut |_| {},
            );
            let _ = report.send(result);
        });
        let result = reported.recv_timeout(Duration::from_secs(2));
        drop(release);
        let err = result
            .expect("the total deadline interrupted the stream wait")
            .expect_err("total timeout");
        assert_eq!(err.kind(), FailureKind::Timeout, "{err}");
        assert!(err.message().contains("total timeout"), "{err}");
    }

    #[test]
    fn a_silent_stream_can_be_cancelled() {
        // A reasoning model that is thinking, or a stalled proxy, sends
        // nothing at all, and the user must still be able to stop it.
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (report, reported) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = read_stream(
                Box::new(SilentBody(released)),
                &crate::chat_completions::ChatCompletions,
                DEFAULT_HEAD_TIMEOUT,
                &|| started.elapsed() > Duration::from_millis(100),
                &mut |_| {},
            );
            let _ = report.send(result.map(|_| ()));
        });

        let result = reported.recv_timeout(Duration::from_secs(10));
        drop(release);
        let err = result
            .expect("the read returned while the endpoint was silent")
            .expect_err("cancelled");
        assert_eq!(err.kind(), FailureKind::Cancelled);
    }

    /// A body of one repeated byte with no line break, counting what it served.
    struct EndlessLine {
        served: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        limit: usize,
    }

    impl Read for EndlessLine {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            use std::sync::atomic::Ordering;
            let served = self.served.load(Ordering::SeqCst);
            let count = buf.len().min(self.limit.saturating_sub(served));
            buf[..count].fill(b'a');
            self.served.fetch_add(count, Ordering::SeqCst);
            Ok(count)
        }
    }

    #[test]
    fn a_body_without_a_line_break_is_refused_without_reading_it_all() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let served = std::sync::Arc::new(AtomicUsize::new(0));
        let body = Box::new(EndlessLine {
            served: std::sync::Arc::clone(&served),
            limit: 64 * 1024 * 1024,
        });
        let err = read_stream(
            body,
            &crate::chat_completions::ChatCompletions,
            DEFAULT_HEAD_TIMEOUT,
            &|| false,
            &mut |_| {},
        )
        .expect_err("refused");
        assert_eq!(err.kind(), FailureKind::InvalidRequest, "{err}");
        let read = served.load(Ordering::SeqCst);
        assert!(read < 4 * 1024 * 1024, "read {read} bytes before refusing");
    }

    /// A local endpoint that accepts one connection and never answers it.
    ///
    /// The connection stays open until the returned sender is dropped, so a
    /// client waits on it rather than seeing it close.
    #[cfg(not(target_family = "wasm"))]
    fn mute_endpoint() -> (String, std::sync::mpsc::Sender<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (hold, held) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let connection = listener.accept();
            let _ = held.recv();
            drop(connection);
        });
        (format!("http://127.0.0.1:{port}"), hold)
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn an_endpoint_that_never_answers_times_out_at_the_head_timeout() {
        let (base, hold) = mute_endpoint();
        let (report, reported) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut plan = RequestPlan::new("m");
            plan.messages = vec![Message::user("hi")];
            let result = stream_completion(
                &UreqFetch::new(),
                &Endpoint::new(base, "k"),
                &crate::chat_completions::ChatCompletions,
                &plan,
                Duration::from_millis(200),
                &|| false,
            );
            let _ = report.send(result.map(|_| ()));
        });

        let result = reported.recv_timeout(Duration::from_secs(10));
        drop(hold);
        let err = result
            .expect("the request gave up on the endpoint")
            .expect_err("timed out");
        assert_eq!(err.kind(), FailureKind::Timeout, "{err}");
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn the_total_deadline_also_bounds_waiting_for_response_headers() {
        let (base, hold) = mute_endpoint();
        let started = Instant::now();
        let (report, reported) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = stream_completion(
                &UreqFetch::new(),
                &Endpoint::new(base, "k"),
                &crate::chat_completions::ChatCompletions,
                &RequestPlan::new("m"),
                RequestTimeouts {
                    head: DEFAULT_HEAD_TIMEOUT,
                    total: Some(Duration::from_millis(200)),
                },
                &|| false,
            );
            let _ = report.send(result);
        });
        let result = reported.recv_timeout(Duration::from_secs(2));
        drop(hold);
        let err = result
            .expect("the request returned before its head timeout")
            .expect_err("the total deadline bounds the HTTP client");
        assert_eq!(err.kind(), FailureKind::Timeout, "{err}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// A client that answers every request with one canned status.
    struct CannedFetch {
        status: u16,
        retry_after: Option<&'static str>,
    }

    impl Fetch for CannedFetch {
        fn send(&self, _request: FetchRequest) -> NetResult<crate::fetch::FetchResponse> {
            Ok(crate::fetch::FetchResponse {
                status: self.status,
                content_type: "application/json".to_owned(),
                retry_after: self.retry_after.map(str::to_owned),
                location: None,
                body: Box::new(std::io::Cursor::new(b"{}".to_vec())),
            })
        }
    }

    /// Streams one completion over a canned client.
    fn complete_over(client: &dyn Fetch) -> NetResult<StreamOutcome> {
        let mut plan = RequestPlan::new("m");
        plan.messages = vec![Message::user("hi")];
        stream_completion(
            client,
            &Endpoint::new("https://api.example.com", "k"),
            &crate::chat_completions::ChatCompletions,
            &plan,
            DEFAULT_HEAD_TIMEOUT,
            &|| false,
        )
    }

    #[test]
    fn a_rate_limit_waits_as_long_as_the_endpoint_asked() {
        let err = complete_over(&CannedFetch {
            status: 429,
            retry_after: Some("7"),
        })
        .expect_err("rate limited");
        assert_eq!(err.kind(), FailureKind::RateLimited);
        assert_eq!(err.retry_after_ms(), Some(7_000));
    }

    #[test]
    fn a_rate_limit_without_a_delay_leaves_the_backoff_to_the_caller() {
        for retry_after in [None, Some("Wed, 21 Oct 2026 07:28:00 GMT")] {
            let err = complete_over(&CannedFetch {
                status: 429,
                retry_after,
            })
            .expect_err("rate limited");
            assert_eq!(err.retry_after_ms(), None, "{retry_after:?}");
        }
    }

    #[test]
    fn an_unavailable_endpoint_that_names_a_delay_is_honored() {
        let err = complete_over(&CannedFetch {
            status: 503,
            retry_after: Some("2"),
        })
        .expect_err("unavailable");
        assert_eq!(err.retry_after_ms(), Some(2_000));
    }

    /// A local endpoint that reads one request and sends one raw response.
    #[cfg(not(target_family = "wasm"))]
    fn answering_endpoint(response: String) -> String {
        use std::io::{BufRead as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
            let mut length = 0_usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0_u8; length];
            let _ = reader.read_exact(&mut body);
            let _ = stream.write_all(response.as_bytes());
        });
        format!("http://127.0.0.1:{port}")
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn the_retry_after_header_reaches_the_response() {
        let base = answering_endpoint(
            "HTTP/1.1 429 Too Many Requests\r\nretry-after: 3\r\ncontent-length: 0\r\n\
             connection: close\r\n\r\n"
                .to_owned(),
        );
        let response = UreqFetch::new()
            .send(FetchRequest::get(format!("{base}/v1/models")))
            .expect("answered");
        assert_eq!(response.status, 429);
        assert_eq!(response.retry_after.as_deref(), Some("3"));
    }

    /// A local endpoint that counts the requests it receives and answers none.
    #[cfg(not(target_family = "wasm"))]
    fn counting_endpoint() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&hits);
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                if connection.is_err() {
                    break;
                }
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        (format!("http://127.0.0.1:{port}"), hits)
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn a_tool_fetch_returns_a_redirect_instead_of_following_it() {
        let (target, hits) = counting_endpoint();
        let base = answering_endpoint(format!(
            "HTTP/1.1 302 Found\r\nlocation: {target}/latest/meta-data/\r\n\
             content-length: 0\r\nconnection: close\r\n\r\n"
        ));
        let fetched =
            fetch_hop(&format!("{base}/page"), "*/*", Duration::from_secs(5)).expect("answered");
        assert_eq!(fetched.status, 302);
        assert_eq!(
            fetched.location.as_deref(),
            Some(format!("{target}/latest/meta-data/").as_str())
        );
        // Give a followed redirect time to land before counting.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the redirect target was requested"
        );
    }

    #[test]
    fn https_is_accepted() {
        validate_url("https://api.example.com").expect("accepted");
        validate_url("https://api.example.com/v1").expect("accepted");
    }

    #[test]
    fn plain_http_is_rejected_for_a_remote_host() {
        let err = validate_url("http://api.example.com").expect_err("rejected");
        assert_eq!(err.code(), rune_core::error::ErrorCode::InvalidField);
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn plain_http_is_accepted_for_loopback() {
        for url in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:8080",
            "http://[::1]:11434/v1",
            "http://localhost/v1",
        ] {
            validate_url(url).unwrap_or_else(|err| panic!("rejected {url}: {err}"));
        }
    }

    #[test]
    fn a_non_loopback_http_url_is_still_rejected_after_the_host_fix() {
        for url in [
            "http://10.0.0.1:8080",
            "http://example.com",
            "http://[2001:db8::1]:8080",
        ] {
            assert!(validate_url(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn the_host_extractor_handles_brackets_userinfo_and_ports() {
        assert_eq!(host_of("example.com/v1"), "example.com");
        assert_eq!(host_of("example.com:8443/v1"), "example.com");
        assert_eq!(host_of("[::1]:11434/v1"), "::1");
        assert_eq!(host_of("[2001:db8::1]/x"), "2001:db8::1");
        assert_eq!(host_of("user:pass@example.com/v1"), "example.com");
    }

    #[test]
    fn a_url_without_a_scheme_is_rejected() {
        assert!(validate_url("api.example.com").is_err());
        assert!(validate_url("ftp://example.com").is_err());
    }

    #[test]
    fn an_empty_url_is_a_missing_field() {
        let err = validate_url("   ").expect_err("rejected");
        assert_eq!(err.code(), rune_core::error::ErrorCode::MissingField);
    }

    #[test]
    fn an_oversized_url_is_rejected() {
        let url = format!("https://example.com/{}", "x".repeat(MAX_URL_BYTES));
        let err = validate_url(&url).expect_err("rejected");
        assert_eq!(err.code(), rune_core::error::ErrorCode::TooLarge);
    }

    #[test]
    fn embedded_credentials_are_rejected() {
        let err = validate_url("https://user:pass@example.com").expect_err("rejected");
        assert!(err.message().contains("credentials"));
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn a_url_without_a_host_is_rejected() {
        assert!(validate_url("https://").is_err());
    }

    #[test]
    fn a_url_with_a_fragment_is_rejected() {
        assert!(validate_url("https://example.com/v1#frag").is_err());
    }

    #[test]
    fn the_path_is_joined_onto_the_base_without_doubling_a_separator() {
        let endpoint = Endpoint::new("https://api.example.com/", "k");
        assert_eq!(
            endpoint.url_for("/v1/messages"),
            "https://api.example.com/v1/messages"
        );
        let endpoint = Endpoint::new("https://api.example.com", "k");
        assert_eq!(
            endpoint.url_for("/v1/messages"),
            "https://api.example.com/v1/messages"
        );
    }

    #[test]
    fn the_bearer_style_renders_a_bearer_header() {
        assert_eq!(AuthStyle::Bearer.header(), "authorization");
        assert_eq!(AuthStyle::Bearer.value("abc"), "Bearer abc");
    }

    #[test]
    fn the_api_key_style_uses_its_own_header() {
        assert_eq!(AuthStyle::ApiKeyHeader.header(), "x-api-key");
        assert_eq!(AuthStyle::ApiKeyHeader.value("abc"), "abc");
    }

    #[test]
    fn an_offline_endpoint_refuses_before_any_network_call() {
        let endpoint = Endpoint::new("https://api.example.com", "k").offline(true);
        let provider = crate::chat_completions::ChatCompletions;
        let plan = RequestPlan::new("m");
        let err = stream_completion(
            &NoFetch,
            &endpoint,
            &provider,
            &plan,
            DEFAULT_HEAD_TIMEOUT,
            &|| false,
        )
        .expect_err("refused");
        assert_eq!(err.kind(), FailureKind::Network);
        assert!(err.message().contains("disabled"));
    }

    #[test]
    fn an_invalid_url_is_reported_before_a_request_is_attempted() {
        let endpoint = Endpoint::new("http://remote.example.com", "k");
        assert!(endpoint.validate().is_err());
    }

    #[test]
    fn usage_is_absent_when_the_provider_reported_none() {
        let outcome = StreamOutcome::default();
        assert!(outcome.usage.is_empty());
        assert!(outcome.finish.is_none());
        assert!(outcome.text().is_empty());
    }

    #[test]
    fn the_outcome_text_concatenates_only_deltas() {
        let outcome = StreamOutcome {
            events: vec![
                ProviderEvent::TextDelta {
                    delta: "one ".to_owned(),
                },
                ProviderEvent::ReasoningDelta {
                    delta: "hidden".to_owned(),
                },
                ProviderEvent::TextDelta {
                    delta: "two".to_owned(),
                },
            ],
            ..StreamOutcome::default()
        };
        assert_eq!(outcome.text(), "one two");
    }

    #[test]
    fn tool_calls_pair_arguments_with_the_name_from_the_start_event() {
        let id = rune_core::id::ToolCallId::new("c1").expect("id");
        let outcome = StreamOutcome {
            events: vec![
                ProviderEvent::ToolCallStart {
                    id: id.clone(),
                    name: "read_file".to_owned(),
                },
                ProviderEvent::ToolCallEnd {
                    id,
                    arguments: "{}".to_owned(),
                },
            ],
            ..StreamOutcome::default()
        };
        let calls = outcome.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "read_file");
        assert_eq!(calls[0].2, "{}");
    }

    #[test]
    fn the_diagnostic_carries_the_failure_and_a_request_summary() {
        let plan = RequestPlan::new("m");
        let error = NetError::classify_status(429, "");
        let value = diagnostic_for(&plan, &error);
        assert_eq!(value["failure"], "rate_limited");
        assert_eq!(value["status"], 429);
        assert_eq!(value["request"]["model"], "m");
    }

    #[test]
    fn a_diagnostic_never_carries_a_credential() {
        let plan = RequestPlan::new("m");
        let error = NetError::new(
            FailureKind::Unauthorized,
            "rejected key sk-abcdefghijklmnopqrst",
        );
        let rendered = diagnostic_for(&plan, &error).to_string();
        assert!(!rendered.contains("sk-abcdefghijklmnopqrst"), "{rendered}");
    }

    #[test]
    fn the_one_shot_message_shape_is_a_single_user_turn() {
        let messages = one_shot_messages("hello");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, crate::message::Role::User);
        assert_eq!(messages[0].text(), "hello");
    }
}
