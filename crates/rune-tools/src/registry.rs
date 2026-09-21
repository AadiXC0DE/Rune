//! The tool registry.

use std::collections::BTreeMap;

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_net::message::ToolSpec;

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
            return Ok(ToolOutput::failure(err.message().to_owned()));
        }

        match tool.call(arguments, context) {
            Ok(output) => Ok(output),
            // A tool that fails for an operational reason is also a tool error:
            // a missing file or a failed command is information the model needs.
            Err(err) if !matches!(err.code(), ErrorCode::Cancelled) => {
                Ok(ToolOutput::failure(err.message().to_owned()))
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
