//! The MCP client: transports, lifecycle, and the tool surface.
//!
//! A client holds one session per configured server. A server that cannot be
//! reached is marked failed without failing its siblings, unless it is declared
//! required, in which case connecting fails as a whole and the children already
//! started are stopped. An automatic reconnect is counted against the server's
//! restart limit and the count is visible in the status report.
//!
//! Two rules keep a broken server from taking the session down with it. Every
//! operation is bounded, so a hung server cannot hold a turn open. And a
//! credential whose expiry is unknown is treated as valid: the server answers
//! with a rejection if it disagrees, and only that answer triggers a reconnect.

use std::collections::BTreeMap;
use std::io::{BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use rune_core::budget::{BudgetSet, LimitName};
use rune_core::error::{ErrorCode, Result, RuneError};

use crate::mcp::config::{ServerConfig, Transport};
use crate::mcp::protocol::{self, Incoming, MAX_FRAME_BYTES};
use crate::mcp::schema::{ProjectedTool, Projection, project_page};

/// Method names, kept here so a typo is a compile error rather than a
/// server-side `method not found`.
const INITIALIZE: &str = "initialize";
const INITIALIZED: &str = "notifications/initialized";
const TOOLS_LIST: &str = "tools/list";
const TOOLS_CALL: &str = "tools/call";

/// Client name reported at initialization.
const CLIENT_NAME: &str = "rune";

/// JSON-RPC code a server uses to report a rejected or expired credential.
///
/// The MCP revisions this client speaks have no other way to say it: the
/// transport-level 401 exists only on an HTTP transport.
const UNAUTHORIZED_CODE: i64 = -32001;

/// Bytes read from a pipe in one call.
const CHUNK_BYTES: usize = 8 * 1024;

/// Time a terminated child is given to exit before it is killed outright.
const TERMINATE_GRACE: Duration = Duration::from_millis(500);

/// Time between checks while waiting for a child to exit.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Programs that deliver a signal to a process group, tried in order.
const KILL_PROGRAMS: [&str; 2] = ["/bin/kill", "kill"];

/// What a server contributed, or why it did not.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Failure {
    /// Server that failed.
    pub server: String,
    /// Why it failed.
    pub error: RuneError,
}

/// The outcome of connecting a set of servers.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ConnectReport {
    /// Names of the servers that are connected.
    pub connected: Vec<String>,
    /// Servers that did not connect, with the reason.
    pub failed: Vec<Failure>,
}

impl ConnectReport {
    /// Returns true when every configured server connected.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// A server, as reported by status output.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ServerStatus {
    /// Configured name.
    pub name: String,
    /// Transport kind.
    pub transport: &'static str,
    /// Whether the server is connected right now.
    pub connected: bool,
    /// Revision negotiated at initialization.
    pub protocol_version: Option<String>,
    /// Tools the server contributed.
    pub tools: usize,
    /// Entries the server sent that could not be projected.
    pub warnings: Vec<String>,
    /// Automatic reconnects already spent.
    pub restarts: u32,
    /// Credential expiry the server reported, absent when it reported none.
    pub credential_expires_at_ms: Option<u64>,
    /// Why the server is not connected, when it is not.
    pub last_error: Option<String>,
}

/// The result of one tool call.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CallOutcome {
    /// Server that ran the tool.
    pub server: String,
    /// Tool name the server knows.
    pub tool: String,
    /// Whether the server reported the call as a failed result.
    pub is_error: bool,
    /// Content blocks the server returned.
    pub content: Value,
    /// Text of the content blocks, bounded by `max_tool_result_bytes`.
    pub text: String,
    /// True when `text` was cut to fit.
    pub truncated: bool,
}

/// Connects to MCP servers and dispatches their tools.
#[derive(Debug)]
pub struct Client {
    limits: BudgetSet,
    variables: Arc<dyn Variables>,
    state: Mutex<State>,
}

/// Resolves the environment values a transport's credentials come from.
///
/// Credential values are read through this rather than from the process
/// directly, so a caller can supply a fixed set and a test never has to mutate
/// process-wide state.
pub trait Variables: Send + Sync + std::fmt::Debug {
    /// Returns the value of an environment variable, if it is set.
    fn value(&self, name: &str) -> Option<String>;
}

/// Reads variables from the process environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessEnvironment;

impl Variables for ProcessEnvironment {
    fn value(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// A fixed set of variable values.
#[derive(Clone, Debug, Default)]
pub struct FixedEnvironment(BTreeMap<String, String>);

impl FixedEnvironment {
    /// Builds a set from name and value pairs.
    #[must_use]
    pub fn new(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self(pairs.into_iter().collect())
    }
}

impl Variables for FixedEnvironment {
    fn value(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

#[derive(Debug, Default)]
struct State {
    servers: Vec<ServerEntry>,
    shutdown: bool,
}

/// One configured server and its live session.
#[derive(Debug)]
struct ServerEntry {
    config: ServerConfig,
    session: Option<Session>,
    tools: Vec<ProjectedTool>,
    warnings: Vec<String>,
    protocol_version: Option<String>,
    credential_expires_at_ms: Option<u64>,
    restarts: u32,
    last_error: Option<String>,
}

impl Client {
    /// Builds a client whose bounds come from a limit set.
    #[must_use]
    pub fn new(limits: &BudgetSet) -> Self {
        Self::with_variables(limits, Arc::new(ProcessEnvironment))
    }

    /// Builds a client that resolves credential variables through `variables`.
    #[must_use]
    pub fn with_variables(limits: &BudgetSet, variables: Arc<dyn Variables>) -> Self {
        Self {
            limits: limits.clone(),
            variables,
            state: Mutex::new(State::default()),
        }
    }

    /// Connects every enabled server.
    pub fn connect_all(&self, servers: &[ServerConfig]) -> Result<ConnectReport> {
        let mut state = self.lock()?;
        state.shutdown = false;
        state.servers = servers
            .iter()
            .filter(|config| config.enabled)
            .map(|config| ServerEntry::new(config.clone()))
            .collect();
        self.connect_state(&mut state)
    }

    /// Returns every tool the connected servers expose.
    pub fn tools(&self) -> Vec<ProjectedTool> {
        self.state
            .lock()
            .map(|state| {
                state
                    .servers
                    .iter()
                    .flat_map(|entry| entry.tools.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns one server's status, or `None` when it is not configured.
    pub fn server(&self, name: &str) -> Option<ServerStatus> {
        self.state.lock().ok().and_then(|state| {
            state
                .servers
                .iter()
                .find(|entry| entry.config.name == name)
                .map(ServerEntry::status)
        })
    }

    /// Returns every configured server's status.
    pub fn status(&self) -> Vec<ServerStatus> {
        self.state
            .lock()
            .map(|state| state.servers.iter().map(ServerEntry::status).collect())
            .unwrap_or_default()
    }

    /// Calls a tool by its projected name.
    pub fn call(&self, name: &str, arguments: &Value) -> Result<CallOutcome> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                "the mcp client has been shut down",
            ));
        }
        let index = state
            .servers
            .iter()
            .position(|entry| entry.tools.iter().any(|tool| tool.name == name))
            .ok_or_else(|| {
                RuneError::new(ErrorCode::NotFound, format!("no mcp tool named `{name}`"))
                    .with_hint("list the tools again; a server may have restarted")
            })?;
        let Some(entry) = state.servers.get_mut(index) else {
            return Err(RuneError::new(
                ErrorCode::NotFound,
                format!("no mcp tool named `{name}`"),
            ));
        };
        let Some(server_tool) = entry
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .map(|tool| tool.server_tool.clone())
        else {
            return Err(RuneError::new(
                ErrorCode::NotFound,
                format!("no mcp tool named `{name}`"),
            ));
        };
        // The parameters are built once so a retry replays the exact call the
        // server rejected rather than one rebuilt from the same arguments.
        let params = json!({ "name": server_tool, "arguments": arguments });
        self.call_entry(entry, &params)
    }

    /// Reconnects every configured server from scratch.
    pub fn reload(&self) -> Result<ConnectReport> {
        let mut state = self.lock()?;
        if state.shutdown {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                "the mcp client has been shut down",
            ));
        }
        let cached: Vec<ServerConfig> = {
            for entry in &mut state.servers {
                entry.close();
            }
            state
                .servers
                .iter()
                .map(|entry| entry.config.clone())
                .collect()
        };
        state.servers = cached.into_iter().map(ServerEntry::new).collect();
        self.connect_state(&mut state)
    }

    /// Stops every session, terminating every child process.
    ///
    /// Terminating a child rather than dropping the handle is the point: a
    /// server blocked on a read would otherwise keep running for the life of the
    /// machine.
    pub fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            for entry in &mut state.servers {
                entry.close();
            }
            state.shutdown = true;
        }
    }

    /// Connects the servers already in state.
    fn connect_state(&self, state: &mut State) -> Result<ConnectReport> {
        let mut report = ConnectReport::default();
        let mut fatal: Option<RuneError> = None;
        for entry in &mut state.servers {
            match self.open_entry(entry) {
                Ok(()) => report.connected.push(entry.config.name.clone()),
                Err(error) => {
                    let name = entry.config.name.clone();
                    entry.session = None;
                    entry.last_error = Some(error.message().to_owned());
                    if entry.config.required {
                        fatal =
                            Some(error.with_hint(format!(
                                "server `{name}` is required and did not start"
                            )));
                        break;
                    }
                    report.failed.push(Failure {
                        server: name,
                        error,
                    });
                }
            }
        }
        if let Some(error) = fatal {
            for entry in &mut state.servers {
                entry.close();
            }
            return Err(error);
        }
        Ok(report)
    }

    /// Opens one entry, spending a reconnect on a rejected credential.
    fn open_entry(&self, entry: &mut ServerEntry) -> Result<()> {
        match entry.establish(&self.limits, &self.variables) {
            Err(error) if error.code() == ErrorCode::AuthenticationRequired => {
                Self::spend_restart(entry)?;
                entry.establish(&self.limits, &self.variables)
            }
            other => other,
        }
    }

    /// Runs one tool call, reconnecting once if the credential is rejected.
    fn call_entry(&self, entry: &mut ServerEntry, params: &Value) -> Result<CallOutcome> {
        // A credential the server already reported as expired is refreshed
        // before the call, so the rejection is not paid for first.
        if entry.session.is_none() || !entry.credential().is_usable(now_ms()) {
            entry.establish(&self.limits, &self.variables)?;
        }
        match entry.call(params, &self.limits) {
            Err(error) if error.code() == ErrorCode::AuthenticationRequired => {
                Self::spend_restart(entry)?;
                entry.establish(&self.limits, &self.variables)?;
                match entry.call(params, &self.limits) {
                    // A credential rejected twice is not going to recover, so
                    // the session is closed rather than left half usable.
                    Err(again) if again.code() == ErrorCode::AuthenticationRequired => {
                        entry.close();
                        Err(again)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    /// Spends one reconnect from an entry's budget and tears it down.
    ///
    /// The budget is what makes a rejected credential recoverable without
    /// letting a permanently broken server be retried in a loop.
    fn spend_restart(entry: &mut ServerEntry) -> Result<()> {
        entry.close();
        if entry.restarts >= entry.config.restart_limit {
            return Err(RuneError::new(
                ErrorCode::AuthenticationRequired,
                format!(
                    "server `{}` rejected the credential and its reconnect budget is spent",
                    entry.config.name
                ),
            )
            .with_hint("raise restart_limit to allow another reconnect"));
        }
        entry.restarts = entry.restarts.saturating_add(1);
        Ok(())
    }

    /// Locks the state, reporting a poisoned lock as an internal failure.
    fn lock(&self) -> Result<MutexGuard<'_, State>> {
        self.state.lock().map_err(|err: PoisonError<_>| {
            RuneError::new(
                ErrorCode::Internal,
                format!("the mcp client state is unavailable: {err}"),
            )
        })
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl ServerEntry {
    /// Builds an entry for a configured server, without connecting.
    fn new(config: ServerConfig) -> Self {
        Self {
            config,
            session: None,
            tools: Vec::new(),
            warnings: Vec::new(),
            protocol_version: None,
            credential_expires_at_ms: None,
            restarts: 0,
            last_error: None,
        }
    }

    /// Returns the credential lifetime the server reported.
    fn credential(&self) -> Credential {
        Credential {
            expires_at_ms: self.credential_expires_at_ms,
        }
    }

    /// Returns the entry's status.
    fn status(&self) -> ServerStatus {
        ServerStatus {
            name: self.config.name.clone(),
            transport: self.config.transport.kind(),
            connected: self.session.is_some(),
            protocol_version: self.protocol_version.clone(),
            tools: self.tools.len(),
            warnings: self.warnings.clone(),
            restarts: self.restarts,
            credential_expires_at_ms: self.credential_expires_at_ms,
            last_error: self.last_error.clone(),
        }
    }

    /// Opens a session, initializes it, and lists its tools.
    fn establish(&mut self, limits: &BudgetSet, variables: &Arc<dyn Variables>) -> Result<()> {
        self.close();
        let mut session = Session::open(&self.config, variables)?;
        let startup = self.config.startup_timeout();
        let (version, result) = initialize(&mut session, startup)?;
        self.protocol_version = Some(version);
        self.credential_expires_at_ms = result
            .pointer("/_meta/expires_at_ms")
            .and_then(Value::as_u64);
        session.notify(INITIALIZED, None, startup)?;
        let page = list_tools(
            &mut session,
            limits,
            &self.config.name,
            self.config.operation_timeout(),
        )?;
        self.tools = page.tools;
        self.warnings = page.warnings;
        self.session = Some(session);
        self.last_error = None;
        Ok(())
    }

    /// Runs a tool and reduces the result.
    fn call(&mut self, params: &Value, limits: &BudgetSet) -> Result<CallOutcome> {
        let name = self.config.name.clone();
        let tool = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let timeout = self.config.operation_timeout();
        let Some(session) = self.session.as_mut() else {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                format!("server `{name}` is not connected"),
            ));
        };
        let result = session.request(TOOLS_CALL, Some(params), timeout)?;
        let limit = limits.get_usize(LimitName::MaxToolResultBytes);
        Ok(reduce_result(&name, &tool, &result, limit))
    }

    /// Ends the session, terminating any child process.
    ///
    /// The tools go with the session: a caller that kept a name from a closed
    /// server would otherwise reach a session that no longer exists.
    fn close(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.shutdown();
        }
        self.tools.clear();
        self.warnings.clear();
        self.protocol_version = None;
        self.credential_expires_at_ms = None;
    }
}

/// Initializes a session, walking down the revision ladder.
///
/// The newest revision is requested first. A server that does not recognize it
/// has two ways to say so: it names the revision it will speak, which is
/// accepted when it is on the ladder, or it refuses the request outright, which
/// is retried with the next revision down. The walk is bounded by the ladder, so
/// a server that refuses every revision ends in the refusal it gave last rather
/// than in a retry loop.
fn initialize(session: &mut Session, startup: Duration) -> Result<(String, Value)> {
    let mut refusal: Option<RuneError> = None;
    for version in protocol::SUPPORTED_VERSIONS {
        let params = json!({
            "protocolVersion": version,
            "capabilities": { "roots": {} },
            "clientInfo": { "name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION") },
        });
        match session.request(INITIALIZE, Some(&params), startup) {
            Ok(result) => match negotiate(&result) {
                Ok(selected) => return Ok((selected, result)),
                // The server named a revision this client does not speak, so the
                // next one down is requested instead.
                Err(error) => refusal = Some(error),
            },
            Err(error) => {
                if !refuses_revision(&error) {
                    return Err(error);
                }
                refusal = Some(error);
            }
        }
    }
    Err(refusal.unwrap_or_else(unsupported_revisions))
}

/// The error used when no revision on the ladder was accepted.
fn unsupported_revisions() -> RuneError {
    RuneError::new(
        ErrorCode::UnsupportedVersion,
        "the server accepted no revision this client speaks",
    )
    .with_hint(format!(
        "supported revisions are {}",
        protocol::SUPPORTED_VERSIONS.join(", ")
    ))
}

/// Returns true when a failure is the server refusing the requested revision.
///
/// Only a refusal that names the parameters or the method counts. A transport
/// failure or a timeout is not a statement about the revision, and retrying it
/// down the ladder would turn one outage into four.
fn refuses_revision(error: &RuneError) -> bool {
    matches!(
        error.code(),
        ErrorCode::InvalidField | ErrorCode::Unsupported
    )
}

/// Checks the revision a server selected against the ladder.
fn negotiate(result: &Value) -> Result<String> {
    let version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| RuneError::missing_field("protocolVersion"))?;
    if !protocol::is_supported(version) {
        return Err(RuneError::new(
            ErrorCode::UnsupportedVersion,
            format!("the server selected protocol revision `{version}`"),
        )
        .with_hint(format!(
            "supported revisions are {}",
            protocol::SUPPORTED_VERSIONS.join(", ")
        )));
    }
    Ok(version.to_owned())
}

/// Lists every tool, following `nextCursor` within the search budget.
fn list_tools(
    session: &mut Session,
    limits: &BudgetSet,
    server: &str,
    timeout: Duration,
) -> Result<Projection> {
    let cap = limits.get_usize(LimitName::McpSearchResultBytes);
    let mut page = Projection::default();
    let mut cursor: Option<String> = None;
    let mut seen: Vec<String> = Vec::new();
    let mut used = 0_usize;

    loop {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
        let result = session.request(TOOLS_LIST, Some(&params), timeout)?;
        used = used.saturating_add(result.to_string().len());
        if used > cap {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                format!("the tool listing of server `{server}` exceeds {cap} bytes"),
            )
            .with_hint(format!(
                "raise {}",
                LimitName::McpSearchResultBytes.as_str()
            )));
        }
        let next = project_page(server, &result)?;
        page.tools.extend(next.tools);
        page.warnings.extend(next.warnings);

        let Some(next) = result
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty())
        else {
            return Ok(page);
        };
        if seen.iter().any(|previous| previous == next) {
            return Err(RuneError::new(
                ErrorCode::ProtocolViolation,
                format!("server `{server}` repeated the cursor `{next}`"),
            ));
        }
        seen.push(next.to_owned());
        cursor = Some(next.to_owned());
    }
}

/// Reduces a `tools/call` result to the content and its text.
fn reduce_result(server: &str, tool: &str, result: &Value, limit: usize) -> CallOutcome {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let content = result.get("content").cloned().unwrap_or(Value::Null);
    let mut text = String::new();
    if let Some(blocks) = content.as_array() {
        for block in blocks {
            let Some(part) = block.get("text").and_then(Value::as_str) else {
                continue;
            };
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(part);
        }
    }
    let truncated = text.len() > limit;
    if truncated {
        text = cut(&text, limit);
    }
    CallOutcome {
        server: server.to_owned(),
        tool: tool.to_owned(),
        is_error,
        content,
        text,
        truncated,
    }
}

/// Returns the largest prefix of `text` that fits in `limit` bytes.
fn cut(text: &str, limit: usize) -> String {
    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text[..end].to_owned()
}

/// A live connection to one server.
#[derive(Debug)]
enum Session {
    /// A child process speaking JSON-RPC on its pipes.
    Stdio(StdioSession),
    /// A streamable HTTP endpoint.
    Http(HttpSession),
    /// The legacy event stream plus message endpoint pair.
    Sse(SseSession),
}

impl Session {
    /// Opens a session for a configured transport.
    fn open(config: &ServerConfig, variables: &Arc<dyn Variables>) -> Result<Self> {
        match &config.transport {
            Transport::Stdio {
                command,
                environment,
            } => StdioSession::open(&config.name, command, environment).map(Self::Stdio),
            Transport::Http {
                url,
                headers,
                header_env,
                bearer_token_env,
            } => HttpSession::open(
                &config.name,
                url,
                headers,
                header_env,
                variables,
                bearer_token_env.as_deref(),
            )
            .map(Self::Http),
            Transport::Sse { url } => SseSession::open(&config.name, url).map(Self::Sse),
        }
    }

    /// Sends a request and returns its result.
    fn request(
        &mut self,
        method: &str,
        params: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value> {
        match self {
            Self::Stdio(session) => session.request(method, params, timeout),
            Self::Http(session) => session.request(method, params, timeout),
            Self::Sse(session) => session.request(method, params, timeout),
        }
    }

    /// Sends a notification, which has no result.
    fn notify(&mut self, method: &str, params: Option<&Value>, timeout: Duration) -> Result<()> {
        match self {
            Self::Stdio(session) => session.notify(method, params),
            Self::Http(session) => session.notify(method, params, timeout),
            Self::Sse(session) => session.notify(method, params, timeout),
        }
    }

    /// Ends the session, terminating any child process.
    fn shutdown(&mut self) {
        if let Self::Stdio(session) = self {
            session.terminate();
        }
    }
}

/// A frame read by the reader thread.
#[derive(Debug)]
enum Event {
    /// One complete frame.
    Frame(String),
    /// The stream ended.
    End,
    /// Reading failed.
    Failed(RuneError),
}

/// A server reached over its standard input and output.
#[derive(Debug)]
struct StdioSession {
    /// Name of the server, for error messages.
    server: String,
    /// Where frames are written.
    stdin: std::process::ChildStdin,
    /// Where frames arrive.
    frames: Receiver<Event>,
    /// The child, kept so it can be terminated.
    child: Child,
    /// Counter behind request identifiers.
    next_id: u64,
}

impl StdioSession {
    /// Spawns the child and starts reading its output.
    fn open(
        server: &str,
        command: &[String],
        environment: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let program = command
            .first()
            .ok_or_else(|| RuneError::missing_field("command"))?;
        let mut builder = Command::new(program);
        builder
            .args(command.iter().skip(1))
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The child leads its own process group, so a server that starts helpers
        // is ended as a tree rather than one process at a time.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            builder.process_group(0);
        }
        let mut child = builder.spawn().map_err(|err| {
            RuneError::new(
                ErrorCode::TransportFailure,
                format!("could not start server `{server}`: {err}"),
            )
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            RuneError::new(ErrorCode::Internal, "the child has no standard input")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            RuneError::new(ErrorCode::Internal, "the child has no standard output")
        })?;
        if let Some(stderr) = child.stderr.take() {
            drain(stderr);
        }
        let (sender, frames) = std::sync::mpsc::channel();
        std::thread::spawn(move || read_frames(stdout, &sender));
        Ok(Self {
            server: server.to_owned(),
            stdin,
            frames,
            child,
            next_id: 1,
        })
    }

    /// Sends a request and waits for its response.
    fn request(
        &mut self,
        method: &str,
        params: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.write(&protocol::request(id, method, params))?;
        let deadline = Instant::now().checked_add(timeout);
        loop {
            let event = match self.frames.recv_timeout(remaining(deadline)) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => {
                    self.terminate();
                    return Err(timeout_error(&self.server, method, timeout));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(RuneError::new(
                        ErrorCode::TransportFailure,
                        format!("server `{}` closed its output", self.server),
                    ));
                }
            };
            match event {
                Event::Frame(text) => {
                    if let Incoming::Response { id: got, outcome } =
                        protocol::parse_incoming(&text)?
                        && got == id
                    {
                        return outcome.map_err(response_error);
                    }
                }
                Event::End => {
                    return Err(RuneError::new(
                        ErrorCode::TransportFailure,
                        format!(
                            "server `{}` closed its output during `{method}`",
                            self.server
                        ),
                    ));
                }
                Event::Failed(error) => return Err(error),
            }
            if remaining(deadline).is_zero() {
                self.terminate();
                return Err(timeout_error(&self.server, method, timeout));
            }
        }
    }

    /// Sends a notification.
    fn notify(&mut self, method: &str, params: Option<&Value>) -> Result<()> {
        self.write(&protocol::notification(method, params))
    }

    /// Writes one frame to the child.
    fn write(&mut self, frame: &str) -> Result<()> {
        let error = |action: &str, err: std::io::Error| {
            RuneError::new(
                ErrorCode::TransportFailure,
                format!("could not {action} server `{}`: {err}", self.server),
            )
        };
        self.stdin
            .write_all(frame.as_bytes())
            .map_err(|err| error("write to", err))?;
        self.stdin.flush().map_err(|err| error("flush", err))
    }

    /// Ends the child process group and closes the pipes.
    fn terminate(&mut self) {
        signal_group(self.child.id(), "TERM");
        if !wait_for(&mut self.child, TERMINATE_GRACE) {
            signal_group(self.child.id(), "KILL");
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

impl Drop for StdioSession {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Delivers a signal to a child's process group.
///
/// This crate forbids unsafe code and the standard library exposes no signal
/// API, so the signal is delivered by running a program that can send one.
#[cfg(unix)]
fn signal_group(group: u32, name: &str) {
    let target = format!("-{group}");
    for program in KILL_PROGRAMS {
        let delivered = Command::new(program)
            .args(["-s", name, "--", target.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if delivered {
            return;
        }
    }
}

/// Delivers no signal, on a platform with no process groups to signal.
///
/// A tree is ended through the platform's own tool instead, which is the same
/// approach the signal path takes elsewhere: this crate forbids unsafe code and
/// the standard library ends one process rather than the tree it started.
#[cfg(not(unix))]
fn signal_group(pid: u32, name: &str) {
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/T"]);
    if name == "KILL" {
        command.arg("/F");
    }
    let _ = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Waits up to `limit` for a child to exit.
fn wait_for(child: &mut Child, limit: Duration) -> bool {
    let deadline = Instant::now().checked_add(limit);
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return true,
            Ok(None) => {}
        }
        if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Reads frames from a child's output until it ends.
fn read_frames(stdout: std::process::ChildStdout, sender: &Sender<Event>) {
    let mut reader = BufReader::with_capacity(CHUNK_BYTES, stdout);
    loop {
        match protocol::read_frame(&mut reader, MAX_FRAME_BYTES) {
            Ok(Some(frame)) => {
                if sender.send(Event::Frame(frame)).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(Event::End);
                return;
            }
            Err(error) => {
                let _ = sender.send(Event::Failed(error));
                return;
            }
        }
    }
}

/// Reads a child's diagnostics and discards them.
///
/// A server that writes more diagnostics than a pipe holds would block on that
/// write and stop answering, so the pipe is drained even though nothing is kept.
fn drain(stderr: std::process::ChildStderr) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut scratch = [0_u8; CHUNK_BYTES];
        while let Ok(read) = reader.read(&mut scratch) {
            if read == 0 {
                return;
            }
        }
    });
}

/// A server reached over streamable HTTP.
#[derive(Debug)]
struct HttpSession {
    /// Name of the server, for error messages.
    server: String,
    /// Endpoint every request is posted to.
    url: String,
    /// Agent carrying the connection pool.
    agent: ureq::Agent,
    /// Headers sent with every request.
    headers: Vec<(String, String)>,
    /// Headers whose value is read from an environment variable.
    header_env: Vec<(String, String)>,
    /// Where environment values come from.
    variables: Arc<dyn Variables>,
    /// Variable holding a bearer token, when one is configured.
    bearer_token_env: Option<String>,
    /// Counter behind request identifiers.
    next_id: u64,
}

impl HttpSession {
    /// Builds a session for an HTTP endpoint.
    fn open(
        server: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        header_env: &BTreeMap<String, String>,
        variables: &Arc<dyn Variables>,
        bearer_token_env: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            server: server.to_owned(),
            url: url.to_owned(),
            agent: http_agent(),
            headers: pairs(headers),
            header_env: pairs(header_env),
            variables: Arc::clone(variables),
            bearer_token_env: bearer_token_env.map(str::to_owned),
            next_id: 1,
        })
    }

    /// Sends a request and returns its result.
    fn request(
        &mut self,
        method: &str,
        params: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let body = self.post(&encode(Some(id), method, params), timeout)?;
        protocol::response_from_body(&body, id)
    }

    /// Sends a notification and accepts any successful reply.
    fn notify(&mut self, method: &str, params: Option<&Value>, timeout: Duration) -> Result<()> {
        self.post(&encode(None, method, params), timeout)
            .map(|_| ())
    }

    /// Posts one frame and returns the reply body.
    fn post(&mut self, body: &str, timeout: Duration) -> Result<String> {
        let mut request = self
            .agent
            .post(&self.url)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        request = self.authorize(request)?;
        let response = request
            .send(body)
            .map_err(|err| transport_error(&self.server, &err))?;
        let status = response.status().as_u16();
        if status == 401 {
            return Err(unauthorized(&self.server));
        }
        if status >= 400 {
            return Err(RuneError::new(
                ErrorCode::RequestRejected,
                format!("server `{}` answered {status}", self.server),
            )
            .with_observed(status.to_string()));
        }
        read_body(response, MAX_FRAME_BYTES)
    }

    /// Applies the configured headers and credential.
    ///
    /// Values are read at request time rather than at connect time, so a token
    /// rotated in the environment is picked up without a reload.
    fn authorize(
        &self,
        mut request: ureq::RequestBuilder<ureq::typestate::WithBody>,
    ) -> Result<ureq::RequestBuilder<ureq::typestate::WithBody>> {
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        for (name, variable) in &self.header_env {
            let value = self
                .variables
                .value(variable)
                .ok_or_else(|| credential_missing(&self.server, variable))?;
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(variable) = &self.bearer_token_env {
            let token = self
                .variables
                .value(variable)
                .ok_or_else(|| credential_missing(&self.server, variable))?;
            request = request.header("authorization", format!("Bearer {token}").as_str());
        }
        Ok(request)
    }
}

/// A server reached the legacy way: an event stream plus a message endpoint.
///
/// The stream is opened for each request and dropped once the response has been
/// read. Reusing one stream would be cheaper, but a long-lived read cannot be
/// cancelled from another thread without unsafe code, and a hung stream would
/// then outlive the call that opened it. Opening per request keeps every
/// operation inside its own timeout.
#[derive(Debug)]
struct SseSession {
    /// Name of the server, for error messages.
    server: String,
    /// Endpoint opened as an event stream.
    url: String,
    /// Agent carrying the connection pool.
    agent: ureq::Agent,
    /// Counter behind request identifiers.
    next_id: u64,
}

impl SseSession {
    /// Builds a session for an event stream endpoint.
    fn open(server: &str, url: &str) -> Result<Self> {
        Ok(Self {
            server: server.to_owned(),
            url: url.to_owned(),
            agent: http_agent(),
            next_id: 1,
        })
    }

    /// Sends a request and returns its result.
    fn request(
        &mut self,
        method: &str,
        params: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let deadline = Instant::now().checked_add(timeout);
        let (mut stream, endpoint) = self.stream(timeout)?;
        let posted = self.post(
            &endpoint,
            &encode(Some(id), method, params),
            remaining(deadline),
        )?;
        // Some servers answer a POST directly; the legacy flow answers on the
        // stream. Both are accepted, and a direct answer is authoritative.
        if let Some(body) = posted
            && !body.trim().is_empty()
            && body.trim_start().starts_with('{')
        {
            return protocol::response_from_body(&body, id);
        }
        loop {
            let Some(frame) = next_event(&mut stream)? else {
                return Err(RuneError::new(
                    ErrorCode::TransportFailure,
                    format!(
                        "server `{}` closed the event stream during `{method}`",
                        self.server
                    ),
                ));
            };
            if let Incoming::Response { id: got, outcome } = protocol::parse_incoming(&frame)?
                && got == id
            {
                return outcome.map_err(response_error);
            }
            if remaining(deadline).is_zero() {
                return Err(timeout_error(&self.server, method, timeout));
            }
        }
    }

    /// Sends a notification.
    fn notify(&mut self, method: &str, params: Option<&Value>, timeout: Duration) -> Result<()> {
        let deadline = Instant::now().checked_add(timeout);
        let (_stream, endpoint) = self.stream(timeout)?;
        self.post(
            &endpoint,
            &encode(None, method, params),
            remaining(deadline),
        )
        .map(|_| ())
    }

    /// Opens the event stream and reads the message endpoint from it.
    fn stream(&self, timeout: Duration) -> Result<(SseStream, String)> {
        let response = self
            .agent
            .get(&self.url)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("accept", "text/event-stream")
            .call()
            .map_err(|err| transport_error(&self.server, &err))?;
        if response.status().as_u16() != 200 {
            return Err(RuneError::new(
                ErrorCode::RequestRejected,
                format!(
                    "the event stream of server `{}` answered {}",
                    self.server,
                    response.status().as_u16()
                ),
            ));
        }
        let mut stream = SseStream {
            reader: BufReader::new(response.into_body().into_reader()),
            event: String::new(),
            server: self.server.clone(),
        };
        loop {
            let Some((name, payload)) = stream.next_pair()? else {
                return Err(RuneError::new(
                    ErrorCode::ProtocolViolation,
                    format!(
                        "server `{}` closed the event stream before naming a message endpoint",
                        self.server
                    ),
                ));
            };
            if name == "endpoint" {
                return Ok((stream, resolve_endpoint(&self.url, &payload)));
            }
        }
    }

    /// Posts one frame to the message endpoint.
    fn post(&self, endpoint: &str, body: &str, timeout: Duration) -> Result<Option<String>> {
        let response = self
            .agent
            .post(endpoint)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("content-type", "application/json")
            .send(body)
            .map_err(|err| transport_error(&self.server, &err))?;
        let status = response.status().as_u16();
        if status == 401 {
            return Err(unauthorized(&self.server));
        }
        if status >= 400 {
            return Err(RuneError::new(
                ErrorCode::RequestRejected,
                format!("server `{}` answered {status}", self.server),
            ));
        }
        Ok(Some(read_body(response, MAX_FRAME_BYTES)?))
    }
}

/// An event stream, read one frame at a time.
struct SseStream {
    /// Lines of the stream.
    reader: BufReader<ureq::BodyReader<'static>>,
    /// Event name of the frame being read.
    event: String,
    /// Name of the server, for error messages.
    server: String,
}

impl std::fmt::Debug for SseStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseStream")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

impl SseStream {
    /// Reads the next `(event name, payload)` pair.
    fn next_pair(&mut self) -> Result<Option<(String, String)>> {
        loop {
            let Some(line) = self.line()? else {
                return Ok(None);
            };
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix("event:") {
                name.trim().clone_into(&mut self.event);
                continue;
            }
            if let Some(payload) = line.strip_prefix("data:") {
                let name = std::mem::take(&mut self.event);
                return Ok(Some((name, payload.trim().to_owned())));
            }
        }
    }

    /// Reads one line of the stream, without its terminator.
    ///
    /// The line is read through the same capped reader the other transports
    /// use, so a stream that never emits a newline is refused rather than
    /// buffered until memory runs out. Blank lines are skipped here; they
    /// separate events and carry nothing.
    fn line(&mut self) -> Result<Option<String>> {
        protocol::read_frame(&mut self.reader, MAX_FRAME_BYTES)
    }
}

/// Reads the next message payload from a stream, skipping bookkeeping events.
fn next_event(stream: &mut SseStream) -> Result<Option<String>> {
    loop {
        let Some((name, payload)) = stream.next_pair()? else {
            return Ok(None);
        };
        if payload.is_empty() {
            continue;
        }
        if name.is_empty() || name == "message" {
            return Ok(Some(payload));
        }
    }
}

/// Resolves an endpoint a server named relative to its stream URL.
fn resolve_endpoint(base: &str, endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_owned();
    }
    if endpoint.starts_with('/') {
        let (scheme, rest) = base.split_once("://").unwrap_or(("http", base));
        let authority = rest.split('/').next().unwrap_or(rest);
        return format!("{scheme}://{authority}{endpoint}");
    }
    format!("{}/{}", base.trim_end_matches('/'), endpoint)
}

/// Builds the HTTP agent shared by the remote transports.
fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

/// Copies a string map into request order.
fn pairs(map: &BTreeMap<String, String>) -> Vec<(String, String)> {
    map.iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Encodes a request or notification frame.
fn encode(id: Option<u64>, method: &str, params: Option<&Value>) -> String {
    match id {
        Some(id) => protocol::request(id, method, params),
        None => protocol::notification(method, params),
    }
}

/// Returns the time left against a deadline.
fn remaining(deadline: Option<Instant>) -> Duration {
    deadline.map_or(Duration::MAX, |deadline| {
        deadline.saturating_duration_since(Instant::now())
    })
}

/// Reads a reply body, refusing one past the frame cap.
fn read_body(response: ureq::http::Response<ureq::Body>, cap: usize) -> Result<String> {
    let limit = u64::try_from(cap.saturating_add(1)).unwrap_or(u64::MAX);
    let mut reader = response.into_body().into_reader().take(limit);
    let mut buffer: Vec<u8> = Vec::new();
    reader.read_to_end(&mut buffer).map_err(RuneError::from)?;
    if buffer.len() > cap {
        return Err(RuneError::too_large("frame", buffer.len(), cap)
            .with_hint("the server sent one reply larger than the frame cap"));
    }
    String::from_utf8(buffer).map_err(|err| {
        RuneError::new(
            ErrorCode::ProtocolViolation,
            format!("a reply is not valid UTF-8: {err}"),
        )
    })
}

/// Maps a server-reported failure onto the taxonomy.
fn response_error(error: protocol::RpcError) -> RuneError {
    if error.code == UNAUTHORIZED_CODE {
        return RuneError::new(ErrorCode::AuthenticationRequired, error.message);
    }
    error.to_rune()
}

/// Maps a transport failure onto the taxonomy.
fn transport_error(server: &str, err: &ureq::Error) -> RuneError {
    let code = match err {
        ureq::Error::Timeout(_) => ErrorCode::Timeout,
        _ => ErrorCode::TransportFailure,
    };
    RuneError::new(code, format!("server `{server}`: {err}"))
}

/// The error used when a credential is rejected.
fn unauthorized(server: &str) -> RuneError {
    RuneError::new(
        ErrorCode::AuthenticationRequired,
        format!("server `{server}` rejected the credential"),
    )
    .with_hint("check the token the configured environment variable holds")
}

/// The error used when a configured credential variable is unset.
fn credential_missing(server: &str, variable: &str) -> RuneError {
    RuneError::new(
        ErrorCode::AuthenticationRequired,
        format!("`{variable}` is not set for server `{server}`"),
    )
    .with_hint("export the variable before starting Rune")
}

/// The error used when an operation outlives its timeout.
fn timeout_error(server: &str, method: &str, timeout: Duration) -> RuneError {
    RuneError::new(
        ErrorCode::Timeout,
        format!(
            "server `{server}` did not answer `{method}` within {} ms",
            timeout.as_millis()
        ),
    )
    .with_hint("raise mcp_operation_timeout_ms for a slow server")
}

/// Returns the wall clock in milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// What a credential's reported lifetime says about using it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expiry {
    /// The server reported no expiry, so only a rejection can say the credential
    /// is no longer accepted.
    Unknown,
    /// The reported expiry has passed.
    Passed,
    /// The reported expiry is still ahead.
    Ahead,
}

/// A credential's reported lifetime.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Credential {
    /// Expiry the server reported, in milliseconds since the Unix epoch.
    pub expires_at_ms: Option<u64>,
}

impl Credential {
    /// Classifies the credential against a clock reading.
    ///
    /// An unreported expiry is [`Expiry::Unknown`], and an unknown expiry is
    /// treated as usable. Refusing a credential because its lifetime is
    /// unstated latches a healthy server into a failed state for the rest of the
    /// session, which is worse than one rejected request that recovers.
    #[must_use]
    pub const fn state(&self, now_ms: u64) -> Expiry {
        match self.expires_at_ms {
            None => Expiry::Unknown,
            Some(expiry) if now_ms >= expiry => Expiry::Passed,
            Some(_) => Expiry::Ahead,
        }
    }

    /// Returns true when the credential may be used as it stands.
    #[must_use]
    pub const fn is_usable(&self, now_ms: u64) -> bool {
        !matches!(self.state(now_ms), Expiry::Passed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_without_a_reported_expiry_is_usable() {
        let credential = Credential {
            expires_at_ms: None,
        };
        assert_eq!(credential.state(0), Expiry::Unknown);
        assert!(credential.is_usable(0));
        assert!(credential.is_usable(u64::MAX / 2));
    }

    #[test]
    fn a_credential_expiry_is_compared_against_the_clock() {
        let credential = Credential {
            expires_at_ms: Some(1_000),
        };
        assert_eq!(credential.state(999), Expiry::Ahead);
        assert!(credential.is_usable(999));
        assert_eq!(credential.state(1_000), Expiry::Passed);
        assert!(!credential.is_usable(1_000));
    }

    #[test]
    fn a_result_carries_the_text_of_its_content_blocks() {
        let result = json!({
            "content": [
                { "type": "text", "text": "first" },
                { "type": "image", "data": "AA==" },
                { "type": "text", "text": "second" }
            ]
        });
        let outcome = reduce_result("s", "t", &result, 1024);
        assert_eq!(outcome.text, "first\nsecond");
        assert!(!outcome.is_error);
        assert!(!outcome.truncated);
    }

    #[test]
    fn a_result_reports_the_server_marking_it_failed() {
        let result = json!({ "content": [], "isError": true });
        assert!(reduce_result("s", "t", &result, 1024).is_error);
    }

    #[test]
    fn a_result_text_is_cut_at_the_limit_and_marked() {
        let text = "x".repeat(64);
        let result = json!({ "content": [{ "type": "text", "text": text }] });
        let outcome = reduce_result("s", "t", &result, 16);
        assert_eq!(outcome.text.len(), 16);
        assert!(outcome.truncated);
    }

    #[test]
    fn a_timeout_error_names_the_server_and_the_method() {
        let error = timeout_error("slow", "tools/list", Duration::from_millis(50));
        assert_eq!(error.code(), ErrorCode::Timeout);
        assert!(error.message().contains("slow"), "{}", error.message());
        assert!(
            error.message().contains("tools/list"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn a_revision_outside_the_ladder_is_refused() {
        let error = negotiate(&json!({ "protocolVersion": "2026-07-28" })).expect_err("refused");
        assert_eq!(error.code(), ErrorCode::UnsupportedVersion);
        assert_eq!(
            negotiate(&json!({ "protocolVersion": "2024-11-05" })).expect("accepted"),
            "2024-11-05"
        );
        assert_eq!(
            negotiate(&json!({})).expect_err("refused").code(),
            ErrorCode::MissingField
        );
    }

    #[test]
    fn a_client_without_servers_exposes_nothing() {
        let client = Client::new(&BudgetSet::new());
        assert!(client.tools().is_empty());
        assert!(client.status().is_empty());
        assert_eq!(
            client
                .call("mcp_x_y", &json!({}))
                .expect_err("refused")
                .code(),
            ErrorCode::NotFound
        );
    }

    #[test]
    fn a_shut_down_client_refuses_further_work() {
        let client = Client::new(&BudgetSet::new());
        client.connect_all(&[]).expect("connected");
        client.shutdown();
        assert_eq!(
            client.reload().expect_err("refused").code(),
            ErrorCode::InvalidState
        );
    }

    #[test]
    fn a_relative_endpoint_resolves_against_the_stream_url() {
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:8000/sse", "/messages"),
            "http://127.0.0.1:8000/messages"
        );
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:8000/sse", "messages"),
            "http://127.0.0.1:8000/sse/messages"
        );
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:8000/sse", "https://other.test/m"),
            "https://other.test/m"
        );
    }

    #[test]
    fn a_notification_frame_carries_no_id() {
        let frame: Value = serde_json::from_str(&encode(None, INITIALIZED, None)).expect("parsed");
        assert!(frame.get("id").is_none());
    }

    #[test]
    fn the_clock_reading_is_nonzero() {
        assert!(now_ms() > 1_600_000_000_000);
    }
}
