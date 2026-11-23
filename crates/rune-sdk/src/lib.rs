//! Embeddable agent library for Rust hosts.
//!
//! An embedder supplies the credential, the tools, and the network path, and
//! drives one conversation through [`Agent`]. Nothing here reads a filesystem or
//! constructs a built-in tool: a host that wants one passes it in, and a host
//! that wants to control outbound traffic supplies a [`HostFetch`], which is
//! then the only client used. The one exception is opt-in: [`Snapshots`]
//! records the workspace's tree at the start and end of each prompt.
//!
//! [`Hooks`] run at the points of a turn the terminal's hooks run at, and a
//! turn that fails after doing some work still returns what it did, with the
//! failure in [`TurnResult::error`].
//!
//! A host that wants tools to live in another process, in any language, uses
//! [`PluginHost`] and speaks the plugin protocol instead.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod agent;
pub mod plugin;

pub use agent::{
    Agent, AgentDiagnostics, AgentOptions, AgentSources, CHECKPOINT_VERSION, CancelFlag,
    DiagnosticSource, Dialect, EVENT_QUEUE_CAPACITY, Events, FetchContext, FetchRequest,
    FetchResponse, HookVeto, Hooks, HostFetch, HostImage, HostTool, HostToolContext,
    HostToolResult, PostToolEffect, PreToolOutcome, PreparedCall, PromptOptions, SdkLimit,
    Snapshots, Turn, TurnResult,
};
pub use plugin::{
    MANIFEST_FILE, MAX_FRAME_BYTES, PLUGIN_PROTOCOL_VERSION, PluginHost, PluginManifest,
    PluginTool, SharedPlugin,
};
