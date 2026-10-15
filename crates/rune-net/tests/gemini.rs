//! Native Gemini wire fixtures, with no compatibility endpoint.

#![cfg(not(target_family = "wasm"))]
#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rune_net::gemini::Gemini;
use rune_net::message::{ContentPart, Message, Role, ToolSpec};
use rune_net::provider::RequestPlan;
use rune_net::stream::{FinishReason, ProviderEvent};
use rune_net::transport::{AuthStyle, Endpoint, agent, stream_completion_observed};
use serde_json::{Value, json};

fn listener() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    listener
}

fn request(listener: &TcpListener) -> (TcpStream, Value) {
    let started = Instant::now();
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(started.elapsed() < Duration::from_secs(10), "no request");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("accept: {error}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("write timeout");
    let mut reader = std::io::BufReader::new(&mut stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("request line");
    assert_eq!(
        line,
        "POST /v1beta/models/gemini-fixture:streamGenerateContent?alt=sse HTTP/1.1\r\n"
    );
    let mut length = 0;
    let mut authenticated = false;
    loop {
        line.clear();
        assert!(reader.read_line(&mut line).expect("header") > 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            assert!(!name.eq_ignore_ascii_case("authorization"));
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().expect("content length");
            }
            if name.eq_ignore_ascii_case("x-goog-api-key") {
                assert_eq!(value.trim(), "fixture-key");
                authenticated = true;
            }
        }
    }
    assert!(authenticated, "native key header");
    let mut body = vec![0; length];
    reader.read_exact(&mut body).expect("request body");
    (stream, serde_json::from_slice(&body).expect("JSON request"))
}

fn respond(stream: &mut TcpStream, frames: &[Value]) {
    use std::fmt::Write as _;
    let mut body = String::new();
    for frame in frames {
        write!(body, "data: {frame}\n\n").expect("frame");
    }
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("response");
}

#[test]
fn native_text_tool_result_signature_and_usage_round_trip() {
    // Exercise both native calls without IDs and newer calls with IDs.
    for native_id in [None, Some("native-call-1")] {
        let listener = listener();
        let address = listener.local_addr().expect("address");
        let mut call = json!({"name": "lookup", "args": {"key": "alpha"}});
        if let Some(id) = native_id {
            call["id"] = json!(id);
        }
        let signed_part = json!({"functionCall": call, "thoughtSignature": "opaque-signed-state"});
        let replay_part = signed_part.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, body) = request(&listener);
            assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be helpful");
            assert_eq!(
                body["contents"][0],
                json!({"role": "user", "parts": [{"text": "look up alpha"}]})
            );
            assert_eq!(
                body["tools"][0]["functionDeclarations"][0]["name"],
                "lookup"
            );
            assert_eq!(
                body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["type"],
                "object"
            );
            assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
            assert_eq!(body["generationConfig"]["maxOutputTokens"], 100);
            assert!(body.get("messages").is_none());
            respond(
                &mut stream,
                &[
                    json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "looking "}]}}]}),
                    json!({"candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "up"}, signed_part]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 4}}),
                    // Usage can arrive after the candidate's finish reason.
                    json!({"usageMetadata": {"promptTokenCount": 12, "candidatesTokenCount": 4, "cachedContentTokenCount": 0, "thoughtsTokenCount": 2}}),
                ],
            );
            let (mut stream, body) = request(&listener);
            assert_eq!(body["contents"][1]["role"], "model");
            assert_eq!(body["contents"][1]["parts"][2], replay_part);
            assert_eq!(
                body["contents"][1]["parts"]
                    .as_array()
                    .expect("parts")
                    .len(),
                3,
                "no duplicated replay text or calls"
            );
            let result = &body["contents"][2]["parts"][0]["functionResponse"];
            assert_eq!(result["name"], "lookup");
            assert_eq!(result["response"], json!({"output": "value for alpha"}));
            assert_eq!(result.get("id").and_then(Value::as_str), native_id);
            respond(
                &mut stream,
                &[
                    json!({"candidates": [{"content": {"parts": [{"text": "found it"}]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 3}}),
                ],
            );
        });
        let endpoint = Endpoint::new(format!("http://{address}/v1beta"), "fixture-key")
            .with_auth(AuthStyle::GoogleApiKeyHeader);
        let client = agent();
        let mut plan = RequestPlan::new("models/gemini-fixture");
        plan.instructions = "be helpful".to_owned();
        plan.messages = vec![Message::user("look up alpha")];
        plan.max_output_tokens = Some(100);
        plan.tools = vec![ToolSpec {
            name: "lookup".to_owned(),
            description: "Look up a key".to_owned(),
            input_schema: json!({"type": "object", "properties": {"key": {"type": "string"}}}),
        }];
        let mut observed = Vec::new();
        let outcome = stream_completion_observed(
            &client,
            &endpoint,
            &Gemini,
            &plan,
            Duration::from_secs(5),
            &|| false,
            &mut |event| observed.push(event.clone()),
        )
        .expect("first completion");
        assert_eq!(outcome.events, observed);
        assert_eq!(outcome.text(), "looking up");
        assert_eq!(outcome.finish, Some(FinishReason::ToolCalls));
        assert_eq!(outcome.usage.input_tokens, Some(12));
        assert_eq!(outcome.usage.output_tokens, Some(4));
        assert_eq!(outcome.usage.cache_read_tokens, Some(0));
        assert_eq!(outcome.usage.reasoning_tokens, Some(2));
        assert_eq!(outcome.usage.cache_write_tokens, None);
        let calls = outcome.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "lookup");
        assert_eq!(
            serde_json::from_str::<Value>(&calls[0].2).expect("arguments"),
            json!({"key": "alpha"})
        );
        let mut assistant = Message::assistant(vec![
            ContentPart::Text {
                text: outcome.text(),
            },
            ContentPart::ToolCall {
                id: rune_core::id::ToolCallId::new(&calls[0].0).expect("call id"),
                name: calls[0].1.clone(),
                arguments: calls[0].2.clone(),
            },
        ]);
        assistant.replay = outcome.replay;
        // Persist and reopen the message before sending the tool result.
        let assistant =
            serde_json::from_str(&serde_json::to_string(&assistant).expect("serialize"))
                .expect("reopen");
        plan.messages.push(assistant);
        plan.messages.push(Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult {
                id: rune_core::id::ToolCallId::new(&calls[0].0).expect("call id"),
                name: "lookup".to_owned(),
                content: "value for alpha".to_owned(),
                is_error: false,
            }],
            replay: None,
        });
        let outcome = stream_completion_observed(
            &client,
            &endpoint,
            &Gemini,
            &plan,
            Duration::from_secs(5),
            &|| false,
            &mut |_| {},
        )
        .expect("tool result completion");
        assert_eq!(outcome.text(), "found it");
        assert_eq!(outcome.finish, Some(FinishReason::Stop));
        assert_eq!(outcome.usage.input_tokens, Some(20));
        assert_eq!(outcome.usage.output_tokens, Some(3));
        assert!(matches!(
            outcome.events.last(),
            Some(ProviderEvent::Finish {
                reason: FinishReason::Stop
            })
        ));
        server.join().expect("fixture server");
    }
}

#[test]
fn cancelling_a_silent_native_stream_closes_the_connection() {
    let listener = listener();
    let address = listener.local_addr().expect("address");
    let (ready, received) = std::sync::mpsc::channel();
    let cancelled = std::sync::Arc::new(AtomicBool::new(false));
    let server = std::thread::spawn(move || {
        let (mut stream, _) = request(&listener);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("response head");
        ready.send(()).expect("ready");
        assert_eq!(stream.read(&mut [0]).expect("client closes"), 0);
    });
    let signal = cancelled.clone();
    let worker = std::thread::spawn(move || {
        let mut plan = RequestPlan::new("gemini-fixture");
        plan.messages = vec![Message::user("hello")];
        stream_completion_observed(
            &agent(),
            &Endpoint::new(format!("http://{address}/v1beta"), "fixture-key")
                .with_auth(AuthStyle::GoogleApiKeyHeader),
            &Gemini,
            &plan,
            Duration::from_secs(30),
            &|| signal.load(Ordering::Acquire),
            &mut |_| {},
        )
    });
    received
        .recv_timeout(Duration::from_secs(5))
        .expect("response headers");
    let started = Instant::now();
    cancelled.store(true, Ordering::Release);
    let error = worker.join().expect("worker").expect_err("cancelled");
    assert_eq!(error.kind(), rune_net::FailureKind::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(2));
    server.join().expect("server observed close");
}
