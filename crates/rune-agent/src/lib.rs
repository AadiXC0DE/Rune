//! The agent turn loop, history, compaction, and steering.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod compaction;
pub mod history;
pub mod steering;
pub mod tokens;
pub mod turn;

pub use compaction::{CompactionStats, Plan, Trigger};
pub use history::{History, Turn};
pub use steering::{Boundary, Cancellation, Steering, SteeringQueue};
pub use turn::{Event, Host, StopReason, TurnOutcome, run_turn};
