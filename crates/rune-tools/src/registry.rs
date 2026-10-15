//! The tool registry.

use std::collections::BTreeMap;

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::tool::ToolSpec;

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput, model_spec};

/// The set of tools available to the model.
#[derive(Default)]
pub struct Registry {
    tools: BTreeMap<String, Box<dyn Tool>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("names", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Registry {
    /// Returns an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tool.
    ///
    /// Returns an error on a duplicate name, because two tools sharing a name
    /// would make a model call ambiguous.
    pub fn insert(&mut self, tool: Box<dyn Tool>) -> Result<()> {
        let name = tool.name().to_owned();
        if self.tools.contains_key(&name) {
            return Err(RuneError::new(
                ErrorCode::AlreadyExists,
                format!("two tools are registered as `{name}`"),
            ));
        }
        let _ = model_spec(tool.as_ref());
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Returns a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(AsRef::as_ref)
    }

    /// Returns every tool name, in a stable order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Returns the number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Returns true when no tool is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Returns true when a tool by this name exists.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Returns the model-facing schemas for the given names.
    ///
    /// An unknown name is skipped rather than failing, because the advertised
    /// set is filtered by policy and a filtered call may still be decoded.
    #[must_use]
    pub fn schemas(&self, names: &[&str]) -> Vec<ToolSpec> {
        names
            .iter()
            .filter_map(|name| self.get(name))
            .map(model_spec)
            .collect()
    }

    /// Returns the schemas for every registered tool.
    #[must_use]
    pub fn all_schemas(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|tool| model_spec(tool.as_ref()))
            .collect()
    }

    /// Returns the read-only tool names, in a stable order.
    #[must_use]
    pub fn read_only_names(&self) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|(_, tool)| tool.is_read_only())
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Executes a tool call.
    ///
    /// Policy is not consulted here: the caller resolves it first and passes a
    /// context. Keeping permission out of this path is what makes a tool unable
    /// to widen its own authority.
    pub fn call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        let tool = self.get(name).ok_or_else(|| {
            RuneError::new(ErrorCode::NotFound, format!("no tool named `{name}`"))
                .with_hint(format!("available tools are {}", self.names().join(", ")))
        })?;

        context.check_cancelled()?;

        if let Err(err) = tool.validate(arguments) {
            // A malformed call is returned to the model as a tool error so it
            // can correct itself rather than the turn ending.
            return Ok(ToolOutput::from_error(&err));
        }

        match tool.call(arguments, context) {
            Ok(output) => Ok(output),
            // A tool that fails for an operational reason is also a tool error:
            // a missing file or a failed command is information the model needs.
            Err(err) if !matches!(err.code(), ErrorCode::Cancelled) => {
                Ok(ToolOutput::from_error(&err))
            }
            Err(err) => Err(err),
        }
    }

    /// Returns the activity for a tool, when it is registered.
    #[must_use]
    pub fn activity(&self, name: &str) -> Option<Activity> {
        self.get(name).map(Tool::activity)
    }

    /// Returns the permission target for a call, when the tool names one.
    #[must_use]
    pub fn permission_target(&self, name: &str, arguments: &serde_json::Value) -> Option<String> {
        self.get(name)
            .and_then(|tool| tool.permission_target(arguments))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingTool {
        error: RuneError,
        fail_validation: bool,
    }

    impl Tool for FailingTool {
        fn name(&self) -> &'static str {
            "fixture"
        }

        fn description(&self) -> &'static str {
            "Return a fixture diagnostic."
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn activity(&self) -> Activity {
            Activity::Read
        }

        fn validate(&self, _arguments: &serde_json::Value) -> Result<()> {
            if self.fail_validation {
                Err(self.error.clone())
            } else {
                Ok(())
            }
        }

        fn call(
            &self,
            _arguments: &serde_json::Value,
            _context: &ExecutionContext,
        ) -> Result<ToolOutput> {
            assert!(!self.fail_validation, "invalid calls must not execute");
            Err(self.error.clone())
        }
    }

    #[test]
    fn validation_and_execution_failures_preserve_the_full_diagnostic() {
        for fail_validation in [false, true] {
            let error = if fail_validation {
                RuneError::invalid_field("path", "invalid path")
            } else {
                RuneError::new(ErrorCode::NotFound, "missing")
            }
            .with_hint("try other.txt")
            .with_observed("missing.txt")
            .with_invariant("fixture_path");
            let mut registry = Registry::new();
            registry
                .insert(Box::new(FailingTool {
                    error: error.clone(),
                    fail_validation,
                }))
                .expect("register");
            let output = registry
                .call(
                    "fixture",
                    &serde_json::json!({}),
                    &ExecutionContext::new("/fixture".into()),
                )
                .expect("a repairable failure is a tool result");
            assert!(output.is_error);
            assert_eq!(output.produced_bytes, output.text.len() as u64);
            let diagnostic: serde_json::Value =
                serde_json::from_str(&output.render()).expect("diagnostic JSON");
            assert_eq!(diagnostic["code"], error.code().as_str());
            assert_eq!(diagnostic["message"], error.message());
            assert_eq!(diagnostic["detail"]["hint"], "try other.txt");
            assert_eq!(diagnostic["detail"]["observed"], "missing.txt");
            assert_eq!(diagnostic["detail"]["invariant"], "fixture_path");
            if fail_validation {
                assert_eq!(diagnostic["detail"]["field"], "path");
            } else {
                assert!(diagnostic["detail"].get("field").is_none());
            }
        }
    }

    #[test]
    fn execution_cancellation_still_propagates_as_an_error() {
        let error = RuneError::new(ErrorCode::Cancelled, "cancelled fixture");
        let mut registry = Registry::new();
        registry
            .insert(Box::new(FailingTool {
                error: error.clone(),
                fail_validation: false,
            }))
            .expect("register");
        assert_eq!(
            registry
                .call(
                    "fixture",
                    &serde_json::json!({}),
                    &ExecutionContext::new("/fixture".into()),
                )
                .expect_err("cancellation must stop execution"),
            error
        );
    }
}
