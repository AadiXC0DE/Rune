//! Permission policy: rule evaluation, admission, and command classification.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod approval;
pub mod command;
pub mod decision;
pub mod review;
pub mod rules;
pub mod settings;
pub mod trust;

pub use approval::{ApprovalOutcome, Grants, Prompt, Resolution, Scope, SessionGrant};
pub use command::{Classification, CommandKind, classify, direct_argv, requires_shell, shell_argv};
pub use decision::{Decision, Layer, Outcome};
pub use review::{
    ReviewBudget, ReviewDecision, ReviewOutcome, ReviewRequest, ReviewSession, Reviewer,
    ReviewerKind, reviewer_for,
};
pub use rules::{Rule, RuleSet};
pub use trust::{ProjectRequest, TrustDecision, TrustStore, WorkspaceTrust, decide};
