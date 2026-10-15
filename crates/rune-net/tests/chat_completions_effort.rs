//! Reasoning effort through a real compatible HTTP endpoint.

#![cfg(not(target_family = "wasm"))]
#![allow(clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use rune_core::config::Effort;
use rune_net::chat_completions::ChatCompletions;
use rune_net::message::Message;
use rune_net::provider::RequestPlan;
use rune_net::transport::{Endpoint, UreqFetch, stream_completion};

#[test]
fn auto_and_high_effort_reach_a_supporting_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("fixture address");
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let started = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(started.elapsed() < Duration::from_secs(5), "no request");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept fixture request: {error}"),
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
            assert_eq!(line, "POST /v1/chat/completions HTTP/1.1\r\n");
            let mut length = 0;
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).expect("request header") > 0);
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
            let request: serde_json::Value = serde_json::from_slice(&body).expect("request JSON");
            assert_eq!(request["model"], "reasoning-fixture");
            assert!(matches!(
                request
                    .get("reasoning_effort")
                    .and_then(serde_json::Value::as_str),
                None | Some("high")
            ));
            requests.push(request);

            let response = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .expect("fixture response");
        }
        requests
    });

    let endpoint = Endpoint::new(format!("http://{address}/v1"), "fixture-key");
    let mut plan = RequestPlan::new("reasoning-fixture");
    plan.messages = vec![Message::user("hello")];
    for effort in [Effort::Auto, Effort::High] {
        plan.effort = effort;
        let outcome = stream_completion(
            &UreqFetch::new(),
            &endpoint,
            &ChatCompletions,
            &plan,
            Duration::from_secs(5),
            &|| false,
        )
        .expect("compatible completion");
        assert_eq!(outcome.text(), "ok");
    }

    let requests = server.join().expect("fixture server");
    assert!(requests[0].get("reasoning_effort").is_none());
    assert_eq!(requests[1]["reasoning_effort"], "high");
    let mut high = requests[1].clone();
    high.as_object_mut()
        .expect("request object")
        .remove("reasoning_effort");
    assert_eq!(requests[0], high, "effort must be the only request change");
}
