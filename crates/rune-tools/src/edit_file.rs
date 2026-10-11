//! The `edit_file` tool.

use rune_core::error::{Result, RuneError};
use serde_json::json;

use crate::contract::{Activity, ExecutionContext, Tool, ToolOutput, validate_closed_arguments};
use crate::mutation::{self, LiteralEdit, Occurrence};
use crate::workspace::resolve;

/// Replaces one or more exact runs of text atomically inside a file.
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
         if the file is modified between the read and the write. For multiple hunks, pass `edits` \
         instead of the top-level replacement fields. Hunks run in order against earlier \
         in-memory results; all must succeed before one atomic write."
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
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": mutation::MAX_EDIT_HUNKS,
                    "description": "Ordered literal hunks, matched against earlier hunks' results. Every hunk must succeed before the file is written. Cannot be combined with top-level replacement fields.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": { "type": "string", "minLength": 1 },
                            "new_string": { "type": "string" },
                            "occurrence": { "type": "integer", "minimum": 1 },
                            "replace_all": { "type": "boolean" }
                        },
                        "required": ["old_string", "new_string"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path"],
            "oneOf": [
                {
                    "required": ["old_string", "new_string"],
                    "not": { "required": ["edits"] }
                },
                {
                    "required": ["edits"],
                    "not": {
                        "anyOf": [
                            { "required": ["old_string"] },
                            { "required": ["new_string"] },
                            { "required": ["occurrence"] },
                            { "required": ["replace_all"] }
                        ]
                    }
                }
            ],
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

    fn validate(&self, arguments: &serde_json::Value) -> Result<()> {
        validate_closed_arguments(arguments, &self.input_schema())?;
        mutation::required_string(self.name(), arguments, "path")?;
        literal_edits(self.name(), arguments)?;
        Ok(())
    }

    fn call(
        &self,
        arguments: &serde_json::Value,
        context: &ExecutionContext,
    ) -> Result<ToolOutput> {
        context.check_cancelled()?;
        validate_closed_arguments(arguments, &self.input_schema())?;
        let tool = self.name();
        let raw = mutation::required_string(tool, arguments, "path")?;
        let edits = literal_edits(tool, arguments)?;
        let path = resolve(context, raw)?.path;
        let prepared = mutation::prepare_edits(&path, &edits)?;
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

/// Decodes either the original single replacement or an ordered batch.
fn literal_edits<'a>(tool: &str, arguments: &'a serde_json::Value) -> Result<Vec<LiteralEdit<'a>>> {
    let Some(value) = arguments.get("edits") else {
        return Ok(vec![literal_edit(tool, arguments)?]);
    };
    for field in ["old_string", "new_string", "occurrence", "replace_all"] {
        if arguments.get(field).is_some() {
            return Err(RuneError::invalid_field(
                format!("{tool} arguments.{field}"),
                "cannot be combined with `edits`",
            )
            .with_hint("put replacement text and selectors inside each edits[] hunk"));
        }
    }
    let edits = value.as_array().ok_or_else(|| {
        RuneError::invalid_field(format!("{tool} arguments.edits"), "expected an array")
    })?;
    if edits.is_empty() {
        return Err(RuneError::invalid_field(
            format!("{tool} arguments.edits"),
            "must not be empty",
        ));
    }
    if edits.len() > mutation::MAX_EDIT_HUNKS {
        return Err(RuneError::too_large(
            format!("{tool} arguments.edits"),
            edits.len(),
            mutation::MAX_EDIT_HUNKS,
        ));
    }
    edits
        .iter()
        .enumerate()
        .map(|(index, edit)| literal_edit(&format!("{tool} edits[{index}]"), edit))
        .collect()
}

/// Reads the text and selector for one literal hunk.
fn literal_edit<'a>(tool: &str, arguments: &'a serde_json::Value) -> Result<LiteralEdit<'a>> {
    Ok(LiteralEdit {
        old: mutation::required_string(tool, arguments, "old_string")?,
        new: mutation::required_string(tool, arguments, "new_string")?,
        occurrence: selector(tool, arguments)?,
    })
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
    fn the_schema_describes_single_and_multiple_edits() {
        let schema = EditFile.input_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(required, ["path"]);
        assert_eq!(
            schema["oneOf"][0]["required"],
            json!(["old_string", "new_string"])
        );
        assert_eq!(schema["oneOf"][1]["required"], json!(["edits"]));
        let batch = &schema["properties"]["edits"];
        assert_eq!(batch["minItems"], 1);
        assert_eq!(batch["maxItems"], mutation::MAX_EDIT_HUNKS);
        assert_eq!(
            batch["items"]["required"],
            json!(["old_string", "new_string"])
        );
        assert_eq!(batch["items"]["additionalProperties"], false);
        assert_eq!(schema["properties"]["occurrence"]["minimum"], 1);
    }

    #[test]
    fn validation_requires_one_complete_replacement_form() {
        for arguments in [
            json!({}),
            json!({ "path": "two.txt" }),
            json!({ "path": "two.txt", "old_string": "alpha" }),
            json!({ "path": "two.txt", "edits": [{ "old_string": "alpha" }] }),
            json!({ "path": "two.txt", "old_string": "alpha", "new_string": "ALPHA", "edits": [] }),
        ] {
            EditFile
                .validate(&arguments)
                .expect_err("incomplete or mixed replacement");
        }
        for arguments in [
            json!({ "path": "two.txt", "old_string": "alpha", "new_string": "ALPHA" }),
            json!({ "path": "two.txt", "edits": [{ "old_string": "alpha", "new_string": "ALPHA" }] }),
        ] {
            EditFile.validate(&arguments).expect("complete replacement");
        }
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
    fn two_literal_hunks_are_applied_in_one_workspace_call() {
        let (dir, context) = workspace();
        let path = seed(&dir, "two.txt", "α.*\r\nbeta\rgamma\n");
        let registry = crate::inventory::builtin_default().expect("registry");
        let arguments = json!({
            "path": "two.txt",
            "edits": [
                { "old_string": "α.*", "new_string": "ALPHA" },
                { "old_string": "gamma", "new_string": "GAMMA!" }
            ]
        });
        assert_eq!(
            registry
                .permission_target("edit_file", &arguments)
                .as_deref(),
            Some("two.txt")
        );
        assert_eq!(registry.activity("edit_file"), Some(Activity::Edit));
        let output = registry
            .call("edit_file", &arguments, &context)
            .expect("call");
        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("2 occurrences"), "{}", output.text);
        assert!(output.text.contains("lines 1-3"), "{}", output.text);
        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"ALPHA\r\nbeta\rGAMMA!\n"
        );
        assert_eq!(std::fs::read_dir(dir.path()).expect("directory").count(), 1);
    }

    #[test]
    fn a_later_missing_or_ambiguous_hunk_leaves_the_entire_file_unchanged() {
        for (old, code, repair) in [
            ("missing", "not_found", "read the file again"),
            ("repeat", "ambiguous_match", "occurrence"),
        ] {
            let (dir, context) = workspace();
            let original = "alpha\r\nrepeat\r\nrepeat\r\n";
            let path = seed(&dir, "two.txt", original);
            let registry = crate::inventory::builtin_default().expect("registry");
            let output = registry
                .call(
                    "edit_file",
                    &json!({
                        "path": "two.txt",
                        "edits": [
                            { "old_string": "alpha", "new_string": "ALPHA" },
                            { "old_string": old, "new_string": "REPLACED" }
                        ]
                    }),
                    &context,
                )
                .expect("model result");
            assert!(output.is_error);
            let diagnostic: serde_json::Value =
                serde_json::from_str(&output.text).expect("diagnostic");
            assert_eq!(diagnostic["code"], code);
            let hint = diagnostic["detail"]["hint"].as_str().expect("repair hint");
            assert!(hint.contains("edits[1]"), "{hint}");
            assert!(hint.contains(repair), "{hint}");
            assert!(hint.contains("no edits were written"), "{hint}");
            assert_eq!(std::fs::read(&path).expect("read"), original.as_bytes());
            assert_eq!(std::fs::read_dir(dir.path()).expect("directory").count(), 1);
        }
    }

    #[test]
    fn hunks_match_prior_results_and_support_per_hunk_selectors() {
        let (dir, context) = workspace();
        let path = seed(&dir, "ordered.txt", "alpha beta beta\n");
        let output = EditFile
            .call(
                &json!({
                    "path": "ordered.txt",
                    "edits": [
                        { "old_string": "alpha", "new_string": "beta" },
                        { "old_string": "beta", "new_string": "BETA", "occurrence": 2 },
                        { "old_string": "beta", "new_string": "remaining", "replace_all": true }
                    ]
                }),
                &context,
            )
            .expect("ordered edits");
        assert!(output.text.contains("4 occurrences"), "{}", output.text);
        assert_eq!(
            std::fs::read(&path).expect("read"),
            b"remaining BETA remaining\n"
        );
    }

    #[test]
    fn malformed_batches_are_refused_without_partial_changes() {
        let (dir, context) = workspace();
        let path = seed(&dir, "two.txt", "alpha beta\n");
        let first = json!({ "old_string": "alpha", "new_string": "ALPHA" });
        let invalid = [
            json!([]),
            json!(null),
            json!({ "old_string": "alpha", "new_string": "ALPHA" }),
            json!([first, null]),
            json!([first, { "old_string": "beta" }]),
            json!([first, { "old_string": 1, "new_string": "BETA" }]),
            json!([first, { "old_string": "beta", "new_string": "BETA", "occurence": 1 }]),
            json!([first, { "old_string": "beta", "new_string": "BETA", "occurrence": 0 }]),
            json!([first, { "old_string": "beta", "new_string": "BETA", "occurrence": "first" }]),
            json!([first, { "old_string": "beta", "new_string": "BETA", "replace_all": "yes" }]),
            json!([first, { "old_string": "beta", "new_string": "BETA", "occurrence": 1, "replace_all": true }]),
            json!([first, { "old_string": "", "new_string": "BETA" }]),
            json!([first, { "old_string": "beta", "new_string": "beta" }]),
            json!([first, { "old_string": "beta", "new_string": "BETA", "occurrence": 2 }]),
            json!(vec![first; mutation::MAX_EDIT_HUNKS + 1]),
        ];
        for edits in invalid {
            EditFile
                .call(&json!({ "path": "two.txt", "edits": edits }), &context)
                .expect_err("invalid batch");
            assert_eq!(std::fs::read(&path).expect("read"), b"alpha beta\n");
        }
        for field in ["old_string", "new_string", "occurrence", "replace_all"] {
            let mut arguments = json!({
                "path": "two.txt",
                "edits": [{ "old_string": "alpha", "new_string": "ALPHA" }]
            });
            arguments[field] = json!(null);
            let err = EditFile
                .call(&arguments, &context)
                .expect_err("mixed forms");
            assert_eq!(err.code(), ErrorCode::InvalidField);
            assert!(
                err.detail()
                    .hint
                    .as_deref()
                    .expect("hint")
                    .contains("edits[]")
            );
            assert_eq!(std::fs::read(&path).expect("read"), b"alpha beta\n");
        }
    }

    #[test]
    fn batches_still_enforce_workspace_containment_and_cancellation() {
        let (_dir, context) = workspace();
        let outside = tempfile::TempDir::new().expect("temp dir");
        let path = seed(&outside, "outside.txt", "alpha beta\n");
        let arguments = json!({
            "path": path.to_str().expect("utf8"),
            "edits": [
                { "old_string": "alpha", "new_string": "ALPHA" },
                { "old_string": "beta", "new_string": "BETA" }
            ]
        });
        assert_eq!(
            EditFile
                .call(&arguments, &context)
                .expect_err("outside")
                .code(),
            ErrorCode::PathOutsideWorkspace
        );
        context.cancellation().cancel();
        assert_eq!(
            EditFile
                .call(&arguments, &context)
                .expect_err("cancelled")
                .code(),
            ErrorCode::Cancelled
        );
        assert_eq!(std::fs::read(&path).expect("read"), b"alpha beta\n");
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
    fn a_file_outside_every_root_is_refused_and_left_alone() {
        let (_dir, context) = workspace();
        let outside = tempfile::TempDir::new().expect("temp dir");
        let target = seed(&outside, "config.txt", "alpha\n");
        let err = EditFile
            .call(
                &json!({
                    "path": target.to_str().expect("utf8"),
                    "old_string": "alpha",
                    "new_string": "beta"
                }),
                &context,
            )
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PathOutsideWorkspace);
        assert_eq!(std::fs::read(&target).expect("read"), b"alpha\n");
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
