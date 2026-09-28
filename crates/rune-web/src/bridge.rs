//! What the page supplies to the harness.
//!
//! A browser tab has no sockets, no processes, and no terminal, so the page
//! provides those three things and nothing else: a request, a command run in
//! the page's own small shell, and a question put to the person watching. The
//! loop, the tools, the permission rules, and the provider dialects are the
//! same code the binary runs.
//!
//! Every method may take as long as the page needs. In the page each one is a
//! JavaScript promise the WebAssembly stack is suspended on, so the harness
//! reads as blocking code while the tab stays responsive.

use std::io::{self, Read};
use std::sync::Arc;

use rune_net::error::{FailureKind, NetError, NetResult};
use rune_net::fetch::{Fetch, FetchRequest, FetchResponse, Method};
use serde::Deserialize;

/// The head of a response, as the page reports it.
#[derive(Clone, Debug, Deserialize)]
pub struct Head {
    /// Handle the body is read through.
    pub handle: i32,
    /// HTTP status.
    pub status: u16,
    /// Media type, parameters included.
    #[serde(default)]
    pub content_type: String,
}

/// What a command produced in the page's shell.
#[derive(Clone, Debug, Deserialize)]
pub struct Ran {
    /// Standard output and standard error, interleaved as the page wrote them.
    pub output: String,
    /// Exit status.
    pub exit_code: i32,
}

/// The services a page provides to the harness.
pub trait Bridge: Send + Sync {
    /// Delivers one event, encoded as a JSON object.
    fn emit(&self, event: &str);

    /// Sends a request and returns once the response head is known.
    fn open(&self, request: &serde_json::Value) -> Result<Head, String>;

    /// Reads the next bytes of a body. Zero means the body has ended.
    fn read(&self, handle: i32, buffer: &mut [u8]) -> io::Result<usize>;

    /// Releases a body that will not be read further.
    fn close(&self, handle: i32);

    /// Asks the person watching whether an action may run.
    fn ask(&self, question: &serde_json::Value) -> bool;

    /// Runs a command in the page's shell.
    fn run(&self, command: &str, cwd: &str) -> Ran;
}

/// A [`Fetch`] that sends every request through the page.
pub struct BridgeFetch {
    bridge: Arc<dyn Bridge>,
}

impl std::fmt::Debug for BridgeFetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeFetch").finish_non_exhaustive()
    }
}

impl BridgeFetch {
    /// Builds a transport over a bridge.
    pub fn new(bridge: Arc<dyn Bridge>) -> Self {
        Self { bridge }
    }
}

impl Fetch for BridgeFetch {
    fn send(&self, request: FetchRequest) -> NetResult<FetchResponse> {
        let method = match request.method {
            Method::Get => "GET",
            Method::Post => "POST",
        };
        let encoded = serde_json::json!({
            "method": method,
            "url": request.url,
            "headers": request.headers,
            "body": String::from_utf8_lossy(&request.body),
        });
        let head = self
            .bridge
            .open(&encoded)
            .map_err(|reason| NetError::new(FailureKind::Network, reason))?;
        Ok(FetchResponse {
            status: head.status,
            content_type: if head.content_type.is_empty() {
                String::from("application/octet-stream")
            } else {
                head.content_type
            },
            body: Box::new(BridgeBody {
                bridge: Arc::clone(&self.bridge),
                handle: head.handle,
                finished: false,
            }),
        })
    }
}

/// A response body read through the page, one chunk at a time.
///
/// Each read is one chunk as the page received it, so a streamed answer reaches
/// the reducer, and the screen, at the pace the endpoint produced it.
struct BridgeBody {
    bridge: Arc<dyn Bridge>,
    handle: i32,
    finished: bool,
}

impl Read for BridgeBody {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.finished || buffer.is_empty() {
            return Ok(0);
        }
        let read = self.bridge.read(self.handle, buffer)?;
        if read == 0 {
            self.finished = true;
        }
        Ok(read)
    }
}

impl Drop for BridgeBody {
    fn drop(&mut self) {
        self.bridge.close(self.handle);
    }
}
