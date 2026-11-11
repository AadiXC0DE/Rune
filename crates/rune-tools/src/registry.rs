//! The tool registry.

use std::collections::BTreeMap;

use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::tool::ToolSpec;

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput, model_spec};

/// The set of tools available to the model.
#[derive(Default)]
pub struct Registry {
    tools: BTreeMap<String, Box<dyn Tool>>,
    /// Whether a tool is offered, where it differs from the tool's own default.
    advertised: BTreeMap<String, bool>,
    /// Replacement descriptions applied to the advertised schemas.
    text: crate::tool_text::ToolText,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("names", &self.tools.keys().collect::<Vec<_>>())
            .field("advertised", &self.advertised)
            .field("text", &self.text.keys().collect::<Vec<_>>())
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

    /// Adds a tool, or replaces the one registered under its name.
    ///
    /// A host uses it to swap a built-in for one built with what it knows only
    /// after the registry exists, keeping any advertisement override.
    pub fn replace(&mut self, tool: Box<dyn Tool>) {
        let _ = model_spec(tool.as_ref());
        self.tools.insert(tool.name().to_owned(), tool);
    }

    /// Returns a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(AsRef::as_ref)
    }

    /// Offers or hides a registered tool, overriding its own default. A hidden
    /// tool stays registered and callable.
    pub fn set_advertised(&mut self, name: &str, advertised: bool) {
        self.advertised.insert(name.to_owned(), advertised);
    }

    /// Replaces advertised descriptions with `text`, after checking that every
    /// tool and property it names exists.
    ///
    /// # Errors
    ///
    /// Returns `invalid_field` naming the first tool or property that does not.
    pub fn set_tool_text(&mut self, text: crate::tool_text::ToolText) -> Result<()> {
        use crate::tool_text::VARIABLE;
        for (name, entry) in &text {
            let Some(tool) = self.get(name) else {
                return Err(RuneError::invalid_field(
                    VARIABLE,
                    format!("{VARIABLE} names `{name}`, which is not a registered tool"),
                ));
            };
            let schema = tool.input_schema();
            for property in entry.properties.keys() {
                if schema["properties"].get(property).is_none() {
                    return Err(RuneError::invalid_field(
                        VARIABLE,
                        format!(
                            "{VARIABLE} names property `{property}` of `{name}`, which the \
                             schema does not have"
                        ),
                    ));
                }
            }
        }
        self.text = text;
        Ok(())
    }

    /// The model-facing schema of a tool, with any replacement text applied.
    fn spec_of(&self, name: &str, tool: &dyn Tool) -> ToolSpec {
        let mut spec = model_spec(tool);
        let Some(entry) = self.text.get(name) else {
            return spec;
        };
        if let Some(description) = &entry.description {
            spec.description = crate::contract::bounded_description(description);
        }
        for (property, text) in &entry.properties {
            if let Some(object) = spec.input_schema["properties"]
                .get_mut(property)
                .and_then(serde_json::Value::as_object_mut)
            {
                if text.is_empty() {
                    object.remove("description");
                } else {
                    object.insert("description".to_owned(), text.clone().into());
                }
            }
        }
        spec
    }

    /// Whether the named tool is offered to the model.
    #[must_use]
    pub fn is_advertised(&self, name: &str) -> bool {
        self.get(name).is_some_and(|tool| {
            self.advertised
                .get(name)
                .copied()
                .unwrap_or_else(|| tool.is_advertised())
        })
    }

    /// Ends whatever every tool keeps running between calls.
    pub fn shutdown(&self) {
        for tool in self.tools.values() {
            tool.shutdown();
        }
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
            .filter(|name| self.is_advertised(name))
            .filter_map(|name| self.get(name).map(|tool| self.spec_of(name, tool)))
            .collect()
    }

    /// Returns the schemas for every registered tool.
    #[must_use]
    pub fn all_schemas(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .filter(|(name, _)| self.is_advertised(name))
            .map(|(name, tool)| self.spec_of(name, tool.as_ref()))
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
    fn a_replaced_tool_keeps_its_name_and_advertisement_override() {
        let mut registry = Registry::new();
        let failing = |error: RuneError| FailingTool {
            error,
            fail_validation: false,
        };
        registry
            .insert(Box::new(failing(RuneError::new(
                ErrorCode::NotFound,
                "first",
            ))))
            .expect("register");
        registry.set_advertised("fixture", false);
        registry.replace(Box::new(failing(RuneError::new(
            ErrorCode::NotFound,
            "second",
        ))));
        assert_eq!(registry.names(), ["fixture"]);
        assert!(!registry.is_advertised("fixture"));
        let output = registry
            .call(
                "fixture",
                &serde_json::json!({}),
                &ExecutionContext::new("/fixture".into()),
            )
            .expect("a tool result");
        assert!(output.text.contains("second"), "{}", output.text);
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

    fn shell_spec(registry: &Registry) -> ToolSpec {
        registry
            .all_schemas()
            .into_iter()
            .find(|spec| spec.name == "shell")
            .expect("shell offered")
    }

    fn text(json: &str) -> crate::tool_text::ToolText {
        serde_json::from_str(json).expect("tool text")
    }

    #[test]
    fn tool_text_replaces_a_description_and_a_property() {
        let mut registry = crate::inventory::builtin_default().expect("built");
        registry
            .set_tool_text(text(
                r#"{"shell": {"description": "Run a command.", "properties": {"cwd": "Where."}}}"#,
            ))
            .expect("accepted");
        let spec = shell_spec(&registry);
        assert_eq!(spec.description, "Run a command.");
        assert_eq!(
            spec.input_schema["properties"]["cwd"]["description"],
            "Where."
        );
        let named = registry.schemas(&["shell"]);
        assert_eq!(named[0].description, "Run a command.");
    }

    #[test]
    fn tool_text_naming_an_unknown_tool_or_property_is_refused() {
        let mut registry = crate::inventory::builtin_default().expect("built");
        let error = registry
            .set_tool_text(text(r#"{"teleport": {"description": "x"}}"#))
            .expect_err("unknown tool");
        assert_eq!(
            error.message(),
            "RUNE_TOOL_TEXT names `teleport`, which is not a registered tool"
        );
        let error = registry
            .set_tool_text(text(r#"{"shell": {"properties": {"colour": "x"}}}"#))
            .expect_err("unknown property");
        assert_eq!(
            error.message(),
            "RUNE_TOOL_TEXT names property `colour` of `shell`, which the schema does not have"
        );
        // A refused text changes nothing.
        assert_ne!(shell_spec(&registry).description, "x");
    }

    #[test]
    fn an_empty_property_text_removes_the_description() {
        let mut registry = crate::inventory::builtin_default().expect("built");
        registry
            .set_tool_text(text(r#"{"shell": {"properties": {"cwd": ""}}}"#))
            .expect("accepted");
        let spec = shell_spec(&registry);
        assert!(
            spec.input_schema["properties"]["cwd"]
                .get("description")
                .is_none()
        );
        assert_eq!(spec.input_schema["properties"]["cwd"]["type"], "string");
    }
}
