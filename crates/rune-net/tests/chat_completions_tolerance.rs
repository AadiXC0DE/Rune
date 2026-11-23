//! Stream shapes compatible servers send, read through a real HTTP endpoint.

#![cfg(not(target_family = "wasm"))]
#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use rune_net::chat_completions::ChatCompletions;
use rune_net::error::{FailureKind, NetResult};
use rune_net::message::Message;
use rune_net::provider::RequestPlan;
use rune_net::stream::FinishReason;
use rune_net::transport::{Endpoint, StreamOutcome, UreqFetch, stream_completion};

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            "fixtures/chat_completions/tolerance/",
            $name,
            ".txt"
        ))
    };
}

/// Serves one recorded stream to one request and returns what the client read.
fn serve(recorded: &'static str, needs_done_marker: bool) -> NetResult<StreamOutcome> {
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
    plan.compat.needs_done_marker = needs_done_marker;
    let outcome = stream_completion(
        &UreqFetch::new(),
        &endpoint,
        &ChatCompletions,
        &plan,
        Duration::from_secs(5),
        &|| false,
    );
    server.join().expect("fixture server");
    outcome
}

fn completed(recorded: &'static str) -> StreamOutcome {
    serve(recorded, false).expect("the stream is accepted")
}

fn failed(recorded: &'static str, needs_done_marker: bool) -> FailureKind {
    serve(recorded, needs_done_marker)
        .expect_err("the stream is incomplete")
        .kind()
}

fn call(id: &str, name: &str, arguments: &str) -> (String, String, String) {
    (id.to_owned(), name.to_owned(), arguments.to_owned())
}

#[test]
fn a_stream_that_finished_without_done_succeeds() {
    let outcome = completed(fixture!("no_done_after_finish"));
    assert_eq!(outcome.finish, Some(FinishReason::Stop));
    assert_eq!(outcome.text(), "All done.");
    assert!(outcome.tool_calls().is_empty());
}

#[test]
fn a_usage_only_last_chunk_records_usage() {
    let outcome = completed(fixture!("usage_only_last_chunk"));
    assert_eq!(outcome.finish, Some(FinishReason::Stop));
    assert_eq!(outcome.text(), "Counted.");
    assert_eq!(outcome.usage.input_tokens, Some(12));
    assert_eq!(outcome.usage.output_tokens, Some(3));
}

#[test]
fn an_empty_function_call_beside_tool_calls_is_ignored() {
    let outcome = completed(fixture!("empty_function_call"));
    assert_eq!(outcome.finish, Some(FinishReason::ToolCalls));
    assert_eq!(
        outcome.tool_calls(),
        vec![call("call_1", "read_file", r#"{"path":"a.txt"}"#)]
    );
}

#[test]
fn an_empty_model_name_is_accepted() {
    let outcome = completed(fixture!("empty_model"));
    assert_eq!(outcome.finish, Some(FinishReason::Stop));
    assert_eq!(outcome.text(), "No model named.");
}

#[test]
fn keep_alive_comments_between_chunks_are_skipped() {
    let outcome = completed(fixture!("keep_alive_comments"));
    assert_eq!(outcome.finish, Some(FinishReason::Stop));
    assert_eq!(outcome.text(), "Still here.");
}

#[test]
fn a_whole_call_in_one_chunk_is_read() {
    let outcome = completed(fixture!("whole_call_one_chunk"));
    assert_eq!(outcome.finish, Some(FinishReason::ToolCalls));
    assert_eq!(
        outcome.tool_calls(),
        vec![call("call_1", "read_file", r#"{"path":"a.txt"}"#)]
    );
}

#[test]
fn calls_without_an_index_are_addressed_by_their_id() {
    let outcome = completed(fixture!("call_without_index"));
    assert_eq!(outcome.finish, Some(FinishReason::ToolCalls));
    assert_eq!(
        outcome.tool_calls(),
        vec![
            call("call_1", "read_file", r#"{"path":"a.txt"}"#),
            call("call_2", "read_file", r#"{"path":"b.txt"}"#),
        ]
    );
}

#[test]
fn calls_ended_by_done_without_a_finish_reason_are_tool_calls() {
    let outcome = completed(fixture!("no_finish_reason_with_calls"));
    assert_eq!(outcome.finish, Some(FinishReason::ToolCalls));
    assert_eq!(
        outcome.tool_calls(),
        vec![call("call_1", "read_file", r#"{"path":"a.txt"}"#)]
    );
}

#[test]
fn complete_calls_ended_by_the_connection_are_tool_calls() {
    let outcome = completed(fixture!("no_finish_no_done_with_calls"));
    assert_eq!(outcome.finish, Some(FinishReason::ToolCalls));
    assert_eq!(
        outcome.tool_calls(),
        vec![call("call_1", "read_file", r#"{"path":"a.txt"}"#)]
    );
}

#[test]
fn crlf_line_ends_are_read() {
    let recorded = fixture!("crlf_line_ends");
    assert!(recorded.contains("\r\n"));
    assert!(!recorded.replace("\r\n", "").contains('\n'));
    let outcome = completed(recorded);
    assert_eq!(outcome.finish, Some(FinishReason::Stop));
    assert_eq!(outcome.text(), "Windows lines.");
}

#[test]
fn a_stream_cut_inside_the_arguments_is_incomplete() {
    assert_eq!(
        failed(fixture!("cut_mid_arguments"), false),
        FailureKind::IncompleteStream
    );
}

#[test]
fn a_stream_with_only_the_role_is_incomplete() {
    assert_eq!(
        failed(fixture!("nothing_at_all"), false),
        FailureKind::IncompleteStream
    );
}

#[test]
fn an_endpoint_that_needs_done_is_incomplete_without_it() {
    assert_eq!(
        failed(fixture!("done_required"), true),
        FailureKind::IncompleteStream
    );
}
