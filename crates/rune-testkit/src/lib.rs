//! Test fixtures shared by every tier of the suite.
//!
//! One mock endpoint and one repository fixture are used everywhere, so a test
//! never has to build its own harness and two tiers cannot drift apart.

#![forbid(unsafe_code)]
// This crate exists only for tests, so it asserts by panicking throughout. The
// guards that forbid panicking exist for shipped code, where a panic on user
// input is a defect.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

pub mod mock;

pub use mock::{MockEndpoint, Script};
