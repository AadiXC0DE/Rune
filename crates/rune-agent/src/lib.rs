//! The agent turn loop, history, compaction, steering, and subagents.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod compaction;
pub mod history;
pub mod steering;
pub mod subagent;
pub mod subagent_tool;
pub mod tokens;
pub mod turn;

pub use compaction::{CompactionStats, Plan, Trigger};
pub use history::{History, Turn};
pub use steering::{Boundary, Cancellation, Steering, SteeringQueue};
pub use subagent::{
    Admission, AdmissionVerdict, Authority, AuthoritySource, Child, ChildBrief, ChildId, ChildKind,
    ChildOutcome, ChildPermission, ChildStep, ChildWork, Feedback, FeedbackMessage,
    MAX_INSTRUCTIONS_BYTES, MAX_MODEL_BYTES, MAX_PROMPT_BYTES, Registry, SubagentAction,
    SubagentRequest, run_child,
};
pub use turn::{Event, Host, StopReason, TurnOutcome, run_turn};
