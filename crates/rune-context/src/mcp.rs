//! The Model Context Protocol client.
//!
//! A server contributes tools the model may call. The client is deliberately
//! small and hand-written: the JSON-RPC layer is a few hundred lines that can be
//! read in one sitting, and the framing is bounded at every point a server
//! controls the size of a message.
//!
//! Three properties matter more than feature coverage. A server's input schema
//! reaches the model unchanged. A server that hangs is interrupted rather than
//! waited on. And a credential whose lifetime is unstated is used until the
//! server itself says otherwise, so a healthy server is never latched into a
//! failed state by a missing field.
//!
//! ```no_run
//! use rune_core::budget::BudgetSet;
//! use rune_context::mcp::{Client, ServerConfig};
//!
//! let servers = ServerConfig::parse_list(&serde_json::json!([]))?;
//! let client = Client::new(&BudgetSet::new());
//! client.connect_all(&servers)?;
//! for tool in client.tools() {
//!     println!("{}", tool.name);
//! }
//! client.shutdown();
//! # Ok::<(), rune_core::error::RuneError>(())
//! ```

pub mod client;
pub mod config;
pub mod protocol;
pub mod schema;

pub use client::{
    CallOutcome, Client, ConnectReport, Credential, Expiry, Failure, ServerStatus, now_ms,
};
pub use config::{MAX_SERVER_NAME_BYTES, ServerConfig, Transport};
pub use protocol::{DEFAULT_VERSION, MAX_FRAME_BYTES, SUPPORTED_VERSIONS, is_supported};
pub use schema::{
    MAX_TOOL_NAME_BYTES, PREFIX, ProjectedTool, Projection, project_page, project_tool,
};
