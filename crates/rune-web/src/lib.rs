//! The agent harness built for a browser tab.
//!
//! The page loads this crate as a WASI module. The turn loop, the tools, the
//! permission rules, and the provider dialects are the ones the binary links;
//! the workspace is an in-memory filesystem the page mounts through WASI, and
//! the network, the shell, and the permission prompt are supplied by the page
//! through [`bridge::Bridge`].
//!
//! Only the entry points are specific to WebAssembly. Everything else builds
//! and is tested on the host, against a bridge written for the tests.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod bridge;
pub mod host;
pub mod shell;

pub use bridge::{Bridge, BridgeFetch, Head, Ran};
pub use host::{Config, Control, Session};

#[cfg(target_family = "wasm")]
mod exports;
