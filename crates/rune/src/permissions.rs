//! Inspecting the permission rules in force.
//!
//! Reports what each rule decides and where it came from, so a surprising
//! refusal can be traced to the layer that produced it.

use std::fmt::Write as _;

use rune_core::config::{PermissionMode, Settings};
use rune_core::error::Result;
use rune_policy::decision::{Layer, Outcome};
use rune_policy::rules::{Rule, RuleSet};

/// One rule as the surface reports it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    /// Tool or tool class the rule applies to.
    pub tool: String,
    /// Target pattern.
    pub pattern: String,
    /// What the rule decides.
    pub outcome: Outcome,
    /// Layer the rule came from.
    pub layer: String,
}

/// Collects the rules a session would apply, in evaluation order.
///
/// The built-in rules come first so a rule from a configuration layer is
/// considered after them; specificity decides the winner, not order.
#[must_use]
pub fn rules_for(settings: &Settings, extra: &RuleSet) -> RuleSet {
    let mut rules = builtin_rules(&settings.permission_mode);
    // The web tools are refused by the built-in set, because that set is built
    // without the configuration and a refusal is the safe thing for it to say.
    // Enabling them is the configuration's decision, so the allow is written
    // here, at a layer above the default, which is what lets it overrule the
    // built-in denial. `offline` refuses everything, so it wins over the
    // setting: there is no point allowing a tool whose transport will refuse
    // the request.
    if settings.web_tools && !settings.offline {
        for tool in ["web_fetch", "web_search"] {
            rules.push(Rule::allow(tool, "*", Layer::User));
        }
    }
    for rule in extra.rules() {
        rules.push(rule.clone());
    }
    rules
}

/// Returns the rules that ship with the harness.
///
/// These name the actions that are always safe and the ones that are never
/// taken without asking, so a fresh install has a usable starting point without
/// a configuration file.
#[must_use]
pub fn builtin_rules(mode: &PermissionMode) -> RuleSet {
    let mut rules = RuleSet::new();

    // Reading is what an agent does constantly; asking each time would make the
    // default unusable.
    for tool in ["read_file", "glob_files", "grep_files"] {
        rules.push(Rule::allow(tool, "*", Layer::Default));
    }

    // A write inside the workspace is recoverable; a command is not.
    rules.push(Rule::allow("write_file", "*", Layer::Default));
    rules.push(Rule::allow("edit_file", "*", Layer::Default));

    // Refused by default, so a caller that never consults the configuration
    // cannot reach the network by omission. Whether the run may use them is
    // settled in `rules_for`, which knows the settings.
    rules.push(Rule::deny("web_fetch", "*", Layer::Default));
    rules.push(Rule::deny("web_search", "*", Layer::Default));

    if matches!(mode, PermissionMode::Ask) {
        // In ask mode the shell is the thing being asked about, so it carries no
        // built-in answer; the mode default supplies one.
        return rules;
    }

    // Commands that only read are allowed without a prompt.
    for pattern in [
        "ls*",
        "pwd",
        "cat *",
        "head *",
        "tail *",
        "wc *",
        "git status*",
        "git diff*",
        "git log*",
        "cargo test*",
        "cargo build*",
        "cargo clippy*",
    ] {
        rules.push(Rule::allow("shell", pattern, Layer::Default));
    }

    rules
}

/// Projects a rule set into reportable rows.
///
/// The rows are in evaluation order, which is the order a reader needs to
/// understand which rule won.
#[must_use]
pub fn rows(rules: &RuleSet) -> Vec<Row> {
    rules
        .rules()
        .iter()
        .map(|rule| Row {
            tool: rule.tool.clone(),
            pattern: rule.pattern.clone(),
            outcome: rule.outcome,
            layer: rule.layer.to_string(),
        })
        .collect()
}

/// Reports what a specific action would decide.
#[must_use]
pub fn explain(rules: &RuleSet, mode: PermissionMode, tool: &str, target: &str) -> String {
    let fallback = rune_policy::decision::mode_default(mode);
    let decision = rules.evaluate(tool, target, fallback);
    let outcome = rune_agent::turn::effective_outcome(mode, decision.outcome);
    match outcome {
        Outcome::Allow => format!("{tool} `{target}`: allowed ({})", decision.explain()),
        Outcome::Deny => format!("{tool} `{target}`: refused ({})", decision.explain()),
        Outcome::Ask => format!(
            "{tool} `{target}`: asks first ({}); mode `{}` decides",
            decision.explain(),
            mode.as_str()
        ),
    }
}

/// Renders the rule table for a terminal.
#[must_use]
pub fn render(rules: &RuleSet, mode: PermissionMode) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "mode: {}", mode.label());

    let rows = rows(rules);
    if rows.is_empty() {
        out.push_str("no rules; every action follows the mode default");
        return out;
    }

    let width = rows
        .iter()
        .map(|row| row.tool.len())
        .max()
        .unwrap_or(0)
        .max("tool".len());
    let _ = writeln!(
        out,
        "\n{:<width$}  {:<6}  {:<10}  pattern",
        "tool", "effect", "source"
    );
    for row in &rows {
        let _ = writeln!(
            out,
            "{:<width$}  {:<6}  {:<10}  {}",
            row.tool,
            row.outcome.as_str(),
            row.layer,
            row.pattern,
        );
    }
    out.trim_end().to_owned()
}

/// Reports the rules as JSON.
#[must_use]
pub fn to_json(rules: &RuleSet, mode: PermissionMode) -> serde_json::Value {
    serde_json::json!({
        "mode": mode.as_str(),
        "rules": rows(rules)
            .iter()
            .map(|row| serde_json::json!({
                "tool": row.tool,
                "pattern": row.pattern,
                "outcome": row.outcome.as_str(),
                "layer": row.layer,
            }))
            .collect::<Vec<_>>(),
    })
}

/// Returns the rules in force, or a failure naming what is wrong with them.
pub fn validated(settings: &Settings) -> Result<RuleSet> {
    let rules = rules_for(settings, &RuleSet::new());
    rules.validate()?;
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_settings() -> Settings {
        Settings::default()
    }

    #[test]
    fn a_fresh_install_allows_reading_without_asking() {
        let rules = builtin_rules(&PermissionMode::Auto);
        let decision = rules.evaluate("read_file", "src/main.rs", Outcome::Ask);
        assert_eq!(decision.outcome, Outcome::Allow);
    }

    #[test]
    fn a_fresh_install_refuses_outbound_requests() {
        let rules = builtin_rules(&PermissionMode::Auto);
        for tool in ["web_fetch", "web_search"] {
            let decision = rules.evaluate(tool, "https://example.com", Outcome::Allow);
            assert_eq!(decision.outcome, Outcome::Deny, "{tool} was not refused");
        }
    }

    #[test]
    fn a_refusal_of_outbound_traffic_survives_full_access() {
        // The rule is declared in every mode, so the only way past it is to
        // change the rule rather than the mode.
        let rules = builtin_rules(&PermissionMode::FullAccess);
        assert_eq!(
            rules
                .evaluate("web_fetch", "https://example.com", Outcome::Allow)
                .outcome,
            Outcome::Deny
        );
    }

    #[test]
    fn a_plain_read_command_is_allowed_without_a_prompt() {
        let rules = builtin_rules(&PermissionMode::Auto);
        assert_eq!(
            rules
                .evaluate("shell", "git status --short", Outcome::Ask)
                .outcome,
            Outcome::Allow
        );
    }

    #[test]
    fn a_destructive_command_still_asks() {
        let rules = builtin_rules(&PermissionMode::Auto);
        assert_eq!(
            rules
                .evaluate("shell", "rm -rf /tmp/x", Outcome::Ask)
                .outcome,
            Outcome::Ask
        );
    }

    #[test]
    fn ask_mode_leaves_the_shell_unanswered() {
        // In ask mode the mode default is what surfaces the prompt, so the shell
        // must not carry a built-in allow.
        let rules = builtin_rules(&PermissionMode::Ask);
        assert!(
            rules.rules().iter().all(|rule| rule.tool != "shell"),
            "ask mode shipped a shell rule"
        );
    }

    #[test]
    fn a_user_rule_is_reported_alongside_the_built_in_ones() {
        let settings = default_settings();
        let mut extra = RuleSet::new();
        extra.push(Rule::deny("shell", "rm -rf *", Layer::User));
        let rules = rules_for(&settings, &extra);
        let rows = rows(&rules);
        assert!(
            rows.iter()
                .any(|row| row.tool == "shell" && row.pattern == "rm -rf *" && row.layer == "user"),
            "{rows:#?}"
        );
    }

    #[test]
    fn a_more_specific_ruleset_overrides_a_built_in() {
        let settings = default_settings();
        let mut extra = RuleSet::new();
        extra.push(Rule::deny("read_file", "secrets/*", Layer::User));
        let rules = rules_for(&settings, &extra);
        assert_eq!(
            rules
                .evaluate("read_file", "secrets/key", Outcome::Allow)
                .outcome,
            Outcome::Deny
        );
        // A path the user rule does not cover keeps the built-in answer.
        assert_eq!(
            rules
                .evaluate("read_file", "src/main.rs", Outcome::Allow)
                .outcome,
            Outcome::Allow
        );
    }

    #[test]
    fn explain_names_the_effect_and_the_reason() {
        let rules = builtin_rules(&PermissionMode::Auto);
        let text = explain(&rules, PermissionMode::Auto, "shell", "git status");
        assert!(text.contains("allowed"), "{text}");
        assert!(text.contains("git status"), "{text}");
    }

    #[test]
    fn explain_says_when_a_prompt_is_coming() {
        let rules = builtin_rules(&PermissionMode::Auto);
        let text = explain(&rules, PermissionMode::Auto, "shell", "rm -rf /");
        assert!(text.contains("asks first"), "{text}");
    }

    #[test]
    fn the_rendered_table_has_a_row_per_rule() {
        let rules = builtin_rules(&PermissionMode::Auto);
        let rendered = render(&rules, PermissionMode::Auto);
        assert!(rendered.contains("mode: auto"), "{rendered}");
        assert!(rendered.contains("read_file"), "{rendered}");
        assert!(rendered.contains("pattern"), "{rendered}");
        // The mode line, a blank, the column header, then one row per rule.
        assert_eq!(
            rendered.lines().count(),
            rules.len().saturating_add(3),
            "{rendered}"
        );
    }

    #[test]
    fn rendering_an_empty_ruleset_says_the_mode_decides() {
        let rendered = render(&RuleSet::new(), PermissionMode::Ask);
        assert!(rendered.contains("mode: ask"), "{rendered}");
        assert!(rendered.contains("mode default"), "{rendered}");
    }

    #[test]
    fn the_reported_json_lists_every_rule() {
        let rules = builtin_rules(&PermissionMode::Auto);
        let value = to_json(&rules, PermissionMode::Auto);
        assert_eq!(value["mode"], "auto");
        assert_eq!(
            value["rules"].as_array().map(Vec::len),
            Some(rules.len()),
            "{value:#?}"
        );
    }

    #[test]
    fn the_built_in_rules_validate() {
        for mode in [
            PermissionMode::Ask,
            PermissionMode::Auto,
            PermissionMode::FullAccess,
        ] {
            builtin_rules(&mode)
                .validate()
                .expect("the built-ins are valid");
        }
    }

    #[test]
    fn the_validated_ruleset_carries_the_builtins_and_the_web_setting() {
        // The built-in set refuses the web tools, because it is built without
        // the configuration. Enabling them adds an allow above that refusal, so
        // the reported set is the built-ins plus one rule per web tool.
        let settings = default_settings();
        let rules = validated(&settings).expect("valid");
        let builtins = builtin_rules(&settings.permission_mode).len();
        let expected = builtins.saturating_add(2);
        assert_eq!(rules.len(), expected);
        assert!(
            settings.web_tools,
            "web tools should be on unless the run says otherwise"
        );
    }

    #[test]
    fn the_web_tools_are_refused_when_the_setting_is_off() {
        // Turning the setting off must leave the refusal in place, so the tools
        // cannot be used by a run that asked not to reach the network.
        let settings = Settings {
            web_tools: false,
            ..default_settings()
        };
        let rules = validated(&settings).expect("valid");
        assert_eq!(rules.len(), builtin_rules(&settings.permission_mode).len());
    }

    #[test]
    fn an_offline_run_does_not_enable_the_web_tools() {
        // Offline refuses every request, so allowing the tools would only make
        // them fail later with a different message.
        let settings = Settings {
            web_tools: true,
            offline: true,
            ..default_settings()
        };
        let rules = validated(&settings).expect("valid");
        assert_eq!(rules.len(), builtin_rules(&settings.permission_mode).len());
    }

    #[test]
    fn the_effect_names_are_the_wire_names() {
        assert_eq!(Outcome::Allow.as_str(), "allow");
        assert_eq!(Outcome::Deny.as_str(), "deny");
        assert_eq!(Outcome::Ask.as_str(), "ask");
    }
}
