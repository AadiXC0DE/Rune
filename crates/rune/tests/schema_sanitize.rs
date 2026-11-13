//! Built-in tool schemas already fit every endpoint's schema subset.

#![allow(clippy::expect_used, clippy::panic)]

use std::borrow::Cow;

use rune_net::compat::SchemaRules;
use rune_net::schema::sanitize;

#[test]
fn built_in_schemas_pass_unchanged() {
    let registry = rune_tools::inventory::builtin_default().expect("built-in tools");
    let specs = rune_tools::inventory::advertisement(&registry);
    assert!(!specs.is_empty());
    for spec in &specs {
        for rules in [
            SchemaRules::Plain,
            SchemaRules::Gemini,
            SchemaRules::Moonshot,
        ] {
            let sanitized = sanitize(&spec.input_schema, rules, false);
            assert!(
                matches!(sanitized, Cow::Borrowed(_)),
                "{} changes under {}: {}",
                spec.name,
                rules.as_str(),
                sanitized
            );
        }
    }
}
