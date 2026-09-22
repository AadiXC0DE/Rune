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
//! network step and every tool call sees the same flag, and a tool that ignores
//! it delays the cancellation rather than preventing it.

use std::fmt;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use rune_agent::History;
use rune_agent::steering::Cancellation;
use rune_agent::turn::{CallResult, Event, PreparedCall, StopReason, TurnOutcome};
use rune_core::budget::{BudgetSet, LimitName};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::tool::ToolSpec;
use rune_net::error::NetError;
use rune_net::message::{ContentPart, ImageRef, validate_tool_specs};
use rune_net::provider::{Provider, RequestPlan, ToolChoice};
use rune_net::redact;
use rune_net::sse::{Decoder, Event as SseEvent};
use rune_net::stream::{FinishReason, ProviderEvent, Usage};
use rune_net::transport::{AuthStyle, Endpoint, MAX_RESPONSE_BYTES, StreamOutcome};
use rune_tools::contract::{Activity, ToolOutput};

/// Version of the checkpoint format written by [`Agent::checkpoint`].
pub const CHECKPOINT_VERSION: u32 = 1;

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
}

impl Dialect {
    /// Builds the dialect implementation.
    #[must_use]
    fn provider(self) -> Box<dyn Provider> {
        match self {
            Self::ChatCompletions => Box::new(rune_net::chat_completions::ChatCompletions),
            Self::Anthropic => Box::new(rune_net::anthropic::Anthropic),
            Self::Responses => Box::new(rune_net::responses::Responses),
        }
    }

    /// Returns how this dialect presents a credential.
    #[must_use]
    const fn auth(self) -> AuthStyle {
        match self {
            Self::Anthropic => AuthStyle::ApiKeyHeader,
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
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FetchResponse {
    /// HTTP status.
    pub status: u16,
    /// Response body, which is a server-sent event stream for a model request.
    pub body: Vec<u8>,
}

impl FetchResponse {
    /// Builds a successful response.
    #[must_use]
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

/// Network access, owned by the embedder.
///
/// The agent routes every request through this trait: a host that needs to run
/// in a browser, behind a proxy, on a different thread, or against a recording
/// implements it, and no other client is constructed.
pub trait HostFetch: Send + Sync {
    /// Performs one request.
    fn post(&self, request: FetchRequest) -> Result<FetchResponse>;
}

/// The fetch used when the embedder supplies none.
#[derive(Debug)]
struct DefaultFetch {
    client: ureq::Agent,
}

impl DefaultFetch {
    /// Builds a fetch over the workspace HTTP client.
    fn new() -> Self {
        Self {
            client: rune_net::transport::agent(),
        }
    }
}

impl HostFetch for DefaultFetch {
    fn post(&self, request: FetchRequest) -> Result<FetchResponse> {
        let mut builder = self.client.post(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }

        let response = builder.send(&request.body).map_err(|err| {
            let kind = match err {
                ureq::Error::Timeout(_) => rune_net::error::FailureKind::Timeout,
                _ => rune_net::error::FailureKind::Network,
            };
            NetError::new(kind, redact::redact(&err.to_string())).to_rune_error()
        })?;

        let status = response.status().as_u16();
        let mut body = Vec::new();
        // One byte past the bound, so an oversized body is refused rather than
        // silently truncated into a decode failure.
        let cap = usize::try_from(MAX_RESPONSE_BYTES).unwrap_or(usize::MAX);
        let limit = u64::try_from(cap).unwrap_or(u64::MAX);
        response
            .into_body()
            .into_reader()
            .take(limit.saturating_add(1))
            .read_to_end(&mut body)?;
        if body.len() > cap {
            return Err(RuneError::too_large("fetch.body", body.len(), cap)
                .with_hint("the endpoint returned more than one response may hold"));
        }

        Ok(FetchResponse { status, body })
    }
}

/// The cancellation signal handed to a host tool.
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
            .finish_non_exhaustive()
    }
}

/// Options for one prompt.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PromptOptions {
    /// Model steps allowed for this turn. Zero means unlimited.
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
    /// outcome can skip this and call [`Turn::result`].
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
    conversation: Arc<Conversation>,
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
        if options.api_key.trim().is_empty() {
            return Err(RuneError::missing_field("api_key")
                .with_hint("set the credential for the endpoint"));
        }
        rune_net::transport::validate_url(&options.base_url)?;

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

        let mut history = History::new();
        history.set_instructions(instructions.clone());

        Ok(Self {
            model,
            instructions,
            endpoint: Endpoint::new(options.base_url, options.api_key)
                .with_auth(options.dialect.auth()),
            dialect: options.dialect,
            fetch,
            specs: Arc::new(specs),
            tools: Arc::new(options.tools),
            limits: BudgetSet::new(),
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
        let mut agent = Self::new(options)?;
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
        self.cancel.reset();
        let completion = Arc::new(Completion::default());
        self.current = Some(Arc::clone(&completion));

        lock(&self.conversation.history).push_user(input.into());

        let (events, receiver) = mpsc::channel();
        let context = TurnContext {
            conversation: Arc::clone(&self.conversation),
            cancel: self.cancel.clone(),
            events,
            model: self.model.clone(),
            instructions: self.instructions.clone(),
            endpoint: self.endpoint.clone(),
            dialect: self.dialect,
            fetch: Arc::clone(&self.fetch),
            tools: Arc::clone(&self.tools),
            specs: Arc::clone(&self.specs),
            limits: self.limits.clone(),
            max_steps: options.max_steps,
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
    pub fn checkpoint(&self) -> Result<Vec<u8>> {
        if self.claim.load(Ordering::SeqCst) {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                "a turn is running, so the conversation is not at a boundary",
            )
            .with_hint("wait for the turn to settle, or close the agent to cancel it"));
        }

        let history = {
            let mut history = lock(&self.conversation.history).clone();
            // The instructions are a property of the host, not of the
            // conversation, and they never enter the checkpoint.
            history.set_instructions(String::new());
            history
        };
        let body = CheckpointBody {
            version: CHECKPOINT_VERSION,
            history,
            usage: *lock(&self.conversation.usage),
        };
        Ok(serde_json::to_vec(&body)?)
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
    events: Sender<Event>,
    model: String,
    instructions: String,
    endpoint: Endpoint,
    dialect: Dialect,
    fetch: Arc<dyn HostFetch>,
    tools: Arc<Vec<HostTool>>,
    specs: Arc<Vec<ToolSpec>>,
    limits: BudgetSet,
    max_steps: u64,
}

impl TurnContext {
    /// Reports an event.
    ///
    /// A dropped receiver means the caller discarded the turn handle; the turn
    /// still runs to a settled result.
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// Records usage accumulated so far.
    fn store_usage(&self, usage: Usage) {
        let mut shared = lock(&self.conversation.usage);
        *shared = shared.merge_max(usage);
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

/// Runs one turn to completion.
fn run_turn(context: &TurnContext) -> Result<TurnProduct> {
    let provider = context.dialect.provider();
    let mut steps: u32 = 0;
    let mut usage = Usage::default();
    let mut calls: Vec<CallResult> = Vec::new();
    let mut images: Vec<HostImage> = Vec::new();
    let mut text = String::new();

    loop {
        if context.cancel.is_cancelled() {
            return Ok(settle(
                context,
                StopReason::Cancelled,
                text,
                usage,
                steps,
                calls,
                images,
            ));
        }
        // Zero means unlimited steps, which is the documented default.
        if context.max_steps > 0 && u64::from(steps) >= context.max_steps {
            return Ok(settle(
                context,
                StopReason::StepLimit,
                text,
                usage,
                steps,
                calls,
                images,
            ));
        }

        steps = steps.saturating_add(1);
        context.emit(Event::TurnStarted { step: steps });

        let plan = context.plan()?;
        let response = match request(context, provider.as_ref(), &plan) {
            Ok(response) => response,
            // A cancellation during the request settles the turn rather than
            // failing it: the caller asked for it, so it is not an error.
            Err(err) if err.code() == ErrorCode::Cancelled => {
                return Ok(settle(
                    context,
                    StopReason::Cancelled,
                    text,
                    usage,
                    steps,
                    calls,
                    images,
                ));
            }
            Err(err) => return Err(err),
        };
        usage = usage.merge_max(response.usage);
        context.store_usage(usage);

        let mut parts: Vec<ContentPart> = Vec::new();
        let mut pending: Vec<PreparedCall> = Vec::new();
        for event in &response.events {
            match event {
                ProviderEvent::TextDelta { delta } => {
                    text.push_str(delta);
                    context.emit(Event::TextDelta {
                        delta: delta.clone(),
                    });
                    parts.push(ContentPart::Text {
                        text: delta.clone(),
                    });
                }
                ProviderEvent::ReasoningDelta { delta } => {
                    context.emit(Event::ReasoningDelta {
                        delta: delta.clone(),
                    });
                    parts.push(ContentPart::Reasoning {
                        text: delta.clone(),
                    });
                }
                ProviderEvent::ToolCallEnd { id, arguments } => {
                    let name = response
                        .events
                        .iter()
                        .find_map(|candidate| match candidate {
                            ProviderEvent::ToolCallStart { id: start, name } if start == id => {
                                Some(name.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    parts.push(ContentPart::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    pending.push(PreparedCall {
                        id: id.to_string(),
                        name,
                        arguments: arguments.clone(),
                    });
                }
                _ => {}
            }
        }

        if !parts.is_empty() || !pending.is_empty() {
            lock(&context.conversation.history)
                .push_assistant_with_replay(coalesce(parts), response.replay.clone());
        }

        let finish = response.finish.unwrap_or(FinishReason::Stop);
        // A cancellation that arrived while the response was being read ends
        // the turn before any tool runs, which is what a caller that closed the
        // agent while the model was thinking expects.
        if context.cancel.is_cancelled() {
            return Ok(settle(
                context,
                StopReason::Cancelled,
                text,
                usage,
                steps,
                calls,
                images,
            ));
        }
        if pending.is_empty() {
            return Ok(settle(
                context,
                stop_reason(finish),
                text,
                usage,
                steps,
                calls,
                images,
            ));
        }

        let (results, cancelled) = execute(context, &pending);
        let mut result_parts = Vec::new();
        for executed in &results {
            result_parts.push(ContentPart::ToolResult {
                id: rune_core::id::ToolCallId::new(executed.result.call.id.clone())?,
                name: executed.result.call.name.clone(),
                content: executed.result.output.text.clone(),
                is_error: executed.result.output.is_error,
            });
            for image in &executed.images {
                result_parts.push(ContentPart::Image {
                    image: context.register_image(image),
                });
            }
            images.extend(executed.images.iter().cloned());
        }
        lock(&context.conversation.history).push_tool_results(result_parts);
        calls.extend(results.into_iter().map(|executed| executed.result));

        if cancelled {
            return Ok(settle(
                context,
                StopReason::Cancelled,
                text,
                usage,
                steps,
                calls,
                images,
            ));
        }
        // A provider error with calls in flight still ends the turn, after their
        // results are recorded, so nothing the model asked for is lost.
        if finish == FinishReason::ProviderError {
            return Ok(settle(
                context,
                StopReason::ProviderFailure,
                text,
                usage,
                steps,
                calls,
                images,
            ));
        }
    }
}

impl TurnContext {
    /// Builds the request plan for the next step.
    fn plan(&self) -> Result<RequestPlan> {
        let history = lock(&self.conversation.history);
        history.validate()?;

        let mut plan = RequestPlan::new(self.model.clone());
        plan.instructions.clone_from(&self.instructions);
        plan.messages = history.to_messages();
        plan.tools.clone_from(self.specs.as_ref());
        plan.tool_choice = ToolChoice::Auto;
        // A budget that allows one call at a time is the only way an embedder
        // asks for them to be serialized, and the endpoint reads the flag.
        plan.parallel_tool_calls = self.limits.get_usize(LimitName::ParallelToolCalls) > 1;
        Ok(plan)
    }
}

/// Sends one request and reduces its response stream.
fn request(
    context: &TurnContext,
    provider: &dyn Provider,
    plan: &RequestPlan,
) -> Result<StreamOutcome> {
    context.cancel.check()?;
    provider.validate(plan)?;
    let body = provider.build_request(plan)?;

    let mut headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        ("accept".to_owned(), "text/event-stream".to_owned()),
        (
            context.endpoint.auth.header().to_owned(),
            context.endpoint.auth.value(&context.endpoint.credential),
        ),
    ];
    for (name, value) in provider.extra_headers() {
        headers.push((name.to_owned(), value));
    }

    let request = FetchRequest {
        url: context.endpoint.url_for(provider.request_path()),
        headers,
        body: serde_json::to_string(&body)?,
    };
    let response = context.fetch.post(request)?;

    if response.status >= 400 {
        let text = String::from_utf8_lossy(&response.body);
        let sanitized = redact::redact(&text);
        return Err(NetError::classify_status(response.status, &sanitized)
            .with_hint(format!(
                "provider `{}` rejected the request",
                provider.name()
            ))
            .to_rune_error());
    }

    reduce(&response.body, provider, &context.cancel)
}

/// Reduces a complete response body into normalized events.
fn reduce(body: &[u8], provider: &dyn Provider, cancel: &Cancellation) -> Result<StreamOutcome> {
    let mut decoder = Decoder::new(provider.limits());
    let mut reducer = provider.reducer();
    let mut frames: Vec<SseEvent> = Vec::new();
    decoder.push(body, &mut frames)?;
    decoder.finish(&mut frames)?;

    let mut outcome = StreamOutcome::default();
    for frame in &frames {
        if frame.is_empty() && frame.name.is_none() {
            continue;
        }
        reducer.apply(Some(&frame.data), &mut outcome.events)?;
    }

    cancel.check()?;
    // A reducer that never saw a terminal payload fails here, so a truncated
    // response can never be mistaken for a complete one.
    reducer.apply(None, &mut outcome.events)?;
    outcome.finish = Some(reducer.finish()?);
    outcome.usage = reducer.usage();
    outcome.replay = reducer.replay();
    Ok(outcome)
}

/// One executed call, with the images it produced.
struct Executed {
    result: CallResult,
    images: Vec<HostImage>,
}

/// Runs the calls the model asked for, in order.
///
/// Every call is answered, including one that never ran, because the history
/// must record a result for each call the model made or the next request would
/// carry a conversation the provider rejects.
fn execute(context: &TurnContext, calls: &[PreparedCall]) -> (Vec<Executed>, bool) {
    let mut results: Vec<Executed> = Vec::with_capacity(calls.len());
    let mut cancelled = false;

    for call in calls {
        if cancelled || context.cancel.is_cancelled() {
            cancelled = true;
            results.push(failed(
                call,
                "the turn was cancelled before this call ran".to_owned(),
            ));
            continue;
        }

        let Some(tool) = context.tools.iter().find(|tool| tool.name == call.name) else {
            results.push(failed(
                call,
                format!("there is no tool named `{}`", call.name),
            ));
            continue;
        };

        let arguments: serde_json::Value = match serde_json::from_str(&call.arguments) {
            Ok(value) => value,
            Err(err) => {
                // Malformed arguments are the model's to correct, not a
                // failure of the turn.
                results.push(failed(
                    call,
                    format!(
                        "the arguments for `{}` are not valid JSON: {err}",
                        call.name
                    ),
                ));
                continue;
            }
        };

        context.emit(Event::ToolStarted {
            call: call.clone(),
            activity: Activity::Execute,
        });
        let host_context = HostToolContext {
            signal: context.cancel.clone().into(),
        };

        match (tool.execute)(&arguments, &host_context) {
            Ok(result) => {
                context.emit(Event::ToolFinished {
                    call: call.clone(),
                    is_error: result.is_error,
                });
                let cap = context.limits.get_usize(LimitName::MaxToolResultBytes);
                let produced = u64::try_from(result.text.len()).unwrap_or(u64::MAX);
                results.push(Executed {
                    result: CallResult {
                        call: call.clone(),
                        output: ToolOutput {
                            text: bound_text(&result.text, cap),
                            is_error: result.is_error,
                            produced_bytes: produced,
                        },
                        executed: true,
                    },
                    images: result.images,
                });
            }
            Err(err) => {
                context.emit(Event::ToolFinished {
                    call: call.clone(),
                    is_error: true,
                });
                if err.code() == ErrorCode::Cancelled {
                    cancelled = true;
                }
                results.push(failed(call, err.message().to_owned()));
            }
        }
    }

    (results, cancelled)
}

/// Builds the answer for a call that did not run.
fn failed(call: &PreparedCall, message: String) -> Executed {
    Executed {
        result: CallResult {
            call: call.clone(),
            output: ToolOutput::failure(message),
            executed: false,
        },
        images: Vec::new(),
    }
}

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
    out.push_str("\n... (truncated)");
    out
}

/// Merges adjacent text parts, so a delta-per-token stream does not become one
/// history entry per token.
fn coalesce(parts: Vec<ContentPart>) -> Vec<ContentPart> {
    let mut out: Vec<ContentPart> = Vec::with_capacity(parts.len());
    for part in parts {
        match (out.last_mut(), &part) {
            (Some(ContentPart::Text { text }), ContentPart::Text { text: next }) => {
                text.push_str(next);
            }
            _ => out.push(part),
        }
    }
    out
}

/// Maps a normalized finish reason onto a stop reason.
fn stop_reason(finish: FinishReason) -> StopReason {
    match finish {
        FinishReason::Stop | FinishReason::ToolCalls => StopReason::Completed,
        FinishReason::MaxTokens => StopReason::OutputLimit,
        FinishReason::ContentFilter => StopReason::ContentFilter,
        FinishReason::MaxModelTurns => StopReason::StepLimit,
        FinishReason::Refused => StopReason::Refused,
        FinishReason::Cancelled => StopReason::Cancelled,
        FinishReason::ProviderError => StopReason::ProviderFailure,
    }
}

/// Records the end of a turn and hands its product back.
fn settle(
    context: &TurnContext,
    stop_reason: StopReason,
    text: String,
    usage: Usage,
    steps: u32,
    calls: Vec<CallResult>,
    images: Vec<HostImage>,
) -> TurnProduct {
    context.emit(Event::Finished {
        reason: stop_reason,
        usage,
        steps,
    });
    TurnProduct {
        outcome: TurnOutcome {
            stop_reason,
            text,
            usage,
            steps,
            calls,
        },
        images,
    }
}

/// Locks a mutex, ignoring poisoning.
///
/// A panic inside a turn must not make the conversation permanently unreadable:
/// the data behind the lock is still consistent, because every writer holds it
/// only while replacing a value.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
