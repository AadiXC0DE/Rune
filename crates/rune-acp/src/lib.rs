//! Agent Client Protocol server over stdio.
//!
//! The server speaks JSON-RPC 2.0 over newline-delimited frames and drives the
//! agent turn loop. Two properties matter most:
//!
//! - A prompt that arrives while a turn is running is admitted rather than
//!   refused, so a message typed while the agent was working is never lost.
//! - Standard output carries protocol frames and nothing else, so a client's
//!   parser never has to distinguish a frame from a diagnostic.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod jsonrpc;
pub mod server;
pub mod session;

pub use server::{Dialect, Server, ServerConfig};
