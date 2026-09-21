//! Subagent delegation.
//!
//! A subagent is a child session a parent starts for one piece of work. Two
//! properties matter more than the rest:
//!
//! - A child inherits the parent's authority and never more. The mode, rules,
//!   workspace, roots, and tool set are captured once, in an [`Admission`], and
//!   every child action is checked against a fresh capture of the live parent.
//! - A change to that authority fails the child closed. A rule edit, a mode
//!   switch, or a moved workspace stops the child rather than letting it
//!   continue with rights the parent no longer holds.
//!
//! Feedback follows steering: a message for a working child is queued rather
//! than raised as a cancellation, and delivered at a boundary where the child
//! can act on it.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex, MutexGuard};

use camino::Utf8PathBuf;
use rune_core::budget::{Budget, BudgetSet, LimitName};
use rune_core::config::{Effort, PermissionMode};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::id::SessionId;
use rune_policy::decision::Outcome;
use rune_policy::rules::RuleSet;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::steering::{Boundary, Cancellation, cancelled_error};

/// Largest task or feedback message accepted for a child.
///
/// A task is a whole request, so this is generous; it is bounded so a runaway
/// caller cannot make one child's prompt unbounded.
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;

/// Largest model identifier accepted for a child.
pub const MAX_MODEL_BYTES: usize = 256;

/// Largest instructions block accepted for a child.
pub const MAX_INSTRUCTIONS_BYTES: usize = 16 * 1024;

/// What a subagent request asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubagentAction {
    /// Create a child for a task.
    Run,
    /// Deliver a message to an existing child.
    Message,
}

impl SubagentAction {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Message => "message",
        }
    }

    /// Parses the wire representation.
    #[must_use]
    pub fn from_name(raw: &str) -> Option<Self> {
        match raw {
            "run" => Some(Self::Run),
            "message" => Some(Self::Message),
            _ => None,
        }
    }
}

impl fmt::Display for SubagentAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The fields a request may carry.
const FIELDS: &[&str] = &[
    "action",
    "task",
    "agent",
    "message",
    "instructions",
    "model",
    "effort",
];

/// A decoded subagent invocation.
///
/// The optional fields are not interchangeable: `model` and `effort` steer the
/// creation of a child, and are refused for a child that already exists, whose
/// model and effort were fixed when it was admitted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SubagentRequest {
    /// Whether this creates a child or addresses one.
    pub action: SubagentAction,
    /// The task, on creation.
    pub task: Option<String>,
    /// The agent the child runs as, or the name of an existing child.
    pub agent: Option<String>,
    /// The message, when addressing an existing child.
    pub message: Option<String>,
    /// Instructions for a new child, replacing the default delegation brief.
    pub instructions: Option<String>,
    /// Model for a new child.
    pub model: Option<String>,
    /// Reasoning effort for a new child.
    pub effort: Option<Effort>,
}

impl SubagentRequest {
    /// Decodes a request.
    ///
    /// Fails closed: an unknown field, a field that does not apply to the
    /// action, or a value outside its bound is refused rather than ignored.
    pub fn decode(value: &Value) -> Result<Self> {
        let object = value.as_object().ok_or_else(|| {
            RuneError::invalid_field("subagent_request", "expected an object of request fields")
        })?;
        for key in object.keys() {
            if !FIELDS.contains(&key.as_str()) {
                return Err(RuneError::invalid_field(
                    key.clone(),
                    "unknown field in a subagent request",
                )
                .with_hint(format!("accepted fields are {}", FIELDS.join(", "))));
            }
        }

        let action = object
            .get("action")
            .ok_or_else(|| RuneError::missing_field("action"))
            .and_then(|raw| {
                raw.as_str()
                    .and_then(SubagentAction::from_name)
                    .ok_or_else(|| RuneError::invalid_field("action", "must be `run` or `message`"))
            })?;

        let task = text(object, "task")?;
        let agent = text(object, "agent")?;
        let message = text(object, "message")?;
        let instructions = text(object, "instructions")?;
        let model = text(object, "model")?;
        let effort = optional_effort(object)?;

        match action {
            SubagentAction::Run => {
                let task = task.ok_or_else(|| RuneError::missing_field("task"))?;
                check_text("task", &task, MAX_PROMPT_BYTES)?;
                if let Some(name) = &agent {
                    check_non_empty("agent", name)?;
                }
                if let Some(value) = &model {
                    check_text("model", value, MAX_MODEL_BYTES)?;
                }
                if let Some(value) = &instructions {
                    check_text("instructions", value, MAX_INSTRUCTIONS_BYTES)?;
                }
                reject(object, &["message"])?;
                Ok(Self {
                    action,
                    task: Some(task),
                    agent,
                    message: None,
                    instructions,
                    model,
                    effort,
                })
            }
            SubagentAction::Message => {
                reject(object, &["task", "instructions", "model", "effort"])?;
                let agent = agent.ok_or_else(|| RuneError::missing_field("agent"))?;
                let message = message.ok_or_else(|| RuneError::missing_field("message"))?;
                check_non_empty("agent", &agent)?;
                check_text("message", &message, MAX_PROMPT_BYTES)?;
                Ok(Self {
                    action,
                    task: None,
                    agent: Some(agent),
                    message: Some(message),
                    instructions: None,
                    model: None,
                    effort: None,
                })
            }
        }
    }

    /// Returns the kind of child this request creates.
    ///
    /// `None` for a message, which addresses a child rather than creating one.
    #[must_use]
    pub fn child_kind(&self) -> Option<ChildKind> {
        match self.action {
            SubagentAction::Message => None,
            SubagentAction::Run => Some(
                self.agent
                    .as_ref()
                    .map_or(ChildKind::OneOff, |name| ChildKind::Named(name.clone())),
            ),
        }
    }
}

/// Reads an optional string field.
///
/// A field present with a non-string value is malformed rather than absent: a
/// caller that sent a number meant something by it.
fn text(object: &serde_json::Map<String, Value>, key: &'static str) -> Result<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(RuneError::invalid_field(key, "must be a string")),
    }
}

/// Reads the reasoning effort, when one was named.
fn optional_effort(object: &serde_json::Map<String, Value>) -> Result<Option<Effort>> {
    let Some(raw) = object.get("effort") else {
        return Ok(None);
    };
    let Some(name) = raw.as_str() else {
        return Err(RuneError::invalid_field("effort", "must be a string"));
    };
    EFFORTS
        .iter()
        .copied()
        .find(|effort| effort.as_str() == name)
        .map(Some)
        .ok_or_else(|| {
            let accepted: Vec<&str> = EFFORTS.iter().map(|effort| effort.as_str()).collect();
            RuneError::invalid_field("effort", format!("unknown effort `{name}`"))
                .with_hint(format!("accepted values are {}", accepted.join(", ")))
        })
}

/// Every effort a request may name.
const EFFORTS: &[Effort] = &[
    Effort::Auto,
    Effort::None,
    Effort::Minimal,
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::Xhigh,
    Effort::Max,
];

/// Refuses fields that do not apply to the action.
///
/// A field that only steers creation is an error once the child exists, because
/// accepting it would imply a change the child cannot honor.
fn reject(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Result<()> {
    for key in keys {
        if object.get(*key).is_some_and(|value| !value.is_null()) {
            return Err(
                RuneError::invalid_field(*key, "accepted only when creating a child")
                    .with_hint(format!("use the `run` action to set `{key}`")),
            );
        }
    }
    Ok(())
}

/// Requires a value that is present to be non-empty.
fn check_non_empty(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(RuneError::invalid_field(field, "must not be empty"));
    }
    Ok(())
}

/// Requires a value to be non-empty and within its bound.
fn check_text(field: &'static str, value: &str, limit: usize) -> Result<()> {
    check_non_empty(field, value)?;
    if value.len() > limit {
        return Err(RuneError::too_large(field, value.len(), limit));
    }
    Ok(())
}

/// The authority a parent session holds.
///
/// These are the inputs an [`Admission`] is computed from. They are read from
/// the live parent rather than cached, so the comparison is against what the
/// parent holds now.
#[derive(Clone, Debug)]
pub struct Authority {
    /// Permission mode in force.
    pub mode: PermissionMode,
    /// Rules in force, including grants recorded this session.
    pub rules: RuleSet,
    /// Workspace root.
    pub workspace: Utf8PathBuf,
    /// Every root the parent may reach.
    pub roots: Vec<Utf8PathBuf>,
    /// Tool names the parent may call.
    pub tools: Vec<String>,
    /// Identity of the MCP tool view, when one is mounted.
    pub mcp_view: Option<String>,
    /// Generation of that view, raised whenever its contents change.
    pub generation: u64,
}

/// The parent's authority as it stood when a child was admitted.
///
/// Computed once. Every child action is then checked against a fresh capture,
/// and any difference refuses the action.
#[derive(Clone, Debug)]
pub struct Admission {
    /// Permission mode at admission.
    pub permission_mode: PermissionMode,
    /// Digest of the rules and the tool set at admission.
    pub rules_fingerprint: String,
    /// Workspace root at admission.
    pub workspace: Utf8PathBuf,
    /// Roots the parent could reach at admission.
    pub roots: Vec<Utf8PathBuf>,
    /// Tool names the parent could call at admission.
    pub tool_names: Vec<String>,
    /// MCP tool view at admission.
    pub mcp_view: Option<String>,
    /// Generation of that view at admission.
    pub generation: u64,
}

impl Admission {
    /// Captures a parent's authority.
    #[must_use]
    pub fn capture(parent: &Authority) -> Self {
        let mut tool_names = parent.tools.clone();
        // Normalized, so a reordered tool list is not mistaken for a change.
        tool_names.sort();
        tool_names.dedup();
        let mut roots = parent.roots.clone();
        roots.sort();
        roots.dedup();
        Self {
            permission_mode: parent.mode,
            rules_fingerprint: fingerprint(&parent.rules, &tool_names),
            workspace: parent.workspace.clone(),
            roots,
            tool_names,
            mcp_view: parent.mcp_view.clone(),
            generation: parent.generation,
        }
    }

    /// Decides whether a child may still call a tool.
    ///
    /// The admitted snapshot is the child's ceiling: a tool outside it is
    /// refused, and so is any action taken after the parent's authority moved.
    /// A refused action is never executed.
    ///
    /// The tool set is compared before the fingerprint because the fingerprint
    /// covers it, so comparing it first is what makes each refusal name the
    /// thing that actually moved.
    #[must_use]
    pub fn permits(&self, current: &Admission, tool: &str) -> AdmissionVerdict {
        if !self.tool_names.iter().any(|name| name == tool) {
            return AdmissionVerdict::refused(format!("the child was admitted without `{tool}`"));
        }
        if self.permission_mode != current.permission_mode {
            return AdmissionVerdict::refused(
                "the permission mode changed after the child was admitted",
            );
        }
        if self.workspace != current.workspace {
            return AdmissionVerdict::refused("the workspace changed after the child was admitted");
        }
        if self.roots != current.roots {
            return AdmissionVerdict::refused(
                "the workspace roots changed after the child was admitted",
            );
        }
        if self.tool_names != current.tool_names {
            return AdmissionVerdict::refused("the tool set changed after the child was admitted");
        }
        if self.mcp_view != current.mcp_view || self.generation != current.generation {
            return AdmissionVerdict::refused(
                "the MCP tool view changed after the child was admitted",
            );
        }
        if self.rules_fingerprint != current.rules_fingerprint {
            return AdmissionVerdict::refused(
                "the permission rules changed after the child was admitted",
            );
        }
        AdmissionVerdict::Permitted
    }
}

/// Whether a child action is still covered by its admission.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AdmissionVerdict {
    /// The action is within the admitted authority.
    Permitted,
    /// The action is outside it, and must not run.
    Refused {
        /// What changed, for the parent's transcript.
        reason: String,
    },
}

impl AdmissionVerdict {
    /// Builds a refusal.
    fn refused(reason: impl Into<String>) -> Self {
        Self::Refused {
            reason: reason.into(),
        }
    }

    /// Returns true when the action may run.
    #[must_use]
    pub const fn is_permitted(&self) -> bool {
        matches!(self, Self::Permitted)
    }

    /// Returns the refusal reason, when there is one.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Permitted => None,
            Self::Refused { reason } => Some(reason),
        }
    }

    /// Converts a refusal into an error for the parent.
    pub fn into_result(self, tool: &str) -> Result<()> {
        match self {
            Self::Permitted => Ok(()),
            Self::Refused { reason } => Err(refused_error(tool, &reason)),
        }
    }
}

/// Builds the error for a refused child action.
fn refused_error(tool: &str, reason: &str) -> RuneError {
    RuneError::new(
        ErrorCode::PermissionDenied,
        format!("the child may not call `{tool}`: {reason}"),
    )
    .with_hint("the child stops rather than acting on an authority the parent no longer holds")
}

/// Returns a digest of the rules and the tool set.
///
/// Covers each rule's tool, pattern, outcome, and layer, because a rule that
/// changed any of those is a change in what the parent permits. The digest is
/// what makes drift detectable; it is not a signature.
fn fingerprint(rules: &RuleSet, tools: &[String]) -> String {
    let mut hasher = Sha256::new();
    for rule in rules.rules() {
        hasher.update(rule.tool.as_bytes());
        hasher.update(b"\x1f");
        hasher.update(rule.pattern.as_bytes());
        hasher.update(b"\x1f");
        hasher.update(rule.outcome.as_str().as_bytes());
        hasher.update(b"\x1f");
        hasher.update(rule.layer.as_str().as_bytes());
        hasher.update(b"\x1e");
    }
    for name in tools {
        hasher.update(name.as_bytes());
        hasher.update(b"\x1e");
    }
    hex(&hasher.finalize())
}

/// Renders bytes as lowercase hex.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Kind of child a parent registered.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ChildKind {
    /// A child for one piece of work, addressed only by its identifier.
    OneOff,
    /// A child addressed by name, so the parent can message it again.
    Named(String),
}

impl ChildKind {
    /// Returns the name, when the child has one.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::OneOff => None,
            Self::Named(name) => Some(name),
        }
    }
}

/// Identifier of one registered child.
///
/// The session identifier is the child's own. It is not a session the product
/// lists or reopens; the child is reached through the parent that registered it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ChildId {
    /// Session the child runs as.
    pub session: SessionId,
    /// Ordinal assigned at creation, unique within the registry.
    pub seq: u64,
}

/// What a child was started for.
///
/// Fixed at creation and never revised, which is why `model` and `effort` are
/// accepted only on a `run`: a child already working cannot change the model it
/// is running on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChildBrief {
    /// The task, when the parent delegated one.
    pub task: Option<String>,
    /// Instructions for the child, replacing the default delegation brief.
    pub instructions: Option<String>,
    /// Model the child runs on, when the parent chose one.
    pub model: Option<String>,
    /// Reasoning effort the child runs at.
    pub effort: Effort,
}

impl Default for ChildBrief {
    fn default() -> Self {
        Self {
            task: None,
            instructions: None,
            model: None,
            effort: Effort::Auto,
        }
    }
}

/// One child session.
#[derive(Debug)]
pub struct Child {
    id: ChildId,
    parent: SessionId,
    kind: ChildKind,
    brief: ChildBrief,
    admission: Admission,
    /// The parent's rules as they stood at admission. The child is decided
    /// against this snapshot, never against the parent's live rules.
    rules: RuleSet,
    feedback: Feedback,
    cancellation: Cancellation,
    steps: u32,
    stop: Option<String>,
}

impl Child {
    /// Builds a working child from the parent's authority as it stands now.
    fn new(
        id: ChildId,
        parent: SessionId,
        kind: ChildKind,
        brief: ChildBrief,
        authority: &Authority,
    ) -> Self {
        Self {
            id,
            parent,
            kind,
            brief,
            admission: Admission::capture(authority),
            rules: authority.rules.clone(),
            feedback: Feedback::default(),
            cancellation: Cancellation::new(),
            steps: 0,
            stop: None,
        }
    }

    /// Returns the child's identifier.
    #[must_use]
    pub const fn id(&self) -> ChildId {
        self.id
    }

    /// Returns what the child was started for.
    #[must_use]
    pub const fn brief(&self) -> &ChildBrief {
        &self.brief
    }

    /// Returns the parent that registered it.
    #[must_use]
    pub const fn parent(&self) -> SessionId {
        self.parent
    }

    /// Returns the child's kind.
    #[must_use]
    pub const fn kind(&self) -> &ChildKind {
        &self.kind
    }

    /// Returns the authority the child was admitted with.
    #[must_use]
    pub const fn admission(&self) -> &Admission {
        &self.admission
    }

    /// Returns the child's feedback queue.
    #[must_use]
    pub const fn feedback(&self) -> &Feedback {
        &self.feedback
    }

    /// Returns the child's cancellation flag.
    #[must_use]
    pub const fn cancellation(&self) -> &Cancellation {
        &self.cancellation
    }

    /// Resolves policy for a child call against the admitted rules.
    ///
    /// A child has no reviewer and no one to prompt, so both an unresolved call
    /// and a denied one are refused: the child inherits what the parent allowed
    /// at admission and nothing more.
    #[must_use]
    pub fn decide(&self, name: &str, target: Option<&str>) -> ChildPermission {
        let (outcome, reason) =
            crate::turn::decide_call(&self.rules, self.admission.permission_mode, name, target);
        match outcome {
            Outcome::Allow => ChildPermission::Allowed,
            Outcome::Deny => ChildPermission::Denied { reason },
            Outcome::Ask => ChildPermission::Unresolved { reason },
        }
    }

    /// Returns true while the child accepts work and feedback.
    #[must_use]
    pub fn is_working(&self) -> bool {
        self.feedback.is_open()
    }

    /// Returns the steps the child completed.
    #[must_use]
    pub const fn steps(&self) -> u32 {
        self.steps
    }

    /// Returns why the child stopped, when it was stopped.
    #[must_use]
    pub fn stop_reason(&self) -> Option<&str> {
        self.stop.as_deref()
    }

    /// Records a completed step.
    fn record_step(&mut self) {
        self.steps = self.steps.saturating_add(1);
    }

    /// Marks the child finished.
    fn finish(&mut self) {
        self.feedback.close();
    }

    /// Marks the child stopped, closing it to further work.
    fn stop(&mut self, reason: impl Into<String>) {
        self.stop = Some(reason.into());
        self.feedback.close();
    }
}

/// Policy outcome for one child call.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ChildPermission {
    /// The child may run the call.
    Allowed,
    /// The admitted rules deny the call.
    Denied {
        /// The rule that decided it.
        reason: String,
    },
    /// No rule resolved the call, and a child cannot ask.
    Unresolved {
        /// The rule evaluation, for the parent's transcript.
        reason: String,
    },
}

impl ChildPermission {
    /// Returns the refusal reason, when the call may not run.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        match self {
            Self::Allowed => None,
            Self::Denied { reason } => Some(format!("the child's rules deny it: {reason}")),
            Self::Unresolved { reason } => Some(format!(
                "no rule resolved it and a child cannot ask for approval: {reason}"
            )),
        }
    }
}

/// Depth of a child's feedback queue.
///
/// Feedback is the child's steering path, so it draws the same bound. Resolved
/// at compile time from the default, and overridable through the limits for a
/// host that lowers it.
const FEEDBACK_DEPTH: usize = match LimitName::SteeringQueueDepth.default_value() {
    Budget::Bounded(value) => value as usize,
    Budget::Unbounded => 1,
};

/// One message queued for a working child.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FeedbackMessage {
    /// The submitted text.
    pub text: String,
    /// Boundary at which it was delivered, once it has been.
    pub drained_at: Option<Boundary>,
}

/// The bounded feedback queue for one child.
///
/// Cloning shares the queue, so a host that is mid-call can submit feedback
/// through a handle while the child works. Submitting never cancels the child.
#[derive(Clone, Debug)]
pub struct Feedback {
    inner: Arc<Mutex<Channel>>,
    depth: usize,
}

#[derive(Debug)]
struct Channel {
    messages: Vec<FeedbackMessage>,
    open: bool,
}

impl Default for Feedback {
    fn default() -> Self {
        Self::new(FEEDBACK_DEPTH)
    }
}

impl Feedback {
    /// Builds a queue with the given depth.
    #[must_use]
    pub fn new(depth: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Channel {
                messages: Vec::new(),
                open: true,
            })),
            depth: depth.max(1),
        }
    }

    /// Builds a queue sized from the limits.
    #[must_use]
    pub fn from_limits(limits: &BudgetSet) -> Self {
        Self::new(limits.get_usize(LimitName::SteeringQueueDepth).max(1))
    }

    /// Queues a message for the child.
    ///
    /// Fails rather than growing without bound, and fails once the child is no
    /// longer working, because a message nobody will read is a lost instruction
    /// the caller should hear about.
    pub fn queue(&self, text: impl Into<String>) -> Result<()> {
        let text = text.into();
        let mut guard = self.lock()?;
        if !guard.open {
            return Err(
                RuneError::new(ErrorCode::InvalidState, "the child is no longer working")
                    .with_hint("start a new child for this work"),
            );
        }
        check_text("feedback", &text, MAX_PROMPT_BYTES)?;
        if guard.messages.len() >= self.depth {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                format!("the feedback queue is full at {} messages", self.depth),
            )
            .with_hint("wait for the child to reach a boundary"));
        }
        guard.messages.push(FeedbackMessage {
            text,
            drained_at: None,
        });
        Ok(())
    }

    /// Returns everything queued, recording the boundary.
    ///
    /// Draining happens at a boundary rather than mid-step, so a message can
    /// never land in the middle of a tool call the child is committed to.
    pub fn drain(&self, boundary: Boundary) -> Vec<FeedbackMessage> {
        let Ok(mut guard) = self.lock() else {
            return Vec::new();
        };
        let mut drained: Vec<FeedbackMessage> = std::mem::take(&mut guard.messages);
        for message in &mut drained {
            message.drained_at = Some(boundary);
        }
        drained
    }

    /// Returns the number of queued messages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().map_or(0, |guard| guard.messages.len())
    }

    /// Returns true when nothing is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the queue depth.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// Returns true while the queue accepts messages.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.lock().is_ok_and(|guard| guard.open)
    }

    /// Stops accepting messages.
    fn close(&self) {
        if let Ok(mut guard) = self.lock() {
            guard.open = false;
        }
    }

    /// Locks the queue, converting a poisoned lock into an error.
    fn lock(&self) -> Result<MutexGuard<'_, Channel>> {
        self.inner.lock().map_err(|_| {
            RuneError::new(
                ErrorCode::Internal,
                "the feedback queue lock was poisoned by a panicking thread",
            )
        })
    }
}

/// The children one process has registered, grouped by parent.
#[derive(Debug)]
pub struct Registry {
    children: BTreeMap<ChildId, Child>,
    by_parent: BTreeMap<SessionId, Vec<ChildId>>,
    cap: usize,
    next_seq: u64,
}

impl Registry {
    /// Builds an empty registry with a per-parent child bound.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            children: BTreeMap::new(),
            by_parent: BTreeMap::new(),
            cap: cap.max(1),
            next_seq: 0,
        }
    }

    /// Builds a registry sized from the limits.
    #[must_use]
    pub fn from_limits(limits: &BudgetSet) -> Self {
        Self::new(limits.get_usize(LimitName::SubagentChildren).max(1))
    }

    /// Returns the bound on children one parent may register.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.cap
    }

    /// Returns the children an ordinary listing shows.
    ///
    /// Always empty. A child belongs to its parent's transcript, so it is not a
    /// conversation a caller may list, reopen, or attach to.
    #[must_use]
    pub fn discoverable(&self) -> Vec<ChildId> {
        Vec::new()
    }

    /// Registers a child for a parent.
    ///
    /// The admission and the rule snapshot are taken from the parent's
    /// authority as it stands now, so a child can never be created against a
    /// wider authority than the parent holds.
    pub fn create(
        &mut self,
        parent: SessionId,
        kind: ChildKind,
        brief: ChildBrief,
        authority: &Authority,
    ) -> Result<ChildId> {
        if let Some(name) = kind.name() {
            check_non_empty("agent", name)?;
            if self.named(&parent, name).is_some() {
                return Err(RuneError::new(
                    ErrorCode::AlreadyExists,
                    format!("child `{name}` is already registered for this parent"),
                )
                .with_hint("address the child that exists, or name this one differently"));
            }
        }
        let held = self.by_parent.get(&parent).map_or(0, Vec::len);
        if held >= self.cap {
            return Err(RuneError::new(
                ErrorCode::LimitExceeded,
                format!("a parent may register {} children", self.cap),
            )
            .with_hint("remove a child that has finished"));
        }

        self.next_seq = self.next_seq.saturating_add(1);
        let id = ChildId {
            session: SessionId::generate(),
            seq: self.next_seq,
        };
        let child = Child::new(id, parent, kind, brief, authority);
        self.children.insert(id, child);
        self.by_parent.entry(parent).or_default().push(id);
        Ok(id)
    }

    /// Registers a child from a decoded request.
    ///
    /// The request's brief is carried onto the child, so the model and effort
    /// the parent asked for are what the child runs on.
    pub fn start(
        &mut self,
        parent: SessionId,
        request: &SubagentRequest,
        authority: &Authority,
    ) -> Result<ChildId> {
        let kind = request.child_kind().ok_or_else(|| {
            RuneError::new(ErrorCode::InvalidState, "a message does not create a child")
                .with_hint("use the `run` action to create one")
        })?;
        let brief = ChildBrief {
            task: request.task.clone(),
            instructions: request.instructions.clone(),
            model: request.model.clone(),
            effort: request.effort.unwrap_or_default(),
        };
        self.create(parent, kind, brief, authority)
    }

    /// Returns a child.
    pub fn get(&self, id: &ChildId) -> Result<&Child> {
        self.children.get(id).ok_or_else(|| unknown_child(id))
    }

    /// Returns a child for modification.
    pub fn get_mut(&mut self, id: &ChildId) -> Result<&mut Child> {
        self.children.get_mut(id).ok_or_else(|| unknown_child(id))
    }

    /// Queues feedback for a working child.
    ///
    /// Queued rather than interrupting: a message for a child that is mid-call
    /// waits for a boundary, because cancelling work already in flight would
    /// discard the result the parent asked for.
    pub fn queue(&self, id: &ChildId, text: impl Into<String>) -> Result<()> {
        self.get(id)?.feedback().queue(text)
    }

    /// Returns the children registered by a parent.
    ///
    /// This is the only question an ordinary lookup asks of the registry, and a
    /// child's own session identifier is not a key here, so an ordinary lookup
    /// cannot reach a child.
    #[must_use]
    pub fn children_of(&self, parent: &SessionId) -> &[ChildId] {
        self.by_parent.get(parent).map_or(&[], Vec::as_slice)
    }

    /// Returns the child a parent registered under a name.
    #[must_use]
    pub fn named(&self, parent: &SessionId, name: &str) -> Option<&Child> {
        self.children_of(parent)
            .iter()
            .filter_map(|id| self.children.get(id))
            .find(|child| child.kind.name() == Some(name))
    }

    /// Removes a child, returning it.
    pub fn remove(&mut self, id: &ChildId) -> Result<Child> {
        let child = self.children.remove(id).ok_or_else(|| unknown_child(id))?;
        if let Some(ids) = self.by_parent.get_mut(&child.parent) {
            ids.retain(|held| held != id);
            if ids.is_empty() {
                self.by_parent.remove(&child.parent);
            }
        }
        Ok(child)
    }

    /// Returns the number of registered children.
    #[must_use]
    pub fn len(&self) -> usize {
        self.children.len()
    }

    /// Returns true when no child is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }
}

/// Builds the error for an unknown child.
fn unknown_child(id: &ChildId) -> RuneError {
    RuneError::new(
        ErrorCode::NotFound,
        format!("no child session `{}` is registered", id.session),
    )
}

/// One step of child work.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ChildStep {
    /// The child wants to call a tool.
    Tool {
        /// Tool name.
        name: String,
        /// Arguments as the child produced them.
        arguments: Value,
    },
    /// The child is done.
    Done {
        /// What the child reports to the parent.
        summary: String,
    },
}

/// The work a child performs.
///
/// Behind a trait because a child turn talks to a provider, which a host
/// supplies and a test replaces with a script.
pub trait ChildWork {
    /// Returns the next step the child wants to take.
    fn next_step(&mut self) -> ChildStep;

    /// Delivers feedback at a boundary.
    fn accept_feedback(&mut self, text: &str);

    /// Returns the permission target for a call, when the tool names one.
    ///
    /// The host owns the tool registry, so the mapping from arguments to target
    /// is asked of it rather than derived a second time here.
    fn target(&self, name: &str, arguments: &Value) -> Option<String>;

    /// Runs a permitted tool call.
    ///
    /// Called only after the child's authority and the admitted rules have both
    /// allowed the call, so an implementation never decides whether it is
    /// allowed.
    fn execute(&mut self, name: &str, arguments: &Value) -> Result<()>;
}

/// The parent authority as it stands now.
pub trait AuthoritySource {
    /// Captures the parent's authority.
    fn authority(&self) -> Authority;
}

/// What a finished child reports.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChildOutcome {
    /// The child.
    pub id: ChildId,
    /// The child's kind.
    pub kind: ChildKind,
    /// What the child reported.
    pub summary: String,
    /// Steps the child completed.
    pub steps: u32,
    /// Feedback messages delivered.
    pub feedback_applied: usize,
}

/// Runs a child until it finishes.
///
/// The parent's authority is re-read before every step, so a change is caught at
/// the next boundary rather than after the child has acted on stale rights. A
/// refusal, a cancellation, or a failing step ends the run and closes the child;
/// the reason is left on the child for the parent to record.
pub fn run_child(
    child: &mut Child,
    work: &mut dyn ChildWork,
    parent: &dyn AuthoritySource,
) -> Result<ChildOutcome> {
    if !child.is_working() {
        return Err(RuneError::new(
            ErrorCode::InvalidState,
            "the child is no longer working",
        ));
    }

    let mut feedback_applied: usize = 0;
    loop {
        if child.cancellation().is_cancelled() {
            child.stop("the child was cancelled");
            return Err(cancelled_error());
        }

        for message in child.feedback().drain(Boundary::Model) {
            work.accept_feedback(&message.text);
            feedback_applied = feedback_applied.saturating_add(1);
        }

        match work.next_step() {
            ChildStep::Done { summary } => {
                let outcome = ChildOutcome {
                    id: child.id(),
                    kind: child.kind().clone(),
                    summary,
                    steps: child.steps(),
                    feedback_applied,
                };
                child.finish();
                return Ok(outcome);
            }
            ChildStep::Tool { name, arguments } => {
                let current = Admission::capture(&parent.authority());
                // Authority first: a child whose parent moved is stopped before
                // the call is even considered on its merits.
                if let AdmissionVerdict::Refused { reason } =
                    child.admission().permits(&current, &name)
                {
                    let error = refused_error(&name, &reason);
                    child.stop(reason);
                    return Err(error);
                }
                // Then the admitted rules, which are the parent's rules as they
                // stood at admission. A child never resolves against the
                // parent's live rules.
                let target = work.target(&name, &arguments);
                if let Some(refusal) = child.decide(&name, target.as_deref()).refusal() {
                    let error = refused_error(&name, &refusal);
                    child.stop(refusal);
                    return Err(error);
                }
                if let Err(err) = work.execute(&name, &arguments) {
                    child.stop(err.message().to_owned());
                    return Err(err);
                }
                child.record_step();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use rune_policy::decision::Layer;
    use rune_policy::rules::Rule;
    use serde_json::json;

    use super::*;

    fn limits() -> BudgetSet {
        BudgetSet::new()
    }

    fn rules() -> RuleSet {
        let mut rules = RuleSet::new();
        rules.push(Rule::deny("shell", "rm *", Layer::User));
        rules.push(Rule::allow("read_file", "*", Layer::Project));
        rules
    }

    /// Sets a limit at the user layer, which is the layer a test may write.
    fn set_limit(budgets: &mut BudgetSet, name: LimitName, value: u64) {
        budgets
            .set(name, Budget::Bounded(value), rune_core::config::Layer::User)
            .expect("set");
    }

    fn authority(tools: &[&str]) -> Authority {
        Authority {
            mode: PermissionMode::Auto,
            rules: rules(),
            workspace: Utf8PathBuf::from("/work"),
            roots: vec![Utf8PathBuf::from("/work")],
            tools: tools.iter().map(|name| (*name).to_owned()).collect(),
            mcp_view: Some("server-a".to_owned()),
            generation: 1,
        }
    }

    fn admission(tools: &[&str]) -> Admission {
        Admission::capture(&authority(tools))
    }

    /// A parent whose authority a test can move mid-child.
    #[derive(Debug)]
    struct LiveParent {
        authority: Rc<RefCell<Authority>>,
    }

    impl LiveParent {
        fn new(tools: &[&str]) -> Self {
            Self::from(authority(tools))
        }

        fn from(parent: Authority) -> Self {
            Self {
                authority: Rc::new(RefCell::new(parent)),
            }
        }

        fn handle(&self) -> Rc<RefCell<Authority>> {
            Rc::clone(&self.authority)
        }
    }

    impl AuthoritySource for LiveParent {
        fn authority(&self) -> Authority {
            self.authority.borrow().clone()
        }
    }

    /// A child whose steps are scripted.
    struct ScriptedChild {
        steps: VecDeque<ChildStep>,
        executed: Vec<String>,
        accepted: Vec<String>,
        cancellation: Cancellation,
        cancelled_during_step: Vec<bool>,
        mid_step: Option<Box<dyn FnOnce()>>,
        /// Permission target to report, overriding the one read from arguments.
        target: Option<String>,
    }

    impl ScriptedChild {
        fn new(cancellation: &Cancellation) -> Self {
            Self {
                steps: VecDeque::new(),
                executed: Vec::new(),
                accepted: Vec::new(),
                cancellation: cancellation.clone(),
                cancelled_during_step: Vec::new(),
                mid_step: None,
                target: None,
            }
        }

        fn with_steps(cancellation: &Cancellation, steps: Vec<ChildStep>) -> Self {
            let mut child = Self::new(cancellation);
            child.steps = steps.into();
            child
        }
    }

    impl ChildWork for ScriptedChild {
        fn next_step(&mut self) -> ChildStep {
            self.steps.pop_front().unwrap_or(ChildStep::Done {
                summary: "finished".to_owned(),
            })
        }

        fn accept_feedback(&mut self, text: &str) {
            self.accepted.push(text.to_owned());
        }

        fn target(&self, _name: &str, arguments: &Value) -> Option<String> {
            self.target.clone().or_else(|| {
                arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
        }

        fn execute(&mut self, name: &str, _arguments: &Value) -> Result<()> {
            self.executed.push(name.to_owned());
            self.cancelled_during_step
                .push(self.cancellation.is_cancelled());
            if let Some(action) = self.mid_step.take() {
                action();
            }
            Ok(())
        }
    }

    fn tool(name: &str) -> ChildStep {
        ChildStep::Tool {
            name: name.to_owned(),
            arguments: json!({ "path": "notes.md" }),
        }
    }

    fn done() -> ChildStep {
        ChildStep::Done {
            summary: "wrote the notes".to_owned(),
        }
    }

    // Request decoding.

    #[test]
    fn a_run_request_needs_a_task() {
        let err = SubagentRequest::decode(&json!({ "action": "run" })).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::MissingField);
        assert_eq!(err.field(), Some("task"));

        let err = SubagentRequest::decode(&json!({ "action": "run", "task": "   " }))
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);

        let request =
            SubagentRequest::decode(&json!({ "action": "run", "task": "count the files" }))
                .expect("accepted");
        assert_eq!(request.action, SubagentAction::Run);
        assert_eq!(request.task.as_deref(), Some("count the files"));
    }

    #[test]
    fn a_message_request_needs_an_agent_and_a_message() {
        let err = SubagentRequest::decode(&json!({ "action": "message", "message": "keep going" }))
            .expect_err("refused");
        assert_eq!(err.field(), Some("agent"));

        let err = SubagentRequest::decode(&json!({ "action": "message", "agent": "runner" }))
            .expect_err("refused");
        assert_eq!(err.field(), Some("message"));

        let err =
            SubagentRequest::decode(&json!({ "action": "message", "agent": " ", "message": "hi" }))
                .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);

        let request = SubagentRequest::decode(
            &json!({ "action": "message", "agent": "runner", "message": "keep going" }),
        )
        .expect("accepted");
        assert_eq!(request.action, SubagentAction::Message);
        assert!(request.child_kind().is_none());
    }

    #[test]
    fn model_and_effort_are_accepted_when_creating_a_child() {
        let request = SubagentRequest::decode(&json!({
            "action": "run",
            "task": "digest the logs",
            "instructions": "report only failures",
            "model": "gpt-5",
            "effort": "high"
        }))
        .expect("accepted");
        assert_eq!(request.model.as_deref(), Some("gpt-5"));
        assert_eq!(request.effort, Some(Effort::High));
        assert_eq!(
            request.instructions.as_deref(),
            Some("report only failures")
        );
        assert_eq!(request.child_kind(), Some(ChildKind::OneOff));
    }

    #[test]
    fn model_and_effort_are_rejected_for_an_existing_child() {
        let err = SubagentRequest::decode(&json!({
            "action": "message",
            "agent": "runner",
            "message": "keep going",
            "model": "gpt-5"
        }))
        .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("model"));

        let err = SubagentRequest::decode(&json!({
            "action": "message",
            "agent": "runner",
            "message": "keep going",
            "effort": "high"
        }))
        .expect_err("refused");
        assert_eq!(err.field(), Some("effort"));

        let err = SubagentRequest::decode(&json!({
            "action": "message",
            "agent": "runner",
            "message": "keep going",
            "task": "another job"
        }))
        .expect_err("refused");
        assert_eq!(err.field(), Some("task"));

        let err = SubagentRequest::decode(&json!({
            "action": "message",
            "agent": "runner",
            "message": "keep going",
            "instructions": "be brief"
        }))
        .expect_err("refused");
        assert_eq!(err.field(), Some("instructions"));
    }

    #[test]
    fn unknown_and_malformed_requests_are_rejected() {
        let err = SubagentRequest::decode(&json!({ "action": "run", "task": "x", "extra": 1 }))
            .expect_err("refused");
        assert_eq!(err.field(), Some("extra"));

        let err = SubagentRequest::decode(&json!("run")).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("subagent_request"));

        let err = SubagentRequest::decode(&json!({ "task": "x" })).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::MissingField);
        assert_eq!(err.field(), Some("action"));

        let err = SubagentRequest::decode(&json!({ "action": "start", "task": "x" }))
            .expect_err("refused");
        assert_eq!(err.field(), Some("action"));

        let err =
            SubagentRequest::decode(&json!({ "action": "run", "task": 7 })).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("task"));
    }

    #[test]
    fn a_run_naming_an_agent_creates_a_named_child() {
        let request = SubagentRequest::decode(&json!({
            "action": "run",
            "task": "sweep the repo",
            "agent": "sweeper"
        }))
        .expect("accepted");
        assert_eq!(
            request.child_kind(),
            Some(ChildKind::Named("sweeper".to_owned()))
        );
    }

    #[test]
    fn starting_from_a_request_carries_its_brief_onto_the_child() {
        let request = SubagentRequest::decode(&json!({
            "action": "run",
            "task": "summarize the diff",
            "agent": "summarizer",
            "instructions": "be terse",
            "model": "gpt-5",
            "effort": "low"
        }))
        .expect("accepted");

        let mut registry = Registry::from_limits(&limits());
        let id = registry
            .start(SessionId::generate(), &request, &authority(&["read_file"]))
            .expect("created");
        let brief = registry.get(&id).expect("present").brief();
        assert_eq!(brief.task.as_deref(), Some("summarize the diff"));
        assert_eq!(brief.instructions.as_deref(), Some("be terse"));
        assert_eq!(brief.model.as_deref(), Some("gpt-5"));
        assert_eq!(brief.effort, Effort::Low);
    }

    #[test]
    fn a_message_does_not_create_a_child() {
        let request = SubagentRequest::decode(&json!({
            "action": "message",
            "agent": "summarizer",
            "message": "keep going"
        }))
        .expect("accepted");
        let mut registry = Registry::from_limits(&limits());
        let err = registry
            .start(SessionId::generate(), &request, &authority(&["read_file"]))
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidState);
        assert!(registry.is_empty());
    }

    #[test]
    fn every_effort_name_decodes() {
        for effort in EFFORTS {
            let request = SubagentRequest::decode(&json!({
                "action": "run",
                "task": "x",
                "effort": effort.as_str()
            }))
            .expect("accepted");
            assert_eq!(request.effort, Some(*effort));
        }

        let err =
            SubagentRequest::decode(&json!({ "action": "run", "task": "x", "effort": "turbo" }))
                .expect_err("refused");
        assert_eq!(err.field(), Some("effort"));
    }

    // Size bounds.

    #[test]
    fn a_task_at_the_bound_is_accepted_and_one_byte_past_it_is_not() {
        let at = "x".repeat(MAX_PROMPT_BYTES);
        let request =
            SubagentRequest::decode(&json!({ "action": "run", "task": at })).expect("accepted");
        assert_eq!(request.task.map(|task| task.len()), Some(MAX_PROMPT_BYTES));

        let past = "x".repeat(MAX_PROMPT_BYTES.saturating_add(1));
        let err = SubagentRequest::decode(&json!({ "action": "run", "task": past }))
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("task"));
    }

    #[test]
    fn a_message_at_the_bound_is_accepted_and_one_byte_past_it_is_not() {
        let at = "x".repeat(MAX_PROMPT_BYTES);
        SubagentRequest::decode(&json!({
            "action": "message",
            "agent": "runner",
            "message": at
        }))
        .expect("accepted");

        let past = "x".repeat(MAX_PROMPT_BYTES.saturating_add(1));
        let err = SubagentRequest::decode(&json!({
            "action": "message",
            "agent": "runner",
            "message": past
        }))
        .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("message"));
    }

    #[test]
    fn a_model_at_the_bound_is_accepted_and_one_byte_past_it_is_not() {
        let at = "m".repeat(MAX_MODEL_BYTES);
        SubagentRequest::decode(&json!({ "action": "run", "task": "x", "model": at }))
            .expect("accepted");

        let past = "m".repeat(MAX_MODEL_BYTES.saturating_add(1));
        let err = SubagentRequest::decode(&json!({ "action": "run", "task": "x", "model": past }))
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("model"));
    }

    #[test]
    fn instructions_at_the_bound_are_accepted_and_one_byte_past_it_is_not() {
        let at = "i".repeat(MAX_INSTRUCTIONS_BYTES);
        SubagentRequest::decode(&json!({ "action": "run", "task": "x", "instructions": at }))
            .expect("accepted");

        let past = "i".repeat(MAX_INSTRUCTIONS_BYTES.saturating_add(1));
        let err = SubagentRequest::decode(&json!({
            "action": "run",
            "task": "x",
            "instructions": past
        }))
        .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("instructions"));
    }

    #[test]
    fn feedback_at_the_bound_is_accepted_and_one_byte_past_it_is_not() {
        let feedback = Feedback::new(4);
        feedback
            .queue("f".repeat(MAX_PROMPT_BYTES))
            .expect("accepted");
        let err = feedback
            .queue("f".repeat(MAX_PROMPT_BYTES.saturating_add(1)))
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::TooLarge);
        assert_eq!(err.field(), Some("feedback"));
    }

    // Authority.

    #[test]
    fn capturing_an_unchanged_authority_is_stable() {
        let parent = authority(&["read_file", "glob_files"]);
        let first = Admission::capture(&parent);
        let second = Admission::capture(&parent);
        assert_eq!(first.rules_fingerprint, second.rules_fingerprint);
        assert_eq!(first.rules_fingerprint.len(), 64);
        assert!(first.permits(&second, "read_file").is_permitted());
    }

    #[test]
    fn reordering_the_tool_set_is_not_a_change() {
        let mut reordered = authority(&["glob_files", "read_file"]);
        reordered.tools = vec!["read_file".to_owned(), "glob_files".to_owned()];
        let admitted = Admission::capture(&authority(&["read_file", "glob_files"]));
        assert!(
            admitted
                .permits(&Admission::capture(&reordered), "read_file")
                .is_permitted()
        );
    }

    #[test]
    fn a_rule_change_changes_the_fingerprint() {
        let mut changed = authority(&["read_file"]);
        changed
            .rules
            .push(Rule::allow("shell", "*", Layer::Session));
        let admitted = admission(&["read_file"]);
        let current = Admission::capture(&changed);
        assert_ne!(admitted.rules_fingerprint, current.rules_fingerprint);
        let verdict = admitted.permits(&current, "read_file");
        assert!(!verdict.is_permitted());
        assert!(verdict.reason().expect("reason").contains("rules"));
    }

    #[test]
    fn every_authority_change_is_refused() {
        let admitted = admission(&["read_file"]);
        let base = authority(&["read_file"]);

        let mut mode = base.clone();
        mode.mode = PermissionMode::FullAccess;
        let mut workspace = base.clone();
        workspace.workspace = Utf8PathBuf::from("/elsewhere");
        let mut roots = base.clone();
        roots.roots.push(Utf8PathBuf::from("/tmp"));
        let mut tools = base.clone();
        tools.tools.push("shell".to_owned());
        let mut removed = base.clone();
        removed.tools.retain(|name| name != "read_file");
        let mut mcp = base.clone();
        mcp.generation = 2;
        let mut unmounted = base.clone();
        unmounted.mcp_view = None;
        let mut remounted = base.clone();
        remounted.mcp_view = Some("server-b".to_owned());

        for (name, parent) in [
            ("mode", mode),
            ("workspace", workspace),
            ("roots", roots),
            ("tool added", tools),
            ("tool removed", removed),
            ("mcp generation", mcp),
            ("mcp view removed", unmounted),
            ("mcp view replaced", remounted),
        ] {
            let verdict = admitted.permits(&Admission::capture(&parent), "read_file");
            assert!(!verdict.is_permitted(), "{name} was permitted");
            assert!(verdict.reason().is_some(), "{name} had no reason");
        }
    }

    #[test]
    fn a_tool_outside_the_admitted_set_is_refused() {
        let admitted = admission(&["read_file"]);
        let current = Admission::capture(&authority(&["read_file", "shell"]));
        let verdict = admitted.permits(&current, "shell");
        assert!(!verdict.is_permitted());
        assert!(verdict.reason().expect("reason").contains("shell"));
        let err = verdict.into_result("shell").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
    }

    #[test]
    fn a_child_cannot_call_a_tool_its_parent_does_not_have() {
        let mut registry = Registry::from_limits(&limits());
        let parent_id = SessionId::generate();
        let id = registry
            .create(
                parent_id,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        let live = LiveParent::new(&["read_file"]);
        let child = registry.get_mut(&id).expect("present");
        let mut work = ScriptedChild::with_steps(child.cancellation(), vec![tool("shell"), done()]);

        let err = run_child(child, &mut work, &live).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
        assert!(work.executed.is_empty(), "the denied tool ran");
        assert!(child.stop_reason().expect("stopped").contains("shell"));
        assert!(!child.is_working());
    }

    #[test]
    fn a_child_cannot_take_an_action_its_parent_rules_deny() {
        let mut registry = Registry::from_limits(&limits());
        let id = registry
            .create(
                SessionId::generate(),
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["shell"]),
            )
            .expect("created");
        let live = LiveParent::new(&["shell"]);
        let child = registry.get_mut(&id).expect("present");
        // The parent's rules deny `shell` on `rm *`, and the child inherits them.
        let mut work = ScriptedChild::with_steps(
            child.cancellation(),
            vec![ChildStep::Tool {
                name: "shell".to_owned(),
                arguments: json!({ "command": "rm -rf /" }),
            }],
        );
        work.target = Some("rm -rf /".to_owned());

        let err = run_child(child, &mut work, &live).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
        assert!(work.executed.is_empty(), "the denied call ran");
        assert!(child.stop_reason().expect("stopped").contains("deny"));
    }

    #[test]
    fn a_child_cannot_run_a_call_that_resolves_to_asking() {
        let mut narrow = authority(&["read_file"]);
        narrow.rules = RuleSet::new();
        narrow
            .rules
            .push(Rule::allow("read_file", "/work/*", Layer::Project));
        let mut registry = Registry::from_limits(&limits());
        let id = registry
            .create(
                SessionId::generate(),
                ChildKind::OneOff,
                ChildBrief::default(),
                &narrow,
            )
            .expect("created");
        let live = LiveParent::from(narrow.clone());
        let child = registry.get_mut(&id).expect("present");
        let mut work =
            ScriptedChild::with_steps(child.cancellation(), vec![tool("read_file"), done()]);
        // A path outside the one allowed pattern resolves to `ask`, and a child
        // has nobody to ask.
        work.target = Some("/etc/hosts".to_owned());

        let err = run_child(child, &mut work, &live).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
        assert!(work.executed.is_empty());
        assert!(child.stop_reason().expect("stopped").contains("ask"));
    }

    #[test]
    fn an_authority_change_mid_child_stops_it() {
        let mut registry = Registry::from_limits(&limits());
        let parent_id = SessionId::generate();
        let id = registry
            .create(
                parent_id,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        let live = LiveParent::new(&["read_file"]);
        let handle = live.handle();

        let child = registry.get_mut(&id).expect("present");
        let mut work = ScriptedChild::with_steps(
            child.cancellation(),
            vec![tool("read_file"), tool("read_file"), done()],
        );
        // The mode moves while the child's first call is in flight.
        work.mid_step = Some(Box::new(move || {
            handle.borrow_mut().mode = PermissionMode::FullAccess;
        }));

        let err = run_child(child, &mut work, &live).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PermissionDenied);
        assert_eq!(work.executed, ["read_file"], "the child kept working");
        assert_eq!(child.steps(), 1);
        assert!(
            child
                .stop_reason()
                .expect("stopped")
                .contains("permission mode")
        );
        assert!(!child.is_working());
    }

    #[test]
    fn a_child_finishes_when_nothing_moves() {
        let mut registry = Registry::from_limits(&limits());
        let parent_id = SessionId::generate();
        let id = registry
            .create(
                parent_id,
                ChildKind::Named("runner".to_owned()),
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        let live = LiveParent::new(&["read_file"]);
        let child = registry.get_mut(&id).expect("present");
        let mut work =
            ScriptedChild::with_steps(child.cancellation(), vec![tool("read_file"), done()]);

        let outcome = run_child(child, &mut work, &live).expect("finished");
        assert_eq!(outcome.summary, "wrote the notes");
        assert_eq!(outcome.steps, 1);
        assert_eq!(outcome.feedback_applied, 0);
        assert_eq!(outcome.kind, ChildKind::Named("runner".to_owned()));
        assert!(!child.is_working());
        assert!(child.stop_reason().is_none());

        // A child that has finished accepts no further work.
        let err = run_child(child, &mut work, &live).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidState);
    }

    #[test]
    fn a_cancelled_child_stops_before_its_next_step() {
        let mut registry = Registry::from_limits(&limits());
        let id = registry
            .create(
                SessionId::generate(),
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        let live = LiveParent::new(&["read_file"]);
        let child = registry.get_mut(&id).expect("present");
        child.cancellation().cancel();
        let mut work = ScriptedChild::with_steps(child.cancellation(), vec![tool("read_file")]);

        let err = run_child(child, &mut work, &live).expect_err("cancelled");
        assert_eq!(err.code(), ErrorCode::Cancelled);
        assert!(work.executed.is_empty());
        assert!(child.stop_reason().expect("stopped").contains("cancelled"));
    }

    // Registry.

    #[test]
    fn a_registry_starts_empty() {
        let registry = Registry::from_limits(&limits());
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.children_of(&SessionId::generate()).is_empty());
        assert!(registry.discoverable().is_empty());
    }

    #[test]
    fn children_are_registered_per_parent() {
        let mut registry = Registry::from_limits(&limits());
        let first = SessionId::generate();
        let second = SessionId::generate();
        let a = registry
            .create(
                first,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        let b = registry
            .create(
                first,
                ChildKind::Named("runner".to_owned()),
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        let c = registry
            .create(
                second,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");

        assert_eq!(registry.children_of(&first), [a, b]);
        assert_eq!(registry.children_of(&second), [c]);
        assert_eq!(registry.get(&a).expect("present").parent(), first);
        assert_eq!(registry.len(), 3);
        assert_ne!(a.session, b.session);

        let named = registry.named(&first, "runner").expect("named present");
        assert_eq!(named.id(), b);
        assert!(registry.named(&first, "absent").is_none());
    }

    #[test]
    fn the_child_bound_is_per_parent_and_reports_limit_exceeded() {
        let mut budgets = BudgetSet::new();
        set_limit(&mut budgets, LimitName::SubagentChildren, 2);
        let mut registry = Registry::from_limits(&budgets);
        assert_eq!(registry.capacity(), 2);

        let first = SessionId::generate();
        let second = SessionId::generate();
        registry
            .create(
                first,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        registry
            .create(
                first,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        let err = registry
            .create(
                first,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::LimitExceeded);

        // The bound is per parent, so another parent is unaffected.
        registry
            .create(
                second,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");

        // Removing a child frees its slot.
        let held = registry.children_of(&first).to_vec();
        registry.remove(&held[0]).expect("removed");
        registry
            .create(
                first,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
    }

    #[test]
    fn a_duplicate_or_empty_name_is_refused() {
        let mut registry = Registry::from_limits(&limits());
        let parent = SessionId::generate();
        registry
            .create(
                parent,
                ChildKind::Named("runner".to_owned()),
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");

        let err = registry
            .create(
                parent,
                ChildKind::Named("runner".to_owned()),
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::AlreadyExists);

        let err = registry
            .create(
                parent,
                ChildKind::Named(" ".to_owned()),
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);

        // Another parent may reuse the name.
        registry
            .create(
                SessionId::generate(),
                ChildKind::Named("runner".to_owned()),
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
    }

    #[test]
    fn removing_a_child_returns_it_and_reports_an_unknown_one() {
        let mut registry = Registry::from_limits(&limits());
        let parent = SessionId::generate();
        let id = registry
            .create(
                parent,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");

        let removed = registry.remove(&id).expect("removed");
        assert_eq!(removed.id(), id);
        assert!(registry.is_empty());
        assert!(registry.children_of(&parent).is_empty());

        let err = registry.remove(&id).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
        let err = registry.get(&id).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
        assert!(err.message().contains(id.session.as_str()));
    }

    #[test]
    fn a_child_is_absent_from_discovery_and_from_the_ordinary_lookup() {
        let mut registry = Registry::from_limits(&limits());
        let parent = SessionId::generate();
        let one = registry
            .create(
                parent,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        let two = registry
            .create(
                parent,
                ChildKind::Named("runner".to_owned()),
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        assert_eq!(registry.len(), 2);

        // Discovery never shows a child, however many are registered.
        assert!(registry.discoverable().is_empty());

        // The ordinary lookup is keyed by session, and a child's own session is
        // not a key, so it resolves to nothing. Only a `ChildId` reaches a
        // child, and a `ChildId` is only ever handed back by `create`.
        for id in [one, two] {
            let child_session = registry.get(&id).expect("present").id().session;
            assert!(registry.children_of(&child_session).is_empty());
            assert!(registry.named(&child_session, "runner").is_none());
        }
        assert_eq!(registry.children_of(&parent), [one, two]);
    }

    // Feedback.

    #[test]
    fn feedback_is_delivered_at_a_boundary() {
        let feedback = Feedback::new(4);
        assert!(feedback.is_empty());
        assert_eq!(feedback.depth(), 4);

        feedback.queue("try the other branch").expect("accepted");
        assert_eq!(feedback.len(), 1);

        let drained = feedback.drain(Boundary::Model);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].text, "try the other branch");
        assert_eq!(drained[0].drained_at, Some(Boundary::Model));
        assert!(feedback.is_empty());
    }

    #[test]
    fn feedback_past_its_depth_reports_limit_exceeded() {
        let mut budgets = BudgetSet::new();
        set_limit(&mut budgets, LimitName::SteeringQueueDepth, 2);
        let feedback = Feedback::from_limits(&budgets);
        feedback.queue("one").expect("accepted");
        feedback.queue("two").expect("accepted");
        let err = feedback.queue("three").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::LimitExceeded);

        feedback.drain(Boundary::Model);
        feedback.queue("three").expect("accepted");
    }

    #[test]
    fn a_zero_child_bound_still_admits_one_child() {
        let mut budgets = BudgetSet::new();
        set_limit(&mut budgets, LimitName::SubagentChildren, 0);
        let mut registry = Registry::from_limits(&budgets);
        assert_eq!(registry.capacity(), 1);

        let parent = SessionId::generate();
        registry
            .create(
                parent,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        let err = registry
            .create(
                parent,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::LimitExceeded);
    }

    #[test]
    fn a_zero_feedback_depth_still_accepts_one_message() {
        let feedback = Feedback::new(0);
        assert_eq!(feedback.depth(), 1);
        feedback.queue("one").expect("accepted");
        let err = feedback.queue("two").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::LimitExceeded);
    }

    #[test]
    fn feedback_is_sized_from_the_limits() {
        let mut budgets = BudgetSet::new();
        set_limit(&mut budgets, LimitName::SteeringQueueDepth, 3);
        let feedback = Feedback::from_limits(&budgets);
        assert_eq!(feedback.depth(), 3);
    }

    #[test]
    fn feedback_queued_mid_step_does_not_cancel_work_and_arrives_at_the_next_boundary() {
        let mut registry = Registry::from_limits(&limits());
        let id = registry
            .create(
                SessionId::generate(),
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        let live = LiveParent::new(&["read_file"]);
        let child = registry.get_mut(&id).expect("present");

        let mut work =
            ScriptedChild::with_steps(child.cancellation(), vec![tool("read_file"), done()]);
        // The parent submits feedback while the child's call is in flight.
        let handle = child.feedback().clone();
        work.mid_step = Some(Box::new(move || {
            handle.queue("also check the changelog").expect("accepted");
        }));

        let outcome = run_child(child, &mut work, &live).expect("finished");
        assert_eq!(work.executed, ["read_file"], "in-flight work was dropped");
        assert_eq!(work.cancelled_during_step, [false]);
        assert!(!child.cancellation().is_cancelled());
        assert_eq!(work.accepted, ["also check the changelog"]);
        assert_eq!(outcome.feedback_applied, 1);
        assert_eq!(outcome.steps, 1);
    }

    #[test]
    fn the_registry_queues_feedback_for_the_child_it_names() {
        let mut registry = Registry::from_limits(&limits());
        let parent = SessionId::generate();
        let first = registry
            .create(
                parent,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        let second = registry
            .create(
                parent,
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");

        registry.queue(&first, "for the first").expect("accepted");
        assert_eq!(registry.get(&first).expect("present").feedback().len(), 1);
        assert!(
            registry
                .get(&second)
                .expect("present")
                .feedback()
                .is_empty()
        );

        let unknown = ChildId {
            session: SessionId::generate(),
            seq: 99,
        };
        let err = registry.queue(&unknown, "for nobody").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }

    #[test]
    fn feedback_for_a_finished_child_is_refused() {
        let mut registry = Registry::from_limits(&limits());
        let id = registry
            .create(
                SessionId::generate(),
                ChildKind::OneOff,
                ChildBrief::default(),
                &authority(&["read_file"]),
            )
            .expect("created");
        let live = LiveParent::new(&["read_file"]);
        let child = registry.get_mut(&id).expect("present");
        let handle = child.feedback().clone();
        handle.queue("before").expect("accepted");

        let mut work = ScriptedChild::with_steps(child.cancellation(), vec![done()]);
        run_child(child, &mut work, &live).expect("finished");

        assert!(!child.is_working());
        assert!(!handle.is_open());
        let err = handle.queue("after").expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidState);
    }

    #[test]
    fn a_queued_feedback_message_is_scoped_to_one_child() {
        let first = Feedback::new(4);
        let second = Feedback::new(4);
        first.queue("for the first").expect("accepted");
        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
    }
}
