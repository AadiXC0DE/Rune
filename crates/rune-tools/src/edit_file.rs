//! The `edit_file` tool.

use rune_core::error::{Result, RuneError};
use serde_json::json;

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput};
use crate::mutation::{self, Occurrence};

/// Replaces an exact run of text inside a file.
#[derive(Clone, Copy, Debug, Default)]
pub struct EditFile;

impl EditFile {
    /// Creates the tool.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replaces exact text inside an existing file and leaves every other byte untouched. The \
         text must occur exactly once unless `occurrence` names which match to replace or \
         `replace_all` replaces every match. Matching is case and whitespace sensitive, and line \
         endings are never normalized. Refuses identical old and new text, and refuses a change \
         if the file is modified between the read and the write."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to edit. A relative path resolves against the workspace root."
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace, including indentation and line endings."
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text, written verbatim."
                },
                "occurrence": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Which match to replace, counted from 1. Only needed when the text occurs more than once."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every match. Cannot be combined with `occurrence`."
                }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }

    fn activity(&self) -> Activity {
        Activity::Edit
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
        let tool = self.name();
        let raw = mutation::required_string(tool, arguments, "path")?;
        let old = mutation::required_string(tool, arguments, "old_string")?;
        let new = mutation::required_string(tool, arguments, "new_string")?;
        let occurrence = selector(tool, arguments)?;
        let path = mutation::resolve(context, raw)?;
        let prepared = mutation::prepare_edit(&path, old, new, occurrence)?;
        context.check_cancelled()?;
        let applied = mutation::apply(prepared)?;
        Ok(ToolOutput::success(format!(
            "Edited `{}` ({}, {})",
            applied.path(),
            count_phrase(applied.replacements()),
            mutation::detail(&applied)
        )))
    }
}

/// Reads the optional selector, defaulting to a single required match.
fn selector(tool: &str, arguments: &serde_json::Value) -> Result<Occurrence> {
    let replace_all = match arguments.get("replace_all") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(value)) => *value,
        Some(_) => {
            return Err(RuneError::invalid_field(
                format!("{tool} arguments.replace_all"),
                "expected a boolean",
            ));
        }
    };
    let index = match arguments.get("occurrence") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(number)) => {
            let raw = number.as_u64().ok_or_else(|| {
                RuneError::invalid_field(
                    format!("{tool} arguments.occurrence"),
                    "expected a positive integer",
                )
            })?;
            if raw == 0 {
                return Err(RuneError::invalid_field(
                    format!("{tool} arguments.occurrence"),
                    "is counted from 1",
                ));
            }
            Some(usize::try_from(raw).unwrap_or(usize::MAX))
        }
        Some(_) => {
            return Err(RuneError::invalid_field(
                format!("{tool} arguments.occurrence"),
                "expected a positive integer",
            ));
        }
    };
    match (replace_all, index) {
        (true, Some(_)) => Err(RuneError::invalid_field(
            format!("{tool} arguments.replace_all"),
            "`replace_all` and `occurrence` select differently, pass one of them",
        )),
        (true, None) => Ok(Occurrence::All),
        (false, Some(index)) => Ok(Occurrence::Index(index)),
        (false, None) => Ok(Occurrence::Unique),
    }
}

/// Describes how many matches an edit replaced.
fn count_phrase(count: usize) -> String {
    if count == 1 {
        "1 occurrence".to_owned()
    } else {
        format!("{count} occurrences")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use rune_core::error::ErrorCode;

    fn workspace() -> (tempfile::TempDir, ExecutionContext) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        (dir, ExecutionContext::new(path))
    }

    fn seed(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, body).expect("seed");
        path
    }

    #[test]
    fn the_schema_requires_the_three_arguments() {
        let schema = EditFile.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(required, ["path", "old_string", "new_string"]);
        assert_eq!(schema["properties"]["occurrence"]["minimum"], 1);
    }

    #[test]
    fn a_single_match_is_replaced_without_a_selector() {
        let (dir, context) = workspace();
        let path = seed(&dir, "one.txt", "alpha\nbeta\n");
        let output = EditFile
            .call(
                &json!({ "path": "one.txt", "old_string": "beta", "new_string": "BETA" }),
                &context,
            )
            .expect("call");
        assert!(!output.is_error);
        assert!(output.text.contains("1 occurrence"), "{}", output.text);
        assert!(output.text.contains("line 2"), "{}", output.text);
        assert_eq!(std::fs::read(&path).expect("read"), b"alpha\nBETA\n");
    }

    #[test]
    fn a_non_unique_match_is_refused_with_the_count() {
        let (dir, context) = workspace();
        let path = seed(&dir, "many.txt", "a b a b a\n");
        let err = EditFile
            .call(
                &json!({ "path": "many.txt", "old_string": "a", "new_string": "x" }),
                &context,
            )
            .expect_err("ambiguous");
        assert_eq!(err.code(), ErrorCode::AmbiguousMatch);
        assert!(err.message().contains('3'), "{}", err.message());
        assert_eq!(std::fs::read(&path).expect("read"), b"a b a b a\n");
    }

    #[test]
    fn replace_all_replaces_every_match_and_reports_the_count() {
        let (dir, context) = workspace();
        let path = seed(&dir, "many.txt", "a b a b a\n");
        let output = EditFile
            .call(
                &json!({
                    "path": "many.txt",
                    "old_string": "a",
                    "new_string": "x",
                    "replace_all": true
                }),
                &context,
            )
            .expect("call");
        assert!(output.text.contains("3 occurrences"), "{}", output.text);
        assert_eq!(std::fs::read(&path).expect("read"), b"x b x b x\n");
    }

    #[test]
    fn an_occurrence_selects_exactly_one_match() {
        let (dir, context) = workspace();
        let path = seed(&dir, "many.txt", "a b a b a\n");
        let output = EditFile
            .call(
                &json!({
                    "path": "many.txt",
                    "old_string": "a",
                    "new_string": "x",
                    "occurrence": 2
                }),
                &context,
            )
            .expect("call");
        assert!(output.text.contains("1 occurrence"), "{}", output.text);
        assert_eq!(std::fs::read(&path).expect("read"), b"a b x b a\n");
    }

    #[test]
    fn an_occurrence_past_the_end_names_the_count() {
        let (dir, context) = workspace();
        let path = seed(&dir, "many.txt", "a b a\n");
        let err = EditFile
            .call(
                &json!({
                    "path": "many.txt",
                    "old_string": "a",
                    "new_string": "x",
                    "occurrence": 9
                }),
                &context,
            )
            .expect_err("range");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.message().contains('2'), "{}", err.message());
        assert_eq!(std::fs::read(&path).expect("read"), b"a b a\n");
    }

    #[test]
    fn occurrence_zero_is_refused_before_any_read() {
        let (_dir, context) = workspace();
        let err = EditFile
            .call(
                &json!({
                    "path": "many.txt",
                    "old_string": "a",
                    "new_string": "x",
                    "occurrence": 0
                }),
                &context,
            )
            .expect_err("zero");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert!(err.field().expect("field").contains("occurrence"));
    }

    #[test]
    fn two_selectors_at_once_are_refused() {
        let (_dir, context) = workspace();
        let err = EditFile
            .call(
                &json!({
                    "path": "many.txt",
                    "old_string": "a",
                    "new_string": "x",
                    "occurrence": 1,
                    "replace_all": true
                }),
                &context,
            )
            .expect_err("both");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn a_wrongly_typed_selector_is_refused() {
        let (_dir, context) = workspace();
        let err = EditFile
            .call(
                &json!({
                    "path": "many.txt",
                    "old_string": "a",
                    "new_string": "x",
                    "occurrence": "first"
                }),
                &context,
            )
            .expect_err("type");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn a_no_op_edit_is_refused_rather_than_reported_as_success() {
        let (dir, context) = workspace();
        let path = seed(&dir, "one.txt", "alpha\n");
        let err = EditFile
            .call(
                &json!({ "path": "one.txt", "old_string": "alpha", "new_string": "alpha" }),
                &context,
            )
            .expect_err("no op");
        assert_eq!(err.code(), ErrorCode::InvalidField);
        assert_eq!(err.field(), Some("new_string"));
        assert_eq!(std::fs::read(&path).expect("read"), b"alpha\n");
    }

    #[test]
    fn text_that_does_not_appear_is_refused() {
        let (dir, context) = workspace();
        seed(&dir, "one.txt", "alpha\n");
        let err = EditFile
            .call(
                &json!({ "path": "one.txt", "old_string": "omega", "new_string": "x" }),
                &context,
            )
            .expect_err("missing");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }

    #[test]
    fn a_crlf_file_is_edited_without_normalization() {
        let (dir, context) = workspace();
        let path = seed(&dir, "dos.txt", "alpha\r\nbeta\r\ngamma\r\n");
        EditFile
            .call(
                &json!({ "path": "dos.txt", "old_string": "beta", "new_string": "BETA" }),
                &context,
            )
            .expect("call");
        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"alpha\r\nBETA\r\ngamma\r\n"
        );
    }

    #[test]
    fn a_cr_only_file_is_edited_without_normalization() {
        let (dir, context) = workspace();
        let path = seed(&dir, "cr.txt", "alpha\rbeta\rgamma");
        EditFile
            .call(
                &json!({ "path": "cr.txt", "old_string": "beta", "new_string": "BETA" }),
                &context,
            )
            .expect("call");
        assert_eq!(std::fs::read(&path).expect("read"), b"alpha\rBETA\rgamma");
    }

    #[test]
    fn a_relative_path_resolves_against_the_workspace() {
        let (dir, context) = workspace();
        let path = seed(&dir, "nested.txt", "alpha\n");
        EditFile
            .call(
                &json!({ "path": "nested.txt", "old_string": "alpha", "new_string": "beta" }),
                &context,
            )
            .expect("call");
        assert_eq!(std::fs::read(&path).expect("read"), b"beta\n");
    }

    #[test]
    fn a_missing_file_is_refused() {
        let (_dir, context) = workspace();
        let err = EditFile
            .call(
                &json!({ "path": "absent.txt", "old_string": "a", "new_string": "b" }),
                &context,
            )
            .expect_err("missing");
        assert_eq!(err.code(), ErrorCode::NotFound);
    }

    #[test]
    fn the_permission_target_is_the_supplied_path() {
        let target = EditFile.permission_target(&json!({ "path": "src/lib.rs" }));
        assert_eq!(target.as_deref(), Some("src/lib.rs"));
        assert_eq!(EditFile.activity(), Activity::Edit);
        assert!(!EditFile.is_read_only());
    }

    #[test]
    fn an_edit_that_would_change_nothing_is_refused_for_identical_text_only() {
        let (dir, context) = workspace();
        let path = seed(&dir, "one.txt", "alpha\n");
        let err = EditFile
            .call(
                &json!({ "path": "one.txt", "old_string": "", "new_string": "x" }),
                &context,
            )
            .expect_err("empty");
        assert_eq!(err.field(), Some("old_string"));
        assert_eq!(std::fs::read(&path).expect("read"), b"alpha\n");
    }
}
