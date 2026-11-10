//! A scripted chat completions provider.
//!
//! Serves one scripted reply per `POST` over real HTTP, in request order, so a
//! whole run can be replayed without a key or a network. It also records what
//! each request looked like, so a test can assert on request size, token use
//! and prefix stability.
//!
//! Token accounting is deterministic:
//!
//! - `prompt_tokens` is the request body length in bytes divided by four,
//!   rounded up.
//! - `completion_tokens` is the reply length divided by four, rounded up. The
//!   reply length is the text plus, for each call, the name and the argument
//!   string. An `http_error` turn counts zero.
//! - The prefix key of a request is the compact JSON of `tools` (or `[]`)
//!   followed by the compact JSON of `messages`. A body that is not JSON has an
//!   empty key.
//! - `cached_tokens` is the length in bytes of the longest common prefix of
//!   this key and the previous request's key, divided by four and rounded up,
//!   capped at `prompt_tokens`. The first request has none.
//! - A prefix break is a request whose predecessor had at least 1,000 prompt
//!   tokens and whose cached share is under one half.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;

/// One scripted reply, given to one `POST`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    /// Text streamed as content.
    #[serde(default)]
    pub text: Option<String>,
    /// Tool calls streamed after the text.
    #[serde(default)]
    pub tool_calls: Vec<ScriptedCall>,
    /// Finish reason; `tool_calls` when there are calls, else `stop`.
    #[serde(default)]
    pub finish: Option<String>,
    /// Sleep before the status line is written.
    #[serde(default)]
    pub delay_ms: u64,
    /// Omit the final `data: [DONE]` line.
    #[serde(default)]
    pub no_done: bool,
    /// Stop the stream after a number of bytes.
    #[serde(default)]
    pub cut_stream: Option<CutStream>,
    /// When set, every other field must be absent or default.
    #[serde(default)]
    pub http_error: Option<HttpError>,
}

/// Where to cut a stream short.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CutStream {
    /// Bytes of the body to write before closing.
    pub after_bytes: usize,
}

/// A plain HTTP error reply.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpError {
    /// The status code.
    pub status: u16,
    /// The body.
    #[serde(default)]
    pub body: String,
    /// Extra response headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// One scripted tool call.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptedCall {
    /// The call id.
    pub id: String,
    /// The tool name.
    pub name: String,
    /// A JSON string is sent verbatim, so malformed arguments can be scripted;
    /// any other value is sent as its compact serialization.
    pub arguments: serde_json::Value,
}

impl ScriptedCall {
    fn argument_string(&self) -> String {
        match &self.arguments {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }
}

impl Turn {
    /// A turn that streams this text and stops.
    #[must_use]
    pub fn text(text: &str) -> Self {
        Self {
            text: Some(text.to_owned()),
            ..Self::default()
        }
    }

    /// A turn that asks for one tool call. A parse failure keeps the arguments
    /// as a string.
    #[must_use]
    pub fn call(id: &str, name: &str, arguments_json: &str) -> Self {
        let arguments = serde_json::from_str(arguments_json)
            .unwrap_or_else(|_| serde_json::Value::String(arguments_json.to_owned()));
        Self {
            tool_calls: vec![ScriptedCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments,
            }],
            ..Self::default()
        }
    }

    /// A turn that answers with a plain HTTP error.
    #[must_use]
    pub fn http_error(status: u16, body: &str) -> Self {
        Self {
            http_error: Some(HttpError {
                status,
                body: body.to_owned(),
                headers: BTreeMap::new(),
            }),
            ..Self::default()
        }
    }

    fn reply_bytes(&self) -> usize {
        if self.http_error.is_some() {
            return 0;
        }
        let text = self.text.as_ref().map_or(0, String::len);
        self.tool_calls.iter().fold(text, |sum, call| {
            sum.saturating_add(call.name.len())
                .saturating_add(call.argument_string().len())
        })
    }
}

/// What one request looked like.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    /// The request target.
    pub path: String,
    /// The raw request body; empty when the provider keeps no bodies.
    pub body: String,
    /// The body length in bytes.
    pub bytes: usize,
    /// Prompt tokens reported for this request.
    pub prompt_tokens: u64,
    /// Cached prompt tokens reported for this request.
    pub cached_tokens: u64,
    /// Completion tokens reported for this request.
    pub completion_tokens: u64,
}

#[derive(Debug, Default)]
struct State {
    turns: Vec<Turn>,
    requests: Vec<RecordedRequest>,
    previous_key: Option<String>,
    keep_bodies: bool,
}

/// A provider that plays a fixed script over HTTP.
#[derive(Debug)]
pub struct ScriptedProvider {
    port: u16,
    state: Arc<Mutex<State>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ScriptedProvider {
    /// Starts a provider that plays these turns in order and keeps every body.
    pub fn start(turns: Vec<Turn>) -> Self {
        Self::launch(turns, true)
    }

    /// Starts a provider that records sizes and token counts but not bodies.
    pub fn start_without_bodies(turns: Vec<Turn>) -> Self {
        Self::launch(turns, false)
    }

    fn launch(turns: Vec<Turn>, keep_bodies: bool) -> Self {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(err) => panic!("could not bind a scripted provider: {err}"),
        };
        let port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(err) => panic!("could not read the scripted provider address: {err}"),
        };
        let state = Arc::new(Mutex::new(State {
            turns,
            keep_bodies,
            ..State::default()
        }));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_state = Arc::clone(&state);
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { break };
                let state = Arc::clone(&thread_state);
                std::thread::spawn(move || {
                    let _ = serve(stream, &state);
                });
            }
        });

        Self {
            port,
            state,
            shutdown,
            thread: Some(thread),
        }
    }

    /// The base URL to configure a provider with.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// How many `POST` requests arrived.
    #[must_use]
    pub fn request_count(&self) -> usize {
        self.state.lock().map_or(0, |state| state.requests.len())
    }

    /// Every `POST` request so far, in arrival order.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state
            .lock()
            .map_or_else(|_| Vec::new(), |state| state.requests.clone())
    }

    /// How many requests broke the prefix after a large one.
    #[must_use]
    pub fn prefix_breaks(&self) -> usize {
        self.requests()
            .windows(2)
            .filter(|pair| {
                pair[0].prompt_tokens >= 1000
                    && pair[1].cached_tokens.saturating_mul(2) < pair[1].prompt_tokens
            })
            .count()
    }
}

impl Drop for ScriptedProvider {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Unblock the accept loop by connecting once.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn ceil_quarter(bytes: usize) -> u64 {
    u64::try_from(bytes.div_ceil(4)).unwrap_or(u64::MAX)
}

fn prefix_key(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return String::new();
    };
    let tools = value
        .get("tools")
        .map_or_else(|| "[]".to_owned(), serde_json::Value::to_string);
    let messages = value
        .get("messages")
        .map_or_else(String::new, serde_json::Value::to_string);
    format!("{tools}{messages}")
}

fn common_prefix(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

/// Serves one connection.
fn serve(stream: TcpStream, state: &Arc<Mutex<State>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut stream = stream;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();

    let mut content_length = 0_usize;
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

    if method == "GET" {
        let body = r#"{"data":[{"id":"scripted"}]}"#;
        stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )?;
        return stream.flush();
    }

    let mut raw = vec![0_u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut raw)?;
    }
    let body = String::from_utf8_lossy(&raw).into_owned();

    let (turn, usage) = {
        let Ok(mut guard) = state.lock() else {
            return Ok(());
        };
        let index = guard.requests.len();
        let turn = guard.turns.get(index).cloned();
        let prompt_tokens = ceil_quarter(raw.len());
        let completion_tokens = turn.as_ref().map_or(0, |t| ceil_quarter(t.reply_bytes()));
        let key = prefix_key(&body);
        let cached_tokens = guard.previous_key.as_ref().map_or(0, |previous| {
            ceil_quarter(common_prefix(previous, &key)).min(prompt_tokens)
        });
        guard.previous_key = Some(key);
        let kept = if guard.keep_bodies {
            body
        } else {
            String::new()
        };
        guard.requests.push(RecordedRequest {
            path,
            body: kept,
            bytes: raw.len(),
            prompt_tokens,
            cached_tokens,
            completion_tokens,
        });
        match turn {
            Some(turn) => (Ok(turn), (prompt_tokens, completion_tokens, cached_tokens)),
            None => (
                Err(format!(
                    r#"{{"error":{{"message":"the script has {} turns and request {} has none"}}}}"#,
                    guard.turns.len(),
                    index.saturating_add(1)
                )),
                (prompt_tokens, completion_tokens, cached_tokens),
            ),
        }
    };

    let turn = match turn {
        Ok(turn) => turn,
        Err(message) => return write_error(&mut stream, 400, &message, &BTreeMap::new()),
    };
    if turn.delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(turn.delay_ms));
    }
    if let Some(error) = &turn.http_error {
        return write_error(&mut stream, error.status, &error.body, &error.headers);
    }

    let mut payload = stream_body(&turn, usage);
    if let Some(cut) = &turn.cut_stream {
        payload.truncate(cut.after_bytes);
    }
    stream.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n",
    )?;
    stream.write_all(&payload)?;
    stream.flush()
}

fn write_error(
    stream: &mut TcpStream,
    status: u16,
    body: &str,
    headers: &BTreeMap<String, String>,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    head.push_str("connection: close\r\n\r\n");
    head.push_str(body);
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

/// Builds the server-sent event body for a turn.
fn stream_body(turn: &Turn, usage: (u64, u64, u64)) -> Vec<u8> {
    use serde_json::json;

    let mut events = vec![json!({"choices":[{"index":0,"delta":{"role":"assistant"}}]})];
    if let Some(text) = &turn.text {
        events.push(json!({"choices":[{"index":0,"delta":{"content":text}}]}));
    }
    for (position, call) in turn.tool_calls.iter().enumerate() {
        events.push(json!({"choices":[{"index":0,"delta":{"tool_calls":[{
            "index":position,
            "id":call.id,
            "type":"function",
            "function":{"name":call.name,"arguments":call.argument_string()}
        }]}}]}));
    }
    let finish = turn.finish.clone().unwrap_or_else(|| {
        if turn.tool_calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        }
        .to_owned()
    });
    events.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]}));
    events.push(json!({"choices":[],"usage":{
        "prompt_tokens":usage.0,
        "completion_tokens":usage.1,
        "prompt_tokens_details":{"cached_tokens":usage.2}
    }}));

    let mut out = String::new();
    for event in events {
        let _ = write!(out, "data: {event}\n\n");
    }
    if !turn.no_done {
        out.push_str("data: [DONE]\n\n");
    }
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into()
    }

    fn post(provider: &ScriptedProvider, body: &str) -> (u16, String) {
        let response = agent()
            .post(&format!("{}/chat/completions", provider.base_url()))
            .header("content-type", "application/json")
            .send(body)
            .expect("request");
        let status = response.status().as_u16();
        (status, response.into_body().read_to_string().expect("body"))
    }

    #[test]
    fn a_reply_streams_text_usage_and_done() {
        let provider = ScriptedProvider::start(vec![Turn::text("hello")]);
        let (status, body) = post(&provider, "{}");
        assert_eq!(status, 200);
        assert!(body.contains(r#""content":"hello""#), "{body}");
        assert!(body.contains(r#""finish_reason":"stop""#), "{body}");
        assert!(body.contains(r#""usage":{"prompt_tokens""#), "{body}");
        assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
    }

    #[test]
    fn tool_calls_get_indices_and_the_tool_calls_finish() {
        let mut turn = Turn::call("a", "read_file", r#"{"path":"x"}"#);
        turn.tool_calls
            .extend(Turn::call("b", "shell", r#"{"command":"ls"}"#).tool_calls);
        let provider = ScriptedProvider::start(vec![turn]);
        let (_, body) = post(&provider, "{}");
        assert!(body.contains(r#""index":0"#), "{body}");
        assert!(body.contains(r#""index":1"#), "{body}");
        assert!(body.contains(r#""finish_reason":"tool_calls""#), "{body}");
    }

    #[test]
    fn string_arguments_are_sent_verbatim() {
        let provider = ScriptedProvider::start(vec![Turn::call("a", "shell", "{broken")]);
        let (_, body) = post(&provider, "{}");
        assert!(body.contains(r#""arguments":"{broken""#), "{body}");
    }

    #[test]
    fn usage_counts_bytes_and_the_shared_prefix() {
        let provider = ScriptedProvider::start(vec![Turn::text("a"), Turn::text("b")]);
        let first = r#"{"tools":[],"messages":["abcdef"]}"#;
        let second = r#"{"tools":[],"messages":["abcxyz!"]}"#;
        post(&provider, first);
        post(&provider, second);
        let requests = provider.requests();
        assert_eq!(requests[0].cached_tokens, 0);
        assert_eq!(requests[0].prompt_tokens, first.len().div_ceil(4) as u64);
        assert_eq!(requests[1].prompt_tokens, second.len().div_ceil(4) as u64);
        // The shared key prefix is `[]["abc`, seven bytes.
        let shared = r#"[]["abc"#.len();
        assert_eq!(requests[1].cached_tokens, shared.div_ceil(4) as u64);
        assert_eq!(requests[1].bytes, second.len());
        assert_eq!(requests[1].body, second);
    }

    #[test]
    fn a_prefix_break_is_counted_only_after_a_large_request() {
        let big = |letter: &str, size: usize| {
            format!(
                r#"{{"tools":[],"messages":[{{"content":"{}"}}]}}"#,
                letter.repeat(size)
            )
        };
        let provider = ScriptedProvider::start(vec![Turn::text("a"), Turn::text("b")]);
        post(&provider, &big("a", 4400));
        post(&provider, &big("b", 4400));
        assert_eq!(provider.prefix_breaks(), 1);

        let provider = ScriptedProvider::start(vec![Turn::text("a"), Turn::text("b")]);
        post(&provider, &big("a", 3800));
        post(&provider, &big("b", 3800));
        assert_eq!(provider.prefix_breaks(), 0);
    }

    #[test]
    fn an_http_error_turn_returns_its_status_and_headers() {
        let mut turn = Turn::http_error(429, "{}");
        turn.http_error
            .as_mut()
            .expect("error")
            .headers
            .insert("retry-after".to_owned(), "0".to_owned());
        let provider = ScriptedProvider::start(vec![turn]);
        let response = agent()
            .post(&format!("{}/chat/completions", provider.base_url()))
            .send("{}")
            .expect("request");
        assert_eq!(response.status().as_u16(), 429);
        assert_eq!(response.headers().get("retry-after").expect("header"), "0");
    }

    #[test]
    fn a_cut_stream_stops_after_the_given_bytes() {
        let mut turn = Turn::text("hello");
        turn.cut_stream = Some(CutStream { after_bytes: 40 });
        let provider = ScriptedProvider::start(vec![turn]);
        let (status, body) = post(&provider, "{}");
        assert_eq!(status, 200);
        assert_eq!(body.len(), 40);
    }

    #[test]
    fn a_request_past_the_script_gets_a_400_naming_the_turn_count() {
        let provider = ScriptedProvider::start(vec![Turn::text("only")]);
        post(&provider, "{}");
        let (status, body) = post(&provider, "{}");
        assert_eq!(status, 400);
        assert!(
            body.contains("the script has 1 turns and request 2 has none"),
            "{body}"
        );
        assert_eq!(provider.request_count(), 2);
    }

    #[test]
    fn get_requests_do_not_consume_turns() {
        let provider = ScriptedProvider::start(vec![Turn::text("first")]);
        let models = agent()
            .get(&format!("{}/models", provider.base_url()))
            .call()
            .expect("request");
        assert_eq!(models.status().as_u16(), 200);
        assert_eq!(provider.request_count(), 0);
        let (_, body) = post(&provider, "{}");
        assert!(body.contains("first"), "{body}");
        assert_eq!(provider.request_count(), 1);
    }
}
