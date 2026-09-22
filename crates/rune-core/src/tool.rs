//! Tool definitions advertised to a model.
//!
//! A tool spec is a domain object: a name, a description, and the JSON Schema of
//! its arguments. It lives here rather than in the transport layer so a tool
//! implementation can describe itself without linking a network client.

use serde::{Deserialize, Serialize};

use crate::error::{Result, RuneError};

/// A tool advertised to the model.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name, as the model must call it.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema for the arguments object.
    pub input_schema: serde_json::Value,
}

/// Largest number of distinct tools advertised in one request.
pub const MAX_TOOLS: usize = 256;

/// Largest tool name accepted.
pub const MAX_TOOL_NAME: usize = 128;

/// Largest tool description accepted.
pub const MAX_TOOL_DESCRIPTION: usize = 4096;

/// Narrowest JSON Schema the input schema may be.
///
/// A provider must receive an object schema; anything else is a defect in the
/// tool definition rather than a user error.
pub fn validate_tool_spec(spec: &ToolSpec) -> Result<()> {
    if spec.name.is_empty() {
        return Err(RuneError::invalid_field("tool.name", "must not be empty"));
    }
    if spec.name.len() > MAX_TOOL_NAME {
        return Err(RuneError::too_large(
            "tool.name",
            spec.name.len(),
            MAX_TOOL_NAME,
        ));
    }
    if !spec
        .name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(RuneError::invalid_field(
            "tool.name",
            format!(
                "`{}` contains characters outside the accepted set",
                spec.name
            ),
        ));
    }
    if spec.description.is_empty() {
        return Err(RuneError::invalid_field(
            "tool.description",
            format!("`{}` has no description", spec.name),
        ));
    }
    if spec.description.len() > MAX_TOOL_DESCRIPTION {
        return Err(RuneError::too_large(
            "tool.description",
            spec.description.len(),
            MAX_TOOL_DESCRIPTION,
        ));
    }
    let Some(object) = spec.input_schema.as_object() else {
        return Err(RuneError::invalid_field(
            "tool.input_schema",
            format!("`{}` does not describe an object", spec.name),
        ));
    };
    match object.get("type").and_then(serde_json::Value::as_str) {
        Some("object") => Ok(()),
        Some(other) => Err(RuneError::invalid_field(
            "tool.input_schema",
            format!("`{}` has type `{other}`, expected `object`", spec.name),
        )),
        None => Err(RuneError::invalid_field(
            "tool.input_schema",
            format!("`{}` does not declare a type", spec.name),
        )),
    }
}

/// Validates a whole set of tool specs.
///
/// A set is validated as a unit because duplicate names are a defect of the set
/// rather than of any one spec.
pub fn validate_tool_specs(specs: &[ToolSpec]) -> Result<()> {
    if specs.len() > MAX_TOOLS {
        return Err(RuneError::too_large("tools", specs.len(), MAX_TOOLS));
    }
    let mut seen = std::collections::HashSet::new();
    for spec in specs {
        validate_tool_spec(spec)?;
        if !seen.insert(spec.name.as_str()) {
            return Err(RuneError::invalid_field(
                "tools",
                format!("`{}` is advertised twice", spec.name),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: "does a thing".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        }
    }

    #[test]
    fn a_well_formed_spec_passes() {
        validate_tool_spec(&spec("read_file")).expect("valid");
    }

    #[test]
    fn a_name_may_use_letters_digits_underscores_and_hyphens() {
        validate_tool_spec(&spec("a_b-C9")).expect("valid");
    }

    #[test]
    fn an_empty_name_is_refused() {
        let err = validate_tool_spec(&spec("")).expect_err("refused");
        assert!(err.message().contains("must not be empty"), "{err}");
    }

    #[test]
    fn a_name_outside_the_accepted_set_is_refused() {
        for bad in ["read file", "read.file", "read/file", "read\u{e9}"] {
            let err = validate_tool_spec(&spec(bad)).expect_err("refused");
            assert!(
                err.message().contains("outside the accepted set"),
                "`{bad}` was not refused: {err}"
            );
        }
    }

    #[test]
    fn an_oversized_name_is_refused() {
        let err = validate_tool_spec(&spec(&"a".repeat(MAX_TOOL_NAME + 1))).expect_err("refused");
        assert_eq!(err.code(), crate::error::ErrorCode::TooLarge);
    }

    #[test]
    fn a_missing_description_is_refused() {
        let mut bad = spec("t");
        bad.description = String::new();
        assert!(validate_tool_spec(&bad).is_err());
    }

    #[test]
    fn an_oversized_description_is_refused() {
        let mut bad = spec("t");
        bad.description = "d".repeat(MAX_TOOL_DESCRIPTION + 1);
        assert_eq!(
            validate_tool_spec(&bad).expect_err("refused").code(),
            crate::error::ErrorCode::TooLarge
        );
    }

    #[test]
    fn a_non_object_schema_is_refused() {
        for schema in [
            serde_json::json!("string"),
            serde_json::json!([1, 2]),
            serde_json::json!(null),
        ] {
            let mut bad = spec("t");
            bad.input_schema = schema;
            let err = validate_tool_spec(&bad).expect_err("refused");
            assert!(err.message().contains("object"), "{err}");
        }
    }

    #[test]
    fn a_schema_with_a_wrong_type_name_is_refused() {
        let mut bad = spec("t");
        bad.input_schema = serde_json::json!({ "type": "array" });
        let err = validate_tool_spec(&bad).expect_err("refused");
        assert!(err.message().contains("expected `object`"), "{err}");
    }

    #[test]
    fn a_schema_without_a_type_is_refused() {
        let mut bad = spec("t");
        bad.input_schema = serde_json::json!({ "properties": {} });
        let err = validate_tool_spec(&bad).expect_err("refused");
        assert!(err.message().contains("does not declare a type"), "{err}");
    }

    #[test]
    fn a_duplicate_name_is_refused() {
        let err = validate_tool_specs(&[spec("t"), spec("t")]).expect_err("refused");
        assert!(err.message().contains("advertised twice"), "{err}");
    }

    #[test]
    fn too_many_tools_are_refused() {
        let many: Vec<ToolSpec> = (0..=MAX_TOOLS).map(|i| spec(&format!("t{i}"))).collect();
        assert_eq!(
            validate_tool_specs(&many).expect_err("refused").code(),
            crate::error::ErrorCode::TooLarge
        );
    }

    #[test]
    fn a_spec_survives_a_round_trip() {
        let original = spec("read_file");
        let text = serde_json::to_string(&original).expect("serialized");
        let back: ToolSpec = serde_json::from_str(&text).expect("deserialized");
        assert_eq!(back, original);
    }
}
