//! Tool schemas rewritten into the JSON Schema subset an endpoint accepts.

use std::borrow::Cow;

use serde_json::{Map, Value};

use crate::compat::SchemaRules;

/// Nesting past which a schema is left as it is.
const MAX_DEPTH: usize = 64;

/// Rewrites `schema` for `rules`, then for strict mode when `strict`; a schema
/// that needs no change is returned borrowed.
#[must_use]
pub fn sanitize(schema: &Value, rules: SchemaRules, strict: bool) -> Cow<'_, Value> {
    if rules == SchemaRules::Plain && !strict {
        return Cow::Borrowed(schema);
    }
    let mut owned = schema.clone();
    walk(&mut owned, rules, strict, 0);
    if owned == *schema {
        Cow::Borrowed(schema)
    } else {
        Cow::Owned(owned)
    }
}

fn walk(node: &mut Value, rules: SchemaRules, strict: bool, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    if let Value::Object(object) = node {
        match rules {
            SchemaRules::Gemini => gemini(object),
            SchemaRules::Moonshot => moonshot(object),
            SchemaRules::Plain => {}
        }
    }
    if strict {
        strict_node(node);
    }
    let Value::Object(object) = node else {
        return;
    };
    let next = depth.saturating_add(1);
    for key in ["properties", "$defs", "definitions"] {
        if let Some(Value::Object(children)) = object.get_mut(key) {
            for child in children.values_mut() {
                walk(child, rules, strict, next);
            }
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(children)) = object.get_mut(key) {
            for child in children {
                walk(child, rules, strict, next);
            }
        }
    }
    match object.get_mut("items") {
        Some(Value::Array(children)) => {
            for child in children {
                walk(child, rules, strict, next);
            }
        }
        Some(child) => walk(child, rules, strict, next),
        None => {}
    }
    if let Some(child @ Value::Object(_)) = object.get_mut("additionalProperties") {
        walk(child, rules, strict, next);
    }
}

fn gemini(object: &mut Map<String, Value>) {
    if let Some(Value::Array(values)) = object.get_mut("enum")
        && !values.iter().all(Value::is_string)
    {
        for value in values.iter_mut() {
            if !value.is_string() {
                *value = Value::String(value.to_string());
            }
        }
        object.insert("type".to_owned(), Value::String("string".to_owned()));
    }
    if let Some(Value::Array(types)) = object.get("type").cloned() {
        let nullable = types.iter().any(|kind| kind == "null");
        let rest: Vec<Value> = types.into_iter().filter(|kind| kind != "null").collect();
        object.remove("type");
        if nullable {
            object.insert("nullable".to_owned(), Value::Bool(true));
        }
        match rest.as_slice() {
            [] => {}
            [only] => {
                object.insert("type".to_owned(), only.clone());
            }
            several => {
                let any: Vec<Value> = several
                    .iter()
                    .map(|kind| {
                        let mut one = Map::new();
                        one.insert("type".to_owned(), kind.clone());
                        Value::Object(one)
                    })
                    .collect();
                object.insert("anyOf".to_owned(), Value::Array(any));
            }
        }
    }
    if let Some(Value::Array(required)) = object.get("required") {
        let present = |name: &Value| {
            name.as_str().is_some_and(|name| {
                object
                    .get("properties")
                    .and_then(Value::as_object)
                    .is_some_and(|properties| properties.contains_key(name))
            })
        };
        let kept: Vec<Value> = required
            .iter()
            .filter(|name| present(name))
            .cloned()
            .collect();
        if kept.is_empty() {
            object.remove("required");
        } else if kept.len() != required.len() {
            object.insert("required".to_owned(), Value::Array(kept));
        }
    }
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if kind.as_deref() == Some("array") && !object.contains_key("items") {
        let mut items = Map::new();
        items.insert("type".to_owned(), Value::String("string".to_owned()));
        object.insert("items".to_owned(), Value::Object(items));
    }
    if kind.is_some_and(|kind| kind != "object") {
        for key in ["properties", "required", "additionalProperties"] {
            object.remove(key);
        }
    }
}

fn moonshot(object: &mut Map<String, Value>) {
    if let Some(reference) = object.get("$ref").cloned() {
        object.clear();
        object.insert("$ref".to_owned(), reference);
        return;
    }
    if let Some(Value::Array(items)) = object.get("items") {
        let first = items
            .first()
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        object.insert("items".to_owned(), first);
    }
}

fn strict_node(node: &mut Value) {
    match node {
        Value::Bool(true) => *node = Value::Object(Map::new()),
        Value::Bool(false) => {
            let mut not = Map::new();
            not.insert("not".to_owned(), Value::Object(Map::new()));
            *node = Value::Object(not);
        }
        Value::Object(object) if !object.contains_key("type") => {
            let kind = if object.contains_key("properties") {
                Some("object")
            } else if object.contains_key("items") {
                Some("array")
            } else if object
                .get("enum")
                .and_then(Value::as_array)
                .is_some_and(|values| !values.is_empty() && values.iter().all(Value::is_string))
            {
                Some("string")
            } else {
                None
            };
            if let Some(kind) = kind {
                object.insert("type".to_owned(), Value::String(kind.to_owned()));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn gemini_of(schema: &Value) -> Value {
        sanitize(schema, SchemaRules::Gemini, false).into_owned()
    }

    #[test]
    fn a_nullable_type_list_becomes_one_nullable_type() {
        assert_eq!(
            gemini_of(&json!({"type": ["string", "null"]})),
            json!({"type": "string", "nullable": true})
        );
    }

    #[test]
    fn several_types_become_any_of() {
        assert_eq!(
            gemini_of(&json!({"type": ["string", "integer"]})),
            json!({"anyOf": [{"type": "string"}, {"type": "integer"}]})
        );
    }

    #[test]
    fn a_number_enum_becomes_strings() {
        assert_eq!(
            gemini_of(&json!({"type": "integer", "enum": [1, 2]})),
            json!({"type": "string", "enum": ["1", "2"]})
        );
    }

    #[test]
    fn required_names_must_be_properties() {
        assert_eq!(
            gemini_of(&json!({"type": "object", "properties": {"a": {}}, "required": ["a", "b"]})),
            json!({"type": "object", "properties": {"a": {}}, "required": ["a"]})
        );
        assert_eq!(
            gemini_of(&json!({"type": "object", "properties": {}, "required": ["b"]})),
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn an_array_without_items_gets_string_items_and_scalars_lose_object_keys() {
        assert_eq!(
            gemini_of(&json!({"type": "array"})),
            json!({"type": "array", "items": {"type": "string"}})
        );
        assert_eq!(
            gemini_of(&json!({"type": "string", "properties": {"x": {}}, "required": ["x"]})),
            json!({"type": "string"})
        );
    }

    #[test]
    fn nested_schemas_are_rewritten() {
        let schema = json!({"type": "object", "properties": {"n": {"type": ["integer", "null"]}}});
        assert_eq!(
            gemini_of(&schema),
            json!({"type": "object", "properties": {"n": {"type": "integer", "nullable": true}}})
        );
    }

    #[test]
    fn a_reference_keeps_only_itself_for_moonshot() {
        let schema = json!({"$ref": "#/$defs/x", "description": "d"});
        assert_eq!(
            sanitize(&schema, SchemaRules::Moonshot, false).into_owned(),
            json!({"$ref": "#/$defs/x"})
        );
    }

    #[test]
    fn an_item_list_keeps_its_first_schema_for_moonshot() {
        let schema = json!({"type": "array", "items": [{"type": "string"}, {"type": "number"}]});
        assert_eq!(
            sanitize(&schema, SchemaRules::Moonshot, false).into_owned(),
            json!({"type": "array", "items": {"type": "string"}})
        );
        assert_eq!(
            sanitize(&json!({"items": []}), SchemaRules::Moonshot, false).into_owned(),
            json!({"items": {}})
        );
    }

    #[test]
    fn strict_mode_replaces_boolean_schemas_and_infers_types() {
        let schema = json!({"properties": {"any": true, "none": false, "pick": {"enum": ["a"]}}});
        assert_eq!(
            sanitize(&schema, SchemaRules::Plain, true).into_owned(),
            json!({"type": "object", "properties": {
                "any": {}, "none": {"not": {}}, "pick": {"type": "string", "enum": ["a"]}
            }})
        );
    }

    #[test]
    fn a_deep_schema_does_not_panic() {
        let mut schema = json!({"type": ["string", "null"]});
        for _ in 0..70 {
            schema = json!({"type": "object", "properties": {"x": schema}});
        }
        let sanitized = gemini_of(&schema);
        assert_eq!(sanitized["type"], "object");
    }

    #[test]
    fn a_schema_that_needs_no_change_is_borrowed() {
        let schema =
            json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]});
        for (rules, strict) in [
            (SchemaRules::Plain, false),
            (SchemaRules::Gemini, false),
            (SchemaRules::Moonshot, false),
            (SchemaRules::Plain, true),
        ] {
            assert!(matches!(sanitize(&schema, rules, strict), Cow::Borrowed(_)));
        }
    }
}
