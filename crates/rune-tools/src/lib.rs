//! Built-in tools and the tool registry.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod contract;
pub mod registry;

pub use contract::{Activity, Cancellation, ExecutionContext, Tool, ToolOutput};
pub use registry::Registry;
