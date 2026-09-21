//! The tool contract.
//!
//! A tool declares its schema, decodes arguments, and executes. It never checks
//! permissions: the agent consults the policy engine before calling, and hands
//! the tool an execution context that already carries the decision. That split
//! is what keeps a tool implementation from being able to widen its own
//! authority.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use rune_core::error::Result;
use serde::{Deserialize, Serialize};

/// What a tool does, used for presentation and for choosing a permission target.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// Reading a file or listing entries.
    Read,
    /// Listing or matching names.
    List,
    /// Writing or replacing a file.
    Write,
    /// Editing part of a file.
    Edit,
    /// Running a command.
    Execute,
    /// Searching content.
    Search,
    /// Reaching the network.
    Network,
    /// Delegating to another agent.
    Delegate,
    /// Asking the user something.
    Interact,
}

impl Activity {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::List => "list",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Execute => "execute",
            Self::Search => "search",
            Self::Network => "network",
            Self::Delegate => "delegate",
            Self::Interact => "interact",
        }
    }

    /// Returns true when the activity cannot change anything outside the process.
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::Read | Self::List | Self::Search)
    }

    /// Returns the present-participle label used while the tool runs.
    #[must_use]
    pub const fn running_label(self) -> &'static str {
        match self {
            Self::Read => "Reading",
            Self::List => "Matching",
            Self::Write => "Writing",
            Self::Edit => "Editing",
            Self::Execute => "Running",
            Self::Search => "Searching",
            Self::Network => "Fetching",
            Self::Delegate => "Delegating",
            Self::Interact => "Asking",
        }
    }

    /// Returns the past-tense label used once the tool finishes.
    #[must_use]
    pub const fn completed_label(self) -> &'static str {
        match self {
            Self::Read => "Read",
            Self::List => "Matched",
            Self::Write => "Wrote",
            Self::Edit => "Edited",
            Self::Execute => "Ran",
            Self::Search => "Searched",
            Self::Network => "Fetched",
            Self::Delegate => "Delegated",
            Self::Interact => "Asked",
        }
    }
}

/// The result of one tool call.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ToolOutput {
    /// Text returned to the model.
    pub text: String,
    /// Whether the tool reported a failure.
    pub is_error: bool,
    /// Bytes the tool produced before any truncation, for the retained-result
    /// accounting.
    pub produced_bytes: u64,
}

impl ToolOutput {
    /// Builds a successful result.
    #[must_use]
    pub fn success(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            produced_bytes: text.len() as u64,
            text,
            is_error: false,
        }
    }

    /// Builds a failed result.
    ///
    /// A tool failure is returned to the model rather than ending the turn, so
    /// the model can adapt. Only a policy denial or a cancellation stops the
    /// call outright.
    #[must_use]
    pub fn failure(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            produced_bytes: text.len() as u64,
            text,
            is_error: true,
        }
    }

    /// Returns the result as a model-visible string.
    #[must_use]
    pub fn render(&self) -> String {
        self.text.clone()
    }
}

/// What a tool may do while executing.
///
/// Carries the resolved decision rather than the rules, so a tool cannot
/// re-evaluate policy and reach a different answer.
#[derive(Clone, Debug)]
pub struct ExecutionContext {
    /// Primary workspace root.
    pub workspace: Utf8PathBuf,
    /// Additional roots the tool may reach.
    pub additional_roots: Vec<Utf8PathBuf>,
    /// Whether the tool was permitted to reach outside the roots.
    pub external_access: bool,
    /// Per-call byte budget for produced output.
    pub max_output_bytes: usize,
    /// Composite cancellation flag, checked between steps of long work.
    cancelled: Arc<AtomicBool>,
}

impl ExecutionContext {
    /// Builds a context.
    #[must_use]
    pub fn new(workspace: Utf8PathBuf) -> Self {
        Self {
            workspace,
            additional_roots: Vec::new(),
            external_access: false,
            max_output_bytes: 64 * 1024,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Adds an additional root.
    #[must_use]
    pub fn with_root(mut self, root: Utf8PathBuf) -> Self {
        self.additional_roots.push(root);
        self
    }

    /// Sets the external-access flag.
    #[must_use]
    pub const fn with_external_access(mut self, allowed: bool) -> Self {
        self.external_access = allowed;
        self
    }

    /// Sets the output byte cap.
    #[must_use]
    pub const fn with_output_cap(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Returns a cancellation handle for this context.
    #[must_use]
    pub fn cancellation(&self) -> Cancellation {
        Cancellation {
            flag: Arc::clone(&self.cancelled),
        }
    }

    /// Returns true when the call has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Returns every root a tool may reach, with the primary root first.
    #[must_use]
    pub fn roots(&self) -> Vec<&Utf8Path> {
        let mut roots = vec![self.workspace.as_path()];
        roots.extend(self.additional_roots.iter().map(Utf8PathBuf::as_path));
        roots
    }

    /// Returns the primary workspace root.
    #[must_use]
    pub fn workspace(&self) -> &Utf8Path {
        self.workspace.as_path()
    }

    /// Returns an error when the call has been cancelled.
    pub fn check_cancelled(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(rune_core::error::RuneError::new(
                rune_core::error::ErrorCode::Cancelled,
                "the tool call was cancelled",
            ));
        }
        Ok(())
    }
}

/// A handle that can cancel a running tool call.
#[derive(Clone, Debug)]
pub struct Cancellation {
    flag: Arc<AtomicBool>,
}

impl Cancellation {
    /// Requests cancellation.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Returns true when cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

/// One tool.
///
/// Implementations are pure with respect to policy: they receive a context and
/// return an output, and they decide nothing about whether they may run.
pub trait Tool: Send + Sync {
    /// Tool name, as advertised to the model.
    fn name(&self) -> &'static str;

    /// Description, bounded to 1024 bytes.
    fn description(&self) -> &'static str;

    /// JSON Schema for the arguments object.
    fn input_schema(&self) -> serde_json::Value;

    /// What the tool does.
    fn activity(&self) -> Activity;

    /// Returns the permission target for a set of arguments, when the tool
    /// operates on one named thing.
    ///
    /// This is what a rule pattern is matched against. A tool with no single
    /// target returns `None`, and the rule is matched against the tool name.
    fn permission_target(&self, _arguments: &serde_json::Value) -> Option<String> {
        None
    }

    /// Returns true when the tool cannot change anything.
    fn is_read_only(&self) -> bool {
        self.activity().is_read_only()
    }

    /// Executes the tool.
    fn call(&self, arguments: &serde_json::Value, context: &ExecutionContext)
    -> Result<ToolOutput>;

    /// Validates arguments before execution.
    ///
    /// A failure here is returned to the model as a tool error, so a malformed
    /// call teaches the model rather than ending the turn.
    fn validate(&self, arguments: &serde_json::Value) -> Result<()> {
        let schema = self.input_schema();
        let Some(required) = schema.get("required").and_then(serde_json::Value::as_array) else {
            return Ok(());
        };
        for name in required {
            let Some(name) = name.as_str() else { continue };
            if arguments.get(name).is_none() {
                return Err(rune_core::error::RuneError::missing_field(format!(
                    "{} arguments.{name}",
                    self.name()
                )));
            }
        }
        Ok(())
    }
}

/// Largest description a tool may advertise.
pub const MAX_DESCRIPTION_BYTES: usize = 1024;

/// Describes a tool for the model.
#[must_use]
pub fn model_spec(tool: &dyn Tool) -> rune_net::message::ToolSpec {
    let mut description = tool.description().to_owned();
    if description.len() > MAX_DESCRIPTION_BYTES {
        // Truncation is explicit rather than silent, because a description cut
        // mid-sentence without a marker looks like a bug in the tool.
        description.truncate(MAX_DESCRIPTION_BYTES.saturating_sub(16));
        description.push_str("... [truncated]");
    }
    rune_net::message::ToolSpec {
        name: tool.name().to_owned(),
        description,
        input_schema: tool.input_schema(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl Tool for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }

        fn description(&self) -> &'static str {
            "Returns its input unchanged."
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            })
        }

        fn activity(&self) -> Activity {
            Activity::Read
        }

        fn permission_target(&self, arguments: &serde_json::Value) -> Option<String> {
            arguments
                .get("text")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        }

        fn call(
            &self,
            arguments: &serde_json::Value,
            _context: &ExecutionContext,
        ) -> Result<ToolOutput> {
            let text = arguments
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            Ok(ToolOutput::success(text))
        }
    }

    fn context() -> ExecutionContext {
        ExecutionContext::new(Utf8PathBuf::from("/tmp/workspace"))
    }

    #[test]
    fn read_only_activities_are_identified() {
        assert!(Activity::Read.is_read_only());
        assert!(Activity::List.is_read_only());
        assert!(Activity::Search.is_read_only());
        assert!(!Activity::Write.is_read_only());
        assert!(!Activity::Edit.is_read_only());
        assert!(!Activity::Execute.is_read_only());
        assert!(!Activity::Network.is_read_only());
    }

    #[test]
    fn an_activity_that_reads_is_treated_as_read_only_by_default() {
        assert!(Echo.is_read_only());
    }

    #[test]
    fn every_activity_has_both_labels() {
        let activities = [
            Activity::Read,
            Activity::List,
            Activity::Write,
            Activity::Edit,
            Activity::Execute,
            Activity::Search,
            Activity::Network,
            Activity::Delegate,
            Activity::Interact,
        ];
        for activity in activities {
            assert!(!activity.running_label().is_empty(), "{activity:?}");
            assert!(!activity.completed_label().is_empty(), "{activity:?}");
            assert!(!activity.as_str().is_empty(), "{activity:?}");
        }
    }

    #[test]
    fn a_tool_executes_through_the_context() {
        let output = Echo
            .call(&serde_json::json!({ "text": "hello" }), &context())
            .expect("call");
        assert_eq!(output.text, "hello");
        assert!(!output.is_error);
    }

    #[test]
    fn a_tool_failure_is_a_result_not_an_error() {
        // The model must be able to see and react to a tool failure, so the
        // failure travels as an output rather than aborting the turn.
        let output = ToolOutput::failure("file not found");
        assert!(output.is_error);
        assert_eq!(output.text, "file not found");
    }

    #[test]
    fn validation_reports_a_missing_required_argument() {
        let err = Echo.validate(&serde_json::json!({})).expect_err("rejected");
        assert_eq!(err.code(), rune_core::error::ErrorCode::MissingField);
        assert!(err.message().contains("echo arguments.text"));
    }

    #[test]
    fn validation_passes_when_every_required_argument_is_present() {
        Echo.validate(&serde_json::json!({ "text": "x" }))
            .expect("valid");
    }

    #[test]
    fn a_tool_declares_the_target_a_rule_matches_against() {
        let target = Echo.permission_target(&serde_json::json!({ "text": "docs/readme.md" }));
        assert_eq!(target.as_deref(), Some("docs/readme.md"));
    }

    #[test]
    fn a_tool_without_a_target_declares_none() {
        struct Untargeted;
        impl Tool for Untargeted {
            fn name(&self) -> &'static str {
                "untargeted"
            }
            fn description(&self) -> &'static str {
                "Does something with no single target."
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object" })
            }
            fn activity(&self) -> Activity {
                Activity::Interact
            }
            fn call(
                &self,
                _arguments: &serde_json::Value,
                _context: &ExecutionContext,
            ) -> Result<ToolOutput> {
                Ok(ToolOutput::success("ok"))
            }
        }
        assert!(
            Untargeted
                .permission_target(&serde_json::json!({}))
                .is_none()
        );
    }

    #[test]
    fn the_context_exposes_the_workspace_first_among_its_roots() {
        let context = context().with_root(Utf8PathBuf::from("/tmp/shared"));
        let roots = context.roots();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], Utf8Path::new("/tmp/workspace"));
        assert_eq!(roots[1], Utf8Path::new("/tmp/shared"));
    }

    #[test]
    fn a_context_starts_without_external_access() {
        let context = context();
        assert!(!context.external_access);
        assert_eq!(context.roots().len(), 1);
    }

    #[test]
    fn cancellation_propagates_from_the_handle_to_the_context() {
        let context = context();
        assert!(!context.is_cancelled());
        let handle = context.cancellation();
        handle.cancel();
        assert!(context.is_cancelled());
        assert!(handle.is_cancelled());
    }

    #[test]
    fn a_cancelled_context_refuses_further_work() {
        let context = context();
        context.cancellation().cancel();
        let err = context.check_cancelled().expect_err("cancelled");
        assert_eq!(err.code(), rune_core::error::ErrorCode::Cancelled);
    }

    #[test]
    fn cancellation_is_shared_across_clones_of_the_handle() {
        let context = context();
        let first = context.cancellation();
        let second = first.clone();
        first.cancel();
        assert!(second.is_cancelled(), "a cloned handle lost the signal");
    }

    #[test]
    fn the_model_spec_carries_the_name_description_and_schema() {
        let spec = model_spec(&Echo);
        assert_eq!(spec.name, "echo");
        assert_eq!(spec.description, "Returns its input unchanged.");
        assert_eq!(spec.input_schema["required"][0], "text");
    }

    #[test]
    fn an_oversized_description_is_truncated_with_a_marker() {
        // The trait returns a static string, so the long description lives for
        // the process lifetime and the test can borrow it.
        struct Verbose {
            text: &'static str,
        }
        impl Tool for Verbose {
            fn name(&self) -> &'static str {
                "verbose"
            }
            fn description(&self) -> &'static str {
                self.text
            }
            fn input_schema(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object" })
            }
            fn activity(&self) -> Activity {
                Activity::Read
            }
            fn call(
                &self,
                _arguments: &serde_json::Value,
                _context: &ExecutionContext,
            ) -> Result<ToolOutput> {
                Ok(ToolOutput::success("ok"))
            }
        }

        let long: &'static str = Box::leak("x".repeat(4000).into_boxed_str());
        let spec = model_spec(&Verbose { text: long });
        assert!(
            spec.description.len() <= MAX_DESCRIPTION_BYTES,
            "description was {} bytes",
            spec.description.len()
        );
        assert!(
            spec.description.ends_with("[truncated]"),
            "truncation was not marked"
        );
    }

    #[test]
    fn the_output_records_how_many_bytes_it_produced() {
        let output = ToolOutput::success("abcd");
        assert_eq!(output.produced_bytes, 4);
        assert_eq!(output.render(), "abcd");
    }
}
