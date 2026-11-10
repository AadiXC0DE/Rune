//! Signed thinking replayed through a real Messages endpoint.

#![cfg(not(target_family = "wasm"))]
#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use rune_core::id::ToolCallId;
use rune_net::anthropic::Anthropic;
use rune_net::message::{ContentPart, Message, Role};
use rune_net::provider::RequestPlan;
use rune_net::stream::FinishReason;
use rune_net::transport::{AuthStyle, Endpoint, UreqFetch, stream_completion};
use serde_json::{Value, json};

const THINKING_TOOL_USE: &str = include_str!("fixtures/anthropic/thinking_tool_use/response.txt");

const SIGNATURE: &str = "EqQBCkYICxgCKkB3n0sZ0kq9rT4xV7mJ2cWb8fLp1yQe6uHa5dNo3iKs9gRt2vXw4zYc7bMn0lPq8jUe1hGf6aDs5oIk3rTy2wEgx0Lb4Vn9Qm7Zc1Jp";

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
    // `accept` inherits `O_NONBLOCK` from a nonblocking listener on macOS,
    // unlike Linux; the read timeout below is the real bound, so the accepted
    // socket must be blocking before it is read.
    stream
        .set_nonblocking(false)
        .expect("blocking request socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("write timeout");
    let mut reader = std::io::BufReader::new(&mut stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("request line");
    assert_eq!(line, "POST /v1/messages HTTP/1.1\r\n");
    let mut length = 0;
    loop {
        line.clear();
        assert!(reader.read_line(&mut line).expect("header") > 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().expect("content length");
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).expect("request body");
    (stream, serde_json::from_slice(&body).expect("JSON request"))
}

fn respond(stream: &mut TcpStream, body: &str) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("response");
}

#[test]
fn the_second_request_carries_the_signed_thinking_block() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("address");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = request(&listener);
        respond(&mut stream, THINKING_TOOL_USE);
        let (mut stream, body) = request(&listener);
        respond(
            &mut stream,
            concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":520,\"output_tokens\":1}}}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"0.4.0\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ),
        );
        body
    });

    let endpoint = Endpoint::new(format!("http://{address}"), "fixture-key")
        .with_auth(AuthStyle::ApiKeyHeader);
    let fetch = UreqFetch::new();
    let mut plan = RequestPlan::new("claude-opus-4-7");
    plan.messages = vec![Message::user("which version is this crate")];

    let outcome = stream_completion(
        &fetch,
        &endpoint,
        &Anthropic,
        &plan,
        Duration::from_secs(5),
        &|| false,
    )
    .expect("first completion");
    assert_eq!(outcome.finish, Some(FinishReason::ToolCalls));
    let calls = outcome.tool_calls();
    assert_eq!(calls.len(), 1);
    let call_id = ToolCallId::new(&calls[0].0).expect("call id");

    let mut assistant = Message::assistant(vec![
        ContentPart::Text {
            text: outcome.text(),
        },
        ContentPart::ToolCall {
            id: call_id.clone(),
            name: calls[0].1.clone(),
            arguments: calls[0].2.clone(),
        },
    ]);
    assistant.replay = outcome.replay;
    // Persist and reopen the message before sending the tool result.
    let assistant = serde_json::from_str(&serde_json::to_string(&assistant).expect("serialize"))
        .expect("reopen");
    plan.messages.push(assistant);
    plan.messages.push(Message {
        role: Role::Tool,
        parts: vec![ContentPart::ToolResult {
            id: call_id,
            name: "read_file".to_owned(),
            content: "version = \"0.4.0\"".to_owned(),
            is_error: false,
        }],
        replay: None,
    });
    let outcome = stream_completion(
        &fetch,
        &endpoint,
        &Anthropic,
        &plan,
        Duration::from_secs(5),
        &|| false,
    )
    .expect("tool result completion");
    assert_eq!(outcome.text(), "0.4.0");

    let body = server.join().expect("fixture server");
    assert_eq!(body["messages"][1]["role"], "assistant");
    let content = body["messages"][1]["content"]
        .as_array()
        .expect("assistant blocks");
    assert_eq!(content.len(), 3, "no duplicated text or calls");
    assert_eq!(
        content[0],
        json!({
            "type": "thinking",
            "thinking": "The user wants the version in Cargo.toml. Reading the file is the direct way to find it.",
            "signature": SIGNATURE,
        })
    );
    assert_eq!(
        content[1],
        json!({"type": "text", "text": "Reading Cargo.toml."})
    );
    assert_eq!(content[2]["type"], "tool_use");
    assert_eq!(content[2]["id"], "toolu_01A09q90qw90lq917835lq9");
    assert_eq!(content[2]["input"], json!({"path": "Cargo.toml"}));
}
