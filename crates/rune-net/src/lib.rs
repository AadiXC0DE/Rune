//! Model provider dialects, streaming, transport, credentials, and the catalog.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod anthropic;
pub mod auth;
pub mod catalog;
pub mod chat_completions;
pub mod error;
pub mod fetch;
pub mod message;
pub mod models_dev;
pub mod provider;
pub mod providers;
pub mod redact;
pub mod responses;
pub mod sse;
pub mod stream;
pub mod transport;

pub use auth::{Credential, CredentialSource};
pub use catalog::{Capabilities, Catalog, ModelMetadata};
pub use error::{FailureKind, NetError, NetResult};
pub use fetch::{Fetch, FetchRequest, FetchResponse, Method};
pub use message::{ContentPart, ImageRef, Message, Role, ToolSpec};
pub use provider::{Provider, RequestPlan, Routing, ToolChoice, validate_order, validate_slug};
pub use providers::{CredentialKind, KNOWN_PROVIDERS, KnownProvider};
pub use stream::{FinishReason, Limit, ProviderEvent, StreamReducer, Usage};
pub use transport::{Endpoint, StreamOutcome};
// The built-in client, present only where it can build.
#[cfg(not(target_family = "wasm"))]
pub use transport::{UreqFetch, agent};
