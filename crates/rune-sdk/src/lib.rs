//! Embeddable agent library for Rust hosts.
//!
//! An embedder supplies the credential, the tools, and the network path, and
//! drives one conversation through [`Agent`]. Nothing here reads a filesystem or
//! constructs a built-in tool: a host that wants one passes it in, and a host
//! that wants to control outbound traffic supplies a [`HostFetch`], which is
//! then the only client used.
//!
//! A host that wants tools to live in another process, in any language, uses
//! [`PluginHost`] and speaks the plugin protocol instead.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod agent;
pub mod plugin;

pub use agent::{
    Agent, AgentOptions, CHECKPOINT_VERSION, CancelFlag, Dialect, Events, FetchRequest,
    FetchResponse, HostFetch, HostImage, HostTool, HostToolContext, HostToolResult, PromptOptions,
    Turn, TurnResult,
};
pub use plugin::{
    MANIFEST_FILE, MAX_FRAME_BYTES, PLUGIN_PROTOCOL_VERSION, PluginHost, PluginManifest,
    PluginTool, SharedPlugin,
};
