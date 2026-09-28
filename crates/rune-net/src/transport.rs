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

use std::io::{BufRead, BufReader, Read};
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
        // No overall body timeout: a long generation is expected, and the head
        // timeout bounds the part that can actually hang.
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

    // A request level timeout bounds the whole exchange; leaving it unset keeps
    // a streamed generation open, which the head timeout bounds instead.
    let response = match request.method {
        Method::Get => {
            let builder = with_headers(client.get(&request.url), &request.headers);
            match request.timeout {
                Some(timeout) => builder
                    .config()
                    .timeout_global(Some(timeout))
                    .build()
                    .call(),
                None => builder.call(),
            }
        }
        Method::Post => {
            let builder = with_headers(client.post(&request.url), &request.headers);
            match request.timeout {
                Some(timeout) => builder
                    .config()
                    .timeout_global(Some(timeout))
                    .build()
                    .send(&request.body),
                None => builder.send(&request.body),
            }
        }
    }
    .map_err(|err| classify_transport_error(&err))?;

    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    Ok(FetchResponse {
        status,
        content_type,
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
    head_timeout: Duration,
    cancel: &dyn Fn() -> bool,
) -> NetResult<StreamOutcome> {
    stream_completion_observed(
        client,
        endpoint,
        provider,
        plan,
        head_timeout,
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
    head_timeout: Duration,
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

    let mut request = FetchRequest::post(url, encoded.into_bytes())
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

    let response = client.send(request)?;

    if response.status >= 400 {
        let text = read_bounded_text(response.body, 64 * 1024);
        let sanitized = redact::redact(&text);
        let mut error = NetError::classify_status(response.status, &sanitized).with_hint(format!(
            "provider `{}` rejected the request",
            provider.name()
        ));
        if response.status == 429 {
            // A rate limit is retryable, and the endpoint may have said when.
            error = error.with_retry_after(1000);
        }
        return Err(error);
    }

    read_stream(response.body, provider, head_timeout, cancel, observe)
}

/// One response to an outbound request that is not a model completion.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Fetched {
    /// HTTP status of the final response.
    pub status: u16,
    /// Media type as reported, parameters included.
    pub content_type: String,
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

/// Fetches a URL for a tool.
///
/// Lives here rather than with the tool because every outbound request has to
/// pass through this module: it is the one place where the address, scheme, and
/// credential refusals are enforced, and a second client elsewhere would be a
/// way around them.
///
/// Absent on a target where the built-in client cannot build. Such a target
/// reaches the endpoint through [`stream_completion`] and its own [`Fetch`], or
/// not at all.
#[cfg(not(target_family = "wasm"))]
pub fn fetch_url(url: &str, accept: &str, timeout: Duration) -> NetResult<Fetched> {
    let response = UreqFetch::new().send(
        FetchRequest::get(url)
            .with_header("user-agent", TOOL_USER_AGENT)
            .with_header("accept", accept)
            .with_timeout(Some(timeout)),
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

    // The client carries no overall timeout, because a streaming generation is
    // expected to be long. A listing is a small document, so it is bounded here
    // rather than leaving a stalled endpoint to hold the command open.
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

/// Reads and reduces a streaming response body.
///
/// The body is read a line at a time, so each frame is reduced and reported as
/// it arrives. Collecting the body first would hold the whole answer back until
/// the response ended, which is the behavior this path exists to avoid.
///
/// Public because a host that reaches an endpoint through its own [`Fetch`],
/// including one that runs where the built-in client cannot compile, reduces
/// the body with the same decoder, limits, and head timeout rather than a
/// second implementation that can disagree with this one.
pub fn read_stream(
    body: Box<dyn Read + Send>,
    provider: &dyn Provider,
    head_timeout: Duration,
    cancel: &dyn Fn() -> bool,
    observe: &mut dyn FnMut(&ProviderEvent),
) -> NetResult<StreamOutcome> {
    let limits = provider.limits();
    let mut decoder = Decoder::new(limits);
    let mut reducer = provider.reducer();
    let mut outcome = StreamOutcome::default();
    let mut frame_buffer: Vec<Event> = Vec::new();

    let reader = BufReader::new(body);
    let started = Instant::now();
    let head_deadline = head_timeout;
    let mut saw_any_event = false;

    for line in reader.split(b'\n') {
        if cancel() {
            return Err(NetError::new(
                FailureKind::Cancelled,
                "the request was cancelled",
            ));
        }

        let line = line.map_err(NetError::from)?;
        let blank = line.is_empty();

        let mut chunk = line;
        chunk.push(b'\n');
        decoder
            .push(&chunk, &mut frame_buffer)
            .map_err(NetError::from)?;

        for event in frame_buffer.drain(..) {
            saw_any_event = true;
            // Every payload reaches the reducer, including the terminator,
            // because the reducer is the authority on what a terminal payload
            // means for its dialect and it is the one that checks the stream
            // ended legally.
            if event.is_empty() && event.name.is_none() {
                continue;
            }
            let before = outcome.events.len();
            reducer
                .apply(Some(&event.data), &mut outcome.events)
                .map_err(NetError::from)?;
            report_new(&outcome.events, before, observe);
        }

        // Judged after the line is decoded, because the blank line that
        // completes the first event is that event arriving, not more waiting.
        if blank && !saw_any_event && started.elapsed() > head_deadline {
            return Err(NetError::new(
                FailureKind::Timeout,
                "the endpoint produced no output within the head timeout",
            )
            .with_hint("raise provider_head_timeout_ms for a slow local model"));
        }

        if decoder.is_done() {
            break;
        }
    }

    decoder.finish(&mut frame_buffer).map_err(NetError::from)?;
    for event in frame_buffer.drain(..) {
        if event.is_empty() && event.name.is_none() {
            continue;
        }
        let before = outcome.events.len();
        reducer
            .apply(Some(&event.data), &mut outcome.events)
            .map_err(NetError::from)?;
        report_new(&outcome.events, before, observe);
    }

    // Signals end of transport. A reducer that never saw a legal terminator
    // reports an incomplete stream, which is retryable, rather than allowing a
    // truncated response to look like a success.
    let before = outcome.events.len();
    reducer
        .apply(None, &mut outcome.events)
        .map_err(NetError::from)?;
    report_new(&outcome.events, before, observe);

    outcome.finish = Some(reducer.finish().map_err(NetError::from)?);
    outcome.usage = reducer.usage();
    outcome.replay = reducer.replay();
    Ok(outcome)
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
