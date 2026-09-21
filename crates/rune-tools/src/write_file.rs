//! The `write_file` tool.

use rune_core::error::Result;
use serde_json::json;

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput};
use crate::mutation;

/// Replaces a whole file, creating it when it does not exist.
#[derive(Clone, Copy, Debug, Default)]
pub struct WriteFile;

impl WriteFile {
    /// Creates the tool.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Writes a file, replacing its entire contents. Creates the file and any missing parent \
         directories. The content is written verbatim, so line endings are preserved exactly. \
         Refuses a change if the file is modified between the read and the write."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to write. A relative path resolves against the workspace root."
                },
                "content": {
                    "type": "string",
                    "description": "Complete new contents of the file."
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Write
    }

    fn permission_target(&self, arguments: &serde_json::Value) -> Option<String> {
        arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        let raw = mutation::required_string(self.name(), arguments, "path")?;
        let content = mutation::required_string(self.name(), arguments, "content")?;
        let path = mutation::resolve(context, raw)?;
        let prepared = mutation::prepare(&path, content)?;
        context.check_cancelled()?;
        let applied = mutation::apply(prepared)?;
        Ok(ToolOutput::success(format!(
            "Wrote `{}` ({})",
            applied.path(),
            mutation::detail(&applied)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn workspace() -> (tempfile::TempDir, ExecutionContext) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        (dir, ExecutionContext::new(path))
    }

    fn path_of(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
        dir.path().join(name)
    }

    #[test]
    fn the_schema_requires_a_path_and_content() {
        let schema = WriteFile.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(required, ["path", "content"]);
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn a_write_creates_the_file_and_reports_the_size() {
        let (dir, context) = workspace();
        let output = WriteFile
            .call(
                &json!({ "path": "notes/one.txt", "content": "hello\n" }),
                &context,
            )
            .expect("call");
        assert!(!output.is_error);
        assert!(output.text.contains("notes/one.txt"), "{}", output.text);
        assert!(output.text.contains("6 bytes"), "{}", output.text);
        assert_eq!(
            std::fs::read(path_of(&dir, "notes/one.txt")).expect("read"),
            b"hello\n"
        );
    }

    #[test]
    fn a_write_replaces_the_previous_contents_and_reports_the_delta() {
        let (dir, context) = workspace();
        std::fs::write(path_of(&dir, "one.txt"), "short\n").expect("seed");
        let output = WriteFile
            .call(
                &json!({ "path": "one.txt", "content": "a much longer body\n" }),
                &context,
            )
            .expect("call");
        assert!(output.text.contains("+13"), "{}", output.text);
        assert_eq!(
            std::fs::read(path_of(&dir, "one.txt")).expect("read"),
            b"a much longer body\n"
        );
    }

    #[test]
    fn a_write_keeps_crlf_endings() {
        let (dir, context) = workspace();
        WriteFile
            .call(
                &json!({ "path": "dos.txt", "content": "a\r\nb\r\n" }),
                &context,
            )
            .expect("call");
        assert_eq!(
            std::fs::read(path_of(&dir, "dos.txt")).expect("read"),
            b"a\r\nb\r\n"
        );
    }

    #[test]
    fn a_missing_argument_is_named_in_the_error() {
        let (_dir, context) = workspace();
        let err = WriteFile
            .call(&json!({ "path": "one.txt" }), &context)
            .expect_err("missing");
        assert_eq!(err.code(), rune_core::error::ErrorCode::MissingField);
        assert!(err.message().contains("write_file arguments.content"));
        WriteFile
            .validate(&json!({ "path": "one.txt" }))
            .expect_err("invalid");
    }

    #[test]
    fn a_non_string_argument_is_refused() {
        let (_dir, context) = workspace();
        let err = WriteFile
            .call(&json!({ "path": "one.txt", "content": 7 }), &context)
            .expect_err("invalid");
        assert_eq!(err.code(), rune_core::error::ErrorCode::InvalidField);
    }

    #[test]
    fn a_path_outside_the_workspace_absolute_form_is_used_as_written() {
        let (dir, context) = workspace();
        let other = dir.path().join("elsewhere.txt");
        let raw = other.to_str().expect("utf8").to_owned();
        WriteFile
            .call(&json!({ "path": raw, "content": "x" }), &context)
            .expect("call");
        assert_eq!(std::fs::read(&other).expect("read"), b"x");
    }

    #[test]
    fn the_permission_target_is_the_supplied_path() {
        let target = WriteFile.permission_target(&json!({ "path": "src/lib.rs" }));
        assert_eq!(target.as_deref(), Some("src/lib.rs"));
        assert_eq!(WriteFile.activity(), Activity::Write);
        assert!(!WriteFile.is_read_only());
    }

    #[test]
    fn a_cancelled_call_does_nothing() {
        let (dir, context) = workspace();
        context.cancellation().cancel();
        let err = WriteFile
            .call(&json!({ "path": "one.txt", "content": "x" }), &context)
            .expect_err("cancelled");
        assert_eq!(err.code(), rune_core::error::ErrorCode::Cancelled);
        assert!(!path_of(&dir, "one.txt").exists());
    }
}
