//! Model provider dialects, streaming, transport, credentials, and the catalog.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod anthropic;
pub mod chat_completions;
pub mod error;
pub mod message;
pub mod provider;
pub mod redact;
pub mod responses;
pub mod sse;
pub mod stream;

pub use error::{FailureKind, NetError, NetResult};
pub use message::{ContentPart, ImageRef, Message, Role, ToolSpec};
pub use provider::{Provider, RequestPlan, ToolChoice};
pub use stream::{FinishReason, Limit, ProviderEvent, StreamReducer, Usage};
