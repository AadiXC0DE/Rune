//! The transport seam.
//!
//! One trait says what this crate needs from an HTTP client: send a request and
//! return a response whose body is read incrementally. The blocking client that
//! implements it lives in `transport`, which is the only module that names that
//! client, so a build for a target where it cannot compile supplies its own
//! implementation instead of a second request path.
//!
//! The body is a reader and never a buffer. A model answers one token at a time
//! and the caller renders each one as it arrives, so collecting the body first
//! would hold the answer back until the response finished.

use std::fmt;
use std::io::Read;
use std::time::Duration;

use crate::error::NetResult;

/// HTTP method for one request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    /// A request with no body.
    Get,
    /// A request carrying a body.
    Post,
}

/// One outbound request.
#[derive(Clone, Debug)]
pub struct FetchRequest {
    /// Method.
    pub method: Method,
    /// Absolute URL.
    pub url: String,
    /// Headers, in the order they are sent.
    pub headers: Vec<(String, String)>,
    /// Request body. Empty for a `Get`.
    pub body: Vec<u8>,
    /// Budget for the whole request, head and body.
    ///
    /// `None` leaves the body unbounded in time, which a streaming completion
    /// needs: a generation legitimately runs for minutes, and the head timeout
    /// bounds the part that can hang.
    pub timeout: Option<Duration>,
}

impl FetchRequest {
    /// Builds a `Get` request.
    #[must_use]
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: None,
        }
    }

    /// Builds a `Post` request.
    #[must_use]
    pub fn post(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: Method::Post,
            url: url.into(),
            headers: Vec::new(),
            body,
            timeout: None,
        }
    }

    /// Adds a header, keeping the order headers were added in.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Sets the budget for the whole request.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }
}

/// What a transport returns.
pub struct FetchResponse {
    /// HTTP status.
    pub status: u16,
    /// Media type as reported, parameters included, or
    /// `application/octet-stream` when the server named none.
    pub content_type: String,
    /// Body, read by the caller as it arrives.
    pub body: Box<dyn Read + Send>,
}

impl fmt::Debug for FetchResponse {
    /// Reports the head only. Reading the body here would consume it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FetchResponse")
            .field("status", &self.status)
            .field("content_type", &self.content_type)
            .finish_non_exhaustive()
    }
}

/// An HTTP client.
///
/// Object safe, so the streaming functions take `&dyn Fetch` and a host can
/// substitute its own client without changing them.
pub trait Fetch: Send + Sync {
    /// Performs one request.
    ///
    /// Returns as soon as the response head is known; the body is read from the
    /// returned reader, which is what keeps a streamed answer incremental.
    fn send(&self, request: FetchRequest) -> NetResult<FetchResponse>;
}
