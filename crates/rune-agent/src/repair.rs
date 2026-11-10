//! Repairs a tool call sent in a near-miss form.
//!
//! A model may call a tool by another harness's name, wrap its arguments in a
//! code fence, or send `"20"` where the schema asks for 20. Such a call is
//! resolved to the call it meant, and every change is recorded, so the result
//! can tell the model what ran and the next call is sent in the right form.

use rune_net::message::ToolSpec;
use rune_tools::contract::conforms;
use serde_json::{Map, Value};

/// A call after repair, ready for policy and execution.
#[derive(Clone, PartialEq, Debug)]
pub struct Resolved {
    /// The offered tool the call runs as.
    pub name: String,
    /// The arguments it runs with.
    pub arguments: Value,
    /// The model's own name when it differed.
    pub renamed_from: Option<String>,
    /// What was changed in the arguments, as short phrases.
    pub repairs: Vec<String>,
    /// A shell command recognised as another tool's call: it runs as that
    /// tool, and the recorded call and its result keep the model's name.
    pub intercepted: bool,
}

/// Why a call could not be resolved.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Unresolved {
    /// No offered tool has the name or an alias of it.
    UnknownTool,
    /// The arguments are not JSON even after repair, with the parse error.
    InvalidJson(String),
}

/// Levels of nested objects and arrays walked when coercing arguments.
const MAX_DEPTH: usize = 6;

/// Tool names used by other harnesses, with the tool each one means.
const ALIASES: &[(&str, &str)] = &[
    ("bash", "shell"),
    ("sh", "shell"),
    ("execute_command", "shell"),
    ("run_command", "shell"),
    ("exec", "shell"),
    ("read", "read_file"),
    ("cat", "read_file"),
    ("view", "read_file"),
    ("view_file", "read_file"),
    ("edit", "edit_file"),
    ("str_replace", "edit_file"),
    ("str_replace_editor", "edit_file"),
    ("replace", "edit_file"),
    ("write", "write_file"),
    ("create_file", "write_file"),
    ("grep", "grep_files"),
    ("search", "grep_files"),
    ("rg", "grep_files"),
    ("glob", "glob_files"),
    ("find", "glob_files"),
    ("ls", "glob_files"),
    ("list_files", "glob_files"),
    ("todo_write", "update_plan"),
    ("todowrite", "update_plan"),
];

/// Argument names used by other harnesses, with the name a schema here uses.
const RENAMES: &[(&str, &str)] = &[
    ("file_path", "path"),
    ("filepath", "path"),
    ("filename", "path"),
    ("file", "path"),
    ("old_str", "old_string"),
    ("oldText", "old_string"),
    ("old_text", "old_string"),
    ("new_str", "new_string"),
    ("newText", "new_string"),
    ("new_text", "new_string"),
    ("cmd", "command"),
];

const FENCED: &str = "the arguments were in a code fence";
const TRAILING_COMMA: &str = "the arguments had a trailing comma";
const STRINGIFIED: &str = "the arguments were a JSON string";

/// Resolves a call against the offered tools.
///
/// The name is matched exactly, then ignoring ASCII case, then through the
/// alias table. An alias applies only when its target is offered and the
/// arguments, once mapped and coerced, fit the target's schema. A call that
/// already fits under its exact name is returned unchanged.
pub fn resolve(name: &str, arguments: &str, offered: &[ToolSpec]) -> Result<Resolved, Unresolved> {
    let direct = offered.iter().find(|spec| spec.name == name).or_else(|| {
        offered
            .iter()
            .find(|spec| spec.name.eq_ignore_ascii_case(name))
    });
    if let Some(spec) = direct {
        let (value, mut repairs) = parse(arguments).map_err(Unresolved::InvalidJson)?;
        let value = coerce(&spec.input_schema, value, &mut repairs);
        return Ok(Resolved {
            name: spec.name.clone(),
            arguments: value,
            renamed_from: (spec.name != name).then(|| name.to_owned()),
            repairs,
            intercepted: false,
        });
    }
    let alias = name.to_ascii_lowercase();
    let target = ALIASES
        .iter()
        .find(|(from, _)| *from == alias)
        .map(|(_, to)| *to)
        .ok_or(Unresolved::UnknownTool)?;
    let spec = offered
        .iter()
        .find(|spec| spec.name == target)
        .ok_or(Unresolved::UnknownTool)?;
    let (value, mut repairs) = parse(arguments).map_err(|_| Unresolved::UnknownTool)?;
    let value = coerce(&spec.input_schema, map_alias(&alias, value), &mut repairs);
    if !conforms(&spec.input_schema, &value) {
        return Err(Unresolved::UnknownTool);
    }
    Ok(Resolved {
        name: spec.name.clone(),
        arguments: value,
        renamed_from: Some(name.to_owned()),
        repairs,
        intercepted: false,
    })
}

/// The note appended to a repaired call's result, or None when nothing changed.
pub fn note(resolved: &Resolved) -> Option<String> {
    let mut sentences = Vec::new();
    if let Some(from) = &resolved.renamed_from {
        sentences.push(format!(
            "there is no tool named `{from}`; this ran as `{}`.",
            resolved.name
        ));
    }
    if !resolved.repairs.is_empty() {
        sentences.push(format!("Repaired: {}.", resolved.repairs.join("; ")));
    }
    (!sentences.is_empty()).then(|| format!("[note: {}]", sentences.join(" ")))
}

/// Parses the arguments, repairing the common ways they arrive malformed.
///
/// An empty string is an empty object. On failure the error is the one the
/// arguments as sent produced.
fn parse(arguments: &str) -> Result<(Value, Vec<String>), String> {
    if arguments.trim().is_empty() {
        return Ok((Value::Object(Map::new()), Vec::new()));
    }
    let mut repairs = Vec::new();
    let value = match serde_json::from_str::<Value>(arguments) {
        Ok(value) => value,
        Err(err) => {
            let mut text = arguments.trim().to_owned();
            if let Some(inner) = strip_fence(&text) {
                text = inner;
                repairs.push(FENCED.to_owned());
            }
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                value
            } else {
                let without = strip_trailing_commas(&text);
                match serde_json::from_str::<Value>(&without) {
                    Ok(value) if without != text => {
                        repairs.push(TRAILING_COMMA.to_owned());
                        value
                    }
                    _ => return Err(err.to_string()),
                }
            }
        }
    };
    if let Value::String(inner) = &value
        && let Ok(object @ Value::Object(_)) = serde_json::from_str::<Value>(inner)
    {
        repairs.push(STRINGIFIED.to_owned());
        return Ok((object, repairs));
    }
    Ok((value, repairs))
}

/// Returns the text inside one surrounding Markdown code fence.
fn strip_fence(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let (first, rest) = lines.split_first()?;
    let (last, body) = rest.split_last()?;
    let language = first.trim().strip_prefix("```")?;
    if last.trim() != "```" || !language.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(body.join("\n"))
}

/// Removes commas that directly precede `}` or `]`, outside strings.
fn strip_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for (index, &c) in chars.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if c == ','
            && chars
                .iter()
                .skip(index.saturating_add(1))
                .find(|next| !next.is_whitespace())
                .is_some_and(|next| matches!(next, '}' | ']'))
        {
            continue;
        }
        out.push(c);
    }
    out
}

/// Maps another harness's argument names onto the alias target's.
fn map_alias(alias: &str, value: Value) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };
    match alias {
        "bash" | "sh" | "execute_command" | "run_command" | "exec" => {
            object = rename(object, "cmd", "command");
            object = rename(object, "workdir", "cwd");
            if !object.contains_key("action") {
                object.insert("action".to_owned(), Value::from("run"));
            }
        }
        "read" | "cat" | "view" | "view_file" => {
            object = rename(object, "file_path", "path");
            object = rename(object, "filePath", "path");
            object = rename(object, "offset", "start_line");
            object = rename(object, "limit", "line_count");
        }
        "write" | "create_file" => object = rename(object, "file_text", "content"),
        "grep" | "search" | "rg" => object = rename(object, "glob", "include"),
        "ls" | "list_files" => {
            if !object.contains_key("pattern") {
                object.insert("pattern".to_owned(), Value::from("*"));
            }
        }
        "todo_write" | "todowrite" => {
            object = rename(object, "todos", "plan");
            if let Some(Value::Array(items)) = object.get_mut("plan") {
                for item in items.iter_mut() {
                    if let Value::Object(fields) = item {
                        *fields = rename(std::mem::take(fields), "content", "step");
                    }
                }
            }
        }
        _ => {}
    }
    Value::Object(object)
}

/// Renames a key in place, keeping its position, when the new name is free.
fn rename(object: Map<String, Value>, from: &str, to: &str) -> Map<String, Value> {
    if !object.contains_key(from) || object.contains_key(to) {
        return object;
    }
    object
        .into_iter()
        .map(|(key, value)| {
            if key == from {
                (to.to_owned(), value)
            } else {
                (key, value)
            }
        })
        .collect()
}

/// Brings arguments that do not fit the schema closer to it, recording each change.
///
/// Unknown keys are dropped only when the arguments then fit.
fn coerce(schema: &Value, value: Value, repairs: &mut Vec<String>) -> Value {
    if conforms(schema, &value) {
        return value;
    }
    let value = coerce_value(schema, value, 1, repairs);
    let mut dropped = Vec::new();
    let without = drop_unknown(schema, value.clone(), 1, &mut dropped);
    if !dropped.is_empty() && conforms(schema, &without) {
        repairs.extend(dropped);
        return without;
    }
    value
}

fn coerce_value(schema: &Value, value: Value, depth: usize, repairs: &mut Vec<String>) -> Value {
    if depth > MAX_DEPTH {
        return value;
    }
    match value {
        Value::Object(object) => Value::Object(coerce_object(schema, object, depth, repairs)),
        Value::Array(items) => match schema.get("items") {
            Some(item_schema) => Value::Array(
                items
                    .into_iter()
                    .map(|item| coerce_value(item_schema, item, depth.saturating_add(1), repairs))
                    .collect(),
            ),
            None => Value::Array(items),
        },
        other => other,
    }
}

fn coerce_object(
    schema: &Value,
    mut object: Map<String, Value>,
    depth: usize,
    repairs: &mut Vec<String>,
) -> Map<String, Value> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return object;
    };
    for (from, to) in RENAMES {
        if object.contains_key(*from)
            && !object.contains_key(*to)
            && properties.contains_key(*to)
            && !properties.contains_key(*from)
        {
            object = rename(object, from, to);
            repairs.push(format!("`{from}` was renamed to `{to}`"));
        }
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut coerced = Map::new();
    for (key, field) in object {
        let Some(field_schema) = properties.get(&key) else {
            coerced.insert(key, field);
            continue;
        };
        if field.is_null() && !required.contains(&key.as_str()) && !conforms(field_schema, &field) {
            repairs.push(format!("`{key}` was null"));
            continue;
        }
        let field = match field {
            Value::String(text) => match from_string(field_schema, &text) {
                Some(parsed) => {
                    repairs.push(format!("`{key}` was a string"));
                    parsed
                }
                None => Value::String(text),
            },
            other => other,
        };
        let field = coerce_value(field_schema, field, depth.saturating_add(1), repairs);
        coerced.insert(key, field);
    }
    coerced
}

/// Reads a string as the scalar, array or object its schema asks for.
fn from_string(schema: &Value, text: &str) -> Option<Value> {
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).collect(),
        _ => return None,
    };
    if types.contains(&"string") {
        return None;
    }
    types.into_iter().find_map(|name| match name {
        "integer" => text.trim().parse::<i64>().ok().map(Value::from),
        "number" => text
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number),
        "boolean" => match text.trim() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        "array" => serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_array),
        "object" => serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object),
        _ => None,
    })
}

/// Removes keys a closed object does not declare, recording each one.
fn drop_unknown(schema: &Value, value: Value, depth: usize, dropped: &mut Vec<String>) -> Value {
    if depth > MAX_DEPTH {
        return value;
    }
    match value {
        Value::Object(object) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let closed = schema.get("additionalProperties").and_then(Value::as_bool) == Some(false);
            let mut kept = Map::new();
            for (key, field) in object {
                match properties.and_then(|fields| fields.get(&key)) {
                    Some(field_schema) => {
                        let field =
                            drop_unknown(field_schema, field, depth.saturating_add(1), dropped);
                        kept.insert(key, field);
                    }
                    None if closed => {
                        dropped.push(format!("`{key}` is not an argument and was dropped"));
                    }
                    None => {
                        kept.insert(key, field);
                    }
                }
            }
            Value::Object(kept)
        }
        Value::Array(items) => match schema.get("items") {
            Some(item_schema) => Value::Array(
                items
                    .into_iter()
                    .map(|item| drop_unknown(item_schema, item, depth.saturating_add(1), dropped))
                    .collect(),
            ),
            None => Value::Array(items),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn offered() -> Vec<ToolSpec> {
        rune_tools::inventory::builtin_default()
            .expect("registry")
            .all_schemas()
    }

    fn resolved(name: &str, arguments: &str) -> Resolved {
        resolve(name, arguments, &offered()).expect("resolved")
    }

    /// Resolves an alias and checks the target and the mapped arguments.
    fn alias(name: &str, arguments: &Value, target: &str, expected: &Value) {
        let call = resolved(name, &arguments.to_string());
        assert_eq!(call.name, target, "{name}");
        assert_eq!(&call.arguments, expected, "{name}");
        assert_eq!(call.renamed_from.as_deref(), Some(name));
    }

    fn spec(name: &str, schema: Value) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: "A tool.".to_owned(),
            input_schema: schema,
        }
    }

    #[test]
    fn shell_aliases_run_a_command() {
        for name in ["bash", "sh", "execute_command", "run_command", "exec"] {
            alias(
                name,
                &json!({ "cmd": "ls", "workdir": "src" }),
                "shell",
                &json!({ "command": "ls", "cwd": "src", "action": "run" }),
            );
        }
        let call = resolved("bash", r#"{"command":"ls"}"#);
        assert_eq!(call.arguments, json!({ "command": "ls", "action": "run" }));
        assert!(call.repairs.is_empty());
    }

    #[test]
    fn read_aliases_read_a_file() {
        for name in ["read", "cat", "view", "view_file"] {
            alias(
                name,
                &json!({ "file_path": "a", "offset": 10, "limit": 5 }),
                "read_file",
                &json!({ "path": "a", "start_line": 10, "line_count": 5 }),
            );
        }
        alias(
            "cat",
            &json!({ "filePath": "a" }),
            "read_file",
            &json!({ "path": "a" }),
        );
        let call = resolved("read", r#"{"file_path":"a","offset":"10","limit":"5"}"#);
        assert_eq!(
            call.arguments,
            json!({ "path": "a", "start_line": 10, "line_count": 5 })
        );
        assert_eq!(
            call.repairs,
            ["`start_line` was a string", "`line_count` was a string"]
        );
    }

    #[test]
    fn edit_aliases_edit_a_file() {
        for name in ["edit", "str_replace", "str_replace_editor", "replace"] {
            alias(
                name,
                &json!({ "file_path": "a", "old_str": "x", "new_str": "y" }),
                "edit_file",
                &json!({ "path": "a", "old_string": "x", "new_string": "y" }),
            );
        }
    }

    #[test]
    fn write_aliases_write_a_file() {
        for name in ["write", "create_file"] {
            alias(
                name,
                &json!({ "path": "a", "file_text": "body" }),
                "write_file",
                &json!({ "path": "a", "content": "body" }),
            );
        }
    }

    #[test]
    fn grep_aliases_search_files() {
        for name in ["grep", "search", "rg"] {
            alias(
                name,
                &json!({ "pattern": "fn", "glob": "*.rs" }),
                "grep_files",
                &json!({ "pattern": "fn", "include": "*.rs" }),
            );
        }
    }

    #[test]
    fn glob_aliases_list_files() {
        for name in ["glob", "find"] {
            alias(
                name,
                &json!({ "pattern": "*.rs" }),
                "glob_files",
                &json!({ "pattern": "*.rs" }),
            );
        }
        for name in ["ls", "list_files"] {
            alias(
                name,
                &json!({ "path": "src" }),
                "glob_files",
                &json!({ "path": "src", "pattern": "*" }),
            );
        }
    }

    #[test]
    fn todo_aliases_update_the_plan() {
        for name in ["todo_write", "todowrite", "TodoWrite"] {
            let sent = json!({
                "todos": [{ "content": "one", "status": "pending", "activeForm": "x" }]
            });
            let call = resolved(name, &sent.to_string());
            assert_eq!(call.name, "update_plan");
            assert_eq!(
                call.arguments,
                json!({ "plan": [{ "step": "one", "status": "pending" }] })
            );
            assert_eq!(
                call.repairs,
                ["`activeForm` is not an argument and was dropped"]
            );
        }
    }

    #[test]
    fn an_alias_needs_its_target_offered() {
        let without_shell: Vec<ToolSpec> = offered()
            .into_iter()
            .filter(|spec| spec.name != "shell")
            .collect();
        assert_eq!(
            resolve("bash", r#"{"command":"ls"}"#, &without_shell),
            Err(Unresolved::UnknownTool)
        );
        assert_eq!(
            resolve("frobnicate", "{}", &offered()),
            Err(Unresolved::UnknownTool)
        );
    }

    #[test]
    fn an_alias_whose_arguments_do_not_fit_is_not_applied() {
        assert_eq!(
            resolve("cat", r#"{"path":"a","line_count":"many"}"#, &offered()),
            Err(Unresolved::UnknownTool)
        );
        assert_eq!(
            resolve("write", r#"{"path":"a"}"#, &offered()),
            Err(Unresolved::UnknownTool),
            "a required argument is missing"
        );
        assert_eq!(
            resolve("bash", "not json", &offered()),
            Err(Unresolved::UnknownTool)
        );
    }

    #[test]
    fn a_name_in_another_case_runs_as_the_offered_tool() {
        let call = resolved("Read_File", r#"{"path":"a"}"#);
        assert_eq!(call.name, "read_file");
        assert_eq!(call.renamed_from.as_deref(), Some("Read_File"));
        assert!(call.repairs.is_empty());
    }

    #[test]
    fn fenced_arguments_are_parsed() {
        let call = resolved("read_file", "```json\n{\"path\":\"a\"}\n```");
        assert_eq!(call.arguments, json!({ "path": "a" }));
        assert_eq!(call.repairs, [FENCED]);
        let call = resolved("read_file", "```\n{\"path\":\"a\"}\n```");
        assert_eq!(call.repairs, [FENCED]);
    }

    #[test]
    fn a_trailing_comma_is_removed() {
        let call = resolved("read_file", r#"{"path":"a",}"#);
        assert_eq!(call.arguments, json!({ "path": "a" }));
        assert_eq!(call.repairs, [TRAILING_COMMA]);
        let call = resolved(
            "edit_file",
            r#"{"path":"a, }","old_string":"x","new_string":"y" , }"#,
        );
        assert_eq!(
            call.arguments["path"], "a, }",
            "a comma inside a string stays"
        );
        assert_eq!(call.repairs, [TRAILING_COMMA]);
    }

    #[test]
    fn stringified_arguments_are_parsed() {
        let call = resolved("read_file", r#""{\"path\":\"a\"}""#);
        assert_eq!(call.arguments, json!({ "path": "a" }));
        assert_eq!(call.repairs, [STRINGIFIED]);
    }

    #[test]
    fn empty_arguments_are_an_empty_object() {
        let call = resolved("git_diff", "  ");
        assert_eq!(call.arguments, json!({}));
        assert!(call.repairs.is_empty());
    }

    #[test]
    fn unparseable_arguments_keep_the_parse_error() {
        let expected = serde_json::from_str::<Value>("{not json")
            .expect_err("invalid")
            .to_string();
        assert_eq!(
            resolve("read_file", "{not json", &offered()),
            Err(Unresolved::InvalidJson(expected))
        );
    }

    #[test]
    fn a_numeric_or_boolean_string_is_coerced() {
        let call = resolved(
            "edit_file",
            r#"{"path":"a","old_string":"x","new_string":"y","occurrence":"2"}"#,
        );
        assert_eq!(call.arguments["occurrence"], 2);
        assert_eq!(call.repairs, ["`occurrence` was a string"]);
        let call = resolved(
            "edit_file",
            r#"{"path":"a","old_string":"x","new_string":"y","replace_all":"true"}"#,
        );
        assert_eq!(call.arguments["replace_all"], true);
        let scale = spec(
            "scale",
            json!({
                "type": "object",
                "properties": { "ratio": { "type": "number" } },
                "additionalProperties": false
            }),
        );
        let call = resolve("scale", r#"{"ratio":"1.5"}"#, &[scale]).expect("resolved");
        assert_eq!(call.arguments, json!({ "ratio": 1.5 }));
    }

    #[test]
    fn an_array_or_object_string_is_coerced() {
        let call = resolved(
            "update_plan",
            r#"{"plan":"[{\"step\":\"one\",\"status\":\"pending\"}]"}"#,
        );
        assert_eq!(
            call.arguments,
            json!({ "plan": [{ "step": "one", "status": "pending" }] })
        );
        assert_eq!(call.repairs, ["`plan` was a string"]);
    }

    #[test]
    fn a_null_optional_argument_is_removed() {
        let call = resolved("read_file", r#"{"path":"a","start_line":null}"#);
        assert_eq!(call.arguments, json!({ "path": "a" }));
        assert_eq!(call.repairs, ["`start_line` was null"]);
    }

    #[test]
    fn other_argument_names_are_renamed() {
        let call = resolved(
            "edit_file",
            r#"{"file_path":"a","old_str":"x","new_str":"y"}"#,
        );
        assert_eq!(
            call.arguments,
            json!({ "path": "a", "old_string": "x", "new_string": "y" })
        );
        assert_eq!(
            call.repairs,
            [
                "`file_path` was renamed to `path`",
                "`old_str` was renamed to `old_string`",
                "`new_str` was renamed to `new_string`",
            ]
        );
        let call = resolved("shell", r#"{"action":"run","cmd":"ls"}"#);
        assert_eq!(call.arguments, json!({ "action": "run", "command": "ls" }));
    }

    #[test]
    fn an_unknown_key_is_dropped_only_when_that_makes_the_call_fit() {
        let call = resolved("read_file", r#"{"path":"a","verbose":true}"#);
        assert_eq!(call.arguments, json!({ "path": "a" }));
        assert_eq!(
            call.repairs,
            ["`verbose` is not an argument and was dropped"]
        );
        // Still missing `path`, so the tool reports what is wrong.
        let call = resolved("read_file", r#"{"verbose":true}"#);
        assert_eq!(call.arguments, json!({ "verbose": true }));
        assert!(call.repairs.is_empty());
    }

    #[test]
    fn coercion_stops_below_six_levels() {
        let nest = |levels: usize| {
            let mut schema = json!({
                "type": "object",
                "properties": { "n": { "type": "integer" } },
                "additionalProperties": false
            });
            let mut value = json!({ "n": "5" });
            for _ in 1..levels {
                schema = json!({
                    "type": "object",
                    "properties": { "inner": schema },
                    "additionalProperties": false
                });
                value = json!({ "inner": value });
            }
            let call = resolve("deep", &value.to_string(), &[spec("deep", schema)]);
            (call.expect("resolved"), value)
        };
        let (call, _) = nest(6);
        assert_eq!(call.repairs, ["`n` was a string"]);
        let (call, sent) = nest(7);
        assert_eq!(call.arguments, sent);
        assert!(call.repairs.is_empty());
    }

    #[test]
    fn a_conforming_call_is_passed_through_untouched() {
        let arguments = r#"{"path":"a","start_line":3}"#;
        let call = resolved("read_file", arguments);
        assert_eq!(
            call,
            Resolved {
                name: "read_file".to_owned(),
                arguments: serde_json::from_str(arguments).expect("json"),
                renamed_from: None,
                repairs: Vec::new(),
                intercepted: false,
            }
        );
        assert_eq!(note(&call), None);
    }

    #[test]
    fn the_note_text_is_exact() {
        let mut call = Resolved {
            name: "shell".to_owned(),
            arguments: json!({}),
            renamed_from: Some("bash".to_owned()),
            repairs: vec!["`limit` was a string".to_owned()],
            intercepted: false,
        };
        assert_eq!(
            note(&call).as_deref(),
            Some(
                "[note: there is no tool named `bash`; this ran as `shell`. Repaired: `limit` \
                 was a string.]"
            )
        );
        call.renamed_from = None;
        call.repairs.push("`file` was renamed to `path`".to_owned());
        assert_eq!(
            note(&call).as_deref(),
            Some("[note: Repaired: `limit` was a string; `file` was renamed to `path`.]")
        );
        call.repairs.clear();
        call.renamed_from = Some("Shell".to_owned());
        assert_eq!(
            note(&call).as_deref(),
            Some("[note: there is no tool named `Shell`; this ran as `shell`.]")
        );
    }
}
