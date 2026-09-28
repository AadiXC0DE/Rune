//! The shell tool, as a browser tab can offer it.
//!
//! A tab cannot start a process, so a command runs in a small shell the page
//! implements over the same files the other tools read. The tool keeps the
//! binary's name and its `command` argument, so a model that learned the tool
//! calls it the same way, and the permission rules match the same target.

use std::fmt::Write as _;
use std::sync::Arc;

use rune_core::error::{Result, RuneError};
use rune_tools::contract::{Activity, ExecutionContext, Tool, ToolOutput};

use crate::bridge::Bridge;

/// Runs a command in the page's shell.
pub struct PageShell {
    bridge: Arc<dyn Bridge>,
}

impl std::fmt::Debug for PageShell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageShell").finish_non_exhaustive()
    }
}

impl PageShell {
    /// Builds the tool over a bridge.
    pub fn new(bridge: Arc<dyn Bridge>) -> Self {
        Self { bridge }
    }
}

impl Tool for PageShell {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Run a command in the workspace and return its output and exit status. This shell \
         offers file commands such as ls, cat, head, tail, wc, grep, find, and tree; it cannot \
         start a compiler or reach the network."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Command line to run."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Execute
    }

    fn permission_target(&self, arguments: &serde_json::Value) -> Option<String> {
        arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        let command = arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .ok_or_else(|| RuneError::invalid_field("command", "`command` is required"))?;
        let ran = self.bridge.run(command, context.workspace().as_str());
        let mut text = ran.output;
        if !text.ends_with('\n') && !text.is_empty() {
            text.push('\n');
        }
        let _ = write!(text, "[exit {}]", ran.exit_code);
        Ok(if ran.exit_code == 0 {
            ToolOutput::success(text)
        } else {
            ToolOutput::failure(text)
        })
    }
}
