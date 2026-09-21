//! Permission policy: rule evaluation, admission, and command classification.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod decision;
pub mod rules;

pub use decision::{Decision, Layer, Outcome};
pub use rules::{Rule, RuleSet};
