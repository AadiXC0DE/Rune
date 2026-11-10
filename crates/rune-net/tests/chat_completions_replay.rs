//! Reasoning read from a real compatible stream and sent back on the next request.

#![cfg(not(target_family = "wasm"))]
#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use rune_core::id::ToolCallId;
use rune_net::chat_completions::ChatCompletions;
use rune_net::compat::ReasoningReplay;
use rune_net::message::{ContentPart, Message, ToolSpec};
use rune_net::provider::{Provider as _, RequestPlan};
use rune_net::transport::{Endpoint, StreamOutcome, UreqFetch, stream_completion};
use serde_json::Value;

const REASONING: &str = "Check the file first. Then answer.";

const FIXTURES: [(&str, &str); 4] = [
    (
        "reasoning_content",
        include_str!("fixtures/chat_completions/reasoning_content/response.txt"),
    ),
    (
        "reasoning",
        include_str!("fixtures/chat_completions/reasoning/response.txt"),
    ),
    (
        "reasoning_text",
        include_str!("fixtures/chat_completions/reasoning_text/response.txt"),
    ),
    (
        "reasoning_details",
        include_str!("fixtures/chat_completions/reasoning_details/response.txt"),
    ),
];

/// Serves one recorded stream to one request and returns what the client read.
fn stream(recorded: &'static str) -> StreamOutcome {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("fixture address");
    let server = std::thread::spawn(move || {
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
        // unlike Linux; the read timeout below is the real bound, so the
        // accepted socket must be blocking before it is read.
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
        assert_eq!(line, "POST /v1/chat/completions HTTP/1.1\r\n");
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
        drop(reader);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{recorded}",
            recorded.len()
        )
        .expect("response");
    });

    let endpoint = Endpoint::new(format!("http://{address}/v1"), "fixture-key");
    let mut plan = RequestPlan::new("fixture-model");
    plan.messages = vec![Message::user("read a.txt")];
    let outcome = stream_completion(
        &UreqFetch::new(),
        &endpoint,
        &ChatCompletions,
        &plan,
        Duration::from_secs(5),
        &|| false,
    )
    .expect("compatible completion");
    server.join().expect("fixture server");
    outcome
}

/// The `reasoning_details` entries exactly as the recorded frames carry them.
fn recorded_details(recorded: &str) -> Vec<Value> {
    recorded
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
        .filter_map(|frame| {
            frame["choices"][0]["delta"]["reasoning_details"]
                .as_array()
                .cloned()
        })
        .flatten()
        .collect()
}

/// Builds the next request with the streamed turn in its history.
fn next_request(outcome: &StreamOutcome, replay: ReasoningReplay) -> Value {
    let mut assistant = Message::assistant(vec![
        ContentPart::Reasoning {
            text: outcome.reasoning(),
        },
        ContentPart::Text {
            text: outcome.text(),
        },
        ContentPart::ToolCall {
            id: ToolCallId::new("call_1").expect("id"),
            name: "read_file".to_owned(),
            arguments: "{\"path\":\"a.txt\"}".to_owned(),
        },
    ]);
    assistant.replay.clone_from(&outcome.replay);
    let mut plan = RequestPlan::new("fixture-model");
    plan.tools = vec![ToolSpec {
        name: "read_file".to_owned(),
        description: "Read a file".to_owned(),
        input_schema: serde_json::json!({ "type": "object" }),
    }];
    plan.messages = vec![Message::user("read a.txt"), assistant];
    plan.compat.reasoning_replay = replay;
    ChatCompletions.build_request(&plan).expect("build")
}

/// The reasoning fields on an encoded assistant message.
fn reasoning_fields(message: &Value) -> Vec<(&str, &Value)> {
    message
        .as_object()
        .expect("message object")
        .iter()
        .filter(|(key, _)| key.starts_with("reasoning"))
        .map(|(key, value)| (key.as_str(), value))
        .collect()
}

#[test]
fn streamed_reasoning_replays_in_the_field_each_endpoint_reads() {
    for (observed, recorded) in FIXTURES {
        let outcome = stream(recorded);
        assert_eq!(outcome.reasoning(), REASONING, "{observed}");
        assert_eq!(outcome.text(), "Reading it.", "{observed}");
        let stored: Value =
            serde_json::from_str(outcome.replay.as_deref().expect("replay")).expect("replay JSON");
        assert_eq!(stored["_tool_call_ids"], serde_json::json!(["call_1"]));
        assert_eq!(stored["reasoning"]["text"], REASONING, "{observed}");
        let field = if observed == "reasoning_details" {
            "reasoning"
        } else {
            observed
        };
        assert_eq!(stored["reasoning"]["field"], field, "{observed}");

        let text = Value::String(REASONING.to_owned());
        let none = next_request(&outcome, ReasoningReplay::None);
        assert!(
            reasoning_fields(&none["messages"][1]).is_empty(),
            "{observed}: {none}"
        );
        for (replay, expected) in [
            (ReasoningReplay::ReasoningContent, "reasoning_content"),
            (ReasoningReplay::Reasoning, "reasoning"),
            (ReasoningReplay::AsObserved, field),
        ] {
            let body = next_request(&outcome, replay);
            assert_eq!(
                reasoning_fields(&body["messages"][1]),
                vec![(expected, &text)],
                "{observed} with {replay:?}"
            );
        }

        let body = next_request(&outcome, ReasoningReplay::ReasoningDetails);
        let fields = reasoning_fields(&body["messages"][1]);
        if observed == "reasoning_details" {
            let details = recorded_details(recorded);
            assert_eq!(details.len(), 3);
            assert_eq!(fields.len(), 1, "{fields:?}");
            assert_eq!(fields[0].0, "reasoning_details");
            assert_eq!(
                serde_json::to_string(fields[0].1).expect("sent"),
                serde_json::to_string(&details).expect("recorded"),
                "the details must be sent back exactly as they arrived"
            );
        } else {
            assert_eq!(fields, vec![("reasoning", &text)], "{observed}");
        }
    }
}
