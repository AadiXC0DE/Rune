//! The dispatch loop.
//!
//! One connection, one reader, one writer. Requests are answered on the reading
//! thread except for a prompt turn, which runs on its own thread so the
//! connection stays live while the model streams. That is what lets a second
//! prompt be admitted and queued instead of refused.
//!
//! Two invariants hold the design together:
//!
//! - Standard output carries frames and nothing else. Every diagnostic goes to
//!   the configured log file, because a stray line on stdout is a malformed
//!   frame as far as the client is concerned.
//! - A prompt that arrives during a turn is admitted. It is queued, or refused
//!   with a named reason once the queue is full. It is never dropped.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use rune_agent::history::History;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{Event, Host, StopReason, run_turn};
use rune_core::budget::{BudgetSet, LimitName};
use rune_core::config::{Effort, PermissionMode};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::Paths;
use rune_net::catalog::Catalog;
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
use serde_json::{Value, json};

use crate::jsonrpc::{FrameError, Id, Message, Request, Response, RpcError, Writer};
use crate::session::{
    SessionConfig, Sessions, config_options, kind_for_name, message_chunk, modes, tool_call,
    tool_call_update, tool_title, usage_update,
};

/// Protocol version this server speaks.
pub const PROTOCOL_VERSION: i64 = 1;

/// How long a blocking wait sleeps before rechecking cancellation.
///
/// A cancel has to take effect within an interactive pause, and a client that
/// never answers a permission request must not hang the turn for good.
const POLL: Duration = Duration::from_millis(25);

/// Time an in-flight turn is given to stop once input ends.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Which dialect an endpoint speaks.
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
pub struct ServerConfig {
    /// State layout, used to create and open sessions.
    pub paths: Paths,
    /// Primary workspace every session is rooted at.
    pub workspace: Utf8PathBuf,
    /// Endpoint model requests go to.
    pub endpoint: Endpoint,
    /// Dialect the endpoint speaks.
    pub dialect: Dialect,
    /// Model used unless a session overrides it.
    pub model: String,
    /// System instructions for every turn.
    pub instructions: String,
    /// Tool registry.
    pub registry: Registry,
    /// Rules in force before any session approval.
    pub rules: RuleSet,
    /// Permission mode used unless a session overrides it.
    pub mode: PermissionMode,
    /// Reasoning effort requested.
    pub effort: Effort,
    /// Limits in force.
    pub limits: BudgetSet,
    /// Where diagnostics go. Standard output is never used for them.
    pub log_file: Option<Utf8PathBuf>,
    /// Usable context size of the active model, reported in usage updates.
    pub context_window: u64,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("workspace", &self.workspace)
            .field("dialect", &self.dialect)
            .field("model", &self.model)
            .field("mode", &self.mode)
            .field("log_file", &self.log_file)
            .finish_non_exhaustive()
    }
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
        let registry = rune_tools::inventory::builtin(&FileLimits::from_budget(&limits), &limits)?;
        let model = model.into();
        let context_window = Catalog::new("acp")
            .metadata_or_default(&model)
            .usable_context();
        let instructions = format!(
            "You are Rune, a coding agent. The workspace is {workspace}. Inspect and change files with the tools."
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
        self.registry = rune_tools::inventory::builtin(&FileLimits::from_budget(&limits), &limits)?;
        self.limits = limits;
        Ok(self)
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
struct Queued {
    id: Id,
    session: String,
    text: String,
}

/// The state of the one turn a connection may be running.
#[derive(Default)]
struct Active {
    /// Absent when the connection is idle.
    cancellation: Option<Cancellation>,
    /// Cancels the tools the running turn invoked. Registered once the turn has
    /// taken its execution context, which is the point at which a tool may exist.
    tools: Option<rune_tools::contract::Cancellation>,
    /// Session the running turn belongs to.
    session: Option<String>,
    /// Prompts admitted while the turn runs, in arrival order.
    queue: VecDeque<Queued>,
}

/// A server serving one connection.
///
/// Shared between the reading thread and the turn thread, so it is always held
/// in an [`Arc`].
pub struct Server<W: Write + Send + 'static> {
    config: ServerConfig,
    writer: Mutex<Writer<W>>,
    sessions: Mutex<Sessions>,
    /// Requests this server issued, by identifier, awaiting a client answer.
    pending: Mutex<BTreeMap<i64, std::sync::mpsc::Sender<Response>>>,
    next_request: AtomicI64,
    log: Mutex<Option<std::fs::File>>,
    active: Mutex<Active>,
    settled: Condvar,
    /// Cap on prompts admitted while a turn runs.
    queue_depth: usize,
}

impl<W: Write + Send + 'static> fmt::Debug for Server<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("model", &self.config.model)
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
            config,
            writer: Mutex::new(Writer::new(output)),
            sessions: Mutex::new(sessions),
            pending: Mutex::new(BTreeMap::new()),
            next_request: AtomicI64::new(1),
            log: Mutex::new(log),
            active: Mutex::new(Active::default()),
            settled: Condvar::new(),
            queue_depth,
        })
    }

    /// Serves messages until the input ends.
    ///
    /// A frame the reader could not accept is answered with an error and the
    /// connection carries on. Only a failure of the stream itself ends the loop.
    pub fn run<R: BufRead>(self: &Arc<Self>, input: R) -> Result<()> {
        let mut reader = crate::jsonrpc::Reader::new(input);
        loop {
            match reader.read_message() {
                Ok(None) => break,
                Ok(Some(message)) => self.dispatch(message),
                Err(err) if err.is_fatal() => {
                    self.shutdown();
                    return Err(err.to_rune_error());
                }
                Err(err) => self.reject(&err),
            }
        }
        self.shutdown();
        Ok(())
    }

    /// Reports a frame the reader could not accept.
    fn reject(&self, err: &FrameError) {
        self.note(format_args!("rejected a frame: {err}"));
        self.fail(None, err.to_rpc_error());
    }

    /// Writes one frame.
    fn send(&self, message: &Message) {
        let outcome = match self.writer.lock() {
            Ok(mut writer) => writer.write_message(message),
            Err(_) => return,
        };
        if let Err(err) = outcome {
            self.note(format_args!("could not write a frame: {err}"));
        }
    }

    /// Writes a diagnostic line.
    ///
    /// Never to standard output: a line there is a frame as far as the client is
    /// concerned, and a malformed frame costs the connection.
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
        self.send(&Message::Response(Response::ok(id, result)));
    }

    /// Reports a failure for a request.
    fn fail(&self, id: Option<Id>, error: RpcError) {
        self.send(&Message::Response(Response::failed(id, error)));
    }

    /// Routes a client's answer to the request that asked for it.
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
                // A receiver that went away is a turn that already stopped,
                // which is normal rather than a fault.
                let _ = sender.send(response);
            }
            None => self.note(format_args!("ignoring a response for unknown request {id}")),
        }
    }

    /// Issues a request to the client and waits for its answer.
    fn ask(&self, method: &str, params: Value, cancellation: &Cancellation) -> Option<Value> {
        let id = self.next_request.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = std::sync::mpsc::channel();
        if let Ok(mut guard) = self.pending.lock() {
            guard.insert(id, sender);
        }
        self.send(&Message::Request(Request::new(
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

    /// Routes one incoming message.
    fn dispatch(self: &Arc<Self>, message: Message) {
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

    /// Handles a request, answering it exactly once.
    fn call(self: &Arc<Self>, request: Request) {
        let id = request.id;
        match self.handle(&request.method, &request.params, Some(id.clone())) {
            Ok(Some(result)) => self.respond(id, result),
            // A queued prompt answers itself once it runs.
            Ok(None) => {}
            Err(error) => self.fail(Some(id), error),
        }
    }

    /// Routes a method to its handler.
    ///
    /// Returns `None` only when the method took ownership of its response.
    fn handle(
        self: &Arc<Self>,
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
            "session/list" => self.list_sessions().map(Some),
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
        self.note(format_args!("client `{client}` opened a connection"));
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
        if let Some(cwd) = optional_string(params, "cwd")? {
            require_absolute(&Utf8PathBuf::from(cwd), "cwd")?;
        }
        if let Some(list) = params.get("mcpServers").and_then(Value::as_array)
            && !list.is_empty()
        {
            // No MCP client is wired in, so the request is reported rather than
            // implying servers the session will never reach.
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
            self.send(&Message::Notification(update));
        }
        self.session_state(&id)
    }

    /// Answers `session/resume`, which replays nothing.
    fn resume_session(&self, params: &Value) -> std::result::Result<Value, RpcError> {
        let id = string_field(params, "sessionId")?;
        let mut sessions = self.lock_sessions()?;
        sessions
            .resume(&id)
            .map_err(|err| RpcError::from_rune(&err))?;
        drop(sessions);
        self.session_state(&id)
    }

    /// Renders the modes and configuration options of an open session.
    fn session_state(&self, id: &str) -> std::result::Result<Value, RpcError> {
        let sessions = self.lock_sessions()?;
        let session = sessions.get(id).map_err(|err| RpcError::from_rune(&err))?;
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
    fn list_sessions(&self) -> std::result::Result<Value, RpcError> {
        let sessions = self.lock_sessions()?;
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
        Ok(json!({ "sessions": listed }))
    }

    /// Answers `session/set_config_option`, returning the resulting options.
    fn set_config_option(&self, params: &Value) -> std::result::Result<Value, RpcError> {
        let id = string_field(params, "sessionId")?;
        let option = string_field(params, "configId")?;
        let value = string_field(params, "value")?;
        let mut sessions = self.lock_sessions()?;
        sessions
            .set_config_option(&id, &option, &value)
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

    /// Requests cancellation for the active turn of one session.
    fn cancel_session(&self, id: &str) {
        let Ok(active) = self.active.lock() else {
            return;
        };
        let belongs_to_caller = active.session.as_deref() == Some(id);
        let cancellation = active.cancellation.clone();
        let tools = active.tools.clone();
        drop(active);

        let Some(cancellation) = cancellation else {
            return;
        };
        if !belongs_to_caller {
            self.note(format_args!(
                "a cancel for `{id}` was ignored; the active turn belongs to another session"
            ));
            return;
        }
        cancellation.cancel();
        if let Some(tools) = tools {
            tools.cancel();
        }
    }

    /// Admits a prompt.
    ///
    /// The turn runs on its own thread so the reading thread stays free. A prompt
    /// arriving while a turn runs is queued and answered once that turn settles,
    /// which is what keeps a client from losing a message typed while the agent
    /// was working.
    fn prompt(
        self: &Arc<Self>,
        params: &Value,
        id: Option<Id>,
    ) -> std::result::Result<Option<Value>, RpcError> {
        let Some(id) = id else {
            return Err(RpcError::invalid_params(
                "`session/prompt` needs an identifier so its result has somewhere to go",
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

        let arriving = Queued { id, session, text };
        let running = {
            let Ok(mut active) = self.active.lock() else {
                return Err(RpcError::internal("the session state is unavailable"));
            };
            if active.cancellation.is_some() {
                {
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
                        "queued a prompt for `{}` behind the running turn",
                        arriving.session
                    ));
                    active.queue.push_back(arriving);
                    None
                }
            } else {
                active.cancellation = Some(Cancellation::new());
                active.tools = None;
                active.session = Some(arriving.session.clone());
                Some(arriving)
            }
        };
        if let Some(arriving) = running {
            let server = Arc::clone(self);
            std::thread::spawn(move || run_queued(&server, arriving));
        }
        Ok(None)
    }

    /// Locks the session map, reporting a poisoned lock as an internal fault.
    fn lock_sessions(&self) -> std::result::Result<std::sync::MutexGuard<'_, Sessions>, RpcError> {
        self.sessions
            .lock()
            .map_err(|_| RpcError::internal("the session state is unavailable"))
    }

    /// Stops any running turn and waits briefly for it to finish reporting.
    fn shutdown(&self) {
        let Ok(mut active) = self.active.lock() else {
            return;
        };
        if let Some(cancellation) = active.cancellation.clone() {
            cancellation.cancel();
        }
        if let Some(tools) = active.tools.clone() {
            tools.cancel();
        }
        let deadline = Instant::now().checked_add(SHUTDOWN_GRACE);
        let expired = |at: Option<Instant>| at.is_none_or(|deadline| Instant::now() >= deadline);
        while active.cancellation.is_some() && !expired(deadline) {
            match self.settled.wait_timeout(active, POLL) {
                Ok((guard, _)) => active = guard,
                Err(_) => return,
            }
        }
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

/// Runs one turn, then every prompt that was queued behind it.
///
/// The queue drains here rather than on the reading thread, so the order the
/// prompts arrived in is the order they run and the order they are answered in.
fn run_queued<W: Write + Send + 'static>(server: &Arc<Server<W>>, first: Queued) {
    let mut next = Some(first);
    while let Some(queued) = next.take() {
        run_one(server, queued);
        next = {
            let Ok(mut active) = server.active.lock() else {
                return;
            };
            if let Some(following) = active.queue.pop_front() {
                active.cancellation = Some(Cancellation::new());
                active.tools = None;
                active.session = Some(following.session.clone());
                Some(following)
            } else {
                active.cancellation = None;
                active.tools = None;
                active.session = None;
                server.settled.notify_all();
                None
            }
        };
    }
}

/// Runs one accepted prompt as a turn and reports its outcome.
fn run_one<W: Write + Send + 'static>(server: &Arc<Server<W>>, queued: Queued) {
    let session = queued.session.clone();
    let snapshot = match server.sessions.lock() {
        Ok(sessions) => sessions.snapshot(&session),
        Err(_) => Err(RuneError::new(
            ErrorCode::Internal,
            "the session state is unavailable",
        )),
    };
    let snapshot = match snapshot {
        Ok(snapshot) => snapshot,
        Err(err) => {
            server.fail(Some(queued.id), RpcError::from_rune(&err));
            return;
        }
    };

    let cancellation = {
        // The tool handle is registered here, because this is the point a tool
        // can first be running.
        let tools = snapshot.context.cancellation();
        server
            .active
            .lock()
            .map(|mut active| {
                active.tools = Some(tools);
                active.cancellation.clone().unwrap_or_default()
            })
            .unwrap_or_default()
    };

    server.record(
        &session,
        SessionEvent::UserMessage {
            text: queued.text.clone(),
        },
    );
    server.send(&Message::Notification(message_chunk(
        &session,
        "user_message_chunk",
        &queued.text,
    )));

    let host = TurnHost {
        server: Arc::clone(server),
        session: session.clone(),
        config: snapshot.config.clone(),
        rules: Mutex::new(snapshot.rules),
        context: snapshot.context.clone(),
        cancellation: cancellation.clone(),
        steering: snapshot.steering,
        tools: server.config.registry.all_schemas(),
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
            // The loop reports the answer once the turn finishes, so the text
            // reaches the client as one chunk rather than as it is produced.
            if !outcome.text.is_empty() {
                server.send(&Message::Notification(message_chunk(
                    &session,
                    "agent_message_chunk",
                    &outcome.text,
                )));
                server.record(
                    &session,
                    SessionEvent::AssistantMessage {
                        turn: u64::from(outcome.steps),
                        text: outcome.text.clone(),
                    },
                );
            }
            server.absorb(&session, history, rules);
            if outcome.stop_reason == StopReason::ProviderFailure {
                server.fail(
                    Some(queued.id),
                    RpcError::from_rune(&RuneError::new(
                        ErrorCode::TransportFailure,
                        "the provider failed before the turn finished",
                    )),
                );
                return;
            }
            server.retire_tools();
            server.respond(
                queued.id,
                json!({ "stopReason": stop_reason(outcome.stop_reason) }),
            );
        }
        Err(err) if cancellation.is_cancelled() || err.code() == ErrorCode::Cancelled => {
            server.absorb(&session, history, rules);
            server.retire_tools();
            server.respond(queued.id, json!({ "stopReason": "cancelled" }));
        }
        Err(err) => {
            server.absorb(&session, history, rules);
            server.retire_tools();
            server.note(format_args!("the turn failed: {err}"));
            server.fail(Some(queued.id), RpcError::from_rune(&err));
        }
    }
}

/// Returns the protocol stop reason for a turn outcome.
///
/// The loop's own names are internal; a client is told one of the reasons the
/// protocol defines.
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

impl<W: Write + Send + 'static> Server<W> {
    /// Appends one event to a session log, reporting a failure without stopping.
    fn record(&self, session: &str, event: SessionEvent) {
        if let Ok(mut sessions) = self.sessions.lock()
            && let Err(err) = sessions.record(session, event)
        {
            self.note(format_args!("could not record an event: {err}"));
        }
    }

    /// Clears the tool cancellation handle of a turn that has stopped.
    fn retire_tools(&self) {
        if let Ok(mut active) = self.active.lock() {
            active.tools = None;
        }
    }

    /// Replaces a session's conversation with what the turn produced.
    fn absorb(&self, session: &str, history: History, rules: RuleSet) {
        if let Ok(mut sessions) = self.sessions.lock()
            && let Err(err) = sessions.absorb(session, history, rules)
        {
            self.note(format_args!("could not retain the turn: {err}"));
        }
    }
}

/// The host a turn runs against.
struct TurnHost<W: Write + Send + 'static> {
    server: Arc<Server<W>>,
    session: String,
    config: SessionConfig,
    rules: Mutex<RuleSet>,
    context: ExecutionContext,
    cancellation: Cancellation,
    steering: SteeringQueue,
    tools: Vec<ToolSpec>,
}

impl<W: Write + Send + 'static> TurnHost<W> {
    /// Reports a tool call to the client.
    fn announce(&self, call: &rune_agent::turn::PreparedCall) {
        let arguments: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
        let target = rune_agent::turn::permission_target_for(
            &self.server.config.registry,
            &call.name,
            &arguments,
        );
        self.server.send(&Message::Notification(tool_call(
            &self.session,
            &call.id,
            &call.name,
            target.as_deref(),
            "in_progress",
            &arguments,
        )));
    }

    /// Asks the client to resolve a call the rules left open.
    fn request_permission(&self, name: &str, target: Option<&str>) -> (Outcome, String) {
        let reference = self.server.next_request.fetch_add(1, Ordering::SeqCst);
        let params = json!({
            "sessionId": self.session,
            "toolCall": {
                "toolCallId": format!("call-{reference}"),
                "title": tool_title(name, target),
                "name": name,
                "kind": kind_for_name(name),
                "status": "pending",
            },
            "options": [
                {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "allow_always", "name": "Allow for this session", "kind": "allow_always"},
                {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
            ],
        });
        let Some(answer) =
            self.server
                .ask("session/request_permission", params, &self.cancellation)
        else {
            return (
                Outcome::Deny,
                format!("the approval for {name} was not answered"),
            );
        };
        let option = answer
            .get("outcome")
            .and_then(|outcome| outcome.get("optionId"))
            .and_then(Value::as_str)
            .unwrap_or("reject_once");
        match option {
            "allow_once" => (Outcome::Allow, format!("the user allowed {name} once")),
            "allow_always" => {
                if let Ok(mut rules) = self.rules.lock() {
                    crate::session::grant(&mut rules, name, target);
                }
                (
                    Outcome::Allow,
                    format!("the user allowed {name} for the rest of the session"),
                )
            }
            _ => (Outcome::Deny, format!("the user rejected {name}")),
        }
    }
}

impl<W: Write + Send + 'static> Host for TurnHost<W> {
    fn dialect(&self) -> &dyn Provider {
        self.server.config.dialect.provider()
    }

    fn endpoint(&self) -> &Endpoint {
        &self.server.config.endpoint
    }

    fn model(&self) -> &str {
        &self.config.model
    }

    fn instructions(&self) -> String {
        self.server.config.instructions.clone()
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
                self.server.send(&Message::Notification(message_chunk(
                    &self.session,
                    "agent_message_chunk",
                    &delta,
                )));
            }
            Event::ReasoningDelta { delta } => {
                self.server.send(&Message::Notification(message_chunk(
                    &self.session,
                    "agent_thought_chunk",
                    &delta,
                )));
            }
            Event::ToolStarted { call, .. } => self.announce(&call),
            Event::ToolFinished { call, is_error } => {
                let status = if is_error { "failed" } else { "completed" };
                self.server.send(&Message::Notification(tool_call_update(
                    &self.session,
                    &call.id,
                    status,
                    None,
                )));
            }
            Event::ToolDenied { call, reason } => {
                self.server.send(&Message::Notification(tool_call_update(
                    &self.session,
                    &call.id,
                    "failed",
                    Some(&reason),
                )));
            }
            Event::Finished { usage, .. } => {
                self.server.send(&Message::Notification(usage_update(
                    &self.session,
                    reported_tokens(usage),
                    self.server.config.context_window,
                )));
            }
            Event::TurnStarted { .. } | Event::SteeringApplied { .. } => {}
        }
    }

    fn execute(&self, name: &str, arguments: &Value) -> Result<ToolOutput> {
        self.server
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
            // This server has no reviewer of its own, so the client is the
            // reviewer for a call the rules left open.
            Outcome::Ask => self.request_permission(name, target),
            resolved => (resolved, reason),
        }
    }

    fn context(&self) -> ExecutionContext {
        self.context.clone()
    }

    fn limits(&self) -> BudgetSet {
        self.server.config.limits.clone()
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

/// Rejects a relative path in a member the protocol requires to be absolute.
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
/// refused by name, because this server advertises support for none of them.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reasons_map_to_the_protocol_names() {
        assert_eq!(stop_reason(StopReason::Completed), "end_turn");
        assert_eq!(stop_reason(StopReason::Cancelled), "cancelled");
        assert_eq!(stop_reason(StopReason::OutputLimit), "max_tokens");
        assert_eq!(stop_reason(StopReason::StepLimit), "max_turn_requests");
        assert_eq!(stop_reason(StopReason::Refused), "refusal");
        assert_eq!(stop_reason(StopReason::ContentFilter), "refusal");
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
}
