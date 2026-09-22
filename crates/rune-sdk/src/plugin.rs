//! The subprocess plugin protocol.
//!
//! A plugin is a program that exposes tools over newline-delimited JSON-RPC on
//! its standard streams. One JSON object per line, in both directions, bounded
//! by [`MAX_FRAME_BYTES`]. The framing is written here rather than taken from a
//! dependency because the bound has to be enforced where the bytes arrive, and
//! because the whole protocol is four frame shapes.
//!
//! The handshake is `initialize`, whose result must declare the version the
//! plugin speaks; a plugin whose manifest or handshake declares another version
//! is refused before any tool is called. A tool call is `tools/call`, and its
//! result is either a string or an object carrying `text` and an optional
//! `is_error`. `shutdown` is a notification, after which the process is expected
//! to exit.
//!
//! Three properties are load-bearing:
//!
//! - A plugin the host has not authorized is never sent a call. The check runs
//!   before a frame is written, so a denied call cannot leave the process.
//! - A plugin that dies or misses a deadline is replaced within a bounded
//!   restart budget, and is disabled once that budget is spent rather than
//!   restarted forever.
//! - Every wait is bounded: startup, each call, and each frame.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rune_core::budget::{BudgetSet, LimitName};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::tool::{ToolSpec, validate_tool_specs};
use rune_tools::contract::ToolOutput;
use serde::{Deserialize, Serialize};

use crate::agent::{HostTool, HostToolResult};

/// Protocol version this build speaks.
pub const PLUGIN_PROTOCOL_VERSION: u32 = 1;

/// Manifest file read from a plugin directory.
pub const MANIFEST_FILE: &str = "plugin.json";

/// Largest frame accepted in either direction, in bytes.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// How often a terminating child is polled for.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Timeout used when a limit is set to unlimited.
const FALLBACK_TIMEOUT_MS: u64 = 60_000;

/// One tool a plugin exposes.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PluginTool {
    /// Tool name, as the model must call it.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema for the arguments object.
    pub input_schema: serde_json::Value,
}

impl PluginTool {
    /// Projects the tool for a model request.
    #[must_use]
    pub fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

/// What a plugin declares about itself.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Protocol version the plugin speaks.
    pub protocol_version: u32,
    /// Plugin name, used in diagnostics.
    pub name: String,
    /// Tools the plugin exposes.
    #[serde(default)]
    pub tools: Vec<PluginTool>,
    /// Program and arguments that start the plugin.
    #[serde(default)]
    pub commands: Vec<String>,
}

impl PluginManifest {
    /// Reads a manifest from a file, or from `plugin.json` inside a directory.
    pub fn load(path: &Path) -> Result<Self> {
        let file = if path.is_dir() {
            path.join(MANIFEST_FILE)
        } else {
            path.to_path_buf()
        };
        let bytes = std::fs::read(&file).map_err(|err| {
            RuneError::new(
                ErrorCode::NotFound,
                format!(
                    "the plugin manifest `{}` could not be read: {err}",
                    file.display()
                ),
            )
        })?;
        let manifest: Self = serde_json::from_slice(&bytes).map_err(|err| {
            RuneError::invalid_field(
                MANIFEST_FILE,
                format!("`{}` is not a valid manifest: {err}", file.display()),
            )
        })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Refuses a manifest this build cannot honour.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(RuneError::missing_field("plugin.name"));
        }
        if self.protocol_version != PLUGIN_PROTOCOL_VERSION {
            return Err(incompatible_version(&self.name, self.protocol_version));
        }
        match self.commands.first() {
            Some(program) if !program.trim().is_empty() => {}
            _ => {
                return Err(RuneError::missing_field("plugin.commands")
                    .with_hint("the first entry is the program to run"));
            }
        }
        let specs: Vec<ToolSpec> = self.tools.iter().map(PluginTool::spec).collect();
        validate_tool_specs(&specs)?;
        Ok(())
    }
}

/// Builds the error for a plugin this build cannot speak to.
fn incompatible_version(name: &str, declared: u32) -> RuneError {
    RuneError::new(
        ErrorCode::UnsupportedVersion,
        format!(
            "plugin `{name}` speaks protocol version {declared}, but this build speaks version {PLUGIN_PROTOCOL_VERSION}"
        ),
    )
    .with_hint("rebuild the plugin against this version of the SDK")
}

/// A plugin process and the conversation with it.
#[derive(Debug)]
struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: Receiver<Incoming>,
}

impl Session {
    /// Returns true when the process has already ended.
    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)) | Err(_))
    }

    /// Closes the plugin's input and waits for it to exit.
    ///
    /// Returns false when it was still running at the deadline, which the
    /// caller answers by killing it.
    fn finish(&mut self, grace: Duration) -> bool {
        self.stdin = None;
        let started = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return true,
                Ok(None) => {}
            }
            if started.elapsed() >= grace {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Killing the child closes its output, which ends the reader by itself.
        // The reader handle is dropped rather than joined: a grandchild holding
        // the pipe open would otherwise block here forever.
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One frame from the plugin, or the reason there will be no more.
#[derive(Debug)]
enum Incoming {
    /// A complete frame.
    Frame(String),
    /// The plugin closed its output.
    Closed,
    /// The stream ended illegally.
    Failed(RuneError),
}

/// A plugin the host has started.
#[derive(Debug)]
pub struct PluginHost {
    name: String,
    tools: Vec<PluginTool>,
    command: Vec<String>,
    limits: BudgetSet,
    session: Option<Session>,
    restarts: u32,
    disabled: Option<String>,
    stopped: bool,
    next_id: u64,
}

impl PluginHost {
    /// Starts the plugin a manifest describes.
    ///
    /// The plugin must complete a handshake within the startup timeout; a
    /// plugin that keeps dying is disabled once its restart budget is spent,
    /// and that failure is returned rather than retried forever.
    pub fn start(path: impl AsRef<Path>) -> Result<Self> {
        Self::start_with_limits(path, BudgetSet::new())
    }

    /// Starts the plugin with explicit limits.
    pub fn start_with_limits(path: impl AsRef<Path>, limits: BudgetSet) -> Result<Self> {
        let manifest = PluginManifest::load(path.as_ref())?;
        let mut host = Self {
            name: manifest.name,
            tools: manifest.tools,
            command: manifest.commands,
            limits,
            session: None,
            restarts: 0,
            disabled: None,
            stopped: false,
            next_id: 1,
        };
        host.attach()?;
        Ok(host)
    }

    /// Returns the plugin name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the tools the plugin declares.
    #[must_use]
    pub fn list_tools(&self) -> Vec<PluginTool> {
        self.tools.clone()
    }

    /// Returns true once the plugin has been disabled or shut down.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.disabled.is_some()
    }

    /// Returns why the plugin was disabled.
    #[must_use]
    pub fn disabled_reason(&self) -> Option<&str> {
        self.disabled.as_deref()
    }

    /// Returns how many restarts have been spent.
    #[must_use]
    pub const fn restarts(&self) -> u32 {
        self.restarts
    }

    /// Calls one tool.
    pub fn call(&mut self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        self.check_tool(name)?;
        self.dispatch(name, arguments)
    }

    /// Calls one tool after the host authorizes it.
    ///
    /// The decision is consulted before anything reaches the plugin, so a
    /// refused call cannot run, be logged by the plugin, or be observed by the
    /// model.
    pub fn call_with_authorization(
        &mut self,
        name: &str,
        arguments: &serde_json::Value,
        authorize: &dyn Fn(&str, &serde_json::Value) -> bool,
    ) -> Result<ToolOutput> {
        self.check_tool(name)?;
        if !authorize(name, arguments) {
            return Err(RuneError::new(
                ErrorCode::PermissionDenied,
                format!("`{name}` was refused"),
            )
            .with_hint("a refused call is never sent to the plugin"));
        }
        self.dispatch(name, arguments)
    }

    /// Shuts the plugin down, killing it when it does not exit in time.
    pub fn shutdown(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        if let Some(mut session) = self.session.take() {
            let frame = notification_frame("shutdown");
            if let Some(stdin) = session.stdin.as_mut() {
                let _ = stdin.write_all(frame.as_bytes());
                let _ = stdin.write_all(b"\n");
                let _ = stdin.flush();
            }
            session.finish(self.grace());
        }
        Ok(())
    }

    /// Refuses a name the plugin does not expose.
    fn check_tool(&self, name: &str) -> Result<()> {
        if self.tools.iter().any(|tool| tool.name == name) {
            return Ok(());
        }
        Err(RuneError::new(
            ErrorCode::NotFound,
            format!("plugin `{}` exposes no tool named `{name}`", self.name),
        ))
    }

    /// Sends one call and reduces its result.
    fn dispatch(&mut self, name: &str, arguments: &serde_json::Value) -> Result<ToolOutput> {
        let id = self.take_id();
        let frame = request_frame(
            id,
            "tools/call",
            Some(serde_json::json!({ "name": name, "arguments": arguments })),
        );
        // Refused before the plugin is touched. An argument too large for one
        // frame is the caller's to fix, and spending a restart on it would kill
        // a plugin that did nothing wrong.
        check_frame(&frame)?;
        self.ensure_running()?;
        if let Err(err) = self.send(&frame) {
            self.note_failure(&err);
            return Err(err);
        }

        match self.wait(id, self.operation_timeout()) {
            Ok(result) => self.reduce(result),
            Err(err) => {
                // A plugin that missed its deadline or lost its output is not
                // reused: the pending response can no longer be matched, and a
                // stream cannot be resynchronised after a partial frame.
                self.note_failure(&err);
                Err(err)
            }
        }
    }

    /// Turns a tool result into the workspace result type.
    fn reduce(&self, value: serde_json::Value) -> Result<ToolOutput> {
        let (text, is_error) = match value {
            serde_json::Value::String(text) => (text, false),
            serde_json::Value::Object(object) => {
                let text = object
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        RuneError::new(
                            ErrorCode::ProtocolViolation,
                            "a tool result carries no `text` field",
                        )
                    })?
                    .to_owned();
                let is_error = object
                    .get("is_error")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                (text, is_error)
            }
            _ => {
                return Err(RuneError::new(
                    ErrorCode::ProtocolViolation,
                    "a tool result is neither a string nor an object",
                ));
            }
        };

        let produced_bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
        Ok(ToolOutput {
            text: crate::agent::bound_text(
                &text,
                self.limits.get_usize(LimitName::MaxToolResultBytes),
            ),
            is_error,
            produced_bytes,
        })
    }

    /// Ensures a live plugin, restarting it within the budget.
    fn ensure_running(&mut self) -> Result<()> {
        if self.stopped {
            return Err(RuneError::new(
                ErrorCode::InvalidState,
                format!("plugin `{}` was shut down", self.name),
            )
            .with_hint("build a new host to start it again"));
        }
        if let Some(reason) = self.disabled.clone() {
            return Err(disabled_error(&self.name, &reason));
        }
        if let Some(session) = self.session.as_mut()
            && session.exited()
        {
            // The process ended between calls. Its input pipe is gone, so the
            // next call needs a fresh process, which spends a restart.
            self.session = None;
            let cause = RuneError::new(
                ErrorCode::TransportFailure,
                format!("plugin `{}` exited between calls", self.name),
            );
            self.note_failure(&cause);
            if self.disabled.is_some() {
                return Err(self.report(&cause));
            }
        }
        if self.session.is_some() {
            return Ok(());
        }
        self.launch()?;
        match self.handshake() {
            Ok(()) => Ok(()),
            Err(cause) => {
                self.note_failure(&cause);
                Err(self.report(&cause))
            }
        }
    }

    /// Starts the plugin, spending the restart budget until it answers.
    ///
    /// A process that cannot be started at all is not a crash loop: it is
    /// reported where it happens rather than retried, because retrying a
    /// missing program cannot change the answer.
    fn attach(&mut self) -> Result<()> {
        loop {
            self.launch()?;
            match self.handshake() {
                Ok(()) => return Ok(()),
                Err(cause) => {
                    self.note_failure(&cause);
                    if self.disabled.is_some() {
                        return Err(self.report(&cause));
                    }
                }
            }
        }
    }

    /// Records a failure that lost the plugin process.
    ///
    /// The process is gone, so one restart is spent. A plugin that keeps
    /// failing is disabled at the limit and reported from then on, because
    /// restarting a crash loop forever is what the limit exists to prevent.
    fn note_failure(&mut self, cause: &RuneError) {
        self.session = None;
        self.restarts = self.restarts.saturating_add(1);
        if self.restarts > self.restart_limit() {
            self.disabled = Some(format!("it failed {} times: {cause}", self.restarts));
        }
    }

    /// Returns the error to report for a failure.
    ///
    /// Once the restart budget is spent, the disabling report replaces the
    /// cause: an embedder that keeps asking needs to be told the plugin is no
    /// longer used, not handed the same fault again.
    fn report(&self, cause: &RuneError) -> RuneError {
        match self.disabled.clone() {
            Some(reason) => disabled_error(&self.name, &reason),
            None => cause.clone(),
        }
    }

    /// Starts one process, when none is running.
    fn launch(&mut self) -> Result<()> {
        if self.session.is_none() {
            self.session = Some(self.spawn()?);
        }
        Ok(())
    }

    /// Completes the handshake with the running process.
    fn handshake(&mut self) -> Result<()> {
        let id = self.take_id();
        let frame = request_frame(
            id,
            "initialize",
            Some(serde_json::json!({ "protocol_version": PLUGIN_PROTOCOL_VERSION })),
        );
        let outcome = self
            .send(&frame)
            .and_then(|()| self.wait(id, self.startup_timeout()))
            .and_then(|result| check_handshake(&self.name, &result));
        if outcome.is_err() {
            self.session = None;
        }
        outcome
    }

    /// Launches the plugin process and its reader.
    fn spawn(&self) -> Result<Session> {
        let program = self.command.first().cloned().unwrap_or_default();
        let mut child = Command::new(&program)
            .args(self.command.iter().skip(1))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| spawn_error(&self.name, &program, &err))?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| {
            RuneError::new(
                ErrorCode::Internal,
                format!("plugin `{}` was started without an output pipe", self.name),
            )
        })?;
        let (sender, incoming) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("rune-plugin-{}", self.name))
            .spawn(move || read_frames(stdout, &sender))
            .map_err(|err| {
                RuneError::new(
                    ErrorCode::Internal,
                    format!("the plugin reader could not start: {err}"),
                )
            })?;

        Ok(Session {
            child,
            stdin,
            incoming,
        })
    }

    /// Writes one frame, refusing one that exceeds the frame cap.
    fn send(&mut self, frame: &str) -> Result<()> {
        if frame.len() > MAX_FRAME_BYTES {
            return Err(
                RuneError::too_large("plugin.frame", frame.len(), MAX_FRAME_BYTES)
                    .with_hint("reduce the arguments for this call"),
            );
        }
        let Some(session) = self.session.as_mut() else {
            return Err(RuneError::new(
                ErrorCode::TransportFailure,
                format!("plugin `{}` is not running", self.name),
            ));
        };
        let Some(stdin) = session.stdin.as_mut() else {
            return Err(RuneError::new(
                ErrorCode::TransportFailure,
                format!("plugin `{}` has no input left", self.name),
            ));
        };
        stdin.write_all(frame.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }

    /// Waits for the response to one request.
    fn wait(&mut self, id: u64, timeout: Duration) -> Result<serde_json::Value> {
        let started = Instant::now();
        loop {
            let remaining = timeout.saturating_sub(started.elapsed());
            let incoming = {
                let session = self.session.as_ref().ok_or_else(|| {
                    RuneError::new(
                        ErrorCode::TransportFailure,
                        format!("plugin `{}` is not running", self.name),
                    )
                })?;
                session.incoming.recv_timeout(remaining)
            };

            match incoming {
                Ok(Incoming::Frame(text)) => match parse_frame(&text)? {
                    Some((got, outcome)) if got == id => return outcome,
                    // A response to a request this host abandoned.
                    Some(_) => {}
                    None => {}
                },
                Ok(Incoming::Closed) | Err(RecvTimeoutError::Disconnected) => {
                    return Err(self.stopped_before(id));
                }
                Ok(Incoming::Failed(err)) => return Err(err),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(RuneError::new(
                        ErrorCode::Timeout,
                        format!(
                            "plugin `{}` did not answer within {} ms",
                            self.name,
                            timeout.as_millis()
                        ),
                    )
                    .with_hint("raise the plugin operation timeout, or fix the plugin"));
                }
            }
        }
    }

    /// Builds the error for a plugin that ended before answering.
    fn stopped_before(&self, id: u64) -> RuneError {
        RuneError::new(
            ErrorCode::TransportFailure,
            format!("plugin `{}` ended before answering request {id}", self.name),
        )
        .with_hint("a call is not repeated, so a side effect cannot run twice")
    }

    /// Returns the next request identifier.
    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Returns how many automatic restarts are permitted.
    ///
    /// An unlimited setting is read as none: a plugin that dies on every start
    /// would otherwise be restarted forever, which is the failure this bound
    /// exists to prevent.
    fn restart_limit(&self) -> u32 {
        self.limits
            .get(LimitName::McpRestartLimit)
            .value()
            .map_or(0, |value| u32::try_from(value).unwrap_or(u32::MAX))
    }

    /// Returns the time allowed for a start and a handshake.
    fn startup_timeout(&self) -> Duration {
        self.millis(LimitName::McpStartupTimeoutMs)
    }

    /// Returns the time allowed for one call.
    fn operation_timeout(&self) -> Duration {
        self.millis(LimitName::McpOperationTimeoutMs)
    }

    /// Returns the time a plugin is given to exit after a shutdown.
    fn grace(&self) -> Duration {
        Duration::from_millis(
            self.limits
                .get(LimitName::McpOperationTimeoutMs)
                .value()
                .unwrap_or(5_000)
                .min(5_000),
        )
    }

    /// Resolves one timeout limit.
    fn millis(&self, name: LimitName) -> Duration {
        Duration::from_millis(self.limits.get(name).value().unwrap_or(FALLBACK_TIMEOUT_MS))
    }
}

/// A plugin shared with the tools built from it.
///
/// A tool an agent advertises outlives the call that builds it, so the host is
/// shared rather than borrowed. Every tool built here reaches the same process
/// and spends the same restart budget, because one plugin is one child.
#[derive(Debug)]
pub struct SharedPlugin {
    host: Arc<Mutex<PluginHost>>,
}

impl SharedPlugin {
    /// Shares one plugin host.
    #[must_use]
    pub fn new(host: PluginHost) -> Self {
        Self {
            host: Arc::new(Mutex::new(host)),
        }
    }

    /// Returns the tools to advertise, one per tool the plugin declares.
    #[must_use]
    pub fn host_tools(&self) -> Vec<HostTool> {
        self.list_tools()
            .into_iter()
            .map(|tool| {
                let host = Arc::clone(&self.host);
                let name = tool.name.clone();
                HostTool::new(
                    tool.name,
                    tool.description,
                    tool.input_schema,
                    move |arguments, context| {
                        context.signal.check()?;
                        let output = borrow(&host)?.call(&name, arguments)?;
                        Ok(if output.is_error {
                            HostToolResult::failure(output.text)
                        } else {
                            HostToolResult::text(output.text)
                        })
                    },
                )
            })
            .collect()
    }

    /// Returns the tools the plugin declares.
    #[must_use]
    pub fn list_tools(&self) -> Vec<PluginTool> {
        borrow(&self.host).map_or_else(|_| Vec::new(), |host| host.list_tools())
    }

    /// Calls one tool after the host authorizes it.
    pub fn call_with_authorization(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        authorize: &dyn Fn(&str, &serde_json::Value) -> bool,
    ) -> Result<ToolOutput> {
        borrow(&self.host)?.call_with_authorization(name, arguments, authorize)
    }

    /// Shuts the plugin down.
    pub fn shutdown(&self) -> Result<()> {
        borrow(&self.host)?.shutdown()
    }

    /// Returns true once the plugin has been disabled.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        borrow(&self.host).is_ok_and(|host| host.is_disabled())
    }

    /// Returns why the plugin was disabled.
    #[must_use]
    pub fn disabled_reason(&self) -> Option<String> {
        borrow(&self.host)
            .ok()
            .and_then(|host| host.disabled_reason().map(str::to_owned))
    }
}

/// Borrows the shared host, refusing a poisoned lock rather than ignoring it.
///
/// A panic while a call was in flight leaves the stream in an unknown state, so
/// the plugin is not reused: continuing would match a response to the wrong
/// request.
fn borrow(host: &Arc<Mutex<PluginHost>>) -> Result<MutexGuard<'_, PluginHost>> {
    host.lock().map_err(|_| {
        RuneError::new(
            ErrorCode::Internal,
            "the plugin host was left unusable by a panic during a call",
        )
        .with_hint("build a new plugin host")
    })
}

/// Builds the error for a process that could not be started.
///
/// A missing program is reported as such rather than as a transport failure:
/// the two want different repairs, and only one of them is worth retrying.
fn spawn_error(name: &str, program: &str, err: &std::io::Error) -> RuneError {
    let code = if err.kind() == std::io::ErrorKind::NotFound {
        ErrorCode::NotFound
    } else {
        ErrorCode::TransportFailure
    };
    RuneError::new(
        code,
        format!("plugin `{name}` could not start `{program}`: {err}"),
    )
    .with_hint("check that the program the manifest names exists and is executable")
}

/// Refuses a frame larger than the cap, in either direction.
fn check_frame(frame: &str) -> Result<()> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(
            RuneError::too_large("plugin.frame", frame.len(), MAX_FRAME_BYTES)
                .with_hint("reduce the arguments for this call"),
        );
    }
    Ok(())
}

/// Builds the error for a plugin that is no longer used.
fn disabled_error(name: &str, reason: &str) -> RuneError {
    RuneError::new(
        ErrorCode::LimitExceeded,
        format!("plugin `{name}` is disabled: {reason}"),
    )
    .with_hint("build a new host to start the plugin again")
}

/// Checks the version a plugin answers a handshake with.
fn check_handshake(name: &str, result: &serde_json::Value) -> Result<()> {
    let Some(declared) = result
        .get("protocol_version")
        .and_then(serde_json::Value::as_u64)
    else {
        return Err(RuneError::new(
            ErrorCode::ProtocolViolation,
            format!("plugin `{name}` answered the handshake without a protocol version"),
        ));
    };
    if declared != u64::from(PLUGIN_PROTOCOL_VERSION) {
        return Err(incompatible_version(
            name,
            u32::try_from(declared).unwrap_or(u32::MAX),
        ));
    }
    Ok(())
}

/// Renders a request frame, without its trailing newline.
fn request_frame(id: u64, method: &str, params: Option<serde_json::Value>) -> String {
    let mut frame = serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method });
    if let Some(params) = params
        && let Some(object) = frame.as_object_mut()
    {
        object.insert("params".to_owned(), params);
    }
    frame.to_string()
}

/// Renders a notification frame, without its trailing newline.
fn notification_frame(method: &str) -> String {
    serde_json::json!({ "jsonrpc": "2.0", "method": method }).to_string()
}

/// Reduces one frame to the response it carries.
///
/// Returns `None` for a frame this host does not answer: a plugin may emit
/// notifications, log to its own output, or send a request, and none of those
/// is a fault. A frame that claims to be a response but is malformed is one.
fn parse_frame(text: &str) -> Result<Option<(u64, Result<serde_json::Value>)>> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Ok(None);
    };
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    if object.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
        return Ok(None);
    }
    // A frame that names a method is a request or a notification. This host
    // serves neither, so one is read and discarded.
    if object.contains_key("method") {
        return Ok(None);
    }
    let Some(id) = object.get("id").and_then(serde_json::Value::as_u64) else {
        return Ok(None);
    };

    if let Some(error) = object.get("error") {
        let raw = error
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("the plugin reported an error without a message");
        let code = match raw {
            -32601 => ErrorCode::Unsupported,
            -32600 | -32602 => ErrorCode::InvalidField,
            _ => ErrorCode::RequestRejected,
        };
        return Ok(Some((id, Err(RuneError::new(code, message.to_owned())))));
    }

    match object.get("result") {
        Some(result) => Ok(Some((id, Ok(result.clone())))),
        None => Err(RuneError::new(
            ErrorCode::ProtocolViolation,
            format!("the response to request {id} carries neither a result nor an error"),
        )),
    }
}

/// Reads frames from the plugin's output until it ends.
fn read_frames(stdout: ChildStdout, sender: &Sender<Incoming>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_frame(&mut reader, MAX_FRAME_BYTES) {
            Ok(Some(frame)) => {
                if sender.send(Incoming::Frame(frame)).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(Incoming::Closed);
                return;
            }
            Err(err) => {
                let _ = sender.send(Incoming::Failed(err));
                return;
            }
        }
    }
}

/// Reads one frame, returning `None` at a clean end of input.
///
/// Blank lines are skipped. Input that ends inside a frame is an error rather
/// than a frame, because a truncated message must never be read as a complete
/// one, and a frame larger than the cap is refused where the bytes arrive.
fn read_frame<R: BufRead>(reader: &mut R, cap: usize) -> Result<Option<String>> {
    loop {
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if buffer.is_empty() {
                    return Ok(None);
                }
                return Err(RuneError::new(
                    ErrorCode::IncompleteStream,
                    "the plugin closed its output inside a frame",
                ));
            }
            let (chunk, consumed, terminated) =
                match available.iter().position(|byte| *byte == b'\n') {
                    Some(index) => (&available[..index], index.saturating_add(1), true),
                    None => (available, available.len(), false),
                };
            if buffer.len().saturating_add(chunk.len()) > cap {
                let observed = buffer.len().saturating_add(chunk.len());
                reader.consume(consumed);
                return Err(RuneError::too_large("plugin.frame", observed, cap).with_hint(
                    "the plugin sent one frame larger than the frame cap; the stream cannot be resynchronised after it",
                ));
            }
            buffer.extend_from_slice(chunk);
            reader.consume(consumed);
            if terminated {
                break;
            }
        }
        if buffer.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(&buffer).map_err(|err| {
            RuneError::new(
                ErrorCode::ProtocolViolation,
                format!("a plugin frame is not valid UTF-8: {err}"),
            )
        })?;
        return Ok(Some(text.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn a_frame_larger_than_the_cap_is_refused() {
        let mut body = String::from("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"");
        body.push_str(&"a".repeat(MAX_FRAME_BYTES));
        body.push_str("\"}\n");
        let mut reader = BufReader::new(Cursor::new(body.into_bytes()));
        let err = read_frame(&mut reader, MAX_FRAME_BYTES).expect_err("oversized frame");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("plugin.frame"));
    }

    #[test]
    fn a_manifest_version_this_build_cannot_speak_is_named_in_full() {
        let manifest = PluginManifest {
            protocol_version: 7,
            name: "legacy".to_owned(),
            tools: Vec::new(),
            commands: vec!["/bin/true".to_owned()],
        };
        let err = manifest.validate().expect_err("unsupported version");
        assert_eq!(err.code(), ErrorCode::UnsupportedVersion);
        assert!(err.message().contains('7'), "{}", err.message());
        assert!(err.message().contains("version 1"), "{}", err.message());
    }
}
