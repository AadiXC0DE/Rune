//! The embedding API.
//!
//! An embedder supplies the credential, the tools, and the network path. The
//! agent owns one in-memory conversation and drives the turn loop over them.
//! Nothing here reaches a filesystem or a built-in tool: a host that wants a
//! tool passes one in, and a host that wants to control outbound traffic
//! supplies a [`HostFetch`], which is then the only client used.
//!
//! A turn runs on its own thread, so the caller can close the agent, or drop the
//! turn handle, while the turn is in flight. Cancellation is cooperative: every
//! network step and every tool call sees the same flag. Host fetches and tools
//! that ignore it delay cancellation until their blocking work returns.

use std::fmt;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

use rune_agent::History;
use rune_agent::compaction;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{CallResult, Event, StopReason, TurnOutcome};
use rune_core::budget::{Budget, BudgetSet, LimitDescription, LimitName};
use rune_core::config::Layer;
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::tool::ToolSpec;
use rune_net::error::NetError;
use rune_net::message::{ContentPart, ImageRef, validate_tool_specs};
use rune_net::provider::{Provider, ProviderCapabilities, Routing};
use rune_net::stream::Usage;
use rune_net::transport::{AuthStyle, Endpoint, StreamOutcome};
use rune_policy::Outcome;
use rune_tools::contract::{Activity, ExecutionContext, ToolOutput};

/// Version of the checkpoint format written by [`Agent::checkpoint`].
pub const CHECKPOINT_VERSION: u32 = 1;

/// Maximum number of unread events retained by a turn.
///
/// A full queue cancels the turn. [`Turn::result`] then returns `LimitExceeded`
/// with the `sdk_event_queue` invariant, even if the caller later drains it.
pub const EVENT_QUEUE_CAPACITY: usize = 1024;

/// Where a diagnostic value comes from, without host data or credentials.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSource {
    /// A default compiled into Rune.
    CompiledDefault,
    /// An option or limit supplied by the embedding host.
    HostSupplied,
    /// The selected Rune wire implementation, independent of server support.
    DialectImplementation,
}

/// Provenance of the agent's inputs. Values and host implementations are omitted.
#[derive(Clone, Debug, serde::Serialize)]
pub struct AgentSources {
    /// The model identifier is required from the host.
    pub model: DiagnosticSource,
    /// The endpoint is required from the host; its URL is never reported.
    pub endpoint: DiagnosticSource,
    /// The dialect selection comes from the host's options.
    pub dialect: DiagnosticSource,
    /// Explicit host instructions, or `None` when the option was absent.
    pub instructions: Option<DiagnosticSource>,
    /// The tool list, including an empty list, comes from the host.
    pub tools: DiagnosticSource,
    /// Whether the host supplied network access or Rune's client is used.
    pub fetch: DiagnosticSource,
}

/// One limit used by the SDK, including its configured and operational values.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SdkLimit {
    /// Configured value, compiled default, unit, range, and host-provided layer.
    #[serde(flatten)]
    pub configured: LimitDescription,
    /// An explicit override is host supplied even if its layer is `Default`.
    pub origin: DiagnosticSource,
    /// Operational value in the configured unit; `None` means unlimited.
    /// Accounts for emergency ceilings and the SDK's fallbacks. For
    /// `ParallelToolCalls`, this controls wire permission for parallel calls;
    /// host tools themselves execute sequentially.
    pub effective: Option<u64>,
}

/// Credential-free snapshot returned by [`Agent::diagnostics`].
#[derive(Clone, Debug, serde::Serialize)]
pub struct AgentDiagnostics {
    /// Name of the selected Rune dialect implementation.
    pub dialect: &'static str,
    /// Wire features, which a particular server or model may further restrict.
    /// These describe the dialect, not additional SDK prompt options.
    pub capabilities: ProviderCapabilities,
    /// Capabilities are obtained from the dialect, without network discovery.
    pub capability_source: DiagnosticSource,
    /// Whether the dialect can express upstream provider preferences.
    pub routing: bool,
    /// Only limits consumed by the SDK; host tool internals have their own policy.
    pub limits: Vec<SdkLimit>,
    /// Fixed maximum unread events per turn.
    pub event_queue_capacity: usize,
    /// Sources of host inputs, without their contents.
    pub sources: AgentSources,
}

/// Which endpoint shape the agent speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Dialect {
    /// `/chat/completions`, the shape most gateways and local servers speak.
    #[default]
    ChatCompletions,
    /// The messages API, which takes its credential in a header of its own.
    Anthropic,
    /// The responses API.
    Responses,
    /// The native Gemini Generate Content API.
    Gemini,
}

impl Dialect {
    /// Builds the dialect implementation.
    #[must_use]
    fn provider(self) -> Box<dyn Provider> {
        match self {
            Self::ChatCompletions => Box::new(rune_net::chat_completions::ChatCompletions),
            Self::Anthropic => Box::new(rune_net::anthropic::Anthropic),
            Self::Responses => Box::new(rune_net::responses::Responses),
            Self::Gemini => Box::new(rune_net::gemini::Gemini),
        }
    }

    /// Returns how this dialect presents a credential.
    #[must_use]
    const fn auth(self) -> AuthStyle {
        match self {
            Self::Anthropic => AuthStyle::ApiKeyHeader,
            Self::Gemini => AuthStyle::GoogleApiKeyHeader,
            Self::ChatCompletions | Self::Responses => AuthStyle::Bearer,
        }
    }
}

/// One outbound request, in the shape a host fetch receives.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FetchRequest {
    /// Absolute URL.
    pub url: String,
    /// Headers, including the one carrying the credential.
    pub headers: Vec<(String, String)>,
    /// Serialized JSON body.
    pub body: String,
}

impl FetchRequest {
    /// Returns a header value by name, ignoring case.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// What a host fetch returns.
///
/// The body is a reader rather than a buffer. A model answers one token at a
/// time and the agent renders each one as it arrives, so collecting the body
/// first would hold the answer back until the response had finished.
pub struct FetchResponse {
    /// HTTP status.
    pub status: u16,
    /// Response body, which is a server-sent event stream for a model request.
    pub body: Box<dyn Read + Send>,
}

impl fmt::Debug for FetchResponse {
    /// Reports the head only. Reading the body here would consume it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FetchResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl FetchResponse {
    /// Builds a response over a body held in memory.
    #[must_use]
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: Box::new(std::io::Cursor::new(body.into())),
        }
    }
}

/// Cancellation and deadlines for one host fetch attempt, including its body.
///
/// Clone this context into the returned reader and check it during bounded
/// waits, including when no bytes arrive. The SDK cannot interrupt an arbitrary
/// blocking host operation. A cooperative host must release its network work
/// and reader within its documented polling or interruption bound.
#[derive(Clone, Debug)]
pub struct FetchContext {
    /// The turn's cancellation signal, shared with its host tools.
    pub signal: CancelFlag,
    /// Absolute deadline for sending the request and reading its entire body.
    /// `None` means `provider_request_timeout_ms` is unlimited.
    pub deadline: Option<Instant>,
    /// Absolute deadline for obtaining the HTTP response head.
    /// Once headers arrive, the SDK separately bounds initial stream output.
    pub head_deadline: Instant,
    /// Set when this attempt ends, without cancelling the rest of the turn.
    stopped: CancelFlag,
}

impl FetchContext {
    /// Returns true if the turn was cancelled or this attempt has ended.
    ///
    /// Use this rather than only `signal` when interrupting a body reader: a
    /// first-event timeout or a completed stream also ends the attempt.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.signal.is_cancelled() || self.stopped.is_cancelled()
    }

    /// Checks cancellation and the total request deadline.
    ///
    /// Suitable for both sending and body reads. Deadline expiry is `Timeout`;
    /// a cancelled turn or an ended attempt is `Cancelled`.
    pub fn check(&self) -> Result<()> {
        self.signal.check()?;
        self.stopped.check()?;
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(RuneError::new(
                ErrorCode::Timeout,
                "host fetch exceeded the total request deadline",
            ));
        }
        Ok(())
    }

    /// Also checks the response-head deadline, for use until headers arrive.
    /// Do not use this for the body, whose bound is the total deadline.
    pub fn check_head(&self) -> Result<()> {
        self.check()?;
        if Instant::now() >= self.head_deadline {
            return Err(RuneError::new(
                ErrorCode::Timeout,
                "host fetch exceeded the response head deadline",
            ));
        }
        Ok(())
    }
}

/// Stops retained host readers on every exit from an attempt, including errors.
struct FetchStopGuard(CancelFlag);

impl Drop for FetchStopGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Network access, owned by the embedder.
///
/// The agent routes every request through this trait: a host that needs to run
/// in a browser, behind a proxy, on a different thread, or against a recording
/// implements it, and no other client is constructed.
pub trait HostFetch: Send + Sync {
    /// Performs one request.
    ///
    /// Returns as soon as the response head is known; the body is read from the
    /// returned reader, which is what keeps a streamed answer incremental.
    fn post(&self, request: FetchRequest) -> Result<FetchResponse>;

    /// Performs a request and returns its optional `Retry-After` header.
    ///
    /// Hosts that expose response headers should override this method so the
    /// retry policy can honor the endpoint's delay. Existing hosts use backoff.
    fn post_with_retry_after(
        &self,
        request: FetchRequest,
    ) -> Result<(FetchResponse, Option<String>)> {
        self.post(request).map(|response| (response, None))
    }

    /// Performs one attempt with cancellation, deadlines and `Retry-After`.
    ///
    /// The SDK calls this method for every attempt. Cooperative hosts override
    /// it, use `context.check_head()` while sending, and retain a context clone
    /// in the body to call `context.check()` during reads. `is_cancelled()` also
    /// signals when decoding ends, so a silent reader can release its resources
    /// even if the total deadline is disabled.
    ///
    /// The default preserves existing `post` and `post_with_retry_after` hosts.
    /// It checks before and after sending, but cannot bound their blocking send
    /// or interrupt their body. Such hosts must migrate to honor the contract.
    fn post_with_context(
        &self,
        request: FetchRequest,
        context: FetchContext,
    ) -> Result<(FetchResponse, Option<String>)> {
        context.check_head()?;
        let response = self.post_with_retry_after(request)?;
        context.check_head()?;
        Ok(response)
    }
}

/// Performs one request over the workspace transport.
///
/// This is the agent's routing of a request onto the shared [`rune_net`] fetch,
/// which is the only place a client is constructed. Routing here rather than
/// building a client keeps the two paths from drifting apart.
fn dispatch(
    client: &rune_net::transport::UreqFetch,
    request: FetchRequest,
    context: Option<&FetchContext>,
) -> Result<(FetchResponse, Option<String>)> {
    if let Some(context) = context {
        context.check_head()?;
    }
    let parsed = rune_net::fetch::FetchRequest {
        method: rune_net::fetch::Method::Post,
        url: request.url,
        headers: request.headers,
        body: request.body.into_bytes(),
        timeout: context.and_then(|context| {
            context
                .deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
        }),
        head_timeout: context.map(|context| {
            context
                .head_deadline
                .saturating_duration_since(Instant::now())
        }),
        follow_redirects: true,
    };
    let response =
        rune_net::fetch::Fetch::send(client, parsed).map_err(|err| err.to_rune_error())?;
    Ok((
        FetchResponse {
            status: response.status,
            body: response.body,
        },
        response.retry_after,
    ))
}

/// The fetch used when the embedder supplies none.
///
/// Holds one transport for the life of the agent, so its connection pool is
/// reused across every request the conversation makes.
#[derive(Debug)]
struct DefaultFetch {
    client: rune_net::transport::UreqFetch,
}

impl DefaultFetch {
    /// Builds a fetch over the workspace transport.
    fn new() -> Self {
        Self {
            client: rune_net::transport::UreqFetch::new(),
        }
    }
}

impl HostFetch for DefaultFetch {
    fn post(&self, request: FetchRequest) -> Result<FetchResponse> {
        dispatch(&self.client, request, None).map(|(response, _)| response)
    }

    fn post_with_retry_after(
        &self,
        request: FetchRequest,
    ) -> Result<(FetchResponse, Option<String>)> {
        dispatch(&self.client, request, None)
    }

    fn post_with_context(
        &self,
        request: FetchRequest,
        context: FetchContext,
    ) -> Result<(FetchResponse, Option<String>)> {
        dispatch(&self.client, request, Some(&context))
    }
}

/// The cancellation signal handed to host tools and fetches.
///
/// Named here rather than reusing the loop's type so the embedding surface and
/// the loop can change independently.
#[derive(Clone, Debug, Default)]
pub struct CancelFlag {
    cancellation: Cancellation,
}

impl CancelFlag {
    /// Returns a flag that is not yet set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Returns true when cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// Returns an error when cancellation was requested.
    pub fn check(&self) -> Result<()> {
        self.cancellation.check()
    }
}

impl From<Cancellation> for CancelFlag {
    fn from(cancellation: Cancellation) -> Self {
        Self { cancellation }
    }
}

/// What a tool may inspect while it runs.
#[derive(Clone, Debug)]
pub struct HostToolContext {
    /// Cancellation signal for the turn the call belongs to.
    pub signal: CancelFlag,
}

/// An image returned by a host tool.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HostImage {
    /// Media type, for example `image/png`.
    pub media_type: String,
    /// Base64 payload, without a data URL prefix.
    pub data: String,
}

/// What a host tool returns.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HostToolResult {
    /// Text handed to the model.
    pub text: String,
    /// Whether the tool reported a failure.
    pub is_error: bool,
    /// Images produced by the call.
    pub images: Vec<HostImage>,
}

impl HostToolResult {
    /// Builds a successful text result.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            images: Vec::new(),
        }
    }

    /// Builds a failed result.
    ///
    /// A tool failure is information for the model rather than the end of the
    /// turn, so the model can adapt to it.
    #[must_use]
    pub fn failure(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            images: Vec::new(),
        }
    }

    /// Attaches an image to the result.
    #[must_use]
    pub fn with_image(mut self, image: HostImage) -> Self {
        self.images.push(image);
        self
    }
}

/// The body of a host tool.
type HostToolBody =
    dyn Fn(&serde_json::Value, &HostToolContext) -> Result<HostToolResult> + Send + Sync;

/// A tool the embedder supplies.
///
/// The agent advertises it to the model and calls it when the model asks. It
/// carries no permission model of its own: an embedder that needs one decides
/// inside `execute`, where it has the arguments.
pub struct HostTool {
    /// Name the model must call.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema for the arguments object.
    pub input_schema: serde_json::Value,
    /// The implementation.
    pub execute: Arc<HostToolBody>,
}

impl HostTool {
    /// Builds a tool.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
        execute: impl Fn(&serde_json::Value, &HostToolContext) -> Result<HostToolResult>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            execute: Arc::new(execute),
        }
    }

    /// Projects the tool for a model request.
    #[must_use]
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

impl fmt::Debug for HostTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostTool")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

/// Everything an agent needs to run.
pub struct AgentOptions {
    /// Credential sent with every request.
    pub api_key: String,
    /// Model identifier.
    pub model: Option<String>,
    /// System instructions, one entry per paragraph.
    pub instructions: Option<Vec<String>>,
    /// Tools the model may call.
    pub tools: Vec<HostTool>,
    /// Base URL of the endpoint, without the request path.
    pub base_url: String,
    /// Endpoint shape.
    pub dialect: Dialect,
    /// Network access. When absent, the workspace HTTP client is used.
    pub fetch: Option<Arc<dyn HostFetch>>,
    /// Compact the conversation automatically before it outgrows the window. Off by default.
    pub auto_compact: bool,
    /// Context window in tokens used for that decision; the built-in default when absent.
    pub context_window: Option<u64>,
}

impl fmt::Debug for AgentOptions {
    /// Deliberately omits the credential.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentOptions")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("instructions", &self.instructions)
            .field("tools", &self.tools.len())
            .field("dialect", &self.dialect)
            .field(
                "fetch",
                &self.fetch.as_ref().map_or("built-in", |_| "host-supplied"),
            )
            .field("auto_compact", &self.auto_compact)
            .field("context_window", &self.context_window)
            .finish_non_exhaustive()
    }
}

/// Options for one prompt.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PromptOptions {
    /// Model steps allowed for this turn. A nonzero value overrides the agent's
    /// `MaxAgentSteps` limit. Zero uses that limit, which is unlimited by default.
    pub max_steps: u64,
}

/// What a turn produced.
#[derive(Clone, Debug)]
pub struct TurnResult {
    /// How the turn ended.
    pub stop_reason: StopReason,
    /// Assistant text across every step.
    pub text: String,
    /// Usage across every step.
    pub usage: Usage,
    /// Steps taken.
    pub steps: u32,
    /// Tool calls made, in order.
    pub calls: Vec<CallResult>,
    /// Images produced by tool calls during this turn.
    pub images: Vec<HostImage>,
}

/// State shared between the agent and a running turn.
#[derive(Debug, Default)]
struct Conversation {
    /// Durable turns.
    history: Mutex<History>,
    /// Usage accumulated across the conversation, including restored turns.
    usage: Mutex<Usage>,
    /// Identifier source for image parts recorded in the history.
    next_image: AtomicU64,
}

/// Where a running turn publishes its product.
#[derive(Debug, Default)]
struct Completion {
    product: Mutex<Option<Result<TurnProduct>>>,
    ready: Condvar,
}

impl Completion {
    /// Records the product, once.
    fn publish(&self, product: Result<TurnProduct>) {
        {
            let mut slot = lock(&self.product);
            if slot.is_none() {
                *slot = Some(product);
            }
        }
        self.ready.notify_all();
    }

    /// Publishes a failure when the worker ended without recording one.
    fn publish_missing(&self) {
        self.publish(Err(RuneError::new(
            ErrorCode::Internal,
            "the turn ended without a result",
        )));
    }

    /// Waits for the product to be recorded.
    fn wait(&self) -> Result<TurnProduct> {
        let mut slot = lock(&self.product);
        while slot.is_none() {
            slot = self
                .ready
                .wait(slot)
                .unwrap_or_else(PoisonError::into_inner);
        }
        match slot.clone() {
            Some(product) => product,
            None => Err(RuneError::new(
                ErrorCode::Internal,
                "the turn ended without a result",
            )),
        }
    }
}

/// What a turn hands back, before it is projected for the caller.
#[derive(Clone, Debug)]
struct TurnProduct {
    outcome: TurnOutcome,
    images: Vec<HostImage>,
}

/// Clears the claim and completes the turn, whatever the worker does.
struct WorkerGuard {
    claim: Arc<AtomicBool>,
    completion: Arc<Completion>,
}

impl WorkerGuard {
    /// Records the product.
    fn publish(&self, product: Result<TurnProduct>) {
        self.completion.publish(product);
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        // Reached without a publish only when the worker unwound, which must
        // still settle the caller rather than leave it waiting.
        self.completion.publish_missing();
        self.claim.store(false, Ordering::SeqCst);
    }
}

/// The turn handle.
///
/// Dropping it cancels the turn and waits for the worker to stop.
#[derive(Debug)]
pub struct Turn {
    completion: Arc<Completion>,
    events: Receiver<Event>,
    worker: Option<JoinHandle<()>>,
    cancel: Cancellation,
}

impl Turn {
    /// Returns the next event, waiting for one when the turn is still running.
    ///
    /// Returns `None` once the turn has ended. A caller that only needs the
    /// outcome can skip this and call [`Turn::result`], provided the turn emits
    /// no more than [`EVENT_QUEUE_CAPACITY`] events.
    pub fn next_event(&mut self) -> Option<Event> {
        self.events.recv().ok()
    }

    /// Returns an iterator over the remaining events.
    pub fn events(&mut self) -> Events<'_> {
        Events {
            events: &self.events,
        }
    }

    /// Completes the turn and returns its result.
    ///
    /// Waiting here is what makes the caller's lifetime the turn's lifetime: the
    /// worker stops, and any cancellation requested while it ran is reflected in
    /// the stop reason.
    ///
    /// If the unread event queue filled, returns `LimitExceeded` with the
    /// `sdk_event_queue` invariant. Consume events as the turn runs to avoid
    /// this backpressure failure on long responses.
    pub fn result(&mut self) -> Result<TurnResult> {
        self.join();
        let product = self.completion.wait()?;
        Ok(TurnResult {
            stop_reason: product.outcome.stop_reason,
            text: product.outcome.text,
            usage: product.outcome.usage,
            steps: product.outcome.steps,
            calls: product.outcome.calls,
            images: product.images,
        })
    }

    /// Waits for the worker, if it is still running.
    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.join();
    }
}

/// An iterator over a running turn's events.
#[derive(Debug)]
pub struct Events<'a> {
    events: &'a Receiver<Event>,
}

impl Iterator for Events<'_> {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        self.events.recv().ok()
    }
}

/// One conversation with a model.
pub struct Agent {
    model: String,
    instructions: String,
    endpoint: Endpoint,
    dialect: Dialect,
    fetch: Arc<dyn HostFetch>,
    tools: Arc<Vec<HostTool>>,
    specs: Arc<Vec<ToolSpec>>,
    limits: BudgetSet,
    /// The window automatic compaction decides against, when it is on.
    auto_compact: Option<compaction::Window>,
    sources: AgentSources,
    conversation: Arc<Conversation>,
    /// Cancellation for the turn started last, which is the only one that can
    /// still be running.
    cancel: Cancellation,
    claim: Arc<AtomicBool>,
    current: Option<Arc<Completion>>,
    closed: bool,
}

impl fmt::Debug for Agent {
    /// Deliberately omits the endpoint, which holds the credential.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Agent")
            .field("model", &self.model)
            .field("dialect", &self.dialect)
            .field("tools", &self.tools.len())
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // No orphan work: a turn outliving its agent is never what a caller
        // means by dropping the agent.
        self.cancel.cancel();
    }
}

impl Agent {
    /// Builds an agent over one empty conversation.
    pub fn new(options: AgentOptions) -> Result<Self> {
        Self::build(options, false)
    }

    /// Builds an agent for an explicitly anonymous loopback endpoint.
    /// `api_key` must be empty; no authentication header is sent.
    pub fn new_anonymous(options: AgentOptions) -> Result<Self> {
        Self::build(options, true)
    }

    fn build(options: AgentOptions, anonymous: bool) -> Result<Self> {
        let model = options
            .model
            .as_ref()
            .map(|model| model.trim().to_owned())
            .unwrap_or_default();
        if model.is_empty() {
            return Err(
                RuneError::missing_field("model").with_hint("set `model` in the agent options")
            );
        }
        if !anonymous && options.api_key.trim().is_empty() {
            return Err(RuneError::missing_field("api_key")
                .with_hint("set the credential for the endpoint"));
        }
        rune_net::transport::validate_url(&options.base_url)?;
        let endpoint = if anonymous {
            if !options.api_key.is_empty() {
                return Err(RuneError::invalid_field(
                    "api_key",
                    "an anonymous endpoint must not have a credential",
                ));
            }
            Endpoint::anonymous(options.base_url)?
        } else {
            Endpoint::new(options.base_url, options.api_key).with_auth(options.dialect.auth())
        };

        let specs: Vec<ToolSpec> = options.tools.iter().map(HostTool::spec).collect();
        validate_tool_specs(&specs)?;

        let instructions = options
            .instructions
            .as_ref()
            .map(|paragraphs| paragraphs.join("\n\n"))
            .unwrap_or_default();

        let fetch: Arc<dyn HostFetch> = options
            .fetch
            .clone()
            .unwrap_or_else(|| Arc::new(DefaultFetch::new()));

        let sources = AgentSources {
            model: DiagnosticSource::HostSupplied,
            endpoint: DiagnosticSource::HostSupplied,
            dialect: DiagnosticSource::HostSupplied,
            instructions: options
                .instructions
                .as_ref()
                .map(|_| DiagnosticSource::HostSupplied),
            tools: DiagnosticSource::HostSupplied,
            fetch: if options.fetch.is_some() {
                DiagnosticSource::HostSupplied
            } else {
                DiagnosticSource::CompiledDefault
            },
        };

        let mut history = History::new();
        history.set_instructions(instructions.clone());

        Ok(Self {
            model,
            instructions,
            endpoint,
            dialect: options.dialect,
            fetch,
            specs: Arc::new(specs),
            tools: Arc::new(options.tools),
            limits: BudgetSet::new(),
            auto_compact: options.auto_compact.then(|| {
                compaction::Window(
                    options
                        .context_window
                        .unwrap_or(rune_net::catalog::DEFAULT_CONTEXT_WINDOW),
                )
            }),
            sources,
            conversation: Arc::new(Conversation {
                history: Mutex::new(history),
                ..Conversation::default()
            }),
            cancel: Cancellation::new(),
            claim: Arc::new(AtomicBool::new(false)),
            current: None,
            closed: false,
        })
    }

    /// Builds an agent over the conversation a checkpoint carries.
    pub fn restore(options: AgentOptions, checkpoint: &[u8]) -> Result<Self> {
        Self::restore_with_limits(options, checkpoint, BudgetSet::new())
    }

    /// Builds an agent over a checkpoint under explicit limits.
    ///
    /// The checkpoint size is checked against the history bound, so a host that
    /// accepts larger conversations restores with the limits it runs under
    /// rather than a second agent rebuilt afterwards.
    pub fn restore_with_limits(
        options: AgentOptions,
        checkpoint: &[u8],
        limits: BudgetSet,
    ) -> Result<Self> {
        let mut agent = Self::new(options)?.with_limits(limits);
        agent.adopt(checkpoint)?;
        Ok(agent)
    }

    /// Replaces the limits used for this agent.
    ///
    /// The agent is otherwise unconstrained by product settings: an embedder
    /// that wants the workspace defaults passes them in.
    #[must_use]
    pub fn with_limits(mut self, limits: BudgetSet) -> Self {
        self.limits = limits;
        self
    }

    /// Reports effective SDK limits, dialect capabilities, and input sources.
    ///
    /// Makes no network requests and includes no credentials, URLs, model
    /// identifiers, instructions, tool definitions, or conversation data.
    /// Limits describe the agent; a nonzero [`PromptOptions::max_steps`] may
    /// override `MaxAgentSteps` for an individual turn. SDK diagnostics do not
    /// infer model context capacity or policies inside host tools and fetches.
    #[must_use]
    pub fn diagnostics(&self) -> AgentDiagnostics {
        let provider = self.dialect.provider();
        let timeouts = rune_net::transport::RequestTimeouts::from_limits(&self.limits);
        let limits = self
            .limits
            .describe()
            .into_iter()
            .filter_map(|configured| {
                let name = configured.name;
                let effective = match name {
                    LimitName::MaxAgentSteps => configured.value.value().filter(|value| *value > 0),
                    LimitName::ProviderHeadTimeoutMs => Some(timeouts.head.as_millis() as u64),
                    LimitName::ProviderRequestTimeoutMs => {
                        timeouts.total.map(|timeout| timeout.as_millis() as u64)
                    }
                    LimitName::ProviderMaxAttempts | LimitName::PromptHistoryBytes => {
                        Some(self.limits.get_usize(name).max(1) as u64)
                    }
                    LimitName::MaxToolResultBytes | LimitName::ParallelToolCalls => {
                        Some(self.limits.get_usize(name) as u64)
                    }
                    _ => return None,
                };
                Some(SdkLimit {
                    origin: if configured.source.is_some() {
                        DiagnosticSource::HostSupplied
                    } else {
                        DiagnosticSource::CompiledDefault
                    },
                    configured,
                    effective,
                })
            })
            .collect();
        AgentDiagnostics {
            dialect: provider.name(),
            capabilities: provider.capabilities(),
            capability_source: DiagnosticSource::DialectImplementation,
            routing: provider.routing() == Routing::Supported,
            limits,
            event_queue_capacity: EVENT_QUEUE_CAPACITY,
            sources: self.sources.clone(),
        }
    }

    /// Returns the model identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the tools this agent advertises.
    #[must_use]
    pub fn tools(&self) -> &[HostTool] {
        &self.tools
    }

    /// Returns the usage accumulated by the conversation.
    #[must_use]
    pub fn usage(&self) -> Usage {
        *lock(&self.conversation.usage)
    }

    /// Returns true when the agent has been closed.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Pins an explicit user constraint verbatim in every subsequent request.
    ///
    /// Pins survive compaction and checkpoint restoration. Exact duplicates
    /// return false. Empty constraints, closed agents, running turns, and pins
    /// exceeding the checkpoint's `PromptHistoryBytes` bound are refused
    /// without changing the conversation.
    pub fn pin_constraint(&mut self, text: impl Into<String>) -> Result<bool> {
        if self.closed || self.claim.load(Ordering::SeqCst) {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                "pinning requires an open agent at a settled turn boundary",
            ));
        }
        let mut history = lock(&self.conversation.history).clone();
        let added = history.pin_constraint(text)?;
        if added {
            self.serialize_checkpoint(history.clone())?;
            *lock(&self.conversation.history) = history;
        }
        Ok(added)
    }

    /// Summarizes an older prefix of the conversation at a settled turn boundary.
    ///
    /// The synchronous host callback receives summary instructions and only the
    /// prefix being removed, including tool activity, plus explicit pinned user
    /// constraints kept independently of its output. It must return a complete
    /// summary or an error, including when its work is cancelled or truncated.
    /// No provider request is made by the SDK itself.
    ///
    /// Uses the shared compaction planner and this agent's limits to retain recent
    /// turns without splitting tool calls from their results. Returns the number
    /// of records removed, or zero without calling the summarizer when there is
    /// no prefix to compact. Closed agents and running turns are refused.
    ///
    /// Callback errors, unusable summaries, invalid tool pairs, and checkpoints
    /// exceeding `PromptHistoryBytes` leave the conversation and usage unchanged.
    /// Successful compaction preserves usage and retained provider replay data.
    pub fn compact(&mut self, summarizer: impl FnOnce(&str) -> Result<String>) -> Result<usize> {
        if self.closed {
            return Err(
                RuneError::new(ErrorCode::InvalidState, "the agent is closed")
                    .with_hint("build a new agent, or restore one from a checkpoint"),
            );
        }
        if self.claim.load(Ordering::SeqCst) {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                "a turn is running, so the conversation cannot be compacted",
            )
            .with_hint("wait for the running turn, or drop its handle to cancel it"));
        }

        let mut history = lock(&self.conversation.history).clone();
        history.validate()?;
        let Some(plan) = compaction::plan(&history, &self.limits) else {
            return Ok(0);
        };
        let summary = summarizer(&compaction::render_summary_request(&history, &plan))?;
        compaction::validate_summary(&summary)?;
        let removed = compaction::apply(&mut history, &plan, compaction::wrap_summary(&summary));
        history.validate()?;
        // Check replayability before installing anything. A host may compact
        // precisely because the current checkpoint is too large.
        self.serialize_checkpoint(history.clone())?;
        *lock(&self.conversation.history) = history;
        Ok(removed)
    }

    /// Runs one turn.
    ///
    /// Only one turn runs at a time. A second prompt while one is in flight is
    /// refused rather than queued, because a conversation has one order and a
    /// queue would invent a different one.
    pub fn prompt(&mut self, input: impl Into<String>, options: PromptOptions) -> Result<Turn> {
        if self.closed {
            return Err(
                RuneError::new(ErrorCode::InvalidState, "the agent is closed")
                    .with_hint("build a new agent, or restore one from a checkpoint"),
            );
        }
        if self.claim.swap(true, Ordering::SeqCst) {
            return Err(
                RuneError::new(ErrorCode::InvalidState, "a turn is already running")
                    .with_hint("wait for the running turn, or drop its handle to cancel it"),
            );
        }
        // Each turn gets a flag of its own. Dropping a handle cancels its turn,
        // and the handle of a finished turn is routinely dropped after the next
        // one has started, as in `turn = agent.prompt(..)`, so a flag shared
        // between turns would cancel the new one.
        self.cancel = Cancellation::new();
        let completion = Arc::new(Completion::default());
        self.current = Some(Arc::clone(&completion));

        lock(&self.conversation.history).push_user(input.into());

        let (events, receiver) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let context = TurnContext {
            conversation: Arc::clone(&self.conversation),
            cancel: self.cancel.clone(),
            events,
            backpressured: AtomicBool::new(false),
            model: self.model.clone(),
            instructions: self.instructions.clone(),
            endpoint: self.endpoint.clone(),
            dialect: self.dialect,
            fetch: Arc::clone(&self.fetch),
            tools: Arc::clone(&self.tools),
            specs: Arc::clone(&self.specs),
            limits: self.limits.clone(),
            auto_compact: self.auto_compact,
            max_steps: if options.max_steps == 0 {
                self.limits
                    .get(LimitName::MaxAgentSteps)
                    .value()
                    .unwrap_or(0)
            } else {
                options.max_steps
            },
        };

        let guard = WorkerGuard {
            claim: Arc::clone(&self.claim),
            completion: Arc::clone(&completion),
        };
        let worker = std::thread::Builder::new()
            .name("rune-sdk-turn".to_owned())
            .spawn(move || {
                let settled = guard;
                let product = run_turn(&context);
                // Backpressure uses cancellation to stop cooperative work, but
                // must remain an explicit failure rather than a cancelled or
                // successful result, including overflow of the final event.
                let product = if context.backpressured.load(Ordering::SeqCst) {
                    Err(RuneError::new(
                        ErrorCode::LimitExceeded,
                        format!(
                            "SDK event queue backpressure: {EVENT_QUEUE_CAPACITY} unread events"
                        ),
                    )
                    .with_invariant("sdk_event_queue")
                    .with_observed(EVENT_QUEUE_CAPACITY.to_string())
                    .with_hint("consume Turn::events or Turn::next_event while the turn runs"))
                } else {
                    product
                };
                settled.publish(product);
            });

        let worker = match worker {
            Ok(worker) => worker,
            Err(err) => {
                self.claim.store(false, Ordering::SeqCst);
                return Err(RuneError::new(
                    ErrorCode::Internal,
                    format!("the turn worker could not start: {err}"),
                ));
            }
        };

        Ok(Turn {
            completion,
            events: receiver,
            worker: Some(worker),
            cancel: self.cancel.clone(),
        })
    }

    /// Closes the agent, cancelling any turn in flight.
    ///
    /// The cancelled turn resolves with a cancelled stop reason; a checkpoint
    /// taken while a turn was running is refused, so closing is the only way to
    /// change an agent's state from the outside during a turn.
    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.cancel.cancel();

        // Wait for the turn to settle, so a caller that closes and then reads
        // the checkpoint sees the final state rather than a race.
        if let Some(completion) = self.current.take() {
            let _ = completion.wait()?;
        }
        Ok(())
    }

    /// Returns opaque bytes carrying the conversation and the usage.
    ///
    /// The format is versioned, and deliberately excludes the credential, the
    /// model, the instructions, and the tools: a checkpoint is data a host may
    /// store or move between processes, and none of those belong in it.
    ///
    /// The serialized bytes must fit within `PromptHistoryBytes`, the same cap
    /// used by restoration. Exceeding it returns `TooLarge` for `checkpoint`.
    pub fn checkpoint(&self) -> Result<Vec<u8>> {
        if self.claim.load(Ordering::SeqCst) {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                "a turn is running, so the conversation is not at a boundary",
            )
            .with_hint("wait for the turn to settle, or close the agent to cancel it"));
        }

        let history = lock(&self.conversation.history).clone();
        self.serialize_checkpoint(history)
    }

    /// Serializes a candidate history under the same bound as restoration.
    fn serialize_checkpoint(&self, mut history: History) -> Result<Vec<u8>> {
        // The instructions are a property of the host, not of the
        // conversation, and they never enter the checkpoint.
        history.set_instructions(String::new());
        let body = CheckpointBody {
            version: CHECKPOINT_VERSION,
            history,
            usage: *lock(&self.conversation.usage),
        };
        let checkpoint = serde_json::to_vec(&body)?;
        let cap = self.limits.get_usize(LimitName::PromptHistoryBytes).max(1);
        if checkpoint.len() > cap {
            return Err(RuneError::too_large("checkpoint", checkpoint.len(), cap));
        }
        Ok(checkpoint)
    }

    /// Adopts the conversation a checkpoint carries.
    fn adopt(&mut self, checkpoint: &[u8]) -> Result<()> {
        let cap = self.limits.get_usize(LimitName::PromptHistoryBytes).max(1);
        if checkpoint.len() > cap {
            return Err(RuneError::too_large("checkpoint", checkpoint.len(), cap));
        }

        let value: serde_json::Value = serde_json::from_slice(checkpoint).map_err(|err| {
            RuneError::invariant("checkpoint", format!("it is not valid JSON: {err}"))
        })?;
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| RuneError::invariant("checkpoint", "it declares no format version"))?;
        if version != u64::from(CHECKPOINT_VERSION) {
            return Err(RuneError::new(
                ErrorCode::UnsupportedVersion,
                format!(
                    "checkpoint version {version} is not supported by this build; expected {CHECKPOINT_VERSION}"
                ),
            )
            .with_hint("the checkpoint was written by another version of this library"));
        }

        let body: CheckpointBody = serde_json::from_value(value).map_err(|err| {
            RuneError::invariant("checkpoint", format!("its payload is malformed: {err}"))
        })?;
        body.history.validate()?;

        let next_image = highest_image(&body.history).saturating_add(1).max(1);
        let mut history = body.history;
        history.set_instructions(self.instructions.clone());
        *lock(&self.conversation.history) = history;
        *lock(&self.conversation.usage) = body.usage;
        self.conversation
            .next_image
            .store(next_image, Ordering::SeqCst);
        Ok(())
    }
}

/// The checkpoint payload.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct CheckpointBody {
    version: u32,
    history: History,
    usage: Usage,
}

/// Returns one past the highest image identifier the history references.
fn highest_image(history: &History) -> u64 {
    let mut highest = 0_u64;
    for turn in history.turns() {
        for part in &turn.parts {
            if let ContentPart::Image { image } = part {
                highest = highest.max(image.id);
            }
        }
    }
    highest
}

/// Everything one turn needs.
struct TurnContext {
    conversation: Arc<Conversation>,
    cancel: Cancellation,
    events: SyncSender<Event>,
    backpressured: AtomicBool,
    model: String,
    instructions: String,
    endpoint: Endpoint,
    dialect: Dialect,
    fetch: Arc<dyn HostFetch>,
    tools: Arc<Vec<HostTool>>,
    specs: Arc<Vec<ToolSpec>>,
    limits: BudgetSet,
    max_steps: u64,
    auto_compact: Option<compaction::Window>,
}

impl TurnContext {
    /// Reports an event.
    ///
    /// Never waits for the consumer. Once full, the queue fails the turn and
    /// stops retaining events; cooperative cancellation stops provider/tools.
    fn emit(&self, event: Event) {
        if self.backpressured.load(Ordering::SeqCst) {
            return;
        }
        if let Err(TrySendError::Full(_)) = self.events.try_send(event) {
            self.backpressured.store(true, Ordering::SeqCst);
            self.cancel.cancel();
        }
    }

    /// Adds one request's usage to the conversation's total.
    fn store_usage(&self, usage: Usage) {
        let mut shared = lock(&self.conversation.usage);
        *shared = shared.merge_sum(usage);
    }

    /// Registers an image and returns the reference recorded in the history.
    fn register_image(&self, image: &HostImage) -> ImageRef {
        let id = self.conversation.next_image.fetch_add(1, Ordering::SeqCst);
        ImageRef {
            id,
            media_type: image.media_type.clone(),
            encoded_bytes: u64::try_from(image.data.len()).unwrap_or(u64::MAX),
        }
    }
}

/// Runs one turn on the loop every surface shares.
fn run_turn(context: &TurnContext) -> Result<TurnProduct> {
    let host = SdkHost::new(context)?;
    let outcome = {
        // The conversation is not readable while a turn runs, so the history
        // is held for the whole turn.
        let mut history = lock(&context.conversation.history);
        rune_agent::turn::run_turn(&mut history, &host).and_then(TurnOutcome::into_strict)?
    };
    // The SDK's answer is the text of every step, not only the last one.
    let mut outcome = outcome;
    outcome.text = outcome
        .narration
        .iter()
        .filter_map(|entry| match entry {
            rune_agent::turn::Narration::Text(text) => Some(text.as_str()),
            rune_agent::turn::Narration::Call(_) => None,
        })
        .collect();
    let images = std::mem::take(&mut *lock(&host.images));
    Ok(TurnProduct { outcome, images })
}

/// The embedder's tools, endpoint and fetch, presented to the shared loop.
struct SdkHost<'a> {
    context: &'a TurnContext,
    provider: Box<dyn Provider>,
    limits: BudgetSet,
    steering: SteeringQueue,
    /// Stop flags of the attempts in flight, set once the loop is done with
    /// each, so a host reader blocked on its body is released.
    attempts: Arc<Mutex<Vec<CancelFlag>>>,
    /// Images of each executed call, in the order the calls ran, until their
    /// results are recorded.
    pending_images: Mutex<std::collections::VecDeque<Vec<HostImage>>>,
    /// Every image the turn produced.
    images: Mutex<Vec<HostImage>>,
}

impl<'a> SdkHost<'a> {
    fn new(context: &'a TurnContext) -> Result<Self> {
        let mut limits = context.limits.clone();
        // Zero means unlimited steps, which is the documented default.
        let steps = if context.max_steps == 0 {
            Budget::Unbounded
        } else {
            Budget::Bounded(context.max_steps)
        };
        limits.set(LimitName::MaxAgentSteps, steps, Layer::CommandLine)?;
        Ok(Self {
            context,
            provider: context.dialect.provider(),
            limits,
            // Nothing steers an embedded turn; the queue stays empty.
            steering: SteeringQueue::new(1),
            attempts: Arc::new(Mutex::new(Vec::new())),
            pending_images: Mutex::new(std::collections::VecDeque::new()),
            images: Mutex::new(Vec::new()),
        })
    }
}

impl rune_agent::turn::Host for SdkHost<'_> {
    fn dialect(&self) -> &dyn Provider {
        self.provider.as_ref()
    }

    fn endpoint(&self) -> &Endpoint {
        &self.context.endpoint
    }

    fn model(&self) -> String {
        self.context.model.clone()
    }

    fn instructions(&self) -> String {
        self.context.instructions.clone()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.context.specs.as_ref().clone()
    }

    // Host tools are opaque, so none is treated as a read the loop may skip.
    fn tool_activity(&self, _name: &str) -> Option<Activity> {
        Some(Activity::Execute)
    }

    fn fetch(&self) -> Box<dyn rune_net::fetch::Fetch> {
        Box::new(SdkFetch {
            fetch: Arc::clone(&self.context.fetch),
            cancel: self.context.cancel.clone(),
            head: rune_net::transport::RequestTimeouts::from_limits(&self.limits).head,
            attempts: Arc::clone(&self.attempts),
        })
    }

    fn attempt_finished(&self) {
        for stop in lock(&self.attempts).drain(..) {
            stop.cancel();
        }
    }

    fn observe_response(&self, outcome: &StreamOutcome) {
        self.context.store_usage(outcome.usage);
    }

    fn prepare_request(
        &self,
        history: &mut History,
        plan: &mut rune_net::provider::RequestPlan,
        client: &dyn rune_net::fetch::Fetch,
    ) -> Result<()> {
        match self.context.auto_compact {
            Some(window) => {
                compaction::auto_compact(history, plan, self, client, window).map(|_| ())
            }
            None => Ok(()),
        }
    }

    fn context_replaced(&self, report: &compaction::CompactionReport) {
        // The summary request is billed like any other.
        self.context.store_usage(report.usage);
    }

    fn emit(&self, event: Event) {
        self.context.emit(event);
    }

    fn execute(&self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        // A full event queue cancels the turn, which must stop a tool before
        // host code runs, including when the queue filled on its start event.
        self.context.cancel.check()?;
        let tool = self
            .context
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .ok_or_else(|| {
                RuneError::new(ErrorCode::NotFound, format!("no tool named `{name}`"))
            })?;
        let host_context = HostToolContext {
            signal: self.context.cancel.clone().into(),
        };
        let result = (tool.execute)(arguments, &host_context);
        // The loop marks every call executed except one that was cancelled,
        // and asks for attachments in the same order, so each executed call
        // leaves exactly one entry.
        let images = match &result {
            Ok(result) => result.images.clone(),
            Err(err) if err.code() == ErrorCode::Cancelled => return Err(err.clone()),
            Err(_) => Vec::new(),
        };
        lock(&self.pending_images).push_back(images);
        let result = result?;
        let cap = self.limits.get_usize(LimitName::MaxToolResultBytes);
        let produced = u64::try_from(result.text.len()).unwrap_or(u64::MAX);
        Ok(ToolOutput {
            text: bound_text(&result.text, cap.saturating_sub(TRUNCATION_MARKER.len())),
            is_error: result.is_error,
            produced_bytes: produced,
            plan_update: None,
            full_text: None,
            diff: None,
        })
    }

    fn tool_attachments(&self, result: &CallResult) -> Vec<ContentPart> {
        if !result.executed {
            return Vec::new();
        }
        let images = lock(&self.pending_images).pop_front().unwrap_or_default();
        let parts = images
            .iter()
            .map(|image| ContentPart::Image {
                image: self.context.register_image(image),
            })
            .collect();
        lock(&self.images).extend(images);
        parts
    }

    // The embedder chose these tools, so the loop applies no policy of its own.
    fn decide(&self, _name: &str, _target: Option<&str>) -> (Outcome, String) {
        (Outcome::Allow, "a host tool".to_owned())
    }

    fn context(&self) -> ExecutionContext {
        ExecutionContext::new(camino::Utf8PathBuf::from("/"))
    }

    fn limits(&self) -> BudgetSet {
        self.limits.clone()
    }

    fn cancellation(&self) -> Cancellation {
        self.context.cancel.clone()
    }

    fn steering(&self) -> &SteeringQueue {
        &self.steering
    }

    fn settles_cancellation(&self) -> bool {
        true
    }

    fn retains_results(&self) -> bool {
        false
    }
}

/// Sends the shared loop's requests through the embedder's [`HostFetch`].
struct SdkFetch {
    fetch: Arc<dyn HostFetch>,
    cancel: Cancellation,
    head: std::time::Duration,
    attempts: Arc<Mutex<Vec<CancelFlag>>>,
}

impl rune_net::fetch::Fetch for SdkFetch {
    fn send(
        &self,
        request: rune_net::fetch::FetchRequest,
    ) -> rune_net::error::NetResult<rune_net::fetch::FetchResponse> {
        let started = Instant::now();
        let out_of_range = || {
            NetError::from_rune_error(RuneError::new(
                ErrorCode::InvalidConfiguration,
                "host fetch deadline is out of range",
            ))
        };
        let context = FetchContext {
            signal: self.cancel.clone().into(),
            deadline: request
                .timeout
                .map(|total| started.checked_add(total).ok_or_else(out_of_range))
                .transpose()?,
            head_deadline: started.checked_add(self.head).ok_or_else(out_of_range)?,
            stopped: CancelFlag::new(),
        };
        // The attempt ends when the loop is done with it, or when the body is
        // dropped, whichever comes first; either releases a waiting reader.
        lock(&self.attempts).push(context.stopped.clone());
        let stop = FetchStopGuard(context.stopped.clone());
        let host_request = FetchRequest {
            url: request.url,
            headers: request.headers,
            body: String::from_utf8_lossy(&request.body).into_owned(),
        };
        let (response, retry_after) = self
            .fetch
            .post_with_context(host_request, context.clone())
            .map_err(NetError::from_rune_error)?;
        context.check_head().map_err(NetError::from_rune_error)?;
        Ok(rune_net::fetch::FetchResponse {
            status: response.status,
            content_type: "text/event-stream".to_owned(),
            retry_after,
            location: None,
            body: Box::new(StoppingBody {
                body: response.body,
                _stop: stop,
            }),
        })
    }
}

/// A host response body that ends its attempt when dropped.
struct StoppingBody {
    body: Box<dyn Read + Send>,
    _stop: FetchStopGuard,
}

impl Read for StoppingBody {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.body.read(buffer)
    }
}

/// Appended to a host tool result cut at its byte bound.
const TRUNCATION_MARKER: &str = "\n... (truncated)";

/// Truncates a tool result to the configured bound, keeping the marker.
pub(crate) fn bound_text(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut end = cap.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut out = text[..end].to_owned();
    out.push_str(TRUNCATION_MARKER);
    out
}

/// Locks a mutex, ignoring poisoning.
///
/// A panic inside a turn must not make the conversation permanently unreadable:
/// the data behind the lock is still consistent, because every writer holds it
/// only while replacing a value.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
