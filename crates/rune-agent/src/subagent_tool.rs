//! Delegating work to a child agent.
//!
//! A child runs with the authority its parent held at the moment it was
//! admitted, captured once. If any part of that authority changes while the
//! child is running, the child is refused rather than continuing with rights it
//! no longer has, which is what keeps delegation from widening what the parent
//! was permitted to do.

use std::sync::{Arc, Mutex};

use crate::subagent::{
    Admission, AdmissionVerdict, Authority, ChildId, ChildKind, SubagentRequest,
};
use rune_core::error::{ErrorCode, Result, RuneError};

use rune_tools::contract::{Activity, ExecutionContext, Tool, ToolOutput};

/// What a delegation produced.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChildOutcome {
    /// Identifier of the child that ran.
    pub id: ChildId,
    /// Display form of that identifier.
    pub label: String,
    /// Its name, when it has one.
    pub name: Option<String>,
    /// What it reported back.
    pub summary: String,
}

/// Runs a child agent.
///
/// The trait exists so the tool can be driven by a test without a model, and by
/// a host that routes delegation its own way.
pub trait Delegate: Send + Sync {
    /// Runs one request. The admission is the authority the child may use.
    fn delegate(&self, request: &SubagentRequest, admission: &Admission) -> Result<ChildOutcome>;

    /// Queues a message to a running child, without cancelling its work.
    fn message(&self, _name: &str, _text: &str) -> Result<ChildOutcome> {
        Err(RuneError::new(
            ErrorCode::Unsupported,
            "this host does not address running children",
        ))
    }
}

/// A delegate that refuses everything, for a host with no child support.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unsupported;

impl Delegate for Unsupported {
    fn delegate(&self, _request: &SubagentRequest, _admission: &Admission) -> Result<ChildOutcome> {
        Err(RuneError::new(
            ErrorCode::Unsupported,
            "delegation is not available in this run",
        )
        .with_hint("run with a host that provides child agents"))
    }
}

/// State the parent holds between delegations.
#[derive(Debug)]
struct Parent {
    authority: Authority,
    admission: Admission,
}

/// Returns every root a context may reach.
fn roots_of(context: &ExecutionContext) -> Vec<camino::Utf8PathBuf> {
    let mut roots = vec![context.workspace.clone()];
    roots.extend(context.additional_roots.iter().cloned());
    roots
}

/// The tool.
pub struct Subagent {
    delegate: Arc<dyn Delegate>,
    parent: Parent,
    /// Names of children already created, so a repeated name is reported rather
    /// than silently starting a second one.
    started: Mutex<Vec<String>>,
}

impl std::fmt::Debug for Subagent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subagent").finish_non_exhaustive()
    }
}

impl Subagent {
    /// Builds the tool from the authority the parent currently holds.
    #[must_use]
    pub fn new(delegate: Arc<dyn Delegate>, authority: Authority) -> Self {
        let admission = Admission::capture(&authority);
        Self {
            delegate,
            parent: Parent {
                authority,
                admission,
            },
            started: Mutex::new(Vec::new()),
        }
    }

    /// Builds the tool for a host that cannot delegate.
    #[must_use]
    pub fn unsupported(authority: Authority) -> Self {
        Self::new(Arc::new(Unsupported), authority)
    }

    /// Re-checks the parent's authority before a child acts.
    ///
    /// A child admitted under one authority and continued under another would be
    /// running with rights it was never granted, so the difference is a refusal
    /// rather than a warning.
    fn recheck(&self, current: &Authority) -> Result<()> {
        let now = Admission::capture(current);
        match self.parent.admission.permits(&now, "subagent") {
            AdmissionVerdict::Permitted => Ok(()),
            verdict @ AdmissionVerdict::Refused { .. } => verdict.into_result("subagent"),
        }
    }
}

impl Tool for Subagent {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn description(&self) -> &'static str {
        "Delegate one task to a child agent, or send a message to a child already \
         running. The child inherits this session's permissions and cannot use \
         authority this session does not have."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["run", "message"],
                    "description": "Create a child, or address one already running."
                },
                "task": {
                    "type": "string",
                    "description": "What the child should do. Required for `run`."
                },
                "agent": {
                    "type": "string",
                    "description": "Agent the child runs as, or the name of a child to address."
                },
                "message": {
                    "type": "string",
                    "description": "Text for a running child. Required for `message`."
                },
                "instructions": {
                    "type": "string",
                    "description": "Instructions for a new child, replacing the default brief."
                },
                "model": {
                    "type": "string",
                    "description": "Model for a new child. Applied only at creation."
                },
                "effort": {
                    "type": "string",
                    "description": "Reasoning effort for a new child. Applied only at creation."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Delegate
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        let request = SubagentRequest::decode(arguments)?;

        // The authority is re-derived from the context of this call rather than
        // trusted from construction time, so a change between calls is caught
        // instead of the child continuing with rights it no longer has.
        let mut current = self.parent.authority.clone();
        current.workspace.clone_from(&context.workspace);
        current.roots = roots_of(context);
        if let Err(err) = self.recheck(&current) {
            return Ok(ToolOutput::failure(format!(
                "the child was not started: {}",
                err.message()
            )));
        }

        let outcome = match request.action {
            crate::subagent::SubagentAction::Message => {
                let Some(name) = request.agent.as_deref() else {
                    return Err(
                        RuneError::missing_field("agent").with_hint("name the child to address")
                    );
                };
                let Some(text) = request.message.as_deref() else {
                    return Err(RuneError::missing_field("message"));
                };
                self.delegate.message(name, text)
            }
            crate::subagent::SubagentAction::Run => {
                let mut started = self.started.lock().map_err(|_| {
                    RuneError::new(ErrorCode::Internal, "the child list lock was poisoned")
                })?;
                if let Some(ChildKind::Named(name)) = request.child_kind()
                    && started.iter().any(|label| label.as_str() == name)
                {
                    return Ok(ToolOutput::failure(format!(
                        "a child named `{name}` is already running"
                    )));
                }
                let outcome = self.delegate.delegate(&request, &self.parent.admission);
                if let Ok(outcome) = &outcome
                    && let Some(name) = outcome.name.clone()
                {
                    started.push(name);
                }
                outcome
            }
        };

        match outcome {
            Ok(outcome) => {
                let name = outcome
                    .name
                    .as_deref()
                    .map_or(String::new(), |name| format!(" ({name})"));
                Ok(ToolOutput::success(format!(
                    "child {}{name}: {}",
                    outcome.label, outcome.summary
                )))
            }
            // A delegation that could not run is a tool failure rather than an
            // abort, so the parent can decide what to do next.
            Err(err) => Ok(ToolOutput::failure(format!(
                "delegation failed: {}",
                err.message()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use camino::Utf8PathBuf;

    /// A delegate that records what it was asked to do.
    struct Recording {
        seen: Mutex<Vec<String>>,
    }

    impl Recording {
        fn new() -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.seen.lock().expect("lock").clone()
        }
    }

    impl Delegate for Recording {
        fn delegate(
            &self,
            request: &SubagentRequest,
            _admission: &Admission,
        ) -> Result<ChildOutcome> {
            self.seen
                .lock()
                .expect("lock")
                .push(request.task.clone().unwrap_or_default());
            Ok(ChildOutcome {
                id: child_id(),
                label: "child-1".to_owned(),
                name: request.child_kind().and_then(|kind| match kind {
                    ChildKind::Named(name) => Some(name.clone()),
                    ChildKind::OneOff => None,
                }),
                summary: "did the work".to_owned(),
            })
        }

        fn message(&self, name: &str, text: &str) -> Result<ChildOutcome> {
            self.seen
                .lock()
                .expect("lock")
                .push(format!("{name}: {text}"));
            Ok(ChildOutcome {
                id: child_id(),
                label: "child-1".to_owned(),
                name: Some(name.to_owned()),
                summary: "delivered".to_owned(),
            })
        }
    }

    fn child_id() -> ChildId {
        ChildId {
            session: rune_core::id::SessionId::generate(),
            seq: 1,
        }
    }

    fn authority() -> Authority {
        Authority {
            mode: rune_core::config::PermissionMode::Auto,
            rules: rune_policy::rules::RuleSet::new(),
            workspace: Utf8PathBuf::from("/w"),
            // The workspace is a root, as it is for a real authority.
            roots: vec![Utf8PathBuf::from("/w")],
            tools: vec!["subagent".to_owned()],
            mcp_view: None,
            generation: 0,
        }
    }

    fn context() -> ExecutionContext {
        ExecutionContext::new(Utf8PathBuf::from("/w"))
    }

    fn tool(delegate: Arc<dyn Delegate>) -> Subagent {
        Subagent::new(delegate, authority())
    }

    /// Builds a tool whose delegate the test also holds.
    #[test]
    fn the_tool_reports_its_identity() {
        let tool = tool(Arc::new(Unsupported));
        assert_eq!(tool.name(), "subagent");
        assert_eq!(tool.activity(), Activity::Delegate);
    }

    #[test]
    fn an_unsupported_host_refuses_rather_than_pretending() {
        let tool = Subagent::unsupported(authority());
        let output = tool
            .call(
                &serde_json::json!({ "action": "run", "task": "do it" }),
                &context(),
            )
            .expect("a typed failure");
        assert!(output.is_error);
        assert!(output.text.contains("delegation"), "{}", output.text);
    }

    #[test]
    fn a_run_delegates_the_task() {
        let delegate = Arc::new(Recording::new());
        let handle: Arc<dyn Delegate> = delegate.clone();
        let tool = tool(handle);
        let output = tool
            .call(
                &serde_json::json!({ "action": "run", "task": "inspect the parser" }),
                &context(),
            )
            .expect("delegated");
        assert!(!output.is_error, "{}", output.text);
        assert_eq!(delegate.calls(), ["inspect the parser"]);
    }

    #[test]
    fn a_message_reaches_a_running_child() {
        let delegate = Arc::new(Recording::new());
        let handle: Arc<dyn Delegate> = delegate.clone();
        let tool = tool(handle);
        let output = tool
            .call(
                &serde_json::json!({
                    "action": "message",
                    "agent": "scout",
                    "message": "focus on the lexer",
                }),
                &context(),
            )
            .expect("messaged");
        assert!(!output.is_error, "{}", output.text);
        assert_eq!(delegate.calls(), ["scout: focus on the lexer"]);
    }

    #[test]
    fn a_message_without_a_name_or_text_is_refused() {
        let tool = tool(Arc::new(Recording::new()));
        let err = tool
            .call(
                &serde_json::json!({ "action": "message", "message": "hi" }),
                &context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::MissingField);
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let tool = tool(Arc::new(Recording::new()));
        let err = tool
            .call(
                &serde_json::json!({ "action": "run", "task": "t", "nope": 1 }),
                &context(),
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn the_same_named_child_is_not_started_twice() {
        let delegate = Arc::new(Recording::new());
        let handle: Arc<dyn Delegate> = delegate.clone();
        let tool = tool(handle);
        let arguments = serde_json::json!({
            "action": "run",
            "task": "first",
            "agent": "scout",
        });
        tool.call(&arguments, &context()).expect("first");
        let second = tool.call(&arguments, &context()).expect("reported");
        assert!(second.is_error, "{}", second.text);
        assert_eq!(delegate.calls().len(), 1, "the child ran twice");
    }

    #[test]
    fn a_child_is_refused_when_the_authority_changed() {
        // A child admitted under one workspace and continued under another would
        // be running with rights it was never granted.
        let tool = tool(Arc::new(Recording::new()));
        let elsewhere = ExecutionContext::new(Utf8PathBuf::from("/other"));
        let output = tool
            .call(
                &serde_json::json!({ "action": "run", "task": "do it" }),
                &elsewhere,
            )
            .expect("reported");
        assert!(output.is_error, "{}", output.text);
        assert!(
            output.text.contains("not started"),
            "the refusal does not say what happened: {}",
            output.text
        );
    }

    #[test]
    fn a_failing_delegate_is_a_tool_failure_not_an_abort() {
        struct Failing;
        impl Delegate for Failing {
            fn delegate(
                &self,
                _request: &SubagentRequest,
                _admission: &Admission,
            ) -> Result<ChildOutcome> {
                Err(RuneError::new(
                    ErrorCode::LimitExceeded,
                    "too many children",
                ))
            }
        }
        let tool = tool(Arc::new(Failing));
        let output = tool
            .call(
                &serde_json::json!({ "action": "run", "task": "do it" }),
                &context(),
            )
            .expect("reported");
        assert!(output.is_error);
        assert!(output.text.contains("too many children"), "{}", output.text);
    }

    #[test]
    fn a_cancelled_context_refuses_before_delegating() {
        let delegate = Arc::new(Recording::new());
        let handle: Arc<dyn Delegate> = delegate.clone();
        let tool = tool(handle);
        let context = context();
        context.cancellation().cancel();
        let err = tool
            .call(
                &serde_json::json!({ "action": "run", "task": "t" }),
                &context,
            )
            .expect_err("cancelled");
        assert_eq!(err.code(), ErrorCode::Cancelled);
        assert!(delegate.calls().is_empty(), "a cancelled call delegated");
    }
}
