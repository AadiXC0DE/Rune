//! Automatic safety review for `auto` mode.
//!
//! An unresolved sensitive call in `auto` mode is sent to a reviewer model
//! before it runs. The reviewer sees the real action, never a masked form, and
//! answers with one structured decision: `clear` or `caution`. A caution holds
//! the action and returns guidance to the agent. A clear authorizes the exact
//! action that was reviewed and nothing else. Any other answer leaves the
//! action unreviewed, and an unreviewed action is held.
//!
//! Two properties are load bearing, and they are why this is a module rather
//! than a function:
//!
//! - A clear names the action it reviewed, so a later action that differs by
//!   one character is not covered by it.
//! - An unavailable review is not a judgment. It is never cached, and the same
//!   action spends at most one attempt per turn, so a reviewer that is down
//!   cannot be asked the same question forever.
//!
//! The transport belongs to the caller. This module owns the protocol, the
//! parse, and the per-turn budget, so the policy can be tested against a
//! scripted reviewer instead of a network.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use rune_core::budget::{BudgetSet, LimitName};
use rune_core::error::{ErrorCode, Result, RuneError};

/// Reviewer model compiled in for each built-in provider.
///
/// A review is one narrow safety question, so the reviewer is a small, fast
/// model rather than the session model. The choice is fixed rather than
/// inherited, so changing the session model cannot redirect the review.
const FIXED_REVIEWERS: &[(&str, &str)] = &[
    ("anthropic", "claude-haiku-4-5"),
    ("chat_completions", "gpt-5-mini"),
    ("responses", "gpt-5-mini"),
];

/// Reviewer used when a provider names none.
const DEFAULT_REVIEWER: &str = "claude-haiku-4-5";

/// A decision model qualifies as a review even when it is only the first word
/// of what it calls a hold.
const CLEAR: &str = "clear";
/// The prefix a caution line carries before its reason.
const CAUTION: &str = "caution";

/// Which model reviews an action.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReviewerKind {
    /// The compiled reviewer for a provider. A configured override does not
    /// apply to this provider.
    Fixed {
        /// The model identifier.
        model: String,
    },
    /// A reviewer chosen by the user, on a provider whose reviewer is
    /// selectable.
    Custom {
        /// The model identifier.
        model: String,
    },
}

impl ReviewerKind {
    /// Returns the model identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        match self {
            Self::Fixed { model } | Self::Custom { model } => model,
        }
    }

    /// Returns true when the model is the compiled choice for the provider.
    #[must_use]
    pub const fn is_fixed(&self) -> bool {
        matches!(self, Self::Fixed { .. })
    }
}

/// Returns the reviewer for a provider.
///
/// The built-in providers have a compiled reviewer; an endpoint declared in the
/// user configuration has none, so its reviewer is whatever the connection
/// declares. On a built-in provider a configured override is not consulted, and
/// the returned kind says so by being [`ReviewerKind::Fixed`]. An endpoint that
/// declares no reviewer gets the compiled default, which the endpoint may not
/// serve; a review that cannot complete is held rather than skipped.
#[must_use]
pub fn reviewer_for(provider: &str, configured: Option<&str>) -> ReviewerKind {
    let configured = configured.map(str::trim).filter(|model| !model.is_empty());
    if accepts_review_override(provider)
        && let Some(model) = configured
    {
        return ReviewerKind::Custom {
            model: model.to_owned(),
        };
    }
    let compiled = FIXED_REVIEWERS
        .iter()
        .find(|(name, _)| *name == provider)
        .map_or(DEFAULT_REVIEWER, |(_, model)| *model);
    ReviewerKind::Fixed {
        model: compiled.to_owned(),
    }
}

/// Returns true when a provider's reviewer can be chosen by the user.
#[must_use]
pub fn accepts_review_override(provider: &str) -> bool {
    !FIXED_REVIEWERS.iter().any(|(name, _)| *name == provider)
}

/// One action submitted for review.
///
/// The action and every target are recorded exactly as they will run. Masking
/// them would leave the reviewer deciding about something other than what
/// executes, which is the one failure the review exists to prevent. Evidence
/// from earlier in the turn is the only part that is bounded, because it is
/// another tool's output rather than the action itself.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ReviewRequest {
    /// The action, exactly as it will run.
    pub action: String,
    /// Every target the action touches, exactly as written.
    pub targets: Vec<String>,
    /// Where the call came from, such as the tool or the parent agent.
    pub origin: String,
    /// The tool call this review answers for.
    pub call_id: String,
    /// Bounded excerpts of the turn's earlier tool results.
    pub prior_evidence: Vec<String>,
    truncated: bool,
}

impl ReviewRequest {
    /// Builds a request with no evidence.
    #[must_use]
    pub fn new(
        action: impl Into<String>,
        targets: Vec<String>,
        origin: impl Into<String>,
        call_id: impl Into<String>,
    ) -> Self {
        Self {
            action: action.into(),
            targets,
            origin: origin.into(),
            call_id: call_id.into(),
            prior_evidence: Vec::new(),
            truncated: false,
        }
    }

    /// Adds one excerpt of earlier tool output.
    ///
    /// The excerpt is cut to the remaining part of
    /// [`LimitName::ReviewContextBytes`], which bounds one entry and the total
    /// together: evidence is context for a decision, not the decision, and an
    /// unbounded result would displace the action being reviewed. Once the
    /// budget is spent, later excerpts are recorded as truncated and dropped.
    pub fn push_evidence(&mut self, evidence: impl AsRef<str>, limits: &BudgetSet) {
        let limit = limits.get_usize(LimitName::ReviewContextBytes);
        let used = self.evidence_bytes();
        let text = evidence.as_ref();
        if used >= limit {
            self.truncated = true;
            return;
        }
        let kept = truncate_bytes(text, limit.saturating_sub(used));
        if kept.len() < text.len() {
            self.truncated = true;
        }
        if kept.is_empty() {
            return;
        }
        self.prior_evidence.push(kept.to_owned());
    }

    /// Returns the bytes of evidence the request carries.
    #[must_use]
    pub fn evidence_bytes(&self) -> usize {
        self.prior_evidence
            .iter()
            .fold(0usize, |total, entry| total.saturating_add(entry.len()))
    }

    /// Returns true when evidence was cut or dropped to fit the bound.
    #[must_use]
    pub const fn evidence_truncated(&self) -> bool {
        self.truncated
    }
}

/// What a reviewer answered.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReviewDecision {
    /// The action may run.
    Clear,
    /// The action is held, with the reason shown to the agent.
    Caution {
        /// Why the reviewer held the action.
        reason: String,
    },
}

impl ReviewDecision {
    /// Parses the reviewer's reply.
    ///
    /// Exactly one decision line is accepted, and surrounding prose is
    /// allowed: a model that explains itself before answering is still
    /// answering. The protocol is one line reading `clear`, or one line
    /// reading `caution: <reason>`. Zero decision lines, two decision lines, an
    /// empty caution, or anything else is rejected, which the caller reports as
    /// an invalid review and may retry once.
    pub fn parse(raw: &str) -> Result<Self> {
        let mut found: Option<Self> = None;
        for line in raw.lines() {
            let Some(decision) = parse_line(line) else {
                continue;
            };
            if found.is_some() {
                return Err(invalid("the reply carries more than one decision"));
            }
            found = Some(decision);
        }
        found.ok_or_else(|| invalid("the reply carries no decision"))
    }

    /// Returns the wire form of the decision.
    #[must_use]
    pub const fn verdict(&self) -> &'static str {
        match self {
            Self::Clear => CLEAR,
            Self::Caution { .. } => CAUTION,
        }
    }
}

/// What a review produced.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReviewOutcome {
    /// The reviewer cleared the named action. Only that exact action is
    /// authorized.
    Clear {
        /// The action the reviewer saw, as the reviewer returned it.
        reviewed_action: String,
    },
    /// The reviewer raised a concern. The action is held.
    Caution {
        /// Why the action is held.
        reason: String,
    },
    /// No verdict was reached, so the action is held. The reason distinguishes
    /// a reviewer failure, a spent attempt, and an exhausted hold budget.
    Unavailable {
        /// Why no verdict was reached.
        reason: String,
    },
    /// The reviewer answered with something that is not one decision, so the
    /// action is held. The caller may retry once with a fresh deadline.
    Invalid,
}

impl ReviewOutcome {
    /// Returns true when the outcome authorizes exactly this action.
    ///
    /// The comparison is exact. A clear for one action says nothing about a
    /// similar action, a longer action, or the same action with one argument
    /// changed.
    #[must_use]
    pub fn authorizes(&self, action: &str) -> bool {
        match self {
            Self::Clear { reviewed_action } => reviewed_action == action,
            Self::Caution { .. } | Self::Unavailable { .. } | Self::Invalid => false,
        }
    }

    /// Returns true when the action is held rather than authorized.
    #[must_use]
    pub const fn holds(&self) -> bool {
        !matches!(self, Self::Clear { .. })
    }

    /// Returns the reason attached to a hold.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Caution { reason } | Self::Unavailable { reason } => Some(reason),
            Self::Clear { .. } | Self::Invalid => None,
        }
    }
}

/// A model that answers review requests.
///
/// The trait exists so the policy runs against scripted answers in a test and
/// against a provider in production. An implementation returns
/// [`ReviewOutcome::Unavailable`] when the request could not be completed, and
/// [`ReviewOutcome::Invalid`] when the reply carried no single decision.
pub trait Reviewer {
    /// Reviews one action.
    fn review(&self, request: &ReviewRequest) -> ReviewOutcome;
}

/// Review activity inside one turn.
///
/// A hold is what the user sees: the action was not run and the agent got
/// guidance. An unavailable attempt is also a hold, because an action with no
/// verdict never runs either, so one limit bounds both. The set of actions that
/// spent an attempt cannot outgrow the hold budget for the same reason.
#[derive(Clone, Debug)]
pub struct ReviewBudget {
    limit: usize,
    holds: usize,
    attempts: usize,
    spent: BTreeSet<String>,
}

impl ReviewBudget {
    /// Builds a budget from the effective limits.
    #[must_use]
    pub fn new(limits: &BudgetSet) -> Self {
        Self {
            limit: limits.get_usize(LimitName::ReviewHoldsPerTurn),
            holds: 0,
            attempts: 0,
            spent: BTreeSet::new(),
        }
    }

    /// Returns the holds permitted in one turn.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Returns the holds taken this turn.
    #[must_use]
    pub const fn holds(&self) -> usize {
        self.holds
    }

    /// Returns the unavailable attempts spent this turn.
    #[must_use]
    pub const fn attempts(&self) -> usize {
        self.attempts
    }

    /// Returns the holds left this turn.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.holds)
    }

    /// Returns true when no further hold can be taken.
    #[must_use]
    pub const fn exhausted(&self) -> bool {
        self.holds >= self.limit
    }

    /// Takes a hold, returning false when the budget is spent.
    pub fn hold(&mut self) -> bool {
        if self.exhausted() {
            return false;
        }
        self.holds = self.holds.saturating_add(1);
        true
    }

    /// Returns true when this action already spent an attempt this turn.
    #[must_use]
    pub fn attempt_spent(&self, action: &str) -> bool {
        self.spent.contains(action)
    }

    /// Records an attempt for an action, returning false when it is not the
    /// first for that action this turn.
    ///
    /// The record exists so the same action is not asked again, not as an
    /// answer: it never authorizes or refuses anything by itself.
    pub fn spend_attempt(&mut self, action: &str) -> bool {
        if self.attempt_spent(action) {
            return false;
        }
        self.attempts = self.attempts.saturating_add(1);
        self.spent.insert(action.to_owned());
        true
    }

    /// Clears the per-turn counters.
    pub fn begin_turn(&mut self) {
        self.holds = 0;
        self.attempts = 0;
        self.spent.clear();
    }
}

/// The review state of one turn.
///
/// Cautions are kept for the current turn: the same action raises the same
/// concern, and re-asking would spend a request to be told the same thing. An
/// unavailable outcome is not kept, so a later action is reviewed normally,
/// while the attempt record keeps the same action from being asked twice.
#[derive(Clone, Debug)]
pub struct ReviewSession {
    budget: ReviewBudget,
    cautions: Vec<(String, String)>,
}

impl ReviewSession {
    /// Builds a session from the effective limits.
    #[must_use]
    pub fn new(limits: &BudgetSet) -> Self {
        Self {
            budget: ReviewBudget::new(limits),
            cautions: Vec::new(),
        }
    }

    /// Starts a turn, dropping the previous turn's cautions and counters.
    pub fn begin_turn(&mut self) {
        self.budget.begin_turn();
        self.cautions.clear();
    }

    /// Returns the per-turn budget.
    #[must_use]
    pub const fn budget(&self) -> &ReviewBudget {
        &self.budget
    }

    /// Returns the cached caution for an action.
    #[must_use]
    pub fn cached_caution(&self, action: &str) -> Option<&str> {
        self.cautions
            .iter()
            .find(|(cached, _)| cached == action)
            .map(|(_, reason)| reason.as_str())
    }

    /// Returns the outcome for one action, asking the reviewer when needed.
    pub fn review(&mut self, reviewer: &dyn Reviewer, request: &ReviewRequest) -> ReviewOutcome {
        if let Some(reason) = self.cached_caution(&request.action) {
            return ReviewOutcome::Caution {
                reason: reason.to_owned(),
            };
        }
        if self.budget.attempt_spent(&request.action) {
            return ReviewOutcome::Unavailable {
                reason: "this action already spent its review attempt for this turn".to_owned(),
            };
        }
        if self.budget.exhausted() {
            return ReviewOutcome::Unavailable {
                reason: format!(
                    "the turn's review hold budget of {} is spent",
                    self.budget.limit()
                ),
            };
        }
        let outcome = reviewer.review(request);
        match outcome {
            ReviewOutcome::Clear { .. } => outcome,
            ReviewOutcome::Caution { ref reason } => {
                self.budget.hold();
                self.cautions.push((request.action.clone(), reason.clone()));
                outcome
            }
            ReviewOutcome::Unavailable { .. } => {
                self.budget.hold();
                self.budget.spend_attempt(&request.action);
                outcome
            }
            ReviewOutcome::Invalid => {
                self.budget.hold();
                outcome
            }
        }
    }
}

/// Returns the decision on one line, ignoring prose and decoration.
fn parse_line(line: &str) -> Option<ReviewDecision> {
    let text = strip_decoration(line);
    if text.eq_ignore_ascii_case(CLEAR) {
        return Some(ReviewDecision::Clear);
    }
    let rest = text.get(..CAUTION.len())?;
    if !rest.eq_ignore_ascii_case(CAUTION) {
        return None;
    }
    let after = strip_decoration(text.get(CAUTION.len()..)?.trim_start_matches(':'));
    if after.is_empty() {
        return None;
    }
    Some(ReviewDecision::Caution {
        reason: bound_reason(after),
    })
}

/// Removes the markdown decoration a model wraps an answer in.
fn strip_decoration(line: &str) -> &str {
    const LEADING: &[char] = &['>', '-', '*', '`', '#', '.', ':'];
    let mut text = line.trim();
    loop {
        let trimmed = text.trim_start_matches(LEADING).trim_start();
        if trimmed.len() == text.len() {
            break;
        }
        text = trimmed;
    }
    loop {
        let trimmed = text.trim_end_matches(['*', '`', '.']).trim_end();
        if trimmed.len() == text.len() {
            break;
        }
        text = trimmed;
    }
    text
}

/// Cuts a reason to the bound an entry of review context gets.
///
/// A reason is a sentence. The bound exists so a reviewer cannot push an
/// unbounded body into the transcript through the field that is quoted back to
/// the agent. The compiled default applies because this parse has no limit set
/// to consult.
fn bound_reason(reason: &str) -> String {
    let limit = usize::try_from(
        LimitName::ReviewContextBytes
            .default_value()
            .effective(rune_core::budget::EMERGENCY_CEILING_BYTES),
    )
    .unwrap_or(usize::MAX);
    truncate_bytes(reason, limit).to_owned()
}

/// Returns text cut to a byte bound, never inside a character.
fn truncate_bytes(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.get(..end).unwrap_or("")
}

/// Returns the error for a reply that carries no single decision.
fn invalid(problem: &str) -> RuneError {
    let mut hint = String::new();
    let _ = writeln!(
        hint,
        "the reply must carry exactly one line reading `{CLEAR}` or `{CAUTION}: <reason>`; one retry is allowed"
    );
    RuneError::new(
        ErrorCode::InvalidField,
        format!("no review decision: {problem}"),
    )
    .with_hint(hint.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// A reviewer that answers from a script and counts its requests.
    struct Scripted {
        answers: RefCell<Vec<ReviewOutcome>>,
        calls: RefCell<Vec<String>>,
    }

    impl Scripted {
        fn new(answers: Vec<ReviewOutcome>) -> Self {
            Self {
                answers: RefCell::new(answers),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.borrow().len()
        }

        fn reviewed(&self, index: usize) -> String {
            self.calls.borrow()[index].clone()
        }
    }

    impl Reviewer for Scripted {
        fn review(&self, request: &ReviewRequest) -> ReviewOutcome {
            self.calls.borrow_mut().push(request.action.clone());
            let mut answers = self.answers.borrow_mut();
            if answers.is_empty() {
                return ReviewOutcome::Unavailable {
                    reason: "the script is spent".to_owned(),
                };
            }
            answers.remove(0)
        }
    }

    fn clear_of(action: &str) -> ReviewOutcome {
        ReviewOutcome::Clear {
            reviewed_action: action.to_owned(),
        }
    }

    fn request(action: &str) -> ReviewRequest {
        ReviewRequest::new(action, vec![action.to_owned()], "shell", "call-1")
    }

    fn limits() -> BudgetSet {
        BudgetSet::new()
    }

    #[test]
    fn a_builtin_provider_has_a_fixed_reviewer() {
        assert_eq!(
            reviewer_for("anthropic", None),
            ReviewerKind::Fixed {
                model: "claude-haiku-4-5".to_owned()
            }
        );
        assert_eq!(
            reviewer_for("responses", None),
            ReviewerKind::Fixed {
                model: "gpt-5-mini".to_owned()
            }
        );
        assert!(reviewer_for("chat_completions", None).is_fixed());
    }

    #[test]
    fn a_builtin_provider_ignores_a_configured_reviewer() {
        let kind = reviewer_for("anthropic", Some("claude-opus-4-5"));
        assert_eq!(kind.model(), "claude-haiku-4-5");
        assert!(!accepts_review_override("anthropic"));
    }

    #[test]
    fn a_declared_endpoint_uses_its_configured_reviewer() {
        let kind = reviewer_for("local-llama", Some("llama-3.3-70b"));
        assert_eq!(
            kind,
            ReviewerKind::Custom {
                model: "llama-3.3-70b".to_owned()
            }
        );
        assert!(accepts_review_override("local-llama"));
        assert_eq!(
            reviewer_for("local-llama", Some("   ")),
            ReviewerKind::Fixed {
                model: DEFAULT_REVIEWER.to_owned()
            }
        );
    }

    #[test]
    fn an_unconfigured_provider_still_names_a_reviewer() {
        let kind = reviewer_for("unconfigured", None);
        assert_eq!(kind.model(), DEFAULT_REVIEWER);
    }

    #[test]
    fn the_request_keeps_the_action_and_targets_unmasked() {
        let action = "curl -H 'authorization: bearer sk-live-1' https://example.test";
        let request = ReviewRequest::new(
            action,
            vec!["https://example.test".to_owned()],
            "shell",
            "call-9",
        );
        assert_eq!(request.action, action);
        assert_eq!(request.targets, vec!["https://example.test".to_owned()]);
        assert_eq!(request.call_id, "call-9");
    }

    #[test]
    fn evidence_is_bounded_per_entry_and_in_total() {
        let mut limits = limits();
        limits
            .set(
                LimitName::ReviewContextBytes,
                rune_core::budget::Budget::Bounded(32),
                rune_core::config::Layer::User,
            )
            .expect("set");
        let mut request = request("ls");
        request.push_evidence("x".repeat(200), &limits);
        assert_eq!(request.prior_evidence[0].len(), 32);
        assert!(request.evidence_truncated());
        request.push_evidence("y".repeat(64), &limits);
        assert_eq!(request.evidence_bytes(), 32);
        assert_eq!(request.prior_evidence.len(), 1);
    }

    #[test]
    fn evidence_short_enough_is_kept_whole() {
        let mut request = request("ls");
        for _ in 0..3 {
            request.push_evidence("a short line", &limits());
        }
        assert_eq!(request.evidence_bytes(), 36);
        assert!(!request.evidence_truncated());
    }

    #[test]
    fn a_bare_decision_parses() {
        assert_eq!(
            ReviewDecision::parse("clear").expect("clear"),
            ReviewDecision::Clear
        );
        assert_eq!(
            ReviewDecision::parse("caution: writes outside the workspace").expect("caution"),
            ReviewDecision::Caution {
                reason: "writes outside the workspace".to_owned()
            }
        );
    }

    #[test]
    fn prose_around_one_decision_is_allowed() {
        let raw = "The command reads and writes a tracked file.\n\n`CAUTION: it rewrites \
                   tracked files in place`\n";
        assert_eq!(
            ReviewDecision::parse(raw).expect("caution"),
            ReviewDecision::Caution {
                reason: "it rewrites tracked files in place".to_owned()
            }
        );
        let clear = "- **clear**\n";
        assert_eq!(
            ReviewDecision::parse(clear).expect("clear"),
            ReviewDecision::Clear
        );
    }

    #[test]
    fn a_reply_without_one_decision_is_rejected() {
        for raw in [
            "",
            "the action looks fine to me",
            "clear\ncaution: it also writes outside",
            "caution:",
            "caution",
            "verdict clear",
        ] {
            let err = ReviewDecision::parse(raw).expect_err(raw);
            assert_eq!(err.code(), ErrorCode::InvalidField);
            assert!(err.detail().hint.is_some());
        }
    }

    #[test]
    fn a_reason_is_cut_to_the_context_bound() {
        let raw = format!("caution: {}", "z".repeat(64 * 1024));
        let ReviewDecision::Caution { reason } = ReviewDecision::parse(&raw).expect("caution")
        else {
            panic!("expected a caution");
        };
        assert_eq!(reason.len(), 8 * 1024);
    }

    #[test]
    fn a_clear_authorizes_only_the_exact_action() {
        let outcome = clear_of("git push origin main");
        assert!(outcome.authorizes("git push origin main"));
        assert!(!outcome.authorizes("git push origin main --force"));
        assert!(!outcome.authorizes("git push origin"));
        assert!(!outcome.authorizes("git push origin main "));
        assert!(!outcome.holds());
    }

    #[test]
    fn a_hold_is_not_an_authorization() {
        let caution = ReviewOutcome::Caution {
            reason: "deletes a directory".to_owned(),
        };
        assert!(!caution.authorizes("rm -rf build"));
        assert!(caution.holds());
        assert_eq!(caution.reason(), Some("deletes a directory"));
        assert!(ReviewOutcome::Invalid.holds());
        assert!(!ReviewOutcome::Invalid.authorizes("ls"));
    }

    #[test]
    fn a_clear_returned_for_a_different_action_does_not_cover_this_one() {
        let mut session = ReviewSession::new(&limits());
        let reviewer = Scripted::new(vec![clear_of("git status")]);
        let outcome = session.review(&reviewer, &request("git push"));
        assert!(outcome.holds());
        assert!(!outcome.authorizes("git push"));
    }

    #[test]
    fn a_caution_is_reused_for_the_same_action_within_the_turn() {
        let mut session = ReviewSession::new(&limits());
        let reviewer = Scripted::new(vec![ReviewOutcome::Caution {
            reason: "writes outside the workspace".to_owned(),
        }]);
        let first = session.review(&reviewer, &request("rm -rf /tmp/x"));
        let second = session.review(&reviewer, &request("rm -rf /tmp/x"));
        assert_eq!(first, second);
        assert_eq!(reviewer.calls(), 1);
        assert_eq!(
            session.cached_caution("rm -rf /tmp/x"),
            Some("writes outside the workspace")
        );
        assert_eq!(session.budget().holds(), 1);
    }

    #[test]
    fn a_cached_caution_does_not_cover_a_changed_action() {
        let mut session = ReviewSession::new(&limits());
        let reviewer = Scripted::new(vec![
            ReviewOutcome::Caution {
                reason: "writes outside the workspace".to_owned(),
            },
            clear_of("rm -rf /tmp/x --interactive"),
        ]);
        session.review(&reviewer, &request("rm -rf /tmp/x"));
        let second = session.review(&reviewer, &request("rm -rf /tmp/x --interactive"));
        assert!(second.authorizes("rm -rf /tmp/x --interactive"));
        assert_eq!(reviewer.calls(), 2);
    }

    #[test]
    fn an_unavailable_review_is_not_cached_and_is_asked_once_per_action() {
        let mut session = ReviewSession::new(&limits());
        let reviewer = Scripted::new(vec![ReviewOutcome::Unavailable {
            reason: "the reviewer timed out".to_owned(),
        }]);
        let first = session.review(&reviewer, &request("git push --force"));
        assert_eq!(
            first.reason(),
            Some("the reviewer timed out"),
            "the first attempt reports the reviewer's reason"
        );
        let second = session.review(&reviewer, &request("git push --force"));
        assert!(second.holds(), "the second attempt still holds the action");
        assert_eq!(
            reviewer.calls(),
            1,
            "the same action spends one attempt per turn"
        );
        assert_eq!(session.cached_caution("git push --force"), None);
        assert_eq!(session.budget().attempts(), 1);

        session.begin_turn();
        let third = session.review(&reviewer, &request("git push --force"));
        assert!(third.holds());
        assert_eq!(reviewer.calls(), 2, "a new turn may ask again");
    }

    #[test]
    fn a_new_turn_drops_the_previous_caution() {
        let mut session = ReviewSession::new(&limits());
        let reviewer = Scripted::new(vec![
            ReviewOutcome::Caution {
                reason: "writes outside the workspace".to_owned(),
            },
            clear_of("rm -rf /tmp/x"),
        ]);
        session.review(&reviewer, &request("rm -rf /tmp/x"));
        session.begin_turn();
        assert_eq!(session.cached_caution("rm -rf /tmp/x"), None);
        let outcome = session.review(&reviewer, &request("rm -rf /tmp/x"));
        assert!(outcome.authorizes("rm -rf /tmp/x"));
        assert_eq!(session.budget().holds(), 0);
    }

    #[test]
    fn the_hold_budget_caps_the_turn() {
        let mut limits = limits();
        limits
            .set(
                LimitName::ReviewHoldsPerTurn,
                rune_core::budget::Budget::Bounded(3),
                rune_core::config::Layer::User,
            )
            .expect("set");
        let mut session = ReviewSession::new(&limits);
        let reviewer = Scripted::new(vec![
            ReviewOutcome::Caution {
                reason: "writes outside the workspace".to_owned(),
            },
            ReviewOutcome::Caution {
                reason: "writes outside the workspace".to_owned(),
            },
            ReviewOutcome::Caution {
                reason: "writes outside the workspace".to_owned(),
            },
            clear_of("ls"),
        ]);

        for index in 0..3 {
            let action = format!("rm -rf /tmp/{index}");
            assert!(session.review(&reviewer, &request(&action)).holds());
        }
        assert!(session.budget().exhausted());
        assert_eq!(session.budget().remaining(), 0);

        let past = session.review(&reviewer, &request("rm -rf /tmp/past"));
        assert!(past.holds());
        assert_eq!(
            past.reason(),
            Some("the turn's review hold budget of 3 is spent")
        );
        assert_eq!(reviewer.calls(), 3, "no request is made past the budget");
    }

    #[test]
    fn the_unavailable_attempt_budget_caps_the_turn() {
        let mut limits = limits();
        limits
            .set(
                LimitName::ReviewHoldsPerTurn,
                rune_core::budget::Budget::Bounded(3),
                rune_core::config::Layer::User,
            )
            .expect("set");
        let mut session = ReviewSession::new(&limits);
        let reviewer = Scripted::new(vec![ReviewOutcome::Unavailable {
            reason: "the credential is missing".to_owned(),
        }]);

        for index in 0..3 {
            let action = format!("rm -rf /tmp/{index}");
            assert!(session.review(&reviewer, &request(&action)).holds());
        }
        assert_eq!(session.budget().attempts(), 3);

        for index in 0..3 {
            let action = format!("rm -rf /tmp/{index}");
            assert!(session.review(&reviewer, &request(&action)).holds());
        }
        assert_eq!(
            reviewer.calls(),
            3,
            "repeating an action spends no further attempt"
        );
        assert_eq!(session.budget().attempts(), 3);

        let past = session.review(&reviewer, &request("rm -rf /tmp/past"));
        assert!(past.holds());
        assert_eq!(session.budget().attempts(), 3);
        assert_eq!(reviewer.calls(), 3, "an exhausted budget asks nothing");
    }

    #[test]
    fn an_invalid_reply_holds_and_is_not_reused() {
        let mut session = ReviewSession::new(&limits());
        let reviewer = Scripted::new(vec![ReviewOutcome::Invalid, clear_of("rm -rf /tmp/x")]);
        let first = session.review(&reviewer, &request("rm -rf /tmp/x"));
        assert_eq!(first, ReviewOutcome::Invalid);
        assert!(first.holds());
        let retry = session.review(&reviewer, &request("rm -rf /tmp/x"));
        assert!(retry.authorizes("rm -rf /tmp/x"));
        assert_eq!(reviewer.calls(), 2, "an invalid reply may be retried once");
        assert_eq!(session.budget().holds(), 1);
    }

    #[test]
    fn a_cleared_action_is_not_held_or_counted() {
        let mut session = ReviewSession::new(&limits());
        let reviewer = Scripted::new(vec![clear_of("ls -la")]);
        let outcome = session.review(&reviewer, &request("ls -la"));
        assert!(outcome.authorizes("ls -la"));
        assert_eq!(session.budget().holds(), 0);
        assert_eq!(reviewer.reviewed(0), "ls -la");
    }
}
