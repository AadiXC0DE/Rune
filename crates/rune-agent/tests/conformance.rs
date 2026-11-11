// Integration tests assert by panicking.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// Every case drives a real listening endpoint, so these run only where a socket
// can be opened.
#![cfg(not(target_family = "wasm"))]

//! A two-step tool exchange through the real loop for every provider family.
//!
//! The server refuses what the vendor refuses, so a request shape the vendor
//! answers with a 400 fails here without a key or a network. Each rule is
//! written from the vendor's documented behaviour and takes what the model
//! produced from the recorded stream, never from what the encoder sent.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camino::Utf8PathBuf;
use rune_agent::History;
use rune_agent::steering::{Cancellation, SteeringQueue};
use rune_agent::turn::{Event, Host, StopReason, run_turn};
use rune_core::budget::BudgetSet;
use rune_core::config::{Effort, Provider as ProviderName, parse_provider};
use rune_core::error::Result;
use rune_net::compat::{EndpointCompat, ModelTraits};
use rune_net::message::ToolSpec;
use rune_net::provider::Provider;
use rune_net::transport::Endpoint;
use rune_policy::decision::Outcome;
use rune_tools::contract::{ExecutionContext, ToolOutput};
use serde_json::{Value, json};

const FINAL_TEXT: &str = "The directory is rune.";
const TOOL_NAME: &str = "current_directory";

/// One vendor rule: the message the server answers with when it fails.
type Rule = fn(&Value) -> std::result::Result<(), String>;

macro_rules! fixture {
    ($case:literal, $file:literal) => {
        include_str!(concat!(
            "../../rune-net/tests/fixtures/conformance/",
            $case,
            "/",
            $file
        ))
    };
}

/// The JSON payload of every data line of a recorded stream.
fn frames(recorded: &str) -> Vec<Value> {
    recorded
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|payload| serde_json::from_str(payload).ok())
        .collect()
}

/// The reasoning text a Chat Completions stream produced.
fn recorded_reasoning(recorded: &str) -> String {
    frames(recorded)
        .iter()
        .filter_map(|frame| frame["choices"][0]["delta"]["reasoning_content"].as_str())
        .collect()
}

/// The `reasoning_details` entries as the recorded frames carry them.
fn recorded_details(recorded: &str) -> Vec<Value> {
    frames(recorded)
        .iter()
        .filter_map(|frame| frame["choices"][0]["delta"]["reasoning_details"].as_array())
        .flatten()
        .cloned()
        .collect()
}

/// The thinking blocks of a recorded Messages stream, as text and signature.
fn recorded_thinking(recorded: &str) -> Vec<(String, String)> {
    let mut blocks: Vec<(u64, String, String)> = Vec::new();
    for frame in frames(recorded) {
        let index = frame["index"].as_u64().unwrap_or_default();
        match frame["type"].as_str() {
            Some("content_block_start") if frame["content_block"]["type"] == "thinking" => {
                blocks.push((index, String::new(), String::new()));
            }
            Some("content_block_delta") => {
                let Some(block) = blocks.iter_mut().find(|block| block.0 == index) else {
                    continue;
                };
                if let Some(text) = frame["delta"]["thinking"].as_str() {
                    block.1.push_str(text);
                }
                if let Some(signature) = frame["delta"]["signature"].as_str() {
                    block.2.push_str(signature);
                }
            }
            _ => {}
        }
    }
    blocks
        .into_iter()
        .map(|(_, text, signature)| (text, signature))
        .collect()
}

/// The finished reasoning items of a recorded Responses stream.
fn recorded_reasoning_items(recorded: &str) -> Vec<Value> {
    frames(recorded)
        .into_iter()
        .filter(|frame| {
            frame["type"] == "response.output_item.done" && frame["item"]["type"] == "reasoning"
        })
        .map(|frame| frame["item"].clone())
        .collect()
}

/// The signed parts of a recorded Gemini stream.
fn recorded_signed_parts(recorded: &str) -> Vec<Value> {
    frames(recorded)
        .iter()
        .filter_map(|frame| frame["candidates"][0]["content"]["parts"].as_array())
        .flatten()
        .filter(|part| part.get("thoughtSignature").is_some())
        .cloned()
        .collect()
}

fn role_messages<'a>(body: &'a Value, role: &'a str) -> impl Iterator<Item = &'a Value> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |message| message["role"] == role)
}

fn makes_calls(message: &Value) -> bool {
    message["tool_calls"]
        .as_array()
        .is_some_and(|calls| !calls.is_empty())
}

/// Every `tool` message answers the assistant message before it, and no
/// assistant message is empty.
fn openai_compatible(body: &Value) -> std::result::Result<(), String> {
    let mut open: Vec<&str> = Vec::new();
    for message in body["messages"].as_array().into_iter().flatten() {
        match message["role"].as_str() {
            Some("assistant") => {
                let has_content = match &message["content"] {
                    Value::String(text) => !text.is_empty(),
                    Value::Array(parts) => !parts.is_empty(),
                    _ => false,
                };
                if !has_content && !makes_calls(message) {
                    return Err("assistant message must have content or tool_calls".to_owned());
                }
                open = message["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|call| call["id"].as_str())
                    .collect();
            }
            Some("tool") => {
                let id = message["tool_call_id"].as_str().unwrap_or_default();
                if !open.contains(&id) {
                    return Err(format!(
                        "tool message `{id}` does not answer a preceding tool_calls id"
                    ));
                }
            }
            _ => open.clear(),
        }
    }
    Ok(())
}

fn deepseek(body: &Value) -> std::result::Result<(), String> {
    if body.get("tools").is_none() {
        return Ok(());
    }
    for message in role_messages(body, "assistant") {
        if !message["reasoning_content"].is_string() {
            return Err("Missing reasoning_content field in the assistant message".to_owned());
        }
    }
    Ok(())
}

/// A message that made calls carries the recorded reasoning back unchanged.
fn reasoning_comes_back(
    body: &Value,
    vendor: &str,
    recorded: &str,
) -> std::result::Result<(), String> {
    let reasoning = recorded_reasoning(recorded);
    for message in role_messages(body, "assistant").filter(|message| makes_calls(message)) {
        if message["reasoning_content"].as_str() != Some(reasoning.as_str()) {
            return Err(format!(
                "{vendor}: reasoning_content must be the recorded reasoning unchanged"
            ));
        }
    }
    Ok(())
}

fn moonshot(body: &Value) -> std::result::Result<(), String> {
    reasoning_comes_back(body, "moonshot", fixture!("kimi", "response-1.txt"))
}

fn zai(body: &Value) -> std::result::Result<(), String> {
    reasoning_comes_back(body, "zai", fixture!("glm", "response-1.txt"))
}

fn minimax(body: &Value) -> std::result::Result<(), String> {
    reasoning_comes_back(body, "minimax", fixture!("minimax", "response-1.txt"))
}

fn openrouter(body: &Value) -> std::result::Result<(), String> {
    let recorded = recorded_details(fixture!("openrouter_claude", "response-1.txt"));
    for message in role_messages(body, "assistant").filter(|message| makes_calls(message)) {
        if message["reasoning_details"].as_array() != Some(&recorded) {
            return Err("reasoning_details must equal the recorded array exactly".to_owned());
        }
    }
    Ok(())
}

fn mistral_id(id: &str) -> bool {
    id.len() == 9 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn mistral(body: &Value) -> std::result::Result<(), String> {
    let messages = body["messages"].as_array().map_or(&[][..], Vec::as_slice);
    for message in messages {
        let calls = message["tool_calls"].as_array().into_iter().flatten();
        let call_ids = calls.filter_map(|call| call["id"].as_str());
        for id in call_ids.chain(message["tool_call_id"].as_str()) {
            if !mistral_id(id) {
                return Err(format!("tool call id `{id}` must be 9 letters and digits"));
            }
        }
    }
    for pair in messages.windows(2) {
        if pair[0]["role"] == "tool" && pair[1]["role"] == "user" {
            return Err("a tool message must not be followed by a user message".to_owned());
        }
    }
    Ok(())
}

/// In an assistant message with `tool_use`, the recorded thinking blocks come
/// first, in order, and signed.
fn thinking_comes_first(
    body: &Value,
    recorded: &[(String, String)],
) -> std::result::Result<(), String> {
    for message in role_messages(body, "assistant") {
        let blocks = message["content"].as_array().map_or(&[][..], Vec::as_slice);
        if !blocks.iter().any(|block| block["type"] == "tool_use") {
            continue;
        }
        let is_thinking =
            |block: &Value| block["type"] == "thinking" || block["type"] == "redacted_thinking";
        let leading = blocks.iter().take_while(|block| is_thinking(block)).count();
        if blocks[leading..].iter().any(is_thinking) {
            return Err("thinking blocks must come before text and tool_use blocks".to_owned());
        }
        if leading != recorded.len() {
            return Err(format!(
                "expected {} recorded thinking blocks first, found {leading}",
                recorded.len()
            ));
        }
        for (block, (text, signature)) in blocks.iter().zip(recorded) {
            let sent = block["signature"].as_str().unwrap_or_default();
            if sent.is_empty() {
                return Err("thinking block has no signature".to_owned());
            }
            if sent != signature || block["thinking"].as_str() != Some(text) {
                return Err("thinking block was changed after it was recorded".to_owned());
            }
        }
    }
    Ok(())
}

fn anthropic_adaptive(body: &Value) -> std::result::Result<(), String> {
    if body["thinking"].get("budget_tokens").is_some() {
        return Err("budget_tokens is not accepted by this model".to_owned());
    }
    for field in ["temperature", "top_p", "top_k"] {
        if body.get(field).is_some() {
            return Err(format!("{field} is not accepted by this model"));
        }
    }
    let choice = &body["tool_choice"]["type"];
    if choice == "any" || choice == "tool" {
        return Err("tool_choice forces tool use, which this model refuses".to_owned());
    }
    thinking_comes_first(
        body,
        &recorded_thinking(fixture!("anthropic_adaptive", "response-1.txt")),
    )
}

fn anthropic_budget(body: &Value) -> std::result::Result<(), String> {
    if let Some(budget) = body["thinking"].get("budget_tokens") {
        let (budget, ceiling) = (budget.as_u64(), body["max_tokens"].as_u64());
        if budget
            .zip(ceiling)
            .is_none_or(|(budget, max)| budget >= max)
        {
            return Err("thinking.budget_tokens must be less than max_tokens".to_owned());
        }
    }
    thinking_comes_first(
        body,
        &recorded_thinking(fixture!("anthropic_budget", "response-1.txt")),
    )
}

fn responses(body: &Value) -> std::result::Result<(), String> {
    let input = body["input"].as_array().map_or(&[][..], Vec::as_slice);
    let mut called: Vec<&str> = Vec::new();
    for item in input {
        match item["type"].as_str() {
            Some("function_call") => called.extend(item["call_id"].as_str()),
            Some("function_call_output") => {
                let id = item["call_id"].as_str().unwrap_or_default();
                if !called.contains(&id) {
                    return Err(format!("no tool call found for function call output {id}"));
                }
            }
            _ => {}
        }
    }
    if !called.is_empty() {
        let sent: Vec<&Value> = input
            .iter()
            .filter(|item| item["type"] == "reasoning")
            .collect();
        let recorded = recorded_reasoning_items(fixture!("responses", "response-1.txt"));
        if !sent.iter().copied().eq(recorded.iter()) {
            return Err("reasoning items must come back unchanged".to_owned());
        }
    }
    Ok(())
}

fn gemini(body: &Value) -> std::result::Result<(), String> {
    let contents = body["contents"].as_array().map_or(&[][..], Vec::as_slice);
    let parts_of = |content: &'_ Value| content["parts"].as_array().cloned().unwrap_or_default();
    let mut replayed = Vec::new();
    for (index, content) in contents.iter().enumerate() {
        let parts = parts_of(content);
        let calls: Vec<&Value> = parts
            .iter()
            .filter_map(|part| part.get("functionCall"))
            .collect();
        if calls.is_empty() {
            continue;
        }
        let next = contents
            .get(index.saturating_add(1))
            .map(parts_of)
            .unwrap_or_default();
        let answers: Vec<&Value> = next
            .iter()
            .filter_map(|part| part.get("functionResponse"))
            .collect();
        let answered = answers.len() == calls.len()
            && calls.iter().zip(&answers).all(|(call, answer)| {
                call["name"] == answer["name"]
                    && (call.get("id").is_none() || call["id"] == answer["id"])
            });
        if !answered {
            return Err("a functionResponse must follow each functionCall".to_owned());
        }
        replayed.extend(parts);
    }
    if !replayed.is_empty() {
        for signed in recorded_signed_parts(fixture!("gemini", "response-1.txt")) {
            if !replayed.contains(&signed) {
                return Err("a replayed part lost its thoughtSignature".to_owned());
            }
        }
    }
    Ok(())
}

/// What a server saw in one request.
struct Seen {
    body: Value,
    refusal: Option<String>,
}

/// A loopback endpoint that applies a family's rules to every request.
struct StrictServer {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StrictServer {
    fn start(rules: Vec<Rule>, responses: Vec<&'static str>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        listener.set_nonblocking(true).expect("nonblocking");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let (seen, stop) = (Arc::clone(&seen), Arc::clone(&stop));
            move || {
                let mut scripted = responses.into_iter();
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            serve(stream, &rules, &mut scripted, &seen).expect("serve request");
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                }
            }
        });
        Self {
            url,
            seen,
            stop,
            thread: Some(thread),
        }
    }

    /// Stops the server and returns what it saw.
    fn finish(mut self) -> Vec<Seen> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("server thread");
        }
        std::mem::take(&mut *self.seen.lock().expect("seen"))
    }
}

/// Answers one request and closes the connection, so one thread can serve a
/// client that opens a new connection per request.
fn serve(
    mut stream: TcpStream,
    rules: &[Rule],
    scripted: &mut impl Iterator<Item = &'static str>,
    seen: &Mutex<Vec<Seen>>,
) -> std::io::Result<()> {
    // `accept` inherits the listener's nonblocking mode on macOS.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let body = read_body(&stream)?;
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let reply = match rules.iter().find_map(|rule| rule(&parsed).err()) {
        Some(message) => Err(message),
        None => scripted
            .next()
            .ok_or_else(|| "the server has no response left for this request".to_owned()),
    };
    let (status, kind, payload) = match &reply {
        Ok(response) => ("200 OK", "text/event-stream", (*response).to_owned()),
        Err(message) => (
            "400 Bad Request",
            "application/json",
            json!({ "error": { "message": message, "type": "invalid_request_error" } }).to_string(),
        ),
    };
    seen.lock().expect("seen").push(Seen {
        body: parsed,
        refusal: reply.err(),
    });
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        payload.len()
    )?;
    stream.write_all(payload.as_bytes())?;
    stream.flush()
}

fn read_body(stream: &TcpStream) -> std::io::Result<Vec<u8>> {
    let mut reader = BufReader::new(stream);
    let mut length = 0;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(body)
}

/// A host that answers the one tool and records its events.
struct ConformanceHost {
    endpoint: Endpoint,
    dialect: Box<dyn Provider>,
    provider: &'static str,
    /// The vendor's real address, so the preset and host rules apply.
    vendor_url: String,
    model: &'static str,
    events: Mutex<Vec<Event>>,
    cancellation: Cancellation,
    steering: SteeringQueue,
    limits: BudgetSet,
}

impl Host for ConformanceHost {
    fn dialect(&self) -> &dyn Provider {
        self.dialect.as_ref()
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn model(&self) -> String {
        self.model.to_owned()
    }

    fn instructions(&self) -> String {
        "You are a test assistant.".to_owned()
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: TOOL_NAME.to_owned(),
            description: "Print the current directory.".to_owned(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }]
    }

    fn effort(&self) -> Effort {
        Effort::High
    }

    fn compat(&self, model: &str) -> (EndpointCompat, ModelTraits) {
        rune_net::compat::resolve(self.provider, Some(&self.vendor_url), model)
    }

    fn emit(&self, event: Event) {
        self.events.lock().expect("events").push(event);
    }

    fn execute(&self, _name: &str, _arguments: &Value) -> Result<ToolOutput> {
        Ok(ToolOutput::success("rune"))
    }

    fn decide(&self, _name: &str, _target: Option<&str>) -> (Outcome, String) {
        (Outcome::Allow, "allowed".to_owned())
    }

    fn context(&self) -> ExecutionContext {
        ExecutionContext::new(Utf8PathBuf::from("/tmp/rune-test"))
    }

    fn limits(&self) -> BudgetSet {
        self.limits.clone()
    }

    fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    fn steering(&self) -> &SteeringQueue {
        &self.steering
    }
}

/// The dialect a configured provider name speaks.
fn dialect_for(provider: &ProviderName) -> Box<dyn Provider> {
    match provider {
        ProviderName::Anthropic => Box::new(rune_net::anthropic::Anthropic),
        ProviderName::Responses => Box::new(rune_net::responses::Responses),
        ProviderName::Gemini => Box::new(rune_net::gemini::Gemini),
        _ => Box::new(rune_net::chat_completions::ChatCompletions),
    }
}

/// One provider family, with the rules its server applies.
struct Case {
    name: &'static str,
    provider: &'static str,
    model: &'static str,
    first: &'static str,
    second: &'static str,
    /// The last rule is the one the family adds.
    rules: &'static [Rule],
    /// Breaks a real second request in the way the family's rule guards.
    spoil: fn(&mut Value),
}

fn assistant_with_calls(body: &mut Value) -> &mut Value {
    body["messages"]
        .as_array_mut()
        .expect("messages")
        .iter_mut()
        .find(|message| message["role"] == "assistant" && makes_calls(message))
        .expect("an assistant message with tool calls")
}

fn change_reasoning(body: &mut Value) {
    let message = assistant_with_calls(body);
    assert!(
        message["reasoning_content"].is_string(),
        "no reasoning sent"
    );
    message["reasoning_content"] = json!("changed");
}

fn assistant_blocks(body: &mut Value) -> &mut Vec<Value> {
    body["messages"]
        .as_array_mut()
        .expect("messages")
        .iter_mut()
        .find(|message| message["role"] == "assistant")
        .expect("an assistant message")["content"]
        .as_array_mut()
        .expect("content blocks")
}

fn spoil_generic(body: &mut Value) {
    let messages = body["messages"].as_array_mut().expect("messages");
    let tool = messages
        .iter_mut()
        .find(|message| message["role"] == "tool")
        .expect("a tool message");
    tool["tool_call_id"] = json!("unanswered");
}

fn spoil_deepseek(body: &mut Value) {
    let message = assistant_with_calls(body);
    message
        .as_object_mut()
        .expect("message")
        .remove("reasoning_content")
        .expect("reasoning_content was sent");
}

fn spoil_openrouter(body: &mut Value) {
    let message = assistant_with_calls(body);
    let details = message["reasoning_details"]
        .as_array_mut()
        .expect("details");
    details[0]["text"] = json!("changed");
}

fn spoil_mistral(body: &mut Value) {
    assistant_with_calls(body)["tool_calls"][0]["id"] = json!("abcd1234");
}

fn spoil_adaptive(body: &mut Value) {
    let blocks = assistant_blocks(body);
    assert_eq!(blocks[0]["type"], "thinking", "no thinking block first");
    blocks.rotate_left(1);
}

fn spoil_budget(body: &mut Value) {
    let ceiling = body["max_tokens"].clone();
    assert!(
        body["thinking"]["budget_tokens"].is_u64(),
        "no thinking budget sent"
    );
    body["thinking"]["budget_tokens"] = ceiling;
}

fn spoil_responses(body: &mut Value) {
    let input = body["input"].as_array_mut().expect("input");
    let item = input
        .iter_mut()
        .find(|item| item["type"] == "reasoning")
        .expect("a reasoning item");
    item["encrypted_content"] = json!("changed");
}

fn spoil_gemini(body: &mut Value) {
    let contents = body["contents"].as_array_mut().expect("contents");
    let part = contents
        .iter_mut()
        .flat_map(|content| content["parts"].as_array_mut().expect("parts"))
        .find(|part| part.get("functionCall").is_some())
        .expect("a function call part");
    part.as_object_mut()
        .expect("part")
        .remove("thoughtSignature")
        .expect("thoughtSignature was sent");
}

const CASES: [Case; 11] = [
    Case {
        name: "generic",
        provider: "chat_completions",
        model: "local-model",
        first: fixture!("generic", "response-1.txt"),
        second: fixture!("generic", "response-2.txt"),
        rules: &[openai_compatible],
        spoil: spoil_generic,
    },
    Case {
        name: "deepseek",
        provider: "deepseek",
        model: "deepseek-flash",
        first: fixture!("deepseek", "response-1.txt"),
        second: fixture!("deepseek", "response-2.txt"),
        rules: &[openai_compatible, deepseek],
        spoil: spoil_deepseek,
    },
    Case {
        name: "kimi",
        provider: "moonshot",
        model: "kimi-k3.1-pro",
        first: fixture!("kimi", "response-1.txt"),
        second: fixture!("kimi", "response-2.txt"),
        rules: &[openai_compatible, moonshot],
        spoil: change_reasoning,
    },
    Case {
        name: "glm",
        provider: "zai",
        model: "glm-5.3",
        first: fixture!("glm", "response-1.txt"),
        second: fixture!("glm", "response-2.txt"),
        rules: &[openai_compatible, zai],
        spoil: change_reasoning,
    },
    Case {
        name: "minimax",
        provider: "minimax",
        model: "MiniMax-M3.1-Flash",
        first: fixture!("minimax", "response-1.txt"),
        second: fixture!("minimax", "response-2.txt"),
        rules: &[openai_compatible, minimax],
        spoil: change_reasoning,
    },
    Case {
        name: "openrouter_claude",
        provider: "openrouter",
        model: "anthropic/claude-sonnet-5-5",
        first: fixture!("openrouter_claude", "response-1.txt"),
        second: fixture!("openrouter_claude", "response-2.txt"),
        rules: &[openai_compatible, openrouter],
        spoil: spoil_openrouter,
    },
    Case {
        name: "mistral",
        provider: "mistral",
        model: "devstral-medium-latest",
        first: fixture!("mistral", "response-1.txt"),
        second: fixture!("mistral", "response-2.txt"),
        rules: &[openai_compatible, mistral],
        spoil: spoil_mistral,
    },
    Case {
        name: "anthropic_adaptive",
        provider: "anthropic",
        model: "claude-opus-5-5",
        first: fixture!("anthropic_adaptive", "response-1.txt"),
        second: fixture!("anthropic_adaptive", "response-2.txt"),
        rules: &[anthropic_adaptive],
        spoil: spoil_adaptive,
    },
    Case {
        name: "anthropic_budget",
        provider: "anthropic",
        model: "claude-haiku-4-5",
        first: fixture!("anthropic_budget", "response-1.txt"),
        second: fixture!("anthropic_budget", "response-2.txt"),
        rules: &[anthropic_budget],
        spoil: spoil_budget,
    },
    Case {
        // The name selects the Responses dialect; it resolves to the `openai`
        // preset, whereas `openai` alone configures Chat Completions.
        name: "responses",
        provider: "responses",
        model: "gpt-6.1-sol",
        first: fixture!("responses", "response-1.txt"),
        second: fixture!("responses", "response-2.txt"),
        rules: &[responses],
        spoil: spoil_responses,
    },
    Case {
        name: "gemini",
        provider: "gemini",
        model: "gemini-3.8-flash",
        first: fixture!("gemini", "response-1.txt"),
        second: fixture!("gemini", "response-2.txt"),
        rules: &[gemini],
        spoil: spoil_gemini,
    },
];

/// Runs one exchange against `rules` and returns the request bodies.
fn run_case(case: &Case, rules: &[Rule]) -> std::result::Result<Vec<Value>, String> {
    let name = case.name;
    let server = StrictServer::start(rules.to_vec(), vec![case.first, case.second]);
    let vendor_url = rune_net::providers::lookup(case.provider)
        .and_then(|entry| entry.base_url)
        .map_or_else(|| server.url.clone(), str::to_owned);
    let mut endpoint = Endpoint::new(server.url.clone(), "test-key");
    endpoint.offline = true;
    let host = ConformanceHost {
        endpoint,
        dialect: dialect_for(&parse_provider(case.provider)),
        provider: case.provider,
        vendor_url,
        model: case.model,
        events: Mutex::new(Vec::new()),
        cancellation: Cancellation::new(),
        steering: SteeringQueue::new(8),
        limits: BudgetSet::new(),
    };
    let mut history = History::new();
    history.push_user("which directory is this");
    let result = run_turn(&mut history, &host);
    let seen = server.finish();
    if let Some(refusal) = seen.iter().find_map(|request| request.refusal.as_deref()) {
        return Err(format!("{name}: the server answered 400: {refusal}"));
    }
    let outcome = result.map_err(|error| format!("{name}: the turn failed: {error}"))?;
    if seen.len() != 2 {
        return Err(format!("{name}: expected 2 requests, saw {}", seen.len()));
    }
    if outcome.text != FINAL_TEXT || outcome.stop_reason != StopReason::Completed {
        return Err(format!(
            "{name}: ended with {:?} and text {:?}",
            outcome.stop_reason, outcome.text
        ));
    }
    let streamed: String = host
        .events
        .lock()
        .expect("events")
        .iter()
        .filter_map(|event| match event {
            Event::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    if streamed != FINAL_TEXT {
        return Err(format!("{name}: streamed text was {streamed:?}"));
    }
    Ok(seen.into_iter().map(|request| request.body).collect())
}

#[test]
fn every_family_completes_a_two_step_tool_exchange() {
    let mut failures = Vec::new();
    for case in &CASES {
        let bodies = match run_case(case, case.rules) {
            Ok(bodies) => bodies,
            Err(failure) => {
                failures.push(failure);
                continue;
            }
        };
        let family_rule = case.rules.last().expect("a rule");
        let mut wrong = bodies[1].clone();
        (case.spoil)(&mut wrong);
        if family_rule(&wrong).is_ok() {
            failures.push(format!("{}: the rule accepted a broken body", case.name));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A correct body for one rule, and how to break it.
struct Wrong {
    name: &'static str,
    rule: Rule,
    right: Value,
    spoil: fn(&mut Value),
    expected: &'static str,
}

fn wrong(
    name: &'static str,
    rule: Rule,
    right: Value,
    spoil: fn(&mut Value),
    expected: &'static str,
) -> Wrong {
    Wrong {
        name,
        rule,
        right,
        spoil,
        expected,
    }
}

/// A Chat Completions request whose assistant message carries `extra`.
fn chat_exchange(id: &str, extra: &Value) -> Value {
    let mut assistant = json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": id,
            "type": "function",
            "function": { "name": TOOL_NAME, "arguments": "{}" },
        }],
    });
    for (key, value) in extra.as_object().expect("extra fields") {
        assistant[key] = value.clone();
    }
    json!({
        "tools": [{ "type": "function" }],
        "messages": [
            { "role": "system", "content": "be brief" },
            { "role": "user", "content": "which directory is this" },
            assistant,
            { "role": "tool", "tool_call_id": id, "content": "rune" },
        ],
    })
}

fn recorded_text_exchange(recorded: &str) -> Value {
    let reasoning = recorded_reasoning(recorded);
    chat_exchange("call_1", &json!({ "reasoning_content": reasoning }))
}

/// A Messages request replaying the recorded thinking before a tool call.
fn anthropic_exchange(recorded: &str, extra: &Value) -> Value {
    let mut blocks: Vec<Value> = recorded_thinking(recorded)
        .into_iter()
        .map(|(text, signature)| {
            json!({ "type": "thinking", "thinking": text, "signature": signature })
        })
        .collect();
    blocks.push(json!({ "type": "tool_use", "id": "toolu_1", "name": TOOL_NAME, "input": {} }));
    let mut body = json!({
        "max_tokens": 8192,
        "messages": [
            { "role": "user", "content": [{ "type": "text", "text": "where" }] },
            { "role": "assistant", "content": blocks },
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "toolu_1", "content": "rune" },
            ] },
        ],
    });
    for (key, value) in extra.as_object().expect("extra fields") {
        body[key] = value.clone();
    }
    body
}

fn set_tool_id(body: &mut Value, id: &str) {
    let message = assistant_with_calls(body);
    message["tool_calls"][0]["id"] = json!(id);
    let messages = body["messages"].as_array_mut().expect("messages");
    messages.last_mut().expect("a tool message")["tool_call_id"] = json!(id);
}

fn wrong_bodies() -> Vec<Wrong> {
    let kimi = fixture!("kimi", "response-1.txt");
    let glm = fixture!("glm", "response-1.txt");
    let max = fixture!("minimax", "response-1.txt");
    let router = recorded_details(fixture!("openrouter_claude", "response-1.txt"));
    let adaptive = fixture!("anthropic_adaptive", "response-1.txt");
    let budget = fixture!("anthropic_budget", "response-1.txt");
    let plain = chat_exchange("call_1", &json!({}));
    let reasoned = chat_exchange("call_1", &json!({ "reasoning_content": "" }));
    let details = chat_exchange("call_1", &json!({ "reasoning_details": router }));
    let nine = chat_exchange("D681PevKs", &json!({}));
    let replies = json!({
        "input": [
            { "type": "reasoning", "id": "x" },
            { "type": "function_call", "call_id": "call_1", "name": TOOL_NAME, "arguments": "{}" },
            { "type": "function_call_output", "call_id": "call_1", "output": "rune" },
        ],
    });
    let mut replies = replies;
    replies["input"][0] =
        recorded_reasoning_items(fixture!("responses", "response-1.txt"))[0].clone();
    let signed = recorded_signed_parts(fixture!("gemini", "response-1.txt"));
    let contents = json!({
        "contents": [
            { "role": "user", "parts": [{ "text": "where" }] },
            { "role": "model", "parts": signed },
            { "role": "user", "parts": [
                { "functionResponse": { "name": TOOL_NAME, "response": { "output": "rune" } } },
            ] },
        ],
    });
    vec![
        wrong(
            "a tool message answering no call",
            openai_compatible,
            plain.clone(),
            |body| body["messages"][3]["tool_call_id"] = json!("other"),
            "does not answer",
        ),
        wrong(
            "an empty assistant message",
            openai_compatible,
            plain.clone(),
            |body| {
                let message = assistant_with_calls(body).as_object_mut().expect("message");
                message.remove("tool_calls");
                body["messages"].as_array_mut().expect("messages").pop();
            },
            "content or tool_calls",
        ),
        wrong(
            "an assistant message without reasoning_content",
            deepseek,
            reasoned.clone(),
            |body| {
                let message = assistant_with_calls(body).as_object_mut().expect("message");
                message.remove("reasoning_content");
            },
            "Missing reasoning_content field in the assistant message",
        ),
        wrong(
            "changed moonshot reasoning",
            moonshot,
            recorded_text_exchange(kimi),
            change_reasoning,
            "moonshot",
        ),
        wrong(
            "changed zai reasoning",
            zai,
            recorded_text_exchange(glm),
            change_reasoning,
            "zai",
        ),
        wrong(
            "changed minimax reasoning",
            minimax,
            recorded_text_exchange(max),
            change_reasoning,
            "minimax",
        ),
        wrong(
            "changed reasoning details",
            openrouter,
            details,
            spoil_openrouter,
            "recorded array exactly",
        ),
        wrong(
            "an eight character call id",
            mistral,
            nine.clone(),
            |body| set_tool_id(body, "abcd1234"),
            "9 letters and digits",
        ),
        wrong(
            "a tool message followed by a user message",
            mistral,
            nine,
            |body| {
                let messages = body["messages"].as_array_mut().expect("messages");
                messages.push(json!({ "role": "user", "content": "and now" }));
            },
            "followed by a user message",
        ),
        wrong(
            "a thinking budget on an adaptive model",
            anthropic_adaptive,
            anthropic_exchange(adaptive, &json!({})),
            |body| body["thinking"] = json!({ "type": "enabled", "budget_tokens": 1024 }),
            "budget_tokens",
        ),
        wrong(
            "a temperature on an adaptive model",
            anthropic_adaptive,
            anthropic_exchange(adaptive, &json!({})),
            |body| body["temperature"] = json!(0.5),
            "temperature",
        ),
        wrong(
            "a forced tool choice on an adaptive model",
            anthropic_adaptive,
            anthropic_exchange(adaptive, &json!({})),
            |body| body["tool_choice"] = json!({ "type": "any" }),
            "tool_choice",
        ),
        wrong(
            "a thinking block after a tool_use block",
            anthropic_adaptive,
            anthropic_exchange(adaptive, &json!({})),
            spoil_adaptive,
            "before text and tool_use",
        ),
        wrong(
            "an unsigned thinking block on an adaptive model",
            anthropic_adaptive,
            anthropic_exchange(adaptive, &json!({})),
            |body| assistant_blocks(body)[0]["signature"] = json!(""),
            "no signature",
        ),
        wrong(
            "a budget equal to the ceiling",
            anthropic_budget,
            anthropic_exchange(
                budget,
                &json!({ "thinking": { "type": "enabled", "budget_tokens": 1024 } }),
            ),
            spoil_budget,
            "less than max_tokens",
        ),
        wrong(
            "an unsigned thinking block on a budget model",
            anthropic_budget,
            anthropic_exchange(budget, &json!({})),
            |body| {
                let block = assistant_blocks(body)[0].as_object_mut().expect("block");
                block.remove("signature");
            },
            "no signature",
        ),
        wrong(
            "an output answering no call",
            responses,
            replies.clone(),
            |body| body["input"][2]["call_id"] = json!("other"),
            "no tool call found",
        ),
        wrong(
            "changed encrypted reasoning",
            responses,
            replies,
            spoil_responses,
            "come back unchanged",
        ),
        wrong(
            "a call with no response",
            gemini,
            contents.clone(),
            |body| {
                body["contents"].as_array_mut().expect("contents").pop();
            },
            "functionResponse must follow",
        ),
        wrong(
            "a part without its signature",
            gemini,
            contents,
            spoil_gemini,
            "lost its thoughtSignature",
        ),
    ]
}

#[test]
fn every_strict_rule_rejects_the_shape_it_guards() {
    for case in wrong_bodies() {
        (case.rule)(&case.right)
            .unwrap_or_else(|message| panic!("{}: right body refused: {message}", case.name));
        let mut body = case.right.clone();
        (case.spoil)(&mut body);
        let message = (case.rule)(&body)
            .expect_err(&format!("{}: the rule accepted the wrong body", case.name));
        assert!(
            message.contains(case.expected),
            "{}: message was {message:?}",
            case.name
        );
    }

    let refuse: Rule = |_| Err("refused by the rule under test".to_owned());
    let failure = run_case(&CASES[0], &[refuse]).expect_err("a refused request fails the case");
    assert!(
        failure.contains("400") && failure.contains("refused by the rule under test"),
        "{failure}"
    );
}
