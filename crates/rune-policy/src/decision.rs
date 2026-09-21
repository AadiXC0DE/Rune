//! Permission decisions.
//!
//! A decision always names the rule that produced it and the layer it came
//! from, so `permissions --explain` can answer "why was this denied" without
//! the user reading the configuration. That property is the reason this type
//! exists rather than a bare enum.

use std::fmt;

use rune_core::config::PermissionMode;
use serde::{Deserialize, Serialize};

/// What a policy decided.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Run the action.
    Allow,
    /// Ask the user before running it.
    Ask,
    /// Refuse the action.
    Deny,
}

impl Outcome {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }

    /// Returns the label shown in the interface.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a rule came from.
///
/// Ordered from most to least specific, which is the order evaluation follows
/// after session state.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// The compiled default for the tool.
    Default,
    /// A rule from the project configuration.
    Project,
    /// A rule from the user configuration.
    User,
    /// A rule recorded for this session only.
    Session,
    /// A grant created by answering an approval prompt.
    Grant,
}

impl Layer {
    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Project => "project",
            Self::User => "user",
            Self::Session => "session",
            Self::Grant => "grant",
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One policy decision, with its justification.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Decision {
    /// What to do.
    pub outcome: Outcome,
    /// Layer that decided it.
    pub layer: Layer,
    /// The rule that matched, rendered for display.
    pub rule: String,
    /// Every rule considered, most specific first, for `--explain`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub considered: Vec<ConsideredRule>,
}

/// One rule that was evaluated.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ConsideredRule {
    /// The rule as written.
    pub pattern: String,
    /// Its outcome.
    pub outcome: Outcome,
    /// Its layer.
    pub layer: Layer,
    /// Whether it matched the target.
    pub matched: bool,
    /// Why it did or did not match, when that is not obvious.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Decision {
    /// Builds a decision with no rule, used for a mode default.
    #[must_use]
    pub fn default_for(outcome: Outcome, layer: Layer, description: impl Into<String>) -> Self {
        Self {
            outcome,
            layer,
            rule: description.into(),
            considered: Vec::new(),
        }
    }

    /// Builds a decision from a matched rule.
    #[must_use]
    pub fn matched(outcome: Outcome, layer: Layer, rule: impl Into<String>) -> Self {
        Self {
            outcome,
            layer,
            rule: rule.into(),
            considered: Vec::new(),
        }
    }

    /// Records the rules that were evaluated.
    #[must_use]
    pub fn with_considered(mut self, considered: Vec<ConsideredRule>) -> Self {
        self.considered = considered;
        self
    }

    /// Returns true when the action may run without further interaction.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self.outcome, Outcome::Allow)
    }

    /// Returns true when the action must not run.
    #[must_use]
    pub const fn is_denied(&self) -> bool {
        matches!(self.outcome, Outcome::Deny)
    }

    /// Returns the single-line explanation used by `--explain`.
    #[must_use]
    pub fn explain(&self) -> String {
        format!(
            "{}: matched `{}` at the {} layer",
            self.outcome, self.rule, self.layer
        )
    }
}

/// Returns the outcome a mode implies for an otherwise unresolved action.
///
/// Full access resolves everything to allow, because the mode exists precisely
/// to remove the checks. The other two modes resolve to asking, which is what
/// an unresolved sensitive action means.
#[must_use]
pub const fn mode_default(mode: PermissionMode) -> Outcome {
    match mode {
        PermissionMode::FullAccess => Outcome::Allow,
        PermissionMode::Ask | PermissionMode::Auto => Outcome::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_render_their_wire_names() {
        assert_eq!(Outcome::Allow.as_str(), "allow");
        assert_eq!(Outcome::Ask.as_str(), "ask");
        assert_eq!(Outcome::Deny.as_str(), "deny");
    }

    #[test]
    fn layers_are_ordered_from_general_to_specific() {
        assert!(Layer::Default < Layer::Project);
        assert!(Layer::Project < Layer::User);
        assert!(Layer::User < Layer::Session);
        assert!(Layer::Session < Layer::Grant);
    }

    #[test]
    fn full_access_resolves_everything_to_allow() {
        assert_eq!(mode_default(PermissionMode::FullAccess), Outcome::Allow);
    }

    #[test]
    fn ask_and_auto_resolve_to_asking() {
        assert_eq!(mode_default(PermissionMode::Ask), Outcome::Ask);
        assert_eq!(mode_default(PermissionMode::Auto), Outcome::Ask);
    }

    #[test]
    fn a_decision_reports_allowed_or_denied() {
        let allow = Decision::default_for(Outcome::Allow, Layer::Default, "default");
        assert!(allow.is_allowed());
        assert!(!allow.is_denied());

        let deny = Decision::matched(Outcome::Deny, Layer::User, "*");
        assert!(deny.is_denied());
        assert!(!deny.is_allowed());
    }

    #[test]
    fn an_explanation_names_the_rule_and_its_layer() {
        let decision = Decision::matched(Outcome::Deny, Layer::User, "git push *");
        let text = decision.explain();
        assert!(text.contains("deny"), "{text}");
        assert!(text.contains("git push *"), "{text}");
        assert!(text.contains("user"), "{text}");
    }

    #[test]
    fn a_decision_round_trips_through_json() {
        let decision =
            Decision::matched(Outcome::Ask, Layer::Project, "*.rs").with_considered(vec![
                ConsideredRule {
                    pattern: "*.rs".to_owned(),
                    outcome: Outcome::Ask,
                    layer: Layer::Project,
                    matched: true,
                    reason: None,
                },
            ]);
        let json = serde_json::to_string(&decision).expect("serialize");
        let parsed: Decision = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decision, parsed);
    }

    #[test]
    fn an_empty_considered_list_is_omitted_from_json() {
        let decision = Decision::default_for(Outcome::Allow, Layer::Default, "d");
        let json = serde_json::to_value(&decision).expect("serialize");
        assert!(json.get("considered").is_none());
    }

    #[test]
    fn a_decision_serializes_its_outcome_as_a_stable_name() {
        let decision = Decision::default_for(Outcome::Allow, Layer::Default, "d");
        let json = serde_json::to_value(&decision).expect("serialize");
        assert_eq!(json["outcome"], "allow");
        assert_eq!(json["layer"], "default");
    }
}
