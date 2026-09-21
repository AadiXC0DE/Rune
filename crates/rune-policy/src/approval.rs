//! The approval flow and the session grants it can create.
//!
//! A prompt is shown for one scope, and the choice that remembers the answer
//! records exactly the scope shown. Both come from [`Scope`], so the string a
//! user reads and the string a grant stores cannot drift apart.
//!
//! Grants live in memory for one session. They are never written to
//! configuration and never restored, which is why they live in this crate
//! rather than in the settings model.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::decision::{Decision, Outcome};
use crate::rules::glob_match;

/// Tools whose target is a command line rather than a name.
///
/// A grant for one of these matches the exact target: a pattern would let the
/// model run a command the user never saw.
const COMMAND_TOOLS: &[&str] = &["shell", "bash", "exec"];

/// The three answers a user can give.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    /// Run the action now and ask again next time.
    RunOnce,
    /// Run it and remember the scope for the rest of the session.
    RememberForSession,
    /// Do not run it.
    Deny,
}

impl ApprovalOutcome {
    /// Every outcome, in the order the prompt offers them.
    pub const ALL: [Self; 3] = [Self::RunOnce, Self::RememberForSession, Self::Deny];

    /// Returns the wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunOnce => "run_once",
            Self::RememberForSession => "remember_for_session",
            Self::Deny => "deny",
        }
    }

    /// Returns the label shown on the choice.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::RunOnce => "Run once",
            Self::RememberForSession => "Remember for this session",
            Self::Deny => "Deny",
        }
    }

    /// Returns true when the answer records a grant.
    #[must_use]
    pub const fn remembers(self) -> bool {
        matches!(self, Self::RememberForSession)
    }

    /// Applies the answer, recording a grant when the answer asks for one.
    pub fn apply(self, grants: &mut Grants, scope: &Scope) -> Outcome {
        match self {
            Self::RunOnce => Outcome::Allow,
            Self::RememberForSession => {
                grants.insert(scope.grant());
                Outcome::Allow
            }
            Self::Deny => Outcome::Deny,
        }
    }
}

impl fmt::Display for ApprovalOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One choice on a prompt.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Choice {
    /// Position on the prompt, counted from one.
    pub index: u8,
    /// The text shown.
    pub label: String,
    /// What the choice does.
    pub outcome: ApprovalOutcome,
}

/// The scope a prompt shows and a grant stores.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Scope {
    tool: String,
    target: String,
}

impl Scope {
    /// Builds a scope from a tool and the target it was called with.
    #[must_use]
    pub fn new(tool: &str, target: &str) -> Self {
        Self {
            tool: tool.to_owned(),
            target: target.to_owned(),
        }
    }

    /// Returns the tool.
    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// Returns the scope string, which is what a grant matches against.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Returns the grant this scope records.
    #[must_use]
    pub fn grant(&self) -> SessionGrant {
        SessionGrant::new(self.tool.clone(), self.target.clone())
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.tool, self.target)
    }
}

/// One grant recorded for the current session.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SessionGrant {
    /// The tool the grant applies to.
    pub tool: String,
    /// The scope the grant covers.
    pub target: String,
}

impl SessionGrant {
    /// Builds a grant.
    #[must_use]
    pub fn new(tool: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            target: target.into(),
        }
    }

    /// Returns true when the grant covers a call.
    ///
    /// A command grant matches its exact target. A grant for any other tool may
    /// use a pattern, because a path or a query is a category the user can see
    /// the shape of.
    #[must_use]
    pub fn matches(&self, tool: &str, target: &str) -> bool {
        if self.tool != tool {
            return false;
        }
        if is_command_tool(tool) {
            return self.target == target;
        }
        glob_match(&self.target, target)
    }
}

/// Returns true when a tool's target is a command line.
#[must_use]
pub fn is_command_tool(tool: &str) -> bool {
    COMMAND_TOOLS.contains(&tool)
}

/// The grants recorded for the current session.
///
/// The list is not capped: the only way to add a grant is a person answering a
/// prompt, and each grant costs one small string pair. Duplicates are refused,
/// so answering the same prompt repeatedly does not grow it.
#[derive(Clone, Debug, Default)]
pub struct Grants {
    entries: Vec<SessionGrant>,
}

impl Grants {
    /// Returns an empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a grant, returning false when an identical grant is present.
    pub fn insert(&mut self, grant: SessionGrant) -> bool {
        if self.entries.contains(&grant) {
            return false;
        }
        self.entries.push(grant);
        true
    }

    /// Returns true when a recorded grant covers a call.
    #[must_use]
    pub fn allows(&self, tool: &str, target: &str) -> bool {
        self.entries.iter().any(|grant| grant.matches(tool, target))
    }

    /// Drops every grant.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Returns the number of grants.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when no grant is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns every grant.
    #[must_use]
    pub fn entries(&self) -> &[SessionGrant] {
        &self.entries
    }
}

/// What a user is asked, and what each answer does.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Prompt {
    /// The question, naming the tool.
    pub title: String,
    /// The exact scope that is run and, for `RememberForSession`, stored.
    pub detail: String,
    /// Why the action is unresolved, from the decision that raised the prompt.
    pub reason: String,
    /// The choices, in the order they are offered.
    pub options: Vec<Choice>,
}

impl Prompt {
    /// Builds the prompt for one scope.
    #[must_use]
    pub fn new(scope: &Scope, decision: &Decision) -> Self {
        Self {
            title: format!("Allow `{}` for the scope below?", scope.tool()),
            detail: scope.target().to_owned(),
            reason: decision.explain(),
            options: ApprovalOutcome::ALL
                .iter()
                .enumerate()
                .map(|(position, outcome)| Choice {
                    index: u8::try_from(position.saturating_add(1)).unwrap_or(u8::MAX),
                    label: outcome.label().to_owned(),
                    outcome: *outcome,
                })
                .collect(),
        }
    }

    /// Returns the choices.
    #[must_use]
    pub fn options(&self) -> &[Choice] {
        &self.options
    }

    /// Returns the choice at a one-based position.
    #[must_use]
    pub fn choice(&self, index: u8) -> Option<&Choice> {
        self.options.iter().find(|choice| choice.index == index)
    }

    /// Returns the scope that is run and stored.
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.detail
    }
}

/// The result of resolving a decision against the session grants.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Resolution {
    /// The action is settled.
    Decided(Outcome),
    /// A human has to choose.
    NeedsPrompt(Prompt),
    /// A human would have to choose, and there is nobody to ask.
    InputUnavailable,
}

impl Resolution {
    /// Returns the settled outcome, when there is one.
    #[must_use]
    pub const fn outcome(&self) -> Option<Outcome> {
        match self {
            Self::Decided(outcome) => Some(*outcome),
            Self::NeedsPrompt(_) | Self::InputUnavailable => None,
        }
    }

    /// Returns true when the action may run.
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Decided(Outcome::Allow))
    }

    /// Returns the prompt, when one is needed.
    #[must_use]
    pub const fn prompt(&self) -> Option<&Prompt> {
        match self {
            Self::NeedsPrompt(prompt) => Some(prompt),
            Self::Decided(_) | Self::InputUnavailable => None,
        }
    }
}

/// Resolves a policy decision into either a settled outcome or a question.
///
/// A recorded grant for the same scope allows the call without asking again. A
/// decision to deny is a hard block and no grant lifts it. An unresolved `ask`
/// needs a person: without one the result is [`Resolution::InputUnavailable`],
/// which callers report as a request for input rather than running the action.
#[must_use]
pub fn resolve(
    decision: &Decision,
    grants: &Grants,
    tool: &str,
    target: &str,
    interactive: bool,
) -> Resolution {
    if decision.is_denied() {
        return Resolution::Decided(Outcome::Deny);
    }
    if decision.is_allowed() || grants.allows(tool, target) {
        return Resolution::Decided(Outcome::Allow);
    }
    if !interactive {
        return Resolution::InputUnavailable;
    }
    Resolution::NeedsPrompt(Prompt::new(&Scope::new(tool, target), decision))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::Layer;

    fn asking(tool: &str, target: &str) -> Decision {
        Decision::matched(Outcome::Ask, Layer::Default, format!("{tool} {target}"))
    }

    fn prompt_for(tool: &str, target: &str) -> Prompt {
        let grants = Grants::new();
        let resolution = resolve(&asking(tool, target), &grants, tool, target, true);
        resolution.prompt().expect("prompt").clone()
    }

    #[test]
    fn a_prompt_offers_exactly_three_labelled_choices() {
        let prompt = prompt_for("shell", "npm test");
        let options = prompt.options();
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].index, 1);
        assert_eq!(options[1].index, 2);
        assert_eq!(options[2].index, 3);
        assert_eq!(options[0].label, "Run once");
        assert_eq!(options[1].label, "Remember for this session");
        assert_eq!(options[2].label, "Deny");
        assert_eq!(options[0].outcome, ApprovalOutcome::RunOnce);
        assert_eq!(options[1].outcome, ApprovalOutcome::RememberForSession);
        assert_eq!(options[2].outcome, ApprovalOutcome::Deny);
        assert_eq!(
            prompt.choice(2).expect("second").outcome,
            ApprovalOutcome::RememberForSession
        );
        assert!(prompt.choice(4).is_none());
        assert!(prompt.title.contains("shell"));
        // The prompt says why the action was not settled.
        assert!(prompt.reason.contains("shell"), "{}", prompt.reason);
        assert!(prompt.reason.contains("ask"), "{}", prompt.reason);
    }

    #[test]
    fn the_stored_scope_equals_the_scope_the_prompt_displays() {
        for (tool, target) in [
            ("shell", "npm test -- --watch=false"),
            ("read_file", "src/lib.rs"),
            ("web_fetch", "domain:example.com"),
        ] {
            let prompt = prompt_for(tool, target);
            assert_eq!(prompt.scope(), target);

            let mut grants = Grants::new();
            let outcome =
                ApprovalOutcome::RememberForSession.apply(&mut grants, &Scope::new(tool, target));

            assert_eq!(outcome, Outcome::Allow);
            assert_eq!(grants.len(), 1);
            let grant = grants.entries().first().expect("grant");
            assert_eq!(grant.target, prompt.scope());
            assert_eq!(grant.target, target);
            assert!(grants.allows(tool, target));
        }
    }

    #[test]
    fn an_unresolved_ask_without_a_person_never_allows() {
        let grants = Grants::new();
        let resolution = resolve(
            &asking("shell", "rm -rf build"),
            &grants,
            "shell",
            "rm -rf build",
            false,
        );
        assert_eq!(resolution, Resolution::InputUnavailable);
        assert_eq!(resolution.outcome(), None);
        assert!(!resolution.is_allow());
        assert!(resolution.prompt().is_none());

        // With a person available the same decision asks instead.
        let interactive = resolve(
            &asking("shell", "rm -rf build"),
            &grants,
            "shell",
            "rm -rf build",
            true,
        );
        assert!(interactive.prompt().is_some());
        assert!(!interactive.is_allow());
    }

    #[test]
    fn a_grant_short_circuits_to_allow() {
        let mut grants = Grants::new();
        grants.insert(SessionGrant::new("shell", "npm test"));
        for interactive in [true, false] {
            let resolution = resolve(
                &asking("shell", "npm test"),
                &grants,
                "shell",
                "npm test",
                interactive,
            );
            assert_eq!(
                resolution,
                Resolution::Decided(Outcome::Allow),
                "interactive {interactive}"
            );
        }
    }

    #[test]
    fn a_deny_is_not_lifted_by_a_grant() {
        let mut grants = Grants::new();
        grants.insert(SessionGrant::new("shell", "npm test"));
        let denied = Decision::matched(Outcome::Deny, Layer::User, "shell npm test");
        assert_eq!(
            resolve(&denied, &grants, "shell", "npm test", true),
            Resolution::Decided(Outcome::Deny)
        );
    }

    #[test]
    fn an_allow_decision_needs_no_prompt() {
        let grants = Grants::new();
        let allowed = Decision::matched(Outcome::Allow, Layer::Project, "ls *");
        assert_eq!(
            resolve(&allowed, &grants, "shell", "ls -la", true),
            Resolution::Decided(Outcome::Allow)
        );
    }

    #[test]
    fn a_command_grant_matches_only_its_exact_target() {
        let grant = SessionGrant::new("shell", "npm test");
        assert!(grant.matches("shell", "npm test"));
        assert!(!grant.matches("shell", "npm test -- --watch=false"));
        assert!(!grant.matches("shell", "npm run test"));
        assert!(!grant.matches("bash", "npm test"));

        // A prefix is not a match either, in either direction.
        let longer = SessionGrant::new("shell", "npm test --watch");
        assert!(!longer.matches("shell", "npm test"));
    }

    #[test]
    fn a_non_command_grant_may_use_a_pattern() {
        let grant = SessionGrant::new("read_file", "src/**");
        assert!(grant.matches("read_file", "src/lib.rs"));
        assert!(grant.matches("read_file", "src/deep/mod.rs"));
        assert!(!grant.matches("read_file", "docs/readme.md"));
        assert!(!grant.matches("write_file", "src/lib.rs"));

        let any = SessionGrant::new("web_search", "*");
        assert!(any.matches("web_search", "rust edition 2024 release notes"));
    }

    #[test]
    fn a_run_once_answer_records_nothing() {
        let mut grants = Grants::new();
        let outcome = ApprovalOutcome::RunOnce.apply(&mut grants, &Scope::new("shell", "npm test"));
        assert_eq!(outcome, Outcome::Allow);
        assert!(grants.is_empty());
        assert!(!grants.allows("shell", "npm test"));
    }

    #[test]
    fn a_deny_answer_records_nothing_and_denies() {
        let mut grants = Grants::new();
        let outcome = ApprovalOutcome::Deny.apply(&mut grants, &Scope::new("shell", "npm test"));
        assert_eq!(outcome, Outcome::Deny);
        assert!(grants.is_empty());
    }

    #[test]
    fn remembering_the_same_scope_twice_records_one_grant() {
        let mut grants = Grants::new();
        let scope = Scope::new("shell", "npm test");
        ApprovalOutcome::RememberForSession.apply(&mut grants, &scope);
        ApprovalOutcome::RememberForSession.apply(&mut grants, &scope);
        assert_eq!(grants.len(), 1);
        assert!(ApprovalOutcome::RememberForSession.remembers());
        assert!(!ApprovalOutcome::RunOnce.remembers());
    }

    #[test]
    fn clearing_grants_drops_every_grant() {
        let mut grants = Grants::new();
        grants.insert(SessionGrant::new("shell", "npm test"));
        grants.insert(SessionGrant::new("read_file", "src/**"));
        assert_eq!(grants.len(), 2);
        assert!(grants.allows("shell", "npm test"));
        assert!(grants.allows("read_file", "src/lib.rs"));

        grants.clear();
        assert!(grants.is_empty());
        assert_eq!(grants.len(), 0);
        assert!(!grants.allows("shell", "npm test"));
        assert!(!grants.allows("read_file", "src/lib.rs"));

        // A fresh session starts with no grants, which is what resume produces.
        assert!(!Grants::new().allows("shell", "npm test"));
    }

    #[test]
    fn inserting_an_identical_grant_is_refused() {
        let mut grants = Grants::new();
        assert!(grants.insert(SessionGrant::new("shell", "npm test")));
        assert!(!grants.insert(SessionGrant::new("shell", "npm test")));
        assert!(grants.insert(SessionGrant::new("shell", "npm run lint")));
        assert_eq!(grants.len(), 2);
    }

    #[test]
    fn approvals_and_resolutions_render_their_wire_names() {
        assert_eq!(ApprovalOutcome::RunOnce.as_str(), "run_once");
        assert_eq!(
            ApprovalOutcome::RememberForSession.as_str(),
            "remember_for_session"
        );
        assert_eq!(ApprovalOutcome::Deny.to_string(), "deny");
        assert_eq!(Scope::new("shell", "ls").to_string(), "shell ls");
        assert!(is_command_tool("shell"));
        assert!(!is_command_tool("read_file"));
    }
}
