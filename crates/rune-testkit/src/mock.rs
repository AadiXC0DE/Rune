//! A mock model endpoint.
//!
//! Serves recorded responses over real HTTP so the transport, the framing, and
//! the dialect reducer are all exercised together. A test that stubbed the
//! transport instead would not catch a framing bug, and a framing bug is exactly
//! the kind that reached a release once already.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// One scripted response.
#[derive(Clone, Debug)]
pub enum Script {
    /// Stream these server-sent event payloads, then terminate.
    Frames(Vec<String>),
    /// Respond with an HTTP status and a body, without streaming.
    Status {
        /// The status code.
        code: u16,
        /// The body.
        body: String,
    },
}

impl Script {
    /// Builds a script that streams a text answer and stops.
    #[must_use]
    pub fn text(answer: &str) -> Self {
        let mut frames =
            vec![r#"{"choices":[{"index":0,"delta":{"role":"assistant"}}]}"#.to_owned()];
        frames.push(format!(
            r#"{{"choices":[{{"index":0,"delta":{{"content":{}}}}}]}}"#,
            serde_json::to_string(answer).unwrap_or_else(|_| "\"\"".to_owned())
        ));
        frames.push(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.to_owned());
        frames.push(
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.to_owned(),
        );
        frames.push("[DONE]".to_owned());
        Self::Frames(frames)
    }

    /// Builds a script that asks for a tool call.
    #[must_use]
    pub fn tool_call(id: &str, name: &str, arguments: &str) -> Self {
        let arguments_json =
            serde_json::to_string(arguments).unwrap_or_else(|_| "\"{}\"".to_owned());
        Self::Frames(vec![
            format!(
                r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"{id}","function":{{"name":"{name}","arguments":""}}}}]}}}}]}}"#
            ),
            format!(
                r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"function":{{"arguments":{arguments_json}}}}}]}}}}]}}"#
            ),
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.to_owned(),
            "[DONE]".to_owned(),
        ])
    }

    /// Builds a script that ends the stream without a terminal frame.
    #[must_use]
    pub fn truncated(text: &str) -> Self {
        Self::Frames(vec![format!(
            r#"{{"choices":[{{"index":0,"delta":{{"content":{}}}}}]}}"#,
            serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
        )])
    }

    /// Builds a script that reports a failed stream.
    #[must_use]
    pub fn provider_error(message: &str) -> Self {
        Self::Frames(vec![format!(
            r#"{{"error":{{"message":{}}}}}"#,
            serde_json::to_string(message).unwrap_or_else(|_| "\"error\"".to_owned())
        )])
    }
}

/// A mock endpoint serving a scripted sequence of responses.
#[derive(Debug)]
pub struct MockEndpoint {
    port: u16,
    handler: Arc<Mutex<Handler>>,
    shutdown: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct Handler {
    scripts: Vec<Script>,
    cursor: usize,
    /// Requests received, for assertions about retries.
    requests: usize,
    /// The last request body, for assertions about what was sent.
    last_body: Option<serde_json::Value>,
    /// Number of requests to fail with a retryable status before succeeding.
    fail_first: usize,
}

impl MockEndpoint {
    /// Starts an endpoint that serves the scripts in order.
    ///
    /// The last script repeats once the sequence is exhausted, so a test that
    /// expects a retry does not have to enumerate every attempt.
    pub fn start(scripts: Vec<Script>) -> Self {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(err) => panic!("could not bind a mock endpoint: {err}"),
        };
        let port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(err) => panic!("could not read the mock endpoint address: {err}"),
        };
        let handler = Arc::new(Mutex::new(Handler {
            scripts,
            ..Handler::default()
        }));
        let shutdown = Arc::new(AtomicUsize::new(0));

        let thread_handler = Arc::clone(&handler);
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_shutdown.load(Ordering::SeqCst) == 1 {
                    break;
                }
                let Ok(stream) = stream else { break };
                let handler = Arc::clone(&thread_handler);
                std::thread::spawn(move || {
                    let _ = serve(stream, &handler);
                });
            }
        });

        Self {
            port,
            handler,
            shutdown,
            thread: Some(thread),
        }
    }

    /// Returns the base URL to configure a provider with.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// Returns how many requests the endpoint received.
    #[must_use]
    pub fn request_count(&self) -> usize {
        self.handler.lock().map_or(0, |h| h.requests)
    }

    /// Returns the most recent request body.
    #[must_use]
    pub fn last_body(&self) -> Option<serde_json::Value> {
        self.handler.lock().ok().and_then(|h| h.last_body.clone())
    }

    /// Configures the endpoint to fail the first `count` requests with a
    /// retryable status.
    pub fn fail_first(&self, count: usize) {
        if let Ok(mut handler) = self.handler.lock() {
            handler.fail_first = count;
        }
    }
}

impl Drop for MockEndpoint {
    fn drop(&mut self) {
        self.shutdown.store(1, Ordering::SeqCst);
        // Unblock the accept loop by connecting once.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Serves one connection.
fn serve(stream: TcpStream, handler: &Arc<Mutex<Handler>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut stream = stream;

    let mut content_length = 0_usize;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        std::io::Read::read_exact(&mut reader, &mut body)?;
    }

    let script = {
        let Ok(mut guard) = handler.lock() else {
            return Ok(());
        };
        guard.requests = guard.requests.saturating_add(1);
        guard.last_body = serde_json::from_slice(&body).ok();

        if guard.fail_first > 0 {
            guard.fail_first = guard.fail_first.saturating_sub(1);
            Script::Status {
                code: 503,
                body: r#"{"error":{"message":"temporarily unavailable"}}"#.to_owned(),
            }
        } else if guard.scripts.is_empty() {
            Script::text("no script")
        } else {
            let index = guard.cursor.min(guard.scripts.len().saturating_sub(1));
            guard.cursor = guard.cursor.saturating_add(1);
            guard.scripts[index].clone()
        }
    };

    match script {
        Script::Frames(frames) => {
            stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            )?;
            for frame in frames {
                stream.write_all(format!("data: {frame}\n\n").as_bytes())?;
                stream.flush()?;
            }
        }
        Script::Status { code, body } => {
            stream.write_all(
                format!(
                    "HTTP/1.1 {code} Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )?;
        }
    }

    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_script_streams_the_answer() {
        let endpoint = MockEndpoint::start(vec![Script::text("hello")]);
        let client = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build();
        let client: ureq::Agent = client.into();
        let response = client
            .post(&format!("{}/chat/completions", endpoint.base_url()))
            .send_empty()
            .expect("request");
        assert_eq!(response.status().as_u16(), 200);
        let body = response.into_body().read_to_string().expect("body");
        assert!(body.contains("hello"), "{body}");
        assert!(body.contains("[DONE]"), "{body}");
    }

    #[test]
    fn a_status_script_returns_that_status() {
        let endpoint = MockEndpoint::start(vec![Script::Status {
            code: 503,
            body: "{}".to_owned(),
        }]);
        let client: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let response = client
            .post(&format!("{}/chat/completions", endpoint.base_url()))
            .send_empty()
            .expect("request");
        assert_eq!(response.status().as_u16(), 503);
    }

    #[test]
    fn the_request_body_is_recorded_for_assertions() {
        let endpoint = MockEndpoint::start(vec![Script::text("x")]);
        let client: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        client
            .post(&format!("{}/chat/completions", endpoint.base_url()))
            .header("content-type", "application/json")
            .send(r#"{"model":"probe"}"#)
            .expect("request");
        let body = endpoint.last_body().expect("recorded");
        assert_eq!(body["model"], "probe");
        assert_eq!(endpoint.request_count(), 1);
    }

    #[test]
    fn the_last_script_repeats_when_the_sequence_is_exhausted() {
        let endpoint = MockEndpoint::start(vec![Script::text("only")]);
        let client: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        for _ in 0..3 {
            let response = client
                .post(&format!("{}/chat/completions", endpoint.base_url()))
                .send_empty()
                .expect("request");
            assert_eq!(response.status().as_u16(), 200);
        }
        assert_eq!(endpoint.request_count(), 3);
    }

    #[test]
    fn a_tool_call_script_streams_a_complete_call() {
        let script = Script::tool_call("c1", "read_file", "{\"path\":\"a.rs\"}");
        let Script::Frames(frames) = script else {
            panic!("expected frames");
        };
        let joined = frames.join("\n");
        assert!(joined.contains("read_file"), "{joined}");
        assert!(joined.contains("tool_calls"), "{joined}");
        assert!(joined.contains("tool_calls\"}]"), "{joined}");
    }

    #[test]
    fn a_truncated_script_has_no_terminal_frame() {
        let Script::Frames(frames) = Script::truncated("half") else {
            panic!("expected frames");
        };
        assert!(!frames.iter().any(|frame| frame.contains("[DONE]")));
        assert!(!frames.iter().any(|frame| frame.contains("finish_reason")));
    }
}
