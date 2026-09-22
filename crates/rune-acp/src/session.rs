//! Session state for one connection.
//!
//! A session is one durable conversation: an event log on disk plus the history
//! rebuilt from it. The log is the authority, so closing a session and loading it
//! again reaches the same conversation, and a client that reconnects can be sent
//! the whole transcript again as updates.
//!
//! Every session is rooted at the workspace the server was started in. One server
//! process serves one primary workspace, so a session cannot silently operate
//! somewhere else.

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use rune_agent::history::History;
use rune_agent::steering::SteeringQueue;
use rune_core::budget::{BudgetSet, LimitName};
use rune_core::config::{Effort, PermissionMode};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::{SessionId, ToolCallId};
use rune_core::paths::Paths;
use rune_net::message::{ContentPart, Role};
use rune_policy::decision::Layer;
use rune_policy::rules::{Rule, RuleSet};
use rune_session::event::{EventFrame, SessionEvent};
use rune_session::store::{SessionStore, load_read_only};
use rune_tools::contract::ExecutionContext;
use serde_json::{Value, json};

use crate::jsonrpc::Notification;

/// Longest accepted model identifier.
pub const MAX_MODEL_BYTES: usize = 256;

/// Everything about a session a client can change.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionConfig {
    /// Model identifier used for the next turn.
    pub model: String,
    /// Reasoning effort requested from the model.
    pub effort: Effort,
    /// Permission mode in force.
    pub mode: PermissionMode,
}

impl SessionConfig {
    /// Builds a configuration.
    #[must_use]
    pub fn new(model: impl Into<String>, effort: Effort, mode: PermissionMode) -> Self {
        Self {
            model: model.into(),
            effort,
            mode,
        }
    }
}

/// What `session/list` reports about one stored session.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionSummary {
    /// Session identifier.
    pub id: String,
    /// Working directory the session belongs to.
    pub cwd: String,
    /// Display title, when one was set.
    pub title: Option<String>,
    /// ISO 8601 timestamp of the last recorded event.
    pub updated_at: Option<String>,
}

/// One active session.
#[derive(Debug)]
pub struct Session {
    id: String,
    config: SessionConfig,
    history: History,
    /// Rules granted by an approval for this session only.
    grants: RuleSet,
    /// Additional roots this session's tools may reach.
    additional_roots: Vec<Utf8PathBuf>,
    store: SessionStore,
}

impl Session {
    /// Returns the configuration in force.
    #[must_use]
    pub const fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Returns the directories this session's tools may reach beyond the
    /// workspace.
    #[must_use]
    pub fn additional_roots(&self) -> &[Utf8PathBuf] {
        &self.additional_roots
    }
}

/// Everything a turn needs, taken before the turn starts.
///
/// Holding a snapshot rather than the session is what lets a turn run while the
/// connection keeps answering: the session map is free to serve another request
/// while the model is streaming.
#[derive(Debug)]
pub struct Snapshot {
    /// A copy of the conversation, replaced once the turn finishes.
    pub history: History,
    /// Configuration in force at the start of the turn.
    pub config: SessionConfig,
    /// Rules, including approvals granted earlier in the session.
    pub rules: RuleSet,
    /// Execution context for the turn's tool calls.
    pub context: ExecutionContext,
    /// Steering queue for the turn.
    pub steering: SteeringQueue,
}

/// The sessions held by one connection.
#[derive(Debug)]
pub struct Sessions {
    entries: BTreeMap<String, Session>,
    paths: Paths,
    workspace: Utf8PathBuf,
    defaults: SessionConfig,
    /// Directories every new session may reach.
    default_roots: Vec<Utf8PathBuf>,
    limits: BudgetSet,
    cap: usize,
}

impl Sessions {
    /// Builds an empty session map.
    #[must_use]
    pub fn new(
        paths: Paths,
        workspace: Utf8PathBuf,
        defaults: SessionConfig,
        limits: &BudgetSet,
        default_roots: Vec<Utf8PathBuf>,
    ) -> Self {
        let cap = limits.get_usize(LimitName::ListEntries).max(1);
        Self {
            entries: BTreeMap::new(),
            paths,
            workspace,
            defaults,
            default_roots,
            limits: limits.clone(),
            cap,
        }
    }

    /// Returns the largest number of sessions this connection may hold.
    #[must_use]
    pub const fn cap(&self) -> usize {
        self.cap
    }

    /// Creates a session and returns its identifier.
    ///
    /// Fails once the map holds the configured bound rather than growing without
    /// limit, because a client that opens sessions in a loop would otherwise hold
    /// one writer lock per iteration.
    pub fn create(&mut self, additional_roots: Vec<Utf8PathBuf>) -> Result<String> {
        // Configured directories come first, so a session reaches what the user
        // saved even when the client asks for none of its own. A root the client
        // names as well is kept once, since the context would otherwise hold the
        // same directory twice.
        let mut roots = self.default_roots.clone();
        for root in additional_roots {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        let additional_roots = roots;
        if self.entries.len() >= self.cap {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                format!(
                    "this connection holds {} sessions, the limit is {}",
                    self.entries.len(),
                    self.cap
                ),
            )
            .with_hint("close a session before opening another"));
        }
        // A collision on a generated identifier is possible, so creation retries
        // within a small bound instead of failing the request.
        let mut last: Option<RuneError> = None;
        let mut attempts: Vec<String> = Vec::new();
        for _ in 0..8_u8 {
            let id = SessionId::generate();
            if self.entries.contains_key(id.as_str()) {
                attempts.push(format!("{id} collided"));
                continue;
            }
            match SessionStore::create(&self.paths, &id) {
                Ok(store) => {
                    let name = id.to_string();
                    self.entries.insert(
                        name.clone(),
                        Session {
                            id: name.clone(),
                            config: self.defaults.clone(),
                            history: History::new(),
                            grants: RuleSet::new(),
                            additional_roots,
                            store,
                        },
                    );
                    return Ok(name);
                }
                Err(err) => {
                    attempts.push(format!("{id}: {}", err.message()));
                    last = Some(err);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            RuneError::new(
                ErrorCode::AlreadyExists,
                "no unused session identifier was available",
            )
            .with_hint("remove stale session directories")
            .with_observed(attempts.join("; "))
        }))
    }

    /// Loads a saved session, returning the updates that replay its history.
    ///
    /// Replaying is the difference between loading and resuming: a client that
    /// loads a conversation shows the transcript it missed, and a client that
    /// resumes already has one on screen.
    pub fn load(&mut self, id: &str) -> Result<Vec<Notification>> {
        self.open(id)?;
        let session = self.get(id)?;
        Ok(replay_updates(&session.id, &session.history))
    }

    /// Reconnects to a saved session without replaying its history.
    pub fn resume(&mut self, id: &str) -> Result<()> {
        self.open(id)?;
        Ok(())
    }

    /// Opens a stored session, replacing any entry already held for it.
    ///
    /// The previous entry holds the writer lock for the same directory, so it is
    /// dropped before the store is opened again.
    fn open(&mut self, id: &str) -> Result<()> {
        let key: SessionId = id.parse()?;
        self.entries.remove(key.as_str());
        let store = SessionStore::open(&self.paths.session_dir(&key))?;
        let state = store.read()?;
        let history = history_from_events(&state.events);
        history.validate().map_err(|err| {
            RuneError::invariant(
                "session_history",
                format!("session `{key}` does not replay into a valid conversation: {err}"),
            )
            .with_hint("recover the session into a new one")
        })?;
        let name = key.to_string();
        self.entries.insert(
            name.clone(),
            Session {
                id: name,
                config: self.defaults.clone(),
                history,
                grants: RuleSet::new(),
                additional_roots: Vec::new(),
                store,
            },
        );
        Ok(())
    }

    /// Lists stored sessions for this workspace, most recently active first.
    ///
    /// Bounded by the configured listing bound, so a large session directory
    /// costs a bounded read.
    #[must_use]
    pub fn list(&self) -> Vec<SessionSummary> {
        let Ok(entries) = std::fs::read_dir(self.paths.sessions_dir()) else {
            return Vec::new();
        };
        let mut out: Vec<SessionSummary> = Vec::new();
        for entry in entries.flatten() {
            if out.len() >= self.cap {
                break;
            }
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
                continue;
            };
            let Ok(state) = load_read_only(&path) else {
                continue;
            };
            out.push(SessionSummary {
                id: state.id.to_string(),
                cwd: self.workspace.to_string(),
                title: state.title,
                updated_at: state
                    .events
                    .last()
                    .and_then(|frame| format_timestamp(frame.timestamp_ms)),
            });
        }
        out.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        out.truncate(self.cap);
        out
    }

    /// Closes a session, releasing its writer lock.
    pub fn close(&mut self, id: &str) -> Result<()> {
        let key = Self::key(id)?;
        if self.entries.remove(&key).is_none() {
            return Err(unknown_session(id));
        }
        Ok(())
    }

    /// Returns one session.
    pub fn get(&self, id: &str) -> Result<&Session> {
        let key = Self::key(id)?;
        self.entries.get(&key).ok_or_else(|| unknown_session(id))
    }

    /// Resolves a client-supplied identifier to the key sessions are stored
    /// under, so a malformed one is reported as a bad field rather than as a
    /// session that does not exist.
    fn key(id: &str) -> Result<String> {
        let parsed: SessionId = id.parse()?;
        Ok(parsed.to_string())
    }

    /// Changes a configuration option.
    pub fn set_config_option(&mut self, id: &str, option: &str, value: &str) -> Result<()> {
        let session = self
            .entries
            .get_mut(id)
            .ok_or_else(|| unknown_session(id))?;
        match option {
            "model" => {
                let model = value.trim();
                if model.is_empty() {
                    return Err(RuneError::invalid_field("model", "must not be empty")
                        .with_hint("pass a model identifier"));
                }
                if model.len() > MAX_MODEL_BYTES {
                    return Err(RuneError::too_large("model", model.len(), MAX_MODEL_BYTES));
                }
                model.clone_into(&mut session.config.model);
                Ok(())
            }
            "effort" => {
                let effort = parse_effort(value).ok_or_else(|| {
                    RuneError::invalid_field(
                        "effort",
                        format!("`{value}` is not a reasoning effort"),
                    )
                    .with_hint("use auto, none, minimal, low, medium, high, xhigh, or max")
                })?;
                session.config.effort = effort;
                Ok(())
            }
            other => Err(RuneError::new(
                ErrorCode::NotFound,
                format!("no configuration option named `{other}`"),
            )
            .with_hint("this server offers `model` and `effort`")),
        }
    }

    /// Changes the permission mode.
    pub fn set_mode(&mut self, id: &str, mode: &str) -> Result<()> {
        let session = self
            .entries
            .get_mut(id)
            .ok_or_else(|| unknown_session(id))?;
        let mode = PermissionMode::from_name(mode).ok_or_else(|| {
            RuneError::invalid_field("modeId", format!("`{mode}` is not a permission mode"))
                .with_hint("use ask, auto, or full_access")
        })?;
        session.config.mode = mode;
        Ok(())
    }

    /// Takes everything a turn needs.
    pub fn snapshot(&self, id: &str) -> Result<Snapshot> {
        let key = Self::key(id)?;
        let Some(session) = self.entries.get(&key) else {
            return Err(unknown_session(id));
        };
        let mut context = ExecutionContext::new(self.workspace.clone())
            .with_output_cap(self.limits.get_usize(LimitName::MaxToolResultBytes));
        for root in &session.additional_roots {
            context = context.with_root(root.clone());
        }
        Ok(Snapshot {
            history: session.history.clone(),
            config: session.config.clone(),
            rules: session.grants.clone(),
            context,
            steering: SteeringQueue::from_limits(&self.limits),
        })
    }

    /// Replaces a session's conversation and approvals with what a turn produced.
    pub fn absorb(&mut self, id: &str, history: History, rules: RuleSet) -> Result<()> {
        let key = Self::key(id)?;
        let session = self
            .entries
            .get_mut(&key)
            .ok_or_else(|| unknown_session(id))?;
        session.history = history;
        session.grants = rules;
        Ok(())
    }

    /// Appends one event to a session's log.
    pub fn record(&mut self, id: &str, event: SessionEvent) -> Result<()> {
        let session = self
            .entries
            .get_mut(id)
            .ok_or_else(|| unknown_session(id))?;
        session.store.append(event)?;
        Ok(())
    }
}

/// Builds the error for a session this connection does not hold.
#[must_use]
pub fn unknown_session(id: &str) -> RuneError {
    RuneError::new(ErrorCode::NotFound, format!("no session named `{id}`"))
        .with_hint("create one with session/new, or list the stored ones with session/list")
        .with_observed(id.to_owned())
}

/// Parses a reasoning effort written by a client.
#[must_use]
pub fn parse_effort(raw: &str) -> Option<Effort> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(Effort::Auto),
        "none" => Some(Effort::None),
        "minimal" => Some(Effort::Minimal),
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" => Some(Effort::Xhigh),
        "max" => Some(Effort::Max),
        _ => None,
    }
}

/// Returns the tool call kind reported for a tool.
#[must_use]
pub fn kind_for_name(name: &str) -> &'static str {
    match name {
        "read_file" => "read",
        "glob_files" | "grep_files" => "search",
        "write_file" | "edit_file" => "edit",
        "shell" => "execute",
        "web_fetch" | "web_search" => "fetch",
        _ => "other",
    }
}

/// Returns the human title reported for a tool call.
#[must_use]
pub fn tool_title(name: &str, target: Option<&str>) -> String {
    let verb = match kind_for_name(name) {
        "read" => "Reading",
        "search" => "Searching",
        "edit" => "Editing",
        "execute" => "Running",
        "fetch" => "Fetching",
        _ => "Running",
    };
    match target {
        Some(target) => format!("{verb} {target}"),
        None => format!("{verb} {name}"),
    }
}

/// Builds a text content block.
#[must_use]
pub fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// Builds a message chunk notification.
#[must_use]
pub fn message_chunk(session: &str, kind: &str, text: &str) -> Notification {
    Notification::new(
        "session/update",
        json!({
            "sessionId": session,
            "update": { "sessionUpdate": kind, "content": text_block(text) },
        }),
    )
}

/// Builds a tool call notification.
#[must_use]
pub fn tool_call(
    session: &str,
    id: &str,
    name: &str,
    target: Option<&str>,
    status: &str,
    raw_input: &Value,
) -> Notification {
    Notification::new(
        "session/update",
        json!({
            "sessionId": session,
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "title": tool_title(name, target),
                "name": name,
                "kind": kind_for_name(name),
                "status": status,
                "rawInput": raw_input,
            },
        }),
    )
}

/// Builds a tool call update notification.
#[must_use]
pub fn tool_call_update(
    session: &str,
    id: &str,
    status: &str,
    output: Option<&str>,
) -> Notification {
    let mut update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        "status": status,
    });
    if let Some(output) = output {
        update["content"] = json!([{ "type": "content", "content": text_block(output) }]);
    }
    Notification::new(
        "session/update",
        json!({ "sessionId": session, "update": update }),
    )
}

/// Builds a context usage notification.
#[must_use]
pub fn usage_update(session: &str, used: u64, size: u64) -> Notification {
    Notification::new(
        "session/update",
        json!({
            "sessionId": session,
            "update": { "sessionUpdate": "usage_update", "used": used, "size": size },
        }),
    )
}

/// Builds the updates that replay a stored conversation.
///
/// The sequence is the conversation itself: what the user said, what the model
/// answered, and every tool call followed by the result that answered it.
#[must_use]
pub fn replay_updates(session: &str, history: &History) -> Vec<Notification> {
    let mut out = Vec::new();
    for turn in history.turns() {
        for part in &turn.parts {
            match part {
                ContentPart::Text { text } => {
                    let kind = if turn.role == Role::User {
                        "user_message_chunk"
                    } else {
                        "agent_message_chunk"
                    };
                    out.push(message_chunk(session, kind, text));
                }
                ContentPart::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    let raw = serde_json::from_str(arguments).unwrap_or(Value::Null);
                    out.push(tool_call(
                        session,
                        id.as_str(),
                        name,
                        None,
                        "in_progress",
                        &raw,
                    ));
                }
                ContentPart::ToolResult {
                    id,
                    content,
                    is_error,
                    ..
                } => {
                    let status = if *is_error { "failed" } else { "completed" };
                    out.push(tool_call_update(
                        session,
                        id.as_str(),
                        status,
                        Some(content),
                    ));
                }
                ContentPart::Reasoning { .. } | ContentPart::Image { .. } => {}
            }
        }
    }
    out
}

/// Rebuilds a conversation from a stored event log.
#[must_use]
pub fn history_from_events(frames: &[EventFrame]) -> History {
    let mut history = History::new();
    let mut calls: Vec<ContentPart> = Vec::new();
    let mut results: Vec<ContentPart> = Vec::new();

    for frame in frames {
        match &frame.event {
            SessionEvent::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                // A result run always follows the calls it answers, so a new
                // call closes the previous result group.
                flush_results(&mut history, &mut results);
                if let Ok(id) = ToolCallId::new(call_id.clone()) {
                    calls.push(ContentPart::ToolCall {
                        id,
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                }
            }
            SessionEvent::ToolResult {
                call_id,
                ok,
                output,
            } => {
                flush_calls(&mut history, &mut calls);
                if let Ok(id) = ToolCallId::new(call_id.clone()) {
                    results.push(ContentPart::ToolResult {
                        id,
                        name: String::new(),
                        content: output.clone(),
                        is_error: !ok,
                    });
                }
            }
            SessionEvent::UserMessage { text } => {
                flush_calls(&mut history, &mut calls);
                flush_results(&mut history, &mut results);
                history.push_user(text.clone());
            }
            SessionEvent::AssistantMessage { text, .. } => {
                flush_calls(&mut history, &mut calls);
                flush_results(&mut history, &mut results);
                if !text.is_empty() {
                    history.push_assistant(vec![ContentPart::Text { text: text.clone() }]);
                }
            }
            SessionEvent::TurnStarted { .. }
            | SessionEvent::Compaction { .. }
            | SessionEvent::UsageRecorded { .. }
            | SessionEvent::TitleSet { .. }
            | SessionEvent::WorkspaceSet { .. }
            | SessionEvent::ChildOf { .. } => {
                flush_calls(&mut history, &mut calls);
                flush_results(&mut history, &mut results);
            }
        }
    }
    flush_calls(&mut history, &mut calls);
    flush_results(&mut history, &mut results);
    history
}

/// Appends the collected assistant turn, when there is one.
fn flush_calls(history: &mut History, calls: &mut Vec<ContentPart>) {
    if calls.is_empty() {
        return;
    }
    history.push_assistant(std::mem::take(calls));
}

/// Appends the collected tool results, when there are any.
fn flush_results(history: &mut History, results: &mut Vec<ContentPart>) {
    if results.is_empty() {
        return;
    }
    history.push_tool_results(std::mem::take(results));
}

/// Formats milliseconds since the epoch as an ISO 8601 timestamp.
#[must_use]
pub fn format_timestamp(millis: u64) -> Option<String> {
    let millis = i64::try_from(millis).ok()?;
    jiff::Timestamp::from_millisecond(millis)
        .ok()
        .map(|at| at.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Adds a session approval for a tool and target.
pub fn grant(rules: &mut RuleSet, tool: &str, target: Option<&str>) {
    let pattern = target.unwrap_or("*");
    let already = rules
        .rules()
        .iter()
        .any(|rule| rule.tool == tool && rule.pattern == pattern);
    if !already {
        rules.push(Rule::allow(tool, pattern, Layer::Session));
    }
}

/// Renders the session modes a client may choose.
#[must_use]
pub fn modes(config: &SessionConfig) -> Value {
    json!({
        "availableModes": [
            {"id": "ask", "name": "Ask", "description": "Request approval for unresolved tool calls."},
            {"id": "auto", "name": "Auto", "description": "Review unresolved tool calls automatically."},
            {"id": "full_access", "name": "Full access", "description": "Run every tool call without approval."},
        ],
        "currentModeId": config.mode.as_str(),
    })
}

/// Renders the configuration options a client may change.
#[must_use]
pub fn config_options(config: &SessionConfig) -> Value {
    let efforts: Vec<Value> = [
        Effort::Auto,
        Effort::None,
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::Xhigh,
        Effort::Max,
    ]
    .into_iter()
    .map(|effort| json!({"value": effort.as_str(), "name": effort_label(effort)}))
    .collect();

    json!([
        {
            "id": "model",
            "name": "Model",
            "description": "Model used for the next turn.",
            "category": "model",
            "type": "select",
            "currentValue": config.model,
            "options": [{"value": config.model, "name": config.model}],
        },
        {
            "id": "effort",
            "name": "Reasoning effort",
            "category": "thought_level",
            "type": "select",
            "currentValue": config.effort.as_str(),
            "options": efforts,
        },
    ])
}

/// Returns the label for a reasoning effort.
fn effort_label(effort: Effort) -> &'static str {
    match effort {
        Effort::Auto => "Auto",
        Effort::None => "None",
        Effort::Minimal => "Minimal",
        Effort::Low => "Low",
        Effort::Medium => "Medium",
        Effort::High => "High",
        Effort::Xhigh => "Very high",
        Effort::Max => "Maximum",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune_core::config::Layer as ConfigLayer;
    use rune_core::id::EventSeq;
    use rune_session::event::EventFrame;

    /// Returns a path inside a temporary directory as a UTF-8 path.
    fn utf8_path(root: &tempfile::TempDir, name: &str) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(root.path().join(name)).expect("utf8 path")
    }

    fn paths_for(root: &tempfile::TempDir) -> Paths {
        let base =
            |name: &str| Utf8PathBuf::from_path_buf(root.path().join(name)).expect("utf8 path");
        Paths {
            config_root: base("config"),
            state_root: base("state"),
            data_root: base("data"),
        }
    }

    fn sessions(root: &tempfile::TempDir) -> Sessions {
        let limits = BudgetSet::new();
        Sessions::new(
            paths_for(root),
            Utf8PathBuf::from("/tmp/work"),
            SessionConfig::new("test/model", Effort::Auto, PermissionMode::Auto),
            &limits,
            Vec::new(),
        )
    }

    #[test]
    fn a_new_session_reaches_the_configured_directories() {
        let root = tempfile::tempdir().expect("temp");
        let configured = utf8_path(&root, "shared");
        std::fs::create_dir(&configured).expect("create");
        let mut sessions = Sessions::new(
            paths_for(&root),
            utf8_path(&root, "work"),
            SessionConfig::new("m", Effort::default(), PermissionMode::default()),
            &BudgetSet::new(),
            vec![configured.clone()],
        );

        let id = sessions.create(Vec::new()).expect("created");
        let session = sessions.get(&id).expect("present");
        assert_eq!(session.additional_roots(), [configured]);
    }

    #[test]
    fn a_root_the_client_names_as_well_is_kept_once() {
        let root = tempfile::tempdir().expect("temp");
        let configured = utf8_path(&root, "shared");
        std::fs::create_dir(&configured).expect("create");
        let mut sessions = Sessions::new(
            paths_for(&root),
            utf8_path(&root, "work"),
            SessionConfig::new("m", Effort::default(), PermissionMode::default()),
            &BudgetSet::new(),
            vec![configured.clone()],
        );

        let id = sessions.create(vec![configured.clone()]).expect("created");
        let session = sessions.get(&id).expect("present");
        // The same directory twice would put it in the context twice, which a
        // resolve would then report as two roots.
        assert_eq!(session.additional_roots().len(), 1);
    }

    #[test]
    fn a_client_root_is_added_alongside_the_configured_ones() {
        let root = tempfile::tempdir().expect("temp");
        let configured = utf8_path(&root, "shared");
        let requested = utf8_path(&root, "extra");
        std::fs::create_dir(&configured).expect("create");
        std::fs::create_dir(&requested).expect("create");
        let mut sessions = Sessions::new(
            paths_for(&root),
            utf8_path(&root, "work"),
            SessionConfig::new("m", Effort::default(), PermissionMode::default()),
            &BudgetSet::new(),
            vec![configured.clone()],
        );

        let id = sessions.create(vec![requested.clone()]).expect("created");
        let session = sessions.get(&id).expect("present");
        assert_eq!(session.additional_roots().len(), 2);
        assert!(session.additional_roots().contains(&requested));
    }

    #[test]
    fn a_new_session_is_held_and_can_be_snapshotted() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut sessions = sessions(&root);
        let id = sessions.create(Vec::new()).expect("create");
        assert!(sessions.get(&id).is_ok());
        let snapshot = sessions.snapshot(&id).expect("snapshot");
        assert!(snapshot.history.is_empty());
        assert_eq!(snapshot.config.model, "test/model");
    }

    #[test]
    fn an_unknown_session_is_a_not_found_error() {
        let root = tempfile::tempdir().expect("tempdir");
        let sessions = sessions(&root);
        let error = sessions.snapshot("aaaaaaaaaaaa").expect_err("missing");
        assert_eq!(error.code(), ErrorCode::NotFound);
    }

    #[test]
    fn a_malformed_session_identifier_is_rejected() {
        let root = tempfile::tempdir().expect("tempdir");
        let sessions = sessions(&root);
        let error = sessions.snapshot("not-an-id").expect_err("rejected");
        assert_eq!(error.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn closing_a_session_releases_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut sessions = sessions(&root);
        let id = sessions.create(Vec::new()).expect("create");
        sessions.close(&id).expect("close");
        assert!(sessions.get(&id).is_err());
        assert!(sessions.close(&id).is_err());
    }

    #[test]
    fn closing_releases_the_writer_lock_so_it_can_be_loaded_again() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut sessions = sessions(&root);
        let id = sessions.create(Vec::new()).expect("create");
        sessions
            .record(
                &id,
                SessionEvent::UserMessage {
                    text: "hi".to_owned(),
                },
            )
            .expect("record");
        sessions.close(&id).expect("close");
        sessions.load(&id).expect("load");
        let session = sessions.get(&id).expect("held");
        assert_eq!(session.history.turns().len(), 1);
    }

    #[test]
    fn loading_replays_history_while_resuming_does_not() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut sessions = sessions(&root);
        let id = sessions.create(Vec::new()).expect("create");
        sessions
            .record(
                &id,
                SessionEvent::UserMessage {
                    text: "hello".to_owned(),
                },
            )
            .expect("record");
        sessions
            .record(
                &id,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    text: "hi there".to_owned(),
                },
            )
            .expect("record");
        sessions.close(&id).expect("close");

        let updates = sessions.load(&id).expect("load");
        assert_eq!(updates.len(), 2);
        let first = updates[0].params["update"]["content"]["text"].clone();
        assert_eq!(first, "hello");
        assert_eq!(
            updates[0].params["update"]["sessionUpdate"],
            "user_message_chunk"
        );
        assert_eq!(
            updates[1].params["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        sessions.close(&id).expect("close");

        sessions.resume(&id).expect("resume");
        let session = sessions.get(&id).expect("held");
        assert_eq!(session.history.turns().len(), 2);
    }

    #[test]
    fn a_tool_call_replays_as_a_call_and_its_result() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut sessions = sessions(&root);
        let id = sessions.create(Vec::new()).expect("create");
        sessions
            .record(
                &id,
                SessionEvent::ToolCall {
                    call_id: "c1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{\"path\":\"a.rs\"}".to_owned(),
                },
            )
            .expect("record");
        sessions
            .record(
                &id,
                SessionEvent::ToolResult {
                    call_id: "c1".to_owned(),
                    ok: true,
                    output: "contents".to_owned(),
                },
            )
            .expect("record");
        sessions.close(&id).expect("close");

        let updates = sessions.load(&id).expect("load");
        let kinds: Vec<String> = updates
            .iter()
            .map(|update| update.params["update"]["sessionUpdate"].to_string())
            .collect();
        assert_eq!(kinds, vec!["\"tool_call\"", "\"tool_call_update\""]);
        assert_eq!(updates[0].params["update"]["kind"], "read");
        assert_eq!(updates[1].params["update"]["status"], "completed");
    }

    #[test]
    fn a_config_option_changes_only_the_named_field() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut sessions = sessions(&root);
        let id = sessions.create(Vec::new()).expect("create");
        sessions
            .set_config_option(&id, "model", "other/model")
            .expect("model");
        sessions
            .set_config_option(&id, "effort", "high")
            .expect("effort");
        let session = sessions.get(&id).expect("held");
        assert_eq!(session.config.model, "other/model");
        assert_eq!(session.config.effort, Effort::High);
        assert_eq!(session.config.mode, PermissionMode::Auto);
    }

    #[test]
    fn an_unknown_config_option_is_reported() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut sessions = sessions(&root);
        let id = sessions.create(Vec::new()).expect("create");
        let error = sessions
            .set_config_option(&id, "temperature", "1")
            .expect_err("rejected");
        assert_eq!(error.code(), ErrorCode::NotFound);
    }

    #[test]
    fn a_mode_change_is_applied() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut sessions = sessions(&root);
        let id = sessions.create(Vec::new()).expect("create");
        sessions.set_mode(&id, "full-access").expect("mode");
        let session = sessions.get(&id).expect("held");
        assert_eq!(session.config.mode, PermissionMode::FullAccess);
        assert!(sessions.set_mode(&id, "bogus").is_err());
    }

    #[test]
    fn the_session_map_is_bounded() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut limits = BudgetSet::new();
        limits
            .set(
                LimitName::ListEntries,
                rune_core::budget::Budget::Bounded(2),
                ConfigLayer::User,
            )
            .expect("set");
        let mut sessions = Sessions::new(
            paths_for(&root),
            utf8_path(&root, "work"),
            SessionConfig::new("test/model", Effort::Auto, PermissionMode::Auto),
            &limits,
            Vec::new(),
        );
        assert_eq!(sessions.cap(), 2);
        sessions.create(Vec::new()).expect("first");
        // The store refuses a session directory it cannot create, so the
        // failure is reported with what the store said rather than with the
        // identifier-collision message that follows it.
        if let Err(err) = sessions.create(Vec::new()) {
            panic!("second: {err} (code {:?})", err.code());
        }
        let error = sessions.create(Vec::new()).expect_err("third");
        assert_eq!(error.code(), ErrorCode::LimitExceeded);
    }

    #[test]
    fn a_grant_is_recorded_once_per_target() {
        let mut rules = RuleSet::new();
        grant(&mut rules, "shell", Some("ls"));
        grant(&mut rules, "shell", Some("ls"));
        grant(&mut rules, "shell", Some("rm"));
        assert_eq!(rules.len(), 2);
    }

    #[test]
    fn history_rebuilt_from_events_pairs_calls_with_results() {
        let frames = vec![
            EventFrame::new(
                EventSeq(1),
                0,
                SessionEvent::UserMessage {
                    text: "run".to_owned(),
                },
            ),
            EventFrame::new(
                EventSeq(2),
                0,
                SessionEvent::ToolCall {
                    call_id: "c1".to_owned(),
                    name: "shell".to_owned(),
                    arguments: "{\"command\":\"ls\"}".to_owned(),
                },
            ),
            EventFrame::new(
                EventSeq(3),
                0,
                SessionEvent::ToolResult {
                    call_id: "c1".to_owned(),
                    ok: true,
                    output: "ok".to_owned(),
                },
            ),
            EventFrame::new(
                EventSeq(4),
                0,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    text: "done".to_owned(),
                },
            ),
        ];
        let history = history_from_events(&frames);
        history.validate().expect("valid");
        assert_eq!(history.turns().len(), 4);
    }

    #[test]
    fn an_empty_log_rebuilds_to_an_empty_conversation() {
        assert!(history_from_events(&[]).is_empty());
    }

    #[test]
    fn the_summary_list_is_empty_before_any_session_exists() {
        let root = tempfile::tempdir().expect("tempdir");
        let sessions = sessions(&root);
        assert!(sessions.list().is_empty());
    }

    #[test]
    fn a_created_session_appears_in_the_list() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut sessions = sessions(&root);
        let id = sessions.create(Vec::new()).expect("create");
        let listed = sessions.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].cwd, "/tmp/work");
    }

    #[test]
    fn a_timestamp_renders_as_iso_8601() {
        assert_eq!(format_timestamp(0).as_deref(), Some("1970-01-01T00:00:00Z"));
    }

    #[test]
    fn mode_and_config_option_payloads_name_their_members() {
        let config = SessionConfig::new("m", Effort::Low, PermissionMode::Ask);
        let modes = modes(&config);
        assert_eq!(modes["currentModeId"], "ask");
        assert_eq!(modes["availableModes"].as_array().expect("array").len(), 3);
        let options = config_options(&config);
        assert_eq!(options[0]["id"], "model");
        assert_eq!(options[1]["currentValue"], "low");
    }

    #[test]
    fn an_empty_conversation_replays_nothing() {
        assert!(replay_updates("s", &History::new()).is_empty());
    }
}
