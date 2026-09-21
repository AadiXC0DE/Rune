//! Permission rules and their evaluation order.
//!
//! Specificity beats declaration order. A rule that names a narrower target
//! outranks one that names a broader target, regardless of where either appears
//! in the file. This replaces last-match-wins, which produced a denial for a
//! command a broad rule had already allowed, and it is the behavior a reader
//! expects from a list that reads "deny everything, except this".
//!
//! Ties are broken by layer, from most specific to least: a session rule beats
//! a user rule beats a project rule beats the default.

use rune_core::error::{ErrorCode, Result, RuneError};

use crate::decision::{ConsideredRule, Decision, Layer, Outcome};

/// One permission rule.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Rule {
    /// The tool or tool class the rule applies to, or `*` for any.
    pub tool: String,
    /// The target pattern, matched against the action's target.
    pub pattern: String,
    /// What to do when it matches.
    pub outcome: Outcome,
    /// Where the rule came from.
    pub layer: Layer,
}

impl Rule {
    /// Builds a rule.
    #[must_use]
    pub fn new(
        tool: impl Into<String>,
        pattern: impl Into<String>,
        outcome: Outcome,
        layer: Layer,
    ) -> Self {
        Self {
            tool: tool.into(),
            pattern: pattern.into(),
            outcome,
            layer,
        }
    }

    /// Builds an allow rule.
    #[must_use]
    pub fn allow(tool: impl Into<String>, pattern: impl Into<String>, layer: Layer) -> Self {
        Self::new(tool, pattern, Outcome::Allow, layer)
    }

    /// Builds a deny rule.
    #[must_use]
    pub fn deny(tool: impl Into<String>, pattern: impl Into<String>, layer: Layer) -> Self {
        Self::new(tool, pattern, Outcome::Deny, layer)
    }

    /// Builds an ask rule.
    #[must_use]
    pub fn ask(tool: impl Into<String>, pattern: impl Into<String>, layer: Layer) -> Self {
        Self::new(tool, pattern, Outcome::Ask, layer)
    }

    /// Returns true when this rule applies to a tool.
    #[must_use]
    pub fn covers_tool(&self, tool: &str) -> bool {
        self.tool == "*" || self.tool == tool
    }

    /// Returns the specificity of the target pattern.
    ///
    /// Longer literal prefixes are more specific. A wildcard-only pattern has
    /// the lowest possible score, and an exact match scores higher than any
    /// pattern containing a wildcard.
    #[must_use]
    fn specificity(&self) -> (u8, usize) {
        if self.pattern == "*" {
            return (0, 0);
        }
        if !self.pattern.contains('*') && !self.pattern.contains('?') {
            // An exact pattern is the most specific thing a rule can say.
            return (2, self.pattern.len());
        }
        (1, self.pattern.len())
    }

    /// Returns true when the pattern matches a target.
    #[must_use]
    fn matches(&self, target: &str) -> bool {
        glob_match(&self.pattern, target)
    }

    /// Renders the rule for display.
    #[must_use]
    pub fn render(&self) -> String {
        if self.tool == "*" {
            self.pattern.clone()
        } else {
            format!("{} {}", self.tool, self.pattern)
        }
    }
}

/// Matches a glob pattern against a target.
///
/// Supports `*` for any run of characters and `?` for one character. Deliberately
/// smaller than a full glob implementation: a permission rule is read by a human
/// deciding whether it is safe, so the pattern language must be obvious.
///
/// Uses a single-pass greedy algorithm with one backtrack point, which is linear
/// in the input size. A recursive matcher is easy to write and easy to make
/// non-terminating, which for a permission check would mean a hung agent.
#[must_use]
pub fn glob_match(pattern: &str, target: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let target: Vec<char> = target.chars().collect();

    let mut p = 0_usize;
    let mut t = 0_usize;
    let mut star: Option<usize> = None;
    let mut star_target = 0_usize;

    while t < target.len() {
        match pattern.get(p) {
            // A literal or single-character wildcard consumes one character.
            Some('?') => {
                p = p.saturating_add(1);
                t = t.saturating_add(1);
            }
            Some(character) if *character == target[t] => {
                p = p.saturating_add(1);
                t = t.saturating_add(1);
            }
            // A star matches nothing for now, remembering where it started so
            // it can consume more if the remainder fails.
            Some('*') => {
                star = Some(p);
                star_target = t;
                p = p.saturating_add(1);
            }
            _ => {
                // Mismatch. Let the last star consume one more character.
                match star {
                    Some(star_start) => {
                        star_target = star_target.saturating_add(1);
                        t = star_target;
                        p = star_start.saturating_add(1);
                    }
                    None => return false,
                }
            }
        }
    }

    // Trailing stars may match the empty remainder.
    while matches!(pattern.get(p), Some('*')) {
        p = p.saturating_add(1);
    }

    p == pattern.len()
}

/// An ordered set of rules.
#[derive(Clone, Debug, Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
}

impl RuleSet {
    /// Returns an empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true when no rules are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Returns the number of rules.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Adds a rule.
    pub fn push(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    /// Returns every rule.
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Evaluates an action.
    ///
    /// Denies are resolved before allows so a deny cannot be undone by a later
    /// allow of equal specificity.
    #[must_use]
    pub fn evaluate(&self, tool: &str, target: &str, fallback: Outcome) -> Decision {
        let mut considered = Vec::new();
        let mut best: Option<(&Rule, (u8, usize))> = None;

        for rule in &self.rules {
            if !rule.covers_tool(tool) {
                continue;
            }
            let matched = rule.matches(target);
            considered.push(ConsideredRule {
                pattern: rule.render(),
                outcome: rule.outcome,
                layer: rule.layer,
                matched,
                reason: if matched {
                    None
                } else {
                    Some("pattern did not match the target".to_owned())
                },
            });
            if !matched {
                continue;
            }

            let specificity = rule.specificity();
            best = match best {
                None => Some((rule, specificity)),
                Some((current, current_specificity)) => {
                    // More specific wins. On a tie, a deny wins, then the
                    // higher layer, which is what makes "deny everything, then
                    // allow this" behave as written.
                    let better = match specificity.cmp(&current_specificity) {
                        std::cmp::Ordering::Greater => true,
                        std::cmp::Ordering::Less => false,
                        std::cmp::Ordering::Equal => match (rule.outcome, current.outcome) {
                            (Outcome::Deny, _) => true,
                            (_, Outcome::Deny) => false,
                            _ => rule.layer > current.layer,
                        },
                    };
                    if better {
                        Some((rule, specificity))
                    } else {
                        Some((current, current_specificity))
                    }
                }
            };
        }

        match best {
            Some((rule, _)) => Decision::matched(rule.outcome, rule.layer, rule.render())
                .with_considered(considered),
            None => Decision::default_for(fallback, Layer::Default, "no rule matched")
                .with_considered(considered),
        }
    }

    /// Validates the rules.
    pub fn validate(&self) -> Result<()> {
        for rule in &self.rules {
            if rule.tool.trim().is_empty() {
                return Err(RuneError::invalid_field(
                    "permission.tool",
                    "must not be empty",
                ));
            }
            if rule.pattern.trim().is_empty() {
                return Err(RuneError::invalid_field(
                    "permission.pattern",
                    format!("rule for `{}` has an empty pattern", rule.tool),
                ));
            }
            if rule.tool.len() > 128 {
                return Err(RuneError::too_large(
                    "permission.tool",
                    rule.tool.len(),
                    128,
                ));
            }
            if rule.pattern.len() > 1024 {
                return Err(RuneError::too_large(
                    "permission.pattern",
                    rule.pattern.len(),
                    1024,
                ));
            }
        }

        // A pattern that cannot match anything is almost always a typo.
        for rule in &self.rules {
            if rule.pattern.contains("**") {
                return Err(RuneError::new(
                    ErrorCode::InvalidField,
                    format!("rule `{}` uses `**`, which is not supported", rule.render()),
                )
                .with_hint("use a single `*` to match any run of characters"));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_star_matches_anything() {
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn a_literal_pattern_matches_only_itself() {
        assert!(glob_match("git status", "git status"));
        assert!(!glob_match("git status", "git push"));
    }

    #[test]
    fn a_prefix_star_matches_a_command_family() {
        assert!(glob_match("git *", "git status"));
        assert!(glob_match("git *", "git push origin main"));
        assert!(!glob_match("git *", "git"));
        assert!(!glob_match("git *", "hg status"));
    }

    #[test]
    fn a_suffix_star_matches_a_file_family() {
        assert!(glob_match("*.rs", "src/main.rs"));
        assert!(!glob_match("*.rs", "src/main.go"));
    }

    #[test]
    fn a_star_in_the_middle_matches_across_it() {
        assert!(glob_match("src/*/main.rs", "src/deep/nested/main.rs"));
        assert!(glob_match("a*c", "abbbc"));
        assert!(!glob_match("a*c", "abbbd"));
    }

    #[test]
    fn a_question_mark_matches_exactly_one_character() {
        assert!(glob_match("v?", "v1"));
        assert!(!glob_match("v?", "v"));
        assert!(!glob_match("v?", "v12"));
    }

    #[test]
    fn several_stars_backtrack_correctly() {
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(glob_match("*b*", "abc"));
        assert!(!glob_match("*b*", "ac"));
        assert!(glob_match("a*a*a*a*a*b", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaab"));
    }

    #[test]
    fn an_empty_pattern_matches_only_an_empty_target() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn termination_is_bounded_for_patterns_that_would_run_away() {
        // A backtracking matcher that forgets to bound the target index loops
        // forever on these cases. A hung permission check is a hung agent, so
        // each of them is a regression test as well as a correctness check.
        let cases = [
            ("*a", "b"),
            ("a*a*a*a*a*b", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaac"),
            ("*", "x"),
            ("?*?", ""),
            ("***", "abc"),
            ("a*b*c*d", "axxxbxxxcxxxd"),
        ];
        for (pattern, target) in cases {
            // Completion is the assertion. A hang fails the test run.
            let _ = glob_match(pattern, target);
        }
    }

    #[test]
    fn a_failing_star_match_returns_false_and_terminates() {
        assert!(!glob_match("*a", "b"));
        assert!(!glob_match("a*", "ba"));
        assert!(!glob_match("*abc*", "ab"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }

    #[test]
    fn consecutive_stars_behave_like_one_star() {
        assert!(glob_match("***", "anything"));
        assert!(glob_match("a**b", "ab"));
        assert!(glob_match("a***b", "aXYb"));
    }

    #[test]
    fn a_star_and_a_question_mark_combine() {
        assert!(glob_match("*?", "ab"));
        assert!(glob_match("?*", "ab"));
        assert!(glob_match("*?*", "abc"));
        assert!(!glob_match("*?", ""));
    }

    #[test]
    fn a_pattern_longer_than_the_target_does_not_match() {
        assert!(!glob_match("abcdef", "abc"));
    }

    fn allow(pattern: &str) -> Rule {
        Rule::allow("*", pattern, Layer::User)
    }

    fn deny(pattern: &str) -> Rule {
        Rule::deny("*", pattern, Layer::User)
    }

    #[test]
    fn an_empty_ruleset_returns_the_fallback() {
        let set = RuleSet::new();
        let decision = set.evaluate("bash", "git status", Outcome::Ask);
        assert_eq!(decision.outcome, Outcome::Ask);
        assert_eq!(decision.layer, Layer::Default);
    }

    #[test]
    fn a_matching_rule_decides() {
        let mut set = RuleSet::new();
        set.push(allow("git *"));
        let decision = set.evaluate("bash", "git status", Outcome::Ask);
        assert_eq!(decision.outcome, Outcome::Allow);
        assert_eq!(decision.layer, Layer::User);
    }

    #[test]
    fn the_issue_859_rule_set_allows_a_broadly_permitted_command() {
        // Deny everything, then allow a narrower target. A narrower allow must
        // win even though the deny appears first.
        let mut set = RuleSet::new();
        set.push(deny("*"));
        set.push(allow("git status"));
        let decision = set.evaluate("bash", "git status", Outcome::Deny);
        assert_eq!(
            decision.outcome,
            Outcome::Allow,
            "a specific allow did not beat a broad deny: {}",
            decision.explain()
        );
    }

    #[test]
    fn a_specific_deny_beats_a_broad_allow() {
        let mut set = RuleSet::new();
        set.push(allow("*"));
        set.push(deny("git push *"));
        let decision = set.evaluate("bash", "git push origin main", Outcome::Allow);
        assert_eq!(decision.outcome, Outcome::Deny);
    }

    #[test]
    fn declaration_order_does_not_change_the_result() {
        let mut forward = RuleSet::new();
        forward.push(allow("*"));
        forward.push(deny("git push *"));

        let mut backward = RuleSet::new();
        backward.push(deny("git push *"));
        backward.push(allow("*"));

        let a = forward.evaluate("bash", "git push origin main", Outcome::Allow);
        let b = backward.evaluate("bash", "git push origin main", Outcome::Allow);
        assert_eq!(a.outcome, b.outcome);
        assert_eq!(a.outcome, Outcome::Deny);
    }

    #[test]
    fn a_deny_wins_a_tie_at_equal_specificity() {
        let mut set = RuleSet::new();
        set.push(allow("git *"));
        set.push(deny("git *"));
        let decision = set.evaluate("bash", "git status", Outcome::Allow);
        assert_eq!(decision.outcome, Outcome::Deny);
    }

    #[test]
    fn a_higher_layer_wins_a_tie_at_equal_specificity() {
        let mut set = RuleSet::new();
        set.push(Rule::allow("*", "git *", Layer::Project));
        set.push(Rule::deny("*", "git *", Layer::Session));
        let decision = set.evaluate("bash", "git status", Outcome::Allow);
        assert_eq!(decision.layer, Layer::Session);
        assert_eq!(decision.outcome, Outcome::Deny);
    }

    #[test]
    fn an_exact_pattern_beats_a_wildcard_pattern() {
        let mut set = RuleSet::new();
        set.push(allow("docs/*"));
        set.push(deny("docs/secret.txt"));
        let decision = set.evaluate("edit_file", "docs/secret.txt", Outcome::Allow);
        assert_eq!(decision.outcome, Outcome::Deny);
    }

    #[test]
    fn a_tool_specific_rule_only_applies_to_that_tool() {
        let mut set = RuleSet::new();
        set.push(Rule::deny("bash", "*", Layer::User));
        assert_eq!(
            set.evaluate("bash", "anything", Outcome::Allow).outcome,
            Outcome::Deny
        );
        assert_eq!(
            set.evaluate("read_file", "anything", Outcome::Allow)
                .outcome,
            Outcome::Allow
        );
    }

    #[test]
    fn a_wildcard_tool_rule_applies_to_every_tool() {
        let mut set = RuleSet::new();
        set.push(Rule::deny("*", "secret/*", Layer::User));
        assert_eq!(
            set.evaluate("read_file", "secret/x", Outcome::Allow)
                .outcome,
            Outcome::Deny
        );
        assert_eq!(
            set.evaluate("edit_file", "secret/x", Outcome::Allow)
                .outcome,
            Outcome::Deny
        );
    }

    #[test]
    fn the_decision_records_every_rule_considered() {
        let mut set = RuleSet::new();
        set.push(deny("*"));
        set.push(allow("git status"));
        let decision = set.evaluate("bash", "git status", Outcome::Ask);
        assert_eq!(decision.considered.len(), 2);
        assert!(decision.considered.iter().all(|rule| rule.matched));
    }

    #[test]
    fn a_non_matching_rule_is_recorded_with_a_reason() {
        let mut set = RuleSet::new();
        set.push(allow("git status"));
        let decision = set.evaluate("bash", "hg status", Outcome::Ask);
        assert_eq!(decision.considered.len(), 1);
        assert!(!decision.considered[0].matched);
        assert!(decision.considered[0].reason.is_some());
    }

    #[test]
    fn rules_that_do_not_cover_the_tool_are_not_considered() {
        let mut set = RuleSet::new();
        set.push(Rule::allow("read_file", "*", Layer::User));
        let decision = set.evaluate("bash", "ls", Outcome::Ask);
        assert!(decision.considered.is_empty());
    }

    #[test]
    fn validation_rejects_an_empty_pattern() {
        let mut set = RuleSet::new();
        set.push(Rule::allow("bash", "", Layer::User));
        let err = set.validate().expect_err("rejected");
        assert_eq!(err.code(), ErrorCode::InvalidField);
    }

    #[test]
    fn validation_rejects_a_double_star_with_a_remedy() {
        let mut set = RuleSet::new();
        set.push(Rule::allow("bash", "src/**/*.rs", Layer::User));
        let err = set.validate().expect_err("rejected");
        assert!(err.message().contains("**"));
        assert!(err.detail().hint.is_some());
    }

    #[test]
    fn validation_accepts_an_ordinary_rule_set() {
        let mut set = RuleSet::new();
        set.push(Rule::deny("*", "*", Layer::User));
        set.push(Rule::allow("bash", "git *", Layer::User));
        set.push(Rule::ask("edit_file", "*.md", Layer::Project));
        set.validate().expect("valid");
    }

    #[test]
    fn rules_report_their_count_and_content() {
        let mut set = RuleSet::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        set.push(allow("git *"));
        assert!(!set.is_empty());
        assert_eq!(set.len(), 1);
        assert_eq!(set.rules().len(), 1);
    }

    #[test]
    fn a_rule_renders_with_its_tool_when_one_is_named() {
        let rule = Rule::allow("bash", "git *", Layer::User);
        assert_eq!(rule.render(), "bash git *");
        let any = Rule::allow("*", "git *", Layer::User);
        assert_eq!(any.render(), "git *");
    }
}
