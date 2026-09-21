//! The dispatch loop.
//!
//! One connection, one reader, one writer. Requests are answered on the reading
//! thread except for a prompt turn, which runs on its own thread so the
//! connection stays live while the model streams: that is what lets a second
//! prompt be accepted and queued instead of rejected.
//!
//! Two invariants hold the design together:
//!
//! - Stdout carries frames and nothing else. Every diagnostic goes to the
//!   configured log file, so a client's parser never sees a stray line.
//! - A prompt that arrives during a turn is admitted. It is either queued or, at
//!   the configured bound, refused with an error. It is never silently dropped.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use camino::Utf8PathBuf;
use rune_agent::history::History;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{Event, Host, StopReason, TurnOutcome, run_turn};
use rune_core::budget::{BudgetSet, LimitName};
use rune_core::config::{Effort, PermissionMode};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;
use rune_net::catalog::{Catalog, DEFAULT_CONTEXT_WINDOW};
use rune_net::message::ToolSpec;
use rune_net::provider::Provider;
use rune_net::stream::Usage;
use rune_net::transport::Endpoint;
use rune_policy::decision::Outcome;
use rune_policy::rules::RuleSet;
use rune_session::event::SessionEvent;
use rune_tools::Registry;
use rune_tools::contract::{ExecutionContext, ToolOutput};
use rune_tools::workspace::FileLimits;
use serde_json::{Map, Value, json};

use crate::jsonrpc::{FrameError, Id, Message, Notification, Request, Response, RpcError, Writer};
use crate::session::{
    SessionConfig, Sessions, config_options, message_chunk, modes, tool_call, tool_call_update,
    usage_update,
};

/// Protocol version this server speaks.
pub const PROTOCOL_VERSION: i64 = 1;

/// Interval between checks of the cancellation flag while waiting.
///
/// A client that cancels expects the turn to stop promptly, and a client that
/// never answers a permission request must not hang the turn forever, so both
/// waits are bounded by this poll.
const POLL: Duration = Duration::from_millis(25);

/// Time the server gives an in-flight turn to stop once input ends.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Which provider dialect the endpoint speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Dialect {
    /// An OpenAI-compatible Chat Completions endpoint.
    #[default]
    ChatCompletions,
    /// An OpenAI Responses endpoint.
    Responses,
    /// An Anthropic Messages endpoint.
    Anthropic,
}

impl Dialect {
    /// Returns the dialect as a provider.
    #[must_use]
    pub fn provider(self) -> &'static dyn Provider {
        static CHAT: rune_net::chat_completions::ChatCompletions =
            rune_net::chat_completions::ChatCompletions;
        static RESPONSES: rune_net::responses::Responses = rune_net::responses::Responses;
        static ANTHROPIC: rune_net::anthropic::Anthropic = rune_net::anthropic::Anthropic;
        match self {
            Self::ChatCompletions => &CHAT,
            Self::Responses => &RESPONSES,
            Self::Anthropic => &ANTHROPIC,
        }
    }

    /// Returns the wire name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::Anthropic => "anthropic",
        }
    }

    /// Parses the wire name.
    #[must_use]
    pub fn from_name(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "chat_completions" | "chat-completions" | "openai" => Some(Self::ChatCompletions),
            "responses" => Some(Self::Responses),
            "anthropic" | "messages" => Some(Self::Anthropic),
            _ => None,
        }
    }
}

/// Everything one server process needs.
#[derive(Debug)]
pub struct ServerConfig {
    /// State layout, used to open and create sessions.
    pub paths: Paths,
    /// Primary workspace every session is rooted at.
    pub workspace: Utf8PathBuf,
    /// Endpoint the model requests go to.
    pub endpoint: Endpoint,
    /// Dialect the endpoint speaks.
    pub dialect: Dialect,
    /// Model used unless a session overrides it.
    pub model: String,
    /// System instructions for every turn.
    pub instructions: String,
    /// Tools advertised to the model.
    pub registry: Registry,
    /// Rules in force before any session approval.
    pub rules: RuleSet,
    /// Permission mode used unless a session overrides it.
    pub mode: PermissionMode,
    /// Reasoning effort requested.
    pub effort: Effort,
    /// Limits in force.
    pub limits: BudgetSet,
    /// Where diagnostics are written. Stdout is never used for them.
    pub log_file: Option<Utf8PathBuf>,
    /// Usable context size of the active model, for usage updates.
    pub context_window: u64,
}

impl ServerConfig {
    /// Builds a configuration with the built-in tool set.
    pub fn new(
        paths: Paths,
        workspace: Utf8PathBuf,
        endpoint: Endpoint,
        model: impl Into<String>,
    ) -> Result<Self> {
        let limits = BudgetSet::new();
        let registry = rune_tools::inventory::builtin(&FileLimits::from_budget(&limits))?;
        let model = model.into();
        let context_window = Catalog::new("acp")
            .metadata_or_default(&model)
            .usable_context();
        let instructions = format!(
            "You are Rune, a coding agent. The workspace is {workspace}. Use the tools to inspect and change files."
        );
        Ok(Self {
            paths,
            workspace,
            endpoint,
            dialect: Dialect::default(),
            model,
            instructions,
            registry,
            rules: RuleSet::new(),
            mode: PermissionMode::default(),
            effort: Effort::default(),
            limits,
            log_file: None,
            context_window,
        })
    }

    /// Sets the dialect.
    #[must_use]
    pub const fn with_dialect(mut self, dialect: Dialect) -> Self {
        self.dialect = dialect;
        self
    }

    /// Sets the tool registry.
    #[must_use]
    pub fn with_registry(mut self, registry: Registry) -> Self {
        self.registry = registry;
        self
    }

    /// Sets the permission rules.
    #[must_use]
    pub fn with_rules(mut self, rules: RuleSet) -> Self {
        self.rules = rules;
        self
    }

    /// Sets the permission mode.
    #[must_use]
    pub const fn with_mode(mut self, mode: PermissionMode) -> Self {
        self.mode = mode;
        self
    }

    /// Sets the reasoning effort.
    #[must_use]
    pub const fn with_effort(mut self, effort: Effort) -> Self {
        self.effort = effort;
        self
    }

    /// Sets the system instructions.
    #[must_use]
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    /// Sets the limits and rebuilds the tool set from them.
    pub fn with_limits(mut self, limits: BudgetSet) -> Result<Self> {
        self.registry = rune_tools::inventory::builtin(&FileLimits::from_budget(&limits))?;
        self.limits = limits;
        self
    }

    /// Sends diagnostics to a file instead of standard error.
    #[must_use]
    pub fn with_log_file(mut self, path: Option<Utf8PathBuf>) -> Self {
        self.log_file = path;
        self
    }

    /// Sets the context size reported in usage updates.
    #[must_use]
    pub const fn with_context_window(mut self, tokens: u64) -> Self {
        self.context_window = tokens;
        self
    }
}

/// A prompt waiting for the active turn to settle.
#[derive(Debug)]
struct Queued {
    id: Id,
    session: String,
    text: String,
}

/// The one turn a connection may be running.
#[derive(Debug, Default)]
struct Active {
    /// Cancellation for the running turn, absent when the connection is idle.
    cancellation: Option<Cancellation>,
    /// Session the running turn belongs to.
    session: Option<String>,
    /// Prompts admitted while the turn runs, in arrival order.
    queue: VecDeque<Queued>,
}

/// State shared between the reading thread and any turn thread.
struct Core<W: Write + Send + 'static> {
    config: ServerConfig,
    writer: Mutex<Writer<W>>,
    sessions: Mutex<Sessions>,
    pending: Mutex<BTreeMap<i64, std::sync::mpsc::Sender<Response>>>,
    next_request: AtomicI64,
    log: Mutex<Option<std::fs::File>>,
    active: Mutex<Active>,
    settled: Condvar,
    /// Cap on prompts admitted while a turn runs.
    queue_depth: usize,
}

impl<W: Write + Send + 'static> fmt::Debug for Core<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Core")
            .field("model", &self.config.model)
            .field("queue_depth", &self.queue_depth)
            .finish_non_exhaustive()
    }
}

impl<W: Write + Send + 'static> Core<W> {
    /// Writes one frame.
    fn send(&self, message: Message) {
        let outcome = match self.writer.lock() {
            Ok(mut writer) => writer.write_message(&message),
            Err(_) => return,
        };
        if let Err(err) = outcome {
            self.note(format_args!("could not write a frame: {err}"));
        }
    }

    /// Writes a diagnostic line.
    ///
    /// Never to stdout: a line there would be a frame as far as the client is
    /// concerned, and a malformed frame ends the connection.
    fn note(&self, message: fmt::Arguments<'_>) {
        let Ok(mut guard) = self.log.lock() else {
            return;
        };
        match guard.as_mut() {
            Some(file) => {
                let _ = writeln!(file, "{message}");
            }
            None => {
                let _ = writeln!(std::io::stderr(), "{message}");
            }
        }
    }

    /// Answers a request.
    fn respond(&self, id: Id, result: Value) {
        self.send(Message::Response(Response::ok(id, result)));
    }

    /// Reports a failure for a request.
    fn fail(&self, id: Option<Id>, error: RpcError) {
        self.send(Message::Response(Response::failed(id, error)));
    }

    /// Routes a response to whoever is waiting for it.
    fn resolve(&self, response: Response) {
        let Some(Id::Number(id)) = response.id.clone() else {
            self.note(format_args!(
                "ignoring a response for an identifier this server did not issue"
            ));
            return;
        };
        let sender = self
            .pending
            .lock()
            .ok()
            .and_then(|mut guard| guard.remove(&id));
        match sender {
            Some(sender) => {
                // A receiver that has gone away is a cancelled turn, which is
                // not an error worth reporting.
                let _ = sender.send(response);
            }
            None => self.note(format_args!("ignoring a response for unknown request {id}")),
        }
    }

    /// Issues a server-to-client request and waits for its answer.
    fn ask(&self, method: &str, params: Value, cancellation: &Cancellation) -> Option<Value> {
        let id = self.next_request.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = std::sync::mpsc::channel();
        if let Ok(mut guard) = self.pending.lock() {
            guard.insert(id, sender);
        }
        self.send(Message::Request(Request::new(
            Id::Number(id),
            method,
            params,
        )));
        let answer = wait_for(&receiver, cancellation);
        if let Ok(mut guard) = self.pending.lock() {
            guard.remove(&id);
        }
        answer
    }

    /// Dispatches one incoming message.
    fn dispatch(&self, message: Message) {
        match message {
            Message::Response(response) => self.resolve(response),
            Message::Notification(notification) => {
                if let Err(error) = self.handle(&notification.method, &notification.params, None) {
                    self.note(format_args!(
                        "notification `{}` failed: {}",
                        notification.method, error.message
                    ));
                }
            }
            Message::Request(request) => self.call(request),
        }
    }

    /// Handles a request, answering it once.
    fn call(&self, request: Request) {
        let id = request.id;
        match self.handle(&request.method, &request.params, Some(id.clone())) {
            Ok(Some(result)) => self.respond(id, result),
            // A queued prompt is answered when it runs.
            Ok(None) => {}
            Err(error) => self.fail(Some(id), error),
        }
    }

    /// Routes a method to its handler.
    ///
    /// Returns `None` when the method took ownership of the response, which only
    /// a queued prompt does.
    fn handle(
        &self,
        method: &str,
        params: &Value,
        id: Option<Id>,
    ) -> std::result::Result<Option<Value>, RpcError> {
        match method {
            "initialize" => Ok(Some(self.initialize(params))),
            "session/new" => self.new_session(params).map(Some),
            "session/load" => self.load_session(params).map(Some),
            "session/resume" => self.resume_session(params).map(Some),
            "session/close" => self.close_session(params).map(Some),
            "session/list" => Ok(Some(self.list_sessions())),
            "session/set_config_option" => self.set_config_option(params).map(Some),
            "session/set_mode" => self.set_mode(params).map(Some),
            "session/cancel" => self.cancel(params).map(Some),
            "session/prompt" => self.prompt(params, id),
            other => Err(RpcError::method_not_found(other)),
        }
    }

    /// Answers `initialize`, advertising what this server supports.
    fn initialize(&self, params: &Value) -> Value {
        let client = params
            .get("clientInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        self.note(format_args!("client `{client}` initialized the connection"));
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {
                    "image": false,
                    "audio": false,
                    "embeddedContext": true,
                },
                "mcpCapabilities": { "http": false, "sse": false },
                "sessionCapabilities": {
                    "close": {},
                    "list": {},
                    "resume": {},
                },
            },
            "agentInfo": {
                "name": "rune",
                "version": env!("CARGO_PKG_VERSION"),
                "title": "Rune",
            },
            "authMethods": [],
        })
    }

    /// Answers `session/new`.
    fn new_session(&self, params: &Value) -> std::result::Result<Value, RpcError> {
        let roots = additional_roots(params)?;
        let cwd = optional_string(params, "cwd")?;
        if let Some(cwd) = &cwd {
            require_absolute(&Utf8PathBuf::from(cwd), "cwd")?;
        }
        if let Some(list) = params.get("mcpServers").and_then(Value::as_array)
            && !list.is_empty()
        {
            // No MCP client is wired into this server, so the servers a client
            // asks for are reported rather than silently implied to be connected.
            self.note(format_args!(
                "ignoring {} requested MCP server(s)",
                list.len()
            ));
        }
        let mut sessions = self.lock_sessions()?;
        let id = sessions
            .create(roots)
            .map_err(|err| RpcError::from_rune(&err))?;
        let session = sessions.get(&id).map_err(|err| RpcError::from_rune(&err))?;
        Ok(json!({
            "sessionId": id,
            "modes": modes(session.config()),
            "configOptions": config_options(session.config()),
        }))
    }

    /// Answers `session/load`, replaying the stored conversation first.
    fn load_session(&self, params: &Value) -> std::result::Result<Value, RpcError> {
        let id = string_field(params, "sessionId")?;
        let updates = {
            let mut sessions = self.lock_sessions()?;
            sessions
                .load(&id)
                .map_err(|err| RpcError::from_rune(&err))?
        };
        for update in updates {
            self.send(Message::Notification(update));
        }
        let sessions = self.lock_sessions()?;
        let session = sessions.get(&id).map_err(|err| RpcError::from_rune(&err))?;
        Ok(json!({
            "modes": modes(session.config()),
            "configOptions": config_options(session.config()),
        }))
    }

    /// Answers `session/resume`, which does not replay anything.
    fn resume_session(&self, params: &Value) -> std::result::Result<Value, RpcError> {
        let id = string_field(params, "sessionId")?;
        let mut sessions = self.lock_sessions()?;
        sessions
            .resume(&id)
            .map_err(|err| RpcError::from_rune(&err))?;
        let session = sessions.get(&id).map_err(|err| RpcError::from_rune(&err))?;
        Ok(json!({
            "modes": modes(session.config()),
            "configOptions": config_options(session.config()),
        }))
    }

    /// Answers `session/close`.
    fn close_session(&self, params: &Value) -> std::result::Result<Value, RpcError> {
        let id = string_field(params, "sessionId")?;
        self.cancel_session(&id);
        let mut sessions = self.lock_sessions()?;
        sessions
            .close(&id)
            .map_err(|err| RpcError::from_rune(&err))?;
        Ok(json!({}))
    }

    /// Answers `session/list`.
    fn list_sessions(&self) -> Value {
        let Ok(sessions) = self.lock_sessions() else {
            return json!({ "sessions": [] });
        };
        let listed: Vec<Value> = sessions
            .list()
            .into_iter()
            .map(|summary| {
                json!({
                    "sessionId": summary.id,
                    "cwd": summary.cwd,
                    "title": summary.title,
                    "updatedAt": summary.updated_at,
                })
            })
            .collect();
        json!({ "sessions": listed })
    }

    /// Answers `session/set_config_option`, returning the resulting options.
    fn set_config_option(&self, params: &Value) -> std::result::Result<Value, RpcError> {
        let id = string_field(params, "sessionId")?;
        let config = string_field(params, "configId")?;
        let value = string_field(params, "value")?;
        let mut sessions = self.lock_sessions()?;
        sessions
            .set_config_option(&id, &config, &value)
            .map_err(|err| RpcError::from_rune(&err))?;
        let session = sessions.get(&id).map_err(|err| RpcError::from_rune(&err))?;
        Ok(json!({ "configOptions": config_options(session.config()) }))
    }

    /// Answers `session/set_mode`.
    fn set_mode(&self, params: &Value) -> std::result::Result<Value, RpcError> {
        let id = string_field(params, "sessionId")?;
        let mode = string_field(params, "modeId")?;
        let mut sessions = self.lock_sessions()?;
        sessions
            .set_mode(&id, &mode)
            .map_err(|err| RpcError::from_rune(&err))?;
        Ok(json!({}))
    }

    /// Stops the active turn.
    fn cancel(&self, params: &Value) -> std::result::Result<Value, RpcError> {
        let id = string_field(params, "sessionId")?;
        self.cancel_session(&id);
        Ok(json!({}))
    }

    /// Requests cancellation for the active turn of a session.
    fn cancel_session(&self, id: &str) {
        let Ok(active) = self.active.lock() else {
            return;
        };
        match (&active.session, &active.cancellation) {
            (Some(active_session), Some(cancellation)) if active_session == id => {
                cancellation.cancel();
            }
            (Some(active_session), Some(_)) => {
                drop(active);
                self.note(format_args!(
                    "a cancel for `{id}` was ignored; the active turn belongs to `{active_session}`"
                ));
            }
            _ => {}
        }
    }

    /// Admits a prompt.
    ///
    /// The turn runs on its own thread so the reading thread stays free. A prompt
    /// that arrives while a turn is running is queued and answered once that turn
    /// settles, which is what keeps a client from losing a message it typed while
    /// the agent was working.
    fn prompt(
        &self,
        params: &Value,
        id: Option<Id>,
    ) -> std::result::Result<Option<Value>, RpcError> {
        let Some(id) = id else {
            return Err(RpcError::invalid_request(
                "`session/prompt` needs an identifier so its result can be reported",
            ));
        };
        let session = string_field(params, "sessionId")?;
        let text = prompt_text(params)?;
        {
            let sessions = self.lock_sessions()?;
            sessions
                .get(&session)
                .map_err(|err| RpcError::from_rune(&err))?;
        }

        let queued = Queued { id, session, text };
        let start = {
            let Ok(mut active) = self.active.lock() else {
                return Err(RpcError::internal("the session state is unavailable"));
            };
            if active.cancellation.is_some() {
                if active.queue.len() >= self.queue_depth {
                    let error = RuneError::new(
                        ErrorCode::LimitExceeded,
                        format!(
                            "{} prompts are already queued for the running turn, the limit is {}",
                            active.queue.len(),
                            self.queue_depth
                        ),
                    )
                    .with_hint("wait for the running turn to settle");
                    self.note(format_args!("refused a queued prompt: {}", error.message()));
                    return Err(RpcError::from_rune(&error));
                }
                self.note(format_args!(
                    "queued prompt for `{}` behind the running turn",
                    queued.session
                ));
                active.queue.push_back(queued);
                false
            } else {
                active.cancellation = Some(Cancellation::new());
                active.session = Some(queued.session.clone());
                true
            }
        };
        if start {
            spawn_turn(self.clone_handle(), queued);
        }
        Ok(None)
    }

    /// Returns a handle that keeps the shared state alive for a turn thread.
    fn clone_handle(self: &Self) -> Arc<Self> {
        // The turn thread needs an owned handle, so the server keeps one and
        // hands out clones of it.
        self.handle
            .lock()
            .map(|guard| Arc::clone(&guard))
            .unwrap_or_else(|_| Arc::new(std::sync::Mutex::new(())))
    }

    /// Locks the session map, reporting a poisoned lock as an internal fault.
    fn lock_sessions(&self) -> std::result::Result<std::sync::MutexGuard<'_, Sessions>, RpcError> {
        self.sessions
            .lock()
            .map_err(|_| RpcError::internal("the session state is unavailable"))
    }
}

/// Waits for a response, giving up when the turn is cancelled.
fn wait_for(receiver: &Receiver<Response>, cancellation: &Cancellation) -> Option<Value> {
    loop {
        match receiver.recv_timeout(POLL) {
            Ok(response) => return response.into_outcome().and_then(std::result::Result::ok),
            Err(RecvTimeoutError::Timeout) => {
                if cancellation.is_cancelled() {
                    return None;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// Starts a turn thread, or continues on the current one when none exists.
fn spawn_turn<W: Write + Send + 'static>(core: Arc<Core<W>>, queued: Queued) {
    std::thread::spawn(move || run_queued(core, queued));
}

/// Runs one turn, then every prompt that was queued behind it.
fn run_queued<W: Write + Send + 'static>(core: Arc<Core<W>>, first: Queued) {
    let mut next = Some(first);
    while let Some(queued) = next.take() {
        run_turn_once(&core, queued);
        next = {
            let Ok(mut active) = core.active.lock() else {
                return;
            };
            match active.queue.pop_front() {
                Some(following) => {
                    active.cancellation = Some(Cancellation::new());
                    active.session = Some(following.session.clone());
                    Some(following)
                }
                None => {
                    active.cancellation = None;
                    active.session = None;
                    core.settled.notify_all();
                    None
                }
            }
        };
    }
}

/// Runs one prompt as a turn and reports its outcome.
fn run_turn_once<W: Write + Send + 'static>(core: &Arc<Core<W>>, queued: Queued) {
    let session = queued.session.clone();
    let snapshot = match core
        .lock_sessions()
        .and_then(|sessions| sessions.snapshot(&session).map_err(|_| ()))
    {
        Ok(snapshot) => snapshot,
        Err(()) => {
            let error = core.lock_sessions().err().map_or_else(
                || RuneError::new(ErrorCode::Internal, "the session state is unavailable"),
                |err| err.to_rune_error(),
            );
            let error = if core
                .lock_sessions()
                .is_ok_and(|sessions| sessions.contains(&session))
            {
                error
            } else {
                crate::session::unknown_session(&session)
            };
            core.fail(Some(queued.id), RpcError::from_rune(&error));
            return;
        }
    };

    let cancellation = core
        .active
        .lock()
        .ok()
        .and_then(|active| active.cancellation.clone())
        .unwrap_or_default();

    if let Ok(mut sessions) = core.sessions.lock() {
        if let Err(err) = sessions.record(
            &session,
            SessionEvent::UserMessage {
                text: queued.text.clone(),
            },
        ) {
            core.note(format_args!("could not record a user message: {err}"));
        }
    }
    core.send(Message::Notification(message_chunk(
        &session,
        "user_message_chunk",
        &queued.text,
    )));

    let host = TurnHost {
        core: Arc::clone(core),
        session: session.clone(),
        config: snapshot.config.clone(),
        rules: Mutex::new(snapshot.rules.clone()),
        context: snapshot.context,
        cancellation: cancellation.clone(),
        steering: snapshot.steering,
        tools: core.config.registry.all_schemas(),
    };

    let mut history = snapshot.history;
    history.push_user(queued.text.clone());
    let outcome = run_turn(&mut history, &host);
    let rules = host
        .rules
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();

    match outcome {
        Ok(outcome) => {
            if !outcome.text.is_empty()
                && let Ok(mut sessions) = core.sessions.lock()
            {
                let _ = sessions.record(
                    &session,
                    SessionEvent::AssistantMessage {
                        turn: u64::from(outcome.steps),
                        text: outcome.text.clone(),
                    },
                );
            }
            if let Ok(mut sessions) = core.sessions.lock() {
                let _ = sessions.absorb(&session, history, rules);
            }
            if outcome.stop_reason == StopReason::ProviderFailure {
                let error = RuneError::new(
                    ErrorCode::TransportFailure,
                    "the provider failed before the turn finished",
                );
                core.fail(Some(queued.id), RpcError::from_rune(&error));
                return;
            }
            core.respond(
                queued.id,
                json!({ "stopReason": stop_reason(outcome.stop_reason) }),
            );
        }
        Err(err) if cancellation.is_cancelled() || err.code() == ErrorCode::Cancelled => {
            if let Ok(mut sessions) = core.sessions.lock() {
                let _ = sessions.absorb(&session, history, rules);
            }
            core.respond(queued.id, json!({ "stopReason": "cancelled" }));
        }
        Err(err) => {
            if let Ok(mut sessions) = core.sessions.lock() {
                let _ = sessions.absorb(&session, history, rules);
            }
            core.note(format_args!("the turn failed: {err}"));
            core.fail(Some(queued.id), RpcError::from_rune(&err));
        }
    }
}

/// Returns the protocol stop reason for a turn outcome.
#[must_use]
pub const fn stop_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Completed => "end_turn",
        StopReason::StepLimit => "max_turn_requests",
        StopReason::OutputLimit => "max_tokens",
        StopReason::Refused | StopReason::ContentFilter => "refusal",
        StopReason::Cancelled => "cancelled",
        StopReason::ProviderFailure => "end_turn",
    }
}

/// The host a turn runs against.
struct TurnHost<W: Write + Send + 'static> {
    core: Arc<Core<W>>,
    session: String,
    config: SessionConfig,
    rules: Mutex<RuleSet>,
    context: ExecutionContext,
    cancellation: Cancellation,
    steering: SteeringQueue,
    tools: Vec<ToolSpec>,
}

impl<W: Write + Send + 'static> TurnHost<W> {
    /// Reports one tool call to the client.
    fn announce(&self, call: &rune_agent::turn::PreparedCall) {
        let arguments: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
        let target = rune_agent::turn::permission_target_for(
            &self.core.config.registry,
            &call.name,
            &arguments,
        );
        self.core.send(Message::Notification(tool_call(
            &self.session,
            &call.id,
            &call.name,
            target.as_deref(),
            "in_progress",
            &arguments,
        )));
    }

    /// Asks the client to approve a call.
    fn request_permission(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        let request_id = self.core.next_request.fetch_add(1, Ordering::SeqCst);
        let prompt = format!("allow {} {}", name, target.unwrap_or("with no target"));
        let params = json!({
            "sessionId": self.session,
            "toolCall": {
                "toolCallId": format!("permission-{request_id}"),
                "title": crate::session::tool_title(name, target),
                "name": name,
                "kind": crate::session::kind_for_name(name),
                "status": "pending",
            },
            "options": [
                {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "allow_always", "name": "Allow for this session", "kind": "allow_always"},
                {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
            ],
        });
        let answer = self
            .core
            .ask("session/request_permission", params, &self.cancellation);
        let Some(answer) = answer else {
            return (
                Outcome::Deny,
                format!("the {} approval was not answered", prompt),
            );
        };
        let option = answer
            .get("outcome")
            .and_then(|outcome| outcome.get("optionId"))
            .and_then(Value::as_str)
            .unwrap_or("reject_once");
        match option {
            "allow_once" => (Outcome::Allow, format!("the user allowed {prompt} once")),
            "allow_always" => {
                if let Ok(mut rules) = self.rules.lock() {
                    crate::session::grant(&mut rules, name, target);
                }
                (
                    Outcome::Allow,
                    format!("the user allowed {prompt} for this session"),
                )
            }
            _ => (Outcome::Deny, format!("the user rejected {prompt}")),
        }
    }
}

impl<W: Write + Send + 'static> Host for TurnHost<W> {
    fn dialect(&self) -> &dyn Provider {
        self.core.config.dialect.provider()
    }

    fn endpoint(&self) -> &Endpoint {
        &self.core.config.endpoint
    }

    fn model(&self) -> &str {
        &self.config.model
    }

    fn instructions(&self) -> String {
        self.core.config.instructions.clone()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }

    fn effort(&self) -> Effort {
        self.config.effort
    }

    fn emit(&self, event: Event) {
        match event {
            Event::TextDelta { delta } => {
                self.core.send(Message::Notification(message_chunk(
                    &self.session,
                    "agent_message_chunk",
                    &delta,
                )));
            }
            Event::ReasoningDelta { delta } => {
                self.core.send(Message::Notification(message_chunk(
                    &self.session,
                    "agent_thought_chunk",
                    &delta,
                )));
            }
            Event::ToolStarted { call, .. } => self.announce(&call),
            Event::ToolFinished { call, is_error } => {
                let status = if is_error { "failed" } else { "completed" };
                self.core.send(Message::Notification(tool_call_update(
                    &self.session,
                    &call.id,
                    status,
                    None,
                )));
            }
            Event::ToolDenied { call, reason } => {
                self.core.send(Message::Notification(tool_call_update(
                    &self.session,
                    &call.id,
                    "failed",
                    Some(&reason),
                )));
            }
            Event::Finished { usage, .. } => {
                self.core.send(Message::Notification(usage_update(
                    &self.session,
                    reported_tokens(usage),
                    self.core.config.context_window,
                )));
            }
            Event::TurnStarted { .. } | Event::SteeringApplied { .. } => {}
        }
    }

    fn execute(&self, name: &str, arguments: &Value) -> Result<ToolOutput> {
        self.core
            .config
            .registry
            .call(name, arguments, &self.context)
    }

    fn decide(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        let rules = self
            .rules
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let (outcome, reason) =
            rune_agent::turn::decide_call(&rules, self.config.mode, name, target);
        match outcome {
            // An unresolved call is the client's to answer. This server has no
            // reviewer of its own, so the client is the reviewer.
            Outcome::Ask => self.request_permission(name, target),
            resolved => (resolved, reason),
        }
    }

    fn context(&self) -> ExecutionContext {
        self.context.clone()
    }

    fn limits(&self) -> BudgetSet {
        self.core.config.limits.clone()
    }

    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    fn steering(&self) -> &SteeringQueue {
        &self.steering
    }
}

/// Returns the token count reported in a usage update.
fn reported_tokens(usage: Usage) -> u64 {
    usage
        .input_tokens
        .unwrap_or(0)
        .saturating_add(usage.output_tokens.unwrap_or(0))
}

/// A server serving one connection.
pub struct Server<W: Write + Send + 'static> {
    core: Arc<Core<W>>,
    /// Held so turn threads keep the shared state alive.
    handle: Arc<Mutex<()>>,
}

impl<W: Write + Send + 'static> fmt::Debug for Server<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("model", &self.core.config.model)
            .finish_non_exhaustive()
    }
}

impl<W: Write + Send + 'static> Server<W> {
    /// Builds a server that writes frames to `output`.
    pub fn new(config: ServerConfig, output: W) -> Result<Self> {
        let log = match &config.log_file {
            Some(path) => Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(PathBuf::from(path.as_str()))?,
            ),
            None => None,
        };
        let queue_depth = config
            .limits
            .get_usize(LimitName::SteeringQueueDepth)
            .max(1);
        let defaults = SessionConfig::new(config.model.clone(), config.effort, config.mode);
        let sessions = Sessions::new(
            config.paths.clone(),
            config.workspace.clone(),
            defaults,
            &config.limits,
        );
        Ok(Self {
            core: Arc::new(Core {
                config,
                writer: Mutex::new(Writer::new(output)),
                sessions: Mutex::new(sessions),
                pending: Mutex::new(BTreeMap::new()),
                next_request: AtomicI64::new(1),
                log: Mutex::new(log),
                active: Mutex::new(Active::default()),
                settled: Condvar::new(),
                queue_depth,
            }),
            handle: Arc::new(Mutex::new(())),
        })
    }

    /// Serves messages until the input ends.
    ///
    /// A malformed frame is answered and the connection continues: only a failure
    /// of the stream itself ends the loop.
    pub fn run<R: BufRead>(&self, input: R) -> Result<()> {
        let mut reader = crate::jsonrpc::Reader::new(input);
        loop {
            match reader.read_message() {
                Ok(None) => break,
                Ok(Some(message)) => self.core.dispatch(message),
                Err(err) if err.is_fatal() => {
                    self.core.shutdown();
                    return Err(err.to_rune_error());
                }
                Err(err) => self.reject(&err),
            }
        }
        self.core.shutdown();
        Ok(())
    }

    /// Reports a frame the reader could not accept.
    fn reject(&self, err: &FrameError) {
        self.core.note(format_args!("rejected a frame: {err}"));
        let mut error = err.to_rpc_error();
        if let FrameError::TooLarge { observed, limit } = err {
            error.data = Some(json!({
                "code": ErrorCode::TooLarge.as_str(),
                "field": "frame",
                "observed": observed,
                "limit": limit,
            }));
        }
        self.core.fail(None, error);
    }
}

impl<W: Write + Send + 'static> Core<W> {
    /// Stops any running turn and waits briefly for it to report.
    fn shutdown(&self) {
        if let Ok(mut active) = self.active.lock() {
            if let Some(cancellation) = &active.cancellation {
                cancellation.cancel();
            }
            let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
            while active.cancellation.is_some() && std::time::Instant::now() < deadline {
                match self.settled.wait_timeout(active, POLL) {
                    Ok((guard, _)) => active = guard,
                    Err(_) => return,
                }
            }
        }
    }
}

/// Reads a required string member.
fn string_field(params: &Value, name: &str) -> std::result::Result<String, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Err(RpcError::from_rune(&RuneError::missing_field(name))),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(_) => Err(RpcError::from_rune(&RuneError::invalid_field(
            name,
            format!("`{name}` must be a string"),
        ))),
    }
}

/// Reads an optional string member.
fn optional_string(params: &Value, name: &str) -> std::result::Result<Option<String>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(RpcError::from_rune(&RuneError::invalid_field(
            name,
            format!("`{name}` must be a string"),
        ))),
    }
}

/// Reads the additional workspace roots a request asks for.
fn additional_roots(params: &Value) -> std::result::Result<Vec<Utf8PathBuf>, RpcError> {
    let Some(value) = params.get("additionalDirectories") else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let Some(list) = value.as_array() else {
        return Err(RpcError::from_rune(&RuneError::invalid_field(
            "additionalDirectories",
            "must be an array of absolute paths",
        )));
    };
    let mut roots = Vec::with_capacity(list.len());
    for entry in list {
        let Some(text) = entry.as_str() else {
            return Err(RpcError::from_rune(&RuneError::invalid_field(
                "additionalDirectories",
                "every entry must be a string",
            )));
        };
        let path = Utf8PathBuf::from(text);
        require_absolute(&path, "additionalDirectories")?;
        roots.push(path);
    }
    Ok(roots)
}

/// Rejects a relative path in a field the protocol requires to be absolute.
fn require_absolute(path: &Utf8PathBuf, field: &str) -> std::result::Result<(), RpcError> {
    if path.is_absolute() {
        return Ok(());
    }
    Err(RpcError::from_rune(&RuneError::invalid_field(
        field,
        format!("`{path}` is not an absolute path"),
    )))
}

/// Extracts the text of a prompt.
///
/// Text blocks and embedded resources carry text; every other block type is
/// reported and skipped, because this server advertises no image support.
fn prompt_text(params: &Value) -> std::result::Result<String, RpcError> {
    let Some(blocks) = params.get("prompt") else {
        return Err(RpcError::from_rune(&RuneError::missing_field("prompt")));
    };
    let Some(blocks) = blocks.as_array() else {
        return Err(RpcError::from_rune(&RuneError::invalid_field(
            "prompt",
            "must be an array of content blocks",
        )));
    };
    let mut text = String::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(part) = block.get("text").and_then(Value::as_str) {
                    push_block(&mut text, part);
                }
            }
            Some("resource") => {
                if let Some(part) = block
                    .get("resource")
                    .and_then(|resource| resource.get("text"))
                    .and_then(Value::as_str)
                {
                    push_block(&mut text, part);
                }
            }
            Some(other) => {
                return Err(RpcError::invalid_params(format!(
                    "this server does not accept `{other}` content in a prompt"
                )));
            }
            None => {
                return Err(RpcError::invalid_params(
                    "a prompt block must declare its type",
                ));
            }
        }
    }
    if text.trim().is_empty() {
        return Err(RpcError::from_rune(&RuneError::invalid_field(
            "prompt",
            "must contain some text",
        )));
    }
    Ok(text)
}

/// Appends one block's text, separating blocks with a blank line.
fn push_block(target: &mut String, text: &str) {
    if !target.is_empty() {
        target.push_str("\n\n");
    }
    target.push_str(text);
}

/// Convenience for callers building a prompt by hand.
#[must_use]
pub fn text_prompt(text: &str) -> Value {
    json!([{ "type": "text", "text": text }])
}

/// Names the context window used when the catalog knows nothing about a model.
#[must_use]
pub const fn fallback_context_window() -> u64 {
    DEFAULT_CONTEXT_WINDOW
}

/// A map from tool name to the tools a turn advertises, for diagnostics.
#[must_use]
pub fn tool_names(registry: &Registry) -> Vec<String> {
    registry.names().into_iter().map(str::to_owned).collect()
}

/// Collects the identifiers a turn announced, for diagnostics.
#[must_use]
pub fn announced_ids(ids: &HashSet<String>) -> Vec<&str> {
    let mut sorted: Vec<&str> = ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted
}

/// Renders a request map for a diagnostic line.
#[must_use]
pub fn describe_params(params: &Value) -> String {
    match params.as_object() {
        Some(object) => {
            let keys: Vec<&str> = object.keys().map(String::as_str).collect();
            keys.join(", ")
        }
        None => "not an object".to_owned(),
    }
}

/// Returns a copy of a params object with one member replaced.
#[must_use]
pub fn with_member(params: &Value, name: &str, value: Value) -> Value {
    let mut object: Map<String, Value> = params.as_object().cloned().unwrap_or_default();
    object.insert(name.to_owned(), value);
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reasons_map_to_their_wire_names() {
        assert_eq!(stop_reason(StopReason::Completed), "end_turn");
        assert_eq!(stop_reason(StopReason::Cancelled), "cancelled");
        assert_eq!(stop_reason(StopReason::OutputLimit), "max_tokens");
        assert_eq!(stop_reason(StopReason::StepLimit), "max_turn_requests");
        assert_eq!(stop_reason(StopReason::Refused), "refusal");
    }

    #[test]
    fn a_dialect_round_trips_through_its_name() {
        for dialect in [
            Dialect::ChatCompletions,
            Dialect::Responses,
            Dialect::Anthropic,
        ] {
            assert_eq!(Dialect::from_name(dialect.name()), Some(dialect));
            assert!(!dialect.provider().name().is_empty());
        }
        assert_eq!(Dialect::from_name("nope"), None);
    }

    #[test]
    fn a_prompt_without_text_is_rejected() {
        let error = prompt_text(&json!({"prompt": [{"type": "text", "text": "   "}]}))
            .expect_err("rejected");
        assert_eq!(error.code, crate::jsonrpc::INVALID_PARAMS);
    }

    #[test]
    fn a_missing_prompt_is_rejected() {
        let error = prompt_text(&json!({})).expect_err("rejected");
        assert_eq!(error.code, crate::jsonrpc::INVALID_PARAMS);
    }

    #[test]
    fn prompt_blocks_are_joined_with_a_blank_line() {
        let text = prompt_text(&json!({
            "prompt": [
                {"type": "text", "text": "one"},
                {"type": "resource", "resource": {"uri": "file:///a", "text": "two"}},
            ]
        }))
        .expect("text");
        assert_eq!(text, "one\n\ntwo");
    }

    #[test]
    fn an_unsupported_block_type_is_reported() {
        let error = prompt_text(&json!({"prompt": [{"type": "image", "data": "x"}]}))
            .expect_err("rejected");
        assert_eq!(error.code, crate::jsonrpc::INVALID_PARAMS);
    }

    #[test]
    fn a_relative_working_directory_is_rejected() {
        let error = require_absolute(&Utf8PathBuf::from("relative"), "cwd").expect_err("rejected");
        assert_eq!(error.code, crate::jsonrpc::INVALID_PARAMS);
    }

    #[test]
    fn additional_roots_must_be_absolute() {
        let error =
            additional_roots(&json!({"additionalDirectories": ["nope"]})).expect_err("rejected");
        assert_eq!(error.code, crate::jsonrpc::INVALID_PARAMS);
        let roots = additional_roots(&json!({"additionalDirectories": ["/tmp/a"]})).expect("roots");
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn a_missing_string_member_is_a_missing_field() {
        let error = string_field(&json!({}), "sessionId").expect_err("rejected");
        assert_eq!(error.data.expect("data")["code"], "missing_field");
    }

    #[test]
    fn a_usage_update_counts_both_directions() {
        let usage = Usage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..Usage::default()
        };
        assert_eq!(reported_tokens(usage), 15);
        assert_eq!(reported_tokens(Usage::default()), 0);
    }

    #[test]
    fn a_text_prompt_builds_the_documented_block() {
        assert_eq!(text_prompt("hi")[0]["type"], "text");
        assert_eq!(text_prompt("hi")[0]["text"], "hi");
    }
}
