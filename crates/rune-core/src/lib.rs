//! Core types, identifiers, error taxonomy, configuration, and state paths.
//!
//! This crate depends on nothing else in the workspace. It must not link a
//! network stack, an async runtime, or a terminal library, so that command
//! dispatch and configuration resolution stay free of that cost.

#![forbid(unsafe_code)]
// Tests assert by panicking. The guards that forbid panicking apply to the
// shipped build, where a panic on user input is a defect.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod budget;
pub mod config;
pub mod error;
pub mod id;
pub mod paths;
pub mod tool;

pub use budget::{Budget, BudgetSet, LimitName};
pub use error::{ErrorCode, Result, RuneError};
pub use id::{EventSeq, SessionId, ToolCallId, TurnId};
